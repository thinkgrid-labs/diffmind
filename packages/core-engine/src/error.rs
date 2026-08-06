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
