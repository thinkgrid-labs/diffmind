//! Context assembly for the review prompt.
//!
//! Reviewing a bare diff is why LLM reviewers hallucinate: the model cannot see
//! the function a hunk landed in, so it guesses at invariants. Two sources fix
//! that, in priority order:
//!
//!   1. the *enclosing* definition of each changed hunk — the single most
//!      useful thing to show, and previously not provided at all;
//!   2. definitions of symbols the diff references but does not contain.

use crate::indexer::{COMMON_KEYWORDS, SymbolIndex};
use core_engine::diff::{FileDiff, parse_diff};
use regex::Regex;
use std::collections::HashSet;

/// How many enclosing bodies to include before the budget is better spent on
/// referenced symbols.
const MAX_ENCLOSING: usize = 6;
/// Cap on referenced-symbol definitions.
const MAX_REFERENCED: usize = 8;

/// Build the context block for a diff. Returns `None` when nothing useful was
/// found, so the caller can omit the section entirely.
pub fn build_context(diff: &str, index: &SymbolIndex, max_bytes: usize) -> Option<String> {
    let files = parse_diff(diff);
    let mut out = String::new();
    let mut included: HashSet<(String, usize)> = HashSet::new();

    // 1. Enclosing definitions.
    let mut enclosing_count = 0;
    'outer: for file in &files {
        for hunk in &file.hunks {
            if enclosing_count >= MAX_ENCLOSING {
                break 'outer;
            }
            let Some(def) = index.enclosing(&file.path, hunk.new_start as usize) else {
                continue;
            };
            if !included.insert((def.file.clone(), def.line)) {
                continue;
            }
            let entry = format!(
                "\n--- Enclosing {} `{}` ({}:{}) ---\n{}\n",
                def.r#type, def.name, def.file, def.line, def.snippet
            );
            if out.len() + entry.len() > max_bytes {
                break 'outer;
            }
            out.push_str(&entry);
            enclosing_count += 1;
        }
    }

    // 2. Symbols referenced by added lines but defined elsewhere.
    for (name, near_file) in referenced_symbols(&files, index)
        .into_iter()
        .take(MAX_REFERENCED)
    {
        let Some(def) = index.lookup(&name, near_file.as_deref()) else {
            continue;
        };
        if !included.insert((def.file.clone(), def.line)) {
            continue;
        }
        let entry = format!(
            "\n--- Definition of `{}` ({}:{}) ---\n{}\n",
            def.name, def.file, def.line, def.snippet
        );
        if out.len() + entry.len() > max_bytes {
            break;
        }
        out.push_str(&entry);
    }

    (!out.trim().is_empty()).then_some(out)
}

/// Identifiers appearing on added lines that the index knows about, paired with
/// the file they were seen in so lookup can prefer a local definition.
fn referenced_symbols(files: &[FileDiff], index: &SymbolIndex) -> Vec<(String, Option<String>)> {
    let re = Regex::new(r"[a-zA-Z_$][a-zA-Z0-9_$]*").expect("static pattern");
    let mut seen = HashSet::new();
    let mut out = Vec::new();

    for file in files {
        for hunk in &file.hunks {
            for line in hunk.added() {
                for m in re.find_iter(&line.text) {
                    let word = m.as_str();
                    if COMMON_KEYWORDS.contains(word) || word.len() < 3 {
                        continue;
                    }
                    if !index.symbols.contains_key(word) {
                        continue;
                    }
                    if seen.insert(word.to_string()) {
                        out.push((word.to_string(), Some(file.path.clone())));
                    }
                }
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indexer::{Indexer, SymbolIndex};
    use std::path::PathBuf;

    /// `name` must be unique per test: these run in parallel, and a shared
    /// temp directory means one test deletes another's fixture mid-run.
    fn index_from(name: &str, files: &[(&str, &str)]) -> (SymbolIndex, PathBuf) {
        let dir = std::env::temp_dir().join(format!("diffmind-rag-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (name, body) in files {
            let path = dir.join(name);
            if let Some(p) = path.parent() {
                std::fs::create_dir_all(p).unwrap();
            }
            std::fs::write(path, body).unwrap();
        }
        let mut indexer = Indexer::new(dir.clone()).unwrap();
        (indexer.build_index(None).unwrap(), dir)
    }

    #[test]
    fn includes_the_enclosing_function_of_a_hunk() {
        let (index, dir) = index_from(
            "enclosing",
            &[(
                "a.rs",
                "pub fn transfer(amount: u64) {\n    check();\n    apply();\n    log();\n}\n",
            )],
        );

        let diff = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -2,2 +2,2 @@\n-    check();\n+    // check();\n";
        let ctx = build_context(diff, &index, 4000).expect("should find the enclosing fn");
        assert!(ctx.contains("Enclosing"), "{ctx}");
        assert!(
            ctx.contains("transfer"),
            "the model needs the whole function: {ctx}"
        );
        assert!(
            ctx.contains("apply()"),
            "including the lines the diff did not touch"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn includes_definitions_of_referenced_symbols() {
        let (index, dir) = index_from(
            "referenced",
            &[
                (
                    "lib.ts",
                    "export function validateToken(t: string) {\n  return t.length > 0;\n}\n",
                ),
                ("app.ts", "export const unused = 1;\n"),
            ],
        );

        let diff = "diff --git a/app.ts b/app.ts\n--- a/app.ts\n+++ b/app.ts\n@@ -1 +1,2 @@\n+  if (validateToken(x)) { go(); }\n";
        let ctx = build_context(diff, &index, 4000).expect("should resolve validateToken");
        assert!(ctx.contains("Definition of `validateToken`"), "{ctx}");
        assert!(ctx.contains("t.length > 0"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn respects_the_byte_budget() {
        let (index, dir) = index_from(
            "budget",
            &[(
                "a.rs",
                &format!("pub fn big() {{\n{}\n}}\n", "    let x = 1;\n".repeat(50)),
            )],
        );
        let diff =
            "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -2,1 +2,1 @@\n+    let x = 2;\n";

        let ctx = build_context(diff, &index, 50);
        assert!(
            ctx.as_ref().is_none_or(|c| c.len() <= 200),
            "context must not blow past its budget: {ctx:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn returns_none_when_nothing_is_known() {
        let (index, dir) = index_from("nothing", &[("a.rs", "// nothing exported\n")]);
        let diff = "diff --git a/z.rs b/z.rs\n--- a/z.rs\n+++ b/z.rs\n@@ -1 +1,2 @@\n+let q = 1;\n";
        assert!(build_context(diff, &index, 4000).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn common_keywords_are_not_treated_as_symbols() {
        let (index, dir) = index_from("keywords", &[("a.ts", "export const type = 1;\n")]);
        let diff =
            "diff --git a/b.ts b/b.ts\n--- a/b.ts\n+++ b/b.ts\n@@ -1 +1,2 @@\n+const type = 2;\n";
        // `type` is a keyword; pulling its "definition" in would be noise.
        let ctx = build_context(diff, &index, 4000);
        assert!(ctx.is_none() || !ctx.unwrap().contains("Definition of `type`"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
