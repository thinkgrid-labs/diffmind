//! Backends that talk to a model server over HTTP.
//!
//! Local-first stays the default and the identity of the tool. This exists
//! because "local" and "in this process" are not the same thing: an Ollama or
//! vLLM instance on the same machine (or the same VPN) keeps the code exactly
//! as private while giving access to a model class diffmind will never ship as
//! a 20 GB download.

use crate::backend::{GenOptions, ReviewBackend};
use crate::error::EngineError;
use crate::prompt::Prompt;
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteProtocol {
    /// Ollama's native `/api/chat`.
    Ollama,
    /// Anything speaking OpenAI's `/chat/completions` — vLLM, LM Studio,
    /// llama.cpp's server, LiteLLM, text-generation-webui.
    OpenAiCompatible,
}

impl RemoteProtocol {
    pub fn parse(s: &str) -> Option<RemoteProtocol> {
        match s.trim().to_lowercase().as_str() {
            "ollama" => Some(RemoteProtocol::Ollama),
            "openai" | "openai-compatible" | "oai" => Some(RemoteProtocol::OpenAiCompatible),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            RemoteProtocol::Ollama => "ollama",
            RemoteProtocol::OpenAiCompatible => "openai-compatible",
        }
    }
}

pub struct RemoteBackend {
    client: reqwest::blocking::Client,
    protocol: RemoteProtocol,
    base_url: String,
    /// `base_url` with any `user:password@` and query string removed. This is
    /// the only form that is ever printed, written to a report or a run
    /// record, or folded into a cache key.
    display_url: String,
    model: String,
    api_key: Option<String>,
    context_tokens: usize,
    /// First retry delay; doubles per attempt. A field so tests need not sleep.
    retry_base: Duration,
    /// Resolved on first use: a server round-trip is not free and only the
    /// cache needs it.
    identity: std::sync::OnceLock<String>,
}

/// Default assumption for a remote model's context window. Deliberately
/// conservative: overrunning a server's real limit produces an opaque 400,
/// whereas under-using it only costs an extra chunk.
const DEFAULT_REMOTE_CONTEXT: usize = 8192;

/// How long to wait for a TCP connection. Separate from the overall timeout,
/// which has to allow minutes of generation: a server that is not there should
/// be reported in seconds, not after the full generation budget.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Largest response body accepted. A review is a few KB of JSON; anything near
/// this is a misbehaving server, and reading it unbounded is a memory DoS.
const MAX_RESPONSE_BYTES: u64 = 8 * 1024 * 1024;

/// Attempts per generation, including the first. Bounded so a flapping server
/// costs a known, small multiple of one request — never an open-ended wait.
const MAX_ATTEMPTS: u32 = 3;

/// Ceiling on a server-requested `Retry-After`.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(30);

/// Characters of a server error body worth showing.
const MAX_ERROR_DETAIL: usize = 300;

/// Strip credentials and query from a URL for display. Returns the input
/// unchanged if it does not parse (it will be rejected anyway).
pub fn redact_url(raw: &str) -> String {
    match reqwest::Url::parse(raw) {
        Ok(mut url) => {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.set_query(None);
            url.set_fragment(None);
            url.to_string().trim_end_matches('/').to_string()
        }
        Err(_) => raw.to_string(),
    }
}

fn is_loopback_host(url: &reqwest::Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// One attempt's failure, classified for the retry loop.
enum Attempt {
    Retry {
        message: String,
        after: Option<Duration>,
    },
    Fatal(String),
}

impl RemoteBackend {
    pub fn new(
        protocol: RemoteProtocol,
        base_url: &str,
        model: &str,
        api_key: Option<String>,
        context_tokens: Option<usize>,
        timeout: Duration,
    ) -> Result<Self, EngineError> {
        let config_err = |m: String| EngineError::Config(m);
        if model.trim().is_empty() {
            return Err(config_err(
                "--backend-model is required when using a remote backend".into(),
            ));
        }

        // Validate the URL here, where the message can be specific, rather
        // than on the first request in the middle of a review.
        let parsed = reqwest::Url::parse(base_url.trim()).map_err(|e| {
            config_err(format!(
                "invalid backend URL '{}': {e}",
                redact_url(base_url)
            ))
        })?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host().is_none() {
            return Err(config_err(format!(
                "backend URL '{}' must be an http:// or https:// address with a host",
                redact_url(base_url)
            )));
        }
        // An API key over plain HTTP to another machine is a credential sent in
        // clear text, along with the code. Loopback is exempt (nothing leaves
        // the host); anything else needs https, or an explicit opt-out.
        if api_key.is_some()
            && parsed.scheme() == "http"
            && !is_loopback_host(&parsed)
            && std::env::var("DIFFMIND_ALLOW_INSECURE_API_KEY").as_deref() != Ok("1")
        {
            return Err(config_err(format!(
                "refusing to send an API key over plain HTTP to {}. Use https://, a \
                 loopback address, or set DIFFMIND_ALLOW_INSECURE_API_KEY=1 to accept the risk.",
                redact_url(base_url)
            )));
        }

        let client = reqwest::blocking::Client::builder()
            .user_agent(concat!("diffmind/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(CONNECT_TIMEOUT.min(timeout))
            .timeout(timeout)
            // A redirect would re-send the code — and, cross-origin, possibly
            // the key — somewhere the user did not configure. Fail instead.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| EngineError::Backend {
                backend: protocol.as_str().into(),
                message: format!("could not build HTTP client: {}", e.without_url()),
            })?;

        let base = base_url.trim().trim_end_matches('/').to_string();
        Ok(RemoteBackend {
            client,
            protocol,
            display_url: redact_url(&base),
            base_url: base,
            model: model.to_string(),
            api_key,
            context_tokens: context_tokens.unwrap_or(DEFAULT_REMOTE_CONTEXT),
            retry_base: Duration::from_secs(1),
            identity: std::sync::OnceLock::new(),
        })
    }

    #[cfg(test)]
    fn with_retry_base(mut self, d: Duration) -> Self {
        self.retry_base = d;
        self
    }

    fn endpoint(&self) -> String {
        match self.protocol {
            RemoteProtocol::Ollama => format!("{}/api/chat", self.base_url),
            RemoteProtocol::OpenAiCompatible => format!("{}/chat/completions", self.base_url),
        }
    }

    fn body(&self, prompt: &Prompt, opts: &GenOptions) -> Value {
        let messages = json!([
            {"role": "system", "content": prompt.system},
            {"role": "user", "content": prompt.user},
        ]);

        match self.protocol {
            RemoteProtocol::Ollama => {
                let mut options = json!({
                    "temperature": opts.temperature,
                    "num_predict": opts.max_new_tokens,
                    "seed": opts.seed,
                    "repeat_penalty": opts.repeat_penalty,
                });
                if let Some(map) = options.as_object_mut()
                    && opts.repeat_penalty <= 1.0
                {
                    map.remove("repeat_penalty");
                }
                let mut body = json!({
                    "model": self.model,
                    "messages": messages,
                    "stream": false,
                    "options": options,
                });
                if opts.json {
                    body["format"] = json!("json");
                }
                body
            }
            RemoteProtocol::OpenAiCompatible => {
                let mut body = json!({
                    "model": self.model,
                    "messages": messages,
                    "temperature": opts.temperature,
                    "max_tokens": opts.max_new_tokens,
                    "seed": opts.seed,
                });
                if opts.json {
                    body["response_format"] = json!({"type": "json_object"});
                }
                body
            }
        }
    }

    fn extract_content(&self, body: &Value) -> Option<String> {
        match self.protocol {
            RemoteProtocol::Ollama => body
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_str())
                .map(str::to_string),
            RemoteProtocol::OpenAiCompatible => body
                .get("choices")
                .and_then(|c| c.get(0))
                .and_then(|c| c.get("message"))
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_str())
                .map(str::to_string),
        }
    }

    fn err(&self, message: impl Into<String>) -> EngineError {
        EngineError::Backend {
            backend: self.protocol.as_str().into(),
            message: message.into(),
        }
    }

    /// Text from the server, made safe to print: control characters removed,
    /// the API key masked if the server echoed it, and the length capped.
    fn sanitize_detail(&self, text: &str) -> String {
        let mut out: String = text
            .chars()
            .filter(|c| !c.is_control() || *c == ' ')
            .take(MAX_ERROR_DETAIL)
            .collect();
        if let Some(key) = &self.api_key
            && key.len() >= 4
        {
            out = out.replace(key.as_str(), "<redacted>");
        }
        out
    }

    /// Read a response body without trusting its size.
    fn read_bounded(&self, response: reqwest::blocking::Response) -> Result<String, Attempt> {
        use std::io::Read;
        let mut buf = Vec::new();
        response
            .take(MAX_RESPONSE_BYTES + 1)
            .read_to_end(&mut buf)
            .map_err(|e| Attempt::Retry {
                message: format!(
                    "the connection to {} was interrupted: {e}",
                    self.display_url
                ),
                after: None,
            })?;
        if buf.len() as u64 > MAX_RESPONSE_BYTES {
            return Err(Attempt::Fatal(format!(
                "response from {} exceeded {} MB",
                self.display_url,
                MAX_RESPONSE_BYTES / (1024 * 1024)
            )));
        }
        String::from_utf8(buf).map_err(|_| Attempt::Fatal("response was not valid UTF-8".into()))
    }

    fn attempt(&self, prompt: &Prompt, opts: &GenOptions) -> Result<String, Attempt> {
        let mut request = self
            .client
            .post(self.endpoint())
            .json(&self.body(prompt, opts));
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }

        let response = request.send().map_err(|e| {
            // reqwest's message embeds the full URL, credentials included;
            // `without_url` drops it and the redacted form is named instead.
            if e.is_connect() {
                Attempt::Retry {
                    message: format!(
                        "could not connect to {}. Is the server running?",
                        self.display_url
                    ),
                    after: None,
                }
            } else if e.is_timeout() {
                // Not retried: the full timeout has already been spent, and
                // retrying would multiply it.
                Attempt::Fatal(format!(
                    "timed out waiting for {}. Try a smaller model or raise --backend-timeout.",
                    self.display_url
                ))
            } else {
                Attempt::Retry {
                    message: format!(
                        "request to {} failed: {}",
                        self.display_url,
                        self.sanitize_detail(&e.without_url().to_string())
                    ),
                    after: None,
                }
            }
        })?;

        let status = response.status();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(|secs| Duration::from_secs(secs).min(MAX_RETRY_AFTER));
        let text = self.read_bounded(response)?;

        if !status.is_success() {
            let detail = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| {
                    v.get("error")
                        .map(|e| e.get("message").unwrap_or(e).to_string())
                })
                .unwrap_or_else(|| text.clone());
            let message = format!("HTTP {status}: {}", self.sanitize_detail(&detail));
            let transient = status == reqwest::StatusCode::TOO_MANY_REQUESTS
                || matches!(status.as_u16(), 500 | 502 | 503 | 504);
            return Err(if transient {
                Attempt::Retry {
                    message,
                    after: retry_after,
                }
            } else {
                Attempt::Fatal(message)
            });
        }

        let body: Value = serde_json::from_str(&text)
            .map_err(|e| Attempt::Fatal(format!("response was not JSON: {e}")))?;

        self.extract_content(&body)
            .ok_or_else(|| Attempt::Fatal("response contained no message content".to_string()))
    }

    /// Ask the server which exact model a name currently resolves to.
    ///
    /// Ollama reports a content digest per tag, so a re-pulled `qwen2.5-coder`
    /// gets a new identity and cannot replay the old weights' cached findings.
    /// OpenAI-compatible servers expose no revision; their identity says
    /// `unverified` so the limitation is visible wherever the identity is.
    fn resolve_identity(&self) -> String {
        let revision = match self.protocol {
            RemoteProtocol::Ollama => self
                .ollama_digest()
                .map(|d| format!("digest={d}"))
                .unwrap_or_else(|| "digest=unverified".into()),
            RemoteProtocol::OpenAiCompatible => "revision=unverified".into(),
        };
        format!(
            "{}|{}|{}|ctx={}|{revision}",
            self.protocol.as_str(),
            self.display_url,
            self.model,
            self.context_tokens
        )
    }

    fn ollama_digest(&self) -> Option<String> {
        let mut req = self
            .client
            .get(format!("{}/api/tags", self.base_url))
            .timeout(Duration::from_secs(5));
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req.send().ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let text = self.read_bounded(resp).ok()?;
        let tags: Value = serde_json::from_str(&text).ok()?;
        let wanted = if self.model.contains(':') {
            self.model.clone()
        } else {
            format!("{}:latest", self.model)
        };
        tags.get("models")?.as_array()?.iter().find_map(|m| {
            let name = m
                .get("name")
                .or_else(|| m.get("model"))
                .and_then(|n| n.as_str())?;
            (name == self.model || name == wanted)
                .then(|| m.get("digest").and_then(|d| d.as_str()))
                .flatten()
                .filter(|d| !d.is_empty() && d.len() <= 128)
                .map(str::to_string)
        })
    }
}

impl ReviewBackend for RemoteBackend {
    fn describe(&self) -> String {
        format!(
            "{} · {} ({})",
            self.model,
            self.protocol.as_str(),
            self.display_url
        )
    }

    fn context_tokens(&self) -> usize {
        self.context_tokens
    }

    fn cache_identity(&self) -> String {
        self.identity
            .get_or_init(|| self.resolve_identity())
            .clone()
    }

    fn supports_constrained_json(&self) -> bool {
        // The server is *asked* for JSON, but nothing guarantees it complies —
        // the caller still needs the repair path.
        false
    }

    fn generate(&mut self, prompt: &Prompt, opts: &GenOptions) -> Result<String, EngineError> {
        let mut last = String::new();
        for attempt in 0..MAX_ATTEMPTS {
            match self.attempt(prompt, opts) {
                Ok(text) => return Ok(text),
                Err(Attempt::Fatal(message)) => return Err(self.err(message)),
                Err(Attempt::Retry { message, after }) => {
                    last = message;
                    if attempt + 1 == MAX_ATTEMPTS {
                        break;
                    }
                    let backoff = self.retry_base.saturating_mul(1u32 << attempt);
                    std::thread::sleep(after.unwrap_or(backoff).min(MAX_RETRY_AFTER));
                }
            }
        }
        Err(self.err(format!("{last} (gave up after {MAX_ATTEMPTS} attempts)")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(protocol: RemoteProtocol) -> RemoteBackend {
        RemoteBackend::new(
            protocol,
            "http://localhost:11434/",
            "qwen2.5-coder:14b",
            None,
            None,
            Duration::from_secs(5),
        )
        .unwrap()
    }

    fn opts() -> GenOptions {
        GenOptions {
            max_new_tokens: 256,
            ..Default::default()
        }
    }

    #[test]
    fn requires_a_model_name() {
        let result = RemoteBackend::new(
            RemoteProtocol::Ollama,
            "http://x",
            "  ",
            None,
            None,
            Duration::from_secs(1),
        );
        let Err(err) = result else {
            panic!("an empty model name must be rejected");
        };
        assert!(err.to_string().contains("--backend-model"));
    }

    #[test]
    fn trailing_slash_does_not_double_up_in_the_endpoint() {
        assert_eq!(
            backend(RemoteProtocol::Ollama).endpoint(),
            "http://localhost:11434/api/chat"
        );
        assert_eq!(
            backend(RemoteProtocol::OpenAiCompatible).endpoint(),
            "http://localhost:11434/chat/completions"
        );
    }

    #[test]
    fn ollama_body_requests_json_and_pins_the_seed() {
        let b = backend(RemoteProtocol::Ollama);
        let body = b.body(
            &Prompt {
                system: "s".into(),
                user: "u".into(),
            },
            &opts(),
        );
        assert_eq!(body["format"], "json");
        assert_eq!(body["stream"], false);
        assert_eq!(body["options"]["num_predict"], 256);
        assert_eq!(body["options"]["seed"], crate::backend::DEFAULT_SEED);
        assert_eq!(body["messages"][0]["role"], "system");
    }

    #[test]
    fn openai_body_uses_response_format() {
        let b = backend(RemoteProtocol::OpenAiCompatible);
        let body = b.body(
            &Prompt {
                system: "s".into(),
                user: "u".into(),
            },
            &opts(),
        );
        assert_eq!(body["response_format"]["type"], "json_object");
        assert_eq!(body["max_tokens"], 256);
    }

    #[test]
    fn json_is_not_requested_when_disabled() {
        let b = backend(RemoteProtocol::Ollama);
        let body = b.body(
            &Prompt {
                system: "s".into(),
                user: "u".into(),
            },
            &GenOptions {
                json: false,
                ..opts()
            },
        );
        assert!(body.get("format").is_none());
    }

    #[test]
    fn extracts_content_from_each_protocol_shape() {
        let ollama = backend(RemoteProtocol::Ollama);
        let v: Value = serde_json::from_str(r#"{"message":{"content":"hi"}}"#).unwrap();
        assert_eq!(ollama.extract_content(&v).as_deref(), Some("hi"));

        let oai = backend(RemoteProtocol::OpenAiCompatible);
        let v: Value =
            serde_json::from_str(r#"{"choices":[{"message":{"content":"hi"}}]}"#).unwrap();
        assert_eq!(oai.extract_content(&v).as_deref(), Some("hi"));

        assert_eq!(oai.extract_content(&json!({"choices": []})), None);
    }

    // ── Failure handling against a scripted local HTTP server ─────────────

    use std::io::{Read as _, Write as _};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// One scripted reply. `Close` drops the connection without answering;
    /// `Stall` accepts and never answers.
    #[derive(Clone)]
    enum Reply {
        Http(u16, Vec<(&'static str, String)>, Vec<u8>),
        Close,
        Stall,
    }

    fn ok_ollama(content: &str) -> Reply {
        Reply::Http(
            200,
            vec![],
            serde_json::to_vec(&json!({"message": {"content": content}})).unwrap(),
        )
    }

    /// Serve `script` in order, one reply per connection; the last repeats.
    /// Returns the base URL and a counter of requests received.
    fn stub(script: Vec<Reply>) -> (String, Arc<AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        std::thread::spawn(move || {
            for (i, conn) in listener.incoming().enumerate() {
                let Ok(mut conn) = conn else { break };
                counter.fetch_add(1, Ordering::SeqCst);
                // Drain the request head and body (Content-Length is small).
                let mut buf = vec![0u8; 256 * 1024];
                let _ = conn.set_read_timeout(Some(Duration::from_millis(500)));
                let _ = conn.read(&mut buf);
                let reply = script
                    .get(i)
                    .or_else(|| script.last())
                    .cloned()
                    .unwrap_or(Reply::Close);
                match reply {
                    Reply::Close => drop(conn),
                    Reply::Stall => {
                        std::thread::sleep(Duration::from_secs(5));
                    }
                    Reply::Http(code, headers, body) => {
                        let mut head = format!(
                            "HTTP/1.1 {code} X\r\nContent-Length: {}\r\nConnection: close\r\n",
                            body.len()
                        );
                        for (k, v) in headers {
                            head.push_str(&format!("{k}: {v}\r\n"));
                        }
                        head.push_str("\r\n");
                        let _ = conn.write_all(head.as_bytes());
                        let _ = conn.write_all(&body);
                    }
                }
            }
        });
        (format!("http://{addr}"), hits)
    }

    fn live(url: &str, timeout: Duration) -> RemoteBackend {
        RemoteBackend::new(RemoteProtocol::Ollama, url, "m", None, None, timeout)
            .unwrap()
            .with_retry_base(Duration::from_millis(10))
    }

    fn prompt() -> Prompt {
        Prompt {
            system: "s".into(),
            user: "u".into(),
        }
    }

    #[test]
    fn a_rate_limit_is_retried_and_then_succeeds() {
        let (url, hits) = stub(vec![
            Reply::Http(
                429,
                vec![("Retry-After", "0".into())],
                b"slow down".to_vec(),
            ),
            Reply::Http(503, vec![], b"busy".to_vec()),
            ok_ollama("{\"findings\": []}"),
        ]);
        let mut b = live(&url, Duration::from_secs(5));
        let out = b
            .generate(&prompt(), &opts())
            .expect("third attempt succeeds");
        assert!(out.contains("findings"));
        assert_eq!(hits.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn server_errors_are_retried_a_bounded_number_of_times() {
        let (url, hits) = stub(vec![Reply::Http(
            500,
            vec![],
            b"{\"error\":\"boom\"}".to_vec(),
        )]);
        let mut b = live(&url, Duration::from_secs(5));
        let err = b.generate(&prompt(), &opts()).unwrap_err().to_string();
        assert!(
            err.contains("HTTP 500") && err.contains("gave up after 3"),
            "{err}"
        );
        assert_eq!(hits.load(Ordering::SeqCst), MAX_ATTEMPTS as usize);
    }

    #[test]
    fn a_client_error_is_not_retried() {
        let (url, hits) = stub(vec![Reply::Http(
            400,
            vec![],
            b"{\"error\":{\"message\":\"context length exceeded\"}}".to_vec(),
        )]);
        let mut b = live(&url, Duration::from_secs(5));
        let err = b.generate(&prompt(), &opts()).unwrap_err().to_string();
        assert!(err.contains("context length exceeded"), "{err}");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_dropped_connection_is_retried() {
        let (url, hits) = stub(vec![Reply::Close, ok_ollama("{}")]);
        let mut b = live(&url, Duration::from_secs(5));
        b.generate(&prompt(), &opts())
            .expect("retry after a dropped connection");
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn invalid_json_and_missing_fields_are_reported_not_retried() {
        let (url, hits) = stub(vec![Reply::Http(
            200,
            vec![],
            b"<html>proxy page</html>".to_vec(),
        )]);
        let mut b = live(&url, Duration::from_secs(5));
        let err = b.generate(&prompt(), &opts()).unwrap_err().to_string();
        assert!(err.contains("not JSON"), "{err}");
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        let (url, _) = stub(vec![Reply::Http(
            200,
            vec![],
            b"{\"unexpected\": true}".to_vec(),
        )]);
        let mut b = live(&url, Duration::from_secs(5));
        let err = b.generate(&prompt(), &opts()).unwrap_err().to_string();
        assert!(err.contains("no message content"), "{err}");
    }

    #[test]
    fn an_oversized_response_is_refused() {
        let huge = vec![b'x'; (MAX_RESPONSE_BYTES + 1024) as usize];
        let (url, _) = stub(vec![Reply::Http(200, vec![], huge)]);
        let mut b = live(&url, Duration::from_secs(20));
        let err = b.generate(&prompt(), &opts()).unwrap_err().to_string();
        assert!(err.contains("exceeded"), "{err}");
    }

    #[test]
    fn an_extremely_large_error_body_is_truncated_in_the_message() {
        let body = vec![b'e'; 200_000];
        let (url, _) = stub(vec![Reply::Http(418, vec![], body)]);
        let mut b = live(&url, Duration::from_secs(5));
        let err = b.generate(&prompt(), &opts()).unwrap_err().to_string();
        assert!(err.len() < 1000, "error message was {} bytes", err.len());
    }

    #[test]
    fn a_stalled_server_times_out_without_retrying() {
        let (url, hits) = stub(vec![Reply::Stall]);
        let mut b = live(&url, Duration::from_millis(500));
        let started = std::time::Instant::now();
        let err = b.generate(&prompt(), &opts()).unwrap_err().to_string();
        assert!(err.contains("timed out"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(4));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn an_unreachable_server_is_named_and_bounded() {
        // Bind and drop, so the port is very likely closed.
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut b = live(&format!("http://127.0.0.1:{port}"), Duration::from_secs(5));
        let started = std::time::Instant::now();
        let err = b.generate(&prompt(), &opts()).unwrap_err().to_string();
        assert!(err.contains("could not connect"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_redirect_is_not_followed() {
        let (url, hits) = stub(vec![Reply::Http(
            307,
            vec![("Location", "http://127.0.0.1:9/elsewhere".into())],
            vec![],
        )]);
        let mut b = live(&url, Duration::from_secs(5));
        let err = b.generate(&prompt(), &opts()).unwrap_err().to_string();
        assert!(err.contains("307"), "{err}");
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn credentials_never_reach_a_label_or_an_error() {
        let b = RemoteBackend::new(
            RemoteProtocol::OpenAiCompatible,
            "https://user:hunter2@llm.example.invalid/v1?api_key=sk-live-123",
            "m",
            Some("sk-secret-value".into()),
            None,
            Duration::from_secs(1),
        )
        .unwrap();
        let label = b.describe();
        assert!(
            !label.contains("hunter2") && !label.contains("sk-live-123"),
            "{label}"
        );
        let id = b.cache_identity();
        assert!(
            !id.contains("hunter2") && !id.contains("sk-live-123"),
            "{id}"
        );
        assert_eq!(
            b.sanitize_detail("bad key sk-secret-value\u{1b}[31m"),
            "bad key <redacted>[31m"
        );
    }

    #[test]
    fn invalid_urls_are_rejected_up_front() {
        for bad in ["not a url", "ftp://host/x", "file:///etc/passwd", "http://"] {
            let r = RemoteBackend::new(
                RemoteProtocol::Ollama,
                bad,
                "m",
                None,
                None,
                Duration::from_secs(1),
            );
            assert!(r.is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn an_api_key_is_not_sent_over_plain_http_to_another_host() {
        let r = RemoteBackend::new(
            RemoteProtocol::OpenAiCompatible,
            "http://llm.example.invalid/v1",
            "m",
            Some("k".into()),
            None,
            Duration::from_secs(1),
        );
        let Err(e) = r else {
            panic!("a cleartext key to a remote host must be refused")
        };
        assert!(e.to_string().contains("plain HTTP"));
        // Loopback and https are fine.
        for ok in [
            "http://127.0.0.1:8000/v1",
            "http://localhost:8000/v1",
            "https://llm.example.invalid/v1",
        ] {
            assert!(
                RemoteBackend::new(
                    RemoteProtocol::OpenAiCompatible,
                    ok,
                    "m",
                    Some("k".into()),
                    None,
                    Duration::from_secs(1)
                )
                .is_ok(),
                "{ok}"
            );
        }
    }

    #[test]
    fn the_cache_identity_tracks_the_ollama_model_digest() {
        let tags = |digest: &str| {
            Reply::Http(
                200,
                vec![],
                serde_json::to_vec(&json!({"models": [
                    {"name": "other:latest", "digest": "zzz"},
                    {"name": "m:latest", "digest": digest},
                ]}))
                .unwrap(),
            )
        };
        let (url, _) = stub(vec![tags("aaa111")]);
        let a = live(&url, Duration::from_secs(5)).cache_identity();
        let (url2, _) = stub(vec![tags("bbb222")]);
        let b = live(&url2, Duration::from_secs(5)).cache_identity();
        assert!(a.contains("digest=aaa111"), "{a}");
        assert!(b.contains("digest=bbb222"), "{b}");

        // A server that cannot say is labelled as such, never guessed.
        let (url3, _) = stub(vec![Reply::Http(404, vec![], vec![])]);
        let c = live(&url3, Duration::from_secs(5)).cache_identity();
        assert!(c.contains("digest=unverified"), "{c}");
        let oai = backend(RemoteProtocol::OpenAiCompatible).cache_identity();
        assert!(oai.contains("revision=unverified"), "{oai}");
    }

    #[test]
    fn protocol_parsing_accepts_the_documented_aliases() {
        assert_eq!(
            RemoteProtocol::parse("ollama"),
            Some(RemoteProtocol::Ollama)
        );
        assert_eq!(
            RemoteProtocol::parse("openai-compatible"),
            Some(RemoteProtocol::OpenAiCompatible)
        );
        assert_eq!(RemoteProtocol::parse("nope"), None);
    }
}
