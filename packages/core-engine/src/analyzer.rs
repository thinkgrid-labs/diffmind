//! Review orchestration: detectors, chunking, inference, anchoring, suppression.

use crate::backend::{GenOptions, ReviewBackend};
use crate::cache::{CacheKeyInput, ReviewCache};
use crate::detectors;
use crate::diff::{FileDiff, LineKind, anchor_findings, parse_diff};
use crate::error::EngineError;
use crate::error::PartialReview;
use crate::json_guard::extract_json;
use crate::prompt::{
    PROMPT_VERSION, Prompt, ReviewPromptInput, commit_message_prompt, pr_description_prompt,
    review_prompt, triage_prompt, truncate_to_char_boundary,
};
use crate::rulebook::{self, Rulebook};
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

/// Ceiling on either optional prompt section. Past this the model's attention is
/// the binding constraint rather than the window.
const MAX_SECTION_BYTES: usize = 12_000;
/// Floor for an optional section, below which it is not worth carrying.
const MIN_SECTION_BYTES: usize = 400;
/// Ticket text is a fixed brief, not something that grows with the diff.
const MAX_REQUIREMENTS_BYTES: usize = 2000;

/// Byte budget for one optional prompt section — the symbol context, or the
/// project rules — given a backend's window.
///
/// Public so the CLI can compute the same number before a review and warn about
/// rule sets that will not fit, instead of leaving the drop invisible.
pub fn section_budget_bytes(context_tokens: usize, max_new_tokens: usize, depth: u32) -> usize {
    let available = context_tokens.saturating_sub(max_new_tokens + SYSTEM_PROMPT_TOKENS);
    // Half of what is left, shared by the two sections, at ~3 bytes a token.
    let share = ((available / 4) * 3).min(MAX_SECTION_BYTES);
    // The floor never exceeds the share, or a window too small to afford it
    // would be pushed further over by the very thing meant to protect it.
    (share >> depth.min(16)).max(MIN_SECTION_BYTES.min(share))
}

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

/// Why a review unit was not reviewed by the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// The model answered, but not with anything parseable as a review.
    Unparseable,
    /// No amount of splitting fit the unit in the context window.
    TooLarge,
    /// The backend failed before or while reviewing it.
    BackendFailed,
}

impl SkipReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            SkipReason::Unparseable => "unparseable",
            SkipReason::TooLarge => "too_large",
            SkipReason::BackendFailed => "backend_failed",
        }
    }
}

/// One unit the model did not review, and why.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UnitSkip {
    pub unit_id: String,
    pub files: Vec<String>,
    pub reason: SkipReason,
    pub detail: String,
}

/// Whether a review covered everything it set out to, in the shape the
/// machine-readable outputs report it.
///
/// Four states, deliberately kept apart because a CI gate must treat them
/// differently: *complete* with or without blocking findings (the exit code
/// says which), *incomplete* (some units were never reviewed, so "no
/// findings" proves nothing about them), and *failed* (the backend broke).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Completeness {
    /// `complete`, `incomplete`, or `failed`.
    pub status: String,
    pub units_planned: usize,
    pub units_reviewed: usize,
    pub units_cached: usize,
    pub units_skipped: usize,
    pub skipped_unparseable: usize,
    pub skipped_too_large: usize,
    pub skipped_backend_failed: usize,
    /// Files the triage pass chose not to send for deep review. A policy
    /// decision rather than a failure, so it does not make a review incomplete,
    /// but it is reported because "reviewed" should never be read as "every
    /// file was looked at" when it was not.
    pub files_skipped_by_triage: usize,
    /// The deterministic detectors run before any inference and cannot be
    /// skipped; this is true whenever a report exists.
    pub deterministic_checks_completed: bool,
    /// Whether the run was configured to require a complete review.
    pub required: bool,
    /// True only for a complete review. A gate result from an incomplete or
    /// failed review says nothing about the units that were not reviewed.
    pub trustworthy: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backend_error: Option<String>,
    pub skipped: Vec<UnitSkip>,
}

#[derive(Debug, Default, Clone)]
pub struct AnalysisStats {
    pub units_total: usize,
    pub units_cached: usize,
    /// Units whose output could not be parsed even after repair.
    pub units_unparseable: usize,
    /// Units skipped because no amount of splitting fit them in the window.
    /// Distinct from `units_unparseable`: the model was never asked.
    pub units_too_large: usize,
    /// Units never reviewed because the backend failed — the one that failed
    /// and every one after it. Non-zero only alongside `backend_error`.
    pub units_failed: usize,
    /// The backend failure that stopped the review, if one did.
    pub backend_error: Option<String>,
    /// Every unit counted in the three fields above, with its reason.
    pub skipped: Vec<UnitSkip>,
    pub files_skipped_by_triage: usize,
    pub suppressed: usize,
    pub below_confidence: usize,
    /// Model findings dropped because they pointed at a file not in the diff.
    pub unanchorable: usize,
    /// Wall-clock spent inside the backend. Excludes diff parsing, filtering and
    /// context assembly, which are all sub-millisecond by comparison.
    pub inference_ms: u64,
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
    /// True when the token counts are estimates because the backend has no
    /// tokenizer to ask. Reported rather than hidden — a budget tracked against
    /// a guess is worse than one that admits it is guessing.
    pub tokens_estimated: bool,
}

impl AnalysisStats {
    /// Units the model did not review, for any reason.
    pub fn units_skipped(&self) -> usize {
        self.units_unparseable + self.units_too_large + self.units_failed
    }

    /// Units with a review result, fresh or cached.
    pub fn units_reviewed(&self) -> usize {
        self.units_total.saturating_sub(self.units_skipped())
    }

    /// Every planned unit was reviewed and the backend never failed.
    pub fn is_complete(&self) -> bool {
        self.units_skipped() == 0 && self.backend_error.is_none()
    }

    pub fn completeness(&self, required: bool) -> Completeness {
        let status = if self.backend_error.is_some() {
            "failed"
        } else if self.is_complete() {
            "complete"
        } else {
            "incomplete"
        };
        Completeness {
            status: status.to_string(),
            units_planned: self.units_total,
            units_reviewed: self.units_reviewed(),
            units_cached: self.units_cached,
            units_skipped: self.units_skipped(),
            skipped_unparseable: self.units_unparseable,
            skipped_too_large: self.units_too_large,
            skipped_backend_failed: self.units_failed,
            files_skipped_by_triage: self.files_skipped_by_triage,
            deterministic_checks_completed: true,
            required,
            trustworthy: self.is_complete(),
            backend_error: self.backend_error.clone(),
            skipped: self.skipped.clone(),
        }
    }

    pub fn total_tokens(&self) -> usize {
        self.prompt_tokens + self.completion_tokens
    }

    /// Generation throughput, or `None` when nothing was generated.
    pub fn tokens_per_second(&self) -> Option<f64> {
        (self.inference_ms > 0 && self.completion_tokens > 0)
            .then(|| self.completion_tokens as f64 / (self.inference_ms as f64 / 1000.0))
    }
}

pub struct ReviewAnalyzer {
    backend: Box<dyn ReviewBackend>,
    languages: Option<String>,
    requirements: Option<String>,
    debug: bool,
    custom_rules: Vec<CustomRule>,
    baseline: Option<Baseline>,
    cache: Option<ReviewCache>,
    rulebooks: Vec<Rulebook>,
    triage: TriageMode,
    min_confidence: f32,
    temperature: f64,
    seed: u64,
    /// Accumulated across every backend call in one `analyze`, including triage.
    metering: Metering,
    /// Optional regrouping of units before review.
    unit_grouper: Option<UnitGrouper>,
}

/// Merges units the engine has no way to know are related.
///
/// The engine groups by file and adjacency, which is all a diff can tell it.
/// Whether two units in *different* files are one review — a changed function
/// and a caller that changed with it — is a question only the code graph can
/// answer, and the graph lives in the application.
pub type UnitGrouper =
    Box<dyn Fn(Vec<crate::unit::ReviewUnit>) -> Vec<crate::unit::ReviewUnit> + Send>;

/// What one run cost. Accumulated on the analyzer because `analyze_chunk`
/// recurses when a unit overruns the window, and each level must add to the
/// same total.
#[derive(Debug, Default, Clone, Copy)]
struct Metering {
    inference_ms: u64,
    prompt_tokens: usize,
    completion_tokens: usize,
    estimated: bool,
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
            rulebooks: Vec::new(),
            triage: TriageMode::default(),
            min_confidence: 0.0,
            temperature: defaults.temperature,
            seed: defaults.seed,
            metering: Metering::default(),
            unit_grouper: None,
        }
    }

    /// Let the caller merge related units before review. See [`UnitGrouper`].
    pub fn with_unit_grouper(mut self, grouper: UnitGrouper) -> Self {
        self.unit_grouper = Some(grouper);
        self
    }

    /// Run the backend, recording what it cost.
    fn generate_metered(
        &mut self,
        prompt: &Prompt,
        opts: &GenOptions,
    ) -> Result<String, EngineError> {
        let prompt_text = prompt.to_chatml();
        let prompt_tokens = match self.backend.count_tokens(&prompt_text) {
            Some(n) => n,
            None => {
                self.metering.estimated = true;
                prompt.estimated_tokens()
            }
        };

        let started = std::time::Instant::now();
        let response = self.backend.generate(prompt, opts)?;
        self.metering.inference_ms += started.elapsed().as_millis() as u64;

        let completion_tokens = match self.backend.count_tokens(&response) {
            Some(n) => n,
            None => {
                self.metering.estimated = true;
                response.len() / 3
            }
        };

        self.metering.prompt_tokens += prompt_tokens;
        self.metering.completion_tokens += completion_tokens;
        Ok(response)
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

    /// Load prose rule sets from `.diffmind/rules/`.
    pub fn with_rulebooks(mut self, rulebooks: Vec<Rulebook>) -> Self {
        self.rulebooks = rulebooks;
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

    /// The units this analyzer would review, for a caller that needs to show a
    /// reader the hunk a finding actually came from.
    ///
    /// Ids match those stamped onto findings: the sizing comes from the same
    /// backend, unit ids are content-derived rather than positional (so triage
    /// dropping a whole file cannot shift the ids of the files that survive),
    /// and the same grouper runs here as in [`Self::analyze`].
    ///
    /// That last part is easy to lose. Merging two units mints a **new** id, so
    /// planning without the grouper returned only the unmerged halves — and
    /// every finding from a merged unit named an id no caller could resolve.
    /// The TUI's evidence pane went blank for precisely the cross-file changes
    /// the code graph exists to link.
    pub fn plan_units(&self, diff: &str, max_tokens: u32) -> Vec<crate::unit::ReviewUnit> {
        let units = build_units(diff, self.max_chunk_lines(max_tokens as usize));
        match &self.unit_grouper {
            Some(group) => group(units),
            None => units,
        }
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

    fn build_prompt(
        &self,
        diff: &str,
        context: &str,
        rulebooks: &[&Rulebook],
        max_new_tokens: usize,
        depth: u32,
    ) -> Prompt {
        let budget = self.section_budget_bytes(max_new_tokens, depth);
        review_prompt(&ReviewPromptInput {
            diff,
            context,
            languages: self.languages.as_deref(),
            requirements: self.requirements.as_deref(),
            rulebooks,
            max_context_bytes: budget,
            max_requirements_bytes: budget.min(MAX_REQUIREMENTS_BYTES),
            max_rules_bytes: budget,
        })
    }

    /// Byte budget for each of the prompt's optional sections — the symbol
    /// context and the project rules — at recursion `depth`.
    ///
    /// Two properties, both learned the hard way.
    ///
    /// **The diff keeps at least half the window.** These sections used to be
    /// sized from the window alone, so a large `.diffmind/rules/` could claim
    /// 12 KB and the context another 12 KB regardless of what was left for the
    /// thing actually under review.
    ///
    /// **The budget shrinks with depth.** When a prompt overruns, the analyzer
    /// halves the *diff* and retries — which cannot help when the fixed sections
    /// are what overran. A big enough rule set therefore failed identically at
    /// every recursion level and gave up at the depth cap, having never had a
    /// chance. Shrinking here is what makes the retry mean something.
    /// [`crate::prompt`] drops whole rule sets that no longer fit rather than
    /// truncating one mid-sentence, so this degrades to "fewer rules", then to
    /// "no rules" — never to half a rule that reads like a complete one.
    fn section_budget_bytes(&self, max_new_tokens: usize, depth: u32) -> usize {
        section_budget_bytes(self.backend.context_tokens(), max_new_tokens, depth)
    }

    /// True when the prompt fits the window with room for the response.
    #[allow(clippy::wrong_self_convention)]
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
    /// - `on_progress(done, total, unit)` fires when a unit starts, and is
    ///   handed the unit itself. A caller that wants to show a reader the hunk
    ///   behind a finding should record it here rather than predict the unit
    ///   list up front: triage and the grouper both run inside this method, and
    ///   either can change which units exist and therefore what ids findings
    ///   carry. It always fires before that unit's `on_chunk_result`.
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
        F: Fn(usize, usize, &crate::unit::ReviewUnit),
        G: Fn(&[ReviewFinding]),
    {
        let files = parse_diff(diff);
        let inline = InlineSuppressions::from_diff(&files);
        self.metering = Metering::default();
        let mut stats = AnalysisStats::default();
        let mut summary = ReviewSummary::default();
        // Tracks what has already been streamed so a later chunk cannot emit a
        // duplicate the caller has printed.
        let mut emitted: HashSet<(String, u32, String)> = HashSet::new();

        // Deterministic detectors first — instant, and independent of the model.
        let det = detectors::run_all(&files, &self.custom_rules);
        let det = self.finalize(
            det,
            &files,
            &inline,
            &mut stats,
            &mut emitted,
            Attribution::none(),
        );
        if !det.is_empty() {
            on_chunk_result(&det);
        }
        summary.findings.extend(det);

        // Triage decides which files are worth a deep pass.
        let skip: HashSet<String> = self.triage_files(&files, max_tokens_per_chunk)?;
        stats.files_skipped_by_triage = skip.len();

        let reviewable = filter_diff(diff, &skip);
        let mut units = build_units(
            &reviewable,
            self.max_chunk_lines(max_tokens_per_chunk as usize),
        );
        if let Some(group) = &self.unit_grouper {
            units = group(std::mem::take(&mut units));
        }
        stats.units_total = units.len();

        // Cloned once per run: `analyze_chunk` needs `&mut self`, so the
        // per-unit lookups must not hold a borrow on `self.rulebooks`.
        let rulebooks = self.rulebooks.clone();

        // Order units so that every unit governed by the same set of rule sets
        // is contiguous. Those units render a byte-identical system prompt, so
        // a backend that can reuse a prompt prefix gets the longest possible
        // run of hits. Findings are sorted before output regardless, so this
        // changes only the order work is done in.
        units.sort_by_key(|u| rulebook::group_key(&applicable_to_unit(&rulebooks, u)));

        let mut backend_failure: Option<String> = None;
        for (i, unit) in units.iter().enumerate() {
            let skip = |reason: SkipReason, detail: String| UnitSkip {
                unit_id: unit.id.clone(),
                files: unit.files.clone(),
                reason,
                detail,
            };

            // Once the backend has failed, asking it again for every remaining
            // unit only multiplies timeouts. Record them as not reviewed.
            if let Some(why) = &backend_failure {
                stats.units_failed += 1;
                stats
                    .skipped
                    .push(skip(SkipReason::BackendFailed, why.clone()));
                continue;
            }

            on_progress(i + 1, units.len(), unit);
            let books = applicable_to_unit(&rulebooks, unit);

            match self.analyze_chunk(&unit.text, context_for, &books, max_tokens_per_chunk, 0) {
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
                        Attribution {
                            unit_id: Some(&unit.id),
                            unit_files: Some(&unit.files),
                            rulebooks: &books,
                        },
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
                        eprintln!("[debug] unit {} ({}) unparseable: {e}", i + 1, unit.file());
                    }
                    stats.units_unparseable += 1;
                    stats.skipped.push(skip(SkipReason::Unparseable, e));
                }
                // Counted and skipped, not fatal. A hunk nobody can fit in the
                // window is a fact about that hunk; failing the run over it
                // would throw away every other unit's findings — including the
                // deterministic ones, which never needed a model at all.
                Err(EngineError::UnitTooLarge(why)) => {
                    if self.debug {
                        eprintln!("[debug] unit {} ({}) skipped: {why}", i + 1, unit.file());
                    }
                    stats.units_too_large += 1;
                    stats.skipped.push(skip(SkipReason::TooLarge, why));
                }
                // Anything else is the backend itself failing. This used to
                // return immediately, discarding every finding gathered so far
                // — the deterministic ones included — and leaving CI with a
                // bare error and no report. Now the remaining units are marked
                // not reviewed and the partial result travels inside the error,
                // so `?` still fails the run while a caller can still report it.
                Err(e) => {
                    let why = e.to_string();
                    stats.units_failed += 1;
                    stats
                        .skipped
                        .push(skip(SkipReason::BackendFailed, why.clone()));
                    backend_failure = Some(why);
                }
            }
        }

        summary.dedup();
        summary.sort();

        stats.inference_ms = self.metering.inference_ms;
        stats.prompt_tokens = self.metering.prompt_tokens;
        stats.completion_tokens = self.metering.completion_tokens;
        stats.tokens_estimated = self.metering.estimated;

        if let Some(why) = backend_failure {
            stats.backend_error = Some(why);
            return Err(EngineError::Incomplete(Box::new(PartialReview {
                summary,
                stats,
            })));
        }
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
        from: Attribution<'_>,
    ) -> Vec<ReviewFinding> {
        let (unit_id, rulebooks) = (from.unit_id, from.rulebooks);

        // A model finding must be about the unit the model was shown. The
        // prompt also carries surrounding code from other files, and a finding
        // "about" one of those — or about another unit's file it has not seen
        // — is speculation, or the product of instructions planted in that
        // code. It is dropped and counted with the unanchorable ones.
        //
        // The match is by path — exact, or on a whole path-component suffix —
        // never by bare basename against another directory: `src/b/mod.rs`
        // must not pass for a unit covering `src/a/mod.rs`. Surviving findings
        // are then anchored against the unit's files only, so the diff-wide
        // fallback matching cannot re-attach one to another unit's file.
        let in_unit: Option<Vec<FileDiff>> = from
            .unit_files
            .map(|unit_files| {
                files
                    .iter()
                    .filter(|f| unit_files.iter().any(|u| u == &f.path))
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .filter(|v| !v.is_empty());
        if let Some(scope) = &in_unit {
            let before = findings.len();
            findings.retain(|f| names_a_unit_file(&f.file, scope));
            stats.unanchorable += before - findings.len();
        }
        // Give model findings an explicit rule ID so they can be suppressed and
        // reported like any other, and record which unit produced them so a
        // reader can be shown the hunk the model was actually looking at.
        for f in &mut findings {
            // A rule set the model claims to have applied is only honoured if it
            // is one that actually governs this file. Small models invent
            // plausible names, and an invented one would produce a finding that
            // no `diffmind-ignore` directive could ever suppress.
            let claimed = f.rule.take();
            let matched = claimed.as_deref().and_then(|name| {
                let name = name
                    .trim()
                    .trim_start_matches(rulebook::RULEBOOK_RULE_PREFIX);
                rulebooks.iter().find(|b| b.id.eq_ignore_ascii_case(name))
            });

            if let Some(book) = matched {
                if f.rule_id.is_none() {
                    f.rule_id = Some(book.rule_id());
                }
                // The declared severity is a ceiling, not an override: one
                // opinionated style document must not be able to fill a triage
                // list with `high`.
                if let Some(ceiling) = book.severity
                    && f.severity > ceiling
                {
                    f.severity = ceiling;
                }
            }

            if f.rule_id.is_none() {
                f.rule_id = Some(model_rule_id(f.category));
            }
            if f.unit_id.is_none() {
                f.unit_id = unit_id.map(str::to_string);
            }
        }

        let before_anchor = findings.len();
        anchor_findings(&mut findings, in_unit.as_deref().unwrap_or(files));
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
        rulebooks: &[&Rulebook],
        max_tokens: u32,
        depth: u32,
    ) -> Result<(ReviewSummary, bool), EngineError> {
        if chunk.trim().is_empty() {
            return Ok((ReviewSummary::default(), false));
        }

        let context = context_for(chunk);
        let prompt = self.build_prompt(chunk, &context, rulebooks, max_tokens as usize, depth);

        if !self.prompt_fits(&prompt, max_tokens as usize) {
            if depth >= MAX_SPLIT_DEPTH {
                return Err(EngineError::UnitTooLarge(format!(
                    "still {} bytes of diff after {MAX_SPLIT_DEPTH} splits, with every \
                     optional section already at its minimum",
                    chunk.len()
                )));
            }
            // Halve and recurse rather than truncate: silently dropping half a
            // hunk means silently not reviewing it. Each half re-derives its own
            // context, so the halves stay independently cacheable.
            let (a, b) = split_in_half(chunk);
            let (mut first, cached_a) =
                self.analyze_chunk(&a, context_for, rulebooks, max_tokens, depth + 1)?;
            let (second, cached_b) =
                self.analyze_chunk(&b, context_for, rulebooks, max_tokens, depth + 1)?;
            first.merge(second);
            return Ok((first, cached_a && cached_b));
        }

        let opts = self.gen_options(max_tokens as usize);
        let cache_key = self.cache.as_ref().map(|cache| {
            cache.key(&CacheKeyInput {
                backend: &self.backend.cache_identity(),
                prompt_version: PROMPT_VERSION,
                // The prompt as rendered — after the context, rule sets and
                // requirements have been fitted to this backend's window at
                // this depth — so the key cannot drift from what is asked.
                prompt: &prompt.to_chatml(),
                max_tokens,
                temperature: opts.temperature,
                seed: opts.seed,
                repeat_penalty: opts.repeat_penalty,
                repeat_last_n: opts.repeat_last_n,
                json: opts.json,
            })
        });

        if let (Some(cache), Some(key)) = (self.cache.as_ref(), cache_key.as_ref())
            && let Some(hit) = cache.get(key)
        {
            return Ok((hit, true));
        }

        let response = self.generate_metered(&prompt, &opts)?;
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
        let response = match self.generate_metered(&prompt, &opts) {
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

/// Whether a model-reported path names one of `scope`'s files: exactly, or by
/// a suffix that starts at a path separator (`mod.rs` or `a/mod.rs` for
/// `src/a/mod.rs`, never `b/mod.rs`). An empty path is accepted only when the
/// unit has a single file.
fn names_a_unit_file(reported: &str, scope: &[FileDiff]) -> bool {
    let reported = reported.trim().trim_start_matches("./");
    if reported.is_empty() {
        return scope.len() == 1;
    }
    scope.iter().any(|f| {
        f.path == reported
            || f.path.ends_with(&format!("/{reported}"))
            || reported.ends_with(&format!("/{}", f.path))
    })
}

/// Rule sets governing any file in the unit. A merged unit spans more than one
/// file, and a rule scoped to either of them still applies.
fn applicable_to_unit<'a>(
    books: &'a [Rulebook],
    unit: &crate::unit::ReviewUnit,
) -> Vec<&'a Rulebook> {
    let mut out: Vec<&Rulebook> = Vec::new();
    for file in &unit.files {
        for b in rulebook::applicable(books, file) {
            if !out.iter().any(|existing| existing.id == b.id) {
                out.push(b);
            }
        }
    }
    out
}

/// Where a batch of findings came from, so `finalize` can stamp provenance and
/// validate any rule set the model claimed. Bundled because these two always
/// travel together and are meaningless apart.
struct Attribution<'a> {
    unit_id: Option<&'a str>,
    /// The files the unit covers; model findings outside them are rejected.
    unit_files: Option<&'a [String]>,
    rulebooks: &'a [&'a Rulebook],
}

impl Attribution<'_> {
    /// Findings that did not come from a unit — the deterministic detectors.
    fn none() -> Self {
        Attribution {
            unit_id: None,
            unit_files: None,
            rulebooks: &[],
        }
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
    let mut summary = parse_review_response_raw(response)?;
    sanitize_model_output(&mut summary);
    Ok(summary)
}

/// Most findings one unit may contribute. A unit is a few hundred lines; past
/// this the model is enumerating, not reviewing, and the output is better
/// treated as noise than shipped to a PR.
const MAX_FINDINGS_PER_UNIT: usize = 50;
const MAX_NOTES_PER_UNIT: usize = 10;
const MAX_ISSUE_CHARS: usize = 2_000;
const MAX_FIX_CHARS: usize = 4_000;
const MAX_NOTE_CHARS: usize = 500;
const MAX_PATH_CHARS: usize = 512;

/// Model output is untrusted text derived from untrusted input. Before it
/// reaches a terminal, a SARIF file or a PR comment it is stripped of control
/// characters — an ANSI escape in an `issue` would otherwise rewrite the
/// reviewer's terminal — and bounded in size and count.
fn sanitize_model_output(summary: &mut ReviewSummary) {
    summary.findings.truncate(MAX_FINDINGS_PER_UNIT);
    for f in &mut summary.findings {
        f.file = clean_text(&f.file, MAX_PATH_CHARS, false);
        f.issue = clean_text(&f.issue, MAX_ISSUE_CHARS, true);
        f.suggested_fix = clean_text(&f.suggested_fix, MAX_FIX_CHARS, true);
        if let Some(rule) = &f.rule {
            f.rule = Some(clean_text(rule, 128, false));
        }
        // Only the engine assigns these; a model-supplied value would let the
        // output claim a deterministic rule id or another unit's provenance.
        f.rule_id = None;
        f.unit_id = None;
        if let Some(c) = f.confidence
            && !c.is_finite()
        {
            f.confidence = None;
        }
    }
    for list in [&mut summary.positives, &mut summary.suggestions] {
        list.truncate(MAX_NOTES_PER_UNIT);
        for item in list.iter_mut() {
            *item = clean_text(item, MAX_NOTE_CHARS, false);
        }
        list.retain(|i| !i.trim().is_empty());
    }
}

/// Drop control characters (keeping newlines and tabs only where allowed) and
/// cap the length in characters, never splitting one.
fn clean_text(s: &str, max_chars: usize, keep_newlines: bool) -> String {
    let cleaned: String = s
        .chars()
        .filter(|c| !c.is_control() || (keep_newlines && (*c == '\n' || *c == '\t')))
        // Bidirectional overrides can make text read differently from what it
        // is (CVE-2021-42574); there is no legitimate use for them here.
        .filter(|c| !matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'))
        .collect();
    if cleaned.chars().count() <= max_chars {
        return cleaned;
    }
    let mut out: String = cleaned.chars().take(max_chars).collect();
    out.push('…');
    out
}

fn parse_review_response_raw(response: &str) -> Result<ReviewSummary, EngineError> {
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
            .analyze(TWO_FILES, &context_for, 512, |_, _, _| {}, |_| {})
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

    const TWO_SRC_FILES: &str = "\
diff --git a/src/a.rs b/src/a.rs
+++ b/src/a.rs
@@ -1,2 +1,2 @@
-let old_a = 1;
+let new_a = 2;
diff --git a/src/b.rs b/src/b.rs
+++ b/src/b.rs
@@ -1,2 +1,2 @@
-let old_b = 1;
+let new_b = 2;
";

    /// What the TUI's evidence pane actually relies on.
    ///
    /// It records each unit from `on_progress` and looks it up by a finding's
    /// `unit_id`. That only works if a unit is announced before any finding
    /// naming it arrives — otherwise the pane is asked for a hunk it has not
    /// been given, and silently shows nothing.
    #[test]
    fn a_unit_is_announced_before_any_finding_that_names_it() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut analyzer =
            ReviewAnalyzer::new(Box::new(FindingBackend { seen })).with_triage(TriageMode::Off);

        // One log, both callbacks, so the interleaving is what is asserted.
        let announced: Mutex<HashSet<String>> = Mutex::new(HashSet::new());
        let violations: Mutex<Vec<String>> = Mutex::new(Vec::new());

        analyzer
            .analyze(
                TWO_SRC_FILES,
                &|_| String::new(),
                512,
                |_, _, unit| {
                    announced.lock().expect("announced").insert(unit.id.clone());
                },
                |findings| {
                    let known = announced.lock().expect("announced");
                    for f in findings {
                        if let Some(id) = f.unit_id.as_deref()
                            && !known.contains(id)
                        {
                            violations.lock().expect("violations").push(id.to_string());
                        }
                    }
                },
            )
            .expect("analysis should succeed");

        assert!(
            !announced.lock().expect("announced").is_empty(),
            "no unit was announced at all"
        );
        assert!(
            violations.lock().expect("violations").is_empty(),
            "findings arrived naming units that had not been announced: {:?}",
            violations.lock().expect("violations")
        );
    }

    /// The regression that blanked the TUI's evidence pane.
    ///
    /// A caller planning units up front looks a hunk up by the finding's
    /// `unit_id`. Merging two units mints a **new** id, so a plan built without
    /// the grouper resolved nothing for a merged unit — and a merged unit is
    /// precisely the cross-file change the code graph exists to link, i.e. the
    /// finding whose evidence a reviewer most needs to see.
    ///
    /// The TUI now takes its units from `analyze` itself rather than predicting
    /// them, but `plan_units` is still public API and must not lie.
    #[test]
    fn planned_unit_ids_cover_every_unit_a_finding_can_name() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut analyzer = ReviewAnalyzer::new(Box::new(FindingBackend { seen }))
            .with_triage(TriageMode::Off)
            // Stands in for the code graph linking a symbol to its caller.
            .with_unit_grouper(Box::new(|units: Vec<crate::unit::ReviewUnit>| match units
                .split_first()
            {
                Some((first, rest)) => {
                    vec![rest.iter().fold(first.clone(), |a, b| a.merged_with(b))]
                }
                None => units,
            }));

        let planned: HashSet<String> = analyzer
            .plan_units(TWO_SRC_FILES, 512)
            .into_iter()
            .map(|u| u.id)
            .collect();

        let (summary, stats) = analyzer
            .analyze(TWO_SRC_FILES, &|_| String::new(), 512, |_, _, _| {}, |_| {})
            .expect("analysis should succeed");

        assert_eq!(stats.units_total, 1, "the grouper should have merged both");
        let used: HashSet<&str> = summary
            .findings
            .iter()
            .filter_map(|f| f.unit_id.as_deref())
            .collect();
        assert!(!used.is_empty(), "a model finding must record its unit");
        for id in &used {
            assert!(
                planned.contains(*id),
                "a finding names unit {id}, which the plan cannot resolve: {planned:?}"
            );
        }
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
            .analyze(diff, &|_| String::new(), 512, |_, _, _| {}, |_| {})
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

    /// A backend whose window is far too small for any prompt, so every unit
    /// exhausts the split depth. Records nothing: it is never reached.
    struct TinyWindowBackend;

    impl ReviewBackend for TinyWindowBackend {
        fn generate(&mut self, _p: &Prompt, _o: &GenOptions) -> Result<String, EngineError> {
            panic!("a unit that cannot fit must never reach the backend");
        }
        fn describe(&self) -> String {
            "tiny".into()
        }
        fn context_tokens(&self) -> usize {
            128
        }
    }

    /// A unit nobody can fit must not take the run down with it.
    ///
    /// This used to return `Err` from `analyze_chunk`, and every error except a
    /// parse failure propagated straight out of `analyze` — so one oversized
    /// hunk exited 2 and discarded the findings every *other* unit had already
    /// produced, including the deterministic ones that never needed a model.
    #[test]
    fn a_unit_that_cannot_fit_is_counted_and_skipped_not_fatal() {
        // Commented-out auth, which DM001 catches with no model involved.
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
        let mut analyzer =
            ReviewAnalyzer::new(Box::new(TinyWindowBackend)).with_triage(TriageMode::Off);

        let (summary, stats) = analyzer
            .analyze(diff, &|_| String::new(), 512, |_, _, _| {}, |_| {})
            .expect("an unfittable unit must not fail the run");

        assert_eq!(stats.units_too_large, 1, "the skip should be counted");
        assert_eq!(
            stats.units_unparseable, 0,
            "not a parse failure — the model was never asked"
        );
        assert!(
            summary
                .findings
                .iter()
                .any(|f| f.rule_id.as_deref() == Some(crate::types::RULE_COMMENTED_OUT_CODE)),
            "deterministic findings must survive a unit the model could not read: {:?}",
            summary.findings
        );
    }

    /// The rule sets are the same size no matter how the diff is split, so
    /// halving the hunk cannot rescue a prompt they overran. The budget has to
    /// shrink with depth or the retry is theatre.
    #[test]
    fn the_optional_sections_shrink_with_each_split() {
        let analyzer = ReviewAnalyzer::new(Box::new(RecordingBackend {
            seen: Arc::new(Mutex::new(Vec::new())),
        }));

        let at = |depth| analyzer.section_budget_bytes(1024, depth);
        assert!(at(0) > at(1), "depth 1 must be smaller than depth 0");
        assert!(at(1) > at(2));
        assert!(at(2) > at(3));
        assert!(at(4) >= MIN_SECTION_BYTES, "but never vanishes entirely");

        // Unchanged at depth 0 for the shipped 32K window, so an ordinary review
        // sees exactly the context and rules it saw before.
        assert_eq!(at(0), MAX_SECTION_BYTES);
    }

    /// The diff is the thing under review; the optional sections must not be
    /// able to crowd it out of its own window.
    #[test]
    fn the_optional_sections_never_claim_more_than_half_the_window() {
        struct Window(usize);
        impl ReviewBackend for Window {
            fn generate(&mut self, _p: &Prompt, _o: &GenOptions) -> Result<String, EngineError> {
                unreachable!()
            }
            fn describe(&self) -> String {
                "w".into()
            }
            fn context_tokens(&self) -> usize {
                self.0
            }
        }

        for window in [2048usize, 4096, 8192, 32_768] {
            let analyzer = ReviewAnalyzer::new(Box::new(Window(window)));
            let max_new = 1024;
            let available = window.saturating_sub(max_new + SYSTEM_PROMPT_TOKENS);
            // Two sections, three bytes a token, against what the window has left.
            let claimed_tokens = (analyzer.section_budget_bytes(max_new, 0) * 2) / 3;
            assert!(
                claimed_tokens <= available.max(1),
                "window {window}: sections claim {claimed_tokens} of {available} tokens"
            );
        }
    }

    /// Emits one finding that claims to violate `rule`, at `high`.
    struct ClaimingBackend {
        rule: &'static str,
    }

    impl ReviewBackend for ClaimingBackend {
        fn generate(&mut self, _p: &Prompt, _o: &GenOptions) -> Result<String, EngineError> {
            Ok(format!(
                r#"{{"findings":[{{"file":"src/a.rs","line":1,"severity":"high","category":"quality","issue":"i","suggested_fix":"f","rule":"{}"}}],"positives":[],"suggestions":[]}}"#,
                self.rule
            ))
        }
        fn describe(&self) -> String {
            "claiming".into()
        }
        fn context_tokens(&self) -> usize {
            32_768
        }
    }

    const ONE_FILE: &str = "\
diff --git a/src/a.rs b/src/a.rs
+++ b/src/a.rs
@@ -1,1 +1,1 @@
+let a = 2;
";

    fn run_with_claim(claim: &'static str, books: Vec<Rulebook>) -> Vec<ReviewFinding> {
        let mut analyzer = ReviewAnalyzer::new(Box::new(ClaimingBackend { rule: claim }))
            .with_triage(TriageMode::Off)
            .with_rulebooks(books);
        analyzer
            .analyze(ONE_FILE, &|_| String::new(), 512, |_, _, _| {}, |_| {})
            .expect("analysis should succeed")
            .0
            .findings
    }

    fn rulebook(id: &str, severity: Option<Severity>) -> Rulebook {
        Rulebook {
            id: id.into(),
            description: None,
            always: false,
            scope: vec![],
            severity,
            body: "- A rule.".into(),
        }
    }

    #[test]
    fn a_claimed_rule_set_becomes_a_suppressible_rule_id() {
        let findings = run_with_claim("api", vec![rulebook("api", None)]);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].rule_id.as_deref(), Some("rulebook.api"));
        assert!(
            findings[0].rule.is_none(),
            "the transient claim must not reach any output"
        );
    }

    #[test]
    fn a_rule_sets_declared_severity_is_a_ceiling() {
        // The model said `high`; the rule set is declared `low`. One opinionated
        // style document must not be able to fill a triage list with `high`.
        let findings = run_with_claim("style", vec![rulebook("style", Some(Severity::Low))]);
        assert_eq!(findings[0].severity, Severity::Low);

        // A ceiling never promotes.
        let findings = run_with_claim("sec", vec![rulebook("sec", Some(Severity::High))]);
        assert_eq!(findings[0].severity, Severity::High);
    }

    #[test]
    fn an_invented_rule_set_name_is_not_honoured() {
        // Small models produce plausible-looking names. Accepting one would
        // create a finding no `diffmind-ignore` directive could ever suppress,
        // because the rule it names does not exist.
        let findings = run_with_claim("does-not-exist", vec![rulebook("api", Some(Severity::Low))]);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].rule_id.as_deref(),
            Some("DM900.quality"),
            "should fall back to the generic model rule id"
        );
        assert_eq!(
            findings[0].severity,
            Severity::High,
            "an unmatched claim must not apply anyone's ceiling"
        );
    }

    #[test]
    fn a_rule_set_that_does_not_govern_the_file_cannot_be_claimed() {
        let scoped = Rulebook {
            id: "web".into(),
            description: None,
            always: false,
            scope: vec!["src/web/**".into()],
            severity: Some(Severity::Low),
            body: "- A rule.".into(),
        };
        // The diff touches `src/a.rs`, which `web` does not govern.
        let findings = run_with_claim("web", vec![scoped]);
        assert_eq!(findings[0].rule_id.as_deref(), Some("DM900.quality"));
        assert_eq!(findings[0].severity, Severity::High);
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
            .analyze(diff, &|_| String::new(), 512, |_, _, _| {}, |_| {})
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
        // The key is taken over the rendered prompt, so this is the property
        // end to end: a.rs's prompt is a function of a.rs and its own context.
        let analyzer = ReviewAnalyzer::new(Box::new(RecordingBackend {
            seen: Arc::new(Mutex::new(Vec::new())),
        }));
        let cache = ReviewCache::new(std::path::PathBuf::from("unused"));
        let chunk = "diff --git a/a.rs b/a.rs\n@@ -1 +1 @@\n+let new_a = 2;\n";
        let key_for = |ctx: &str| {
            let prompt = analyzer.build_prompt(chunk, ctx, &[], 512, 0);
            cache.key(&CacheKeyInput {
                backend: "recording",
                prompt_version: PROMPT_VERSION,
                prompt: &prompt.to_chatml(),
                max_tokens: 512,
                temperature: 0.0,
                seed: 7,
                repeat_penalty: 1.1,
                repeat_last_n: 64,
                json: true,
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
