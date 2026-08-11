//! Review orchestration: detectors, chunking, inference, anchoring, suppression.

use crate::backend::{GenOptions, ReviewBackend};
use crate::cache::{CacheKeyInput, ReviewCache};
use crate::detectors;
use crate::diff::{FileDiff, LineKind, anchor_findings, parse_diff};
use crate::error::EngineError;
use crate::json_guard::extract_json;
use crate::prompt::{
    PROMPT_VERSION, Prompt, ReviewPromptInput, commit_message_prompt, pr_description_prompt,
    review_prompt, triage_prompt, truncate_to_char_boundary,
};
use crate::suppression::{Baseline, InlineSuppressions};
use crate::types::{
    CommitSuggestion, CustomRule, PrDescription, ReviewFinding, ReviewSummary, model_rule_id,
};
use crate::unit::build_units;
use std::collections::HashSet;

/// Rough cost of the system prompt in tokens, reserved out of the context
/// budget before sizing a chunk.
const SYSTEM_PROMPT_TOKENS: usize = 700;
/// Average tokens consumed by one line of a code diff. Deliberately generous —
/// underestimating overruns the window and fails the whole chunk.
const TOKENS_PER_DIFF_LINE: usize = 12;
const MIN_CHUNK_LINES: usize = 60;
/// Beyond this a single chunk holds more than a reviewer would read at once and
/// the model's attention degrades regardless of the window size.
const MAX_CHUNK_LINES: usize = 1200;

/// How many times a chunk may be halved when its prompt overruns the window.
const MAX_SPLIT_DEPTH: u32 = 4;

/// Triage only pays for itself once a diff spans enough files that skipping
/// some saves more than the extra inference pass costs.
const TRIAGE_MIN_FILES: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TriageMode {
    /// Never triage; review every file.
    Off,
    /// Triage when the diff spans at least `TRIAGE_MIN_FILES` files.
    #[default]
    Auto,
    /// Always triage.
    On,
}

impl TriageMode {
    pub fn parse(s: &str) -> TriageMode {
        match s.trim().to_lowercase().as_str() {
            "off" | "never" | "false" => TriageMode::Off,
            "on" | "always" | "true" => TriageMode::On,
            _ => TriageMode::Auto,
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct AnalysisStats {
    pub units_total: usize,
    pub units_cached: usize,
    /// Units whose output could not be parsed even after repair.
    pub units_unparseable: usize,
    pub files_skipped_by_triage: usize,
    pub suppressed: usize,
    pub below_confidence: usize,
    /// Model findings dropped because they pointed at a file not in the diff.
    pub unanchorable: usize,
}

pub struct ReviewAnalyzer {
    backend: Box<dyn ReviewBackend>,
    languages: Option<String>,
    requirements: Option<String>,
    debug: bool,
    custom_rules: Vec<CustomRule>,
    baseline: Option<Baseline>,
    cache: Option<ReviewCache>,
    triage: TriageMode,
    min_confidence: f32,
    temperature: f64,
    seed: u64,
}

impl ReviewAnalyzer {
    pub fn new(backend: Box<dyn ReviewBackend>) -> Self {
        let defaults = GenOptions::default();
        ReviewAnalyzer {
            backend,
            languages: None,
            requirements: None,
            debug: false,
            custom_rules: Vec::new(),
            baseline: None,
            cache: None,
            triage: TriageMode::default(),
            min_confidence: 0.0,
            temperature: defaults.temperature,
            seed: defaults.seed,
        }
    }

    /// Set the detected languages for this session. The names are injected into
    /// the system prompt so the model applies language-appropriate review idioms.
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

    pub fn with_debug(mut self, debug: bool) -> Self {
        self.debug = debug;
        self
    }

    /// Load team-defined rules from `.diffmind/rules.toml`.
    pub fn with_custom_rules(mut self, rules: Vec<CustomRule>) -> Self {
        self.custom_rules = rules;
        self
    }

    pub fn with_baseline(mut self, baseline: Option<Baseline>) -> Self {
        self.baseline = baseline;
        self
    }

    pub fn with_cache(mut self, cache: Option<ReviewCache>) -> Self {
        self.cache = cache;
        self
    }

    pub fn with_triage(mut self, triage: TriageMode) -> Self {
        self.triage = triage;
        self
    }

    pub fn with_min_confidence(mut self, min: f32) -> Self {
        self.min_confidence = min.clamp(0.0, 1.0);
        self
    }

    pub fn with_sampling(mut self, temperature: f64, seed: u64) -> Self {
        self.temperature = temperature.max(0.0);
        self.seed = seed;
        self
    }

    pub fn backend_description(&self) -> String {
        self.backend.describe()
    }

    /// Reclaim the backend so an already-loaded model can be reused for the
    /// next request. This is what makes `diffmind serve` worth running: the
    /// weights stay resident instead of being dropped with the analyzer.
    pub fn into_backend(self) -> Box<dyn ReviewBackend> {
        self.backend
    }

    pub fn context_tokens(&self) -> usize {
        self.backend.context_tokens()
    }

    fn gen_options(&self, max_new_tokens: usize) -> GenOptions {
        GenOptions {
            max_new_tokens,
            temperature: self.temperature,
            seed: self.seed,
            json: true,
            debug: self.debug,
            ..GenOptions::default()
        }
    }

    /// Chunk size that fits the backend's real context window.
    ///
    /// This used to be a hardcoded 300 lines because the window was assumed to
    /// be 4096 tokens. Qwen2.5-Coder is 32K, so most of the budget was being
    /// left on the table and the diff was split into far more inference passes
    /// than necessary.
    fn max_chunk_lines(&self, max_new_tokens: usize) -> usize {
        let available = self
            .backend
            .context_tokens()
            .saturating_sub(max_new_tokens + SYSTEM_PROMPT_TOKENS);
        (available / TOKENS_PER_DIFF_LINE).clamp(MIN_CHUNK_LINES, MAX_CHUNK_LINES)
    }

    fn build_prompt(&self, diff: &str, context: &str) -> Prompt {
        review_prompt(&ReviewPromptInput {
            diff,
            context,
            languages: self.languages.as_deref(),
            requirements: self.requirements.as_deref(),
            max_context_bytes: self.context_budget_bytes(),
            max_requirements_bytes: 2000,
        })
    }

    /// Byte budget for the RAG/context section, scaled to the window rather
    /// than pinned at the 2 KB a 4K window demanded.
    fn context_budget_bytes(&self) -> usize {
        let ctx = self.backend.context_tokens();
        // Roughly a sixth of the window, three bytes per token.
        ((ctx / 6) * 3).clamp(1500, 12_000)
    }

    /// True when the prompt fits the window with room for the response.
    fn prompt_fits(&self, prompt: &Prompt, max_new_tokens: usize) -> bool {
        let budget = self
            .backend
            .context_tokens()
            .saturating_sub(max_new_tokens + 64);
        let used = self
            .backend
            .count_tokens(&prompt.to_chatml())
            .unwrap_or_else(|| prompt.estimated_tokens());
        used < budget
    }

    // ── Main entry point ──────────────────────────────────────────────────

    /// Analyze a diff, streaming results as they arrive.
    ///
    /// - `context_for(chunk)` supplies the symbol context for one chunk. It is
    ///   a callback rather than a string because context must be assembled per
    ///   chunk: see [`Self::analyze_chunk`].
    /// - `on_progress(done, total)` fires when a chunk starts.
    /// - `on_chunk_result(findings)` fires with each batch as it completes, so
    ///   the CLI can print before the whole diff is processed.
    ///
    /// Findings passed to `on_chunk_result` are already anchored, suppressed and
    /// deduplicated, so what is streamed matches what the final summary contains.
    pub fn analyze<F, G>(
        &mut self,
        diff: &str,
        context_for: &dyn Fn(&str) -> String,
        max_tokens_per_chunk: u32,
        on_progress: F,
        on_chunk_result: G,
    ) -> Result<(ReviewSummary, AnalysisStats), EngineError>
    where
        F: Fn(usize, usize),
        G: Fn(&[ReviewFinding]),
    {
        let files = parse_diff(diff);
        let inline = InlineSuppressions::from_diff(&files);
        let mut stats = AnalysisStats::default();
        let mut summary = ReviewSummary::default();
        // Tracks what has already been streamed so a later chunk cannot emit a
        // duplicate the caller has printed.
        let mut emitted: HashSet<(String, u32, String)> = HashSet::new();

        // Deterministic detectors first — instant, and independent of the model.
        let det = detectors::run_all(&files, &self.custom_rules);
        let det = self.finalize(det, &files, &inline, &mut stats, &mut emitted, None);
        if !det.is_empty() {
            on_chunk_result(&det);
        }
        summary.findings.extend(det);

        // Triage decides which files are worth a deep pass.
        let skip: HashSet<String> = self.triage_files(&files, max_tokens_per_chunk)?;
        stats.files_skipped_by_triage = skip.len();

        let reviewable = filter_diff(diff, &skip);
        let units = build_units(
            &reviewable,
            self.max_chunk_lines(max_tokens_per_chunk as usize),
        );
        stats.units_total = units.len();

        for (i, unit) in units.iter().enumerate() {
            on_progress(i + 1, units.len());

            match self.analyze_chunk(&unit.text, context_for, max_tokens_per_chunk, 0) {
                Ok((unit_summary, cached)) => {
                    if cached {
                        stats.units_cached += 1;
                    }
                    let findings = self.finalize(
                        unit_summary.findings,
                        &files,
                        &inline,
                        &mut stats,
                        &mut emitted,
                        Some(&unit.id),
                    );
                    if !findings.is_empty() {
                        on_chunk_result(&findings);
                    }
                    summary.findings.extend(findings);
                    summary.positives.extend(unit_summary.positives);
                    summary.suggestions.extend(unit_summary.suggestions);
                }
                Err(EngineError::SerializationError(e)) => {
                    if self.debug {
                        eprintln!("[debug] unit {} ({}) unparseable: {e}", i + 1, unit.file);
                    }
                    stats.units_unparseable += 1;
                }
                Err(e) => return Err(e),
            }
        }

        summary.dedup();
        summary.sort();
        Ok((summary, stats))
    }

    /// Anchor, suppress, confidence-filter and deduplicate a batch of findings.
    fn finalize(
        &self,
        mut findings: Vec<ReviewFinding>,
        files: &[FileDiff],
        inline: &InlineSuppressions,
        stats: &mut AnalysisStats,
        emitted: &mut HashSet<(String, u32, String)>,
        unit_id: Option<&str>,
    ) -> Vec<ReviewFinding> {
        // Give model findings an explicit rule ID so they can be suppressed and
        // reported like any other, and record which unit produced them so a
        // reader can be shown the hunk the model was actually looking at.
        for f in &mut findings {
            if f.rule_id.is_none() {
                f.rule_id = Some(model_rule_id(f.category));
            }
            if f.unit_id.is_none() {
                f.unit_id = unit_id.map(str::to_string);
            }
        }

        let before_anchor = findings.len();
        anchor_findings(&mut findings, files);
        stats.unanchorable += before_anchor - findings.len();

        let before_conf = findings.len();
        let min_conf = self.min_confidence;
        findings.retain(|f| f.confidence_or_default() >= min_conf);
        stats.below_confidence += before_conf - findings.len();

        stats.suppressed +=
            crate::suppression::apply(&mut findings, inline, self.baseline.as_ref());

        findings.retain(|f| {
            let key = (
                f.file.clone(),
                f.line,
                f.issue
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .to_lowercase(),
            );
            emitted.insert(key)
        });

        findings
    }

    /// Run one chunk, consulting the cache and splitting if the prompt overruns.
    /// Returns the summary and whether it came from the cache.
    ///
    /// Context is assembled for *this chunk*. It used to be assembled once from
    /// the whole diff and shared by every chunk, which broke the cache outright:
    /// the shared string is part of the cache key, so editing any one file
    /// changed the key of every other file's chunk and a re-review after a
    /// force-push re-inferred the entire diff. Sharing it was also wrong on the
    /// merits — a chunk covering `auth.rs` was handed the enclosing functions of
    /// whichever six files happened to sort first.
    fn analyze_chunk(
        &mut self,
        chunk: &str,
        context_for: &dyn Fn(&str) -> String,
        max_tokens: u32,
        depth: u32,
    ) -> Result<(ReviewSummary, bool), EngineError> {
        if chunk.trim().is_empty() {
            return Ok((ReviewSummary::default(), false));
        }

        let context = context_for(chunk);
        let prompt = self.build_prompt(chunk, &context);

        if !self.prompt_fits(&prompt, max_tokens as usize) {
            if depth >= MAX_SPLIT_DEPTH {
                return Err(EngineError::ForwardError(
                    "a single diff hunk is too large for the model's context window even after \
                     splitting; review that file on its own"
                        .into(),
                ));
            }
            // Halve and recurse rather than truncate: silently dropping half a
            // hunk means silently not reviewing it. Each half re-derives its own
            // context, so the halves stay independently cacheable.
            let (a, b) = split_in_half(chunk);
            let (mut first, cached_a) =
                self.analyze_chunk(&a, context_for, max_tokens, depth + 1)?;
            let (second, cached_b) = self.analyze_chunk(&b, context_for, max_tokens, depth + 1)?;
            first.merge(second);
            return Ok((first, cached_a && cached_b));
        }

        let cache_key = self.cache.as_ref().map(|_| {
            ReviewCache::key(&CacheKeyInput {
                backend: &self.backend.describe(),
                prompt_version: PROMPT_VERSION,
                chunk,
                context: &context,
                languages: self.languages.as_deref().unwrap_or(""),
                requirements: self.requirements.as_deref().unwrap_or(""),
                max_tokens,
                temperature: self.temperature,
                seed: self.seed,
            })
        });

        if let (Some(cache), Some(key)) = (self.cache.as_ref(), cache_key.as_ref())
            && let Some(hit) = cache.get(key)
        {
            return Ok((hit, true));
        }

        let response = self
            .backend
            .generate(&prompt, &self.gen_options(max_tokens as usize))?;
        let summary = parse_review_response(&response)?;

        if let (Some(cache), Some(key)) = (self.cache.as_ref(), cache_key.as_ref()) {
            cache.put(key, &summary);
        }

        Ok((summary, false))
    }

    // ── Triage ────────────────────────────────────────────────────────────

    /// Decide which files to skip. Returns the set of paths not worth a deep
    /// pass. Any failure yields an empty set — triage is an optimisation, and
    /// reviewing too much is always safer than reviewing too little.
    fn triage_files(
        &mut self,
        files: &[FileDiff],
        max_tokens: u32,
    ) -> Result<HashSet<String>, EngineError> {
        let active = match self.triage {
            TriageMode::Off => false,
            TriageMode::On => files.len() > 1,
            TriageMode::Auto => files.len() >= TRIAGE_MIN_FILES,
        };
        if !active {
            return Ok(HashSet::new());
        }

        let summaries = files
            .iter()
            .map(file_summary)
            .collect::<Vec<_>>()
            .join("\n");

        let prompt = triage_prompt(&summaries);
        if !self.prompt_fits(&prompt, 512) {
            return Ok(HashSet::new());
        }

        let opts = GenOptions {
            max_new_tokens: 512.min(max_tokens as usize).max(128),
            ..self.gen_options(512)
        };
        let response = match self.backend.generate(&prompt, &opts) {
            Ok(r) => r,
            Err(e) => {
                if self.debug {
                    eprintln!("[debug] triage failed, reviewing everything: {e}");
                }
                return Ok(HashSet::new());
            }
        };

        let Some(json) = extract_json(&response) else {
            return Ok(HashSet::new());
        };
        let Ok(parsed) = serde_json::from_str::<TriageResult>(json) else {
            return Ok(HashSet::new());
        };

        // Only honour a skip for a path the model copied correctly; a
        // hallucinated path must never cause a real file to go unreviewed.
        let known: HashSet<&str> = files.iter().map(|f| f.path.as_str()).collect();
        let skip: HashSet<String> = parsed
            .skip
            .into_iter()
            .filter(|p| known.contains(p.as_str()))
            .collect();

        // Refuse to skip everything — that is a malformed answer, not a review.
        if skip.len() >= files.len() {
            return Ok(HashSet::new());
        }

        if self.debug && !skip.is_empty() {
            eprintln!("[debug] triage skipped {} low-risk file(s)", skip.len());
        }
        Ok(skip)
    }

    // ── PR description ────────────────────────────────────────────────────

    pub fn generate_pr_description(
        &mut self,
        diff: &str,
        ticket: Option<&str>,
        max_new_tokens: usize,
    ) -> Result<PrDescription, EngineError> {
        let budget = self.diff_budget_bytes(max_new_tokens);
        let diff = truncate_to_char_boundary(diff, budget);
        let prompt = pr_description_prompt(diff, ticket);
        let response = self
            .backend
            .generate(&prompt, &self.gen_options(max_new_tokens))?;

        extract_json(&response)
            .and_then(|j| serde_json::from_str::<PrDescription>(j).ok())
            .filter(|d| !d.title.trim().is_empty())
            .ok_or_else(|| {
                EngineError::SerializationError(
                    "model did not return a usable PR description".into(),
                )
            })
    }

    // ── Commit message ────────────────────────────────────────────────────

    pub fn generate_commit_message(
        &mut self,
        diff: &str,
        max_new_tokens: usize,
    ) -> Result<CommitSuggestion, EngineError> {
        let budget = self.diff_budget_bytes(max_new_tokens);
        let diff = truncate_to_char_boundary(diff, budget);
        let prompt = commit_message_prompt(diff);
        let response = self
            .backend
            .generate(&prompt, &self.gen_options(max_new_tokens))?;

        extract_json(&response)
            .and_then(|j| serde_json::from_str::<CommitSuggestion>(j).ok())
            .filter(|c| !c.message.trim().is_empty())
            .ok_or_else(|| {
                EngineError::SerializationError(
                    "model did not return a usable commit message".into(),
                )
            })
    }

    /// Byte budget for a whole-diff summarisation prompt, scaled to the window
    /// instead of the flat 10 KB that a 4K assumption implied.
    fn diff_budget_bytes(&self, max_new_tokens: usize) -> usize {
        let available = self
            .backend
            .context_tokens()
            .saturating_sub(max_new_tokens + SYSTEM_PROMPT_TOKENS);
        (available * 3).clamp(4_000, 60_000)
    }
}

#[derive(serde::Deserialize)]
struct TriageResult {
    #[serde(default)]
    #[allow(dead_code)]
    review: Vec<String>,
    #[serde(default)]
    skip: Vec<String>,
}

/// One line describing a file for the triage prompt: path, churn, and a taste
/// of what changed.
fn file_summary(file: &FileDiff) -> String {
    let added = file
        .hunks
        .iter()
        .flat_map(|h| h.lines.iter())
        .filter(|l| l.kind == LineKind::Added)
        .count();
    let removed = file
        .hunks
        .iter()
        .flat_map(|h| h.lines.iter())
        .filter(|l| l.kind == LineKind::Removed)
        .count();

    let preview: String = file
        .hunks
        .iter()
        .flat_map(|h| h.added())
        .map(|l| l.text.trim())
        .filter(|t| !t.is_empty())
        .take(3)
        .collect::<Vec<_>>()
        .join(" | ");

    format!(
        "- {} (+{added}/-{removed}) {}",
        file.path,
        truncate_to_char_boundary(&preview, 160)
    )
}

/// Drop whole files from a raw diff. Operating on the text (rather than
/// re-serialising the parsed form) keeps the exact bytes the model sees
/// identical to an unfiltered run, so the cache stays warm.
fn filter_diff(diff: &str, skip: &HashSet<String>) -> String {
    if skip.is_empty() {
        return diff.to_string();
    }

    let mut out = String::with_capacity(diff.len());
    let mut skipping = false;

    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            let path = parse_header_path(line);
            skipping = skip.contains(&path);
        }
        if !skipping {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn parse_header_path(line: &str) -> String {
    parse_diff(&format!("{line}\n@@ -1 +1 @@\n"))
        .first()
        .map(|f| f.path.clone())
        .unwrap_or_default()
}

/// Split a chunk at a hunk boundary when possible, else at the midpoint line.
fn split_in_half(chunk: &str) -> (String, String) {
    let lines: Vec<&str> = chunk.lines().collect();
    if lines.len() < 2 {
        return (chunk.to_string(), String::new());
    }
    let mid = lines.len() / 2;

    let header: Vec<&str> = lines
        .iter()
        .take_while(|l| {
            l.starts_with("diff --git")
                || l.starts_with("index ")
                || l.starts_with("--- ")
                || l.starts_with("+++ ")
        })
        .copied()
        .collect();

    // Prefer a hunk header near the midpoint so both halves stay coherent, but
    // only one that leaves actual content on both sides — splitting at the
    // first `@@` would put the entire body in the second half and loop.
    let split_at = lines
        .iter()
        .enumerate()
        .filter(|(i, l)| *i > header.len() && *i < lines.len() && l.starts_with("@@"))
        .min_by_key(|(i, _)| (*i as i64 - mid as i64).abs())
        .map(|(i, _)| i)
        .unwrap_or_else(|| mid.max(header.len() + 1).min(lines.len() - 1));

    let first = lines[..split_at].join("\n") + "\n";
    // Replay the file header so the second half still names its file.
    let mut second = header.join("\n");
    if !second.is_empty() {
        second.push('\n');
    }
    second.push_str(&(lines[split_at..].join("\n") + "\n"));

    (first, second)
}

/// Parse a model response into a summary.
///
/// Accepts the documented object form and, for backwards compatibility, a bare
/// findings array. Uses the string-aware extractor so a `}` inside an `issue`
/// no longer truncates the object.
pub fn parse_review_response(response: &str) -> Result<ReviewSummary, EngineError> {
    let Some(json) = extract_json(response) else {
        // A model that produced no JSON at all found nothing worth reporting
        // often enough that erroring here would be noisier than it is worth,
        // but the caller counts these so the user is told.
        return Err(EngineError::SerializationError(
            "no JSON value found in model output".into(),
        ));
    };

    if let Ok(summary) = serde_json::from_str::<ReviewSummary>(json) {
        return Ok(summary);
    }

    if let Ok(findings) = serde_json::from_str::<Vec<ReviewFinding>>(json) {
        return Ok(ReviewSummary {
            findings,
            ..Default::default()
        });
    }

    // Salvage the well-formed entries from an array where one element has a bad
    // enum value or a missing field — losing four good findings to one typo is
    // a poor trade.
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(json) {
        let array = value
            .get("findings")
            .and_then(|f| f.as_array())
            .or_else(|| value.as_array());
        if let Some(items) = array {
            let findings: Vec<ReviewFinding> = items
                .iter()
                .filter_map(|v| serde_json::from_value(v.clone()).ok())
                .collect();
            if !findings.is_empty() {
                return Ok(ReviewSummary {
                    findings,
                    positives: string_list(&value, "positives"),
                    suggestions: string_list(&value, "suggestions"),
                });
            }
        }
        // A syntactically valid object with an empty findings list is a real
        // "nothing found" answer, not a parse failure.
        if value.get("findings").is_some() {
            return Ok(ReviewSummary {
                findings: Vec::new(),
                positives: string_list(&value, "positives"),
                suggestions: string_list(&value, "suggestions"),
            });
        }
    }

    Err(EngineError::SerializationError(
        "model output was not a recognisable review object".into(),
    ))
}

fn string_list(value: &serde_json::Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Severity;
    use std::sync::{Arc, Mutex};

    /// Records the prompts it was asked to complete and answers "no findings",
    /// so a test can assert on what the analyzer sent rather than on a model.
    struct RecordingBackend {
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl ReviewBackend for RecordingBackend {
        fn generate(&mut self, prompt: &Prompt, _opts: &GenOptions) -> Result<String, EngineError> {
            self.seen
                .lock()
                .expect("recording backend mutex")
                .push(prompt.to_chatml());
            Ok(r#"{"findings":[],"positives":[],"suggestions":[]}"#.to_string())
        }
        fn describe(&self) -> String {
            "recording".into()
        }
        fn context_tokens(&self) -> usize {
            32_768
        }
    }

    /// Reports one finding per call, anchored to the first added line it is
    /// shown, so a test can follow provenance through the pipeline.
    struct FindingBackend {
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl ReviewBackend for FindingBackend {
        fn generate(&mut self, prompt: &Prompt, _opts: &GenOptions) -> Result<String, EngineError> {
            let chatml = prompt.to_chatml();
            let line = chatml
                .lines()
                .filter(|l| l.starts_with("@@"))
                .filter_map(|l| l.split('+').nth(1))
                .filter_map(|l| l.split(',').next())
                .filter_map(|n| n.trim().parse::<u32>().ok())
                .next()
                .unwrap_or(1);
            self.seen.lock().expect("mutex").push(chatml);
            Ok(format!(
                r#"{{"findings":[{{"file":"src/a.rs","line":{line},"severity":"high","category":"quality","issue":"issue at {line}","suggested_fix":"f"}}],"positives":[],"suggestions":[]}}"#
            ))
        }
        fn describe(&self) -> String {
            "finding".into()
        }
        fn context_tokens(&self) -> usize {
            32_768
        }
    }

    const TWO_FILES: &str = "\
diff --git a/a.rs b/a.rs
@@ -1,2 +1,2 @@
-let old_a = 1;
+let new_a = 2;
diff --git a/b.rs b/b.rs
@@ -1,2 +1,2 @@
-let old_b = 1;
+let new_b = 2;
";

    /// The regression this file exists to prevent.
    ///
    /// Context used to be assembled once from the whole diff and shared by every
    /// chunk. Because the shared string is part of the cache key, editing `b.rs`
    /// changed `a.rs`'s key too and a re-review re-inferred everything — the
    /// exact opposite of what the cache is for.
    #[test]
    fn each_chunk_gets_context_for_only_its_own_content() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let backend = Box::new(RecordingBackend { seen: seen.clone() });
        let mut analyzer = ReviewAnalyzer::new(backend).with_triage(TriageMode::Off);

        // Stands in for the real symbol index: context derived from the chunk.
        // Note the ordering — handed the *whole* diff (the old behaviour) this
        // returns A's context, so the b.rs assertions below are what actually
        // fail if per-chunk assembly is ever reverted.
        let context_for = |chunk: &str| {
            if chunk.contains("a.rs") {
                "CONTEXT-FOR-A".to_string()
            } else {
                "CONTEXT-FOR-B".to_string()
            }
        };

        analyzer
            .analyze(TWO_FILES, &context_for, 512, |_, _| {}, |_| {})
            .expect("analysis should succeed");

        let prompts = seen.lock().expect("recording backend mutex");
        assert_eq!(prompts.len(), 2, "one call per file");

        let a = prompts
            .iter()
            .find(|p| p.contains("a.rs"))
            .expect("a.rs should have been reviewed");
        assert!(a.contains("CONTEXT-FOR-A"));
        assert!(
            !a.contains("CONTEXT-FOR-B"),
            "a.rs's prompt must not carry b.rs's context — that coupling is what \
             made one file's edit invalidate every other file's cache entry"
        );

        let b = prompts
            .iter()
            .find(|p| p.contains("b.rs"))
            .expect("b.rs should have been reviewed");
        assert!(
            b.contains("CONTEXT-FOR-B") && !b.contains("CONTEXT-FOR-A"),
            "b.rs was handed a.rs's context, so context is being assembled once \
             for the whole diff and shared again:\n{b}"
        );
    }

    /// Two regions of one file are two units, and each finding records which
    /// unit produced it — that is what lets the reader be shown the exact hunk
    /// the model was looking at.
    #[test]
    fn distant_regions_are_reviewed_separately_and_findings_name_their_unit() {
        let diff = "\
diff --git a/src/a.rs b/src/a.rs
+++ b/src/a.rs
@@ -10,2 +10,2 @@
-let a = 1;
+let a = 2;
@@ -400,2 +400,2 @@
-let z = 1;
+let z = 2;
";
        let seen = Arc::new(Mutex::new(Vec::new()));
        let backend = Box::new(FindingBackend { seen: seen.clone() });
        let mut analyzer = ReviewAnalyzer::new(backend).with_triage(TriageMode::Off);

        let (summary, stats) = analyzer
            .analyze(diff, &|_| String::new(), 512, |_, _| {}, |_| {})
            .expect("analysis should succeed");

        assert_eq!(stats.units_total, 2, "two regions, two units");

        let ids: HashSet<&str> = summary
            .findings
            .iter()
            .filter_map(|f| f.unit_id.as_deref())
            .collect();
        assert_eq!(
            ids.len(),
            2,
            "each finding should name the unit it came from, and the two units \
             must be distinguishable: {:?}",
            summary.findings
        );
    }

    #[test]
    fn detector_findings_carry_no_unit_id() {
        // They are not produced from a unit, and they already carry an exact
        // file and line. Claiming a unit would be a lie the TUI would act on.
        let diff = "\
diff --git a/src/auth.js b/src/auth.js
--- a/src/auth.js
+++ b/src/auth.js
@@ -10,6 +10,6 @@
 function check() {
-  const token = read();
-  if (!token) throw new Error('no token');
-  return verify(token);
+  // const token = read();
+  // if (!token) throw new Error('no token');
+  // return verify(token);
 }
";
        let seen = Arc::new(Mutex::new(Vec::new()));
        let backend = Box::new(RecordingBackend { seen });
        let mut analyzer = ReviewAnalyzer::new(backend).with_triage(TriageMode::Off);

        let (summary, _) = analyzer
            .analyze(diff, &|_| String::new(), 512, |_, _| {}, |_| {})
            .expect("analysis should succeed");

        let detector_findings: Vec<_> = summary
            .findings
            .iter()
            .filter(|f| f.rule_id.as_deref() == Some(crate::types::RULE_COMMENTED_OUT_CODE))
            .collect();
        assert!(
            !detector_findings.is_empty(),
            "expected the commented-out-code detector to fire"
        );
        assert!(detector_findings.iter().all(|f| f.unit_id.is_none()));
    }

    /// The consequence the user actually feels: a cached chunk stays cached when
    /// an unrelated file in the same diff changes.
    #[test]
    fn an_unrelated_files_edit_does_not_change_a_chunks_cache_key() {
        let key_for = |ctx: &str| {
            ReviewCache::key(&CacheKeyInput {
                backend: "recording",
                prompt_version: PROMPT_VERSION,
                chunk: "diff --git a/a.rs b/a.rs\n@@ -1 +1 @@\n+let new_a = 2;\n",
                context: ctx,
                languages: "Rust",
                requirements: "",
                max_tokens: 512,
                temperature: 0.0,
                seed: 7,
            })
        };

        // Per-chunk context: a.rs's context is a function of a.rs alone, so it
        // is identical across the two runs and the key is stable.
        assert_eq!(
            key_for("enclosing fn in a.rs"),
            key_for("enclosing fn in a.rs"),
            "a.rs's key must not move when only b.rs changed"
        );
        // And the key still tracks context that genuinely differs.
        assert_ne!(key_for("enclosing fn in a.rs"), key_for("something else"));
    }

    #[test]
    fn parses_the_documented_object_form() {
        let s = parse_review_response(
            r#"{"findings":[{"file":"a.rs","line":3,"severity":"high","category":"security","issue":"i","suggested_fix":"f"}],"positives":["good"],"suggestions":[]}"#,
        )
        .unwrap();
        assert_eq!(s.findings.len(), 1);
        assert_eq!(s.findings[0].severity, Severity::High);
        assert_eq!(s.positives, vec!["good"]);
    }

    #[test]
    fn parses_a_bare_array_for_backwards_compatibility() {
        let s = parse_review_response(
            r#"[{"file":"a.rs","line":3,"severity":"low","category":"quality","issue":"i","suggested_fix":"f"}]"#,
        )
        .unwrap();
        assert_eq!(s.findings.len(), 1);
    }

    #[test]
    fn survives_a_brace_inside_an_issue_string() {
        // The old find/rfind slice truncated the object here.
        let s = parse_review_response(
            r#"{"findings":[{"file":"a.rs","line":1,"severity":"low","category":"quality","issue":"use ${x} not {y}","suggested_fix":"f"}],"positives":[],"suggestions":[]}"#,
        )
        .unwrap();
        assert_eq!(s.findings[0].issue, "use ${x} not {y}");
    }

    #[test]
    fn salvages_good_entries_from_a_partly_invalid_array() {
        let s = parse_review_response(
            r#"{"findings":[
                {"file":"a.rs","line":1,"severity":"nonsense","category":"quality","issue":"bad","suggested_fix":""},
                {"file":"b.rs","line":2,"severity":"high","category":"security","issue":"good","suggested_fix":""}
            ],"positives":["p"],"suggestions":[]}"#,
        )
        .unwrap();
        assert_eq!(s.findings.len(), 1, "the valid entry should survive");
        assert_eq!(s.findings[0].issue, "good");
        assert_eq!(s.positives, vec!["p"]);
    }

    #[test]
    fn an_empty_findings_list_is_a_real_answer_not_a_failure() {
        let s = parse_review_response(r#"{"findings":[],"positives":["clean"],"suggestions":[]}"#)
            .unwrap();
        assert!(s.findings.is_empty());
        assert_eq!(s.positives, vec!["clean"]);
    }

    #[test]
    fn prose_only_output_is_an_error_the_caller_can_count() {
        assert!(parse_review_response("I could not find any issues.").is_err());
    }

    #[test]
    fn filter_diff_removes_only_the_named_files() {
        let diff = "\
diff --git a/keep.rs b/keep.rs
@@ -1 +1 @@
+keep
diff --git a/drop.rs b/drop.rs
@@ -1 +1 @@
+drop
diff --git a/also-keep.rs b/also-keep.rs
@@ -1 +1 @@
+also
";
        let skip: HashSet<String> = ["drop.rs".to_string()].into_iter().collect();
        let out = filter_diff(diff, &skip);
        assert!(out.contains("keep.rs"));
        assert!(out.contains("also-keep.rs"));
        assert!(!out.contains("drop.rs"));
        assert!(!out.contains("+drop"));
    }

    #[test]
    fn filter_diff_is_a_noop_without_skips() {
        let diff = "diff --git a/a.rs b/a.rs\n@@ -1 +1 @@\n+x\n";
        assert_eq!(filter_diff(diff, &HashSet::new()), diff);
    }

    #[test]
    fn split_in_half_replays_the_file_header() {
        let chunk = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1,2 +1,2 @@\n+one\n@@ -9,2 +9,2 @@\n+two\n";
        let (first, second) = split_in_half(chunk);
        assert!(first.contains("+one"));
        assert!(second.contains("+two"));
        assert!(
            second.contains("diff --git"),
            "the second half must still name its file:\n{second}"
        );
    }

    #[test]
    fn file_summary_reports_churn() {
        let files =
            parse_diff("diff --git a/a.rs b/a.rs\n@@ -1,3 +1,3 @@\n-old\n+new\n+extra\n context\n");
        let s = file_summary(&files[0]);
        assert!(s.contains("a.rs"));
        assert!(s.contains("+2/-1"), "got: {s}");
        assert!(s.contains("new"));
    }
}
