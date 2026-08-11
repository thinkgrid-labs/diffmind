//! Suppressions: inline comments and a project baseline.
//!
//! Every static-analysis tool that survives adoption has an escape hatch. One
//! unfixable false positive on a legacy file is otherwise enough for a team to
//! delete the CI step permanently, and they never put it back.

use crate::diff::{FileDiff, LineKind};
use crate::types::ReviewFinding;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Marker recognised in source comments, e.g.
/// `// diffmind-ignore-next-line DM002` or `# diffmind-ignore`.
const MARKER: &str = "diffmind-ignore";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// Applies to the line the comment is on.
    SameLine,
    /// Applies to the line after the comment.
    NextLine,
    /// Applies to every finding in the file.
    File,
}

#[derive(Debug, Clone)]
struct Directive {
    scope: Scope,
    /// Rule IDs this directive covers. Empty means "every rule".
    rules: Vec<String>,
}

/// All inline suppressions found in a diff, indexed for lookup.
#[derive(Debug, Default, Clone)]
pub struct InlineSuppressions {
    /// file → line → directives applying to that line.
    by_line: HashMap<String, HashMap<u32, Vec<Directive>>>,
    /// file → whole-file directives.
    by_file: HashMap<String, Vec<Directive>>,
}

impl InlineSuppressions {
    /// Scan a parsed diff for suppression comments.
    ///
    /// Context lines are scanned too: a pre-existing suppression comment above
    /// a line that someone edited today is unchanged, so it appears in the diff
    /// as context, not as an addition.
    pub fn from_diff(files: &[FileDiff]) -> Self {
        let mut out = InlineSuppressions::default();

        for file in files {
            for hunk in &file.hunks {
                for line in &hunk.lines {
                    if line.kind == LineKind::Removed {
                        continue;
                    }
                    let Some(directive) = parse_directive(&line.text) else {
                        continue;
                    };
                    match directive.scope {
                        Scope::File => out
                            .by_file
                            .entry(file.path.clone())
                            .or_default()
                            .push(directive),
                        Scope::SameLine => {
                            if let Some(n) = line.new_line {
                                out.by_line
                                    .entry(file.path.clone())
                                    .or_default()
                                    .entry(n)
                                    .or_default()
                                    .push(directive);
                            }
                        }
                        Scope::NextLine => {
                            if let Some(n) = line.new_line {
                                out.by_line
                                    .entry(file.path.clone())
                                    .or_default()
                                    .entry(n + 1)
                                    .or_default()
                                    .push(directive);
                            }
                        }
                    }
                }
            }
        }

        out
    }

    pub fn is_suppressed(&self, finding: &ReviewFinding) -> bool {
        let rule = finding.rule_id();

        if let Some(directives) = self.by_file.get(&finding.file)
            && directives.iter().any(|d| covers(d, &rule))
        {
            return true;
        }

        self.by_line
            .get(&finding.file)
            .and_then(|lines| lines.get(&finding.line))
            .is_some_and(|ds| ds.iter().any(|d| covers(d, &rule)))
    }

    pub fn is_empty(&self) -> bool {
        self.by_line.is_empty() && self.by_file.is_empty()
    }
}

/// A directive with no explicit rule list covers everything. Otherwise a rule
/// matches on exact ID or on a dotted prefix, so `DM900` suppresses
/// `DM900.quality` without listing every category.
fn covers(d: &Directive, rule: &str) -> bool {
    d.rules.is_empty()
        || d.rules.iter().any(|r| {
            r == rule
                || rule
                    .strip_prefix(r.as_str())
                    .is_some_and(|rest| rest.starts_with('.'))
        })
}

/// True when this line is itself a diffmind suppression comment.
///
/// Detectors use it to avoid anchoring a hunk-level finding onto the very
/// comment that was written to suppress it — which would put the finding on the
/// directive's own line and make `-next-line` miss by one.
pub fn is_directive_line(text: &str) -> bool {
    text.contains(MARKER)
}

fn parse_directive(text: &str) -> Option<Directive> {
    let idx = text.find(MARKER)?;
    let rest = &text[idx + MARKER.len()..];

    let (scope, rest) = if let Some(r) = rest.strip_prefix("-next-line") {
        (Scope::NextLine, r)
    } else if let Some(r) = rest.strip_prefix("-file") {
        (Scope::File, r)
    } else {
        (Scope::SameLine, rest)
    };

    // Everything up to a closing comment token is the rule list.
    let rest = rest.split("*/").next().unwrap_or(rest);
    let rules: Vec<String> = rest
        .split([',', ' ', '\t'])
        .map(|s| s.trim())
        .filter(|s| !s.is_empty() && s.chars().next().is_some_and(|c| c.is_alphanumeric()))
        .map(|s| s.to_string())
        .collect();

    Some(Directive { scope, rules })
}

// ─── Baseline ────────────────────────────────────────────────────────────────

/// Accepted pre-existing findings, so a team can adopt diffmind on a codebase
/// that already has issues without the gate failing on day one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Baseline {
    pub version: u32,
    pub generated_at: String,
    pub entries: Vec<BaselineEntry>,
    /// Fast lookup, rebuilt on load rather than serialized.
    #[serde(skip)]
    index: HashSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaselineEntry {
    pub fingerprint: String,
    pub file: String,
    pub rule_id: String,
    /// Kept purely so a human can read the file and know what was accepted.
    pub issue: String,
}

impl Default for Baseline {
    fn default() -> Self {
        Baseline {
            version: 1,
            generated_at: String::new(),
            entries: Vec::new(),
            index: HashSet::new(),
        }
    }
}

impl Baseline {
    pub fn from_findings(findings: &[ReviewFinding], generated_at: String) -> Self {
        let mut entries: Vec<BaselineEntry> = findings
            .iter()
            .map(|f| BaselineEntry {
                fingerprint: f.fingerprint(),
                file: f.file.clone(),
                rule_id: f.rule_id(),
                issue: truncate(&f.issue, 160),
            })
            .collect();
        entries.sort_by(|a, b| a.fingerprint.cmp(&b.fingerprint));
        entries.dedup_by(|a, b| a.fingerprint == b.fingerprint);

        let index = entries.iter().map(|e| e.fingerprint.clone()).collect();
        Baseline {
            version: 1,
            generated_at,
            entries,
            index,
        }
    }

    pub fn parse(json: &str) -> Result<Self, serde_json::Error> {
        let mut b: Baseline = serde_json::from_str(json)?;
        b.index = b.entries.iter().map(|e| e.fingerprint.clone()).collect();
        Ok(b)
    }

    pub fn contains(&self, finding: &ReviewFinding) -> bool {
        self.index.contains(&finding.fingerprint())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max).collect::<String>() + "…"
}

/// Drop findings covered by inline directives or the baseline.
/// Returns the number removed so the CLI can tell the user what it hid.
pub fn apply(
    findings: &mut Vec<ReviewFinding>,
    inline: &InlineSuppressions,
    baseline: Option<&Baseline>,
) -> usize {
    let before = findings.len();
    findings.retain(|f| !inline.is_suppressed(f) && !baseline.is_some_and(|b| b.contains(f)));
    before - findings.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::parse_diff;
    use crate::types::{Category, Severity};

    fn finding(file: &str, line: u32, rule: &str) -> ReviewFinding {
        ReviewFinding {
            file: file.into(),
            line,
            severity: Severity::High,
            category: Category::Quality,
            issue: "problem".into(),
            suggested_fix: String::new(),
            confidence: None,
            rule_id: Some(rule.into()),
            unit_id: None,
        }
    }

    #[test]
    fn ignore_next_line_suppresses_the_following_line() {
        let diff = "\
diff --git a/a.ts b/a.ts
--- a/a.ts
+++ b/a.ts
@@ -1,2 +1,4 @@
 const a = 1;
+// diffmind-ignore-next-line DM002
+const b = 2;
";
        let files = parse_diff(diff);
        let s = InlineSuppressions::from_diff(&files);
        assert!(s.is_suppressed(&finding("a.ts", 3, "DM002")));
        assert!(
            !s.is_suppressed(&finding("a.ts", 2, "DM002")),
            "the comment's own line"
        );
        assert!(
            !s.is_suppressed(&finding("a.ts", 3, "DM001")),
            "different rule"
        );
    }

    #[test]
    fn bare_ignore_suppresses_every_rule() {
        let diff = "diff --git a/a.ts b/a.ts\n--- a/a.ts\n+++ b/a.ts\n@@ -1 +1,2 @@\n+const x = 1; // diffmind-ignore\n";
        let s = InlineSuppressions::from_diff(&parse_diff(diff));
        assert!(s.is_suppressed(&finding("a.ts", 1, "DM001")));
        assert!(s.is_suppressed(&finding("a.ts", 1, "anything")));
    }

    #[test]
    fn file_scope_covers_every_line() {
        let diff = "diff --git a/a.ts b/a.ts\n--- a/a.ts\n+++ b/a.ts\n@@ -1 +1,2 @@\n+/* diffmind-ignore-file DM900 */\n";
        let s = InlineSuppressions::from_diff(&parse_diff(diff));
        assert!(
            s.is_suppressed(&finding("a.ts", 999, "DM900.security")),
            "prefix match"
        );
        assert!(
            !s.is_suppressed(&finding("b.ts", 1, "DM900.security")),
            "other file"
        );
    }

    #[test]
    fn a_preexisting_comment_on_a_context_line_still_counts() {
        // The suppression was committed last week; today's diff only touches the
        // line below it, so the comment arrives as context.
        let diff = "\
diff --git a/a.ts b/a.ts
--- a/a.ts
+++ b/a.ts
@@ -4,3 +4,3 @@
 // diffmind-ignore-next-line DM002
-const b = 1;
+const b = 2;
";
        let s = InlineSuppressions::from_diff(&parse_diff(diff));
        assert!(s.is_suppressed(&finding("a.ts", 5, "DM002")));
    }

    #[test]
    fn baseline_matches_after_the_line_moves() {
        let f = finding("a.ts", 10, "DM001");
        let b = Baseline::from_findings(std::slice::from_ref(&f), "now".into());
        let round_tripped = Baseline::parse(&serde_json::to_string(&b).unwrap()).unwrap();

        let mut moved = f.clone();
        moved.line = 400;
        assert!(
            round_tripped.contains(&moved),
            "baseline must survive line drift"
        );

        let other = finding("a.ts", 10, "DM002");
        assert!(!round_tripped.contains(&other));
    }

    #[test]
    fn apply_reports_how_many_it_hid() {
        let diff = "diff --git a/a.ts b/a.ts\n--- a/a.ts\n+++ b/a.ts\n@@ -1 +1,2 @@\n+const x = 1; // diffmind-ignore\n";
        let s = InlineSuppressions::from_diff(&parse_diff(diff));
        let mut findings = vec![finding("a.ts", 1, "DM001"), finding("a.ts", 2, "DM001")];
        assert_eq!(apply(&mut findings, &s, None), 1);
        assert_eq!(findings.len(), 1);
    }
}
