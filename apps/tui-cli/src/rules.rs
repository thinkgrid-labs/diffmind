use core_engine::CustomRule;
use serde::Deserialize;
use std::path::Path;

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

    #[test]
    fn a_malformed_file_yields_no_rules_rather_than_crashing() {
        let dir = write_rules("[[rule]\npattern = ");
        assert!(load_custom_rules(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
