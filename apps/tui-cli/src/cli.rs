use clap::{Parser, Subcommand, ValueEnum};

#[derive(ValueEnum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputFormat {
    /// Human-readable text (default)
    #[default]
    Text,
    /// JSON object — machine-readable, suitable for CI tooling
    Json,
    /// SARIF 2.1.0 — upload to GitHub Code Scanning for inline PR annotations
    Sarif,
    /// Markdown report, e.g. for a PR comment or a job summary
    Markdown,
}

impl OutputFormat {
    pub fn parse_str(s: &str) -> Option<OutputFormat> {
        match s.trim().to_lowercase().as_str() {
            "text" => Some(OutputFormat::Text),
            "json" => Some(OutputFormat::Json),
            "sarif" => Some(OutputFormat::Sarif),
            "markdown" | "md" => Some(OutputFormat::Markdown),
            _ => None,
        }
    }

    /// Formats that write a machine-readable document to stdout, where any
    /// decorative output would corrupt the result.
    pub fn is_machine_readable(&self) -> bool {
        !matches!(self, OutputFormat::Text)
    }
}

/// Every option defaults to `None` rather than a clap default, so
/// `.diffmind/config.toml` can tell "the user asked for this" apart from
/// "nobody said anything". Effective defaults live in `resolve_settings`.
#[derive(Parser, Debug, Default)]
#[command(name = "diffmind")]
#[command(bin_name = "diffmind")]
#[command(author = "Thinkgrid Labs <dennis@thinkgrid.dev>")]
#[command(version)]
#[command(about = "Local-first AI code review — on-device inference, no cloud required", long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Base branch to diff against [default: the repo's default branch]
    #[arg(short, long)]
    pub branch: Option<String>,

    /// Model size: 0.5b, 1.5b, 3b, 7b, 14b, 32b [default: 1.5b]
    #[arg(short, long)]
    pub model: Option<String>,

    /// Review only the most recent commit (HEAD~1..HEAD).
    #[arg(short, long)]
    pub last: bool,

    /// Review staged changes only (`git diff --cached`).
    #[arg(long)]
    pub staged: bool,

    /// Read diff from stdin
    #[arg(long)]
    pub stdin: bool,

    /// Review an explicit revision range, e.g. `v1.2.0..HEAD` or `main...HEAD`.
    /// A bare `a..b` positional argument is recognised as one too.
    #[arg(long, value_name = "RANGE")]
    pub range: Option<String>,

    /// Launch interactive TUI
    #[arg(short, long)]
    pub tui: bool,

    /// User story / acceptance criteria to validate the diff against.
    /// Accepts a file path (e.g. ticket.md) or inline text.
    #[arg(long, value_name = "FILE_OR_TEXT")]
    pub ticket: Option<String>,

    /// Minimum severity to report: low, medium, high [default: low]
    #[arg(long)]
    pub min_severity: Option<String>,

    /// Minimum confidence to report, 0.0-1.0. Deterministic detectors score
    /// 0.9+; unscored model findings default to 0.5.
    #[arg(long)]
    pub min_confidence: Option<f32>,

    /// Severity that causes a non-zero exit [default: same as --min-severity]
    #[arg(long)]
    pub fail_on: Option<String>,

    /// Output format: text, json, sarif, markdown [default: text]
    #[arg(short, long)]
    pub format: Option<String>,

    /// Write output to a file instead of stdout
    #[arg(short, long, value_name = "FILE")]
    pub output: Option<String>,

    /// Maximum output tokens per diff chunk [default: 1024]
    #[arg(long)]
    pub max_tokens: Option<u32>,

    /// Two-pass triage on large diffs: auto, on, off [default: auto]
    #[arg(long)]
    pub triage: Option<String>,

    /// Sampling temperature. 0 is greedy and reproducible [default: 0]
    #[arg(long)]
    pub temperature: Option<f64>,

    /// Sampling seed. Fixed by default so a diff always reviews the same way.
    #[arg(long)]
    pub seed: Option<u64>,

    /// Review documentation (.md, .rst, .txt) too. Skipped by default —
    /// a reviewer prompt finds vulnerabilities in prose.
    #[arg(long)]
    pub include_docs: bool,

    /// Skip the on-disk result cache for this run
    #[arg(long)]
    pub no_cache: bool,

    /// Do not refresh the code graph before reviewing. Faster, but findings are
    /// judged against whatever the graph last saw.
    #[arg(long)]
    pub no_index: bool,

    /// Ignore `.diffmind/baseline.json` for this run
    #[arg(long)]
    pub no_baseline: bool,

    /// Do not use a running `diffmind serve` daemon
    #[arg(long)]
    pub no_daemon: bool,

    /// Print raw model output and token counts to stderr
    #[arg(long)]
    pub debug: bool,

    /// Inference device: auto, cpu, metal [default: auto]
    #[arg(long)]
    pub device: Option<String>,

    // ── Remote backend ────────────────────────────────────────────────────
    /// Inference backend: local, ollama, openai-compatible [default: local]
    #[arg(long, value_name = "KIND")]
    pub backend: Option<String>,

    /// Base URL of the remote backend, e.g. http://localhost:11434
    #[arg(long, value_name = "URL")]
    pub backend_url: Option<String>,

    /// Model name on the remote backend, e.g. qwen2.5-coder:14b
    #[arg(long, value_name = "NAME")]
    pub backend_model: Option<String>,

    /// Environment variable holding the remote API key [default: DIFFMIND_API_KEY]
    #[arg(long, value_name = "VAR")]
    pub backend_api_key_env: Option<String>,

    /// Context window of the remote model, in tokens
    #[arg(long, value_name = "N")]
    pub backend_context: Option<usize>,

    /// Request timeout for the remote backend, in seconds [default: 300]
    #[arg(long, value_name = "SECS")]
    pub backend_timeout: Option<u64>,

    /// Specific files or directories to review, or a revision range (optional)
    pub files: Vec<String>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Download or refresh the local AI model files
    Download {
        /// Model to download: 0.5b, 1.5b, 3b, 7b, 14b, 32b.
        /// Omit to see an interactive list with hardware requirements.
        #[arg(short, long)]
        model: Option<String>,

        /// Force a fresh download even if the model already exists
        #[arg(short, long)]
        force: bool,

        /// Verify an existing model's integrity and exit
        #[arg(long)]
        verify: bool,
    },
    /// Build a symbol index of the local repository for context-aware reviews
    Index {
        /// Discard the existing index and rebuild from scratch
        #[arg(long)]
        rebuild: bool,
    },
    /// Generate a PR title and description from the current branch diff
    Describe {
        #[arg(short, long)]
        branch: Option<String>,
        #[arg(short, long)]
        last: bool,
        #[arg(long)]
        stdin: bool,
        #[arg(long, value_name = "FILE_OR_TEXT")]
        ticket: Option<String>,
        #[arg(short, long)]
        model: Option<String>,
        #[arg(long)]
        device: Option<String>,
    },
    /// Suggest a conventional commit message for staged changes
    Commit {
        #[arg(short, long)]
        model: Option<String>,
        #[arg(long)]
        device: Option<String>,
        /// Run `git commit` automatically with the suggested message
        #[arg(long)]
        apply: bool,
    },
    /// Record current findings as accepted, so only new issues fail the gate
    Baseline {
        #[command(subcommand)]
        action: BaselineAction,
    },
    /// Install git hooks that run diffmind before push or commit
    InstallHooks {
        /// Which hook to install
        #[arg(long, default_value = "pre-push")]
        hook: String,
        /// Severity that blocks the operation
        #[arg(long, default_value = "high")]
        min_severity: String,
        /// Overwrite an existing hook
        #[arg(long)]
        force: bool,
    },
    /// Keep the model resident so subsequent reviews skip the load cost
    Serve {
        /// Unload the model and exit after this many idle seconds
        #[arg(long, default_value_t = 600)]
        idle_timeout: u64,
        /// TCP port on 127.0.0.1. 0 picks a free one.
        #[arg(long, default_value_t = 0)]
        port: u16,
        #[arg(short, long)]
        model: Option<String>,
        #[arg(long)]
        device: Option<String>,
        /// Stop a running daemon instead of starting one
        #[arg(long)]
        stop: bool,
        /// Report whether a daemon is running
        #[arg(long)]
        status: bool,
    },
    /// Manage the prose rule sets in `.diffmind/rules/`
    Rules {
        #[command(subcommand)]
        action: RulesAction,
    },
    /// Findings, cost and accept/wrong ratio over recorded runs
    Stats {
        /// Delete recorded runs. Verdict history is kept.
        #[arg(long)]
        clear: bool,
    },
    /// Inspect or clear the review cache
    Cache {
        #[command(subcommand)]
        action: CacheAction,
    },
}

#[derive(Subcommand, Debug)]
pub enum BaselineAction {
    /// Review the working tree and record every finding as accepted
    Create {
        #[arg(short, long)]
        branch: Option<String>,
        #[arg(short, long)]
        model: Option<String>,
    },
    /// Show what the baseline currently accepts
    Show,
    /// Delete the baseline
    Clear,
}

#[derive(Subcommand, Debug)]
pub enum RulesAction {
    /// Write a starter `.diffmind/rules/default.md`
    Init,
    /// List the rule sets that would be loaded, and what they govern
    List,
}

#[derive(Subcommand, Debug)]
pub enum CacheAction {
    /// Delete every cached review result
    Clear,
    /// Report the cache location and size
    Show,
}

pub fn parse() -> Cli {
    Cli::parse()
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn unset_flags_stay_none_so_config_can_supply_them() {
        let cli = Cli::try_parse_from(["diffmind"]).unwrap();
        assert!(
            cli.branch.is_none(),
            "a clap default would mask the config file"
        );
        assert!(cli.model.is_none());
        assert!(cli.min_severity.is_none());
        assert!(cli.format.is_none());
    }

    #[test]
    fn explicit_flags_are_captured() {
        let cli =
            Cli::try_parse_from(["diffmind", "--branch", "develop", "--min-severity", "high"])
                .unwrap();
        assert_eq!(cli.branch.as_deref(), Some("develop"));
        assert_eq!(cli.min_severity.as_deref(), Some("high"));
    }

    #[test]
    fn format_parsing_accepts_documented_names() {
        assert_eq!(OutputFormat::parse_str("SARIF"), Some(OutputFormat::Sarif));
        assert_eq!(OutputFormat::parse_str("md"), Some(OutputFormat::Markdown));
        assert_eq!(OutputFormat::parse_str("text"), Some(OutputFormat::Text));
        assert_eq!(OutputFormat::parse_str("xml"), None);
    }

    #[test]
    fn only_text_is_decorated() {
        assert!(!OutputFormat::Text.is_machine_readable());
        assert!(OutputFormat::Json.is_machine_readable());
        assert!(OutputFormat::Sarif.is_machine_readable());
    }

    #[test]
    fn positional_files_still_parse() {
        let cli = Cli::try_parse_from(["diffmind", "src/a.rs", "src/b.rs"]).unwrap();
        assert_eq!(cli.files, vec!["src/a.rs", "src/b.rs"]);
    }

    #[test]
    fn a_range_can_be_given_explicitly_or_positionally() {
        let cli = Cli::try_parse_from(["diffmind", "--range", "v1.0..HEAD"]).unwrap();
        assert_eq!(cli.range.as_deref(), Some("v1.0..HEAD"));
        assert!(cli.files.is_empty());

        // The positional form arrives as a file and is classified later, once
        // git can be asked whether it resolves.
        let cli = Cli::try_parse_from(["diffmind", "v1.0..HEAD", "src/"]).unwrap();
        assert!(cli.range.is_none());
        assert_eq!(cli.files, vec!["v1.0..HEAD", "src/"]);
    }

    #[test]
    fn subcommands_parse() {
        assert!(matches!(
            Cli::try_parse_from(["diffmind", "serve", "--stop"])
                .unwrap()
                .command,
            Some(Commands::Serve { stop: true, .. })
        ));
        assert!(matches!(
            Cli::try_parse_from(["diffmind", "baseline", "create"])
                .unwrap()
                .command,
            Some(Commands::Baseline {
                action: BaselineAction::Create { .. }
            })
        ));
    }
}
