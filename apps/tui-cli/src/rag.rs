//! Context assembly — deciding what the model gets to see besides the diff.
//!
//! Reviewing a bare diff is why LLM reviewers hallucinate: the model cannot see
//! the function a hunk landed in, so it guesses at invariants. Four sources fix
//! that, in priority order:
//!
//!   1. the **enclosing** definition of each changed hunk — what the change is
//!      part of;
//!   2. **callers** of the changed symbols — the blast radius. A signature or
//!      contract change is only reviewable against the code that depends on it,
//!      and this is the edge the old regex index could not produce at all;
//!   3. definitions of symbols the diff **references** but does not contain;
//!   4. the **test file** for the changed file, which states the intended
//!      behaviour more precisely than any amount of surrounding code.
//!
//! Everything is bounded. Context that grows with the repository would defeat
//! the point: the budget is spent on the few facts that bear on this hunk, not
//! on a summary of everything.

use crate::graph::{Def, Graph};
use core_engine::diff::{FileDiff, parse_diff};
use std::collections::HashSet;
use std::path::Path;

/// Enclosing bodies to include before the budget is better spent elsewhere.
const MAX_ENCLOSING: usize = 4;
/// Callers per changed symbol. Past a couple, they stop being evidence and
/// start being a directory listing.
const MAX_CALLERS_PER_SYMBOL: usize = 3;
const MAX_CALLERS_TOTAL: usize = 6;
const MAX_REFERENCED: usize = 6;
/// Lines of any single definition. A 400-line function contributes its opening
/// contract, not its whole body.
const MAX_DEF_LINES: usize = 40;

/// Build the context block for a diff. Returns `None` when nothing useful was
/// found, so the caller can omit the section entirely.
pub fn build_context(
    diff: &str,
    graph: &Graph,
    project_root: &Path,
    max_bytes: usize,
) -> Option<String> {
    let files = parse_diff(diff);
    let mut out = String::new();
    let mut included: HashSet<(String, u32)> = HashSet::new();

    // 1. What each hunk is part of, and which symbols the change touches.
    //
    // Asked about the lines that actually changed, not `hunk.new_start` — a
    // hunk begins three lines of context *above* the edit, which for a change
    // to a function's first line resolves to whatever sits between functions,
    // i.e. nothing. That silently emptied the blast radius for exactly the
    // change it matters most for: an altered signature.
    let mut changed_symbols: Vec<Def> = Vec::new();
    let mut enclosing_count = 0;
    'files: for file in &files {
        for hunk in &file.hunks {
            let touched = hunk.changed_new_lines();
            let lines = if touched.is_empty() {
                // A pure deletion has no post-image line to ask about.
                vec![hunk.new_start]
            } else {
                touched
            };

            let Some(def) = lines.iter().find_map(|l| graph.enclosing(&file.path, *l)) else {
                continue;
            };
            if !changed_symbols.iter().any(|d| d == &def) {
                changed_symbols.push(def.clone());
            }
            if enclosing_count >= MAX_ENCLOSING {
                continue;
            }
            if !included.insert((def.path.clone(), def.start_line)) {
                continue;
            }
            let Some(body) = def.source(project_root, MAX_DEF_LINES) else {
                continue;
            };
            let entry = format!(
                "\n--- Enclosing {} `{}` ({}:{}) ---\n{body}\n",
                def.kind, def.name, def.path, def.start_line
            );
            if out.len() + entry.len() > max_bytes {
                break 'files;
            }
            out.push_str(&entry);
            enclosing_count += 1;
        }
    }

    // 2. Blast radius. Who depends on what just changed.
    let mut caller_count = 0;
    'callers: for symbol in &changed_symbols {
        for caller in graph.callers_of(&symbol.name, MAX_CALLERS_PER_SYMBOL) {
            if caller_count >= MAX_CALLERS_TOTAL {
                break 'callers;
            }
            if !included.insert((caller.path.clone(), caller.start_line)) {
                continue;
            }
            let Some(body) = caller.source(project_root, MAX_DEF_LINES) else {
                continue;
            };
            let entry = format!(
                "\n--- Caller of `{}`: {} `{}` ({}:{}) ---\n{body}\n",
                symbol.name, caller.kind, caller.name, caller.path, caller.start_line
            );
            if out.len() + entry.len() > max_bytes {
                break 'callers;
            }
            out.push_str(&entry);
            caller_count += 1;
        }
    }

    // 3. Symbols the added lines mention but do not define here.
    for (name, near) in referenced_symbols(&files, graph)
        .into_iter()
        .take(MAX_REFERENCED)
    {
        let Some(def) = graph
            .definitions_of(&name, near.as_deref(), 1)
            .into_iter()
            .next()
        else {
            continue;
        };
        if !included.insert((def.path.clone(), def.start_line)) {
            continue;
        }
        let Some(body) = def.source(project_root, MAX_DEF_LINES) else {
            continue;
        };
        let entry = format!(
            "\n--- Definition of `{}` ({}:{}) ---\n{body}\n",
            def.name, def.path, def.start_line
        );
        if out.len() + entry.len() > max_bytes {
            break;
        }
        out.push_str(&entry);
    }

    // 4. The test file, which says what the code is *supposed* to do.
    for file in &files {
        let Some(test) = test_file_for(&file.path, project_root) else {
            continue;
        };
        let Ok(body) = std::fs::read_to_string(project_root.join(&test)) else {
            continue;
        };
        let head: String = body
            .lines()
            .take(MAX_DEF_LINES)
            .collect::<Vec<_>>()
            .join("\n");
        let entry = format!("\n--- Tests for {} ({}) ---\n{head}\n", file.path, test);
        if out.len() + entry.len() > max_bytes {
            break;
        }
        out.push_str(&entry);
        // One is evidence of intent; several is a second diff to read.
        break;
    }

    (!out.trim().is_empty()).then_some(out)
}

/// The test file for `path`, by convention. No graph needed — conventions are
/// how humans find tests, and they are right often enough to be worth 20 lines.
fn test_file_for(path: &str, project_root: &Path) -> Option<String> {
    let (dir, file) = match path.rsplit_once('/') {
        Some((d, f)) => (d.to_string(), f.to_string()),
        None => (String::new(), path.to_string()),
    };
    let (stem, ext) = file.rsplit_once('.')?;
    // Already a test; it is its own intent.
    if stem.ends_with("_test") || stem.ends_with(".test") || stem.ends_with(".spec") {
        return None;
    }

    let joined = |d: &str, f: &str| {
        if d.is_empty() {
            f.to_string()
        } else {
            format!("{d}/{f}")
        }
    };
    let candidates = [
        joined(&dir, &format!("{stem}.test.{ext}")),
        joined(&dir, &format!("{stem}.spec.{ext}")),
        joined(&dir, &format!("{stem}_test.{ext}")),
        joined(&dir, &format!("__tests__/{stem}.test.{ext}")),
        joined(&dir, &format!("test_{stem}.{ext}")),
        format!("tests/{stem}.{ext}"),
    ];
    candidates
        .into_iter()
        .find(|c| project_root.join(c).is_file())
}

/// Names the added lines mention that the graph knows about, paired with the
/// file they were seen in so lookup can prefer a local definition.
fn referenced_symbols(files: &[FileDiff], graph: &Graph) -> Vec<(String, Option<String>)> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();

    for file in files {
        for hunk in &file.hunks {
            for line in hunk.added() {
                for word in line
                    .text
                    .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
                {
                    if word.len() < 3 || !seen.insert(word.to_string()) {
                        continue;
                    }
                    // A name defined in the diff's own file is already visible.
                    if graph.definitions_of(word, None, 1).is_empty() {
                        continue;
                    }
                    out.push((word.to_string(), Some(file.path.clone())));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn project(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("diffmind-rag-{name}-{}", std::process::id()));
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

    /// The whole reason for the graph: a changed contract is only reviewable
    /// against the code that depends on it.
    #[test]
    fn context_includes_callers_of_a_changed_function() {
        let root = project("callers");
        write(
            &root,
            "src/auth.rs",
            "pub fn validate_token(t: &str) -> bool {\n    !t.is_empty()\n}\n",
        );
        write(
            &root,
            "src/api.rs",
            "pub fn login(t: &str) -> bool {\n    validate_token(t)\n}\n",
        );
        let graph = indexed(&root);

        let diff = "\
diff --git a/src/auth.rs b/src/auth.rs
+++ b/src/auth.rs
@@ -1,3 +1,3 @@
-pub fn validate_token(t: &str) -> bool {
+pub fn validate_token(t: &str) -> Result<(), Error> {
";
        let ctx = build_context(diff, &graph, &root, 8000).expect("should build context");
        assert!(ctx.contains("Enclosing function `validate_token`"));
        assert!(
            ctx.contains("Caller of `validate_token`"),
            "the blast radius is the point:\n{ctx}"
        );
        assert!(
            ctx.contains("fn login"),
            "the caller's body should be shown"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The regression that only showed up at real scale: git puts three lines
    /// of context above an edit, so a change to a function's *first* line has a
    /// hunk starting outside the function entirely.
    #[test]
    fn a_hunk_whose_context_starts_above_the_function_still_finds_its_callers() {
        let root = project("hunkstart");
        write(
            &root,
            "src/glob.rs",
            "const A: u32 = 1;\nconst B: u32 = 2;\n\npub fn matches(p: &str) -> bool {\n    !p.is_empty()\n}\n",
        );
        write(
            &root,
            "src/user.rs",
            "pub fn filter_paths(p: &str) -> bool {\n    matches(p)\n}\n",
        );
        let graph = indexed(&root);

        // Hunk starts at line 1 (context), but the change is on line 4.
        let diff = "\
diff --git a/src/glob.rs b/src/glob.rs
+++ b/src/glob.rs
@@ -1,6 +1,6 @@
 const A: u32 = 1;
 const B: u32 = 2;

-pub fn matches(p: &str) -> bool {
+pub fn matches(p: &str, strict: bool) -> bool {
     !p.is_empty()
 }
";
        let ctx = build_context(diff, &graph, &root, 8000).expect("context");
        assert!(
            ctx.contains("Caller of `matches`"),
            "a changed signature must bring its callers:\n{ctx}"
        );
        assert!(ctx.contains("filter_paths"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_test_file_is_included_when_convention_finds_one() {
        let root = project("tests");
        write(
            &root,
            "src/math.ts",
            "export function add(a, b) { return a + b; }\n",
        );
        write(
            &root,
            "src/math.test.ts",
            "test('adds', () => { expect(add(1,2)).toBe(3); });\n",
        );
        let graph = indexed(&root);

        let diff = "\
diff --git a/src/math.ts b/src/math.ts
+++ b/src/math.ts
@@ -1,1 +1,1 @@
+export function add(a, b) { return a - b; }
";
        let ctx = build_context(diff, &graph, &root, 8000).unwrap();
        assert!(ctx.contains("Tests for src/math.ts"), "got:\n{ctx}");
        assert!(ctx.contains("expect(add(1,2))"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_test_file_does_not_look_for_its_own_test_file() {
        let root = project("selftest");
        assert_eq!(test_file_for("src/a.test.ts", &root), None);
        assert_eq!(test_file_for("src/a_test.rs", &root), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn context_is_bounded_by_the_byte_budget() {
        let root = project("budget");
        let big = format!("pub fn huge() {{\n{}}}\n", "    let x = 1;\n".repeat(200));
        write(&root, "src/a.rs", &big);
        let graph = indexed(&root);

        let diff =
            "diff --git a/src/a.rs b/src/a.rs\n+++ b/src/a.rs\n@@ -2,1 +2,1 @@\n+    let x = 2;\n";
        let ctx = build_context(diff, &graph, &root, 200);
        assert!(ctx.as_deref().map(str::len).unwrap_or(0) <= 400);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_unindexed_project_yields_no_context_rather_than_failing() {
        let root = project("bare");
        let graph = Graph::open(&root).unwrap();
        let diff =
            "diff --git a/src/a.rs b/src/a.rs\n+++ b/src/a.rs\n@@ -1,1 +1,1 @@\n+let x = 1;\n";
        assert!(build_context(diff, &graph, &root, 8000).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_symbol_is_never_included_twice() {
        // A function that is both the enclosing definition and a referenced
        // symbol should appear once, not burn the budget twice.
        let root = project("dupe");
        write(
            &root,
            "src/a.rs",
            "pub fn helper() {}\npub fn caller() {\n    helper();\n}\n",
        );
        let graph = indexed(&root);
        let diff = "\
diff --git a/src/a.rs b/src/a.rs
+++ b/src/a.rs
@@ -2,3 +2,3 @@
+pub fn caller() {
+    helper();
+}
";
        let ctx = build_context(diff, &graph, &root, 8000).unwrap();
        assert_eq!(
            ctx.matches("pub fn helper()").count(),
            1,
            "duplicated context wastes the budget:\n{ctx}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
