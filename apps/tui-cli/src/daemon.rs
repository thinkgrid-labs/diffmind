//! `diffmind serve` — keep the model resident between invocations.
//!
//! Loading a 1.1 GB GGUF and initialising candle costs seconds on every single
//! run, which is why nobody habitually runs `diffmind commit`. The daemon pays
//! that once and unloads itself after an idle period, like `ssh-agent`.
//!
//! Transport is newline-delimited JSON over TCP bound to 127.0.0.1. A Unix
//! socket would be marginally tighter but does not exist on Windows, and the
//! loopback bind plus a per-instance bearer token gives equivalent isolation:
//! another user on the machine cannot read the token file, and without it every
//! request is refused. Connections are handled one at a time on purpose — there
//! is one model and one GPU, so concurrency would only queue inside candle.
//!
//! Because connections are sequential, the transport has to bound everything
//! an *unauthenticated* peer can make it wait for or allocate: a request line
//! is capped at [`MAX_REQUEST_BYTES`] and must arrive whole within
//! [`REQUEST_DEADLINE`], so a local process that connects and stalls costs the
//! daemon at most that long — not the model's residency, and not its memory.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use core_engine::{AnalysisStats, ReviewSummary};

/// Largest request line the daemon will buffer. A review request carries the
/// post-filter diff (itself capped at 1.5 MB by the CLI) plus rule sets and a
/// baseline, so 16 MiB is generous for a legitimate client and still a hard
/// ceiling for a hostile one.
pub const MAX_REQUEST_BYTES: usize = 16 * 1024 * 1024;

/// Largest response line a client will buffer. Findings are bounded in number
/// and length by the engine; this only guards a client against a peer that is
/// not really a diffmind daemon.
const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

/// How long an accepted connection has to deliver its complete request line.
/// Loopback delivers 16 MiB in milliseconds, so this only ever fires for a
/// peer that is stalling or trickling bytes.
pub const REQUEST_DEADLINE: Duration = Duration::from_secs(30);

/// Written by the daemon, read by clients.
#[derive(Serialize, Deserialize)]
pub struct DaemonInfo {
    pub port: u16,
    pub token: String,
    pub pid: u32,
    /// Which model and device the resident backend was built with. A client
    /// asking for anything else must not be served stale weights.
    pub model: String,
    pub device: String,
    pub version: String,
}

/// Manual so the bearer token can never reach a log line through `{:?}`.
impl std::fmt::Debug for DaemonInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DaemonInfo")
            .field("port", &self.port)
            .field("token", &"<redacted>")
            .field("pid", &self.pid)
            .field("model", &self.model)
            .field("device", &self.device)
            .field("version", &self.version)
            .finish()
    }
}

pub fn info_path(home: &Path) -> PathBuf {
    home.join(".diffmind").join("daemon.json")
}

pub fn read_info(home: &Path) -> Option<DaemonInfo> {
    let raw = std::fs::read_to_string(info_path(home)).ok()?;
    let info: DaemonInfo = serde_json::from_str(&raw).ok()?;
    // A daemon from an older build may not speak this protocol.
    (info.version == crate::output::VERSION).then_some(info)
}

/// Publish the daemon's address and token.
///
/// The token must never be readable by another user, not even briefly. Writing
/// the file and *then* tightening its mode — what this used to do — left a
/// window in which it existed with the umask's permissions (typically 0644),
/// and a failed `chmod` was silently ignored. Now the file is created 0600 from
/// the first byte under a temporary name, then renamed into place, so a reader
/// sees either the previous file or the complete new one.
pub fn write_info(home: &Path, info: &DaemonInfo) -> Result<()> {
    let path = info_path(home);
    let parent = path
        .parent()
        .context("daemon info path has no parent directory")?;
    std::fs::create_dir_all(parent)?;
    let body = serde_json::to_string_pretty(info)?;
    crate::secure_fs::write_private_atomic(&path, body.as_bytes())
        .with_context(|| format!("could not write {}", path.display()))
}

pub fn clear_info(home: &Path) {
    let _ = std::fs::remove_file(info_path(home));
}

// ─── Protocol ────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Shutdown,
    Review(Box<ReviewRequest>),
}

/// Everything the client can vary per invocation.
///
/// A field missing here is not a missing feature — it is a flag that silently
/// does nothing the moment a daemon is running, because the daemon falls back
/// to whatever `diffmind serve` was started with. Nothing about the field's
/// absence looks wrong from either side.
///
/// Note the deliberate lack of `#[serde(default)]` on the scalars: a default
/// would let a renamed or dropped field arrive as `0`, which for `seed` is a
/// perfectly plausible value and would quietly change every result. `read_info`
/// already refuses a daemon from another build, so a hard parse error is the
/// better failure.
#[derive(Debug, Serialize, Deserialize)]
pub struct ReviewRequest {
    pub diff: String,
    pub requirements: Option<String>,
    pub max_tokens: u32,
    pub min_confidence: f32,
    pub triage: String,
    /// Sampling, which decides whether two runs of the same diff agree. The
    /// daemon must not substitute its own: reproducibility is the property the
    /// whole gate rests on, and `--seed` silently ignored is worse than absent.
    pub temperature: f64,
    pub seed: u64,
    pub rules: Vec<core_engine::CustomRule>,
    /// Prose rule sets, sent rather than re-read: the daemon may be serving a
    /// different repository than the one it was started in.
    #[serde(default)]
    pub rulebooks: Vec<core_engine::Rulebook>,
    pub baseline: Option<String>,
    pub use_cache: bool,
    /// Where the daemon loads the symbol index and cache from. The client does
    /// not send assembled context: the daemon owns chunking, so only it knows
    /// what each chunk contains and therefore what context that chunk needs.
    pub project_root: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    Pong { model: String, device: String },
    Ok(Box<ReviewResponse>),
    Error { message: String },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ReviewResponse {
    pub summary: ReviewSummary,
    pub stats: SerializableStats,
    pub backend: String,
}

/// `AnalysisStats` is not serializable in the engine (it is a plain report
/// struct); mirror it rather than force serde into the engine's public API.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SerializableStats {
    pub units_total: usize,
    #[serde(default)]
    pub inference_ms: u64,
    #[serde(default)]
    pub prompt_tokens: usize,
    #[serde(default)]
    pub completion_tokens: usize,
    #[serde(default)]
    pub tokens_estimated: bool,
    pub units_cached: usize,
    pub units_unparseable: usize,
    #[serde(default)]
    pub units_too_large: usize,
    pub files_skipped_by_triage: usize,
    pub suppressed: usize,
    pub below_confidence: usize,
    pub unanchorable: usize,
    /// Completeness, added in 0.9.3. `read_info` refuses a daemon from another
    /// build, so these defaults only ever apply to a hand-written request.
    #[serde(default)]
    pub units_failed: usize,
    #[serde(default)]
    pub backend_error: Option<String>,
    #[serde(default)]
    pub skipped: Vec<core_engine::UnitSkip>,
}

impl From<&AnalysisStats> for SerializableStats {
    fn from(s: &AnalysisStats) -> Self {
        SerializableStats {
            units_total: s.units_total,
            inference_ms: s.inference_ms,
            prompt_tokens: s.prompt_tokens,
            completion_tokens: s.completion_tokens,
            tokens_estimated: s.tokens_estimated,
            units_cached: s.units_cached,
            units_unparseable: s.units_unparseable,
            units_too_large: s.units_too_large,
            files_skipped_by_triage: s.files_skipped_by_triage,
            suppressed: s.suppressed,
            below_confidence: s.below_confidence,
            unanchorable: s.unanchorable,
            units_failed: s.units_failed,
            backend_error: s.backend_error.clone(),
            skipped: s.skipped.clone(),
        }
    }
}

impl From<SerializableStats> for AnalysisStats {
    fn from(s: SerializableStats) -> Self {
        AnalysisStats {
            units_total: s.units_total,
            inference_ms: s.inference_ms,
            prompt_tokens: s.prompt_tokens,
            completion_tokens: s.completion_tokens,
            tokens_estimated: s.tokens_estimated,
            units_cached: s.units_cached,
            units_unparseable: s.units_unparseable,
            units_too_large: s.units_too_large,
            files_skipped_by_triage: s.files_skipped_by_triage,
            suppressed: s.suppressed,
            below_confidence: s.below_confidence,
            unanchorable: s.unanchorable,
            units_failed: s.units_failed,
            backend_error: s.backend_error,
            skipped: s.skipped,
        }
    }
}

/// One request or response per line. Diffs and findings both contain newlines,
/// so every payload is a single serialized JSON line.
fn send(stream: &mut TcpStream, value: &impl Serialize) -> Result<()> {
    let mut line = serde_json::to_string(value)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;
    stream.flush()?;
    Ok(())
}

/// Why a message could not be received. Distinguished so the server can tell
/// a peer *why* it was refused, and so tests can assert on the reason.
#[derive(Debug, PartialEq, Eq)]
enum RecvError {
    Closed,
    Truncated,
    TooLarge,
    Deadline,
    Io(String),
}

impl std::fmt::Display for RecvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RecvError::Closed => write!(f, "connection closed before a message arrived"),
            RecvError::Truncated => write!(f, "connection closed mid-message"),
            RecvError::TooLarge => write!(f, "message exceeds the size limit"),
            RecvError::Deadline => write!(f, "message did not arrive in time"),
            RecvError::Io(e) => write!(f, "read failed: {e}"),
        }
    }
}

impl std::error::Error for RecvError {}

/// Read one newline-terminated message of at most `max` bytes.
///
/// `BufRead::read_line` — what this used to be — buffers without limit, so any
/// local process could make the daemon allocate until it was killed, and its
/// per-read timeout let a peer that sends one byte a minute hold the
/// (sequential) daemon forever. Here the size is capped before it is buffered
/// and `deadline`, when given, bounds the *whole* message rather than each read.
fn recv_line(
    mut stream: &TcpStream,
    max: usize,
    deadline: Option<Instant>,
) -> Result<Vec<u8>, RecvError> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        if let Some(deadline) = deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(RecvError::Deadline);
            }
            stream
                .set_read_timeout(Some(remaining))
                .map_err(|e| RecvError::Io(e.to_string()))?;
        }
        let n = match stream.read(&mut chunk) {
            Ok(n) => n,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Err(RecvError::Deadline);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(RecvError::Io(e.to_string())),
        };
        if n == 0 {
            return Err(if buf.is_empty() {
                RecvError::Closed
            } else {
                RecvError::Truncated
            });
        }
        let part = chunk.get(..n).unwrap_or_default();
        let (body, complete) = match part.iter().position(|&b| b == b'\n') {
            Some(pos) => (part.get(..pos).unwrap_or_default(), true),
            None => (part, false),
        };
        if buf.len().saturating_add(body.len()) > max {
            return Err(RecvError::TooLarge);
        }
        buf.extend_from_slice(body);
        if complete {
            return Ok(buf);
        }
    }
}

fn recv<T: for<'de> Deserialize<'de>>(stream: &TcpStream, max: usize) -> Result<T> {
    let line = recv_line(stream, max, None)?;
    serde_json::from_slice(&line).context("malformed message")
}

// ─── Token ───────────────────────────────────────────────────────────────────

/// 256 bits from the operating system's CSPRNG, hex-encoded.
///
/// This used to hash the pid, the clock, a stack address and `$USER` — every
/// one of which another local user can observe or narrow down, so the token's
/// real entropy was a few dozen bits at best. There is no fallback: if the OS
/// cannot supply randomness the daemon refuses to start rather than run with
/// a guessable credential.
fn generate_token() -> Result<String> {
    let bytes: [u8; 32] =
        crate::secure_fs::os_random().context("could not generate a daemon token")?;
    Ok(crate::secure_fs::hex(&bytes))
}

/// Constant-time comparison so a token cannot be recovered byte-by-byte.
/// Only the length — which is public (64 hex characters) — short-circuits.
fn tokens_match(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a
        .bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
}

// ─── Server ──────────────────────────────────────────────────────────────────

/// Envelope every client sends: the token plus the request.
///
/// No `Debug`: the token must not be printable, and nothing needs to print one.
#[derive(Serialize, Deserialize)]
struct Envelope {
    token: String,
    #[serde(flatten)]
    request: Request,
}

pub struct Server {
    listener: TcpListener,
    token: String,
    idle_timeout: Duration,
    request_deadline: Duration,
}

impl Server {
    pub fn bind(port: u16, idle_timeout: Duration) -> Result<Self> {
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let listener =
            TcpListener::bind(addr).with_context(|| format!("could not bind 127.0.0.1:{port}"))?;
        Ok(Server {
            listener,
            // Fresh per bind, so a restarted daemon never honours a token an
            // earlier instance published.
            token: generate_token()?,
            idle_timeout,
            request_deadline: REQUEST_DEADLINE,
        })
    }

    /// Shorten the per-request deadline so tests need not wait 30 s.
    #[cfg(test)]
    fn with_request_deadline(mut self, d: Duration) -> Self {
        self.request_deadline = d;
        self
    }

    pub fn port(&self) -> Result<u16> {
        Ok(self.listener.local_addr()?.port())
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    /// Serve until the idle timeout expires or a shutdown is requested.
    /// `handle` performs the actual review; the transport knows nothing about
    /// models.
    pub fn run<F>(&self, mut handle: F) -> Result<()>
    where
        F: FnMut(ReviewRequest) -> Response,
    {
        // Poll rather than block forever so the idle timer can fire even when
        // no client ever connects again.
        self.listener.set_nonblocking(true)?;
        let mut last_activity = Instant::now();

        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    // A connection that cannot be made blocking cannot be
                    // served safely (reads would spin); drop it and carry on.
                    if stream.set_nonblocking(false).is_err() {
                        continue;
                    }
                    // Writing a response to a client that stopped reading must
                    // not wedge the daemon either.
                    let _ = stream.set_write_timeout(Some(self.request_deadline));

                    if self.serve_one(stream, &mut handle) {
                        return Ok(());
                    }
                    // Timed from completion, so a long review does not count
                    // as idle time the moment it finishes.
                    last_activity = Instant::now();
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if last_activity.elapsed() >= self.idle_timeout {
                        eprintln!(
                            "  diffmind daemon idle for {}s — unloading model and exiting.",
                            self.idle_timeout.as_secs()
                        );
                        return Ok(());
                    }
                    std::thread::sleep(Duration::from_millis(150));
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Returns true when the daemon should stop.
    fn serve_one<F>(&self, mut stream: TcpStream, handle: &mut F) -> bool
    where
        F: FnMut(ReviewRequest) -> Response,
    {
        // Bounded in size and in total time *before* authentication, because
        // until the token is checked the peer is anyone on the machine.
        let deadline = Instant::now() + self.request_deadline;
        let envelope: Envelope = match recv_line(&stream, MAX_REQUEST_BYTES, Some(deadline)) {
            Ok(line) => match serde_json::from_slice(&line) {
                Ok(e) => e,
                Err(_) => {
                    // Deliberately not echoing the parser's message: it is
                    // about the peer's bytes, and the peer is unauthenticated.
                    let _ = send(
                        &mut stream,
                        &Response::Error {
                            message: "malformed request".into(),
                        },
                    );
                    return false;
                }
            },
            Err(e) => {
                let _ = send(
                    &mut stream,
                    &Response::Error {
                        message: format!("request refused: {e}"),
                    },
                );
                return false;
            }
        };

        if !tokens_match(&envelope.token, &self.token) {
            let _ = send(
                &mut stream,
                &Response::Error {
                    message: "invalid token".into(),
                },
            );
            return false;
        }

        match envelope.request {
            Request::Ping => {
                let _ = send(
                    &mut stream,
                    &Response::Pong {
                        model: String::new(),
                        device: String::new(),
                    },
                );
                false
            }
            Request::Shutdown => {
                let _ = send(
                    &mut stream,
                    &Response::Pong {
                        model: String::new(),
                        device: String::new(),
                    },
                );
                true
            }
            Request::Review(req) => {
                let response = handle(*req);
                let _ = send(&mut stream, &response);
                false
            }
        }
    }
}

// ─── Client ──────────────────────────────────────────────────────────────────

pub struct Client {
    info: DaemonInfo,
}

impl Client {
    /// Connect to a running daemon whose resident model matches what the caller
    /// wants. Returns `None` when there is no usable daemon — the caller then
    /// loads the model in-process as normal.
    pub fn connect(home: &Path, model: &str, device: &str) -> Option<Client> {
        let info = read_info(home)?;
        if info.model != model || info.device != device {
            return None;
        }
        let client = Client { info };
        // A stale daemon.json outlives a crashed daemon, so prove it answers
        // before committing the request to it.
        client.ping().ok()?;
        Some(client)
    }

    fn open(&self) -> Result<TcpStream> {
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, self.info.port));
        let stream = TcpStream::connect_timeout(&addr, Duration::from_millis(500))?;
        // Inference on a big diff legitimately takes minutes.
        stream.set_read_timeout(Some(Duration::from_secs(1800)))?;
        Ok(stream)
    }

    fn call(&self, request: Request) -> Result<Response> {
        let mut stream = self.open()?;
        let envelope = Envelope {
            token: self.info.token.clone(),
            request,
        };
        send(&mut stream, &envelope)?;
        recv(&stream, MAX_RESPONSE_BYTES)
    }

    pub fn ping(&self) -> Result<()> {
        match self.call(Request::Ping)? {
            Response::Pong { .. } => Ok(()),
            Response::Error { message } => anyhow::bail!(message),
            Response::Ok(_) => anyhow::bail!("unexpected response to ping"),
        }
    }

    pub fn shutdown(&self) -> Result<()> {
        self.call(Request::Shutdown).map(|_| ())
    }

    pub fn review(&self, request: ReviewRequest) -> Result<ReviewResponse> {
        match self.call(Request::Review(Box::new(request)))? {
            Response::Ok(r) => Ok(*r),
            Response::Error { message } => anyhow::bail!(message),
            Response::Pong { .. } => anyhow::bail!("unexpected response to review"),
        }
    }

    pub fn describe(&self) -> String {
        format!("daemon pid {} on port {}", self.info.pid, self.info.port)
    }
}

/// Ask a running daemon to stop. Returns false when none was running.
pub fn stop(home: &Path) -> bool {
    let Some(info) = read_info(home) else {
        clear_info(home);
        return false;
    };
    let client = Client { info };
    let stopped = client.shutdown().is_ok();
    clear_info(home);
    stopped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_home(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("diffmind-daemon-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn info(port: u16, token: &str) -> DaemonInfo {
        DaemonInfo {
            port,
            token: token.into(),
            pid: std::process::id(),
            model: "1.5b".into(),
            device: "auto".into(),
            version: crate::output::VERSION.to_string(),
        }
    }

    #[test]
    fn info_round_trips_through_disk() {
        let home = tmp_home("info");
        write_info(&home, &info(1234, "abc")).unwrap();
        let read = read_info(&home).expect("should read back");
        assert_eq!(read.port, 1234);
        assert_eq!(read.token, "abc");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn a_daemon_from_another_version_is_ignored() {
        let home = tmp_home("version");
        let mut i = info(1, "t");
        i.version = "0.0.1-ancient".into();
        std::fs::create_dir_all(home.join(".diffmind")).unwrap();
        std::fs::write(info_path(&home), serde_json::to_string(&i).unwrap()).unwrap();
        assert!(
            read_info(&home).is_none(),
            "a daemon speaking an older protocol must not be used"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[cfg(unix)]
    #[test]
    fn the_token_file_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let home = tmp_home("perms");
        write_info(&home, &info(1, "secret")).unwrap();
        let mode = std::fs::metadata(info_path(&home))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "token must not be readable by other users");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn tokens_are_unguessable_and_distinct() {
        let a = generate_token().unwrap();
        let b = generate_token().unwrap();
        assert_eq!(a.len(), 64, "256 bits, hex-encoded");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "two daemons must not share a token");
    }

    /// Regression for the weak-entropy token: nothing observable about the
    /// process may determine it. Two tokens minted by the same process in the
    /// same instant used to differ only in the nanosecond clock.
    #[test]
    fn tokens_do_not_repeat_across_many_draws() {
        let draws: std::collections::HashSet<String> =
            (0..256).map(|_| generate_token().unwrap()).collect();
        assert_eq!(draws.len(), 256);
    }

    #[test]
    fn the_token_never_appears_in_debug_output() {
        let i = info(1, "super-secret-token-value");
        let shown = format!("{i:?}");
        assert!(!shown.contains("super-secret-token-value"), "{shown}");
        assert!(shown.contains("<redacted>"));
    }

    /// Spawn a server with a handler that records whether it ran.
    fn spawn_server(
        idle: Duration,
    ) -> (
        u16,
        String,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        std::thread::JoinHandle<()>,
    ) {
        spawn_server_with_deadline(idle, REQUEST_DEADLINE)
    }

    fn spawn_server_with_deadline(
        idle: Duration,
        deadline: Duration,
    ) -> (
        u16,
        String,
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
        std::thread::JoinHandle<()>,
    ) {
        let server = Server::bind(0, idle)
            .unwrap()
            .with_request_deadline(deadline);
        let port = server.port().unwrap();
        let token = server.token().to_string();
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = calls.clone();
        let handle = std::thread::spawn(move || {
            server
                .run(|_| {
                    seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Response::Ok(Box::new(ReviewResponse {
                        summary: ReviewSummary::default(),
                        stats: SerializableStats::default(),
                        backend: "test".into(),
                    }))
                })
                .unwrap();
        });
        (port, token, calls, handle)
    }

    fn raw_exchange(port: u16, payload: &[u8]) -> String {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        // The server may refuse and close before reading everything; a reset
        // on write is part of what is being tested, not a failure.
        let _ = s.write_all(payload);
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        out
    }

    fn sample_request() -> ReviewRequest {
        ReviewRequest {
            diff: "d".into(),
            requirements: None,
            max_tokens: 16,
            min_confidence: 0.0,
            triage: "off".into(),
            temperature: 0.0,
            seed: core_engine::DEFAULT_SEED,
            rules: vec![],
            rulebooks: vec![],
            baseline: None,
            use_cache: false,
            project_root: ".".into(),
        }
    }

    #[test]
    fn the_daemon_listens_on_loopback_only() {
        let server = Server::bind(0, Duration::from_secs(1)).unwrap();
        let addr = server.listener.local_addr().unwrap();
        assert!(addr.ip().is_loopback(), "bound to {addr}");
    }

    #[test]
    fn malformed_requests_are_refused_and_the_daemon_keeps_serving() {
        let (port, token, calls, handle) = spawn_server(Duration::from_secs(30));

        for garbage in [
            &b"not json at all\n"[..],
            &b"{\"token\": 12, \"op\": \"ping\"}\n"[..],
            &b"{\"op\": \"review\"}\n"[..],
            &b"\xff\xfe\x00\n"[..],
            &b"\n"[..],
        ] {
            let reply = raw_exchange(port, garbage);
            assert!(reply.contains("malformed request"), "reply: {reply}");
        }
        // Closed mid-message: no newline, then EOF.
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.write_all(b"{\"token\":\"abc").unwrap();
        s.shutdown(std::net::Shutdown::Write).unwrap();
        let mut reply = String::new();
        let _ = s.read_to_string(&mut reply);
        assert!(reply.contains("closed mid-message"), "reply: {reply}");

        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        let owner = Client {
            info: info(port, &token),
        };
        owner.ping().expect("still serving after garbage");
        owner.shutdown().unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn an_oversized_request_is_refused_without_reaching_the_handler() {
        let (port, token, calls, handle) = spawn_server(Duration::from_secs(30));

        // One byte over the cap, no newline: the old reader would have kept
        // buffering for as long as the peer kept sending.
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let block = vec![b'a'; 1024 * 1024];
        let mut sent = 0usize;
        while sent <= MAX_REQUEST_BYTES {
            if s.write_all(&block).is_err() {
                break; // refused and closed — expected
            }
            sent += block.len();
        }
        let mut reply = String::new();
        let _ = s.read_to_string(&mut reply);
        if !reply.is_empty() {
            assert!(reply.contains("size limit"), "reply: {reply}");
        }
        drop(s);

        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        let owner = Client {
            info: info(port, &token),
        };
        owner
            .ping()
            .expect("still serving after an oversized request");
        owner.shutdown().unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn recv_line_enforces_the_cap_and_the_deadline() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();

        // Too large.
        let writer = std::thread::spawn(move || {
            let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
            let _ = c.write_all(&[b'x'; 200]);
            std::thread::sleep(Duration::from_millis(200));
        });
        let (server_side, _) = listener.accept().unwrap();
        assert_eq!(
            recv_line(
                &server_side,
                100,
                Some(Instant::now() + Duration::from_secs(5))
            ),
            Err(RecvError::TooLarge)
        );
        writer.join().unwrap();

        // Trickling: a byte at a time never completes within the deadline,
        // even though each individual read succeeds quickly.
        let writer = std::thread::spawn(move || {
            let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
            for _ in 0..30 {
                if c.write_all(b"x").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        let (server_side, _) = listener.accept().unwrap();
        let started = Instant::now();
        assert_eq!(
            recv_line(
                &server_side,
                1000,
                Some(Instant::now() + Duration::from_millis(400))
            ),
            Err(RecvError::Deadline)
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the deadline bounds the whole message, not each read"
        );
        drop(server_side);
        writer.join().unwrap();
    }

    /// A peer that connects and never speaks used to pin the sequential daemon
    /// for 120 s per read; one that trickled bytes, indefinitely. It must now
    /// cost at most the request deadline, after which the next client is served.
    #[test]
    fn a_stalled_connection_does_not_block_the_next_client_forever() {
        let deadline = Duration::from_millis(700);
        let (port, token, _calls, handle) =
            spawn_server_with_deadline(Duration::from_secs(120), deadline);
        let stalled = TcpStream::connect(("127.0.0.1", port)).unwrap();

        let owner = Client {
            info: info(port, &token),
        };
        let started = Instant::now();
        owner
            .ping()
            .expect("served once the stalled peer's deadline passed");
        assert!(
            started.elapsed() < deadline + Duration::from_secs(5),
            "waited {:?}",
            started.elapsed()
        );
        drop(stalled);
        owner.shutdown().unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn concurrent_clients_are_all_served_in_turn() {
        let (port, token, calls, handle) = spawn_server(Duration::from_secs(30));
        let workers: Vec<_> = (0..4)
            .map(|_| {
                let token = token.clone();
                std::thread::spawn(move || {
                    let c = Client {
                        info: info(port, &token),
                    };
                    c.review(sample_request()).map(|_| ())
                })
            })
            .collect();
        for w in workers {
            w.join().unwrap().expect("every queued client is answered");
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 4);
        Client {
            info: info(port, &token),
        }
        .shutdown()
        .unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn a_restarted_daemon_does_not_honour_the_previous_token() {
        let (port_a, token_a, _, handle_a) = spawn_server(Duration::from_secs(30));
        Client {
            info: info(port_a, &token_a),
        }
        .shutdown()
        .unwrap();
        handle_a.join().unwrap();

        let (port_b, token_b, calls, handle_b) = spawn_server(Duration::from_secs(30));
        assert_ne!(token_a, token_b);
        let stale = Client {
            info: info(port_b, &token_a),
        };
        let err = stale.review(sample_request()).unwrap_err();
        assert!(err.to_string().contains("invalid token"));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        Client {
            info: info(port_b, &token_b),
        }
        .shutdown()
        .unwrap();
        handle_b.join().unwrap();
    }

    #[test]
    fn the_daemon_exits_on_its_own_after_the_idle_timeout() {
        let (_port, _token, _calls, handle) = spawn_server(Duration::from_millis(300));
        let started = Instant::now();
        handle.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn rewriting_the_info_file_keeps_it_private() {
        let home = tmp_home("rewrite");
        write_info(&home, &info(1, "first")).unwrap();
        write_info(&home, &info(2, "second")).unwrap();
        let read = read_info(&home).unwrap();
        assert_eq!(read.port, 2);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(info_path(&home))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0);
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn token_comparison_rejects_mismatches() {
        assert!(tokens_match("abc", "abc"));
        assert!(!tokens_match("abc", "abd"));
        assert!(!tokens_match("abc", "ab"));
    }

    #[test]
    fn connect_returns_none_when_the_model_differs() {
        let home = tmp_home("mismatch");
        write_info(&home, &info(1, "t")).unwrap();
        assert!(
            Client::connect(&home, "3b", "auto").is_none(),
            "a daemon holding a different model must not answer"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn connect_returns_none_when_the_daemon_is_dead() {
        let home = tmp_home("dead");
        // Port 1 is not listening; this is the stale-daemon.json case.
        write_info(&home, &info(1, "t")).unwrap();
        assert!(Client::connect(&home, "1.5b", "auto").is_none());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn server_serves_a_review_and_honours_shutdown() {
        let server = Server::bind(0, Duration::from_secs(30)).unwrap();
        let port = server.port().unwrap();
        let token = server.token().to_string();

        let handle = std::thread::spawn(move || {
            server
                .run(|_req| {
                    Response::Ok(Box::new(ReviewResponse {
                        summary: ReviewSummary {
                            positives: vec!["from the daemon".into()],
                            ..Default::default()
                        },
                        stats: SerializableStats::default(),
                        backend: "test".into(),
                    }))
                })
                .unwrap();
        });

        let client = Client {
            info: info(port, &token),
        };

        client.ping().expect("daemon should answer a ping");

        let response = client
            .review(ReviewRequest {
                diff: "d".into(),
                requirements: None,
                max_tokens: 128,
                min_confidence: 0.0,
                triage: "off".into(),
                temperature: 0.0,
                seed: core_engine::DEFAULT_SEED,
                rules: vec![],
                rulebooks: vec![],
                baseline: None,
                use_cache: false,
                project_root: ".".into(),
            })
            .expect("review should round-trip");
        assert_eq!(response.summary.positives, vec!["from the daemon"]);

        client.shutdown().unwrap();
        handle.join().unwrap();
    }

    /// Everything added to this protocol carries `#[serde(default)]`, which is
    /// what lets an older daemon accept a newer request — and also what would
    /// let a renamed or unserializable field silently arrive empty.
    ///
    /// Both failures here are invisible: rule sets would quietly stop applying
    /// on the daemon path, and every daemon-served review would report zero
    /// cost, which is the number Phase 2 is measured on.
    #[test]
    fn rule_sets_and_cost_survive_the_wire() {
        let server = Server::bind(0, Duration::from_secs(30)).unwrap();
        let port = server.port().unwrap();
        let token = server.token().to_string();

        let handle = std::thread::spawn(move || {
            server
                .run(|req| {
                    // Echo what arrived, so the assertions below see the
                    // deserialized request rather than the one we constructed.
                    let seen = req
                        .rulebooks
                        .first()
                        .map(|b| format!("{}|{}", b.id, b.body))
                        .unwrap_or_else(|| "NO RULEBOOKS".into());
                    Response::Ok(Box::new(ReviewResponse {
                        summary: ReviewSummary {
                            positives: vec![seen],
                            ..Default::default()
                        },
                        stats: SerializableStats {
                            units_total: 3,
                            inference_ms: 1234,
                            prompt_tokens: 500,
                            completion_tokens: 60,
                            tokens_estimated: true,
                            ..Default::default()
                        },
                        backend: "test".into(),
                    }))
                })
                .unwrap();
        });

        let client = Client {
            info: info(port, &token),
        };
        let response = client
            .review(ReviewRequest {
                diff: "d".into(),
                requirements: None,
                max_tokens: 128,
                min_confidence: 0.0,
                triage: "off".into(),
                temperature: 0.0,
                seed: core_engine::DEFAULT_SEED,
                rules: vec![],
                rulebooks: vec![core_engine::Rulebook {
                    id: "api".into(),
                    description: None,
                    always: false,
                    scope: vec!["src/api/**".into()],
                    severity: Some(core_engine::Severity::High),
                    body: "- Handlers return ApiError.".into(),
                }],
                baseline: None,
                use_cache: false,
                project_root: ".".into(),
            })
            .expect("review should round-trip");

        assert_eq!(
            response.summary.positives,
            vec!["api|- Handlers return ApiError."],
            "the rule set must reach the daemon, or it silently stops applying"
        );

        let stats: AnalysisStats = response.stats.into();
        assert_eq!(stats.inference_ms, 1234);
        assert_eq!(stats.prompt_tokens, 500);
        assert_eq!(stats.completion_tokens, 60);
        assert!(stats.tokens_estimated);

        client.shutdown().unwrap();
        handle.join().unwrap();
    }

    /// `--seed` and `--temperature` used to be absent from the protocol
    /// entirely, so a daemon reviewed with whatever sampling `diffmind serve`
    /// was started with. Two runs of the same diff could then disagree for no
    /// visible reason — and reproducibility is the property the gate rests on.
    #[test]
    fn sampling_survives_the_wire() {
        let server = Server::bind(0, Duration::from_secs(30)).unwrap();
        let port = server.port().unwrap();
        let token = server.token().to_string();

        let handle = std::thread::spawn(move || {
            server
                .run(|req| {
                    // Echo what the handler actually received, so the assertion
                    // sees the deserialized request rather than the sent one.
                    Response::Ok(Box::new(ReviewResponse {
                        summary: ReviewSummary {
                            positives: vec![format!("{}|{}", req.temperature, req.seed)],
                            ..Default::default()
                        },
                        stats: SerializableStats::default(),
                        backend: "test".into(),
                    }))
                })
                .unwrap();
        });

        let client = Client {
            info: info(port, &token),
        };
        // Deliberately not the defaults — a dropped field would read as 0.0/0
        // or as the daemon's own values, and both would pass a laxer assertion.
        let response = client
            .review(ReviewRequest {
                diff: "d".into(),
                requirements: None,
                max_tokens: 128,
                min_confidence: 0.0,
                triage: "off".into(),
                temperature: 0.7,
                seed: 424_242,
                rules: vec![],
                rulebooks: vec![],
                baseline: None,
                use_cache: false,
                project_root: ".".into(),
            })
            .expect("review should round-trip");

        assert_eq!(
            response.summary.positives,
            vec!["0.7|424242"],
            "sampling must reach the daemon, or --seed silently does nothing"
        );

        client.shutdown().unwrap();
        handle.join().unwrap();
    }

    /// A partial review must cross the wire as one: if `backend_error` or the
    /// skip list were dropped, a daemon-served review whose backend failed
    /// would arrive looking complete and exit 0.
    #[test]
    fn completeness_survives_the_wire() {
        let server = Server::bind(0, Duration::from_secs(30)).unwrap();
        let port = server.port().unwrap();
        let token = server.token().to_string();
        let handle = std::thread::spawn(move || {
            server
                .run(|_| {
                    let stats = AnalysisStats {
                        units_total: 3,
                        units_unparseable: 1,
                        units_failed: 1,
                        backend_error: Some("HTTP 503".into()),
                        skipped: vec![core_engine::UnitSkip {
                            unit_id: "u1".into(),
                            files: vec!["a.rs".into()],
                            reason: core_engine::SkipReason::BackendFailed,
                            detail: "HTTP 503".into(),
                        }],
                        ..Default::default()
                    };
                    Response::Ok(Box::new(ReviewResponse {
                        summary: ReviewSummary::default(),
                        stats: (&stats).into(),
                        backend: "test".into(),
                    }))
                })
                .unwrap();
        });
        let client = Client {
            info: info(port, &token),
        };
        let stats: AnalysisStats = client.review(sample_request()).unwrap().stats.into();
        assert!(!stats.is_complete());
        assert_eq!(stats.backend_error.as_deref(), Some("HTTP 503"));
        assert_eq!(stats.units_skipped(), 2);
        assert_eq!(stats.skipped.len(), 1);
        client.shutdown().unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn a_bad_token_is_refused_and_never_reaches_the_handler() {
        // A short idle timeout so the server winds itself down promptly.
        let server = Server::bind(0, Duration::from_millis(600)).unwrap();
        let port = server.port().unwrap();
        let real_token = server.token().to_string();

        let handler_ran = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = handler_ran.clone();

        let handle = std::thread::spawn(move || {
            server
                .run(|_| {
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                    Response::Error {
                        message: "should never be reached".into(),
                    }
                })
                .unwrap();
        });

        let attacker = Client {
            info: info(port, &"0".repeat(64)),
        };
        let err = attacker
            .review(ReviewRequest {
                diff: "secret code".into(),
                requirements: None,
                max_tokens: 16,
                min_confidence: 0.0,
                triage: "off".into(),
                temperature: 0.0,
                seed: core_engine::DEFAULT_SEED,
                rules: vec![],
                rulebooks: vec![],
                baseline: None,
                use_cache: false,
                project_root: ".".into(),
            })
            .unwrap_err();
        assert!(err.to_string().contains("invalid token"));
        assert!(
            !handler_ran.load(std::sync::atomic::Ordering::SeqCst),
            "an unauthenticated request must be rejected before any inference runs"
        );

        // A holder of the real token can still stop it immediately.
        let owner = Client {
            info: info(port, &real_token),
        };
        owner.shutdown().unwrap();
        handle.join().unwrap();
    }
}
