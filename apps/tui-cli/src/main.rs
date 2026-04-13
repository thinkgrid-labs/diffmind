use anyhow::Result;
use core_engine::{DevicePreference, ReviewAnalyzer, ReviewFinding, Severity};
use indicatif::{ProgressBar, ProgressStyle};
use std::{
    collections::HashSet,
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::Mutex;

mod cli;
mod download;
mod git;
mod indexer;
mod rag;

use crate::indexer::Indexer;

#[tokio::main]
async fn main() -> Result<()> {
    let args = cli::parse();

    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map_err(|_| anyhow::anyhow!("Could not find home directory"))?;
    let model_dir = PathBuf::from(home).join(".diffmind").join("models");
    let project_root = std::env::current_dir()?;

    // 1. Handle subcommands
    if let Some(command) = args.command {
        match command {
            cli::Commands::Download { model, force } => {
                download::ensure_model_files(model.as_deref(), &model_dir, force)?;
                return Ok(());
            }
            cli::Commands::Index => {
                let mut indexer = Indexer::new(project_root.clone());
                let existing = Indexer::load(&project_root);
                let new_index = indexer.build_index(existing)?;
                indexer.save(&new_index)?;
                println!("Index updated: {} symbols found", new_index.symbols.len());
                return Ok(());
            }
        }
    }

    // 2. Capture diff
    let diff = if args.stdin {
        use std::io::Read;
        let mut buffer = String::new();
        io::stdin().read_to_string(&mut buffer).ok();
        buffer
    } else if args.last {
        git::get_last_commit_diff(&args.files)?
    } else {
        git::get_diff(&args.branch, &args.files)?
    };

    if diff.trim().is_empty() {
        println!("No changes detected. Nothing to analyze.");
        return Ok(());
    }

    // 3. Resolve ticket / user story content (file path or inline text)
    let ticket = resolve_ticket(args.ticket.as_deref());

    // 4. Launch UI (TUI or static)
    if args.tui {
        run_tui(diff, model_dir, project_root, args.model.clone(), ticket).await?;
    } else {
        let min_sev = parse_severity(&args.min_severity);
        let has_findings =
            run_static(&diff, &model_dir, &project_root, &args, min_sev, ticket).await?;

        // Non-zero exit if any findings at or above --min-severity (CI gate).
        if has_findings {
            std::process::exit(1);
        }
    }

    Ok(())
}

// ─── Device helpers ──────────────────────────────────────────────────────────

fn parse_device(s: &str) -> DevicePreference {
    match s.to_lowercase().as_str() {
        "metal" => DevicePreference::Metal,
        "cpu" => DevicePreference::Cpu,
        _ => DevicePreference::Auto,
    }
}

// ─── Severity helpers ────────────────────────────────────────────────────────

fn parse_severity(s: &str) -> Severity {
    match s.to_lowercase().as_str() {
        "high" => Severity::High,
        "medium" => Severity::Medium,
        _ => Severity::Low,
    }
}

/// Returns true if `finding` meets or exceeds `threshold`.
/// Ordering: High > Medium > Low.
fn severity_rank(s: &Severity) -> u8 {
    match s {
        Severity::High => 2,
        Severity::Medium => 1,
        Severity::Low => 0,
    }
}

fn meets_threshold(finding: &Severity, threshold: &Severity) -> bool {
    severity_rank(finding) >= severity_rank(threshold)
}

// ─── Ticket / requirements resolver ─────────────────────────────────────────

/// Accepts either a file path or inline text.
/// - If the value is a path that exists on disk → read and return its contents.
/// - Otherwise treat the value itself as the requirements text.
fn resolve_ticket(input: Option<&str>) -> Option<String> {
    let raw = input?;
    let path = std::path::Path::new(raw);
    if path.exists() {
        match std::fs::read_to_string(path) {
            Ok(content) => Some(content),
            Err(e) => {
                eprintln!("Warning: could not read ticket file '{}': {}", raw, e);
                None
            }
        }
    } else {
        Some(raw.to_string())
    }
}

// ─── Language detection ──────────────────────────────────────────────────────

/// Extracts language names from a git diff by inspecting file extensions in
/// `diff --git` header lines. Used to build a language-aware system prompt.
fn detect_languages(diff: &str) -> Vec<String> {
    let mut langs: HashSet<String> = HashSet::new();

    for line in diff.lines() {
        if !line.starts_with("diff --git") {
            continue;
        }
        // The rightmost '.' gives the extension (appears twice in the header;
        // rsplit ensures we grab the real extension and not a path component).
        if let Some(ext) = line.rsplit('.').next() {
            // Strip any trailing whitespace that could follow the filename.
            let ext = ext.split_whitespace().next().unwrap_or("");
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
                "cpp" | "cc" | "cxx" => Some("C++"),
                "c" | "h" => Some("C"),
                "php" => Some("PHP"),
                _ => None,
            };
            if let Some(l) = lang {
                langs.insert(l.to_string());
            }
        }
    }

    let mut result: Vec<String> = langs.into_iter().collect();
    result.sort(); // deterministic ordering for stable prompts
    result
}

// ─── Static (non-TUI) runner ─────────────────────────────────────────────────

fn count_diff_files(diff: &str) -> usize {
    diff.lines().filter(|l| l.starts_with("diff --git")).count()
}

fn make_spinner(msg: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ")
            .template("  {spinner:.cyan}  {msg}")
            .unwrap(),
    );
    pb.set_message(msg.to_string());
    pb.enable_steady_tick(Duration::from_millis(80));
    pb
}

/// Returns `true` if any findings at or above `min_severity` were found,
/// which causes `main` to exit with code 1 (CI gate).
async fn run_static(
    diff: &str,
    model_dir: &Path,
    project_root: &Path,
    args: &cli::Cli,
    min_severity: Severity,
    ticket: Option<String>,
) -> Result<bool> {
    let model_path = model_dir.join(format!("qwen2.5-coder-{}-instruct-q4_k_m.gguf", args.model));
    let tokenizer_path = model_dir.join("tokenizer.json");

    if !model_path.exists() || !tokenizer_path.exists() {
        return Err(anyhow::anyhow!(
            "Model files not found. Run `diffmind download` first."
        ));
    }

    // ── Header ────────────────────────────────────────────────────────────────
    let langs = detect_languages(diff);
    let model_label = download::find_model(&args.model)
        .map(|m| format!("{} · Q4_K_M · {:.1} GB", m.name, m.size_gb))
        .unwrap_or_else(|| format!("Qwen2.5-Coder-{} · Q4_K_M", args.model));
    let stack_label = if langs.is_empty() {
        "unknown".to_string()
    } else {
        langs.join(", ")
    };
    let file_count = count_diff_files(diff);
    let branch_label = if args.stdin {
        "(stdin)".to_string()
    } else if args.last {
        "HEAD~1..HEAD  (last commit)".to_string()
    } else {
        match git::current_branch() {
            Some(current) if current != args.branch => format!("{} → {}", current, args.branch),
            _ => args.branch.clone(),
        }
    };

    eprintln!();
    eprintln!("  diffmind  code review");
    eprintln!("  {}", "─".repeat(52));
    eprintln!("  {:<10} {}", "Model", model_label);
    eprintln!("  {:<10} {}", "Branch", branch_label);
    eprintln!(
        "  {:<10} {} file{}",
        "Changed",
        file_count,
        if file_count == 1 { "" } else { "s" }
    );
    eprintln!("  {:<10} {}", "Stack", stack_label);
    if let Some(ref t) = ticket {
        let preview: String = t.chars().take(60).collect();
        eprintln!(
            "  {:<10} {}{}",
            "Ticket",
            preview,
            if t.len() > 60 { "..." } else { "" }
        );
    }
    eprintln!();

    // ── Load model weights ────────────────────────────────────────────────────
    let spinner = make_spinner("Loading model into memory...");
    let model_bytes = std::fs::read(&model_path)?;
    let tokenizer_bytes = std::fs::read(&tokenizer_path)?;

    // ── RAG context ───────────────────────────────────────────────────────────
    let index = Indexer::load(project_root);
    let mut context = String::new();
    if let Some(idx) = index
        && let Some(rag_text) = rag::get_rag_context(diff, &idx)
    {
        context = rag_text;
    }

    // ── Build analyzer ────────────────────────────────────────────────────────
    let device_pref = parse_device(&args.device);
    let mut analyzer = ReviewAnalyzer::new_with_device(&model_bytes, &tokenizer_bytes, device_pref)
        .map_err(|e| anyhow::anyhow!(e.to_string()))?
        .with_languages(langs)
        .with_debug(args.debug);

    if let Some(req) = ticket {
        analyzer = analyzer.with_requirements(req);
    }

    // ── Run inference with live elapsed timer + per-chunk progress ────────────
    spinner.set_message("Analyzing diff...");

    // Background thread ticks elapsed seconds so the spinner stays alive even
    // during long single-chunk inference (model blocks the main thread).
    let timer_pb = spinner.clone();
    let timer_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let timer_done_clone = timer_done.clone();
    let chunk_label = std::sync::Arc::new(std::sync::Mutex::new(String::from("chunk 1/1")));
    let chunk_label_clone = chunk_label.clone();
    std::thread::spawn(move || {
        let start = std::time::Instant::now();
        while !timer_done_clone.load(std::sync::atomic::Ordering::Relaxed) {
            let secs = start.elapsed().as_secs();
            let elapsed = if secs < 60 {
                format!("{}s", secs)
            } else {
                format!("{}m {}s", secs / 60, secs % 60)
            };
            let label = chunk_label_clone.lock().unwrap().clone();
            timer_pb.set_message(format!("Analyzing {}  ({} elapsed)", label, elapsed));
            std::thread::sleep(Duration::from_secs(1));
        }
    });

    // ── Streaming: print findings as each chunk completes (text mode only) ──────
    // indicatif's .println() draws above the spinner without disturbing it.
    let pb_progress = spinner.clone();
    let pb_stream = spinner.clone();
    let stream_min_sev = min_severity.clone();
    let stream_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let stream_count_clone = stream_count.clone();
    let is_text = matches!(args.format, cli::OutputFormat::Text);

    let (summary, skipped) = analyzer
        .analyze_diff_chunked_with_progress(
            diff,
            &context,
            args.max_tokens,
            move |done, total| {
                *chunk_label.lock().unwrap() = format!("chunk {}/{}", done, total);
                pb_progress.set_message(format!("Analyzing chunk {}/{}...", done, total));
            },
            move |chunk_findings| {
                if !is_text {
                    return;
                }
                for f in chunk_findings {
                    if meets_threshold(&f.severity, &stream_min_sev) {
                        let n = stream_count_clone
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                            + 1;
                        pb_stream.println(format_finding(f, &format!("#{}", n)));
                    }
                }
            },
        )
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;

    timer_done.store(true, std::sync::atomic::Ordering::Relaxed);
    spinner.finish_and_clear();

    // ── Warn about chunks the model failed to parse ───────────────────────────
    if skipped > 0 {
        eprintln!(
            "  !  {} chunk{} returned unparseable output (model may need more tokens or a larger variant).",
            skipped,
            if skipped == 1 { "" } else { "s" }
        );
    }

    // ── Filter findings to threshold ──────────────────────────────────────────
    let findings: Vec<&ReviewFinding> = summary
        .findings
        .iter()
        .filter(|f| meets_threshold(&f.severity, &min_severity))
        .collect();

    match args.format {
        cli::OutputFormat::Json => {
            // Emit the full summary as JSON — pipe-friendly for CI dashboards
            let out = serde_json::json!({
                "findings": findings,
                "positives": summary.positives,
                "suggestions": summary.suggestions,
            });
            let json =
                serde_json::to_string_pretty(&out).map_err(|e| anyhow::anyhow!(e.to_string()))?;
            println!("{}", json);
        }
        cli::OutputFormat::Text => {
            // Findings were already streamed above — just print the footer.
            if findings.is_empty() {
                if skipped > 0 {
                    eprintln!(
                        "  ?  No parseable findings — try `--model 3b` for better output quality."
                    );
                } else {
                    eprintln!("  {}  No issues found.", "✓".green().bold());
                }
            } else {
                print_summary(findings.len(), skipped);
            }
            print_positives_and_suggestions(&summary.positives, &summary.suggestions);
        }
    }

    Ok(!findings.is_empty())
}

// ─── Coloured finding renderer ────────────────────────────────────────────────

use core_engine::Category;
use crossterm::style::Stylize;

fn severity_color(f: &ReviewFinding) -> String {
    match f.severity {
        Severity::High => format!("{}", " HIGH ".on_red().white().bold()),
        Severity::Medium => format!("{}", " MED  ".on_dark_yellow().white().bold()),
        Severity::Low => format!("{}", " LOW  ".on_dark_cyan().white().bold()),
    }
}

fn category_icon(f: &ReviewFinding) -> &'static str {
    match f.category {
        Category::Security => "🔒",
        Category::Quality => "🐛",
        Category::Performance => "⚡",
        Category::Maintainability => "📐",
        Category::Compliance => "📋",
    }
}

fn wrap_text(text: &str, indent: usize, width: usize) -> String {
    let pad = " ".repeat(indent);
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.is_empty() {
            current.push_str(word);
        } else if current.len() + 1 + word.len() <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(format!("{}{}", pad, current));
            current = word.to_string();
        }
    }
    if !current.is_empty() {
        lines.push(format!("{}{}", pad, current));
    }
    lines.join("\n")
}

/// Build a fully-formatted, ANSI-coloured string for one finding.
/// `counter` is a short label shown on the header row, e.g. `"#1"` or `"2/5"`.
fn format_finding(f: &ReviewFinding, counter: &str) -> String {
    let sev = severity_color(f);
    let icon = category_icon(f);
    let cat = format!("{:?}", f.category).to_lowercase();
    let loc = format!("{}:{}", f.file, f.line).dark_grey();
    let counter_label = format!("[{}]", counter).dark_grey();

    let mut out = String::new();

    // ── Header row ──
    out.push_str(&format!(
        "\n  {}  {}  {}  {} {}\n",
        sev,
        icon,
        cat.dark_grey(),
        loc,
        counter_label
    ));

    // ── Separator ──
    out.push_str(&format!("  {}\n", "─".repeat(62).dark_grey()));

    // ── Issue ──
    let issue_wrapped = wrap_text(&f.issue, 10, 68);
    let mut issue_lines = issue_wrapped.lines();
    out.push_str(&format!(
        "  {}  {}\n",
        "Issue".red().bold(),
        issue_lines.next().unwrap_or("").trim_start()
    ));
    for line in issue_lines {
        out.push_str(&format!("{}\n", line));
    }

    // ── Fix ──
    let fix_wrapped = wrap_text(&f.suggested_fix, 10, 68);
    let mut fix_lines = fix_wrapped.lines();
    out.push_str(&format!(
        "  {}    {}\n",
        "Fix".green().bold(),
        fix_lines.next().unwrap_or("").trim_start()
    ));
    for line in fix_lines {
        out.push_str(&format!("{}\n", line));
    }

    out
}

fn print_summary(count: usize, skipped: usize) {
    if skipped > 0 {
        eprintln!(
            "  {}  {} chunk{} had unparseable output — try --model 3b",
            "!".yellow(),
            skipped,
            if skipped == 1 { "" } else { "s" }
        );
    }
    eprintln!(
        "  {}  {} finding{}  {}",
        "⚠".yellow().bold(),
        count,
        if count == 1 { "" } else { "s" },
        "(exit 1)".dark_grey()
    );
    eprintln!();
}

fn print_positives_and_suggestions(positives: &[String], suggestions: &[String]) {
    if positives.is_empty() && suggestions.is_empty() {
        return;
    }

    if !positives.is_empty() {
        eprintln!("  {}", "─".repeat(62).dark_grey());
        eprintln!("  {}  What looks good", "✓".green().bold());
        for p in positives {
            eprintln!("     {}  {}", "·".green(), p);
        }
        eprintln!();
    }

    if !suggestions.is_empty() {
        if positives.is_empty() {
            eprintln!("  {}", "─".repeat(62).dark_grey());
        }
        eprintln!("  💡  Suggestions");
        for s in suggestions {
            eprintln!("     {}  {}", "·".dark_yellow(), s);
        }
        eprintln!();
    }
}

// ─── TUI runner ───────────────────────────────────────────────────────────────

use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};

struct App {
    findings: Vec<ReviewFinding>,
    state: ListState,
    status: String,
    analyzing: bool,
    diff: String,
    model_dir: PathBuf,
    project_root: PathBuf,
    model_id: String,
    ticket: Option<String>,
}

async fn run_tui(
    diff: String,
    model_dir: PathBuf,
    project_root: PathBuf,
    model_id: String,
    ticket: Option<String>,
) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let app = Arc::new(Mutex::new(App {
        findings: Vec::new(),
        state: ListState::default(),
        status: "Ready — press 'a' to analyze".to_string(),
        analyzing: false,
        diff,
        model_dir,
        project_root,
        model_id,
        ticket,
    }));

    let res = tui_loop(&mut terminal, Arc::clone(&app)).await;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    res
}

async fn tui_loop<B: Backend>(terminal: &mut Terminal<B>, app: Arc<Mutex<App>>) -> Result<()>
where
    B::Error: Send + Sync + 'static,
{
    loop {
        {
            let mut app_lock = app.lock().await;
            terminal.draw(|f| ui(f, &mut app_lock))?;
        }

        if event::poll(Duration::from_millis(100))?
            && let Event::Key(key) = event::read()?
        {
            let mut app_lock = app.lock().await;
            match key.code {
                KeyCode::Char('q') => return Ok(()),
                KeyCode::Down | KeyCode::Char('j') => {
                    let i = match app_lock.state.selected() {
                        Some(i) if !app_lock.findings.is_empty() => {
                            (i + 1) % app_lock.findings.len()
                        }
                        _ => 0,
                    };
                    app_lock.state.select(Some(i));
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    let i = match app_lock.state.selected() {
                        Some(i) if !app_lock.findings.is_empty() => {
                            if i == 0 {
                                app_lock.findings.len() - 1
                            } else {
                                i - 1
                            }
                        }
                        _ => 0,
                    };
                    app_lock.state.select(Some(i));
                }
                KeyCode::Char('a') => {
                    if !app_lock.analyzing {
                        app_lock.analyzing = true;
                        app_lock.status = "Analyzing...".to_string();
                        let app_clone = Arc::clone(&app);
                        tokio::spawn(async move {
                            let app_err = Arc::clone(&app_clone);
                            if let Err(e) = background_analysis(app_clone).await {
                                let mut app = app_err.lock().await;
                                app.status = format!("Error: {}", e);
                                app.analyzing = false;
                            }
                        });
                    }
                }
                _ => {}
            }
        }
    }
}

async fn background_analysis(app: Arc<Mutex<App>>) -> Result<()> {
    let (diff, model_dir, project_root, model_id, ticket) = {
        let app = app.lock().await;
        (
            app.diff.clone(),
            app.model_dir.clone(),
            app.project_root.clone(),
            app.model_id.clone(),
            app.ticket.clone(),
        )
    };

    let model_path = model_dir.join(format!("qwen2.5-coder-{}-instruct-q4_k_m.gguf", model_id));
    let tokenizer_path = model_dir.join("tokenizer.json");

    let model_bytes = std::fs::read(model_path)?;
    let tokenizer_bytes = std::fs::read(tokenizer_path)?;

    let index = Indexer::load(&project_root);
    let mut context = String::new();
    if let Some(idx) = index
        && let Some(rag_text) = rag::get_rag_context(&diff, &idx)
    {
        context = rag_text;
    }

    let langs = detect_languages(&diff);
    let mut analyzer =
        ReviewAnalyzer::new_with_device(&model_bytes, &tokenizer_bytes, DevicePreference::Auto)
            .map_err(|e| anyhow::anyhow!(e.to_string()))?
            .with_languages(langs);

    if let Some(req) = ticket {
        analyzer = analyzer.with_requirements(req);
    }

    let summary = analyzer
        .analyze_diff_chunked(&diff, &context, 1024)
        .map_err(|e| anyhow::anyhow!(e.to_string()))?;

    let mut app_lock = app.lock().await;
    let count = summary.findings.len();
    app_lock.findings = summary.findings;
    app_lock.status = format!(
        "Done — {} finding{}",
        count,
        if count == 1 { "" } else { "s" }
    );
    app_lock.analyzing = false;
    if count > 0 {
        app_lock.state.select(Some(0));
    }
    Ok(())
}

fn ui(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(f.area());

    let header = Paragraph::new(format!(" diffmind  {}", app.status))
        .block(Block::default().borders(Borders::ALL).title("Status"))
        .style(if app.analyzing {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::Green)
        });
    f.render_widget(header, chunks[0]);

    if app.findings.is_empty() && !app.analyzing {
        render_welcome_screen(f, chunks[1]);
    } else {
        let body_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(30), Constraint::Percentage(70)])
            .split(chunks[1]);

        let items: Vec<ListItem> = app
            .findings
            .iter()
            .map(|f| {
                let color = match f.severity {
                    Severity::High => Color::Red,
                    Severity::Medium => Color::Yellow,
                    Severity::Low => Color::Cyan,
                };
                let tag = match f.category {
                    core_engine::Category::Compliance => "[Req]",
                    _ => match f.severity {
                        Severity::High => "[High]",
                        Severity::Medium => "[Med]",
                        Severity::Low => "[Low]",
                    },
                };
                ListItem::new(format!("{} {}", tag, f.file)).style(Style::default().fg(color))
            })
            .collect();

        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title("Findings"))
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol(">> ");
        f.render_stateful_widget(list, body_chunks[0], &mut app.state);

        let detail_text = if let Some(i) = app.state.selected() {
            if let Some(finding) = app.findings.get(i) {
                format!(
                    "FILE: {}\nLINE: {}\nCATEGORY: {:?}\n\nISSUE:\n{}\n\nFIX:\n{}",
                    finding.file,
                    finding.line,
                    finding.category,
                    finding.issue,
                    finding.suggested_fix
                )
            } else {
                "No selection".to_string()
            }
        } else {
            "Select a finding with j/k".to_string()
        };

        let detail = Paragraph::new(detail_text)
            .block(Block::default().borders(Borders::ALL).title("Details"))
            .wrap(Wrap { trim: true });
        f.render_widget(detail, body_chunks[1]);
    }

    let footer = Paragraph::new(" [q] Quit  [a] Analyze  [j/k] Navigate ")
        .style(Style::default().fg(Color::DarkGray));
    f.render_widget(footer, chunks[2]);
}

fn render_welcome_screen(f: &mut Frame, area: ratatui::layout::Rect) {
    let welcome = Paragraph::new(
        "\n  Welcome to diffmind\n  ====================\n\n  Press 'a' to run analysis\n  j / k to navigate findings\n  q to quit",
    )
    .block(Block::default().borders(Borders::ALL).title("Start"))
    .style(Style::default().fg(Color::Cyan));
    f.render_widget(welcome, area);
}
