//! Linking review units the graph says belong together.
//!
//! The engine groups hunks by file and adjacency, which is everything a diff on
//! its own can tell it. It cannot know that the function changed in `auth.rs`
//! and the block changed in `api.rs` are two halves of one edit.
//!
//! Reviewed apart, the model is asked to judge an interaction while seeing only
//! one side of it as background — and pays for the other side's context twice.
//! Reviewed together it is one call, one context, and the actual question. That
//! is the rare change that is both more accurate *and* cheaper.

use core_engine::ReviewUnit;
use std::collections::HashSet;

use super::Graph;

/// Ceiling on a merged unit, in lines. Two related units are worth reading
/// together; six are a second diff, and the model's attention degrades long
/// before the context window does.
const MAX_MERGED_LINES: usize = 600;
/// Ceiling on how many units may fold into one.
const MAX_MERGED_UNITS: usize = 3;

/// Merge units connected by a call edge, when both ends changed in this diff.
pub fn link_related(units: Vec<ReviewUnit>, graph: &Graph) -> Vec<ReviewUnit> {
    if units.len() < 2 {
        return units;
    }

    // What each unit declares, and what its added lines mention.
    let profiles: Vec<Profile> = units.iter().map(|u| profile(u, graph)).collect();

    let mut merged: Vec<ReviewUnit> = Vec::new();
    let mut consumed = vec![false; units.len()];

    for i in 0..units.len() {
        if consumed[i] {
            continue;
        }
        consumed[i] = true;
        let mut unit = units[i].clone();
        let mut combined = profiles[i].clone();
        let mut count = 1;

        for j in (i + 1)..units.len() {
            if consumed[j] || count >= MAX_MERGED_UNITS {
                continue;
            }
            // Same file is already the engine's job; only cross-file links are
            // new information.
            if units[j].files.iter().any(|f| unit.files.contains(f)) {
                continue;
            }
            if !related(&combined, &profiles[j]) {
                continue;
            }
            if unit.line_count() + units[j].line_count() > MAX_MERGED_LINES {
                continue;
            }

            unit = unit.merged_with(&units[j]);
            combined.absorb(&profiles[j]);
            consumed[j] = true;
            count += 1;
        }

        merged.push(unit);
    }

    merged
}

#[derive(Clone, Default)]
struct Profile {
    /// Symbols declared inside this unit's changed region.
    declares: HashSet<String>,
    /// Names its added lines mention.
    mentions: HashSet<String>,
}

impl Profile {
    fn absorb(&mut self, other: &Profile) {
        self.declares.extend(other.declares.iter().cloned());
        self.mentions.extend(other.mentions.iter().cloned());
    }
}

/// Two units are related when one declares something the other calls. The check
/// runs both ways: whether the caller or the callee is listed first in the diff
/// is an accident of path ordering.
fn related(a: &Profile, b: &Profile) -> bool {
    a.declares.iter().any(|d| b.mentions.contains(d))
        || b.declares.iter().any(|d| a.mentions.contains(d))
}

fn profile(unit: &ReviewUnit, graph: &Graph) -> Profile {
    let mut declares = HashSet::new();
    let mut mentions = HashSet::new();

    // Anything the graph knows is declared over the unit's changed span.
    for line in unit.new_start..=unit.new_end {
        if let Some(def) = graph.enclosing(unit.file(), line) {
            declares.insert(def.name);
        }
    }

    for line in unit.text.lines() {
        let Some(added) = line.strip_prefix('+') else {
            continue;
        };
        if added.starts_with("++") {
            continue;
        }
        for word in added.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$')) {
            if word.len() >= 3 {
                mentions.insert(word.to_string());
            }
        }
    }

    Profile { declares, mentions }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_engine::build_units;
    use std::path::{Path, PathBuf};

    fn project(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("diffmind-link-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("src")).unwrap();
        d
    }

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, body).unwrap();
    }

    fn indexed(root: &Path) -> Graph {
        let mut g = Graph::open(root).unwrap();
        g.index(root, &|_| {}).unwrap();
        g
    }

    /// The change this exists for: a signature and the call site updated
    /// together, in different files.
    const BOTH_SIDES: &str = "\
diff --git a/src/auth.rs b/src/auth.rs
+++ b/src/auth.rs
@@ -1,3 +1,3 @@
-pub fn validate_token(t: &str) -> bool {
+pub fn validate_token(t: &str, strict: bool) -> bool {
     !t.is_empty()
 }
diff --git a/src/api.rs b/src/api.rs
+++ b/src/api.rs
@@ -1,3 +1,3 @@
 pub fn login(t: &str) -> bool {
-    validate_token(t)
+    validate_token(t, true)
 }
";

    fn setup(name: &str) -> (PathBuf, Graph) {
        let root = project(name);
        write(
            &root,
            "src/auth.rs",
            "pub fn validate_token(t: &str, strict: bool) -> bool {\n    !t.is_empty()\n}\n",
        );
        write(
            &root,
            "src/api.rs",
            "pub fn login(t: &str) -> bool {\n    validate_token(t, true)\n}\n",
        );
        let g = indexed(&root);
        (root, g)
    }

    #[test]
    fn a_changed_symbol_and_its_changed_caller_become_one_unit() {
        let (root, graph) = setup("pair");
        let units = build_units(BOTH_SIDES, 1000);
        assert_eq!(units.len(), 2, "the engine sees two files, two units");

        let linked = link_related(units, &graph);
        assert_eq!(linked.len(), 1, "the graph sees one change");

        let unit = &linked[0];
        assert_eq!(unit.files.len(), 2);
        assert!(unit.text.contains("src/auth.rs"));
        assert!(unit.text.contains("src/api.rs"));
        assert!(unit.text.contains("strict: bool"));
        assert!(unit.text.contains("validate_token(t, true)"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn unrelated_files_are_left_alone() {
        let root = project("unrelated");
        write(&root, "src/a.rs", "pub fn alpha() {}\n");
        write(&root, "src/b.rs", "pub fn beta() {}\n");
        let graph = indexed(&root);

        let diff = "\
diff --git a/src/a.rs b/src/a.rs
+++ b/src/a.rs
@@ -1,1 +1,1 @@
+pub fn alpha() { let x = 1; }
diff --git a/src/b.rs b/src/b.rs
+++ b/src/b.rs
@@ -1,1 +1,1 @@
+pub fn beta() { let y = 2; }
";
        let units = build_units(diff, 1000);
        assert_eq!(
            link_related(units, &graph).len(),
            2,
            "no call edge, no merge"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn merging_never_loses_a_hunk() {
        let (root, graph) = setup("lossless");
        let before = build_units(BOTH_SIDES, 1000);
        let before_hunks: usize = before.iter().map(|u| u.hunk_count).sum();

        let after = link_related(before, &graph);
        let after_hunks: usize = after.iter().map(|u| u.hunk_count).sum();

        assert_eq!(before_hunks, after_hunks, "a merge must not drop a hunk");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_merged_unit_gets_its_own_identity() {
        // The cache key is derived from the unit; a merged unit reviewed as one
        // call must not collide with either half reviewed alone.
        let (root, graph) = setup("identity");
        let units = build_units(BOTH_SIDES, 1000);
        let (a, b) = (units[0].id.clone(), units[1].id.clone());

        let merged = link_related(units, &graph);
        assert_ne!(merged[0].id, a);
        assert_ne!(merged[0].id, b);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_oversized_pair_is_left_apart() {
        let root = project("oversize");
        let big = "    let x = 1;\n".repeat(400);
        write(&root, "src/a.rs", &format!("pub fn target() {{\n{big}}}\n"));
        write(
            &root,
            "src/b.rs",
            &format!("pub fn user() {{\n    target();\n{big}}}\n"),
        );
        let graph = indexed(&root);

        let diff = format!(
            "diff --git a/src/a.rs b/src/a.rs\n+++ b/src/a.rs\n@@ -1,401 +1,401 @@\n+pub fn target() {{\n{}\
             diff --git a/src/b.rs b/src/b.rs\n+++ b/src/b.rs\n@@ -1,402 +1,402 @@\n+pub fn user() {{\n+    target();\n{}",
            "+    let x = 1;\n".repeat(400),
            "+    let x = 1;\n".repeat(400)
        );
        let units = build_units(&diff, 2000);
        let linked = link_related(units, &graph);
        assert!(
            linked.len() >= 2,
            "merging two huge units would blow the model's attention, not just its window"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_empty_or_single_unit_diff_is_untouched() {
        let root = project("trivial");
        let graph = indexed(&root);
        assert!(link_related(vec![], &graph).is_empty());

        let diff =
            "diff --git a/src/a.rs b/src/a.rs\n+++ b/src/a.rs\n@@ -1,1 +1,1 @@\n+let x = 1;\n";
        assert_eq!(link_related(build_units(diff, 1000), &graph).len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }
}
