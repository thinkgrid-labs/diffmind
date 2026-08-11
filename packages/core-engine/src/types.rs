//! Core output types shared by every backend, detector, and output format.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Ordering matters: `derive(Ord)` yields `Low < Medium < High`, which is what
/// `--min-severity` filtering relies on. Do not reorder these variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Low,
    Medium,
    High,
}

impl Severity {
    pub fn parse(s: &str) -> Severity {
        match s.trim().to_lowercase().as_str() {
            "high" | "error" | "critical" => Severity::High,
            "medium" | "med" | "warning" | "warn" => Severity::Medium,
            _ => Severity::Low,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Severity::High => "high",
            Severity::Medium => "medium",
            Severity::Low => "low",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

impl Category {
    pub fn parse(s: &str) -> Category {
        match s.trim().to_lowercase().as_str() {
            "security" => Category::Security,
            "performance" | "perf" => Category::Performance,
            "maintainability" => Category::Maintainability,
            "compliance" => Category::Compliance,
            _ => Category::Quality,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Category::Security => "security",
            Category::Quality => "quality",
            Category::Performance => "performance",
            Category::Maintainability => "maintainability",
            Category::Compliance => "compliance",
        }
    }
}

// ─── Built-in rule IDs ───────────────────────────────────────────────────────
//
// Every finding carries a stable ID so it can be suppressed inline, baselined,
// and reported as a SARIF `ruleId`. These strings are part of the public
// contract — changing one silently un-suppresses findings in user baselines.

/// A block of code was commented out rather than deleted.
pub const RULE_COMMENTED_OUT_CODE: &str = "DM001";
/// A declaration was removed while references to it remain.
pub const RULE_REMOVED_USED_VARIABLE: &str = "DM002";

/// Rule ID assigned to a finding produced by the model rather than a
/// deterministic detector. Suffixed with the category so users can suppress,
/// say, all model-authored maintainability noise without losing security.
pub fn model_rule_id(category: Category) -> String {
    format!("DM900.{}", category.as_str())
}

/// Human-readable description for a built-in rule ID, used in SARIF's rule
/// metadata table.
pub fn rule_description(id: &str) -> Option<&'static str> {
    match id {
        RULE_COMMENTED_OUT_CODE => Some("Code was commented out instead of deleted"),
        RULE_REMOVED_USED_VARIABLE => Some("A declaration was removed but is still referenced"),
        _ => None,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewFinding {
    pub file: String,
    pub line: u32,
    pub severity: Severity,
    pub category: Category,
    pub issue: String,
    pub suggested_fix: String,
    /// How much to trust this finding. Deterministic detectors set a high value;
    /// the model leaves it unset, in which case `DEFAULT_MODEL_CONFIDENCE` applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    /// Stable rule identifier, e.g. `DM001` or a custom rule's `id`.
    /// Absent only on freshly-parsed model output, before `assign_rule_ids`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<String>,
    /// The review unit this finding came out of, so a reader can be shown the
    /// exact hunk and context that produced it. Absent on findings from the
    /// deterministic detectors, which are not derived from a unit and already
    /// carry an exact file and line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit_id: Option<String>,
}

/// Confidence attributed to a model finding that did not report its own.
/// Deliberately below the deterministic detectors so `--min-confidence` can
/// filter to "mechanically certain" findings only.
pub const DEFAULT_MODEL_CONFIDENCE: f32 = 0.5;

impl ReviewFinding {
    pub fn rule_id(&self) -> String {
        self.rule_id
            .clone()
            .unwrap_or_else(|| model_rule_id(self.category))
    }

    pub fn confidence_or_default(&self) -> f32 {
        self.confidence.unwrap_or(DEFAULT_MODEL_CONFIDENCE)
    }

    /// Stable identity for baselining, deliberately independent of line number
    /// so a finding survives unrelated edits above it. Two findings with the
    /// same fingerprint are considered the same issue.
    pub fn fingerprint(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.file.as_bytes());
        hasher.update(b"\x00");
        hasher.update(self.rule_id().as_bytes());
        hasher.update(b"\x00");
        // Normalise whitespace so re-wrapped model prose keeps its identity.
        let normalized: String = self.issue.split_whitespace().collect::<Vec<_>>().join(" ");
        hasher.update(normalized.to_lowercase().as_bytes());
        format!("{:x}", hasher.finalize())[..16].to_string()
    }

    /// Deduplication key. Unlike `fingerprint` this *does* include the line, so
    /// the same rule firing at two places in one file stays two findings.
    fn dedup_key(&self) -> (String, u32, String, String) {
        let normalized: String = self.issue.split_whitespace().collect::<Vec<_>>().join(" ");
        (
            self.file.clone(),
            self.line,
            self.rule_id(),
            normalized.to_lowercase(),
        )
    }
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

impl ReviewSummary {
    /// Remove duplicates across the whole summary.
    ///
    /// `Vec::dedup` only collapses *adjacent* equal elements, so findings that
    /// repeat across non-consecutive chunks used to survive it. This compares
    /// against everything seen so far while preserving first-seen order.
    pub fn dedup(&mut self) {
        let mut seen = std::collections::HashSet::new();
        self.findings.retain(|f| seen.insert(f.dedup_key()));

        dedup_strings(&mut self.positives);
        dedup_strings(&mut self.suggestions);
    }

    /// Sort findings most-severe first, then by file and line, so output order
    /// is stable across runs regardless of the order chunks completed in.
    pub fn sort(&mut self) {
        self.findings.sort_by(|a, b| {
            b.severity
                .cmp(&a.severity)
                .then_with(|| a.file.cmp(&b.file))
                .then_with(|| a.line.cmp(&b.line))
        });
    }

    pub fn merge(&mut self, other: ReviewSummary) {
        self.findings.extend(other.findings);
        self.positives.extend(other.positives);
        self.suggestions.extend(other.suggestions);
    }
}

fn dedup_strings(items: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    items.retain(|s| {
        let key: String = s
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        !key.is_empty() && seen.insert(key)
    });
}

/// A single user-defined rule loaded from `.diffmind/rules.toml`.
/// Matched against every added line in the diff before the AI model runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomRule {
    /// Stable identifier used for suppression and SARIF reporting.
    /// Defaults to a slug derived from the message when omitted.
    #[serde(default)]
    pub id: Option<String>,
    /// Regex pattern matched against added lines (`+` lines) in the diff.
    pub pattern: String,
    /// Human-readable description shown as the finding's issue text.
    pub message: String,
    /// Optional remediation hint shown as the finding's suggested fix.
    #[serde(default)]
    pub fix: Option<String>,
    /// Severity: `"high"`, `"medium"`, or `"low"`. Defaults to `"medium"`.
    #[serde(default = "default_rule_severity")]
    pub severity: String,
    /// Category: `"security"`, `"quality"`, `"performance"`, `"maintainability"`.
    /// Defaults to `"quality"`.
    #[serde(default = "default_rule_category")]
    pub category: String,
    /// Optional file glob filters (e.g. `["*.ts", "*.tsx"]`).
    /// When empty the rule applies to every file in the diff.
    #[serde(default)]
    pub files: Vec<String>,
}

impl CustomRule {
    /// The rule's effective ID: the explicit `id` when set, otherwise a slug of
    /// the message prefixed with `custom.` so it can never collide with a `DM*`.
    pub fn effective_id(&self) -> String {
        if let Some(id) = &self.id
            && !id.trim().is_empty()
        {
            return id.trim().to_string();
        }
        let slug: String = self
            .message
            .chars()
            .map(|c| {
                if c.is_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '-'
                }
            })
            .collect::<String>()
            .split('-')
            .filter(|s| !s.is_empty())
            .take(5)
            .collect::<Vec<_>>()
            .join("-");
        format!("custom.{slug}")
    }
}

fn default_rule_severity() -> String {
    "medium".into()
}
fn default_rule_category() -> String {
    "quality".into()
}

/// Output of `generate_pr_description` — ready to paste into GitHub / GitLab.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PrDescription {
    /// Imperative title under 72 characters.
    #[serde(default)]
    pub title: String,
    /// Two to four bullet points summarising what changed and why.
    #[serde(default)]
    pub summary: Vec<String>,
    /// Checklist of steps a reviewer should take to verify the change.
    #[serde(default)]
    pub test_plan: Vec<String>,
}

/// Output of `generate_commit_message` — conventional commit format.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CommitSuggestion {
    /// Single-line conventional commit message (under 72 chars).
    #[serde(default)]
    pub message: String,
    /// Optional multi-line body explaining *why* the change was made.
    /// Empty string when a one-liner is sufficient.
    #[serde(default)]
    pub body: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(file: &str, line: u32, issue: &str) -> ReviewFinding {
        ReviewFinding {
            file: file.into(),
            line,
            severity: Severity::Medium,
            category: Category::Quality,
            issue: issue.into(),
            suggested_fix: String::new(),
            confidence: None,
            rule_id: None,
            unit_id: None,
        }
    }

    #[test]
    fn severity_orders_low_to_high() {
        assert!(Severity::High > Severity::Medium);
        assert!(Severity::Medium > Severity::Low);
    }

    #[test]
    fn dedup_removes_non_adjacent_duplicates() {
        let mut s = ReviewSummary {
            findings: vec![
                finding("a.rs", 1, "boom"),
                finding("b.rs", 2, "other"),
                finding("a.rs", 1, "boom"),
            ],
            positives: vec!["Good  naming".into(), "x".into(), "good naming".into()],
            suggestions: vec![],
        };
        s.dedup();
        assert_eq!(s.findings.len(), 2, "non-adjacent duplicate survived");
        assert_eq!(s.positives.len(), 2, "whitespace/case variant survived");
    }

    #[test]
    fn fingerprint_is_line_independent() {
        let a = finding("a.rs", 1, "the same issue");
        let b = finding("a.rs", 99, "The Same   Issue");
        assert_eq!(
            a.fingerprint(),
            b.fingerprint(),
            "baseline entries must survive line drift and re-wrapping"
        );
    }

    #[test]
    fn custom_rule_id_defaults_to_message_slug() {
        let r = CustomRule {
            id: None,
            pattern: "x".into(),
            message: "Remove debug logging before merging".into(),
            fix: None,
            severity: "medium".into(),
            category: "quality".into(),
            files: vec![],
        };
        assert_eq!(
            r.effective_id(),
            "custom.remove-debug-logging-before-merging"
        );
    }

    #[test]
    fn sort_puts_high_severity_first() {
        let mut s = ReviewSummary {
            findings: vec![finding("a.rs", 1, "low"), {
                let mut f = finding("b.rs", 2, "high");
                f.severity = Severity::High;
                f
            }],
            positives: vec![],
            suggestions: vec![],
        };
        s.sort();
        assert_eq!(s.findings[0].severity, Severity::High);
    }
}
