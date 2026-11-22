use thiserror::Error;

use crate::analyzer::AnalysisStats;
use crate::types::ReviewSummary;

/// What a review produced before the backend failed: the deterministic
/// findings and every unit reviewed so far, with the units that were not.
///
/// Carried inside the error so that any caller using `?` still fails exactly
/// as before — a backend failure never silently becomes a clean result — while
/// a caller that knows what to do with a partial report can recover it.
#[derive(Debug)]
pub struct PartialReview {
    pub summary: ReviewSummary,
    pub stats: AnalysisStats,
}

#[derive(Error, Debug)]
pub enum EngineError {
    #[error("failed to load tokenizer: {0}")]
    TokenizerError(String),
    #[error("failed to read gguf file: {0}")]
    GgufError(String),
    #[error("failed to load model weights: {0}")]
    ModelLoadError(String),
    #[error("{0}")]
    DeviceUnavailable(String),
    #[error("tensor error: {0}")]
    TensorError(#[from] candle_core::Error),
    #[error("forward pass error: {0}")]
    ForwardError(String),
    #[error("sampling error: {0}")]
    SamplingError(String),
    #[error("serialization error: {0}")]
    SerializationError(String),
    /// A review unit could not be made to fit the model's context window, even
    /// after splitting and after shrinking everything optional in the prompt.
    ///
    /// Reported as an error so the analyzer can tell it apart from a chunk that
    /// merely came back unparseable, but it is *counted and skipped* rather than
    /// failing the run: one unreviewable hunk must not discard the findings
    /// every other hunk produced.
    #[error("review unit does not fit the context window: {0}")]
    UnitTooLarge(String),
    #[error("io error: {0}")]
    Io(String),
    /// A remote backend (Ollama, OpenAI-compatible) failed to answer.
    #[error("backend '{backend}' request failed: {message}")]
    Backend { backend: String, message: String },
    #[error("configuration error: {0}")]
    Config(String),
    /// The backend failed part-way through a review. The review is incomplete
    /// and must be reported as a failure; the partial results are attached for
    /// diagnostics. `stats.backend_error` holds the underlying message.
    #[error("review incomplete: {}", .0.stats.backend_error.as_deref().unwrap_or("backend failed"))]
    Incomplete(Box<PartialReview>),
}

impl From<std::io::Error> for EngineError {
    fn from(e: std::io::Error) -> Self {
        EngineError::Io(e.to_string())
    }
}
