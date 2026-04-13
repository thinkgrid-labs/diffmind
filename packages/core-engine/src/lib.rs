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

/// The complete result of analyzing one or more diff chunks.
/// Always populated — even when there are no bug findings, `positives` and
/// `suggestions` let the reviewer know what looks good and what could improve.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReviewSummary {
    /// Bugs, vulnerabilities, and code quality issues (may be empty).
    #[serde(default)]
    pub findings: Vec<ReviewFinding>,
    /// Things done well in this diff — at least one entry when the code is reasonable.
    #[serde(default)]
    pub positives: Vec<String>,
    /// Low-priority optional improvements that are not bugs.
    #[serde(default)]
    pub suggestions: Vec<String>,
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
    /// When true, print the raw model output and token counts to stderr for each chunk.
    debug: bool,
}

/// Hard upper bound on total tokens (prompt + generated) fed to the model.
/// Qwen2.5-Coder GGUF variants support 4096 tokens; exceeding this causes a
/// candle panic or garbage output with no clean error.
const MAX_CONTEXT_TOKENS: usize = 4096;

/// Which compute device to use for inference.
#[derive(Debug, Clone, Default)]
pub enum DevicePreference {
    /// Try Metal on macOS, fall back to CPU everywhere else.
    #[default]
    Auto,
    /// Force CPU inference on all platforms.
    Cpu,
    /// Force Metal (macOS / Apple Silicon). Returns an error on other platforms.
    Metal,
}

/// Select the best available device according to the caller's preference.
/// Prints a one-line status to stderr so the user knows what's being used.
pub fn resolve_device(pref: &DevicePreference) -> Result<Device, EngineError> {
    match pref {
        DevicePreference::Cpu => Ok(Device::Cpu),

        DevicePreference::Metal => {
            #[cfg(target_os = "macos")]
            {
                Device::new_metal(0)
                    .map_err(|e| EngineError::ModelLoadError(format!("Metal unavailable: {e}")))
            }
            #[cfg(not(target_os = "macos"))]
            Err(EngineError::ModelLoadError(
                "Metal is only available on macOS".into(),
            ))
        }

        DevicePreference::Auto => {
            #[cfg(target_os = "macos")]
            {
                match Device::new_metal(0) {
                    Ok(d) => {
                        eprintln!("  Device     Metal (Apple Silicon GPU)");
                        return Ok(d);
                    }
                    Err(_) => {
                        eprintln!("  Device     CPU  (Metal unavailable, using Accelerate BLAS)");
                    }
                }
            }
            #[cfg(not(target_os = "macos"))]
            eprintln!("  Device     CPU");

            Ok(Device::Cpu)
        }
    }
}

impl ReviewAnalyzer {
    pub fn new(model_bytes: &[u8], tokenizer_bytes: &[u8]) -> Result<Self, EngineError> {
        Self::new_with_device(model_bytes, tokenizer_bytes, DevicePreference::Auto)
    }

    pub fn new_with_device(
        model_bytes: &[u8],
        tokenizer_bytes: &[u8],
        device_pref: DevicePreference,
    ) -> Result<Self, EngineError> {
        let device = resolve_device(&device_pref)?;

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
            debug: false,
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

    /// Print raw model output and token counts to stderr for each chunk.
    /// Useful for diagnosing why findings are empty or JSON parsing fails.
    pub fn with_debug(mut self, debug: bool) -> Self {
        self.debug = debug;
        self
    }

    /// Like [`analyze_diff_chunked`] but calls `on_progress(done, total)` after
    /// each chunk completes so callers can display a live progress indicator.
    ///
    /// Returns `(summary, skipped)` where `skipped` is the number of chunks
    /// the model processed but returned unparseable JSON for — useful for
    /// surfacing silent failures to the user.
    pub fn analyze_diff_chunked_with_progress<F>(
        &mut self,
        diff: &str,
        context: &str,
        max_tokens_per_chunk: u32,
        on_progress: F,
    ) -> Result<(ReviewSummary, usize), EngineError>
    where
        F: Fn(usize, usize),
    {
        // Run deterministic rules first — catches patterns the model reliably misses.
        let det_findings: Vec<ReviewFinding> = detect_commented_out_code(diff)
            .into_iter()
            .chain(detect_removed_used_variables(diff))
            .collect();

        const MAX_CHUNK_LINES: usize = 300;
        let chunks = chunk_diff(diff, MAX_CHUNK_LINES);
        // Pre-count non-empty chunks so callers can show "N/total".
        let total = chunks.iter().filter(|c| !c.trim().is_empty()).count();
        let mut done = 0;
        let mut skipped = 0;

        let mut all_findings = det_findings;
        let mut all_positives: Vec<String> = Vec::new();
        let mut all_suggestions: Vec<String> = Vec::new();

        for chunk in chunks {
            if chunk.trim().is_empty() {
                continue;
            }
            done += 1;
            on_progress(done, total);
            match self.analyze_diff_internal(&chunk, context, max_tokens_per_chunk as usize) {
                Ok(summary) => {
                    all_findings.extend(summary.findings);
                    all_positives.extend(summary.positives);
                    all_suggestions.extend(summary.suggestions);
                }
                // Small models often produce malformed JSON for a single chunk.
                // Count skips so the caller can warn the user instead of silently
                // returning "no issues".
                Err(EngineError::SerializationError(_)) => skipped += 1,
                Err(e) => return Err(e),
            }
        }

        // Deduplicate positives/suggestions that repeat across chunks.
        all_positives.dedup();
        all_suggestions.dedup();

        Ok((
            ReviewSummary {
                findings: all_findings,
                positives: all_positives,
                suggestions: all_suggestions,
            },
            skipped,
        ))
    }

    pub fn analyze_diff_chunked(
        &mut self,
        diff: &str,
        context: &str,
        max_tokens_per_chunk: u32,
    ) -> Result<ReviewSummary, EngineError> {
        self.analyze_diff_chunked_with_progress(diff, context, max_tokens_per_chunk, |_, _| {})
            .map(|(summary, _)| summary)
    }

    fn analyze_diff_internal(
        &mut self,
        diff: &str,
        context: &str,
        max_new_tokens: usize,
    ) -> Result<ReviewSummary, EngineError> {
        if diff.trim().is_empty() {
            return Ok(ReviewSummary::default());
        }

        let prompt = self.format_prompt(diff, context);

        if self.debug {
            let token_count = self
                .tokenizer
                .encode(&*prompt, true)
                .map(|t| t.len())
                .unwrap_or(0);
            eprintln!(
                "\n[debug] prompt tokens: {}  max_new: {}",
                token_count, max_new_tokens
            );
            eprintln!(
                "[debug] diff being analyzed ({} bytes):\n{}\n",
                diff.len(),
                diff
            );
        }

        let response = self.generate(&prompt, max_new_tokens)?;

        if self.debug {
            eprintln!("[debug] raw model output:\n{}\n", response);
        }

        // Try the primary format: {"findings": [...], "positives": [...], "suggestions": [...]}
        if let Some(start) = response.find('{')
            && let Some(end) = response.rfind('}')
            && end > start
        {
            let slice = &response[start..=end];
            if self.debug {
                eprintln!("[debug] extracted JSON object slice:\n{}\n", slice);
            }
            if let Ok(summary) = serde_json::from_str::<ReviewSummary>(slice) {
                return Ok(summary);
            }
        }

        // Fallback: bare array (old format / model didn't follow new schema)
        if let Some(start) = response.find('[')
            && let Some(end) = response.rfind(']')
            && end > start
        {
            let slice = &response[start..=end];
            if self.debug {
                eprintln!("[debug] fallback: extracted JSON array slice:\n{}\n", slice);
            }
            let findings: Vec<ReviewFinding> = serde_json::from_str(slice)
                .map_err(|e| EngineError::SerializationError(e.to_string()))?;
            return Ok(ReviewSummary {
                findings,
                positives: Vec::new(),
                suggestions: Vec::new(),
            });
        }

        if self.debug {
            eprintln!("[debug] no valid JSON found in output");
        }
        Ok(ReviewSummary::default())
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
            "<|im_start|>system\nYou are an expert Senior Software Engineer and Code Reviewer. Analyze the git diff and provide a thorough code review for {} code.{}{}\n\nFocus on:\n1. **Security**: Vulnerabilities, exposed secrets, insecure handling, disabled auth or validation.\n2. **Quality**: Bugs, anti-patterns, logical errors, commented-out functions or blocks of code (flag as medium severity), dead code left behind.\n3. **Performance**: Bottlenecks, inefficient algorithms or queries.\n4. **Maintainability**: Hard-to-read code, poor naming, high complexity, TODOs left in production code.{}\n\nIMPORTANT: If a function, method, or block has been commented out in the diff (lines starting with // or /* that previously were code), always flag it — commented-out code is a quality issue even if the change looks small.\n\nReturn a JSON object ONLY with exactly this structure:\n{{\n  \"findings\": [{{ \"file\": \"path\", \"line\": 12, \"severity\": \"high\"|\"medium\"|\"low\", \"category\": {}, \"issue\": \"description\", \"suggested_fix\": \"fix\" }}],\n  \"positives\": [\"one sentence describing something done well\"],\n  \"suggestions\": [\"one sentence optional improvement that is not a bug\"]\n}}\n\nRules:\n- findings: real issues only. Use [] if there are none.\n- positives: always include 1-3 things done well (clean logic, good naming, proper error handling, etc.). Never leave this empty.\n- suggestions: nice-to-have improvements (tests, docs, refactors). Use [] if nothing comes to mind.\n- Keep each positive/suggestion to one concise sentence.<|im_end|>\n<|im_start|>user\nAnalyze this diff:\n{}\n<|im_end|>\n<|im_start|>assistant\n",
            stack, requirements_section, context_section, compliance_focus, category_hint, diff
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
                    )));
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

/// Returns `true` once `s` contains a syntactically-complete JSON value
/// (object `{…}` or array `[…]`) with balanced brackets, respecting string
/// literals and escapes.
///
/// This lets `generate()` exit early as soon as the model has closed its
/// answer, instead of running until the token cap.
fn json_array_complete(s: &str) -> bool {
    let trimmed = s.trim_start();
    let (open, close) = match trimmed.chars().next() {
        Some('{') => ('{', '}'),
        Some('[') => ('[', ']'),
        _ => return false,
    };
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
            c if c == open && !in_string => depth += 1,
            c if c == close && !in_string => {
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

// ─── Deterministic rule: detect commented-out code blocks ────────────────────
//
// The LLM reliably misses this pattern.  We catch it mechanically by walking
// diff hunks and checking whether removed lines reappear as `// <same code>`.
// Auth/security files are flagged High; everything else is Medium.

struct Hunk {
    file: String,
    start_line: u32,
    removed: Vec<String>,
    added: Vec<String>,
}

pub fn detect_commented_out_code(diff: &str) -> Vec<ReviewFinding> {
    let mut findings: Vec<ReviewFinding> = Vec::new();
    let mut current_file = String::new();
    let mut hunk = Hunk {
        file: String::new(),
        start_line: 0,
        removed: Vec::new(),
        added: Vec::new(),
    };

    let flush = |h: &mut Hunk, findings: &mut Vec<ReviewFinding>| {
        if h.removed.len() < 3 || h.added.is_empty() || h.file.is_empty() {
            h.removed.clear();
            h.added.clear();
            return;
        }
        // Count removed lines that appear in added as `// <code>` or `/* <code>`
        let matches = h
            .removed
            .iter()
            .filter(|code| {
                let stripped = code.trim();
                h.added.iter().any(|a| {
                    let a = a.trim();
                    a == format!("// {}", stripped)
                        || a == format!("//{}", stripped)
                        || a.trim_start_matches('/').trim_start_matches('*').trim() == stripped
                })
            })
            .count();

        // Require ≥60% of removed lines to match as comments
        if matches * 10 >= h.removed.len() * 6 {
            let is_sensitive = h.file.contains("auth")
                || h.file.contains("login")
                || h.file.contains("token")
                || h.file.contains("password")
                || h.file.contains("security")
                || h.file.contains("middleware")
                || h.removed.iter().any(|c| {
                    c.contains("auth")
                        || c.contains("token")
                        || c.contains("password")
                        || c.contains("login")
                        || c.contains("validate")
                        || c.contains("sanitize")
                });

            let (severity, category, issue) = if is_sensitive {
                (
                    Severity::High,
                    Category::Security,
                    format!(
                        "Security-sensitive logic has been entirely commented out ({} lines). \
                         The function body is now dead code and will not execute — \
                         this may silently break authentication or validation.",
                        h.removed.len()
                    ),
                )
            } else {
                (
                    Severity::Medium,
                    Category::Quality,
                    format!(
                        "A code block of {} lines has been commented out. \
                         Commented-out code is technical debt — either restore it or delete it.",
                        h.removed.len()
                    ),
                )
            };

            findings.push(ReviewFinding {
                file: h.file.clone(),
                line: h.start_line,
                severity,
                category,
                issue,
                suggested_fix:
                    "Restore the logic if it should be active, or remove the commented block \
                     entirely. Use version control (git revert/branch) instead of commenting out."
                        .to_string(),
                confidence: Some(0.95),
            });
        }

        h.removed.clear();
        h.added.clear();
    };

    for line in diff.lines() {
        if line.starts_with("diff --git") {
            flush(&mut hunk, &mut findings);
            // Extract the b/ filename
            if let Some(b_part) = line.split(" b/").nth(1) {
                current_file = b_part.trim().to_string();
            }
            hunk.file = current_file.clone();
        } else if line.starts_with("@@") {
            flush(&mut hunk, &mut findings);
            hunk.file = current_file.clone();
            // Parse the new-file start line from `@@ -a,b +c,d @@`
            if let Some(new_part) = line.split('+').nth(1)
                && let Some(num) = new_part.split(',').next()
            {
                hunk.start_line = num.trim().parse().unwrap_or(1);
            }
        } else if line.starts_with('-') && !line.starts_with("---") {
            let code = line[1..].trim().to_string();
            if !code.is_empty() {
                hunk.removed.push(code);
            }
        } else if line.starts_with('+') && !line.starts_with("+++") {
            let code = line[1..].trim().to_string();
            if !code.is_empty() {
                hunk.added.push(code);
            }
        }
    }

    flush(&mut hunk, &mut findings);
    findings
}

// ─── Deterministic rule: removed variable still referenced ───────────────────
//
// Catches the pattern: a `const`/`let`/`var`/`def`/`var` declaration is on a
// `-` line but the declared name still appears in unchanged or added context
// lines — a guaranteed runtime ReferenceError / NameError.

pub fn detect_removed_used_variables(diff: &str) -> Vec<ReviewFinding> {
    let mut findings: Vec<ReviewFinding> = Vec::new();
    let mut current_file = String::new();
    // Collect all lines per file: (is_removed, is_added, text)
    let mut file_lines: Vec<(bool, bool, String)> = Vec::new();
    let mut file_start_line: u32 = 1;

    let flush_file = |file: &str,
                      lines: &[(bool, bool, String)],
                      start: u32,
                      findings: &mut Vec<ReviewFinding>| {
        if file.is_empty() {
            return;
        }
        // Regex-lite: find `const|let|var|def ` declarations on removed lines
        // and check if the name still appears in context/added lines.
        for (idx, (removed, _added, text)) in lines.iter().enumerate() {
            if !removed {
                continue;
            }
            let name = extract_declared_name(text);
            let name = match name {
                Some(n) if !n.is_empty() => n,
                _ => continue,
            };
            // Check if `name` appears in any non-removed line (context or added)
            let still_used = lines
                .iter()
                .any(|(rem, _, t)| !rem && contains_identifier(t, &name));
            if still_used {
                let line_no = start + idx as u32;
                findings.push(ReviewFinding {
                        file: file.to_string(),
                        line: line_no,
                        severity: Severity::High,
                        category: Category::Quality,
                        issue: format!(
                            "Variable `{}` was removed but is still referenced in the same scope. \
                             This will cause a ReferenceError (JavaScript) or NameError (Python) at runtime.",
                            name
                        ),
                        suggested_fix: format!(
                            "Restore the declaration of `{}`, or remove all references to it. \
                             Do not leave code that uses an undefined variable.",
                            name
                        ),
                        confidence: Some(0.92),
                    });
            }
        }
    };

    for line in diff.lines() {
        if line.starts_with("diff --git") {
            flush_file(&current_file, &file_lines, file_start_line, &mut findings);
            file_lines.clear();
            if let Some(b) = line.split(" b/").nth(1) {
                current_file = b.trim().to_string();
            }
        } else if line.starts_with("@@") {
            if let Some(new_part) = line.split('+').nth(1)
                && let Some(num) = new_part.split(',').next()
            {
                let hunk_start: u32 = num.trim().parse().unwrap_or(1);
                if file_lines.is_empty() {
                    file_start_line = hunk_start;
                }
            }
        } else if line.starts_with('-') && !line.starts_with("---") {
            file_lines.push((true, false, line[1..].to_string()));
        } else if line.starts_with('+') && !line.starts_with("+++") {
            file_lines.push((false, true, line[1..].to_string()));
        } else {
            // Context line
            file_lines.push((false, false, line.to_string()));
        }
    }
    flush_file(&current_file, &file_lines, file_start_line, &mut findings);
    findings
}

/// Extracts the declared identifier from a `const x =`, `let x =`, `var x =`,
/// `def x(`, `x :=` or `val x =` line.  Returns `None` if no pattern matches.
fn extract_declared_name(line: &str) -> Option<String> {
    let trimmed = line.trim();
    // JavaScript / TypeScript
    for kw in &["const ", "let ", "var "] {
        if let Some(rest) = trimmed.strip_prefix(kw) {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
                .collect();
            if !name.is_empty() {
                return Some(name);
            }
        }
    }
    // Python
    if let Some(rest) = trimmed.strip_prefix("def ") {
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if !name.is_empty() {
            return Some(name);
        }
    }
    // Go / Rust short assignment or `let x`
    if let Some(pos) = trimmed.find(" := ") {
        let name: String = trimmed[..pos]
            .chars()
            .rev()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        if !name.is_empty() {
            return Some(name);
        }
    }
    None
}

/// Returns true if `text` contains `name` as a whole identifier (not a substring).
fn contains_identifier(text: &str, name: &str) -> bool {
    let mut start = 0;
    while let Some(pos) = text[start..].find(name) {
        let abs = start + pos;
        let before_ok = abs == 0
            || !text
                .chars()
                .nth(abs.saturating_sub(1))
                .map(|c| c.is_alphanumeric() || c == '_' || c == '$')
                .unwrap_or(false);
        let after_ok = abs + name.len() >= text.len()
            || !text
                .chars()
                .nth(abs + name.len())
                .map(|c| c.is_alphanumeric() || c == '_' || c == '$')
                .unwrap_or(false);
        if before_ok && after_ok {
            return true;
        }
        start = abs + 1;
    }
    false
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
