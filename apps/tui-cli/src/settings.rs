//! Merging CLI flags, `.diffmind/config.toml`, and built-in defaults into one
//! resolved view, so the rest of the CLI never has to ask where a value came from.

use crate::cli::{Cli, OutputFormat};
use crate::config::{FileConfig, resolve};
use anyhow::{Result, anyhow};
use core_engine::{DevicePreference, RemoteProtocol, Severity, TriageMode};
use std::time::Duration;

pub const DEFAULT_MODEL: &str = "1.5b";
pub const DEFAULT_MAX_TOKENS: u32 = 1024;
pub const DEFAULT_BACKEND_TIMEOUT_SECS: u64 = 300;
pub const DEFAULT_API_KEY_ENV: &str = "DIFFMIND_API_KEY";

#[derive(Debug, Clone)]
pub enum BackendChoice {
    /// The bundled GGUF running in-process.
    Local {
        model: String,
        device: DevicePreference,
        device_arg: String,
    },
    Remote {
        protocol: RemoteProtocol,
        url: String,
        model: String,
        api_key: Option<String>,
        context_tokens: Option<usize>,
        timeout: Duration,
    },
}

impl BackendChoice {
    /// Identity used for daemon matching and cache keys.
    pub fn identity(&self) -> (String, String) {
        match self {
            BackendChoice::Local {
                model, device_arg, ..
            } => (model.clone(), device_arg.clone()),
            BackendChoice::Remote {
                protocol,
                url,
                model,
                ..
            } => (format!("{}:{}", protocol.as_str(), model), url.clone()),
        }
    }

    pub fn is_local(&self) -> bool {
        matches!(self, BackendChoice::Local { .. })
    }
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub branch: Option<String>,
    pub backend: BackendChoice,
    pub min_severity: Severity,
    /// Severity at or above which the process exits non-zero.
    pub fail_on: Severity,
    pub min_confidence: f32,
    pub max_tokens: u32,
    pub format: OutputFormat,
    pub output_path: Option<String>,
    pub triage: TriageMode,
    pub temperature: f64,
    pub seed: u64,
    pub use_cache: bool,
    pub use_baseline: bool,
    pub use_daemon: bool,
    pub debug: bool,
}

pub fn resolve_settings(cli: &Cli, file: &FileConfig) -> Result<Settings> {
    let r = &file.review;

    let min_severity = parse_severity(resolve(
        cli.min_severity.clone(),
        r.min_severity.clone(),
        "low".to_string(),
    ))?;

    // The gate defaults to the reporting threshold so "what I see" and "what
    // fails the build" agree unless the user deliberately separates them.
    let fail_on = match cli.fail_on.clone().or(r.fail_on.clone()) {
        Some(s) => parse_severity(s)?,
        None => min_severity,
    };

    if fail_on < min_severity {
        return Err(anyhow!(
            "--fail-on {} is below --min-severity {}: findings that would fail the build \
             are being filtered out before the gate sees them.",
            fail_on.as_str(),
            min_severity.as_str()
        ));
    }

    let format_name = resolve(cli.format.clone(), r.format.clone(), "text".to_string());
    let format = OutputFormat::parse_str(&format_name).ok_or_else(|| {
        anyhow!("unknown --format '{format_name}'. Use text, json, sarif, or markdown.")
    })?;

    let triage = TriageMode::parse(&resolve(
        cli.triage.clone(),
        r.triage.clone(),
        "auto".to_string(),
    ));

    let min_confidence = resolve(cli.min_confidence, r.min_confidence, 0.0);
    if !(0.0..=1.0).contains(&min_confidence) {
        return Err(anyhow!("--min-confidence must be between 0.0 and 1.0"));
    }

    let temperature = resolve(cli.temperature, r.temperature, 0.0);
    if temperature < 0.0 {
        return Err(anyhow!("--temperature cannot be negative"));
    }

    Ok(Settings {
        branch: cli.branch.clone().or(r.branch.clone()),
        backend: resolve_backend(cli, file)?,
        min_severity,
        fail_on,
        min_confidence,
        max_tokens: resolve(cli.max_tokens, r.max_tokens, DEFAULT_MAX_TOKENS).max(64),
        format,
        output_path: cli.output.clone(),
        triage,
        temperature,
        seed: resolve(cli.seed, r.seed, core_engine::DEFAULT_SEED),
        // A cache is only sound at temperature 0; above it, two runs are
        // *supposed* to differ, and replaying one would be a lie.
        use_cache: !cli.no_cache && resolve(None, r.cache, true) && temperature == 0.0,
        use_baseline: !cli.no_baseline,
        use_daemon: !cli.no_daemon,
        debug: cli.debug,
    })
}

fn resolve_backend(cli: &Cli, file: &FileConfig) -> Result<BackendChoice> {
    let kind = resolve(
        cli.backend.clone(),
        file.backend.kind.clone(),
        "local".to_string(),
    );

    if kind.eq_ignore_ascii_case("local") {
        let model = resolve(
            cli.model.clone(),
            file.review.model.clone(),
            DEFAULT_MODEL.to_string(),
        );
        if crate::download::find_model(&model).is_none() {
            return Err(anyhow!(
                "unknown model '{model}'. Valid options: {}",
                crate::download::model_ids().join(", ")
            ));
        }
        let device_arg = resolve(
            cli.device.clone(),
            file.review.device.clone(),
            "auto".to_string(),
        );
        return Ok(BackendChoice::Local {
            model,
            device: DevicePreference::parse(&device_arg),
            device_arg,
        });
    }

    let protocol = RemoteProtocol::parse(&kind).ok_or_else(|| {
        anyhow!("unknown --backend '{kind}'. Use local, ollama, or openai-compatible.")
    })?;

    let url = cli
        .backend_url
        .clone()
        .or(file.backend.url.clone())
        .unwrap_or_else(|| default_url(protocol));

    let model = cli
        .backend_model
        .clone()
        .or(file.backend.model.clone())
        .ok_or_else(|| {
            anyhow!(
                "--backend {kind} needs a model name. Pass --backend-model <name>, \
                 or set backend.model in .diffmind/config.toml."
            )
        })?;

    let key_env = resolve(
        cli.backend_api_key_env.clone(),
        file.backend.api_key_env.clone(),
        DEFAULT_API_KEY_ENV.to_string(),
    );
    // Read the key from the environment only — a config file holding a secret
    // ends up committed.
    let api_key = std::env::var(&key_env)
        .ok()
        .filter(|k| !k.trim().is_empty());

    Ok(BackendChoice::Remote {
        protocol,
        url,
        model,
        api_key,
        context_tokens: cli.backend_context.or(file.backend.context_tokens),
        timeout: Duration::from_secs(resolve(
            cli.backend_timeout,
            file.backend.timeout_secs,
            DEFAULT_BACKEND_TIMEOUT_SECS,
        )),
    })
}

fn default_url(protocol: RemoteProtocol) -> String {
    match protocol {
        RemoteProtocol::Ollama => "http://localhost:11434".into(),
        RemoteProtocol::OpenAiCompatible => "http://localhost:8000/v1".into(),
    }
}

fn parse_severity(s: String) -> Result<Severity> {
    match s.trim().to_lowercase().as_str() {
        "high" => Ok(Severity::High),
        "medium" | "med" => Ok(Severity::Medium),
        "low" => Ok(Severity::Low),
        other => Err(anyhow!(
            "unknown severity '{other}'. Use low, medium, or high."
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn cli(args: &[&str]) -> Cli {
        let mut full = vec!["diffmind"];
        full.extend_from_slice(args);
        Cli::try_parse_from(full).unwrap()
    }

    #[test]
    fn defaults_are_applied_when_nothing_is_set() {
        let s = resolve_settings(&cli(&[]), &FileConfig::default()).unwrap();
        assert_eq!(s.min_severity, Severity::Low);
        assert_eq!(s.fail_on, Severity::Low);
        assert_eq!(s.max_tokens, DEFAULT_MAX_TOKENS);
        assert_eq!(s.format, OutputFormat::Text);
        assert_eq!(s.seed, core_engine::DEFAULT_SEED);
        assert_eq!(s.temperature, 0.0, "greedy by default so reviews reproduce");
        assert!(s.backend.is_local());
    }

    #[test]
    fn cli_beats_config_which_beats_default() {
        let mut file = FileConfig::default();
        file.review.min_severity = Some("medium".into());
        file.review.model = Some("3b".into());

        let s = resolve_settings(&cli(&["--min-severity", "high"]), &file).unwrap();
        assert_eq!(s.min_severity, Severity::High, "CLI wins");
        match &s.backend {
            BackendChoice::Local { model, .. } => assert_eq!(model, "3b", "config supplies model"),
            _ => panic!("expected local"),
        }
    }

    #[test]
    fn fail_on_defaults_to_min_severity() {
        let s =
            resolve_settings(&cli(&["--min-severity", "medium"]), &FileConfig::default()).unwrap();
        assert_eq!(s.fail_on, Severity::Medium);
    }

    #[test]
    fn a_gate_below_the_report_threshold_is_rejected() {
        // Otherwise the gate silently never fires: the findings that would
        // trip it are filtered out before it is consulted.
        let err = resolve_settings(
            &cli(&["--min-severity", "high", "--fail-on", "low"]),
            &FileConfig::default(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("below --min-severity"));
    }

    #[test]
    fn reporting_everything_while_gating_on_high_is_allowed() {
        let s = resolve_settings(
            &cli(&["--min-severity", "low", "--fail-on", "high"]),
            &FileConfig::default(),
        )
        .unwrap();
        assert_eq!(s.min_severity, Severity::Low);
        assert_eq!(s.fail_on, Severity::High);
    }

    #[test]
    fn caching_is_disabled_above_temperature_zero() {
        let s = resolve_settings(&cli(&["--temperature", "0.7"]), &FileConfig::default()).unwrap();
        assert!(
            !s.use_cache,
            "replaying a cached answer would defeat the point of sampling"
        );
    }

    #[test]
    fn an_unknown_model_is_rejected_with_the_valid_list() {
        let err = resolve_settings(&cli(&["--model", "99b"]), &FileConfig::default()).unwrap_err();
        assert!(err.to_string().contains("1.5b"));
    }

    #[test]
    fn remote_backend_requires_a_model_name() {
        let err =
            resolve_settings(&cli(&["--backend", "ollama"]), &FileConfig::default()).unwrap_err();
        assert!(err.to_string().contains("--backend-model"));
    }

    #[test]
    fn remote_backend_defaults_its_url_per_protocol() {
        let s = resolve_settings(
            &cli(&[
                "--backend",
                "ollama",
                "--backend-model",
                "qwen2.5-coder:14b",
            ]),
            &FileConfig::default(),
        )
        .unwrap();
        match &s.backend {
            BackendChoice::Remote { url, protocol, .. } => {
                assert_eq!(url, "http://localhost:11434");
                assert_eq!(*protocol, RemoteProtocol::Ollama);
            }
            _ => panic!("expected remote"),
        }
    }

    #[test]
    fn backend_identity_distinguishes_configurations() {
        let local = resolve_settings(&cli(&["--model", "3b"]), &FileConfig::default()).unwrap();
        let remote = resolve_settings(
            &cli(&["--backend", "ollama", "--backend-model", "x"]),
            &FileConfig::default(),
        )
        .unwrap();
        assert_ne!(
            local.backend.identity(),
            remote.backend.identity(),
            "a daemon or cache entry for one must never be reused for the other"
        );
    }

    #[test]
    fn invalid_format_and_confidence_are_rejected() {
        assert!(resolve_settings(&cli(&["--format", "xml"]), &FileConfig::default()).is_err());
        assert!(
            resolve_settings(&cli(&["--min-confidence", "1.5"]), &FileConfig::default()).is_err()
        );
    }
}
