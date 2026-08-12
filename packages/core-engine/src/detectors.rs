//! Deterministic detectors — high-confidence patterns the model reliably
//! misses, matched mechanically against the parsed diff at zero inference cost.

use crate::diff::{DiffHunk, FileDiff, LineKind, parse_diff};
use crate::types::{
    Category, CustomRule, RULE_COMMENTED_OUT_CODE, RULE_REMOVED_USED_VARIABLE, ReviewFinding,
    Severity,
};
use regex::Regex;

/// Run every deterministic detector over an already-parsed diff.
pub fn run_all(files: &[FileDiff], rules: &[CustomRule]) -> Vec<ReviewFinding> {
    let mut out = detect_commented_out_code(files);
    out.extend(detect_removed_used_variables(files));
    out.extend(detect_custom_rule_violations(files, rules));
    out
}

/// Convenience wrapper for callers holding a raw diff string.
pub fn run_all_str(diff: &str, rules: &[CustomRule]) -> Vec<ReviewFinding> {
    run_all(&parse_diff(diff), rules)
}

// ─── DM001: commented-out code ───────────────────────────────────────────────

/// Comment prefixes worth recognising. `//` and `/*` cover the C family, `#`
/// covers Python/Ruby/shell, `--` covers SQL and Lua.
const COMMENT_PREFIXES: &[&str] = &["//", "/*", "*", "#", "--"];

/// Strip a leading comment marker, returning the code behind it.
fn uncomment(line: &str) -> Option<&str> {
    let t = line.trim();
    for p in COMMENT_PREFIXES {
        if let Some(rest) = t.strip_prefix(p) {
            return Some(rest.trim());
        }
    }
    None
}

pub fn detect_commented_out_code(files: &[FileDiff]) -> Vec<ReviewFinding> {
    let mut findings = Vec::new();

    for file in files {
        for hunk in &file.hunks {
            let removed: Vec<&str> = hunk
                .removed()
                .map(|l| l.text.trim())
                .filter(|t| !t.is_empty())
                .collect();
            let added: Vec<&str> = hunk
                .added()
                .map(|l| l.text.trim())
                .filter(|t| !t.is_empty())
                .collect();

            // Below three lines this is indistinguishable from an ordinary edit.
            if removed.len() < 3 || added.is_empty() {
                continue;
            }

            let matches = removed
                .iter()
                .filter(|code| added.iter().any(|a| uncomment(a) == Some(**code)))
                .count();

            // Require ≥60% of removed lines to reappear as comments.
            if matches * 10 < removed.len() * 6 {
                continue;
            }

            let is_sensitive = is_security_sensitive(&file.path, &removed);
            let line = first_added_line(hunk);

            let (severity, category, issue) = if is_sensitive {
                (
                    Severity::High,
                    Category::Security,
                    format!(
                        "Security-sensitive logic has been entirely commented out ({} lines). \
                         The block is now dead code and will not execute — \
                         this may silently break authentication or validation.",
                        removed.len()
                    ),
                )
            } else {
                (
                    Severity::Medium,
                    Category::Quality,
                    format!(
                        "A code block of {} lines has been commented out. \
                         Commented-out code is technical debt — either restore it or delete it.",
                        removed.len()
                    ),
                )
            };

            findings.push(ReviewFinding {
                file: file.path.clone(),
                line,
                severity,
                category,
                issue,
                suggested_fix:
                    "Restore the logic if it should be active, or remove the commented block \
                     entirely. Use version control (git revert/branch) instead of commenting out."
                        .to_string(),
                confidence: Some(0.95),
                rule: None,
                unit_id: None,
                rule_id: Some(RULE_COMMENTED_OUT_CODE.to_string()),
            });
        }
    }

    findings
}

fn is_security_sensitive(path: &str, removed: &[&str]) -> bool {
    const PATH_HINTS: &[&str] = &[
        "auth",
        "login",
        "token",
        "password",
        "security",
        "middleware",
        "session",
        "permission",
        "acl",
        "crypt",
    ];
    const CODE_HINTS: &[&str] = &[
        "auth",
        "token",
        "password",
        "login",
        "validate",
        "sanitize",
        "verify",
        "permission",
        "authorize",
    ];

    let lower_path = path.to_lowercase();
    if PATH_HINTS.iter().any(|h| lower_path.contains(h)) {
        return true;
    }
    removed.iter().any(|c| {
        let lc = c.to_lowercase();
        CODE_HINTS.iter().any(|h| lc.contains(h))
    })
}

/// Post-image line to attribute a hunk-level finding to.
///
/// Skips diffmind's own suppression comments: the natural way to silence a
/// hunk-level finding is to write `// diffmind-ignore-next-line DM001` above the
/// block, but that comment then *becomes* the first added line, so anchoring to
/// it would place the finding on the directive and leave it un-suppressed.
fn first_added_line(hunk: &DiffHunk) -> u32 {
    hunk.added()
        .filter(|l| !crate::suppression::is_directive_line(&l.text))
        .filter_map(|l| l.new_line)
        .next()
        .unwrap_or(hunk.new_start)
}

// ─── DM002: removed declaration still referenced ─────────────────────────────

/// Catches a declaration removed on a `-` line whose name still appears in
/// surviving code — a guaranteed runtime `ReferenceError` / `NameError`.
///
/// The previous implementation flagged *any* edit to a declaration line, because
/// it asked "does this name appear on a non-removed line?" and the replacement
/// declaration is itself a non-removed line. `-const t = 30;` / `+const t = 60;`
/// therefore produced a HIGH "will crash at runtime" finding for a value change.
/// Three guards fix it:
///   1. skip when the name is re-declared anywhere in the same file diff,
///   2. scope the usage search to the same hunk rather than the whole file,
///   3. ignore member accesses (`self.t`, `opts.t`), which are not references
///      to the removed local.
pub fn detect_removed_used_variables(files: &[FileDiff]) -> Vec<ReviewFinding> {
    let mut findings = Vec::new();

    for file in files {
        // A deleted file has no surviving references by definition.
        if file.is_deletion {
            continue;
        }

        // Names re-declared anywhere in this file's diff — a move or a value
        // change, not a removal.
        let redeclared: Vec<String> = file
            .hunks
            .iter()
            .flat_map(|h| h.lines.iter())
            .filter(|l| l.kind != LineKind::Removed)
            .filter_map(|l| extract_declared_name(&l.text))
            .collect();

        for hunk in &file.hunks {
            for line in hunk.removed() {
                let Some(name) = extract_declared_name(&line.text) else {
                    continue;
                };
                // Single-character names produce too many coincidental matches.
                if name.len() < 2 || redeclared.contains(&name) {
                    continue;
                }

                let still_used = hunk
                    .lines
                    .iter()
                    .filter(|l| l.kind != LineKind::Removed)
                    // A mention inside a commented-out line is not a live
                    // reference. Without this, commenting a block out fires
                    // both this rule and DM001 for the same edit, and DM001 is
                    // the one that actually describes what happened.
                    .filter(|l| uncomment(&l.text).is_none())
                    .any(|l| references_identifier(&l.text, &name));

                if !still_used {
                    continue;
                }

                findings.push(ReviewFinding {
                    file: file.path.clone(),
                    line: nearest_surviving_line(hunk, line.old_line),
                    severity: Severity::High,
                    category: Category::Quality,
                    issue: format!(
                        "`{}` was declared on a removed line but is still referenced in the \
                         same block. This will fail at runtime with {}.",
                        name,
                        runtime_error_name(&file.path)
                    ),
                    suggested_fix: format!(
                        "Restore the declaration of `{name}`, or remove the remaining references to it."
                    ),
                    confidence: Some(0.92),
                    rule: None,
            unit_id: None,
                        rule_id: Some(RULE_REMOVED_USED_VARIABLE.to_string()),
                });
            }
        }
    }

    findings
}

/// Name the failure using the language's own vocabulary — telling a Rust
/// developer to expect a "ReferenceError" undermines every other finding.
fn runtime_error_name(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "js" | "jsx" | "mjs" | "cjs" | "ts" | "tsx" => "a ReferenceError",
        "py" => "a NameError",
        "rb" => "a NameError",
        "rs" => "a compile error (cannot find value in this scope)",
        "go" => "a compile error (undefined identifier)",
        "java" | "kt" | "kts" | "cs" | "c" | "h" | "cpp" | "cc" | "cxx" | "swift" => {
            "a compile error (undeclared identifier)"
        }
        _ => "an undefined-identifier error",
    }
}

/// A removed line has no post-image number, so attribute the finding to the
/// nearest surviving line in the hunk.
fn nearest_surviving_line(hunk: &DiffHunk, old_line: Option<u32>) -> u32 {
    let target = old_line.unwrap_or(0);
    hunk.lines
        .iter()
        .filter(|l| l.kind != LineKind::Removed)
        .filter_map(|l| l.new_line.map(|n| (n, l.old_line.unwrap_or(n))))
        .min_by_key(|(_, old)| (*old as i64 - target as i64).abs())
        .map(|(new, _)| new)
        .unwrap_or(hunk.new_start)
}

/// Extract the declared identifier from a declaration line.
/// Returns `None` when the line declares nothing.
pub fn extract_declared_name(line: &str) -> Option<String> {
    let trimmed = line.trim();

    // A commented-out declaration declares nothing.
    if uncomment(trimmed).is_some() {
        return None;
    }

    let take_ident = |s: &str| -> String {
        s.chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
            .collect()
    };

    // Rust `let mut x`, JS/TS `const|let|var x`.
    for kw in &["const ", "let ", "var "] {
        if let Some(rest) = trimmed.strip_prefix(kw) {
            let rest = rest.strip_prefix("mut ").unwrap_or(rest);
            let name = take_ident(rest);
            if !name.is_empty() {
                return Some(name);
            }
        }
    }

    // Python / Ruby / Rust function definitions.
    for kw in &["def ", "fn ", "pub fn ", "async fn "] {
        if let Some(rest) = trimmed.strip_prefix(kw) {
            let name = take_ident(rest);
            if !name.is_empty() {
                return Some(name);
            }
        }
    }

    // Go short assignment `x := …`.
    if let Some(pos) = trimmed.find(":=") {
        let lhs = trimmed[..pos].trim();
        // Multi-value `a, b := f()` is not a single declaration we can track.
        if !lhs.contains(',') {
            let name: String = lhs
                .chars()
                .rev()
                .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            if !name.is_empty() && name.chars().next().is_some_and(|c| !c.is_numeric()) {
                return Some(name);
            }
        }
    }

    None
}

/// True when `text` references `name` as a standalone identifier.
///
/// Member accesses (`self.name`, `opts.name`) are excluded: they read a field,
/// not the removed local, and were a large source of false positives.
///
/// Everything here works in characters rather than bytes, because `name` can
/// legitimately be non-ASCII — [`extract_declared_name`] collects it with
/// `char::is_alphanumeric`, which is Unicode-aware, so `const élan = 1` yields
/// `élan`. Byte arithmetic got that wrong twice over: advancing by one byte past
/// a multi-byte first character landed mid-codepoint and panicked the next
/// slice, and comparing the neighbouring *byte* against ASCII treated a UTF-8
/// continuation byte as a word boundary — so `élan` looked like a standalone
/// reference inside `béélan`.
pub fn references_identifier(text: &str, name: &str) -> bool {
    if name.is_empty() {
        return false;
    }
    let is_ident = |c: char| c.is_alphanumeric() || c == '_' || c == '$';

    let mut search_from = 0;
    while let Some(pos) = text[search_from..].find(name) {
        let abs = search_from + pos;
        let end = abs + name.len();

        // `find` reports a char boundary and `name` matched whole, so both
        // slices split cleanly.
        let before = text[..abs].chars().next_back();
        let after = text[end..].chars().next();

        let standalone = before.is_none_or(|c| !is_ident(c)) && after.is_none_or(|c| !is_ident(c));
        let is_member_access = before == Some('.');

        if standalone && !is_member_access {
            return true;
        }

        // Step past this occurrence's first character, not its first byte.
        let step = text[abs..].chars().next().map_or(1, char::len_utf8);
        search_from = abs + step;
        if search_from >= text.len() {
            break;
        }
    }
    false
}

// ─── Custom rules from .diffmind/rules.toml ──────────────────────────────────

/// Returns true when `file_path` matches the glob `pattern`.
///
/// Supports `*` (within one path segment), `?`, `**` (any number of segments),
/// and literal text. A pattern with no `/` matches the file name at any depth,
/// so `*.ts` means `**/*.ts`.
///
/// This was previously a handful of `starts_with`/`contains` special cases,
/// which could not express the most natural pattern of all — a directory
/// prefix. `src/api/**` matched nothing, and `**/gen/**` was a substring test
/// that also matched `src/gen-legacy/`.
pub fn file_matches_glob(file_path: &str, pattern: &str) -> bool {
    let path = file_path.trim().trim_start_matches("./");
    let pattern = pattern.trim();
    if pattern.is_empty() || path.is_empty() {
        return false;
    }
    if pattern == "*" || pattern == "**" {
        return true;
    }

    if !pattern.contains('/') {
        let base = path.rsplit('/').next().unwrap_or(path);
        return wildcard_matches(base, pattern);
    }

    let path_segments: Vec<&str> = path.split('/').collect();
    let pattern_segments: Vec<&str> = pattern.split('/').collect();
    if match_segments(&path_segments, &pattern_segments) {
        return true;
    }

    // Unanchored fallback, so a partial path like `utils/helper.ts` still
    // matches `src/utils/helper.ts` as it always has.
    let mut floating = vec!["**"];
    floating.extend(pattern_segments);
    match_segments(&path_segments, &floating)
}

fn match_segments(path: &[&str], pattern: &[&str]) -> bool {
    match (pattern.first(), path.first()) {
        (None, None) => true,
        (None, Some(_)) => false,
        // `**` matches zero segments, or one and then itself again.
        (Some(&"**"), _) => {
            match_segments(path, &pattern[1..])
                || (!path.is_empty() && match_segments(&path[1..], pattern))
        }
        (Some(_), None) => false,
        (Some(p), Some(s)) => wildcard_matches(s, p) && match_segments(&path[1..], &pattern[1..]),
    }
}

/// `*` and `?` against a single segment. Iterative with backtracking, so a
/// pathological pattern cannot blow the stack.
fn wildcard_matches(text: &str, pattern: &str) -> bool {
    let t: Vec<char> = text.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    let (mut ti, mut pi) = (0usize, 0usize);
    let (mut star, mut resume) = (usize::MAX, 0usize);

    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            ti += 1;
            pi += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = pi;
            pi += 1;
            resume = ti;
        } else if star != usize::MAX {
            pi = star + 1;
            resume += 1;
            ti = resume;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

pub fn detect_custom_rule_violations(
    files: &[FileDiff],
    rules: &[CustomRule],
) -> Vec<ReviewFinding> {
    if rules.is_empty() {
        return vec![];
    }

    // Pre-compile patterns; rules with invalid regex are skipped with a warning
    // rather than silently ignored — a typo used to disable a rule invisibly.
    let compiled: Vec<(&CustomRule, Regex)> = rules
        .iter()
        .filter_map(|r| match Regex::new(&r.pattern) {
            Ok(re) => Some((r, re)),
            Err(e) => {
                eprintln!(
                    "  !  rule '{}' has an invalid regex and was skipped: {e}",
                    r.effective_id()
                );
                None
            }
        })
        .collect();

    let mut findings = Vec::new();

    for file in files {
        for hunk in &file.hunks {
            for line in hunk.added() {
                for (rule, re) in &compiled {
                    if !rule.files.is_empty()
                        && !rule.files.iter().any(|g| file_matches_glob(&file.path, g))
                    {
                        continue;
                    }
                    if re.is_match(&line.text) {
                        findings.push(ReviewFinding {
                            file: file.path.clone(),
                            line: line.new_line.unwrap_or(hunk.new_start),
                            severity: Severity::parse(&rule.severity),
                            category: Category::parse(&rule.category),
                            issue: rule.message.clone(),
                            suggested_fix: rule.fix.clone().unwrap_or_default(),
                            confidence: Some(1.0),
                            rule: None,
                            unit_id: None,
                            rule_id: Some(rule.effective_id()),
                        });
                    }
                }
            }
        }
    }

    findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::parse_diff;

    #[test]
    fn value_change_to_a_declaration_is_not_a_removal() {
        // The regression that made DM002 fire on ordinary edits.
        let diff = "\
diff --git a/src/config.ts b/src/config.ts
--- a/src/config.ts
+++ b/src/config.ts
@@ -1,3 +1,3 @@
 export function f() {
-  const timeout = 30;
+  const timeout = 60;
   return timeout;
 }
";
        let findings = detect_removed_used_variables(&parse_diff(diff));
        assert!(
            findings.is_empty(),
            "changing a declaration's value must not be reported as a removal: {findings:#?}"
        );
    }

    #[test]
    fn genuinely_removed_declaration_is_flagged() {
        let diff = "\
diff --git a/src/config.ts b/src/config.ts
--- a/src/config.ts
+++ b/src/config.ts
@@ -1,4 +1,3 @@
 export function f() {
-  const timeout = 30;
   return timeout;
 }
";
        let findings = detect_removed_used_variables(&parse_diff(diff));
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].rule_id.as_deref(),
            Some(RULE_REMOVED_USED_VARIABLE)
        );
        assert!(findings[0].issue.contains("ReferenceError"));
    }

    #[test]
    fn rust_file_gets_a_rust_error_name() {
        let diff = "\
diff --git a/src/main.rs b/src/main.rs
--- a/src/main.rs
+++ b/src/main.rs
@@ -1,4 +1,3 @@
 fn f() {
-    let timeout = 30;
     println!(\"{}\", timeout);
 }
";
        let findings = detect_removed_used_variables(&parse_diff(diff));
        assert_eq!(findings.len(), 1);
        assert!(
            findings[0].issue.contains("compile error"),
            "Rust code should not be told to expect a ReferenceError: {}",
            findings[0].issue
        );
    }

    #[test]
    fn member_access_is_not_a_reference_to_the_removed_local() {
        let diff = "\
diff --git a/src/a.ts b/src/a.ts
--- a/src/a.ts
+++ b/src/a.ts
@@ -1,4 +1,3 @@
 function f(opts) {
-  const timeout = 30;
   return opts.timeout;
 }
";
        let findings = detect_removed_used_variables(&parse_diff(diff));
        assert!(
            findings.is_empty(),
            "opts.timeout is a field read: {findings:#?}"
        );
    }

    #[test]
    fn commenting_a_block_out_fires_only_the_commented_out_rule() {
        // The whole block is commented, so the "surviving reference" to `token`
        // is itself a comment. Reporting an undefined-identifier crash here
        // would be wrong, and duplicates DM001's (correct) finding.
        let diff = "\
diff --git a/src/auth.js b/src/auth.js
--- a/src/auth.js
+++ b/src/auth.js
@@ -10,6 +10,7 @@
 function checkAuth(req) {
-  const token = readToken(req);
-  if (!token) throw new Error('no token');
-  return verify(token);
+  // const token = readToken(req);
+  // if (!token) throw new Error('no token');
+  // return verify(token);
+  return true;
 }
";
        let files = parse_diff(diff);
        assert!(
            detect_removed_used_variables(&files).is_empty(),
            "a reference inside a comment is not a live reference"
        );
        assert_eq!(
            detect_commented_out_code(&files).len(),
            1,
            "DM001 should still describe what actually happened"
        );
    }

    #[test]
    fn a_trailing_comment_does_not_hide_a_real_reference() {
        let diff = "\
diff --git a/src/a.ts b/src/a.ts
--- a/src/a.ts
+++ b/src/a.ts
@@ -1,4 +1,3 @@
 function f() {
-  const timeout = 30;
   return timeout; // still used here
 }
";
        assert_eq!(
            detect_removed_used_variables(&parse_diff(diff)).len(),
            1,
            "only whole-line comments should be skipped"
        );
    }

    #[test]
    fn moved_declaration_is_not_flagged() {
        let diff = "\
diff --git a/src/a.ts b/src/a.ts
--- a/src/a.ts
+++ b/src/a.ts
@@ -1,5 +1,5 @@
 function f() {
-  const timeout = 30;
   doSomething();
+  const timeout = 30;
   return timeout;
 }
";
        let findings = detect_removed_used_variables(&parse_diff(diff));
        assert!(
            findings.is_empty(),
            "a moved declaration is still declared: {findings:#?}"
        );
    }

    #[test]
    fn references_identifier_handles_non_ascii_lines() {
        // Byte/char offset confusion used to misread the preceding character.
        assert!(references_identifier("// ✅ check timeout now", "timeout"));
        assert!(!references_identifier("let timeoutValue = 1;", "timeout"));
        assert!(!references_identifier("self.timeout", "timeout"));
    }

    /// A non-ASCII *identifier*, not merely a non-ASCII line. `is_alphanumeric`
    /// is Unicode-aware, so these names are real and reachable.
    #[test]
    fn a_non_ascii_identifier_is_matched_by_character_not_by_byte() {
        assert!(references_identifier("return élan;", "élan"));
        assert!(references_identifier("f(élan)", "élan"));

        // Part of a longer identifier is not a reference to it. The old
        // byte-wise boundary check saw a UTF-8 continuation byte, decided that
        // was a word boundary, and said yes.
        assert!(
            !references_identifier("béélan = 1;", "élan"),
            "`élan` inside `béélan` is not a standalone reference"
        );
        assert!(!references_identifier("élanor()", "élan"));

        // A member access is a field read, as in the ASCII case — and reaching
        // this branch is what used to advance by one byte into the middle of
        // `é` and abort the process on the next search.
        assert!(!references_identifier("return this.élan;", "élan"));
        assert!(!references_identifier("a.élan + b.élan", "élan"));
    }

    /// The panic, end to end. `--stdin` or `core.quotePath=false` is not needed
    /// here: an identifier is enough, and a review that aborts mid-run reports
    /// nothing at all.
    #[test]
    fn a_non_ascii_declaration_does_not_abort_the_detector() {
        let diff = "\
diff --git a/src/a.ts b/src/a.ts
--- a/src/a.ts
+++ b/src/a.ts
@@ -1,4 +1,3 @@
 function f(opts) {
-  const élan = 30;
   return opts.élan;
 }
";
        let findings = detect_removed_used_variables(&parse_diff(diff));
        assert!(
            findings.is_empty(),
            "opts.élan is a field read, exactly as opts.timeout is: {findings:#?}"
        );
    }

    #[test]
    fn a_removed_non_ascii_declaration_is_still_flagged() {
        let diff = "\
diff --git a/src/a.ts b/src/a.ts
--- a/src/a.ts
+++ b/src/a.ts
@@ -1,4 +1,3 @@
 function f() {
-  const élan = 30;
   return élan;
 }
";
        let findings = detect_removed_used_variables(&parse_diff(diff));
        assert_eq!(findings.len(), 1, "got: {findings:#?}");
        assert!(findings[0].issue.contains("élan"));
    }

    #[test]
    fn commented_out_block_is_flagged_with_a_real_line_number() {
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
        let findings = detect_commented_out_code(&parse_diff(diff));
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].severity,
            Severity::High,
            "auth path is sensitive"
        );
        assert_eq!(
            findings[0].rule_id.as_deref(),
            Some(RULE_COMMENTED_OUT_CODE)
        );
        assert_eq!(findings[0].line, 11, "should point at the first added line");
    }

    #[test]
    fn an_ignore_comment_above_a_block_actually_suppresses_it() {
        // The documented way to silence a hunk-level finding. The directive
        // itself must not become the finding's anchor line, or the suppression
        // lands one line short and silently does nothing.
        let diff = "\
diff --git a/src/auth.js b/src/auth.js
--- a/src/auth.js
+++ b/src/auth.js
@@ -10,6 +10,8 @@
 function checkAuth(req) {
-  const token = readToken(req);
-  if (!token) throw new Error('no token');
-  return verify(token);
+  // diffmind-ignore-next-line DM001
+  // const token = readToken(req);
+  // if (!token) throw new Error('no token');
+  // return verify(token);
+  return true;
 }
";
        let files = parse_diff(diff);
        let mut findings = detect_commented_out_code(&files);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].line, 12, "should anchor past the directive");

        let inline = crate::suppression::InlineSuppressions::from_diff(&files);
        let hidden = crate::suppression::apply(&mut findings, &inline, None);
        assert_eq!(hidden, 1, "the directive above the block must suppress it");
        assert!(findings.is_empty());
    }

    #[test]
    fn python_hash_comments_are_recognised() {
        let diff = "\
diff --git a/app.py b/app.py
--- a/app.py
+++ b/app.py
@@ -1,5 +1,5 @@
 def f():
-    a = 1
-    b = 2
-    c = 3
+    # a = 1
+    # b = 2
+    # c = 3
";
        let findings = detect_commented_out_code(&parse_diff(diff));
        assert_eq!(
            findings.len(),
            1,
            "'#' comments should count as commented-out code"
        );
    }

    #[test]
    fn custom_rules_report_correct_new_file_line_numbers() {
        let diff = "\
diff --git a/src/a.ts b/src/a.ts
--- a/src/a.ts
+++ b/src/a.ts
@@ -5,3 +5,4 @@
 const a = 1;
-const b = 2;
+console.log('debug');
 const c = 3;
";
        let rules = vec![CustomRule {
            id: Some("no-console".into()),
            pattern: "console\\.log".into(),
            message: "no console.log".into(),
            fix: Some("use the logger".into()),
            severity: "medium".into(),
            category: "quality".into(),
            files: vec!["*.ts".into()],
        }];
        let findings = detect_custom_rule_violations(&parse_diff(diff), &rules);
        assert_eq!(findings.len(), 1);
        // Context line 5, removed line consumes no post-image number, so the
        // added line is 6.
        assert_eq!(findings[0].line, 6);
        assert_eq!(findings[0].rule_id.as_deref(), Some("no-console"));
        assert_eq!(findings[0].suggested_fix, "use the logger");
    }

    #[test]
    fn custom_rule_file_filter_is_respected() {
        let diff =
            "diff --git a/a.py b/a.py\n--- a/a.py\n+++ b/a.py\n@@ -1 +1,2 @@\n+console.log('x')\n";
        let rules = vec![CustomRule {
            id: None,
            pattern: "console\\.log".into(),
            message: "no console".into(),
            fix: None,
            severity: "medium".into(),
            category: "quality".into(),
            files: vec!["*.ts".into()],
        }];
        assert!(detect_custom_rule_violations(&parse_diff(diff), &rules).is_empty());
    }

    #[test]
    fn glob_supports_recursive_directory_patterns() {
        assert!(file_matches_glob("src/gen/api.ts", "**/gen/**"));
        assert!(!file_matches_glob("src/app/api.ts", "**/gen/**"));
        assert!(file_matches_glob("a/b/c.rs", "*.rs"));
    }

    #[test]
    fn glob_matches_a_directory_prefix() {
        // The most natural pattern to write, and the one the old matcher could
        // not express at all.
        assert!(file_matches_glob("src/api/users.rs", "src/api/**"));
        assert!(file_matches_glob("src/api/v2/users.rs", "src/api/**"));
        assert!(!file_matches_glob("src/web/users.rs", "src/api/**"));
    }

    #[test]
    fn glob_combines_a_prefix_with_an_extension() {
        assert!(file_matches_glob("src/api/users.rs", "src/api/**/*.rs"));
        assert!(file_matches_glob("src/api/v2/users.rs", "src/api/**/*.rs"));
        assert!(
            !file_matches_glob("src/api/users.ts", "src/api/**/*.rs"),
            "the extension still has to match"
        );
    }

    #[test]
    fn glob_respects_segment_boundaries() {
        // `contains` used to say yes to both of these.
        assert!(!file_matches_glob("src/gen-legacy/api.ts", "**/gen/**"));
        assert!(!file_matches_glob("src/apiary/x.rs", "src/api/**"));
    }

    #[test]
    fn glob_still_matches_an_exact_or_partial_path() {
        assert!(file_matches_glob("src/a.ts", "src/a.ts"));
        assert!(file_matches_glob("apps/web/src/a.ts", "src/a.ts"));
        assert!(!file_matches_glob("src/b.ts", "src/a.ts"));
    }

    #[test]
    fn glob_handles_a_star_inside_a_segment() {
        assert!(file_matches_glob("src/user.test.ts", "*.test.ts"));
        assert!(file_matches_glob("src/handlers/get_user.rs", "**/get_*.rs"));
        assert!(!file_matches_glob(
            "src/handlers/set_user.rs",
            "**/get_*.rs"
        ));
    }
}
