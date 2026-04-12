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
    /// The diff does not satisfy a requirement from the provided user story
    /// or acceptance criteria.
    Compliance,
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
    /// Comma-separated language names detected from the diff (e.g. "Rust, TypeScript").
    /// Injected into the system prompt so the model uses the right idioms.
    languages: Option<String>,
    /// User story / acceptance criteria from the feature ticket.
    /// When set the model also checks whether the diff satisfies the requirements.
    requirements: Option<String>,
}

/// Hard upper bound on total tokens (prompt + generated) fed to the model.
/// Qwen2.5-Coder GGUF variants support 4096 tokens; exceeding this causes a
/// candle panic or garbage output with no clean error.
const MAX_CONTEXT_TOKENS: usize = 4096;

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
            languages: None,
            requirements: None,
        })
    }

    /// Set the detected languages for this session. The names are injected into
    /// the system prompt so the model applies language-appropriate review idioms.
    ///
    /// # Example
    /// ```ignore
    /// let analyzer = ReviewAnalyzer::new(&model, &tok)?
    ///     .with_languages(vec!["Rust".into(), "TypeScript".into()]);
    /// ```
    pub fn with_languages(mut self, langs: Vec<String>) -> Self {
        if !langs.is_empty() {
            self.languages = Some(langs.join(", "));
        }
        self
    }

    /// Provide the user story / acceptance criteria from the feature ticket.
    /// The model will flag any requirements that are missing or incorrectly
    /// implemented in the diff as `category: "compliance"` findings.
    pub fn with_requirements(mut self, requirements: String) -> Self {
        if !requirements.trim().is_empty() {
            self.requirements = Some(requirements);
        }
        self
    }

    pub fn analyze_diff_chunked(
        &mut self,
        diff: &str,
        context: &str,
        max_tokens_per_chunk: u32,
    ) -> Result<Vec<ReviewFinding>, EngineError> {
        // chunk_diff already splits on every `diff --git` boundary (one chunk per
        // file).  The secondary line limit is only hit for very large single-file
        // diffs.  300 lines is a better default than 150: most files fit in one
        // inference pass, and the early JSON-complete exit in `generate()` means
        // processing stops as soon as the answer array is closed.
        const MAX_CHUNK_LINES: usize = 300;
        let chunks = chunk_diff(diff, MAX_CHUNK_LINES);
        let mut all_findings: Vec<ReviewFinding> = Vec::new();

        for chunk in chunks {
            if chunk.trim().is_empty() {
                continue;
            }
            match self.analyze_diff_internal(&chunk, context, max_tokens_per_chunk as usize) {
                Ok(findings) => all_findings.extend(findings),
                // Small models often produce malformed JSON for a single chunk.
                // Skip the chunk and continue rather than aborting the whole run.
                Err(EngineError::SerializationError(_)) => {}
                Err(e) => return Err(e),
            }
        }

        Ok(all_findings)
    }

    fn analyze_diff_internal(
        &mut self,
        diff: &str,
        context: &str,
        max_new_tokens: usize,
    ) -> Result<Vec<ReviewFinding>, EngineError> {
        if diff.trim().is_empty() {
            return Ok(Vec::new());
        }

        let prompt = self.format_prompt(diff, context);
        let response = self.generate(&prompt, max_new_tokens)?;

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
        // Guard against large RAG/business context exhausting the token budget.
        // ~2 000 bytes ≈ 500 tokens at 4 bytes/token leaves headroom for the diff.
        const MAX_CONTEXT_BYTES: usize = 2000;
        let context = truncate_to_char_boundary(context, MAX_CONTEXT_BYTES);

        let context_section = if context.is_empty() {
            String::new()
        } else {
            format!("\n\n### Business Context / Requirements:\n{}\n", context)
        };

        let stack = self
            .languages
            .as_deref()
            .unwrap_or("TypeScript, JavaScript, Rust, Go, Python");

        // Requirements section — only present when a ticket was provided
        let requirements_section = match &self.requirements {
            Some(req) => {
                const MAX_REQ_BYTES: usize = 2000;
                let truncated = truncate_to_char_boundary(req, MAX_REQ_BYTES);
                format!("\n\n### User Story / Acceptance Criteria:\n{}\n", truncated)
            }
            None => String::new(),
        };

        // Compliance focus line — only added when requirements are present
        let compliance_focus = if self.requirements.is_some() {
            "\n5. **Compliance**: Does the diff satisfy every acceptance criterion? Flag missing or incomplete requirements as category \"compliance\"."
        } else {
            ""
        };

        // Extend the category list in the JSON schema hint when compliance is active
        let category_hint = if self.requirements.is_some() {
            "\"security\"|\"quality\"|\"performance\"|\"maintainability\"|\"compliance\""
        } else {
            "\"security\"|\"quality\"|\"performance\"|\"maintainability\""
        };

        format!(
            "<|im_start|>system\nYou are an expert Senior Software Engineer and Code Reviewer. Analyze the git diff and provide a comprehensive code review for {} code.{}{} \n\nFocus on:\n1. **Security**: Vulnerabilities, secrets, insecure handling.\n2. **Quality**: Bugs, anti-patterns, logical errors.\n3. **Performance**: Bottlenecks, inefficient code.\n4. **Maintainability**: Readability, naming, complexity.{}\n\nReturn a JSON array ONLY. Format: [{{ \"file\": \"path\", \"line\": 12, \"severity\": \"high\"|\"medium\"|\"low\", \"category\": {}, \"issue\": \"description\", \"suggested_fix\": \"code\" }}]\nIf no issues, return [].<|im_end|>\n<|im_start|>user\nAnalyze this diff:\n{}\n<|im_end|>\n<|im_start|>assistant\n",
            stack,
            requirements_section,
            context_section,
            compliance_focus,
            category_hint,
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

        if tokens_ids.len() >= MAX_CONTEXT_TOKENS {
            return Err(EngineError::ForwardError(format!(
                "prompt is too long ({} tokens, limit {}); reduce diff size or context",
                tokens_ids.len(),
                MAX_CONTEXT_TOKENS
            )));
        }
        // Cap output so prompt + generated never exceeds MAX_CONTEXT_TOKENS.
        let max_new = max_len.min(MAX_CONTEXT_TOKENS - tokens_ids.len());

        let mut logits_processor = LogitsProcessor::new(rand::random::<u64>(), Some(0.1), None);
        let mut generated_text = String::new();

        let eos_token_id = self.tokenizer.token_to_id("<|im_end|>");
        let alt_eos_token_id = self.tokenizer.token_to_id("<|endoftext|>");

        for i in 0..max_new {
            // Defensive: stop if we somehow reach the limit mid-generation.
            if tokens_ids.len() >= MAX_CONTEXT_TOKENS {
                break;
            }
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

            // Defensive check for GQA/RoPE artifacts appearing as empty tensors
            if logits.elem_count() == 0 {
                break;
            }

            let logits = logits.squeeze(0).map_err(EngineError::TensorError)?;

            // The model might return [batch, vocab] (2D) or [batch, seq, vocab] (3D).
            // After squeeze(0), we expect either [vocab] (1D) or [seq, vocab] (2D).
            let logits = match logits.dims().len() {
                1 => logits,
                2 => {
                    let n_rows = logits.dim(0)?;
                    if n_rows == 0 {
                        break;
                    }
                    logits.get(n_rows - 1).map_err(EngineError::TensorError)?
                }
                _ => {
                    return Err(EngineError::ForwardError(format!(
                        "unexpected logits shape: {:?}",
                        logits.dims()
                    )))
                }
            };

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

            // Stop as soon as the JSON array is syntactically complete.
            // The model has produced its answer; continuing only generates
            // commentary or repeated tokens that are discarded anyway.
            if json_array_complete(&generated_text) {
                break;
            }
        }

        Ok(generated_text)
    }
}

/// Returns `true` once `s` contains a syntactically-complete JSON array
/// (`[…]` with balanced brackets, respecting string literals and escapes).
///
/// This lets `generate()` exit early as soon as the model has closed the
/// answer array, instead of running until the token cap.
fn json_array_complete(s: &str) -> bool {
    let trimmed = s.trim_start();
    if !trimmed.starts_with('[') {
        return false;
    }
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escape = false;
    for ch in trimmed.chars() {
        if escape {
            escape = false;
            continue;
        }
        match ch {
            '\\' if in_string => escape = true,
            '"' => in_string = !in_string,
            '[' if !in_string => depth += 1,
            ']' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

/// Truncates `s` to at most `max_bytes` bytes, stepping back to the nearest
/// valid UTF-8 character boundary so the result is always valid UTF-8.
fn truncate_to_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
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

        // Truncate extremely long lines (e.g. minified code) to prevent OOM
        if line.len() > 2048 {
            current.push_str(&line[..2048]);
            current.push_str("... [line truncated]");
        } else {
            current.push_str(line);
        }

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
