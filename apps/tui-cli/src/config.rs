//! `.diffmind/config.toml` — project defaults for the CLI.
//!
//! Without this every flag has to be retyped on every invocation and a team
//! has no way to standardise on a model, a severity gate, or a base branch.
//! Precedence is the conventional one: explicit CLI flag > config file >
//! built-in default.

use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    #[serde(default)]
    pub review: ReviewConfig,
    #[serde(default)]
    pub backend: BackendConfig,
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(deny_unknown_fields)]
pub struct ReviewConfig {
    pub branch: Option<String>,
    pub model: Option<String>,
    pub device: Option<String>,
    pub min_severity: Option<String>,
    pub min_confidence: Option<f32>,
    pub max_tokens: Option<u32>,
    pub format: Option<String>,
    pub triage: Option<String>,
    pub cache: Option<bool>,
    pub temperature: Option<f64>,
    pub seed: Option<u64>,
    /// Fail the run (exit 1) only at or above this severity. Defaults to
    /// `min_severity` so the reported set and the gate agree.
    pub fail_on: Option<String>,
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(deny_unknown_fields)]
pub struct BackendConfig {
    /// `local`, `ollama`, or `openai-compatible`.
    pub kind: Option<String>,
    pub url: Option<String>,
    pub model: Option<String>,
    /// Name of the environment variable holding the API key. The key itself is
    /// deliberately not accepted here — config files get committed.
    pub api_key_env: Option<String>,
    pub context_tokens: Option<usize>,
    pub timeout_secs: Option<u64>,
}

impl FileConfig {
    /// Load `<project_root>/.diffmind/config.toml`.
    ///
    /// A malformed config is reported and ignored rather than fatal: a typo in
    /// an optional file should not block a review. `deny_unknown_fields` means
    /// a misspelled key is surfaced instead of silently doing nothing.
    pub fn load(project_root: &Path) -> Self {
        let path = project_root.join(".diffmind").join("config.toml");
        if !path.exists() {
            return FileConfig::default();
        }

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("  !  could not read .diffmind/config.toml: {e}");
                return FileConfig::default();
            }
        };

        match toml::from_str(&content) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("  !  could not parse .diffmind/config.toml: {e}");
                FileConfig::default()
            }
        }
    }
}

/// Pick the first value that is set: CLI flag, then config file, then default.
pub fn resolve<T: Clone>(cli: Option<T>, file: Option<T>, default: T) -> T {
    cli.or(file).unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, body: &str) {
        std::fs::create_dir_all(dir.join(".diffmind")).unwrap();
        std::fs::write(dir.join(".diffmind").join("config.toml"), body).unwrap();
    }

    fn tmpdir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("diffmind-cfg-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn missing_file_yields_defaults() {
        let d = tmpdir("missing");
        let c = FileConfig::load(&d);
        assert!(c.review.branch.is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn parses_both_sections() {
        let d = tmpdir("full");
        write(
            &d,
            r#"
[review]
branch = "develop"
model = "3b"
min_severity = "medium"
fail_on = "high"
cache = true

[backend]
kind = "ollama"
url = "http://localhost:11434"
model = "qwen2.5-coder:14b"
"#,
        );
        let c = FileConfig::load(&d);
        assert_eq!(c.review.branch.as_deref(), Some("develop"));
        assert_eq!(c.review.model.as_deref(), Some("3b"));
        assert_eq!(c.review.fail_on.as_deref(), Some("high"));
        assert_eq!(c.review.cache, Some(true));
        assert_eq!(c.backend.kind.as_deref(), Some("ollama"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_misspelled_key_is_reported_not_silently_ignored() {
        let d = tmpdir("typo");
        write(&d, "[review]\nmin_severty = \"high\"\n");
        // deny_unknown_fields makes this a parse error, so we fall back to
        // defaults *and* print a warning, rather than pretending it applied.
        let c = FileConfig::load(&d);
        assert!(c.review.min_severity.is_none());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn resolve_prefers_cli_then_file_then_default() {
        assert_eq!(resolve(Some(1), Some(2), 3), 1);
        assert_eq!(resolve(None, Some(2), 3), 2);
        assert_eq!(resolve(None::<i32>, None, 3), 3);
    }
}
