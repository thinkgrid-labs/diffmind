use candle_core::quantized::gguf_file;
use candle_core::{Device, Tensor};
use candle_transformers::models::quantized_qwen2::ModelWeights as Qwen2;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum EngineError {
    #[error("failed to load tokenizer: {0}")]
    TokenizerError(String),
    #[error("failed to read gguf file: {0}")]
    GgufError(String),
    #[error("failed to load model weights: {0}")]
    ModelLoadError(String),
    #[error("tensor error: {0}")]
    TensorError(#[from] candle_core::Error),
    #[error("forward pass error: {0}")]
    ForwardError(String),
    #[error("sampling error: {0}")]
    SamplingError(String),
    #[error("serialization error: {0}")]
    SerializationError(String),
}

// ─── Output Types ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    Security,
    Quality,
    Performance,
    Maintainability,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewFinding {
    pub file: String,
    pub line: u32,
    pub severity: Severity,
    pub category: Category,
    pub issue: String,
    pub suggested_fix: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
}

// ─── ReviewAnalyzer ──────────────────────────────────────────────────────────

pub struct ReviewAnalyzer {
    device: Device,
    model: Qwen2,
    tokenizer: tokenizers::Tokenizer,
}

impl ReviewAnalyzer {
    pub fn new(model_bytes: &[u8], tokenizer_bytes: &[u8]) -> Result<Self, EngineError> {
        let device = Device::Cpu;

        let tokenizer = tokenizers::Tokenizer::from_bytes(tokenizer_bytes)
            .map_err(|e| EngineError::TokenizerError(e.to_string()))?;

        let mut reader = std::io::Cursor::new(model_bytes);
        let gguf = gguf_file::Content::read(&mut reader)
            .map_err(|e| EngineError::GgufError(e.to_string()))?;

        let model = Qwen2::from_gguf(gguf, &mut reader, &device)
            .map_err(|e| EngineError::ModelLoadError(e.to_string()))?;

        Ok(ReviewAnalyzer {
            device,
            model,
            tokenizer,
        })
    }

    pub fn analyze_diff_chunked(
        &mut self,
        diff: &str,
        context: &str,
        _max_tokens_per_chunk: u32,
    ) -> Result<Vec<ReviewFinding>, EngineError> {
        let chunks = chunk_diff(diff, 150);
        let mut all_findings: Vec<ReviewFinding> = Vec::new();

        for chunk in chunks {
            if chunk.trim().is_empty() {
                continue;
            }
            let chunk_findings = self.analyze_diff_internal(&chunk, context)?;
            all_findings.extend(chunk_findings);
        }

        Ok(all_findings)
    }

    fn analyze_diff_internal(
        &mut self,
        diff: &str,
        context: &str,
    ) -> Result<Vec<ReviewFinding>, EngineError> {
        if diff.trim().is_empty() {
            return Ok(Vec::new());
        }

        let prompt = self.format_prompt(diff, context);
        let response = self.generate(&prompt, 512)?;

        let json_start = response.find('[').unwrap_or(0);
        let json_end = response
            .rfind(']')
            .unwrap_or_else(|| response.len().saturating_sub(1));

        if json_end <= json_start || json_end >= response.len() {
            return Ok(Vec::new());
        }

        let json_slice = &response[json_start..=json_end];
        let findings: Vec<ReviewFinding> = serde_json::from_str(json_slice)
            .map_err(|e| EngineError::SerializationError(e.to_string()))?;

        Ok(findings)
    }

    fn format_prompt(&self, diff: &str, context: &str) -> String {
        let context_section = if context.is_empty() {
            String::new()
        } else {
            format!("\n\n### Business Context / Requirements:\n{}\n", context)
        };

        format!(
            "<|im_start|>system\nYou are an expert Senior Software Engineer and Code Reviewer. Analyze the git diff and provide a comprehensive code review for TypeScript, NestJS, and React Native code.{} \n\nFocus on:\n1. **Security**: Vulnerabilities, secrets, insecure handling.\n2. **Quality**: Bugs, anti-patterns, logical errors.\n3. **Performance**: Bottlenecks, inefficient code.\n4. **Maintainability**: Readability, naming, complexity.\n\nReturn a JSON array ONLY. Format: [{{ \"file\": \"path\", \"line\": 12, \"severity\": \"high\"|\"medium\"|\"low\", \"category\": \"security\"|\"quality\"|\"performance\"|\"maintainability\", \"issue\": \"description\", \"suggested_fix\": \"code\" }}]\nIf no issues, return [].<|im_end|>\n<|im_start|>user\nAnalyze this diff:\n{}\n<|im_end|>\n<|im_start|>assistant\n",
            context_section,
            diff
        )
    }

    fn generate(&mut self, prompt: &str, max_len: usize) -> Result<String, EngineError> {
        use candle_transformers::generation::LogitsProcessor;

        let tokens = self
            .tokenizer
            .encode(prompt, true)
            .map_err(|e| EngineError::TokenizerError(e.to_string()))?;
        let mut tokens_ids = tokens.get_ids().to_vec();

        let mut logits_processor = LogitsProcessor::new(1337, Some(0.1), None);
        let mut generated_text = String::new();

        let eos_token_id = self.tokenizer.token_to_id("<|im_end|>");
        let alt_eos_token_id = self.tokenizer.token_to_id("<|endoftext|>");

        for i in 0..max_len {
            let context_size = if i > 0 { 1 } else { tokens_ids.len() };
            let start_pos = tokens_ids.len().saturating_sub(context_size);
            let input = Tensor::new(&tokens_ids[start_pos..], &self.device)
                .map_err(EngineError::TensorError)?
                .unsqueeze(0)
                .map_err(EngineError::TensorError)?;

            let logits = self
                .model
                .forward(&input, tokens_ids.len() - context_size)
                .map_err(|e| EngineError::ForwardError(e.to_string()))?;
            let logits = logits.squeeze(0).map_err(EngineError::TensorError)?;
            let logits = logits
                .get(logits.dim(0)? - 1)
                .map_err(EngineError::TensorError)?;

            let next_token = logits_processor
                .sample(&logits)
                .map_err(|e| EngineError::SamplingError(e.to_string()))?;

            tokens_ids.push(next_token);

            if Some(next_token) == eos_token_id || Some(next_token) == alt_eos_token_id {
                break;
            }

            let decoded = self
                .tokenizer
                .decode(&[next_token], true)
                .map_err(|e| EngineError::TokenizerError(e.to_string()))?;
            generated_text.push_str(&decoded);
        }

        Ok(generated_text)
    }
}

fn chunk_diff(diff: &str, max_lines: usize) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::with_capacity(4096);
    let mut line_count = 0;

    for line in diff.lines() {
        if (line.starts_with("diff --git") || line_count >= max_lines) && !current.is_empty() {
            chunks.push(std::mem::take(&mut current));
            line_count = 0;
        }
        current.push_str(line);
        current.push('\n');
        line_count += 1;
    }

    if !current.is_empty() {
        chunks.push(current);
    }

    if chunks.is_empty() && !diff.is_empty() {
        vec![diff.to_string()]
    } else {
        chunks
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chunk_diff_basic() {
        let diff = "line1\nline2\nline3\nline4\nline5";
        let chunks = chunk_diff(diff, 2);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0], "line1\nline2\n");
        assert_eq!(chunks[1], "line3\nline4\n");
        assert_eq!(chunks[2], "line5\n");
    }

    #[test]
    fn test_chunk_diff_with_git_header() {
        let diff = "diff --git a/file1.js b/file1.js\n+added line\ndiff --git a/file2.js b/file2.js\n-removed line";
        // Even with max_lines = 100, it should split at "diff --git"
        let chunks = chunk_diff(diff, 100);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].contains("file1.js"));
        assert!(chunks[1].contains("file2.js"));
    }

    #[test]
    fn test_chunk_diff_empty() {
        let chunks = chunk_diff("", 10);
        assert!(chunks.is_empty());
    }

    #[test]
    fn test_chunk_diff_single_line() {
        let chunks = chunk_diff("only one line", 10);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0], "only one line\n");
    }
}
