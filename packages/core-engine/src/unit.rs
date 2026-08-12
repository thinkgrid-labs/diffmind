//! Review units — the thing that gets sent to the model, and cached.
//!
//! Chunking used to be purely arithmetic: accumulate lines until the budget is
//! full, cut, repeat. That has two costs.
//!
//! The first is caching. A chunk held every hunk of a file up to the line
//! budget, so editing one line at the top of a file changed the chunk text and
//! re-inferred all of its other hunks too. Grouping by *region* means a change
//! in one function only invalidates the unit covering that function.
//!
//! The second is coherence. An arithmetic cut lands wherever the budget runs
//! out — frequently mid-function, and sometimes separating two hunks that only
//! make sense read together. Units group hunks that sit near each other in the
//! file, which is the boundary a human reviewer would pick.
//!
//! The trade is more model calls on a cold run: three units pay the system
//! prompt three times where one chunk paid it once. `ADJACENCY_GAP_LINES` is
//! set generously, and `MAX_UNITS_PER_FILE` caps the fragmentation, so a
//! typical file still yields one or two units.

use crate::diff::parse_hunk_header;
use crate::prefilter::{header_path, is_file_header_line};
use sha2::{Digest, Sha256};

/// Hunks closer than this in the post-image are read together. Git already
/// merges hunks within ~6 lines of each other (two lots of default context), so
/// anything under this threshold is "the same region of the file".
const ADJACENCY_GAP_LINES: u32 = 30;

/// Ceiling on how far one file may fragment. Without it, a file with twenty
/// scattered one-line edits would cost twenty model calls.
const MAX_UNITS_PER_FILE: usize = 8;

/// One reviewable region of one file.
#[derive(Debug, Clone)]
pub struct ReviewUnit {
    /// Files this unit covers. Usually one; more when the graph linked a
    /// changed symbol to callers that changed in the same diff.
    pub files: Vec<String>,
    /// Self-contained diff text: the file's header, then this unit's hunks.
    /// Original bytes, so hunk headers and line numbers are untouched.
    pub text: String,
    /// Stable identity for this region's content. Derived from the path and the
    /// unit's own text only, so it does not move when an unrelated file in the
    /// same diff changes.
    pub id: String,
    pub hunk_count: usize,
    /// Post-image span this unit covers, for attributing a finding to a unit.
    pub new_start: u32,
    pub new_end: u32,
}

impl ReviewUnit {
    /// The file this unit is primarily about — the one its line span refers to.
    pub fn file(&self) -> &str {
        self.files.first().map(String::as_str).unwrap_or("")
    }

    /// Does this unit cover `line` in `file`? Answers for the primary region;
    /// a merged unit's other files are carried in `files`.
    pub fn covers(&self, file: &str, line: u32) -> bool {
        self.file() == file && line >= self.new_start && line <= self.new_end
    }

    /// Combine two units into one review.
    ///
    /// Used when a changed symbol and code that calls it were **both** edited.
    /// Reviewed apart, the model judges an interaction while seeing only one
    /// side of it as background — and pays for the other side's context twice.
    /// Together it is one call, one context, and the actual question.
    pub fn merged_with(&self, other: &ReviewUnit) -> ReviewUnit {
        let mut files = self.files.clone();
        for f in &other.files {
            if !files.contains(f) {
                files.push(f.clone());
            }
        }

        let text = format!("{}{}", self.text, other.text);
        let mut h = Sha256::new();
        for f in &files {
            h.update(f.as_bytes());
            h.update(b"\x00");
        }
        h.update(text.as_bytes());

        ReviewUnit {
            files,
            text,
            id: format!("{:x}", h.finalize())[..16].to_string(),
            hunk_count: self.hunk_count + other.hunk_count,
            // The span still describes the primary file; a range across two
            // files would not mean anything.
            new_start: self.new_start,
            new_end: self.new_end,
        }
    }

    pub fn line_count(&self) -> usize {
        self.text.lines().count()
    }
}

#[derive(Debug)]
struct RawHunk {
    new_start: u32,
    new_end: u32,
    lines: Vec<String>,
}

#[derive(Debug)]
struct RawFile {
    path: String,
    header: Vec<String>,
    hunks: Vec<RawHunk>,
}

/// Split a diff into review units.
///
/// `max_lines` bounds a unit so it has a chance of fitting the model's window;
/// a unit that still overruns is halved by the analyzer as a last resort. A
/// single hunk larger than the budget becomes its own unit rather than being
/// cut mid-hunk here.
pub fn build_units(diff: &str, max_lines: usize) -> Vec<ReviewUnit> {
    let max_lines = max_lines.max(1);
    let mut units = Vec::new();

    for file in split_files(diff) {
        if file.hunks.is_empty() {
            // A rename or mode change with no content. Sending the header alone
            // asks the model to review nothing, and it obliges by inventing
            // something.
            continue;
        }

        let header_lines = file.header.len();
        for group in group_hunks(&file.hunks, max_lines.saturating_sub(header_lines).max(1)) {
            units.push(assemble(&file, &group));
        }
    }

    units
}

/// Group a file's hunks into regions: merge everything within
/// `ADJACENCY_GAP_LINES`, then keep merging the closest pair until the file is
/// under both the unit cap and the line budget.
fn group_hunks(hunks: &[RawHunk], max_lines: usize) -> Vec<Vec<usize>> {
    let mut groups: Vec<Vec<usize>> = (0..hunks.len()).map(|i| vec![i]).collect();
    if groups.len() <= 1 {
        return groups;
    }

    let span = |g: &Vec<usize>| -> (u32, u32) {
        let start = g.iter().map(|&i| hunks[i].new_start).min().unwrap_or(0);
        let end = g.iter().map(|&i| hunks[i].new_end).max().unwrap_or(0);
        (start, end)
    };
    let lines = |g: &Vec<usize>| -> usize { g.iter().map(|&i| hunks[i].lines.len()).sum() };

    // Pass 1: merge neighbours that sit in the same region of the file.
    let mut merged: Vec<Vec<usize>> = Vec::new();
    for group in groups.drain(..) {
        match merged.last_mut() {
            Some(prev)
                if span(&group).0.saturating_sub(span(prev).1) <= ADJACENCY_GAP_LINES
                    && lines(prev) + lines(&group) <= max_lines =>
            {
                prev.extend(group);
            }
            _ => merged.push(group),
        }
    }

    // Pass 2: if the file still fragments past the cap, merge the closest
    // remaining pair until it does not. Budget still wins over the cap — an
    // over-long unit would only be halved again downstream.
    while merged.len() > MAX_UNITS_PER_FILE {
        let Some(best) = (0..merged.len() - 1)
            .filter(|&i| lines(&merged[i]) + lines(&merged[i + 1]) <= max_lines)
            .min_by_key(|&i| span(&merged[i + 1]).0.saturating_sub(span(&merged[i]).1))
        else {
            // Every remaining pair would overrun the budget. Fragmentation is
            // the lesser evil.
            break;
        };
        let tail = merged.remove(best + 1);
        merged[best].extend(tail);
    }

    merged
}

fn assemble(file: &RawFile, group: &[usize]) -> ReviewUnit {
    let mut text = String::new();
    for line in &file.header {
        text.push_str(line);
        text.push('\n');
    }
    for &i in group {
        for line in &file.hunks[i].lines {
            text.push_str(line);
            text.push('\n');
        }
    }

    let new_start = group
        .iter()
        .map(|&i| file.hunks[i].new_start)
        .min()
        .unwrap_or(0);
    let new_end = group
        .iter()
        .map(|&i| file.hunks[i].new_end)
        .max()
        .unwrap_or(0);

    // Path is hashed alongside the text so two files with identical content
    // (a copied fixture, say) stay distinguishable.
    let mut h = Sha256::new();
    h.update(file.path.as_bytes());
    h.update(b"\x00");
    h.update(text.as_bytes());
    let id = format!("{:x}", h.finalize())[..16].to_string();

    ReviewUnit {
        files: vec![file.path.clone()],
        text,
        id,
        hunk_count: group.len(),
        new_start,
        new_end,
    }
}

/// Walk the diff text into per-file headers and raw hunks, preserving bytes.
fn split_files(diff: &str) -> Vec<RawFile> {
    let mut files: Vec<RawFile> = Vec::new();
    let mut hunk: Option<RawHunk> = None;

    // Close the open hunk into the open file.
    fn flush(files: &mut [RawFile], hunk: &mut Option<RawHunk>) {
        if let (Some(file), Some(h)) = (files.last_mut(), hunk.take()) {
            file.hunks.push(h);
        }
    }

    let lines: Vec<&str> = diff.lines().collect();
    for (i, line) in lines.iter().copied().enumerate() {
        if line.starts_with("diff --git ") {
            flush(&mut files, &mut hunk);
            files.push(RawFile {
                path: header_path(line),
                header: vec![line.to_string()],
                hunks: Vec::new(),
            });
            continue;
        }

        // A `--- old` / `+++ new` / `@@ …` sequence with no `diff --git` above
        // it is the only file boundary a plain `diff -u` provides. Without this,
        // a piped multi-file diff produced one anonymous unit spanning every
        // file, and a finding in it could not be anchored to a path at all.
        // See `prefilter::starts_file_header_pair`.
        if crate::diff::starts_file_header_pair(&lines, i)
            && (files.is_empty()
                || hunk.is_some()
                || files.last().is_some_and(|f| !f.hunks.is_empty()))
        {
            flush(&mut files, &mut hunk);
            files.push(RawFile {
                path: String::new(),
                header: Vec::new(),
                hunks: Vec::new(),
            });
        }

        if line.starts_with("@@") {
            flush(&mut files, &mut hunk);
            // A hunk with no preceding file header still deserves reviewing;
            // attribute it to an unnamed file rather than dropping it.
            if files.is_empty() {
                files.push(RawFile {
                    path: String::new(),
                    header: Vec::new(),
                    hunks: Vec::new(),
                });
            }
            let (_, new_start) = parse_hunk_header(line);
            hunk = Some(RawHunk {
                new_start,
                new_end: new_start,
                lines: vec![line.to_string()],
            });
            continue;
        }

        let Some(file) = files.last_mut() else {
            continue;
        };

        if hunk.is_none() {
            if is_file_header_line(line) {
                // `+++ b/path` is authoritative — it survives paths with spaces.
                if let Some(rest) = line.strip_prefix("+++ ") {
                    let p = rest.trim();
                    if p != "/dev/null" {
                        file.path = p.strip_prefix("b/").unwrap_or(p).to_string();
                    }
                }
                file.header.push(line.to_string());
            }
            continue;
        }

        let Some(h) = hunk.as_mut() else { continue };
        // Context and added lines occupy the post-image; removed lines do not.
        if matches!(line.chars().next(), Some('+') | Some(' ') | None) {
            h.new_end = h.new_end.saturating_add(1);
        }
        h.lines.push(line.to_string());
    }

    flush(&mut files, &mut hunk);

    // `new_end` counted lines; turn it into an inclusive last line.
    for file in &mut files {
        for h in &mut file.hunks {
            h.new_end = h.new_end.saturating_sub(1).max(h.new_start);
        }
    }

    files
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two edits far apart in one file: separate regions, separate units.
    const TWO_REGIONS: &str = "\
diff --git a/src/a.rs b/src/a.rs
--- a/src/a.rs
+++ b/src/a.rs
@@ -10,2 +10,2 @@ fn near_the_top() {
-let a = 1;
+let a = 2;
@@ -400,2 +400,2 @@ fn far_below() {
-let z = 1;
+let z = 2;
";

    #[test]
    fn adjacent_hunks_become_one_unit() {
        let diff = "\
diff --git a/src/a.rs b/src/a.rs
+++ b/src/a.rs
@@ -10,2 +10,2 @@
-let a = 1;
+let a = 2;
@@ -18,2 +18,2 @@
-let b = 1;
+let b = 2;
";
        let units = build_units(diff, 1000);
        assert_eq!(units.len(), 1, "hunks 8 lines apart are one region");
        assert_eq!(units[0].hunk_count, 2);
        assert!(units[0].text.contains("+let a = 2;"));
        assert!(units[0].text.contains("+let b = 2;"));
    }

    #[test]
    fn distant_hunks_become_separate_units() {
        let units = build_units(TWO_REGIONS, 1000);
        assert_eq!(units.len(), 2, "390 lines apart is not one region");
        assert_eq!(units[0].new_start, 10);
        assert_eq!(units[1].new_start, 400);
        assert_ne!(units[0].id, units[1].id);
    }

    #[test]
    fn every_unit_carries_its_file_header() {
        // Without it the model cannot attribute a finding to a path, and
        // anchoring drops the finding entirely.
        for unit in build_units(TWO_REGIONS, 1000) {
            assert!(
                unit.text.contains("diff --git a/src/a.rs"),
                "unit is anonymous:\n{}",
                unit.text
            );
            assert!(unit.text.contains("+++ b/src/a.rs"));
        }
    }

    /// The point of the whole module.
    #[test]
    fn editing_one_region_does_not_change_the_other_regions_id() {
        let before = build_units(TWO_REGIONS, 1000);
        let after = build_units(&TWO_REGIONS.replace("+let a = 2;", "+let a = 3;"), 1000);

        assert_eq!(before.len(), 2);
        assert_ne!(
            before[0].id, after[0].id,
            "the edited region must re-review"
        );
        assert_eq!(
            before[1].id, after[1].id,
            "the untouched region must stay cached — this is what makes a \
             re-review after a force-push nearly free"
        );
    }

    #[test]
    fn a_units_id_ignores_the_rest_of_the_diff() {
        let alone = build_units(TWO_REGIONS, 1000);
        let with_company = build_units(
            &format!(
                "{TWO_REGIONS}\
diff --git a/src/unrelated.rs b/src/unrelated.rs
+++ b/src/unrelated.rs
@@ -1,1 +1,1 @@
-let q = 1;
+let q = 2;
"
            ),
            1000,
        );

        assert_eq!(with_company.len(), 3);
        assert_eq!(alone[0].id, with_company[0].id);
        assert_eq!(alone[1].id, with_company[1].id);
    }

    #[test]
    fn identical_content_in_two_files_gets_distinct_ids() {
        let diff = "\
diff --git a/a.rs b/a.rs
+++ b/a.rs
@@ -1,1 +1,1 @@
+same();
diff --git a/b.rs b/b.rs
+++ b/b.rs
@@ -1,1 +1,1 @@
+same();
";
        let units = build_units(diff, 1000);
        assert_eq!(units.len(), 2);
        assert_ne!(units[0].id, units[1].id, "the path is part of the identity");
    }

    #[test]
    fn a_file_never_fragments_past_the_cap() {
        let mut diff = String::from("diff --git a/big.rs b/big.rs\n+++ b/big.rs\n");
        // 20 single-line edits, each 100 lines from the last.
        for i in 0..20 {
            let line = 1 + i * 100;
            diff.push_str(&format!("@@ -{line},1 +{line},1 @@\n-old {i}\n+new {i}\n"));
        }
        let units = build_units(&diff, 1000);
        assert!(
            units.len() <= MAX_UNITS_PER_FILE,
            "20 scattered edits cost {} model calls",
            units.len()
        );
        // Nothing may be lost to the merging.
        for i in 0..20 {
            let needle = format!("+new {i}");
            assert!(
                units.iter().any(|u| u.text.contains(&needle)),
                "{needle} was dropped"
            );
        }
    }

    #[test]
    fn the_line_budget_is_respected_over_adjacency() {
        let mut diff = String::from("diff --git a/big.rs b/big.rs\n+++ b/big.rs\n");
        for i in 0..4 {
            let line = 1 + i * 5;
            diff.push_str(&format!("@@ -{line},20 +{line},20 @@\n"));
            for j in 0..20 {
                diff.push_str(&format!("+line {i}-{j}\n"));
            }
        }
        // Adjacent by line number, but merging all four would blow the budget.
        let units = build_units(&diff, 50);
        assert!(units.len() > 1, "budget must split an over-long region");
    }

    #[test]
    fn a_header_only_file_produces_no_unit() {
        // A pure rename has nothing to review; a header-only prompt just invites
        // the model to make something up.
        let diff = "\
diff --git a/old.rs b/new.rs
similarity index 100%
rename from old.rs
rename to new.rs
";
        assert!(build_units(diff, 1000).is_empty());
    }

    #[test]
    fn units_span_the_lines_they_cover() {
        let units = build_units(TWO_REGIONS, 1000);
        assert!(units[0].covers("src/a.rs", 10));
        assert!(!units[0].covers("src/a.rs", 400));
        assert!(units[1].covers("src/a.rs", 400));
        assert!(
            !units[0].covers("src/other.rs", 10),
            "coverage is per file, not per line number"
        );
    }

    /// A piped diff has no `diff --git` lines, and every unit still has to name
    /// a file — anchoring drops a finding whose path it cannot resolve, so an
    /// anonymous unit is a unit whose findings are thrown away.
    #[test]
    fn a_plain_diff_yields_one_named_unit_per_file() {
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
        let units = build_units(diff, 1000);
        assert_eq!(units.len(), 2, "two files, two units");

        let files: Vec<&str> = units.iter().map(|u| u.file()).collect();
        assert_eq!(files, ["src/one.rs", "src/two.rs"]);

        // Each unit carries only its own content, and its own header.
        assert!(units[0].text.contains("+let a = 2;"));
        assert!(!units[0].text.contains("+let b = 2;"));
        assert!(units[1].text.contains("+++ b/src/two.rs"));
        assert_ne!(units[0].id, units[1].id);
    }

    #[test]
    fn empty_input_yields_no_units() {
        assert!(build_units("", 1000).is_empty());
        assert!(build_units("\n\n", 1000).is_empty());
    }

    #[test]
    fn hunk_bodies_survive_byte_for_byte() {
        let units = build_units(TWO_REGIONS, 1000);
        // The trailing section of a hunk header names the enclosing function —
        // real context for the model, and easy to lose in a re-serialisation.
        assert!(
            units[0]
                .text
                .contains("@@ -10,2 +10,2 @@ fn near_the_top() {")
        );
        assert!(
            units[1]
                .text
                .contains("@@ -400,2 +400,2 @@ fn far_below() {")
        );
    }
}
