use clap::{Parser, Subcommand, ValueEnum};

#[derive(ValueEnum, Clone, Debug, Default)]
pub enum OutputFormat {
    /// Human-readable text (default)
    #[default]
    Text,
    /// JSON array — machine-readable, suitable for CI tooling
    Json,
}

#[derive(Parser, Debug)]
#[command(name = "diffmind")]
#[command(bin_name = "diffmind")]
#[command(author = "Thinkgrid Labs <dennis@thinkgrid.dev>")]
#[command(version = "0.5.0")]
#[command(about = "Local-first AI code review — on-device inference, no cloud required", long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Target branch to diff against
    #[arg(short, long, default_value = "main")]
    pub branch: String,

    /// Model size to use (1.5b, 3b)
    #[arg(short, long, default_value = "1.5b")]
    pub model: String,

    /// Initial analysis from stdin
    #[arg(long)]
    pub stdin: bool,

    /// Launch interactive TUI
    #[arg(short, long)]
    pub tui: bool,

    /// User story / acceptance criteria to validate the diff against.
    /// Accepts a file path (e.g. ticket.md) or inline text.
    /// The model will check whether the diff satisfies the requirements
    /// in addition to its standard security/quality review.
    #[arg(long, value_name = "FILE_OR_TEXT")]
    pub ticket: Option<String>,

    /// Minimum severity to report — also the threshold for non-zero exit code (high, medium, low)
    #[arg(long, default_value = "low")]
    pub min_severity: String,

    /// Output format: text (default) or json
    #[arg(short, long, default_value = "text")]
    pub format: OutputFormat,

    /// Maximum output tokens per diff chunk
    #[arg(long, default_value_t = 1024)]
    pub max_tokens: u32,

    /// Specific files or directories to review (optional)
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
    },
    /// Build a symbol index of the local repository for context-aware reviews
    Index,
}

pub fn parse() -> Cli {
    Cli::parse()
}
