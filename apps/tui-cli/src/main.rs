use anyhow::{Context, Result};
use core_engine::{
    AnalysisStats, Baseline, CommitSuggestion, CustomRule, PrDescription, PrefilterOptions,
    PrefilterReport, ReviewAnalyzer, ReviewBackend, ReviewCache, ReviewFinding, ReviewSummary,
    Rulebook, Severity,
};
use crossterm::style::Stylize;
use indicatif::{ProgressBar, ProgressStyle};
use std::{
    collections::HashSet,
    io::{self, IsTerminal, Read},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

mod cli;
mod config;
mod daemon;
mod download;
mod git;
mod graph;
mod hooks;
mod output;
mod rag;
mod rules;
mod runs;
mod settings;
mod tui;

use crate::cli::OutputFormat;
use crate::graph::Graph;
use crate::settings::{BackendChoice, Settings};

/// Exit codes. Distinguishing "found problems" from "could not run" is what
/// lets a CI job tell a real review failure apart from a crashed binary — both
/// used to be exit 1.
const EXIT_FINDINGS: i32 = 1;
const EXIT_ERROR: i32 = 2;

fn main() {
    let code = match run() {
        Ok(exit) => exit,
        Err(e) => {
            eprintln!("\n  {}  {e:#}\n", "error".red().bold());
            EXIT_ERROR
        }
    };
    std::process::exit(code);
}

fn home_dir() -> Result<PathBuf> {
    std::env::var("DIFFMIND_HOME")
        .or_else(|_| std::env::var("HOME"))
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .map_err(|_| anyhow::anyhow!("could not determine home directory; set DIFFMIND_HOME"))
}

fn run() -> Result<i32> {
    let mut args = cli::parse();

    let home = home_dir()?;
    let model_dir = home.join(".diffmind").join("models");
    // Anchor project files at the repository root so running from a
    // subdirectory still finds `.diffmind/`.
    let project_root = git::repo_root().unwrap_or(std::env::current_dir()?);
    let file_config = config::FileConfig::load(&project_root);

    if let Some(command) = args.command.take() {
        return run_command(
            command,
            &args,
            &home,
            &model_dir,
            &project_root,
            &file_config,
        );
    }

    let settings = settings::resolve_settings(&args, &file_config)?;
    let diff = capture_diff(&args, &settings)?;

    if diff.trim().is_empty() {
        // Not an error: a clean branch is a passing review.
        if !settings.format.is_machine_readable() {
            eprintln!("\n  No changes detected. Nothing to analyze.\n");
        } else {
            emit(
                &settings,
                &ReviewSummary::default(),
                &AnalysisStats::default(),
                "none",
            )?;
        }
        return Ok(0);
    }

    // Before anything costs a token, and before the TUI/CLI split so both
    // surfaces review exactly the same set of hunks.
    let (diff, prefilter) = apply_prefilter(&diff, &settings, &project_root)?;

    // Refresh the graph before either surface reads it.
    if settings.auto_index {
        sync_graph(&project_root, settings.format.is_machine_readable());
    }

    if prefilter.dropped_everything() {
        // Distinct from "no changes": there *were* changes, and none of them
        // were worth a reviewer's attention. Reporting zero findings without
        // saying so would read as "reviewed, found nothing".
        if !settings.format.is_machine_readable() {
            eprintln!(
                "\n  Nothing to review — all {} hunk{} filtered ({}).\n",
                prefilter.hunks_total,
                if prefilter.hunks_total == 1 { "" } else { "s" },
                prefilter.reason_summary()
            );
        } else {
            emit(
                &settings,
                &ReviewSummary::default(),
                &AnalysisStats::default(),
                "none",
            )?;
        }
        return Ok(0);
    }

    if args.tui {
        tui::run(
            diff,
            model_dir,
            project_root,
            settings,
            resolve_ticket(args.ticket.as_deref()),
            prefilter,
        )?;
        return Ok(0);
    }

    review(
        &args,
        &settings,
        &diff,
        &prefilter,
        &home,
        &model_dir,
        &project_root,
    )
}

fn run_command(
    command: cli::Commands,
    args: &cli::Cli,
    home: &Path,
    model_dir: &Path,
    project_root: &Path,
    file_config: &config::FileConfig,
) -> Result<i32> {
    match command {
        cli::Commands::Download {
            model,
            force,
            verify,
        } => {
            if verify {
                let id = model.unwrap_or_else(|| settings::DEFAULT_MODEL.to_string());
                return Ok(if download::verify_model_files(&id, model_dir)? {
                    0
                } else {
                    EXIT_ERROR
                });
            }
            download::ensure_model_files(model.as_deref(), model_dir, force)?;
            Ok(0)
        }

        cli::Commands::Index { rebuild } => {
            if rebuild {
                let _ = std::fs::remove_file(Graph::path(project_root));
            }
            let mut graph = Graph::open(project_root)?;
            let spinner = make_spinner("Indexing...", false);
            let stats = graph.index(project_root, &|n| {
                spinner.set_message(format!("Indexing... {n} files"))
            })?;
            spinner.finish_and_clear();
            runs::ensure_gitignore(project_root);

            let (files, defs, refs) = graph.counts();
            println!(
                "Indexed {files} file(s): {defs} definitions, {refs} references \
                 ({} reparsed, {} unchanged, {} removed)",
                stats.files_indexed, stats.files_unchanged, stats.files_removed
            );
            Ok(0)
        }

        cli::Commands::Describe {
            branch,
            last,
            stdin,
            ticket,
            model,
            device,
        } => {
            let mut merged = clone_args_for_subcommand(args, model, device);
            merged.branch = branch;
            merged.last = last;
            merged.stdin = stdin;
            let settings = settings::resolve_settings(&merged, file_config)?;

            let diff = capture_diff(&merged, &settings)?;
            if diff.trim().is_empty() {
                println!("No changes detected. Nothing to describe.");
                return Ok(0);
            }
            let ticket_text = resolve_ticket(ticket.as_deref());
            run_describe(&diff, model_dir, ticket_text.as_deref(), &settings)?;
            Ok(0)
        }

        cli::Commands::Commit {
            model,
            device,
            apply,
        } => {
            let merged = clone_args_for_subcommand(args, model, device);
            let settings = settings::resolve_settings(&merged, file_config)?;
            let diff = git::get_staged_diff(&[])?;
            run_commit(&diff, model_dir, &settings, apply)?;
            Ok(0)
        }

        cli::Commands::Baseline { action } => {
            run_baseline(action, args, file_config, model_dir, project_root, home)
        }

        cli::Commands::InstallHooks {
            hook,
            min_severity,
            force,
        } => {
            let git_dir = git::git_dir()
                .context("not inside a git repository — run this from your project")?;
            hooks::install(&git_dir, &hook, &min_severity, force)?;
            Ok(0)
        }

        cli::Commands::Serve {
            idle_timeout,
            port,
            model,
            device,
            stop,
            status,
        } => {
            let merged = clone_args_for_subcommand(args, model, device);
            let settings = settings::resolve_settings(&merged, file_config)?;
            run_serve(
                idle_timeout,
                port,
                stop,
                status,
                &settings,
                home,
                model_dir,
                project_root,
            )
        }

        cli::Commands::Stats { clear } => {
            if clear {
                let removed = runs::clear_runs(project_root)?;
                println!("Cleared {removed} recorded run(s). Verdict history kept.");
                return Ok(0);
            }
            print_stats(project_root);
            Ok(0)
        }

        cli::Commands::Rules { action } => match action {
            cli::RulesAction::Init => {
                let path = rules::init_rulebooks(project_root)?;
                println!("  ✓  Wrote {}", path.display());
                println!("     Edit it, then commit it — review standards belong in the repo.");
                Ok(0)
            }
            cli::RulesAction::List => {
                let books = rules::load_rulebooks(project_root);
                if books.is_empty() {
                    println!("No rule sets. Run `diffmind rules init` to scaffold one.");
                    return Ok(0);
                }
                for b in &books {
                    let scope = if b.scope.is_empty() {
                        "whole repository".to_string()
                    } else {
                        b.scope.join(", ")
                    };
                    let severity = b.severity.map(|s| s.as_str()).unwrap_or("unset");
                    println!("  {:<24} {severity:<7} {scope}", b.id);
                }
                Ok(0)
            }
        },

        cli::Commands::Cache { action } => {
            let base = project_root.join(".diffmind");
            match action {
                cli::CacheAction::Clear => {
                    if let Some(cache) = ReviewCache::open(&base) {
                        cache.clear()?;
                        println!("Cache cleared.");
                    }
                    Ok(0)
                }
                cli::CacheAction::Show => {
                    let dir = base.join("cache");
                    let (count, bytes) = cache_stats(&dir);
                    println!("  Location  {}", dir.display());
                    println!("  Entries   {count}");
                    println!("  Size      {:.1} MB", bytes as f64 / 1_048_576.0);
                    Ok(0)
                }
            }
        }
    }
}

/// Subcommands accept their own `--model`/`--device`; fold them into a copy of
/// the top-level args so settings resolution stays in one place.
fn clone_args_for_subcommand(
    args: &cli::Cli,
    model: Option<String>,
    device: Option<String>,
) -> cli::Cli {
    cli::Cli {
        command: None,
        model: model.or_else(|| args.model.clone()),
        device: device.or_else(|| args.device.clone()),
        backend: args.backend.clone(),
        backend_url: args.backend_url.clone(),
        backend_model: args.backend_model.clone(),
        backend_api_key_env: args.backend_api_key_env.clone(),
        backend_context: args.backend_context,
        backend_timeout: args.backend_timeout,
        max_tokens: args.max_tokens,
        temperature: args.temperature,
        seed: args.seed,
        debug: args.debug,
        no_cache: args.no_cache,
        no_daemon: args.no_daemon,
        ..Default::default()
    }
}

fn cache_stats(dir: &Path) -> (usize, u64) {
    let Ok(read) = std::fs::read_dir(dir) else {
        return (0, 0);
    };
    read.flatten()
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .fold((0, 0), |(c, b), m| (c + 1, b + m.len()))
}

// ─── Diff capture ────────────────────────────────────────────────────────────

fn capture_diff(args: &cli::Cli, settings: &Settings) -> Result<String> {
    // Each of these names a different set of changes. Silently honouring
    // whichever the code happens to check first would review something the user
    // did not ask for.
    let selectors = [
        (args.stdin, "--stdin"),
        (args.staged, "--staged"),
        (args.last, "--last"),
        (args.range.is_some(), "--range"),
    ];
    let chosen: Vec<&str> = selectors
        .iter()
        .filter(|(on, _)| *on)
        .map(|(_, name)| *name)
        .collect();
    if chosen.len() > 1 {
        anyhow::bail!(
            "{} select different changes — pass only one.",
            chosen.join(" and ")
        );
    }

    if args.stdin {
        let mut buffer = String::new();
        io::stdin()
            .read_to_string(&mut buffer)
            .context("could not read the diff from stdin")?;
        return Ok(buffer);
    }

    if !git::is_repo() {
        anyhow::bail!(
            "not inside a git repository. Run diffmind from your project, or pipe a diff:\n\
             \x20 git diff main...HEAD | diffmind --stdin"
        );
    }

    if args.staged {
        return git::get_staged_diff(&args.files);
    }
    if args.last {
        return git::get_last_commit_diff(&args.files);
    }

    if let Some((range, paths)) = resolve_range(args) {
        return git::get_range_diff(&range, &paths);
    }

    // Nobody said which branch — ask git rather than assuming `main`.
    let branch = settings.branch.clone().unwrap_or_else(git::default_branch);
    git::get_diff(&branch, &args.files)
}

/// Work out whether this run is scoped to an explicit revision range, and which
/// positional arguments are paths rather than the range itself.
///
/// `--range` is authoritative. Failing that, a leading positional that resolves
/// as a range is taken as one, so `diffmind v1.2.0..HEAD` works the way anyone
/// would expect without a flag. The detection is strict — both endpoints must
/// resolve and the string must not name an existing path — so `diffmind ../lib`
/// is still a path.
fn resolve_range(args: &cli::Cli) -> Option<(String, Vec<String>)> {
    if let Some(range) = args.range.clone() {
        return Some((range, args.files.clone()));
    }
    let first = args.files.first()?;
    git::looks_like_range(first).then(|| (first.clone(), args.files[1..].to_vec()))
}

/// Accepts either a file path or inline text.
fn resolve_ticket(input: Option<&str>) -> Option<String> {
    let raw = input?;
    let path = Path::new(raw);
    if path.is_file() {
        match std::fs::read_to_string(path) {
            Ok(content) => Some(content),
            Err(e) => {
                eprintln!("  !  could not read ticket file '{raw}': {e}");
                None
            }
        }
    } else {
        Some(raw.to_string())
    }
}

/// Extracts language names from a git diff by inspecting file extensions in
/// `diff --git` header lines, so the prompt can name the right idioms.
fn detect_languages(diff: &str) -> Vec<String> {
    let mut langs: HashSet<String> = HashSet::new();

    for file in core_engine::parse_diff(diff) {
        let ext = file.path.rsplit('.').next().unwrap_or("");
        let lang = match ext {
            "rs" => Some("Rust"),
            "ts" | "tsx" => Some("TypeScript"),
            "js" | "jsx" | "mjs" | "cjs" => Some("JavaScript"),
            "py" => Some("Python"),
            "go" => Some("Go"),
            "java" => Some("Java"),
            "kt" | "kts" => Some("Kotlin"),
            "swift" => Some("Swift"),
            "rb" => Some("Ruby"),
            "cs" => Some("C#"),
            "cpp" | "cc" | "cxx" | "hpp" => Some("C++"),
            "c" | "h" => Some("C"),
            "php" => Some("PHP"),
            "scala" => Some("Scala"),
            "ex" | "exs" => Some("Elixir"),
            "sql" => Some("SQL"),
            _ => None,
        };
        if let Some(l) = lang {
            langs.insert(l.to_string());
        }
    }

    let mut result: Vec<String> = langs.into_iter().collect();
    result.sort(); // deterministic ordering for stable prompts
    result
}

// ─── Backend construction ────────────────────────────────────────────────────

pub fn build_backend(choice: &BackendChoice, model_dir: &Path) -> Result<Box<dyn ReviewBackend>> {
    match choice {
        BackendChoice::Local { model, device, .. } => {
            let info = download::find_model(model)
                .ok_or_else(|| anyhow::anyhow!("unknown model '{model}'"))?;
            let model_path = model_dir.join(info.gguf_filename);
            let tokenizer_path = model_dir.join("tokenizer.json");

            if !model_path.exists() || !tokenizer_path.exists() {
                anyhow::bail!(
                    "model files not found. Run `diffmind download --model {model}` first."
                );
            }
            // Catch a truncated download here, where the fix is obvious, rather
            // than letting candle fail with an opaque GGUF parse error.
            if download::looks_truncated(model_dir, info.gguf_filename) {
                anyhow::bail!(
                    "{} is incomplete (a previous download was interrupted).\n\
                     Re-download with: diffmind download --model {model} --force",
                    info.gguf_filename
                );
            }

            let backend =
                core_engine::CandleBackend::from_path(&model_path, &tokenizer_path, device.clone())
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok(Box::new(backend))
        }

        BackendChoice::Remote {
            protocol,
            url,
            model,
            api_key,
            context_tokens,
            timeout,
        } => {
            let backend = core_engine::RemoteBackend::new(
                *protocol,
                url,
                model,
                api_key.clone(),
                *context_tokens,
                *timeout,
            )
            .map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok(Box::new(backend))
        }
    }
}

/// Assemble a fully-configured analyzer. Shared by the CLI, the TUI and the daemon.
/// Everything a project contributes to a review beyond the diff itself.
/// Bundled because these three are always loaded together, from the same
/// `.diffmind/` directory, and are meaningless apart.
#[derive(Default, Clone)]
pub struct ProjectRules {
    /// Regex rules from `.diffmind/rules.toml`.
    pub custom: Vec<CustomRule>,
    /// Prose rule sets from `.diffmind/rules/*.md`.
    pub books: Vec<Rulebook>,
    /// Findings already accepted, from `.diffmind/baseline.json`.
    pub baseline: Option<Baseline>,
}

impl ProjectRules {
    pub fn load(project_root: &Path, use_baseline: bool) -> Self {
        ProjectRules {
            custom: rules::load_custom_rules(project_root),
            books: rules::load_rulebooks(project_root),
            baseline: load_baseline(project_root, use_baseline),
        }
    }
}

pub fn build_analyzer(
    backend: Box<dyn ReviewBackend>,
    settings: &Settings,
    project_root: &Path,
    diff: &str,
    ticket: Option<String>,
    project: ProjectRules,
) -> ReviewAnalyzer {
    let mut analyzer = ReviewAnalyzer::new(backend)
        .with_unit_grouper(unit_grouper(project_root))
        .with_languages(detect_languages(diff))
        .with_custom_rules(project.custom)
        .with_rulebooks(project.books)
        .with_baseline(project.baseline)
        .with_triage(settings.triage)
        .with_min_confidence(settings.min_confidence)
        .with_sampling(settings.temperature, settings.seed)
        .with_debug(settings.debug);

    if settings.use_cache {
        analyzer = analyzer.with_cache(ReviewCache::open(&project_root.join(".diffmind")));
    }
    if let Some(req) = ticket {
        analyzer = analyzer.with_requirements(req);
    }
    analyzer
}

/// Report what the recorded runs say about whether this tool is earning its
/// keep. The accept-to-wrong ratio is the headline: every other number here is
/// context for it.
fn print_stats(project_root: &Path) {
    let s = runs::summarize(project_root);

    if s.runs == 0 && s.accepted + s.dismissed + s.wrong == 0 {
        println!("  No runs recorded yet. Review something, then come back.");
        return;
    }

    println!();
    println!("  diffmind  stats");
    println!("  {}", "─".repeat(52));
    println!("  {:<16} {}", "Runs", s.runs);
    if let (Some(first), Some(last)) = (&s.first_run, &s.last_run) {
        println!(
            "  {:<16} {} → {}",
            "Period",
            &first[..10.min(first.len())],
            &last[..10.min(last.len())]
        );
    }
    println!("  {:<16} {}", "Median findings", s.median_findings);
    println!("  {:<16} {:.1}s", "Median time", s.median_seconds);
    println!("  {:<16} {}", "Median tokens", s.median_tokens);
    println!("  {:<16} {:.0}%", "Cache hits", s.cache_hit_rate * 100.0);
    println!();

    println!(
        "  {:<16} {} accepted · {} dismissed · {} wrong",
        "Verdicts", s.accepted, s.dismissed, s.wrong
    );
    match s.accept_to_wrong() {
        Some(ratio) => {
            // The target from the plan. Below it, the tool is costing more
            // attention than it saves and the rules or the model need work.
            let verdict = if ratio >= 2.0 {
                "on target"
            } else {
                "below the 2:1 target"
            };
            println!("  {:<16} {ratio:.1}:1  ({verdict})", "Accept : wrong");
        }
        None if s.accepted + s.dismissed == 0 => {
            println!(
                "  {:<16} no verdicts yet — use the TUI to accept or reject findings",
                "Accept : wrong"
            );
        }
        None => println!("  {:<16} nothing marked wrong yet", "Accept : wrong"),
    }

    if !s.worst_rules.is_empty() {
        println!();
        println!("  Most often wrong");
        for (rule, count) in &s.worst_rules {
            println!("     {count:>3}  {rule}");
        }
    }
    println!();
}

/// Reviewable size limit, applied to the diff *after* filtering. Past this,
/// chunking alone takes longer than a human review.
const MAX_REVIEWABLE_DIFF_KB: usize = 1500;

/// Drop the parts of a diff that are not worth spending inference on.
///
/// Runs before the size check on purpose: a branch whose diff is 90% lockfile
/// used to be refused outright, when what was left was perfectly reviewable.
fn apply_prefilter(
    diff: &str,
    settings: &Settings,
    project_root: &Path,
) -> Result<(String, PrefilterReport)> {
    let paths: Vec<String> = core_engine::parse_diff(diff)
        .into_iter()
        .map(|f| f.path)
        .collect();

    // Two sources the engine cannot consult itself: git attributes, and the
    // file's own header banner. A generated file usually declares itself on
    // line 1, which a hunk deep in the file would never show.
    let mut generated_paths = git::linguist_generated(&paths);
    for path in &paths {
        if generated_paths.contains(path) {
            continue;
        }
        if let Ok(head) = read_head(&project_root.join(path))
            && core_engine::looks_generated(&head)
        {
            generated_paths.insert(path.clone());
        }
    }

    let (filtered, report) = core_engine::prefilter(
        diff,
        &PrefilterOptions {
            generated_paths,
            ignore_globs: settings.ignore_globs.clone(),
        },
    );

    let size_kb = filtered.len() / 1024;
    if size_kb > MAX_REVIEWABLE_DIFF_KB {
        anyhow::bail!(
            "diff is too large to review ({size_kb} KB of reviewable changes, limit \
             {MAX_REVIEWABLE_DIFF_KB} KB). Review specific paths instead, e.g. `diffmind src/`."
        );
    }

    Ok((filtered, report))
}

/// First 4 KB of a file — enough for a generated-file banner, and bounded so a
/// multi-megabyte artifact costs nothing to classify.
fn read_head(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut buf = vec![0u8; 4096];
    let mut file = std::fs::File::open(path)?;
    let n = file.read(&mut buf)?;
    buf.truncate(n);
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Build the per-chunk symbol-context provider the analyzer calls.
///
/// The index is loaded once and captured, so assembling context per chunk costs
/// a symbol lookup rather than a re-read of `symbols.json`. Returning a closure
/// (instead of one context string for the whole diff) is what keeps each
/// chunk's cache key independent of the other files in the diff.
/// Bring the code graph up to date before reviewing.
///
/// Incremental and mtime-keyed: re-checking an unchanged 647-file repository
/// costs about a tenth of a second, which is nothing beside one inference pass.
///
/// Doing it automatically matters more than the cost. A stale graph does not
/// merely miss new code — it reports **wrong line ranges**, and `Def::source`
/// then reads those lines out of the working tree and hands the model unrelated
/// code labelled as the enclosing function. Confidently wrong context is worse
/// than none, so the default is to never let the graph fall behind.
///
/// Never fatal: the graph is an optimisation, and a review must still run
/// without it.
fn sync_graph(project_root: &Path, quiet: bool) {
    let Ok(mut graph) = Graph::open(project_root) else {
        return;
    };
    // Only the first build is slow enough to be worth a spinner.
    let spinner = (graph.is_empty() && !quiet)
        .then(|| make_spinner("Building code graph (first run)...", false));

    let progress = |n: usize| {
        if let Some(s) = &spinner {
            s.set_message(format!("Building code graph... {n} files"));
        }
    };
    if let Err(e) = graph.index(project_root, &progress) {
        eprintln!("  !  code graph not refreshed: {e}");
    }
    if let Some(s) = spinner {
        s.finish_and_clear();
    }
    runs::ensure_gitignore(project_root);
}

/// Merge units the code graph says are two halves of one change. Without a
/// graph this is the identity function, so behaviour is unchanged.
pub fn unit_grouper(project_root: &Path) -> core_engine::analyzer::UnitGrouper {
    let graph = Graph::open(project_root).ok().filter(|g| !g.is_empty());
    Box::new(move |units| match &graph {
        Some(g) => graph::link_related(units, g),
        None => units,
    })
}

pub fn context_builder(project_root: &Path, budget: usize) -> impl Fn(&str) -> String {
    let graph = Graph::open(project_root).ok().filter(|g| !g.is_empty());
    let root = project_root.to_path_buf();
    move |chunk: &str| {
        graph
            .as_ref()
            .and_then(|g| rag::build_context(chunk, g, &root, budget))
            .unwrap_or_default()
    }
}

fn load_baseline(project_root: &Path, enabled: bool) -> Option<Baseline> {
    if !enabled {
        return None;
    }
    let path = project_root.join(".diffmind").join("baseline.json");
    let raw = std::fs::read_to_string(path).ok()?;
    match Baseline::parse(&raw) {
        Ok(b) => Some(b),
        Err(e) => {
            eprintln!("  !  could not parse .diffmind/baseline.json: {e}");
            None
        }
    }
}

// ─── Review ──────────────────────────────────────────────────────────────────

fn make_spinner(msg: &str, quiet: bool) -> ProgressBar {
    // Machine-readable output goes to stdout; a spinner on stderr is fine, but
    // a non-TTY (CI log, pipe) should not collect thousands of tick frames.
    if quiet || !io::stderr().is_terminal() {
        return ProgressBar::hidden();
    }
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ")
            .template("  {spinner:.cyan}  {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_spinner()),
    );
    pb.set_message(msg.to_string());
    pb.enable_steady_tick(Duration::from_millis(80));
    pb
}

fn review(
    args: &cli::Cli,
    settings: &Settings,
    diff: &str,
    prefilter: &PrefilterReport,
    home: &Path,
    model_dir: &Path,
    project_root: &Path,
) -> Result<i32> {
    let ticket = resolve_ticket(args.ticket.as_deref());
    let project = ProjectRules::load(project_root, settings.use_baseline);
    let is_text = settings.format == OutputFormat::Text;

    // We are about to write the cache and a run record. Claim the ignore file
    // first, so neither ever shows up as an untracked change.
    runs::ensure_gitignore(project_root);

    print_header(
        args,
        settings,
        diff,
        ticket.as_deref(),
        project.custom.len(),
        prefilter,
        is_text,
    );

    let spinner = make_spinner("Loading model...", !is_text);

    // Prefer a resident daemon: it has already paid the model-load cost.
    let (model_id, device_id) = settings.backend.identity();
    let daemon_client = if settings.use_daemon && settings.backend.is_local() {
        daemon::Client::connect(home, &model_id, &device_id)
    } else {
        None
    };

    let (summary, stats, backend_label) = if let Some(client) = daemon_client {
        spinner.set_message(format!("Analyzing via {}...", client.describe()));
        // Context is assembled daemon-side, per chunk, from `project_root`.
        let response = client
            .review(daemon::ReviewRequest {
                diff: diff.to_string(),
                languages: detect_languages(diff),
                requirements: ticket.clone(),
                max_tokens: settings.max_tokens,
                min_confidence: settings.min_confidence,
                triage: format!("{:?}", settings.triage).to_lowercase(),
                rules: project.custom.clone(),
                rulebooks: project.books.clone(),
                baseline: project
                    .baseline
                    .as_ref()
                    .and_then(|b| serde_json::to_string(b).ok()),
                use_cache: settings.use_cache,
                project_root: project_root.to_string_lossy().to_string(),
            })
            .context("the daemon failed; retry with --no-daemon to run in-process")?;
        spinner.finish_and_clear();
        (response.summary, response.stats.into(), response.backend)
    } else {
        let backend = build_backend(&settings.backend, model_dir)?;
        let backend_label = backend.describe();

        let context_for = context_builder(project_root, 8000);
        let mut analyzer = build_analyzer(
            backend,
            settings,
            project_root,
            diff,
            ticket.clone(),
            project,
        );

        spinner.set_message("Analyzing diff...");
        let (summary, stats) =
            run_with_progress(&mut analyzer, diff, &context_for, settings, &spinner)?;
        spinner.finish_and_clear();
        (summary, stats, backend_label)
    };

    // Report at --min-severity; gate at --fail-on.
    let shown: Vec<&ReviewFinding> = summary
        .findings
        .iter()
        .filter(|f| f.severity >= settings.min_severity)
        .collect();
    let gated = shown
        .iter()
        .filter(|f| f.severity >= settings.fail_on)
        .count();

    let filtered = ReviewSummary {
        findings: shown.iter().map(|f| (*f).clone()).collect(),
        positives: summary.positives.clone(),
        suggestions: summary.suggestions.clone(),
    };

    if is_text {
        output::print_positives_and_suggestions(&filtered.positives, &filtered.suggestions);
        output::print_footer(shown.len(), gated, &stats);
    }
    emit(settings, &filtered, &stats, &backend_label)?;

    // File the run. Never fatal: a review that found real problems must still be
    // reported even if the notes could not be written.
    let record = runs::RunRecord::new(
        git::head_sha().unwrap_or_else(|| "working-tree".into()),
        git::current_branch(),
        backend_label.clone(),
        &filtered,
        &stats,
        prefilter,
    );
    let rendered = output::markdown(&filtered, &stats, &backend_label);
    if let Err(e) = runs::save(project_root, &record, &rendered) {
        eprintln!("  !  could not record this run: {e}");
    }

    Ok(if gated > 0 { EXIT_FINDINGS } else { 0 })
}

/// Drive the analyzer with a live spinner and streamed findings.
fn run_with_progress(
    analyzer: &mut ReviewAnalyzer,
    diff: &str,
    context_for: &dyn Fn(&str) -> String,
    settings: &Settings,
    spinner: &ProgressBar,
) -> Result<(ReviewSummary, AnalysisStats)> {
    // The model blocks the calling thread, so a background ticker keeps the
    // elapsed time honest during a long single-chunk inference.
    let label = Arc::new(std::sync::Mutex::new(String::from("unit 1/1")));
    let done = Arc::new(AtomicBool::new(false));
    {
        let (label, done, pb) = (label.clone(), done.clone(), spinner.clone());
        std::thread::spawn(move || {
            let start = std::time::Instant::now();
            while !done.load(Ordering::Relaxed) {
                let secs = start.elapsed().as_secs();
                let elapsed = if secs < 60 {
                    format!("{secs}s")
                } else {
                    format!("{}m {}s", secs / 60, secs % 60)
                };
                let current = label.lock().map(|l| l.clone()).unwrap_or_default();
                pb.set_message(format!("Analyzing {current}  ({elapsed} elapsed)"));
                std::thread::sleep(Duration::from_millis(500));
            }
        });
    }

    let is_text = settings.format == OutputFormat::Text;
    let min_severity = settings.min_severity;
    let counter = Arc::new(AtomicUsize::new(0));

    let progress_label = label.clone();
    let stream_pb = spinner.clone();
    let stream_counter = counter.clone();

    let result = analyzer.analyze(
        diff,
        context_for,
        settings.max_tokens,
        move |chunk, total| {
            if let Ok(mut l) = progress_label.lock() {
                *l = format!("unit {chunk}/{total}");
            }
        },
        move |findings| {
            if !is_text {
                return;
            }
            for f in findings.iter().filter(|f| f.severity >= min_severity) {
                let n = stream_counter.fetch_add(1, Ordering::Relaxed) + 1;
                let rendered = output::format_finding(f, &format!("#{n}"));
                if stream_pb.is_hidden() {
                    // A hidden bar swallows `println`, so in CI (no TTY) the
                    // findings would never be shown at all.
                    eprintln!("{rendered}");
                } else {
                    // Draws above the live spinner without disturbing it.
                    stream_pb.println(rendered);
                }
            }
        },
    );

    done.store(true, Ordering::Relaxed);
    result.map_err(|e| anyhow::anyhow!("{e}"))
}

fn print_header(
    args: &cli::Cli,
    settings: &Settings,
    diff: &str,
    ticket: Option<&str>,
    rule_count: usize,
    report: &PrefilterReport,
    is_text: bool,
) {
    if !is_text {
        return;
    }

    let files = core_engine::parse_diff(diff);
    let langs = detect_languages(diff);

    let model_label = match &settings.backend {
        BackendChoice::Local { model, .. } => download::find_model(model)
            .map(|m| format!("{} · Q4_K_M · {:.1} GB", m.name, m.size_gb))
            .unwrap_or_else(|| model.clone()),
        BackendChoice::Remote {
            protocol,
            model,
            url,
            ..
        } => {
            format!("{model} · {} ({url})", protocol.as_str())
        }
    };

    let source = if args.stdin {
        "(stdin)".to_string()
    } else if args.staged {
        "staged changes".to_string()
    } else if args.last {
        "last commit".to_string()
    } else if let Some((range, _)) = resolve_range(args) {
        range
    } else {
        let base = settings.branch.clone().unwrap_or_else(git::default_branch);
        match git::current_branch() {
            Some(current) if current != base => format!("{current} → {base}"),
            _ => base,
        }
    };

    eprintln!();
    eprintln!("  diffmind  code review");
    eprintln!("  {}", "─".repeat(52));
    eprintln!("  {:<10} {model_label}", "Model");
    eprintln!("  {:<10} {source}", "Source");
    eprintln!(
        "  {:<10} {} file{}",
        "Changed",
        files.len(),
        if files.len() == 1 { "" } else { "s" }
    );
    // The number that makes the filtering trustworthy: what was skipped, and
    // why. Silent filtering reads as "reviewed everything" when it did not.
    if report.hunks_dropped() > 0 {
        eprintln!(
            "  {:<10} {} hunks → {} reviewable ({} filtered: {})",
            "Filtered",
            report.hunks_total,
            report.hunks_kept,
            report.hunks_dropped(),
            report.reason_summary()
        );
    }
    eprintln!(
        "  {:<10} {}",
        "Stack",
        if langs.is_empty() {
            "unknown".to_string()
        } else {
            langs.join(", ")
        }
    );
    if rule_count > 0 {
        eprintln!("  {:<10} {rule_count} custom", "Rules");
    }
    eprintln!(
        "  {:<10} report ≥{}, fail ≥{}",
        "Gate",
        settings.min_severity.as_str(),
        settings.fail_on.as_str()
    );
    if let Some(t) = ticket {
        let preview: String = t.chars().take(60).collect();
        eprintln!(
            "  {:<10} {preview}{}",
            "Ticket",
            if t.chars().count() > 60 { "..." } else { "" }
        );
    }
    eprintln!();
}

/// Write machine-readable output to `--output` or stdout.
fn emit(
    settings: &Settings,
    summary: &ReviewSummary,
    stats: &AnalysisStats,
    model: &str,
) -> Result<()> {
    if settings.format == OutputFormat::Text && settings.output_path.is_none() {
        return Ok(());
    }

    let rendered = if settings.format == OutputFormat::Text {
        // `--output report.md` with the default format still deserves a
        // readable document rather than an empty file.
        output::render(OutputFormat::Markdown, summary, stats, model)
    } else {
        output::render(settings.format, summary, stats, model)
    };

    match &settings.output_path {
        Some(path) => {
            if let Some(parent) = Path::new(path).parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, &rendered).with_context(|| format!("could not write {path}"))?;
            eprintln!("  Report written to {path}");
        }
        None => println!("{rendered}"),
    }
    Ok(())
}

// ─── PR description ──────────────────────────────────────────────────────────

fn run_describe(
    diff: &str,
    model_dir: &Path,
    ticket: Option<&str>,
    settings: &Settings,
) -> Result<()> {
    eprintln!();
    eprintln!("  diffmind  PR description");
    eprintln!("  {}", "─".repeat(52));
    let files = core_engine::parse_diff(diff).len();
    eprintln!(
        "  {:<10} {files} file{}",
        "Changed",
        if files == 1 { "" } else { "s" }
    );
    eprintln!();

    let spinner = make_spinner("Loading model...", false);
    let backend = build_backend(&settings.backend, model_dir)?;
    let mut analyzer = ReviewAnalyzer::new(backend)
        .with_sampling(settings.temperature, settings.seed)
        .with_debug(settings.debug);

    spinner.set_message("Generating PR description...");
    let desc = analyzer
        .generate_pr_description(diff, ticket, 768)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    spinner.finish_and_clear();

    print_pr_description(&desc);
    Ok(())
}

fn print_pr_description(desc: &PrDescription) {
    eprintln!();
    eprintln!("  {}  Title", "─".repeat(62).dark_grey());
    eprintln!();
    eprintln!("    {}", desc.title.clone().bold());
    eprintln!();

    if !desc.summary.is_empty() {
        eprintln!("  {}  Summary", "─".repeat(62).dark_grey());
        eprintln!();
        for item in &desc.summary {
            eprintln!("    {}  {item}", "·".cyan());
        }
        eprintln!();
    }

    if !desc.test_plan.is_empty() {
        eprintln!("  {}  Test plan", "─".repeat(62).dark_grey());
        eprintln!();
        for item in &desc.test_plan {
            eprintln!("    {}  {item}", "☐".dark_grey());
        }
        eprintln!();
    }

    eprintln!("  {}", "─".repeat(62).dark_grey());
    eprintln!(
        "  {}  Copy the above to your PR description.",
        "·".dark_grey()
    );
    eprintln!();
}

// ─── Commit message ──────────────────────────────────────────────────────────

fn run_commit(diff: &str, model_dir: &Path, settings: &Settings, apply: bool) -> Result<()> {
    eprintln!();
    eprintln!("  diffmind  commit message");
    eprintln!("  {}", "─".repeat(52));
    let files = core_engine::parse_diff(diff).len();
    eprintln!(
        "  {:<10} {files} file{}",
        "Staged",
        if files == 1 { "" } else { "s" }
    );
    eprintln!();

    let spinner = make_spinner("Loading model...", false);
    let backend = build_backend(&settings.backend, model_dir)?;
    let mut analyzer = ReviewAnalyzer::new(backend)
        .with_sampling(settings.temperature, settings.seed)
        .with_debug(settings.debug);

    spinner.set_message("Generating commit message...");
    let suggestion = analyzer
        .generate_commit_message(diff, 512)
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    spinner.finish_and_clear();

    print_commit_suggestion(&suggestion);
    if apply {
        run_git_commit(&suggestion)?;
    }
    Ok(())
}

fn print_commit_suggestion(s: &CommitSuggestion) {
    eprintln!();
    eprintln!("  {}", "─".repeat(62).dark_grey());
    eprintln!();
    eprintln!("  {}", s.message.clone().bold());
    if !s.body.trim().is_empty() {
        eprintln!();
        for line in s.body.lines() {
            eprintln!("  {}", line.dark_grey());
        }
    }
    eprintln!();
    eprintln!("  {}", "─".repeat(62).dark_grey());
    eprintln!(
        "  {}  Run:  git commit -m \"{}\"",
        "·".dark_grey(),
        s.message
    );
    eprintln!("  {}  Or:   diffmind commit --apply", "·".dark_grey());
    eprintln!();
}

fn run_git_commit(s: &CommitSuggestion) -> Result<()> {
    use std::process::Command;

    let mut cmd = Command::new("git");
    cmd.args(["commit", "-m", &s.message]);
    if !s.body.trim().is_empty() {
        cmd.args(["-m", s.body.trim()]);
    }

    eprintln!("  {}  Running git commit...", "·".dark_grey());
    let status = cmd.status()?;
    if !status.success() {
        anyhow::bail!("git commit failed");
    }
    eprintln!("  {}  Committed.", "✓".green().bold());
    Ok(())
}

// ─── Baseline ────────────────────────────────────────────────────────────────

fn run_baseline(
    action: cli::BaselineAction,
    args: &cli::Cli,
    file_config: &config::FileConfig,
    model_dir: &Path,
    project_root: &Path,
    _home: &Path,
) -> Result<i32> {
    let path = project_root.join(".diffmind").join("baseline.json");

    match action {
        cli::BaselineAction::Show => {
            let Some(baseline) = load_baseline(project_root, true) else {
                println!("No baseline at {}", path.display());
                return Ok(0);
            };
            println!(
                "  {} accepted finding(s), recorded {}",
                baseline.len(),
                baseline.generated_at
            );
            for entry in &baseline.entries {
                println!("  {}  {}  {}", entry.fingerprint, entry.rule_id, entry.file);
            }
            Ok(0)
        }

        cli::BaselineAction::Clear => {
            if path.exists() {
                std::fs::remove_file(&path)?;
                println!("Baseline removed.");
            } else {
                println!("No baseline to remove.");
            }
            Ok(0)
        }

        cli::BaselineAction::Create { branch, model } => {
            let mut merged = clone_args_for_subcommand(args, model, None);
            merged.branch = branch;
            let mut settings = settings::resolve_settings(&merged, file_config)?;
            // A baseline must capture everything, or the first `--min-severity
            // low` run would fail on findings the baseline never recorded.
            settings.min_severity = Severity::Low;
            settings.min_confidence = 0.0;
            settings.use_baseline = false;

            let diff = match merged.branch.clone() {
                Some(b) => git::get_diff(&b, &[])?,
                None => git::get_working_tree_diff(&[])?,
            };
            if diff.trim().is_empty() {
                anyhow::bail!(
                    "no changes to baseline. A baseline records the findings that already \
                     exist in your working tree; pass --branch <base> to baseline a whole branch."
                );
            }

            // Same filtering as a real review, or the baseline would record —
            // and then permanently accept — findings on files no review will
            // ever look at again.
            let (diff, _) = apply_prefilter(&diff, &settings, project_root)?;

            eprintln!("  Reviewing to establish a baseline — this runs a full analysis.\n");
            let backend = build_backend(&settings.backend, model_dir)?;
            let context_for = context_builder(project_root, 8000);
            let mut analyzer = build_analyzer(
                backend,
                &settings,
                project_root,
                &diff,
                None,
                ProjectRules {
                    baseline: None,
                    ..ProjectRules::load(project_root, false)
                },
            );

            let spinner = make_spinner("Analyzing...", false);
            let (summary, _) =
                run_with_progress(&mut analyzer, &diff, &context_for, &settings, &spinner)?;
            spinner.finish_and_clear();

            let baseline =
                Baseline::from_findings(&summary.findings, chrono::Utc::now().to_rfc3339());
            std::fs::create_dir_all(path.parent().expect("baseline path has a parent"))?;
            std::fs::write(&path, serde_json::to_string_pretty(&baseline)?)?;

            println!(
                "\n  ✓  Baselined {} finding(s) → {}",
                baseline.len(),
                path.display()
            );
            println!("     Commit this file. Future runs report only new issues.");
            Ok(0)
        }
    }
}

// ─── Daemon ──────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn run_serve(
    idle_timeout: u64,
    port: u16,
    stop: bool,
    status: bool,
    settings: &Settings,
    home: &Path,
    model_dir: &Path,
    project_root: &Path,
) -> Result<i32> {
    if stop {
        return Ok(if daemon::stop(home) {
            println!("Daemon stopped.");
            0
        } else {
            println!("No daemon was running.");
            0
        });
    }

    let (model_id, device_id) = settings.backend.identity();

    if status {
        return Ok(match daemon::Client::connect(home, &model_id, &device_id) {
            Some(client) => {
                println!("Running: {}", client.describe());
                0
            }
            None => {
                println!("No daemon running for model '{model_id}' on '{device_id}'.");
                0
            }
        });
    }

    if !settings.backend.is_local() {
        anyhow::bail!(
            "a daemon only helps a local model — a remote backend has no weights to keep resident."
        );
    }

    // Refuse to start a second daemon for the same model rather than race for
    // the info file.
    if daemon::Client::connect(home, &model_id, &device_id).is_some() {
        anyhow::bail!(
            "a daemon is already running for this model. Stop it with `diffmind serve --stop`."
        );
    }

    let server = daemon::Server::bind(port, Duration::from_secs(idle_timeout))?;
    let bound_port = server.port()?;

    eprintln!("  Loading model into memory...");
    let backend = build_backend(&settings.backend, model_dir)?;
    let backend_label = backend.describe();

    daemon::write_info(
        home,
        &daemon::DaemonInfo {
            port: bound_port,
            token: server.token().to_string(),
            pid: std::process::id(),
            model: model_id,
            device: device_id,
            version: output::VERSION.to_string(),
        },
    )?;

    eprintln!("  diffmind daemon ready");
    eprintln!("    backend       {backend_label}");
    eprintln!("    listening     127.0.0.1:{bound_port}");
    eprintln!("    idle timeout  {idle_timeout}s");
    eprintln!("    stop with     diffmind serve --stop\n");

    // One resident backend, reused across requests. Held in an Option so it can
    // be moved into the analyzer and back out on each call.
    let mut resident = Some(backend);
    let settings = settings.clone();
    let project_root = project_root.to_path_buf();

    let result = server.run(|req| {
        let Some(backend) = resident.take() else {
            return daemon::Response::Error {
                message: "backend unavailable".into(),
            };
        };

        let baseline = req
            .baseline
            .as_deref()
            .and_then(|raw| Baseline::parse(raw).ok());

        // Everything the client can vary per invocation must come from the
        // request, not from whatever the daemon happened to be started with —
        // otherwise `--no-cache` and friends are silently ignored, and a daemon
        // started in one repo would write another repo's cache.
        let mut per_request = settings.clone();
        per_request.max_tokens = req.max_tokens;
        per_request.min_confidence = req.min_confidence;
        per_request.triage = core_engine::TriageMode::parse(&req.triage);
        per_request.use_cache = req.use_cache;

        let request_root = if req.project_root.is_empty() {
            project_root.clone()
        } else {
            PathBuf::from(&req.project_root)
        };

        let mut analyzer = build_analyzer(
            backend,
            &per_request,
            &request_root,
            &req.diff,
            req.requirements.clone(),
            ProjectRules {
                custom: req.rules.clone(),
                books: req.rulebooks.clone(),
                baseline,
            },
        );

        // Built here rather than sent by the client: the daemon owns chunking,
        // so only the daemon knows what each chunk contains.
        let context_for = context_builder(&request_root, 8000);
        let outcome = analyzer.analyze(&req.diff, &context_for, req.max_tokens, |_, _| {}, |_| {});

        let backend_label = analyzer.backend_description();
        // Reclaim the loaded weights for the next request — the whole point of
        // the daemon.
        resident = Some(analyzer.into_backend());

        match outcome {
            Ok((summary, stats)) => daemon::Response::Ok(Box::new(daemon::ReviewResponse {
                summary,
                stats: (&stats).into(),
                backend: backend_label,
            })),
            Err(e) => daemon::Response::Error {
                message: e.to_string(),
            },
        }
    });

    daemon::clear_info(home);
    result?;
    Ok(0)
}
