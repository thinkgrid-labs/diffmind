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

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use core_engine::{AnalysisStats, ReviewSummary};

/// Written by the daemon, read by clients.
#[derive(Debug, Serialize, Deserialize)]
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

pub fn info_path(home: &Path) -> PathBuf {
    home.join(".diffmind").join("daemon.json")
}

pub fn read_info(home: &Path) -> Option<DaemonInfo> {
    let raw = std::fs::read_to_string(info_path(home)).ok()?;
    let info: DaemonInfo = serde_json::from_str(&raw).ok()?;
    // A daemon from an older build may not speak this protocol.
    (info.version == crate::output::VERSION).then_some(info)
}

pub fn write_info(home: &Path, info: &DaemonInfo) -> Result<()> {
    let path = info_path(home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(info)?)?;
    restrict_permissions(&path);
    Ok(())
}

/// The file holds a bearer token, so keep it owner-readable only.
#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {
    // Windows inherits the user profile's ACL, which is already per-user.
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

#[derive(Debug, Serialize, Deserialize)]
pub struct ReviewRequest {
    pub diff: String,
    pub languages: Vec<String>,
    pub requirements: Option<String>,
    pub max_tokens: u32,
    pub min_confidence: f32,
    pub triage: String,
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
    pub files_skipped_by_triage: usize,
    pub suppressed: usize,
    pub below_confidence: usize,
    pub unanchorable: usize,
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
            files_skipped_by_triage: s.files_skipped_by_triage,
            suppressed: s.suppressed,
            below_confidence: s.below_confidence,
            unanchorable: s.unanchorable,
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
            files_skipped_by_triage: s.files_skipped_by_triage,
            suppressed: s.suppressed,
            below_confidence: s.below_confidence,
            unanchorable: s.unanchorable,
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

fn recv<T: for<'de> Deserialize<'de>>(reader: &mut BufReader<&TcpStream>) -> Result<T> {
    let mut line = String::new();
    let n = reader.read_line(&mut line)?;
    if n == 0 {
        anyhow::bail!("connection closed before a message arrived");
    }
    serde_json::from_str(&line).context("malformed message")
}

// ─── Token ───────────────────────────────────────────────────────────────────

/// Derive a token from process-local entropy. It only has to be unguessable by
/// another local user within the daemon's lifetime, and it is stored in a
/// 0600 file, so a hash of pid + time + address entropy is sufficient.
fn generate_token() -> String {
    let mut h = Sha256::new();
    h.update(std::process::id().to_le_bytes());
    h.update(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
            .to_le_bytes(),
    );
    let stack_probe = &h as *const _ as usize;
    h.update(stack_probe.to_le_bytes());
    h.update(
        std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .unwrap_or_default()
            .as_bytes(),
    );
    format!("{:x}", h.finalize())
}

/// Constant-time comparison so a token cannot be recovered byte-by-byte.
fn tokens_match(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

// ─── Server ──────────────────────────────────────────────────────────────────

/// Envelope every client sends: the token plus the request.
#[derive(Debug, Serialize, Deserialize)]
struct Envelope {
    token: String,
    #[serde(flatten)]
    request: Request,
}

pub struct Server {
    listener: TcpListener,
    token: String,
    idle_timeout: Duration,
}

impl Server {
    pub fn bind(port: u16, idle_timeout: Duration) -> Result<Self> {
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let listener =
            TcpListener::bind(addr).with_context(|| format!("could not bind 127.0.0.1:{port}"))?;
        Ok(Server {
            listener,
            token: generate_token(),
            idle_timeout,
        })
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
                    stream.set_nonblocking(false).ok();
                    // A wedged client must not pin the model in memory forever.
                    stream.set_read_timeout(Some(Duration::from_secs(120))).ok();

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
        let envelope: Envelope = {
            let mut reader = BufReader::new(&stream);
            match recv(&mut reader) {
                Ok(e) => e,
                Err(e) => {
                    let _ = send(
                        &mut stream,
                        &Response::Error {
                            message: e.to_string(),
                        },
                    );
                    return false;
                }
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
        let mut reader = BufReader::new(&stream);
        recv(&mut reader)
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
        let a = generate_token();
        let b = generate_token();
        assert_eq!(a.len(), 64);
        assert_ne!(a, b, "two daemons must not share a token");
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
                languages: vec![],
                requirements: None,
                max_tokens: 128,
                min_confidence: 0.0,
                triage: "off".into(),
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
                languages: vec![],
                requirements: None,
                max_tokens: 16,
                min_confidence: 0.0,
                triage: "off".into(),
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
