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
    model: String,
    api_key: Option<String>,
    context_tokens: usize,
}

/// Default assumption for a remote model's context window. Deliberately
/// conservative: overrunning a server's real limit produces an opaque 400,
/// whereas under-using it only costs an extra chunk.
const DEFAULT_REMOTE_CONTEXT: usize = 8192;

impl RemoteBackend {
    pub fn new(
        protocol: RemoteProtocol,
        base_url: &str,
        model: &str,
        api_key: Option<String>,
        context_tokens: Option<usize>,
        timeout: Duration,
    ) -> Result<Self, EngineError> {
        if model.trim().is_empty() {
            return Err(EngineError::Config(
                "--backend-model is required when using a remote backend".into(),
            ));
        }

        let client = reqwest::blocking::Client::builder()
            .user_agent(concat!("diffmind/", env!("CARGO_PKG_VERSION")))
            .timeout(timeout)
            .build()
            .map_err(|e| EngineError::Backend {
                backend: protocol.as_str().into(),
                message: format!("could not build HTTP client: {e}"),
            })?;

        Ok(RemoteBackend {
            client,
            protocol,
            base_url: base_url.trim_end_matches('/').to_string(),
            model: model.to_string(),
            api_key,
            context_tokens: context_tokens.unwrap_or(DEFAULT_REMOTE_CONTEXT),
        })
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
}

impl ReviewBackend for RemoteBackend {
    fn describe(&self) -> String {
        format!(
            "{} · {} ({})",
            self.model,
            self.protocol.as_str(),
            self.base_url
        )
    }

    fn context_tokens(&self) -> usize {
        self.context_tokens
    }

    fn supports_constrained_json(&self) -> bool {
        // The server is *asked* for JSON, but nothing guarantees it complies —
        // the caller still needs the repair path.
        false
    }

    fn generate(&mut self, prompt: &Prompt, opts: &GenOptions) -> Result<String, EngineError> {
        let mut request = self
            .client
            .post(self.endpoint())
            .json(&self.body(prompt, opts));
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }

        let response = request.send().map_err(|e| {
            // A connection refusal here almost always means "the server isn't
            // running", which is worth saying out loud rather than dumping a
            // reqwest chain on the user.
            if e.is_connect() {
                self.err(format!(
                    "could not connect to {}. Is the server running?",
                    self.base_url
                ))
            } else if e.is_timeout() {
                self.err(format!(
                    "timed out waiting for {}. Try a smaller model or raise --backend-timeout.",
                    self.base_url
                ))
            } else {
                self.err(e.to_string())
            }
        })?;

        let status = response.status();
        let text = response.text().map_err(|e| self.err(e.to_string()))?;

        if !status.is_success() {
            let detail = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| {
                    v.get("error")
                        .map(|e| e.get("message").unwrap_or(e).to_string())
                })
                .unwrap_or_else(|| text.chars().take(300).collect());
            return Err(self.err(format!("HTTP {status}: {detail}")));
        }

        let body: Value = serde_json::from_str(&text)
            .map_err(|e| self.err(format!("response was not JSON: {e}")))?;

        self.extract_content(&body)
            .ok_or_else(|| self.err("response contained no message content".to_string()))
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
