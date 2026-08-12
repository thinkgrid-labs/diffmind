//! diffmind's review engine.
//!
//! The pipeline is deliberately independent of any one model: a diff is parsed
//! once, run through deterministic detectors, chunked to whatever context the
//! active backend advertises, reviewed, then anchored, suppressed and
//! deduplicated. Swapping the local GGUF for an Ollama or vLLM endpoint changes
//! only which `ReviewBackend` is constructed.

pub mod analyzer;
pub mod backend;
pub mod cache;
pub mod detectors;
pub mod diff;
pub mod error;
pub mod json_guard;
pub mod prefilter;
pub mod prompt;
pub mod rulebook;
pub mod sarif;
pub mod suppression;
pub mod types;
pub mod unit;

pub use analyzer::{AnalysisStats, ReviewAnalyzer, TriageMode, parse_review_response};
pub use backend::{
    CandleBackend, DEFAULT_SEED, DevicePreference, GenOptions, RemoteBackend, RemoteProtocol,
    ReviewBackend, resolve_device,
};
pub use cache::ReviewCache;
pub use detectors::{detect_commented_out_code, detect_removed_used_variables};
pub use diff::{FileDiff, anchor_findings, parse_diff};
pub use error::EngineError;
pub use prefilter::{DropReason, PrefilterOptions, PrefilterReport, looks_generated, prefilter};
pub use rulebook::{DEFAULT_RULEBOOK, RULEBOOK_RULE_PREFIX, Rulebook};
pub use sarif::to_sarif;
pub use suppression::{Baseline, InlineSuppressions};
pub use types::{
    Category, CommitSuggestion, CustomRule, PrDescription, ReviewFinding, ReviewSummary, Severity,
};
pub use unit::{ReviewUnit, build_units};
