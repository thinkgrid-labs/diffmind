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
            eprintln!("  !  Could not read .diffmind/rules.toml: {e}");
            return vec![];
        }
    };

    match toml::from_str::<RulesFile>(&content) {
        Ok(f) => {
            if !f.rule.is_empty() {
                eprintln!(
                    "  {:<10} {} custom rule{}",
                    "Rules",
                    f.rule.len(),
                    if f.rule.len() == 1 { "" } else { "s" }
                );
            }
            f.rule
        }
        Err(e) => {
            eprintln!("  !  Could not parse .diffmind/rules.toml: {e}");
            vec![]
        }
    }
}
