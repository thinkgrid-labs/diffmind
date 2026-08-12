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
use std::collections::{HashMap, HashSet};
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
    //
    // Candidate names are gathered from the text first and resolved in one
    // batch. Asking the graph about each word as it was encountered meant a
    // query per distinct word in the diff — thousands on a large branch, times
    // every review unit — and then the six survivors were looked up a second
    // time to get the definition that had just been discarded.
    let candidates = referenced_names(&files);
    let known = graph.definitions_of_names(
        &candidates
            .iter()
            .map(|(n, _)| n.clone())
            .collect::<Vec<_>>(),
    );
    let mut by_name: HashMap<&str, Vec<&Def>> = HashMap::new();
    for def in &known {
        by_name.entry(def.name.as_str()).or_default().push(def);
    }

    let mut referenced_included = 0;
    for (name, seen_in) in &candidates {
        if referenced_included >= MAX_REFERENCED {
            break;
        }
        let Some(defs) = by_name.get(name.as_str()) else {
            continue;
        };
        // A definition in the file the name was seen in is far likelier to be
        // the referent than a same-named symbol elsewhere.
        let Some(def) = defs
            .iter()
            .find(|d| &d.path == seen_in)
            .or_else(|| defs.first())
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
        referenced_included += 1;
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

/// Candidate symbol names from the added lines, in order of first appearance,
/// each paired with the file it was seen in so lookup can prefer a local
/// definition.
///
/// Purely textual — whether the graph knows a name is decided in one batch by
/// the caller. This used to consult the graph per word, which is what made
/// context assembly scale with the size of the diff rather than with the number
/// of symbols actually reported.
fn referenced_names(files: &[FileDiff]) -> Vec<(String, String)> {
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
                    out.push((word.to_string(), file.path.clone()));
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

    /// When two files declare the same name, the one in the file being reviewed
    /// is the referent. This preference used to live in `Graph::definitions_of`;
    /// it moved here when name resolution became a single batched query, and it
    /// is the kind of thing that goes missing in such a move.
    #[test]
    fn a_local_definition_wins_over_a_same_named_one_elsewhere() {
        let root = project("ambiguous-ref");
        write(
            &root,
            "src/a.rs",
            "pub fn shared_helper() {\n    let from_a = 1;\n}\npub fn uses_it() {}\n",
        );
        write(
            &root,
            "src/z.rs",
            "pub fn shared_helper() {\n    let from_z = 2;\n}\n",
        );
        let graph = indexed(&root);

        // The diff touches z.rs and mentions the ambiguous name.
        let diff = "\
diff --git a/src/z.rs b/src/z.rs
+++ b/src/z.rs
@@ -1,3 +1,3 @@
+pub fn caller() { shared_helper(); }
";
        let ctx = build_context(diff, &graph, &root, 8000).expect("context");
        assert!(
            ctx.contains("let from_z"),
            "z.rs's own definition should be the one shown:\n{ctx}"
        );
        assert!(
            !ctx.contains("let from_a"),
            "a same-named definition in another file is not the referent:\n{ctx}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Names the graph has never heard of must not consume the referenced-symbol
    /// budget. Resolution is batched now, so a diff full of ordinary words has to
    /// leave room for the few that are real symbols.
    #[test]
    fn unknown_words_do_not_crowd_out_real_symbols() {
        let root = project("crowded-words");
        write(
            &root,
            "src/lib.rs",
            "pub fn real_target() {\n    let m = 1;\n}\n",
        );
        let graph = indexed(&root);

        // Plenty of noise words before the one symbol that matters.
        let noise: String = (0..40)
            .map(|i| format!("+    let unknown_word_{i} = {i};\n"))
            .collect();
        let diff = format!(
            "diff --git a/src/use.rs b/src/use.rs\n+++ b/src/use.rs\n@@ -1,50 +1,50 @@\n{noise}+    real_target();\n"
        );

        let ctx = build_context(&diff, &graph, &root, 8000).expect("context");
        assert!(
            ctx.contains("Definition of `real_target`"),
            "the one real symbol must survive a diff full of unknown words:\n{ctx}"
        );
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
