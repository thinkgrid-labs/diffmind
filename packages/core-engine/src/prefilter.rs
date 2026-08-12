//! Deterministic noise removal, applied before anything costs a token.
//!
//! Most of a real diff is not reviewable: lockfiles, generated clients, minified
//! bundles, and the reformat churn a formatter leaves behind. Dropping it is the
//! single highest-leverage thing in the pipeline — it costs nothing and removes
//! the majority of a typical branch.
//!
//! This used to live entirely in git pathspec excludes (`:!pnpm-lock.yaml` and
//! friends). That worked, but an excluded file never enters the diff at all, so
//! there was no way to tell the user *what* was dropped — and "312 hunks → 74
//! reviewable" is exactly the number that makes a reviewer trust the tool. Path
//! exclusion now happens here, in process, where it can be counted.
//!
//! Filtering is textual and works at file and hunk granularity. Surviving bytes
//! are passed through unchanged, so line numbers, hunk headers and anchoring all
//! keep working without re-serialising the diff.

use crate::detectors::file_matches_glob;
use std::collections::{BTreeMap, HashSet};

/// Why a file or hunk was not worth reviewing. Ordered by how often it fires on
/// a real diff, which is also the order it reads best in the summary line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum DropReason {
    Lockfile,
    Generated,
    Minified,
    Asset,
    Snapshot,
    /// Whitespace-only or pure-reformat change.
    Reformat,
    /// Matched a glob from `.diffmind/config.toml`.
    UserIgnored,
}

impl DropReason {
    pub fn label(self) -> &'static str {
        match self {
            DropReason::Lockfile => "lockfiles",
            DropReason::Generated => "generated",
            DropReason::Minified => "minified",
            DropReason::Asset => "assets",
            DropReason::Snapshot => "snapshots",
            DropReason::Reformat => "formatting",
            DropReason::UserIgnored => "ignored",
        }
    }
}

#[derive(Debug, Default, Clone)]
pub struct PrefilterReport {
    pub hunks_total: usize,
    pub hunks_kept: usize,
    pub files_total: usize,
    pub files_kept: usize,
    /// Hunks dropped, by reason.
    pub dropped: BTreeMap<DropReason, usize>,
}

impl PrefilterReport {
    pub fn hunks_dropped(&self) -> usize {
        self.hunks_total.saturating_sub(self.hunks_kept)
    }

    /// `lockfiles, generated, formatting` — reasons ordered by weight, so the
    /// biggest contributor is named first.
    pub fn reason_summary(&self) -> String {
        let mut reasons: Vec<(&DropReason, &usize)> = self.dropped.iter().collect();
        reasons.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        reasons
            .iter()
            .map(|(r, _)| r.label())
            .collect::<Vec<_>>()
            .join(", ")
    }

    pub fn dropped_everything(&self) -> bool {
        self.hunks_total > 0 && self.hunks_kept == 0
    }
}

#[derive(Debug, Default, Clone)]
pub struct PrefilterOptions {
    /// Paths the caller already knows are generated — from
    /// `.gitattributes linguist-generated` and from sniffing file headers.
    /// Resolved by the caller because it needs git and the filesystem, which
    /// the engine deliberately does not touch.
    pub generated_paths: HashSet<String>,
    /// User globs from `.diffmind/config.toml`.
    pub ignore_globs: Vec<String>,
}

/// Filenames that are always a lockfile, regardless of directory.
const LOCKFILE_NAMES: &[&str] = &[
    "pnpm-lock.yaml",
    "package-lock.json",
    "yarn.lock",
    "npm-shrinkwrap.json",
    "bun.lockb",
    "Cargo.lock",
    "poetry.lock",
    "Pipfile.lock",
    "uv.lock",
    "composer.lock",
    "Gemfile.lock",
    "go.sum",
    "flake.lock",
    "pubspec.lock",
    "gradle.lockfile",
];

const ASSET_EXTS: &[&str] = &[
    "svg", "png", "jpg", "jpeg", "gif", "webp", "ico", "pdf", "woff", "woff2", "ttf", "eot", "mp4",
    "mp3", "zip", "gz", "wasm",
];

/// Extensions whose indentation is load-bearing, so a leading-whitespace change
/// is a semantic change and must never be dismissed as reformatting.
const INDENT_SENSITIVE_EXTS: &[&str] = &[
    "py", "yaml", "yml", "pyi", "coffee", "haml", "slim", "sass", "nim", "jade", "pug",
];

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn extension(path: &str) -> &str {
    let base = basename(path);
    match base.rfind('.') {
        Some(i) if i + 1 < base.len() => &base[i + 1..],
        _ => "",
    }
}

/// Classify a whole file. `None` means the file is reviewable.
fn classify_path(path: &str, opts: &PrefilterOptions) -> Option<DropReason> {
    // User globs win: an explicit ignore is a decision, not a heuristic.
    if opts.ignore_globs.iter().any(|g| file_matches_glob(path, g)) {
        return Some(DropReason::UserIgnored);
    }
    if opts.generated_paths.contains(path) {
        return Some(DropReason::Generated);
    }

    let base = basename(path);
    if LOCKFILE_NAMES.contains(&base) {
        return Some(DropReason::Lockfile);
    }

    let ext = extension(path);
    if base.ends_with(".min.js") || base.ends_with(".min.css") || ext == "map" {
        return Some(DropReason::Minified);
    }
    if ext == "snap" || path.contains("/__snapshots__/") {
        return Some(DropReason::Snapshot);
    }
    if ASSET_EXTS.contains(&ext) {
        return Some(DropReason::Asset);
    }

    None
}

/// Normalise a line for reformat comparison.
///
/// Whitespace **outside** string literals is collapsed to one space
/// (`collapse`) or removed entirely; whitespace **inside** a literal is content
/// and is always preserved, so `"hello  world"` → `"hello world"` reads as the
/// behaviour change it is rather than as a formatter pass.
///
/// Leading indentation is preserved only for languages where it changes
/// meaning — in Python, re-indenting a block moves it into or out of a scope,
/// which is precisely the kind of change a reviewer must see. Indentation is
/// compared by raw width, so a tabs-to-spaces conversion also survives.
///
/// An unterminated quote (an apostrophe in a comment, say) makes the rest of
/// the line compare verbatim. That errs towards keeping the hunk, which is the
/// safe direction.
fn normalize_for_reformat(line: &str, indent_sensitive: bool, collapse: bool) -> String {
    let indent = if indent_sensitive {
        line.len() - line.trim_start().len()
    } else {
        0
    };

    let mut body = String::with_capacity(line.len());
    let mut in_string = false;
    let mut delimiter = ' ';
    let mut escaped = false;
    let mut pending_space = false;

    for ch in line.chars() {
        if in_string {
            body.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == delimiter {
                in_string = false;
            }
            continue;
        }

        if ch.is_whitespace() {
            pending_space = true;
            continue;
        }
        // A run of whitespace becomes one space, or nothing. Either way a run
        // at the end of the line is simply dropped.
        if pending_space && collapse && !body.is_empty() {
            body.push(' ');
        }
        pending_space = false;

        if ch == '"' || ch == '\'' || ch == '`' {
            in_string = true;
            delimiter = ch;
        }
        body.push(ch);
    }

    if indent > 0 {
        format!("{}{body}", " ".repeat(indent))
    } else {
        body
    }
}

/// True when a hunk changes only whitespace — trailing spaces, re-wrapping, a
/// formatter pass. Compared as ordered sequences: a hunk that moves a line to a
/// different position changed the program, even if the text is all still there.
fn is_reformat_only(added: &[&str], removed: &[&str], indent_sensitive: bool) -> bool {
    if added.is_empty() && removed.is_empty() {
        return false;
    }

    let seq = |lines: &[&str], collapse: bool| -> Vec<String> {
        lines
            .iter()
            .map(|l| normalize_for_reformat(l, indent_sensitive, collapse))
            .filter(|l| !l.trim().is_empty())
            .collect()
    };

    // Spacing preserved: catches trailing whitespace, blank-line churn, and
    // re-indentation in languages where indentation carries no meaning.
    if seq(added, true) == seq(removed, true) {
        return true;
    }

    // Spacing removed: catches `x=1` → `x = 1`, which is what a formatter
    // actually does most of the time. Safe to apply unconditionally because
    // `normalize_for_reformat` never touches whitespace inside a literal.
    seq(added, false) == seq(removed, false)
}

/// Drop the noise from a raw unified diff.
///
/// Returns the surviving diff text (original bytes, minus whole files and whole
/// hunks) and a report of what went.
pub fn prefilter(diff: &str, opts: &PrefilterOptions) -> (String, PrefilterReport) {
    let mut out = String::with_capacity(diff.len());
    let mut report = PrefilterReport::default();
    let mut file = FileState::default();
    let mut hunk = HunkState::default();

    let lines: Vec<&str> = diff.lines().collect();
    for (i, line) in lines.iter().copied().enumerate() {
        if line.starts_with("diff --git ") {
            flush_file(&mut out, &mut report, &mut file, &mut hunk);
            file.classify(&header_path(line), opts);
            file.header.push(line.to_string());
            continue;
        }

        // A `--- old` / `+++ new` pair opens a file when nothing else has.
        //
        // `git diff` announces each file with a `diff --git` line, so the pair
        // that follows belongs to a file already open. A plain `diff -u` — or
        // anything piped to `--stdin` — has no such line, and then this pair is
        // the only thing separating one file from the next. Without recognising
        // it, the first file's header was dropped (leaving hunks that could not
        // be attributed to any path) and, in a multi-file diff, the second
        // file's header arrived while a hunk was open and was counted as `-` and
        // `+` content lines — silently corrupting both files.
        let opens_a_file = crate::diff::starts_file_header_pair(&lines, i)
            && (file.header.is_empty() || !hunk.lines.is_empty());
        if opens_a_file {
            flush_file(&mut out, &mut report, &mut file, &mut hunk);
        }

        // Header lines belong to the file, not to any hunk, and are replayed
        // only if some hunk of that file survives.
        if hunk.lines.is_empty()
            && (opens_a_file || !file.header.is_empty())
            && is_file_header_line(line)
        {
            // `+++ b/path` is the authoritative path; re-classify against it.
            if let Some(rest) = line.strip_prefix("+++ ") {
                let p = rest.trim();
                if p != "/dev/null" {
                    file.classify(p.strip_prefix("b/").unwrap_or(p), opts);
                }
            }
            file.header.push(line.to_string());
            continue;
        }

        if line.starts_with("@@") {
            flush_hunk(&mut out, &mut report, &mut file, &mut hunk);
            hunk.lines.push(line.to_string());
            continue;
        }

        if hunk.lines.is_empty() {
            continue;
        }

        match line.chars().next() {
            Some('+') => hunk.added.push(line[1..].to_string()),
            Some('-') => hunk.removed.push(line[1..].to_string()),
            // "\ No newline at end of file" is metadata; keep it with the hunk
            // but do not let it count as content.
            _ => {}
        }
        hunk.lines.push(line.to_string());
    }

    flush_file(&mut out, &mut report, &mut file, &mut hunk);

    (out, report)
}

/// The file currently being walked.
#[derive(Default)]
struct FileState {
    /// `diff --git`, `index`, `---`, `+++` … replayed only if a hunk survives.
    header: Vec<String>,
    /// Set when the whole file is noise, which short-circuits every hunk in it.
    reason: Option<DropReason>,
    indent_sensitive: bool,
    kept_any: bool,
    header_emitted: bool,
}

impl FileState {
    fn classify(&mut self, path: &str, opts: &PrefilterOptions) {
        self.reason = classify_path(path, opts);
        self.indent_sensitive = INDENT_SENSITIVE_EXTS.contains(&extension(path));
    }
}

/// The hunk currently being buffered. Buffered rather than streamed because
/// whether it survives is only known once its last line has been read.
#[derive(Default)]
struct HunkState {
    lines: Vec<String>,
    added: Vec<String>,
    removed: Vec<String>,
}

pub(crate) fn is_file_header_line(line: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "index ",
        "--- ",
        "+++ ",
        "old mode",
        "new mode",
        "new file mode",
        "deleted file mode",
        "similarity index",
        "rename ",
        "copy ",
        "Binary files",
    ];
    PREFIXES.iter().any(|p| line.starts_with(p))
}

fn flush_hunk(
    out: &mut String,
    report: &mut PrefilterReport,
    file: &mut FileState,
    hunk: &mut HunkState,
) {
    if hunk.lines.is_empty() {
        return;
    }
    report.hunks_total += 1;

    let added: Vec<&str> = hunk.added.iter().map(String::as_str).collect();
    let removed: Vec<&str> = hunk.removed.iter().map(String::as_str).collect();
    let reason = file.reason.or_else(|| {
        is_reformat_only(&added, &removed, file.indent_sensitive).then_some(DropReason::Reformat)
    });

    match reason {
        Some(reason) => {
            *report.dropped.entry(reason).or_insert(0) += 1;
        }
        None => {
            report.hunks_kept += 1;
            if !file.header_emitted {
                for h in &file.header {
                    out.push_str(h);
                    out.push('\n');
                }
                file.header_emitted = true;
            }
            for l in &hunk.lines {
                out.push_str(l);
                out.push('\n');
            }
            file.kept_any = true;
        }
    }

    *hunk = HunkState::default();
}

fn flush_file(
    out: &mut String,
    report: &mut PrefilterReport,
    file: &mut FileState,
    hunk: &mut HunkState,
) {
    flush_hunk(out, report, file, hunk);
    if !file.header.is_empty() {
        report.files_total += 1;
        if file.kept_any {
            report.files_kept += 1;
        }
    }
    *file = FileState::default();
}

/// Post-image path from a `diff --git a/x b/x` line. Deliberately simple: the
/// `+++ b/…` line that follows corrects us whenever it is not.
pub(crate) fn header_path(line: &str) -> String {
    let rest = line.strip_prefix("diff --git ").unwrap_or(line).trim();
    match rest.rfind(" b/") {
        Some(i) => rest[i + 3..].trim().to_string(),
        None => rest.to_string(),
    }
}

/// Does this file's content mark it as generated?
///
/// Checks the first few lines only — the convention (Go, protobuf, OpenAPI
/// generators, `linguist-generated`) is always a header banner.
pub fn looks_generated(contents: &str) -> bool {
    const MARKERS: &[&str] = &[
        "@generated",
        "do not edit",
        "code generated by",
        "auto-generated",
        "autogenerated",
        "automatically generated",
        "this file is generated",
    ];
    contents
        .lines()
        .take(5)
        .map(|l| l.to_lowercase())
        .any(|l| MARKERS.iter().any(|m| l.contains(m)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> PrefilterOptions {
        PrefilterOptions::default()
    }

    fn count(diff: &str, opts: &PrefilterOptions) -> (String, PrefilterReport) {
        prefilter(diff, opts)
    }

    const LOCKFILE_PLUS_CODE: &str = "\
diff --git a/pnpm-lock.yaml b/pnpm-lock.yaml
--- a/pnpm-lock.yaml
+++ b/pnpm-lock.yaml
@@ -1,3 +1,3 @@
-  resolution: {integrity: sha512-aaa}
+  resolution: {integrity: sha512-bbb}
diff --git a/src/auth.rs b/src/auth.rs
--- a/src/auth.rs
+++ b/src/auth.rs
@@ -10,2 +10,2 @@
-let token = None;
+let token = Some(secret);
";

    #[test]
    fn a_lockfile_is_dropped_and_counted_while_code_survives() {
        let (out, report) = count(LOCKFILE_PLUS_CODE, &opts());

        assert!(out.contains("src/auth.rs"));
        assert!(out.contains("+let token = Some(secret);"));
        assert!(
            !out.contains("pnpm-lock.yaml"),
            "the lockfile should not reach the model"
        );

        assert_eq!(report.hunks_total, 2);
        assert_eq!(report.hunks_kept, 1);
        assert_eq!(report.files_total, 2);
        assert_eq!(report.files_kept, 1);
        assert_eq!(report.dropped.get(&DropReason::Lockfile), Some(&1));
        // The number the reviewer actually reads.
        assert_eq!(report.reason_summary(), "lockfiles");
    }

    #[test]
    fn surviving_bytes_are_passed_through_unchanged() {
        // Anchoring depends on hunk headers and line numbers surviving intact.
        let (out, _) = count(LOCKFILE_PLUS_CODE, &opts());
        assert!(out.contains("@@ -10,2 +10,2 @@"));
        assert!(out.contains("diff --git a/src/auth.rs b/src/auth.rs"));
        assert!(out.contains("+++ b/src/auth.rs"));
    }

    #[test]
    fn a_pure_formatter_pass_leaves_nothing_to_review() {
        let diff = "\
diff --git a/src/a.rs b/src/a.rs
+++ b/src/a.rs
@@ -1,2 +1,2 @@
-let x=1;
+let x = 1;
-fn  f( ) {}
+fn f() {}
";
        let (out, report) = count(diff, &opts());
        assert!(
            out.trim().is_empty(),
            "a reformat-only diff must cost zero inference passes, got:\n{out}"
        );
        assert_eq!(report.dropped.get(&DropReason::Reformat), Some(&1));
        assert!(report.dropped_everything());
    }

    #[test]
    fn trailing_whitespace_churn_is_reformatting() {
        // Built by concatenation so the trailing spaces are unambiguous and
        // cannot be stripped by an editor or a formatter run on this file.
        let diff = format!(
            "diff --git a/src/a.rs b/src/a.rs\n+++ b/src/a.rs\n@@ -1,1 +1,1 @@\n-{}\n+{}\n",
            "let x = 1;   ", "let x = 1;"
        );
        let (_, report) = count(&diff, &opts());
        assert_eq!(report.hunks_kept, 0);
    }

    #[test]
    fn whitespace_inside_a_string_literal_is_content_not_formatting() {
        // `"a  b"` → `"a b"` changes what the program prints. Removing all
        // whitespace before comparing would dismiss it as a formatter pass.
        let diff = "\
diff --git a/src/a.rs b/src/a.rs
+++ b/src/a.rs
@@ -1,1 +1,1 @@
-let msg = \"hello  world\";
+let msg = \"hello world\";
";
        let (_, report) = count(diff, &opts());
        assert_eq!(
            report.hunks_kept, 1,
            "a change inside a string literal must survive the pre-filter"
        );
    }

    #[test]
    fn reindenting_python_is_not_reformatting() {
        // Moving a line out of an `if` changes when it runs. Dismissing this as
        // whitespace would hide a real bug.
        let diff = "\
diff --git a/app.py b/app.py
+++ b/app.py
@@ -1,3 +1,3 @@
 if ok:
-    commit()
+commit()
";
        let (out, report) = count(diff, &opts());
        assert_eq!(
            report.hunks_kept, 1,
            "indentation is semantic in Python and must survive"
        );
        assert!(out.contains("commit()"));
    }

    #[test]
    fn reordering_lines_is_not_reformatting() {
        // Same tokens, different order — that is a behaviour change.
        let diff = "\
diff --git a/src/a.rs b/src/a.rs
+++ b/src/a.rs
@@ -1,2 +1,2 @@
-close();
-open();
+open();
+close();
";
        let (_, report) = count(diff, &opts());
        assert_eq!(report.hunks_kept, 1);
    }

    #[test]
    fn one_reformat_hunk_does_not_drop_its_files_real_hunk() {
        let diff = "\
diff --git a/src/a.rs b/src/a.rs
+++ b/src/a.rs
@@ -1,1 +1,1 @@
-let x=1;
+let x = 1;
@@ -50,1 +50,1 @@
-let safe = true;
+let safe = false;
";
        let (out, report) = count(diff, &opts());
        assert_eq!(report.hunks_total, 2);
        assert_eq!(report.hunks_kept, 1);
        assert!(out.contains("+let safe = false;"));
        assert!(!out.contains("+let x = 1;"));
        assert!(
            out.contains("diff --git"),
            "the surviving hunk still needs its file header or findings cannot be attributed"
        );
    }

    #[test]
    fn generated_paths_from_the_caller_are_dropped() {
        let diff = "\
diff --git a/src/api/client.ts b/src/api/client.ts
+++ b/src/api/client.ts
@@ -1,1 +1,1 @@
+export const x = 1;
";
        let mut o = opts();
        o.generated_paths.insert("src/api/client.ts".to_string());
        let (out, report) = count(diff, &o);
        assert!(out.trim().is_empty());
        assert_eq!(report.dropped.get(&DropReason::Generated), Some(&1));
    }

    #[test]
    fn user_globs_are_honoured() {
        let diff = "\
diff --git a/src/legacy/x.ts b/src/legacy/x.ts
+++ b/src/legacy/x.ts
@@ -1,1 +1,1 @@
+const x = 1;
";
        let mut o = opts();
        o.ignore_globs.push("**/legacy/**".to_string());
        let (out, report) = count(diff, &o);
        assert!(out.trim().is_empty());
        assert_eq!(report.dropped.get(&DropReason::UserIgnored), Some(&1));
    }

    #[test]
    fn minified_and_asset_files_are_classified_separately() {
        assert_eq!(
            classify_path("dist/app.min.js", &opts()),
            Some(DropReason::Minified)
        );
        assert_eq!(
            classify_path("dist/app.js.map", &opts()),
            Some(DropReason::Minified)
        );
        assert_eq!(
            classify_path("public/logo.svg", &opts()),
            Some(DropReason::Asset)
        );
        assert_eq!(
            classify_path("src/__snapshots__/a.snap", &opts()),
            Some(DropReason::Snapshot)
        );
        assert_eq!(classify_path("src/main.rs", &opts()), None);
    }

    #[test]
    fn nested_lockfiles_are_matched_by_basename() {
        assert_eq!(
            classify_path("apps/web/pnpm-lock.yaml", &opts()),
            Some(DropReason::Lockfile)
        );
        assert_eq!(
            classify_path("services/api/go.sum", &opts()),
            Some(DropReason::Lockfile)
        );
    }

    #[test]
    fn a_clean_diff_passes_through_untouched() {
        let diff = "\
diff --git a/src/a.rs b/src/a.rs
+++ b/src/a.rs
@@ -1,1 +1,1 @@
-let a = 1;
+let b = 2;
";
        let (out, report) = count(diff, &opts());
        assert_eq!(report.hunks_kept, report.hunks_total);
        assert_eq!(report.hunks_dropped(), 0);
        assert!(out.contains("+let b = 2;"));
        assert!(report.reason_summary().is_empty());
    }

    /// Real `git diff` output carries `index`, mode and rename metadata that the
    /// hand-written fixtures above omit. If any of it were dropped or reordered,
    /// downstream parsing and finding-anchoring would drift.
    #[test]
    fn realistic_git_output_round_trips_byte_for_byte() {
        let diff = "\
diff --git a/src/auth.rs b/src/auth.rs
index 6a1b2c3..7d4e5f6 100644
--- a/src/auth.rs
+++ b/src/auth.rs
@@ -10,3 +10,3 @@ impl Session {
     fn login(&self) {
-        let token = None;
+        let token = Some(self.mint());
     }
@@ -40,2 +40,2 @@ impl Session {
-        expire();
+        expire_all();
diff --git a/src/new.rs b/src/new.rs
new file mode 100644
index 0000000..1234567
--- /dev/null
+++ b/src/new.rs
@@ -0,0 +1,2 @@
+pub fn added() {}
+
";
        let (out, report) = count(diff, &opts());
        assert_eq!(report.files_total, 2);
        assert_eq!(report.files_kept, 2);
        assert_eq!(report.hunks_total, 3);
        assert_eq!(report.hunks_kept, 3);
        assert_eq!(
            out, diff,
            "a diff with no noise in it must survive untouched"
        );
    }

    /// `git diff main...HEAD | diffmind --stdin` is documented, but a diff can
    /// reach stdin from `diff -u`, `git format-patch`, or a code-review tool,
    /// and then there is no `diff --git` line at all.
    ///
    /// Both halves of this used to be broken: the first file's header was
    /// dropped entirely, and the second file's header arrived while a hunk was
    /// open and was counted as removed/added *content* — so two files became one
    /// corrupted hunk with no path attached to either.
    #[test]
    fn a_plain_diff_with_no_git_headers_keeps_its_files_apart() {
        let diff = "\
--- a/src/one.rs
+++ b/src/one.rs
@@ -1,2 +1,2 @@
-let a = 1;
+let a = 2;
--- a/src/two.rs
+++ b/src/two.rs
@@ -1,2 +1,2 @@
-let b = 1;
+let b = 2;
";
        let (out, report) = count(diff, &opts());

        assert_eq!(report.files_total, 2, "two files, not one");
        assert_eq!(report.hunks_total, 2);
        assert_eq!(report.hunks_kept, 2);
        assert_eq!(out, diff, "a clean plain diff must survive untouched");

        // The paths have to reach the model, or no finding can be anchored.
        assert!(out.contains("+++ b/src/one.rs"));
        assert!(out.contains("+++ b/src/two.rs"));

        // And the second file's header must not have been eaten as content.
        let files = crate::diff::parse_diff(&out);
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["src/one.rs", "src/two.rs"]);
    }

    /// Classification works off the `+++` path, so the noise rules apply to a
    /// piped diff exactly as they do to one diffmind ran itself.
    #[test]
    fn a_plain_diff_is_still_filtered_by_path() {
        let diff = "\
--- a/pnpm-lock.yaml
+++ b/pnpm-lock.yaml
@@ -1,2 +1,2 @@
-  integrity: sha512-aaa
+  integrity: sha512-bbb
--- a/src/auth.rs
+++ b/src/auth.rs
@@ -1,2 +1,2 @@
-let token = None;
+let token = Some(secret);
";
        let (out, report) = count(diff, &opts());
        assert_eq!(report.dropped.get(&DropReason::Lockfile), Some(&1));
        assert!(!out.contains("pnpm-lock.yaml"));
        assert!(out.contains("+let token = Some(secret);"));
        assert!(
            out.contains("+++ b/src/auth.rs"),
            "the surviving file still needs its header"
        );
    }

    /// The pair detection must not fire on ordinary content. A removed SQL
    /// comment renders as `--- note`, which is why the `@@` is required too.
    #[test]
    fn a_removed_sql_comment_is_not_mistaken_for_a_file_header() {
        let diff = "\
diff --git a/q.sql b/q.sql
+++ b/q.sql
@@ -1,4 +1,4 @@
 select 1;
--- old note
+++ new note
 select 2;
";
        let (out, report) = count(diff, &opts());
        assert_eq!(report.files_total, 1, "one file, not two");
        assert_eq!(report.hunks_total, 1);
        assert_eq!(out, diff, "content must pass through untouched");
    }

    #[test]
    fn empty_input_is_not_an_error() {
        let (out, report) = count("", &opts());
        assert!(out.is_empty());
        assert_eq!(report.hunks_total, 0);
        assert!(!report.dropped_everything(), "nothing in, nothing dropped");
    }

    #[test]
    fn generated_marker_is_detected_in_a_header_banner() {
        assert!(looks_generated(
            "// Code generated by protoc. DO NOT EDIT.\n"
        ));
        assert!(looks_generated("/* @generated */\nconst x = 1;\n"));
        assert!(looks_generated("#\n# Automatically generated file\n#\n"));
        assert!(!looks_generated("fn main() {}\n"));
        assert!(
            !looks_generated(&format!("{}\n// @generated\n", "x\n".repeat(10))),
            "a marker buried mid-file is a comment, not a banner"
        );
    }

    #[test]
    fn reason_summary_names_the_biggest_contributor_first() {
        let mut r = PrefilterReport::default();
        r.dropped.insert(DropReason::Lockfile, 1);
        r.dropped.insert(DropReason::Reformat, 9);
        assert_eq!(r.reason_summary(), "formatting, lockfiles");
    }
}
