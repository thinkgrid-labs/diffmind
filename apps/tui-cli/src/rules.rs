use core_engine::{CustomRule, Rulebook, rulebook};
use serde::Deserialize;
use std::path::Path;

/// Load prose rule sets from `<project_root>/.diffmind/rules/**/*.md`.
///
/// A file that fails to parse is reported and skipped rather than aborting the
/// review — but it *is* reported, because a rulebook silently doing nothing is
/// the failure mode that makes people stop trusting the feature.
pub fn load_rulebooks(project_root: &Path) -> Vec<Rulebook> {
    let dir = project_root.join(".diffmind").join("rules");
    if !dir.is_dir() {
        return Vec::new();
    }

    let mut paths: Vec<std::path::PathBuf> = walkdir::WalkDir::new(&dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.path().to_path_buf())
        .filter(|p| p.extension().is_some_and(|x| x == "md"))
        .collect();
    // Deterministic order: the rendered prompt must not change because the
    // filesystem returned entries differently, or the cache key moves with it.
    paths.sort();

    let mut books = Vec::new();
    for path in paths {
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let Ok(text) = std::fs::read_to_string(&path) else {
            eprintln!("  !  could not read {}", path.display());
            continue;
        };
        match rulebook::parse(&stem, &text) {
            Ok(book) => books.push(book),
            Err(e) => eprintln!("  !  {}: {e}", path.display()),
        }
    }

    warn_on_duplicate_rulebook_ids(&books);
    books
}

fn warn_on_duplicate_rulebook_ids(books: &[Rulebook]) {
    let mut seen = std::collections::HashMap::new();
    for b in books {
        *seen.entry(b.id.as_str()).or_insert(0usize) += 1;
    }
    for (id, count) in seen {
        if count > 1 {
            eprintln!(
                "  !  {count} rule sets share the id '{id}' — a finding attributed to it \
                 is ambiguous, and suppressing one suppresses them all. Set an explicit \
                 `id:` in the front matter."
            );
        }
    }
}

/// Scaffold `.diffmind/rules/default.md`. Refuses to overwrite.
pub fn init_rulebooks(project_root: &Path) -> anyhow::Result<std::path::PathBuf> {
    let dir = project_root.join(".diffmind").join("rules");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("default.md");
    if path.exists() {
        anyhow::bail!(
            "{} already exists — edit it, or add another .md file beside it",
            path.display()
        );
    }
    std::fs::write(&path, core_engine::DEFAULT_RULEBOOK)?;
    Ok(path)
}

/// Mirror of the TOML file structure — `[[rule]]` becomes a `Vec<CustomRule>`.
#[derive(Deserialize, Default)]
struct RulesFile {
    #[serde(default, rename = "rule")]
    rule: Vec<CustomRule>,
}

/// Load custom rules from `<project_root>/.diffmind/rules.toml`.
/// Returns an empty Vec (and prints a warning) if the file is missing or invalid.
pub fn load_custom_rules(project_root: &Path) -> Vec<CustomRule> {
    let path = project_root.join(".diffmind").join("rules.toml");
    if !path.exists() {
        return vec![];
    }

    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("  !  could not read .diffmind/rules.toml: {e}");
            return vec![];
        }
    };

    let rules = match toml::from_str::<RulesFile>(&content) {
        Ok(f) => f.rule,
        Err(e) => {
            eprintln!("  !  could not parse .diffmind/rules.toml: {e}");
            return vec![];
        }
    };

    warn_on_duplicate_ids(&rules);
    rules
}

/// Two rules sharing an ID make suppression ambiguous: ignoring one silently
/// ignores the other.
fn warn_on_duplicate_ids(rules: &[CustomRule]) {
    let mut seen = std::collections::HashMap::new();
    for rule in rules {
        let id = rule.effective_id();
        *seen.entry(id).or_insert(0usize) += 1;
    }
    for (id, count) in seen {
        if count > 1 {
            eprintln!(
                "  !  {count} rules share the id '{id}' — suppressing one will suppress them all. \
                 Set an explicit `id` on each."
            );
        }
    }
}

/// Validate `.diffmind/rules/` and `.diffmind/rules.toml`. Returns the exit code.
///
/// Exists because every other check in this module warns on stderr during a
/// review, where it scrolls past behind the findings. A rule set that stopped
/// working deserves to fail CI, not to be mentioned in passing.
pub fn check(project_root: &Path) -> anyhow::Result<i32> {
    let mut problems = 0usize;

    let dir = project_root.join(".diffmind").join("rules");
    // Name files, not ids: when two rule sets collide the ids are identical, so
    // an id-based message tells the reader nothing about which file to edit.
    let mut parsed: Vec<(std::path::PathBuf, Rulebook)> = Vec::new();

    if dir.is_dir() {
        let mut entries: Vec<std::path::PathBuf> = walkdir::WalkDir::new(&dir)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .map(|e| e.path().to_path_buf())
            .filter(|p| p.extension().is_some_and(|x| x == "md"))
            .collect();
        entries.sort();

        for path in entries {
            let path = path.as_path();
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let text = match std::fs::read_to_string(path) {
                Ok(t) => t,
                Err(e) => {
                    println!("  ✗  {}: {e}", path.display());
                    problems += 1;
                    continue;
                }
            };
            match core_engine::rulebook::parse(&stem, &text) {
                Ok(book) => {
                    // A glob nobody can satisfy is the silent-no-op this whole
                    // command exists to surface.
                    for glob in &book.scope {
                        if glob.trim().is_empty() {
                            println!("  ✗  {}: empty glob in `scope`", path.display());
                            problems += 1;
                        }
                    }
                    println!("  ✓  {:<28} {}", book.id, path.display());
                    parsed.push((path.to_path_buf(), book));
                }
                Err(e) => {
                    println!("  ✗  {}: {e}", path.display());
                    problems += 1;
                }
            }
        }
    }

    // Two rule sets saying the same thing cost tokens twice and give the model
    // two chances to report the same finding.
    for i in 0..parsed.len() {
        for j in (i + 1)..parsed.len() {
            if parsed[i].1.body == parsed[j].1.body {
                println!(
                    "  ✗  {} and {} have identical bodies — delete one.",
                    parsed[i].0.display(),
                    parsed[j].0.display()
                );
                problems += 1;
            }
        }
    }

    // Counted from what was parsed above rather than by calling
    // `load_rulebooks`, which would re-parse every file and print its own copy
    // of the warnings this command has already reported.
    let mut ids: std::collections::HashMap<&str, Vec<&std::path::Path>> = Default::default();
    for (path, book) in &parsed {
        ids.entry(book.id.as_str()).or_default().push(path);
    }
    let mut collisions: Vec<_> = ids.iter().filter(|(_, v)| v.len() > 1).collect();
    collisions.sort_by_key(|(id, _)| *id);
    for (id, paths) in collisions {
        let names: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
        println!(
            "  ✗  {} rule sets share the id '{id}' ({}) — set an explicit `id`.",
            paths.len(),
            names.join(", ")
        );
        problems += 1;
    }

    // Pattern rules: an invalid regex is skipped at review time with a warning
    // nobody reads. Here it is a failure.
    for rule in load_custom_rules(project_root) {
        if let Err(e) = regex::Regex::new(&rule.pattern) {
            println!("  ✗  rule '{}': invalid regex — {e}", rule.effective_id());
            problems += 1;
        }
    }

    if problems == 0 {
        println!("\n  {} rule set(s), no problems.", parsed.len());
        Ok(0)
    } else {
        println!("\n  {problems} problem(s).");
        Ok(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_rules(body: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "diffmind-rules-{}-{}",
            std::process::id(),
            body.len()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".diffmind")).unwrap();
        std::fs::write(dir.join(".diffmind/rules.toml"), body).unwrap();
        dir
    }

    #[test]
    fn missing_file_is_not_an_error() {
        let dir = std::env::temp_dir().join(format!("diffmind-norules-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(load_custom_rules(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn loads_rules_with_ids_and_fixes() {
        let dir = write_rules(
            r#"
[[rule]]
id = "no-console"
pattern = "console\\.log"
message = "Remove debug logging"
fix = "Use the structured logger"
severity = "medium"
category = "quality"
files = ["*.ts"]
"#,
        );
        let rules = load_custom_rules(&dir);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].effective_id(), "no-console");
        assert_eq!(rules[0].fix.as_deref(), Some("Use the structured logger"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rules_without_an_id_still_load() {
        let dir = write_rules(
            "[[rule]]\npattern = \"TODO\"\nmessage = \"Resolve TODO before merging\"\n",
        );
        let rules = load_custom_rules(&dir);
        assert_eq!(rules.len(), 1);
        assert!(rules[0].effective_id().starts_with("custom."));
        // Documented defaults still apply.
        assert_eq!(rules[0].severity, "medium");
        assert_eq!(rules[0].category, "quality");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn rulebook_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("diffmind-rb-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".diffmind/rules")).unwrap();
        dir
    }

    #[test]
    fn no_rules_directory_is_not_an_error() {
        let dir = std::env::temp_dir().join(format!("diffmind-norb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(load_rulebooks(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rule_sets_load_in_a_deterministic_order() {
        // The rendered prompt is part of the cache key. If the filesystem's
        // ordering leaked through, the key would move between identical runs.
        let dir = rulebook_dir("order");
        for name in ["zebra", "alpha", "middle"] {
            std::fs::write(
                dir.join(format!(".diffmind/rules/{name}.md")),
                format!("- Rule from {name}.\n"),
            )
            .unwrap();
        }
        let ids: Vec<String> = load_rulebooks(&dir).into_iter().map(|b| b.id).collect();
        assert_eq!(ids, ["alpha", "middle", "zebra"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nested_rule_sets_are_found() {
        let dir = rulebook_dir("nested");
        std::fs::create_dir_all(dir.join(".diffmind/rules/backend")).unwrap();
        std::fs::write(dir.join(".diffmind/rules/backend/db.md"), "- No raw SQL.\n").unwrap();
        let books = load_rulebooks(&dir);
        assert_eq!(books.len(), 1);
        assert_eq!(books[0].id, "db");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_broken_rule_set_is_skipped_without_losing_the_others() {
        let dir = rulebook_dir("broken");
        std::fs::write(dir.join(".diffmind/rules/good.md"), "- A real rule.\n").unwrap();
        std::fs::write(
            dir.join(".diffmind/rules/bad.md"),
            "---\nnonsense-key: 1\n---\n\n- x\n",
        )
        .unwrap();
        let books = load_rulebooks(&dir);
        assert_eq!(books.len(), 1, "one bad file must not disable the rest");
        assert_eq!(books[0].id, "good");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_markdown_files_are_ignored() {
        let dir = rulebook_dir("ext");
        std::fs::write(dir.join(".diffmind/rules/notes.txt"), "not a rule set\n").unwrap();
        std::fs::write(dir.join(".diffmind/rules/real.md"), "- A rule.\n").unwrap();
        let books = load_rulebooks(&dir);
        assert_eq!(books.len(), 1);
        assert_eq!(books[0].id, "real");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn init_scaffolds_once_and_refuses_to_overwrite() {
        let dir = std::env::temp_dir().join(format!("diffmind-rbinit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let path = init_rulebooks(&dir).expect("first init should succeed");
        assert!(path.exists());
        // The scaffolded file must be loadable, or `rules init` hands the user
        // something broken.
        assert_eq!(load_rulebooks(&dir).len(), 1);

        std::fs::write(&path, "- Edited by the user.\n").unwrap();
        assert!(init_rulebooks(&dir).is_err(), "must not clobber an edit");
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("Edited by")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_malformed_file_yields_no_rules_rather_than_crashing() {
        let dir = write_rules("[[rule]\npattern = ");
        assert!(load_custom_rules(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
