//! A real unified-diff parser.
//!
//! Every detector used to re-parse the raw diff with its own ad-hoc line
//! scanning, and each got the line arithmetic subtly wrong in a different way.
//! Parsing once into a typed structure fixes them together and gives the
//! finding-anchoring pass real line numbers to snap to.

/// Where a line came from in a unified diff hunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Context,
    Added,
    Removed,
}

#[derive(Debug, Clone)]
pub struct DiffLine {
    pub kind: LineKind,
    /// Line content with the leading `+`/`-`/space marker stripped.
    pub text: String,
    /// 1-based line number in the post-image. `None` for removed lines.
    pub new_line: Option<u32>,
    /// 1-based line number in the pre-image. `None` for added lines.
    pub old_line: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct DiffHunk {
    /// First post-image line this hunk covers (from `@@ -a,b +c,d @@`).
    pub new_start: u32,
    pub lines: Vec<DiffLine>,
}

impl DiffHunk {
    pub fn added(&self) -> impl Iterator<Item = &DiffLine> {
        self.lines.iter().filter(|l| l.kind == LineKind::Added)
    }

    pub fn removed(&self) -> impl Iterator<Item = &DiffLine> {
        self.lines.iter().filter(|l| l.kind == LineKind::Removed)
    }

    /// Post-image line numbers this hunk touches — the set a model finding is
    /// allowed to point at.
    pub fn changed_new_lines(&self) -> Vec<u32> {
        self.added().filter_map(|l| l.new_line).collect()
    }
}

#[derive(Debug, Clone)]
pub struct FileDiff {
    /// Post-image path (the `b/` side), or the `a/` side for deletions.
    pub path: String,
    pub hunks: Vec<DiffHunk>,
    /// True when the header marked this file as deleted.
    pub is_deletion: bool,
}

impl FileDiff {
    pub fn changed_new_lines(&self) -> Vec<u32> {
        let mut lines: Vec<u32> = self
            .hunks
            .iter()
            .flat_map(|h| h.changed_new_lines())
            .collect();
        lines.sort_unstable();
        lines.dedup();
        lines
    }
}

/// Is `lines[i]` the `--- old` of a `--- old` / `+++ new` / `@@ …` file header?
///
/// The only file boundary a plain `diff -u` provides. `git diff` announces each
/// file with a `diff --git` line, but a diff reaching `--stdin` from `diff -u`,
/// `format-patch` or a review tool has nothing else to separate one file from
/// the next.
///
/// Deliberately strict about all three lines. Inside a hunk, a removed line of
/// content beginning `-- ` renders as `--- `, and SQL comments do exactly that —
/// requiring the `@@` too is what stops `-- note` / `++ x` being read as a file
/// boundary. Unified diff always puts the hunk header immediately after the path
/// pair, so nothing legitimate is lost.
pub(crate) fn starts_file_header_pair(lines: &[&str], i: usize) -> bool {
    lines[i].starts_with("--- ")
        && lines.get(i + 1).is_some_and(|l| l.starts_with("+++ "))
        && lines.get(i + 2).is_some_and(|l| l.starts_with("@@"))
}

/// Parse a unified diff into per-file hunks.
///
/// Tolerant by design: `git diff` output is interleaved with `index`, `old
/// mode`, `similarity index` and binary-file lines that carry no content, and
/// stdin may hand us a fragment with no `diff --git` header at all.
pub fn parse_diff(diff: &str) -> Vec<FileDiff> {
    let mut files: Vec<FileDiff> = Vec::new();
    let mut current: Option<FileDiff> = None;
    let mut hunk: Option<DiffHunk> = None;
    let mut new_line: u32 = 0;
    let mut old_line: u32 = 0;

    // Close the open hunk into the open file.
    fn flush_hunk(current: &mut Option<FileDiff>, hunk: &mut Option<DiffHunk>) {
        if let (Some(file), Some(h)) = (current.as_mut(), hunk.take())
            && !h.lines.is_empty()
        {
            file.hunks.push(h);
        }
    }

    let lines: Vec<&str> = diff.lines().collect();
    for (i, line) in lines.iter().copied().enumerate() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            flush_hunk(&mut current, &mut hunk);
            if let Some(f) = current.take() {
                files.push(f);
            }
            current = Some(FileDiff {
                path: parse_git_header_path(rest),
                hunks: Vec::new(),
                is_deletion: false,
            });
            continue;
        }

        if line.starts_with("deleted file mode") {
            if let Some(f) = current.as_mut() {
                f.is_deletion = true;
            }
            continue;
        }

        // In a plain multi-file diff there is no `diff --git` line to close the
        // previous file, so the next `--- old` / `+++ new` pair has to. Without
        // this the second file's `+++` merely *renamed* the first file — every
        // hunk in the diff ended up under the last path seen, and findings in
        // the earlier files were attributed to the wrong one.
        if starts_file_header_pair(&lines, i)
            && current
                .as_ref()
                .is_some_and(|f| !f.hunks.is_empty() || hunk.is_some())
        {
            flush_hunk(&mut current, &mut hunk);
            if let Some(f) = current.take() {
                files.push(f);
            }
        }

        // The `--- old` / `+++ new` path pair, which only ever appears *outside*
        // a hunk. Inside one, a line beginning `--- ` or `+++ ` is content whose
        // diff marker happens to be followed by two more of the same character:
        // a removed `-- note` in SQL, Lua or Haskell renders as `--- note`.
        // Handling those unconditionally meant such a line was dropped from the
        // hunk, and its `+++` twin silently renamed the file to the comment's
        // own text — so every finding in that file pointed at a path that does
        // not exist.
        if hunk.is_none() {
            // `+++ b/path` is authoritative when present — it survives paths
            // with spaces, which the `diff --git` line cannot express
            // unambiguously.
            if let Some(rest) = line.strip_prefix("+++ ") {
                let path = rest.trim();
                if path != "/dev/null" {
                    let path = path.strip_prefix("b/").unwrap_or(path).to_string();
                    match current.as_mut() {
                        Some(f) => f.path = path,
                        None => {
                            current = Some(FileDiff {
                                path,
                                hunks: Vec::new(),
                                is_deletion: false,
                            });
                        }
                    }
                }
                continue;
            }

            if line.starts_with("--- ") {
                continue;
            }
        }

        if line.starts_with("@@") {
            flush_hunk(&mut current, &mut hunk);
            let (old_start, new_start) = parse_hunk_header(line);
            old_line = old_start;
            new_line = new_start;
            // A hunk with no preceding file header (e.g. a hand-piped fragment)
            // still deserves to be parsed; attribute it to an unnamed file.
            if current.is_none() {
                current = Some(FileDiff {
                    path: String::new(),
                    hunks: Vec::new(),
                    is_deletion: false,
                });
            }
            hunk = Some(DiffHunk {
                new_start,
                lines: Vec::new(),
            });
            continue;
        }

        let Some(h) = hunk.as_mut() else { continue };

        // "\ No newline at end of file" is metadata, not content.
        if line.starts_with('\\') {
            continue;
        }

        let (kind, text) = match line.chars().next() {
            Some('+') => (LineKind::Added, &line[1..]),
            Some('-') => (LineKind::Removed, &line[1..]),
            Some(' ') => (LineKind::Context, &line[1..]),
            // An empty line inside a hunk is a context line whose single space
            // was stripped by an intermediate tool. Treating it as context keeps
            // the line counters aligned.
            None => (LineKind::Context, ""),
            // Anything else ends the hunk (e.g. a trailing "-- " signature).
            _ => {
                flush_hunk(&mut current, &mut hunk);
                continue;
            }
        };

        let (nl, ol) = match kind {
            LineKind::Added => {
                let n = new_line;
                new_line += 1;
                (Some(n), None)
            }
            LineKind::Removed => {
                let o = old_line;
                old_line += 1;
                (None, Some(o))
            }
            LineKind::Context => {
                let (n, o) = (new_line, old_line);
                new_line += 1;
                old_line += 1;
                (Some(n), Some(o))
            }
        };

        h.lines.push(DiffLine {
            kind,
            text: text.to_string(),
            new_line: nl,
            old_line: ol,
        });
    }

    flush_hunk(&mut current, &mut hunk);
    if let Some(f) = current.take() {
        files.push(f);
    }

    files
}

/// Extract the post-image path from the remainder of a `diff --git ` line.
///
/// The naive `split(" b/")` this replaced broke on any path containing the
/// literal " b/" — e.g. `a/my b/dir/x.rs`. Splitting the halves evenly is
/// correct for the overwhelmingly common case where both sides are equal, and
/// the `+++ b/…` line corrects us whenever it is not.
fn parse_git_header_path(rest: &str) -> String {
    let rest = rest.trim();
    // Quoted paths: git escapes non-ASCII / spaces as "a/x" "b/y".
    if rest.starts_with('"')
        && let Some(last_quote_pair) = rest.rfind("\" \"")
    {
        let b_side = &rest[last_quote_pair + 3..];
        let b_side = b_side.trim_end_matches('"');
        return b_side.strip_prefix("b/").unwrap_or(b_side).to_string();
    }

    if let Some(candidate) = even_split(rest) {
        return candidate.to_string();
    }

    // Fall back to the last " b/" occurrence — right more often than the first
    // when a directory is literally named `b`.
    if let Some(idx) = rest.rfind(" b/") {
        return rest[idx + 3..].trim().to_string();
    }
    rest.to_string()
}

/// `a/<path> b/<path>` where both sides name the same file — the overwhelmingly
/// common case, and the only one that stays right when the path itself contains
/// " b/". Returns `None` when the two halves are not identical, leaving the
/// caller's `rfind` fallback to handle renames.
///
/// The arithmetic used to be `(len - 1) / 2`, which is one byte long: the
/// remainder after `a/` is `<path> b/<path>`, so for a path of `n` bytes it is
/// `2n + 3` bytes, not `2n + 1`. The comparison therefore never matched and
/// this branch never returned — every correct answer came from the fallback
/// below it. Worse, the slice was taken without a boundary check, so a rename
/// like `a/x.rs b/ääää.rs` landed mid-codepoint and panicked the process.
fn even_split(rest: &str) -> Option<&str> {
    let a_body = rest.strip_prefix("a/")?;
    // `<path> b/<path>` is `2n + 3` bytes, so an odd remainder cannot be one.
    let n = a_body.len().checked_sub(3).filter(|r| r % 2 == 0)? / 2;
    // A rename whose two sides merely happen to be the same byte length can put
    // `n` inside a multi-byte character. Refuse rather than slice.
    if !a_body.is_char_boundary(n) {
        return None;
    }
    let candidate = &a_body[..n];
    (rest.len() == 2 + n + 3 + n && a_body[n..] == format!(" b/{candidate}")).then_some(candidate)
}

/// Parse `@@ -old_start,old_count +new_start,new_count @@ optional context`.
/// Returns `(old_start, new_start)`, defaulting to 1 on malformed input.
pub(crate) fn parse_hunk_header(line: &str) -> (u32, u32) {
    let mut old_start = 1u32;
    let mut new_start = 1u32;

    // Only look inside the `@@ … @@` markers: the trailing context section can
    // contain `+` and `-` characters from the enclosing function signature.
    let body = line.strip_prefix("@@").unwrap_or(line);
    let body = match body.find("@@") {
        Some(end) => &body[..end],
        None => body,
    };

    for token in body.split_whitespace() {
        if let Some(v) = token.strip_prefix('-') {
            old_start = v.split(',').next().unwrap_or("1").parse().unwrap_or(1);
        } else if let Some(v) = token.strip_prefix('+') {
            new_start = v.split(',').next().unwrap_or("1").parse().unwrap_or(1);
        }
    }

    (old_start, new_start)
}

// ─── Finding anchoring ───────────────────────────────────────────────────────

/// Snap model-reported locations onto lines that actually changed.
///
/// Small models routinely emit a plausible-looking `file`/`line` pair that
/// corresponds to nothing in the diff — they count lines in the prompt, not in
/// the file. Findings pointing at a file outside the diff are dropped; findings
/// with an off-by-N line are moved to the nearest line the diff actually
/// touched. Deterministic detectors already produce exact locations and are
/// left alone.
pub fn anchor_findings(findings: &mut Vec<crate::types::ReviewFinding>, files: &[FileDiff]) {
    if files.is_empty() {
        return;
    }

    findings.retain_mut(|f| {
        let Some(file) = match_file(&f.file, files) else {
            // A finding about a file that is not in the diff cannot be verified
            // and cannot be annotated in a PR. Drop it rather than ship a
            // location that points nowhere.
            return false;
        };

        // Normalise the path to the diff's spelling so downstream consumers
        // (SARIF, PR annotations) get a repo-relative path that resolves.
        f.file = file.path.clone();

        let changed = file.changed_new_lines();
        if changed.is_empty() {
            // Pure deletion: no post-image line to point at. Anchor to the
            // first hunk's start so the finding still lands in the right region.
            f.line = file.hunks.first().map(|h| h.new_start).unwrap_or(1);
            return true;
        }

        if changed.contains(&f.line) {
            return true;
        }

        // Snap to the nearest changed line.
        let nearest = changed
            .iter()
            .min_by_key(|&&l| (l as i64 - f.line as i64).abs())
            .copied()
            .unwrap_or(1);
        f.line = nearest;
        true
    });
}

/// Resolve a model-reported path against the diff's real paths.
/// Matches exactly, then by suffix, then by basename.
fn match_file<'a>(reported: &str, files: &'a [FileDiff]) -> Option<&'a FileDiff> {
    let reported = reported.trim().trim_start_matches("./");
    if reported.is_empty() {
        // Single-file diffs are unambiguous, so an unlabelled finding is safe
        // to attribute. With several files in play, guessing would be wrong.
        return if files.len() == 1 {
            files.first()
        } else {
            None
        };
    }

    if let Some(f) = files.iter().find(|f| f.path == reported) {
        return Some(f);
    }
    if let Some(f) = files
        .iter()
        .find(|f| f.path.ends_with(reported) || reported.ends_with(&f.path))
    {
        return Some(f);
    }

    let base = |p: &str| p.rsplit('/').next().unwrap_or(p).to_string();
    let reported_base = base(reported);
    let mut matches = files.iter().filter(|f| base(&f.path) == reported_base);
    let first = matches.next()?;
    // Ambiguous basename (two `mod.rs`) — refuse to guess.
    if matches.next().is_some() {
        None
    } else {
        Some(first)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Category, ReviewFinding, Severity};

    const SAMPLE: &str = "\
diff --git a/src/auth.rs b/src/auth.rs
index 111..222 100644
--- a/src/auth.rs
+++ b/src/auth.rs
@@ -10,6 +10,7 @@ fn login() {
 let a = 1;
-let old = 2;
+let new = 3;
+let extra = 4;
 let b = 5;
";

    #[test]
    fn parses_line_numbers_from_hunk_header() {
        let files = parse_diff(SAMPLE);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "src/auth.rs");
        let h = &files[0].hunks[0];
        assert_eq!(h.new_start, 10);

        // Context "let a = 1;" is line 10; the removed line consumes no
        // post-image number; the two added lines are 11 and 12.
        let added: Vec<_> = h
            .added()
            .map(|l| (l.new_line.unwrap(), l.text.clone()))
            .collect();
        assert_eq!(
            added,
            vec![(11, "let new = 3;".into()), (12, "let extra = 4;".into())]
        );

        let removed: Vec<_> = h.removed().map(|l| l.old_line.unwrap()).collect();
        assert_eq!(removed, vec![11], "removed lines track the pre-image");
    }

    #[test]
    fn hunk_header_ignores_plus_in_trailing_context() {
        // The function signature after the second @@ contains a '+'.
        let (old, new) = parse_hunk_header("@@ -1,5 +42,5 @@ fn add(a: i32 + b: i32) {");
        assert_eq!((old, new), (1, 42));
    }

    #[test]
    fn parses_path_containing_b_slash() {
        let files = parse_diff(
            "diff --git a/my b/dir.rs b/my b/dir.rs\n--- a/my b/dir.rs\n+++ b/my b/dir.rs\n@@ -1 +1 @@\n+x\n",
        );
        assert_eq!(files[0].path, "my b/dir.rs");
    }

    /// The `diff --git` line has to stand on its own: a binary file or a mode
    /// change carries no `+++ b/…` to correct it, and the pre-filter classifies
    /// on the header before any `+++` has been read.
    #[test]
    fn the_git_header_alone_resolves_a_path_containing_b_slash() {
        assert_eq!(
            parse_git_header_path("a/my b/dir.rs b/my b/dir.rs"),
            "my b/dir.rs",
            "the last ' b/' is the wrong split point when the path contains one"
        );
        assert_eq!(parse_git_header_path("a/src/x.rs b/src/x.rs"), "src/x.rs");
    }

    /// The even split is only valid when both sides are the same file; a rename
    /// must fall through to the `b/` side rather than report the `a/` side.
    #[test]
    fn a_rename_reports_the_post_image_path() {
        assert_eq!(parse_git_header_path("a/old.rs b/new.rs"), "new.rs");
        // Two different names that happen to be the same byte length — the case
        // an even split would silently get wrong if it did not compare halves.
        assert_eq!(parse_git_header_path("a/aa.rs b/bb.rs"), "bb.rs");
    }

    /// `core.quotePath=false` is a common setting, and a piped diff can carry
    /// raw UTF-8 regardless. Slicing the header at a byte midpoint used to land
    /// inside a codepoint and abort the process.
    #[test]
    fn a_non_ascii_path_does_not_panic_the_parser() {
        assert_eq!(parse_git_header_path("a/x.rs b/ääää.rs"), "ääää.rs");
        assert_eq!(parse_git_header_path("a/ä.rs b/ä.rs"), "ä.rs");
        assert_eq!(parse_git_header_path("a/éé b/x"), "x");

        let files = parse_diff("diff --git a/x.rs b/ääää.rs\n@@ -1 +1 @@\n+x\n");
        assert_eq!(files[0].path, "ääää.rs");
    }

    /// A diff with no `diff --git` lines — `diff -u`, `format-patch`, or a
    /// review tool piped to `--stdin`.
    ///
    /// Nothing closed the previous file, so the second `+++` merely *renamed*
    /// the first one: every hunk in the diff collapsed under the last path seen,
    /// and findings in the earlier files were reported against the wrong file.
    #[test]
    fn a_plain_multi_file_diff_keeps_its_files_apart() {
        let files = parse_diff(
            "\
--- a/src/one.rs
+++ b/src/one.rs
@@ -1,2 +1,2 @@
-let a = 1;
+let a = 2;
--- a/src/two.rs
+++ b/src/two.rs
@@ -10,2 +10,2 @@
-let b = 1;
+let b = 2;
",
        );

        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["src/one.rs", "src/two.rs"]);
        assert_eq!(
            files[0].hunks.len(),
            1,
            "one hunk each, not both in one file"
        );
        assert_eq!(files[1].hunks.len(), 1);
        assert_eq!(files[0].hunks[0].new_start, 1);
        assert_eq!(files[1].hunks[0].new_start, 10);
        assert_eq!(
            files[1].hunks[0].added().next().map(|l| l.text.as_str()),
            Some("let b = 2;")
        );
    }

    /// The pair detection must not fire on content. A removed SQL comment
    /// (`-- note`) renders as `--- note`, which is why the `@@` is required too.
    #[test]
    fn a_removed_sql_comment_is_not_a_file_boundary() {
        let files = parse_diff(
            "\
diff --git a/q.sql b/q.sql
+++ b/q.sql
@@ -1,4 +1,4 @@
 select 1;
--- old note
+++ new note
 select 2;
",
        );
        assert_eq!(files.len(), 1, "one file, not two");
        assert_eq!(files[0].path, "q.sql");
        let removed: Vec<&str> = files[0].hunks[0]
            .removed()
            .map(|l| l.text.as_str())
            .collect();
        assert_eq!(removed, ["-- old note"], "still a removed content line");
    }

    fn f(file: &str, line: u32) -> ReviewFinding {
        ReviewFinding {
            file: file.into(),
            line,
            severity: Severity::Medium,
            category: Category::Quality,
            issue: "x".into(),
            suggested_fix: String::new(),
            confidence: None,
            rule_id: None,
            rule: None,
            unit_id: None,
        }
    }

    #[test]
    fn anchoring_snaps_to_nearest_changed_line() {
        let files = parse_diff(SAMPLE);
        let mut findings = vec![f("src/auth.rs", 400)];
        anchor_findings(&mut findings, &files);
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].line, 12,
            "should snap to the closest added line"
        );
    }

    #[test]
    fn anchoring_drops_findings_for_files_not_in_the_diff() {
        let files = parse_diff(SAMPLE);
        let mut findings = vec![f("src/imaginary.rs", 3)];
        anchor_findings(&mut findings, &files);
        assert!(findings.is_empty(), "hallucinated file should not survive");
    }

    #[test]
    fn anchoring_normalises_a_suffix_path() {
        let files = parse_diff(SAMPLE);
        let mut findings = vec![f("auth.rs", 11)];
        anchor_findings(&mut findings, &files);
        assert_eq!(findings[0].file, "src/auth.rs");
        assert_eq!(findings[0].line, 11);
    }
}
