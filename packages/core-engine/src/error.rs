use thiserror::Error;

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
}

impl From<std::io::Error> for EngineError {
    fn from(e: std::io::Error) -> Self {
        EngineError::Io(e.to_string())
    }
}
