//! Interactive review — the terminal UI behind `diffmind --tui`.
//!
//! This is the surface the whole tool is for: not a report, but a place to sit
//! while deciding what to say about someone else's branch. Three things follow
//! from that.
//!
//! **It analyses on launch.** Time-to-first-signal is the point; a tool that
//! opens and waits for a keypress has already spent the reviewer's attention.
//!
//! **A finding shows its evidence.** The detail pane carries the actual hunk and
//! the context the model was given, because "trust me, line 42" is exactly what
//! makes people stop trusting an AI reviewer.
//!
//! **Every finding gets a verdict.** `a`/`d`/`w` are not UI niceties — they are
//! the measurement. Whether this tool is worth running is decided by the
//! accept-to-wrong ratio, and nothing else in the system can observe it.
//!
//! Output here is private to the reviewer, which is what licenses the model
//! being wrong sometimes: a bad finding costs one keystroke, not an author's
//! afternoon.

use anyhow::Result;
use core_engine::{AnalysisStats, Category, PrefilterReport, ReviewFinding, Severity};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
};
use std::{
    collections::HashMap,
    io,
    path::PathBuf,
    sync::mpsc::{self, Receiver, TryRecvError},
    time::Duration,
};

use crate::runs::{self, Verdict};
use crate::settings::Settings;

/// The evidence behind a finding: the hunk the model was shown, and the symbol
/// context it was given alongside.
#[derive(Clone)]
struct UnitView {
    text: String,
    context: String,
}

enum Msg {
    Progress(String),
    /// One unit, sent as it starts — always before any finding that names it.
    ///
    /// Sent from the analyzer's own progress callback rather than planned up
    /// front. Predicting the unit list means reimplementing triage and unit
    /// grouping outside the engine, and getting either subtly wrong leaves a
    /// finding pointing at an id nothing can resolve — a silently empty
    /// evidence pane on exactly the cross-file changes that most need one.
    Unit(String, Box<UnitView>),
    Findings(Vec<ReviewFinding>),
    Done(Box<AnalysisStats>),
    Error(String),
}

struct App {
    findings: Vec<ReviewFinding>,
    /// Verdict per finding index. Absent means undecided.
    verdicts: HashMap<usize, Verdict>,
    units: HashMap<String, UnitView>,
    state: ListState,
    status: String,
    analyzing: bool,
    detail_scroll: u16,
    stats: Option<AnalysisStats>,
    min_severity: Severity,
    sha: String,
    project_root: PathBuf,
}

impl App {
    fn select(&mut self, delta: isize) {
        if self.findings.is_empty() {
            return;
        }
        let len = self.findings.len() as isize;
        let current = self.state.selected().unwrap_or(0) as isize;
        let next = (current + delta).rem_euclid(len);
        self.state.select(Some(next as usize));
        self.detail_scroll = 0;
    }

    fn selected(&self) -> Option<(usize, &ReviewFinding)> {
        let i = self.state.selected()?;
        self.findings.get(i).map(|f| (i, f))
    }

    fn evidence(&self, finding: &ReviewFinding) -> Option<&UnitView> {
        self.units.get(finding.unit_id.as_deref()?)
    }

    /// Record a verdict for the selected finding and advance.
    ///
    /// The verdict is written through to disk immediately rather than batched on
    /// quit: a reviewer who closes the terminal mid-triage should not lose the
    /// judgements they already made.
    fn judge(&mut self, verdict: Verdict) {
        let Some((index, finding)) = self.selected() else {
            return;
        };
        let finding = finding.clone();

        if let Err(e) = runs::record_verdict(&self.project_root, &self.sha, &finding, verdict) {
            self.status = format!("Could not record verdict: {e}");
            return;
        }
        self.verdicts.insert(index, verdict);

        self.status = match verdict {
            Verdict::Accepted => match copy_to_clipboard(&review_comment(&finding)) {
                Ok(()) => "Accepted — comment copied to clipboard".into(),
                // The verdict still counted; only the copy failed.
                Err(e) => format!("Accepted (clipboard unavailable: {e})"),
            },
            Verdict::Dismissed => "Dismissed".into(),
            Verdict::Wrong => "Marked wrong — this counts against the accept ratio".into(),
        };

        // Move on: triage is a queue, and stopping on a decided item invites
        // deciding it twice.
        self.select(1);
    }

    fn undecided(&self) -> usize {
        self.findings.len() - self.verdicts.len()
    }
}

/// A finding, shaped for pasting into a review thread.
fn review_comment(f: &ReviewFinding) -> String {
    let mut out = format!("`{}:{}` — {}", f.file, f.line, f.issue.trim());
    if !f.suggested_fix.trim().is_empty() {
        out.push_str(&format!("\n\nSuggested: {}", f.suggested_fix.trim()));
    }
    out
}

/// Copy via OSC 52, the terminal's own clipboard escape.
///
/// Chosen over a clipboard crate because it needs no dependency, no X11/Wayland
/// probing, and — the reason that matters — it works over SSH, where a reviewer
/// reading a remote branch would otherwise have nothing to paste from.
///
/// Not universal: tmux and screen need clipboard passthrough enabled, and a few
/// terminals refuse it outright. Failure is reported, never silent, so a
/// reviewer never believes they have copied something they have not.
fn copy_to_clipboard(text: &str) -> Result<()> {
    use std::io::Write;
    const OSC52_LIMIT: usize = 74_994; // Conservative; many terminals cap ~100 KB.

    let encoded = base64(text.as_bytes());
    if encoded.len() > OSC52_LIMIT {
        anyhow::bail!("too large for the terminal clipboard");
    }

    let mut out = io::stdout();
    write!(out, "\x1b]52;c;{encoded}\x07")?;
    out.flush()?;
    Ok(())
}

fn base64(data: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(TABLE[(n >> 18 & 63) as usize] as char);
        out.push(TABLE[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

pub fn run(
    diff: String,
    model_dir: PathBuf,
    project_root: PathBuf,
    settings: Settings,
    ticket: Option<String>,
    prefilter: PrefilterReport,
) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App {
        findings: Vec::new(),
        verdicts: HashMap::new(),
        units: HashMap::new(),
        state: ListState::default(),
        status: "Loading model…".into(),
        analyzing: false,
        detail_scroll: 0,
        stats: None,
        min_severity: settings.min_severity,
        sha: crate::git::head_sha().unwrap_or_else(|| "working-tree".into()),
        project_root: project_root.clone(),
    };

    let result = event_loop(
        &mut terminal,
        &mut app,
        diff,
        model_dir,
        project_root,
        settings,
        ticket,
        prefilter,
    );

    // Restore the terminal even if the loop failed — leaving a user in raw mode
    // with no echo is worse than any error message.
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    result
}

#[allow(clippy::too_many_arguments)]
fn event_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    diff: String,
    model_dir: PathBuf,
    project_root: PathBuf,
    settings: Settings,
    ticket: Option<String>,
    prefilter: PrefilterReport,
) -> Result<()>
where
    B::Error: Send + Sync + 'static,
{
    let spawn = |app: &mut App| -> Receiver<Msg> {
        app.analyzing = true;
        app.findings.clear();
        app.verdicts.clear();
        // A re-run re-derives its units; keeping the old ones would leave the
        // pane showing a hunk from the previous run for an id that no longer
        // exists in it.
        app.units.clear();
        app.stats = None;
        app.status = "Loading model…".into();

        let (tx, rx) = mpsc::channel();
        let (diff, model_dir, project_root, settings, ticket) = (
            diff.clone(),
            model_dir.clone(),
            project_root.clone(),
            settings.clone(),
            ticket.clone(),
        );
        let prefilter = prefilter.clone();

        // A dedicated OS thread: candle inference is CPU-bound and blocking, and
        // must not share a pool with the UI.
        std::thread::spawn(move || {
            if let Err(e) = analyze(
                &tx,
                diff,
                model_dir,
                project_root,
                settings,
                ticket,
                prefilter,
            ) {
                let _ = tx.send(Msg::Error(format!("{e:#}")));
            }
        });
        rx
    };

    // Start immediately. Waiting for a keypress spends the one thing the
    // reviewer came here to save.
    let mut receiver: Option<Receiver<Msg>> = Some(spawn(app));

    loop {
        terminal.draw(|f| draw(f, app))?;

        if let Some(rx) = &receiver {
            let mut disconnected = false;
            loop {
                match rx.try_recv() {
                    Ok(Msg::Progress(s)) => app.status = s,
                    Ok(Msg::Unit(id, view)) => {
                        app.units.insert(id, *view);
                    }
                    Ok(Msg::Findings(mut batch)) => {
                        batch.retain(|f| f.severity >= app.min_severity);
                        let was_empty = app.findings.is_empty();
                        app.findings.extend(batch);
                        if was_empty && !app.findings.is_empty() {
                            app.state.select(Some(0));
                        }
                    }
                    Ok(Msg::Done(stats)) => {
                        app.analyzing = false;
                        app.status = format!(
                            "{} finding{} — a accept · d dismiss · w wrong",
                            app.findings.len(),
                            if app.findings.len() == 1 { "" } else { "s" }
                        );
                        app.stats = Some(*stats);
                    }
                    Ok(Msg::Error(e)) => {
                        app.analyzing = false;
                        app.status = format!("Error: {e}");
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
            if disconnected {
                if app.analyzing {
                    app.analyzing = false;
                    app.status = "Analysis stopped unexpectedly".into();
                }
                receiver = None;
            }
        }

        if !event::poll(Duration::from_millis(80))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        // Windows reports both press and release; acting on both double-steps.
        if key.kind != KeyEventKind::Press {
            continue;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Down | KeyCode::Char('j') => app.select(1),
            KeyCode::Up | KeyCode::Char('k') => app.select(-1),
            KeyCode::PageDown => app.detail_scroll = app.detail_scroll.saturating_add(5),
            KeyCode::PageUp => app.detail_scroll = app.detail_scroll.saturating_sub(5),

            KeyCode::Char('a') if !app.analyzing => app.judge(Verdict::Accepted),
            KeyCode::Char('d') if !app.analyzing => app.judge(Verdict::Dismissed),
            KeyCode::Char('w') if !app.analyzing => app.judge(Verdict::Wrong),

            // `a` used to mean "analyze". It now means "accept", which is the
            // verb a reviewer reaches for constantly; re-running is rare.
            KeyCode::Char('r') if !app.analyzing => receiver = Some(spawn(app)),
            _ => {}
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn analyze(
    tx: &mpsc::Sender<Msg>,
    diff: String,
    model_dir: PathBuf,
    project_root: PathBuf,
    settings: Settings,
    ticket: Option<String>,
    prefilter: PrefilterReport,
) -> Result<()> {
    let backend = crate::build_backend(&settings.backend, &model_dir)?;
    let backend_label = backend.describe();
    let context_for = crate::context_builder(&project_root, 8000);

    // Loaded exactly as the CLI loads it, so the TUI cannot report a different
    // result than the same flags do outside it.
    let project = crate::ProjectRules::load(&project_root, settings.use_baseline);

    let mut analyzer =
        crate::build_analyzer(backend, &settings, &project_root, &diff, ticket, project);

    let _ = tx.send(Msg::Progress("Analyzing…".into()));

    let findings_tx = tx.clone();
    // Borrowed, not moved: `context_for` is handed to `analyze` at the same
    // time, and both uses are read-only.
    let context_ref = &context_for;
    let (summary, stats) = analyzer
        .analyze(
            &diff,
            &context_for,
            settings.max_tokens,
            |done, total, unit| {
                // Capture the evidence as the unit starts. The analyzer
                // guarantees this lands before any finding naming it, so the
                // detail pane is never asked for a hunk it has not been given.
                let _ = tx.send(Msg::Unit(
                    unit.id.clone(),
                    Box::new(UnitView {
                        context: context_ref(&unit.text),
                        text: unit.text.clone(),
                    }),
                ));
                let _ = tx.send(Msg::Progress(format!("Analyzing unit {done}/{total}…")));
            },
            move |batch| {
                let _ = findings_tx.send(Msg::Findings(batch.to_vec()));
            },
        )
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // File the run, exactly as the CLI path does — a review is a review however
    // it was launched, and `diffmind stats` must not have a hole in it.
    let record = runs::RunRecord::new(
        crate::git::head_sha().unwrap_or_else(|| "working-tree".into()),
        crate::git::current_branch(),
        backend_label.clone(),
        &summary,
        &stats,
        &prefilter,
    );
    let rendered = crate::output::markdown(&summary, &stats, &backend_label);
    let _ = runs::save(&project_root, &record, &rendered);

    let _ = tx.send(Msg::Done(Box::new(stats)));
    Ok(())
}

// ─── Rendering ───────────────────────────────────────────────────────────────

fn severity_color(s: Severity) -> Color {
    match s {
        Severity::High => Color::Red,
        Severity::Medium => Color::Yellow,
        Severity::Low => Color::Cyan,
    }
}

fn category_label(c: Category) -> &'static str {
    match c {
        Category::Security => "security",
        Category::Quality => "quality",
        Category::Performance => "perf",
        Category::Maintainability => "maint",
        Category::Compliance => "req",
    }
}

fn verdict_mark(v: Option<Verdict>) -> Span<'static> {
    match v {
        Some(Verdict::Accepted) => Span::styled("✓ ", Style::default().fg(Color::Green)),
        Some(Verdict::Dismissed) => Span::styled("· ", Style::default().fg(Color::DarkGray)),
        Some(Verdict::Wrong) => Span::styled("✗ ", Style::default().fg(Color::Magenta)),
        None => Span::raw("  "),
    }
}

fn draw(f: &mut Frame, app: &mut App) {
    let rows = Layout::default()
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
    f.render_widget(header, rows[0]);

    if app.findings.is_empty() {
        render_empty(f, rows[1], app);
    } else {
        render_findings(f, rows[1], app);
    }

    let footer = if app.findings.is_empty() {
        " [q] Quit  [r] Re-run ".to_string()
    } else {
        format!(
            " [j/k] Move  [a] Accept+copy  [d] Dismiss  [w] Wrong  [PgUp/PgDn] Scroll  [r] Re-run  [q] Quit  ·  {} left ",
            app.undecided()
        )
    };
    f.render_widget(
        Paragraph::new(footer).style(Style::default().fg(Color::DarkGray)),
        rows[2],
    );
}

fn render_empty(f: &mut Frame, area: Rect, app: &App) {
    let body = if app.analyzing {
        "\n  Analyzing…\n\n  Findings appear here as each unit completes."
    } else if app.stats.is_some() {
        "\n  No issues found.\n\n  Press 'r' to re-run."
    } else {
        "\n  Starting…"
    };

    let widget = Paragraph::new(body)
        .block(Block::default().borders(Borders::ALL).title("Review"))
        .style(Style::default().fg(Color::Cyan))
        .wrap(Wrap { trim: false });
    f.render_widget(widget, area);
}

fn render_findings(f: &mut Frame, area: Rect, app: &mut App) {
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
        .split(area);

    let items: Vec<ListItem> = app
        .findings
        .iter()
        .enumerate()
        .map(|(i, finding)| {
            let color = severity_color(finding.severity);
            let tag = match finding.severity {
                Severity::High => "HIGH",
                Severity::Medium => "MED ",
                Severity::Low => "LOW ",
            };
            // Show the tail of the path: the filename is what identifies a
            // finding, and a long prefix pushes it off the panel.
            let path = &finding.file;
            let shown = if path.chars().count() > 26 {
                format!(
                    "…{}",
                    path.chars()
                        .skip(path.chars().count() - 25)
                        .collect::<String>()
                )
            } else {
                path.clone()
            };
            ListItem::new(Line::from(vec![
                verdict_mark(app.verdicts.get(&i).copied()),
                Span::styled(
                    format!("{tag} "),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("{shown}:{}", finding.line)),
            ]))
        })
        .collect();

    let title = format!("Findings ({})", app.findings.len());
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("> ");
    f.render_stateful_widget(list, body[0], &mut app.state);

    let lines = match app.selected() {
        Some((index, finding)) => detail_lines(app, index, finding),
        None => vec![Line::from("Select a finding with j/k")],
    };

    f.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title("Detail"))
            .wrap(Wrap { trim: false })
            .scroll((app.detail_scroll, 0)),
        body[1],
    );
}

fn heading(text: &str, color: Color) -> Line<'static> {
    Line::from(Span::styled(
        text.to_string(),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    ))
}

fn detail_lines(app: &App, index: usize, finding: &ReviewFinding) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(vec![
            Span::styled("Severity  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                finding.severity.as_str().to_uppercase(),
                Style::default()
                    .fg(severity_color(finding.severity))
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        kv("Category", category_label(finding.category)),
        kv("Location", &format!("{}:{}", finding.file, finding.line)),
        kv("Rule", &finding.rule_id()),
        kv(
            "Confidence",
            &format!("{:.0}%", finding.confidence_or_default() * 100.0),
        ),
    ];

    if let Some(v) = app.verdicts.get(&index) {
        let (label, color) = match v {
            Verdict::Accepted => ("accepted", Color::Green),
            Verdict::Dismissed => ("dismissed", Color::DarkGray),
            Verdict::Wrong => ("marked wrong", Color::Magenta),
        };
        lines.push(Line::from(vec![
            Span::styled("Verdict   ", Style::default().fg(Color::DarkGray)),
            Span::styled(label.to_string(), Style::default().fg(color)),
        ]));
    }

    lines.push(Line::from(""));
    lines.push(heading("Issue", Color::Red));
    lines.extend(finding.issue.lines().map(|l| Line::from(l.to_string())));

    if !finding.suggested_fix.trim().is_empty() {
        lines.push(Line::from(""));
        lines.push(heading("Suggested fix", Color::Green));
        lines.extend(
            finding
                .suggested_fix
                .lines()
                .map(|l| Line::from(l.to_string())),
        );
    }

    // The evidence. A finding a reviewer cannot check is a finding they will
    // eventually stop reading.
    match app.evidence(finding) {
        Some(unit) => {
            lines.push(Line::from(""));
            lines.push(heading("The diff it reviewed", Color::Cyan));
            lines.extend(unit.text.lines().map(diff_line));

            if !unit.context.trim().is_empty() {
                lines.push(Line::from(""));
                lines.push(heading("Context it was given", Color::Blue));
                lines.extend(unit.context.lines().map(|l| {
                    Line::from(Span::styled(
                        l.to_string(),
                        Style::default().fg(Color::DarkGray),
                    ))
                }));
            }
        }
        None => {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "Found by a deterministic detector — no model, no prompt.",
                Style::default().fg(Color::DarkGray),
            )));
        }
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        format!(
            "Suppress with:  // diffmind-ignore-next-line {}",
            finding.rule_id()
        ),
        Style::default().fg(Color::DarkGray),
    )));

    lines
}

/// Colour a diff line the way a reviewer expects to read it.
fn diff_line(line: &str) -> Line<'static> {
    let color = match line.chars().next() {
        Some('+') if !line.starts_with("+++") => Color::Green,
        Some('-') if !line.starts_with("---") => Color::Red,
        Some('@') => Color::Cyan,
        _ => Color::Gray,
    };
    Line::from(Span::styled(line.to_string(), Style::default().fg(color)))
}

fn kv<'a>(key: &'a str, value: &str) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{key:<10}"), Style::default().fg(Color::DarkGray)),
        Span::raw(value.to_string()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(sev: Severity) -> ReviewFinding {
        ReviewFinding {
            file: "src/a.rs".into(),
            line: 1,
            severity: sev,
            category: Category::Quality,
            issue: "x".into(),
            suggested_fix: String::new(),
            confidence: None,
            rule: None,
            unit_id: None,
            rule_id: None,
        }
    }

    fn app(n: usize, name: &str) -> App {
        App {
            findings: (0..n).map(|_| finding(Severity::High)).collect(),
            verdicts: HashMap::new(),
            units: HashMap::new(),
            state: ListState::default(),
            status: String::new(),
            analyzing: false,
            detail_scroll: 0,
            stats: None,
            min_severity: Severity::Low,
            sha: "test".into(),
            // Per-test directory: these write real verdict files, and cargo
            // runs tests in parallel within one process.
            project_root: std::env::temp_dir()
                .join(format!("diffmind-tui-{name}-{}", std::process::id())),
        }
    }

    #[test]
    fn selection_wraps_in_both_directions() {
        let mut a = app(3, "wrap");
        a.state.select(Some(0));

        a.select(1);
        assert_eq!(a.state.selected(), Some(1));

        a.select(-1);
        a.select(-1);
        assert_eq!(
            a.state.selected(),
            Some(2),
            "moving up from 0 wraps to the end"
        );

        a.select(1);
        assert_eq!(
            a.state.selected(),
            Some(0),
            "moving down from the end wraps to 0"
        );
    }

    #[test]
    fn selection_is_a_noop_with_no_findings() {
        let mut a = app(0, "empty-select");
        a.select(1);
        assert_eq!(a.state.selected(), None, "must not index an empty list");
    }

    #[test]
    fn scrolling_the_detail_pane_never_underflows() {
        let mut a = app(1, "scroll");
        a.detail_scroll = 0;
        a.detail_scroll = a.detail_scroll.saturating_sub(5);
        assert_eq!(a.detail_scroll, 0);
    }

    #[test]
    fn changing_selection_resets_the_detail_scroll() {
        let mut a = app(2, "reset-scroll");
        a.state.select(Some(0));
        a.detail_scroll = 12;
        a.select(1);
        assert_eq!(
            a.detail_scroll, 0,
            "stale scroll would hide the new finding's text"
        );
    }

    #[test]
    fn a_verdict_is_persisted_and_advances_the_queue() {
        let mut a = app(3, "judge");
        a.state.select(Some(0));

        a.judge(Verdict::Wrong);

        assert_eq!(a.verdicts.get(&0), Some(&Verdict::Wrong));
        assert_eq!(
            a.state.selected(),
            Some(1),
            "stopping on a decided finding invites deciding it twice"
        );
        // Written through immediately, not batched until quit.
        let recorded = runs::load_verdicts(&a.project_root);
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].verdict, Verdict::Wrong);

        let _ = std::fs::remove_dir_all(&a.project_root);
    }

    #[test]
    fn judging_with_nothing_selected_is_a_noop() {
        let mut a = app(0, "noop-judge");
        a.judge(Verdict::Accepted);
        assert!(a.verdicts.is_empty());
        assert!(runs::load_verdicts(&a.project_root).is_empty());
    }

    #[test]
    fn undecided_counts_only_what_is_left() {
        let mut a = app(3, "undecided");
        a.state.select(Some(0));
        assert_eq!(a.undecided(), 3);
        a.judge(Verdict::Dismissed);
        assert_eq!(a.undecided(), 2);
        let _ = std::fs::remove_dir_all(&a.project_root);
    }

    #[test]
    fn a_review_comment_is_shaped_for_pasting() {
        let mut f = finding(Severity::High);
        f.file = "src/auth.ts".into();
        f.line = 11;
        f.issue = "Hardcoded secret.".into();
        f.suggested_fix = "Read it from the environment.".into();

        let comment = review_comment(&f);
        assert!(comment.starts_with("`src/auth.ts:11` — Hardcoded secret."));
        assert!(comment.contains("Suggested: Read it from the environment."));
    }

    #[test]
    fn a_comment_with_no_fix_has_no_dangling_heading() {
        let mut f = finding(Severity::Low);
        f.issue = "Just this.".into();
        f.suggested_fix = "   ".into();
        assert!(!review_comment(&f).contains("Suggested"));
    }

    #[test]
    fn base64_matches_the_reference_encoding() {
        // Clipboard copy is silent when it works, so a wrong encoder would only
        // show up as a paste of garbage.
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_handles_utf8_beyond_ascii() {
        assert_eq!(base64("é".as_bytes()), "w6k=");
        assert_eq!(base64("→".as_bytes()), "4oaS");
    }

    #[test]
    fn evidence_is_found_by_unit_id() {
        let mut a = app(1, "evidence");
        a.units.insert(
            "unit-1".into(),
            UnitView {
                text: "@@ -1 +1 @@\n+x".into(),
                context: "fn enclosing() {}".into(),
            },
        );

        let mut f = finding(Severity::High);
        assert!(
            a.evidence(&f).is_none(),
            "a detector finding has no unit and must not borrow another's hunk"
        );

        f.unit_id = Some("unit-1".into());
        assert_eq!(
            a.evidence(&f).map(|u| u.text.as_str()),
            Some("@@ -1 +1 @@\n+x")
        );
    }

    fn rendered(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_detail_pane_shows_the_hunk_and_the_context_behind_a_finding() {
        // A finding a reviewer cannot check is a finding they stop reading.
        let mut a = app(1, "detail");
        a.units.insert(
            "u1".into(),
            UnitView {
                text: "@@ -10,2 +10,2 @@\n-const token = null;\n+const token = \"sk-live\";".into(),
                context: "--- Enclosing function `auth` (src/auth.ts:1) ---".into(),
            },
        );
        a.findings[0].unit_id = Some("u1".into());
        a.findings[0].issue = "Hardcoded secret.".into();
        a.state.select(Some(0));

        let text = rendered(&detail_lines(&a, 0, &a.findings[0]));
        assert!(text.contains("Hardcoded secret."));
        assert!(text.contains("The diff it reviewed"));
        assert!(text.contains("+const token = \"sk-live\";"));
        assert!(text.contains("Context it was given"));
        assert!(text.contains("Enclosing function `auth`"));
        assert!(text.contains("diffmind-ignore-next-line"));
    }

    #[test]
    fn a_detector_finding_says_so_instead_of_showing_an_empty_evidence_pane() {
        let a = app(1, "detail-detector");
        let text = rendered(&detail_lines(&a, 0, &a.findings[0]));
        assert!(text.contains("deterministic detector"));
        assert!(!text.contains("The diff it reviewed"));
    }

    #[test]
    fn an_empty_context_omits_its_heading_rather_than_showing_a_blank_section() {
        let mut a = app(1, "detail-nocontext");
        a.units.insert(
            "u1".into(),
            UnitView {
                text: "@@ -1 +1 @@\n+x".into(),
                context: "   \n".into(),
            },
        );
        a.findings[0].unit_id = Some("u1".into());

        let text = rendered(&detail_lines(&a, 0, &a.findings[0]));
        assert!(text.contains("The diff it reviewed"));
        assert!(!text.contains("Context it was given"));
    }

    #[test]
    fn a_recorded_verdict_is_visible_in_the_detail_pane() {
        let mut a = app(1, "detail-verdict");
        a.verdicts.insert(0, Verdict::Wrong);
        let text = rendered(&detail_lines(&a, 0, &a.findings[0]));
        assert!(text.contains("marked wrong"));
    }

    #[test]
    fn diff_lines_are_coloured_without_mistaking_the_file_header() {
        // `+++ b/x` starts with '+' but is not an added line.
        let added = diff_line("+let a = 1;");
        let header = diff_line("+++ b/src/a.rs");
        assert_ne!(
            format!("{added:?}"),
            format!("{header:?}"),
            "the file header must not render as an addition"
        );
    }
}
