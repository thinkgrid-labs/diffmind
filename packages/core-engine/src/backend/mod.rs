//! Pluggable inference backends.
//!
//! diffmind's value is the pipeline around the model — diff parsing,
//! deterministic detectors, anchoring, suppressions, output formats, CI
//! plumbing — none of which depends on *where* the tokens come from. Hard-wiring
//! it to one 1.5B GGUF capped the ceiling: plenty of users already run a warm
//! 14B in Ollama, and self-hosted vLLM keeps the privacy story fully intact.

use crate::error::EngineError;
use crate::prompt::Prompt;

pub mod candle_backend;
pub mod remote;

pub use candle_backend::{CandleBackend, DevicePreference, resolve_device};
pub use remote::{RemoteBackend, RemoteProtocol};

/// Knobs a caller can turn per generation.
#[derive(Debug, Clone)]
pub struct GenOptions {
    pub max_new_tokens: usize,
    /// 0.0 means greedy. Greedy plus constrained decoding is the reproducible
    /// default: the same diff must produce the same findings, or a CI gate
    /// flaps and gets disabled.
    pub temperature: f64,
    pub seed: u64,
    /// Constrain output to a single valid JSON document.
    pub json: bool,
    /// Penalty applied to recently-emitted tokens; 1.0 disables it.
    pub repeat_penalty: f32,
    /// How far back the repeat penalty looks.
    pub repeat_last_n: usize,
    pub debug: bool,
}

impl Default for GenOptions {
    fn default() -> Self {
        GenOptions {
            max_new_tokens: 1024,
            temperature: 0.0,
            seed: DEFAULT_SEED,
            json: true,
            repeat_penalty: 1.1,
            repeat_last_n: 64,
            debug: false,
        }
    }
}

/// Fixed by default so two runs over the same diff agree. `rand::random()` was
/// previously used here, which made every review non-reproducible: the same
/// branch could pass CI once and fail the next time with no code change.
pub const DEFAULT_SEED: u64 = u64::from_be_bytes(*b"diffmind");

pub trait ReviewBackend: Send {
    fn generate(&mut self, prompt: &Prompt, opts: &GenOptions) -> Result<String, EngineError>;

    /// Human-readable label for the header line, e.g.
    /// "Qwen2.5-Coder-1.5B · Q4_K_M · Metal".
    fn describe(&self) -> String;

    /// Usable context window in tokens.
    fn context_tokens(&self) -> usize;

    /// Exact token count when the backend owns a tokenizer, else `None`.
    fn count_tokens(&self, _text: &str) -> Option<usize> {
        None
    }

    /// Whether this backend enforces JSON structurally during decoding.
    /// When false the caller must be ready to repair the output.
    fn supports_constrained_json(&self) -> bool {
        false
    }
}

/// How many prompt tokens to leave for the response.
pub fn budget_for_prompt(context_tokens: usize, max_new: usize) -> usize {
    context_tokens.saturating_sub(max_new).max(512)
}
