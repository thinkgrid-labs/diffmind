//! Interactive review browser.
//!
//! Rewritten around a worker thread and a channel. The previous version spawned
//! blocking candle inference onto a tokio worker (starving the runtime), held
//! the app mutex across `terminal.draw`, and quietly ignored `--device`,
//! `--min-severity`, `--max-tokens`, custom rules and the baseline — so the TUI
//! could report a different result than the same flags did in the CLI.

use anyhow::Result;
use core_engine::{AnalysisStats, Category, ReviewFinding, Severity};
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
    io,
    path::PathBuf,
    sync::mpsc::{self, Receiver, TryRecvError},
    time::Duration,
};

use crate::settings::Settings;

enum Msg {
    Progress(String),
    Findings(Vec<ReviewFinding>),
    Done(Box<AnalysisStats>),
    Error(String),
}

struct App {
    findings: Vec<ReviewFinding>,
    state: ListState,
    status: String,
    analyzing: bool,
    detail_scroll: u16,
    stats: Option<AnalysisStats>,
    min_severity: Severity,
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
}

pub fn run(
    diff: String,
    model_dir: PathBuf,
    project_root: PathBuf,
    settings: Settings,
    ticket: Option<String>,
) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App {
        findings: Vec::new(),
        state: ListState::default(),
        status: "Ready — press 'a' to analyze".into(),
        analyzing: false,
        detail_scroll: 0,
        stats: None,
        min_severity: settings.min_severity,
    };

    let result = event_loop(
        &mut terminal,
        &mut app,
        diff,
        model_dir,
        project_root,
        settings,
        ticket,
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

fn event_loop<B: Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
    diff: String,
    model_dir: PathBuf,
    project_root: PathBuf,
    settings: Settings,
    ticket: Option<String>,
) -> Result<()>
where
    B::Error: Send + Sync + 'static,
{
    let mut receiver: Option<Receiver<Msg>> = None;

    loop {
        terminal.draw(|f| draw(f, app))?;

        // Drain worker messages so findings appear as each chunk completes.
        if let Some(rx) = &receiver {
            let mut disconnected = false;
            loop {
                match rx.try_recv() {
                    Ok(Msg::Progress(s)) => app.status = s,
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
                            "Done — {} finding{}",
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
            KeyCode::Char('a') if !app.analyzing => {
                app.analyzing = true;
                app.findings.clear();
                app.stats = None;
                app.status = "Loading model...".into();

                let (tx, rx) = mpsc::channel();
                receiver = Some(rx);

                let (diff, model_dir, project_root, settings, ticket) = (
                    diff.clone(),
                    model_dir.clone(),
                    project_root.clone(),
                    settings.clone(),
                    ticket.clone(),
                );

                // A dedicated OS thread: candle inference is CPU-bound and
                // blocking, and must not share a pool with the UI.
                std::thread::spawn(move || {
                    if let Err(e) = analyze(&tx, diff, model_dir, project_root, settings, ticket) {
                        let _ = tx.send(Msg::Error(format!("{e:#}")));
                    }
                });
            }
            _ => {}
        }
    }
}

fn analyze(
    tx: &mpsc::Sender<Msg>,
    diff: String,
    model_dir: PathBuf,
    project_root: PathBuf,
    settings: Settings,
    ticket: Option<String>,
) -> Result<()> {
    // Every one of these was previously ignored by the TUI path.
    let backend = crate::build_backend(&settings.backend, &model_dir)?;
    let context = crate::build_context(&project_root, &diff, 8000);
    let custom_rules = crate::rules::load_custom_rules(&project_root);
    let baseline = if settings.use_baseline {
        std::fs::read_to_string(project_root.join(".diffmind").join("baseline.json"))
            .ok()
            .and_then(|raw| core_engine::Baseline::parse(&raw).ok())
    } else {
        None
    };

    let mut analyzer = crate::build_analyzer(
        backend,
        &settings,
        &project_root,
        &diff,
        ticket,
        custom_rules,
        baseline,
    );

    let _ = tx.send(Msg::Progress("Analyzing...".into()));

    let progress_tx = tx.clone();
    let findings_tx = tx.clone();
    let (summary, stats) = analyzer
        .analyze(
            &diff,
            &context,
            settings.max_tokens,
            move |done, total| {
                let _ =
                    progress_tx.send(Msg::Progress(format!("Analyzing chunk {done}/{total}...")));
            },
            move |batch| {
                let _ = findings_tx.send(Msg::Findings(batch.to_vec()));
            },
        )
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    // The streaming callback already delivered the findings; only the tail of
    // the summary is still needed.
    let _ = summary;
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

fn draw(f: &mut Frame, app: &mut App) {
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

    if app.findings.is_empty() {
        render_empty(f, chunks[1], app);
    } else {
        render_findings(f, chunks[1], app);
    }

    let footer = Paragraph::new(" [q] Quit  [a] Analyze  [j/k] Navigate  [PgUp/PgDn] Scroll ")
        .style(Style::default().fg(Color::DarkGray));
    f.render_widget(footer, chunks[2]);
}

fn render_empty(f: &mut Frame, area: Rect, app: &App) {
    let body = if app.analyzing {
        "\n  Analyzing…\n\n  Findings appear here as each chunk completes."
    } else if app.stats.is_some() {
        "\n  No issues found.\n\n  Press 'a' to re-run."
    } else {
        "\n  Welcome to diffmind\n\n  Press 'a' to run analysis\n  j / k to navigate findings\n  q to quit"
    };

    let widget = Paragraph::new(body)
        .block(Block::default().borders(Borders::ALL).title("Start"))
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
        .map(|finding| {
            let color = severity_color(finding.severity);
            let tag = match finding.severity {
                Severity::High => "HIGH",
                Severity::Medium => "MED ",
                Severity::Low => "LOW ",
            };
            // Show the tail of the path: the filename is what identifies a
            // finding, and a long prefix pushes it off the panel.
            let path = &finding.file;
            let shown = if path.chars().count() > 28 {
                format!(
                    "…{}",
                    path.chars()
                        .skip(path.chars().count() - 27)
                        .collect::<String>()
                )
            } else {
                path.clone()
            };
            ListItem::new(Line::from(vec![
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

    let detail = match app.state.selected().and_then(|i| app.findings.get(i)) {
        Some(finding) => {
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
                Line::from(""),
                Line::from(Span::styled(
                    "Issue",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                )),
            ];
            lines.extend(finding.issue.lines().map(|l| Line::from(l.to_string())));

            if !finding.suggested_fix.trim().is_empty() {
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "Suggested fix",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                )));
                lines.extend(
                    finding
                        .suggested_fix
                        .lines()
                        .map(|l| Line::from(l.to_string())),
                );
            }

            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!(
                    "Suppress with:  // diffmind-ignore-next-line {}",
                    finding.rule_id()
                ),
                Style::default().fg(Color::DarkGray),
            )));

            Paragraph::new(lines)
        }
        None => Paragraph::new("Select a finding with j/k"),
    };

    f.render_widget(
        detail
            .block(Block::default().borders(Borders::ALL).title("Details"))
            .wrap(Wrap { trim: false })
            .scroll((app.detail_scroll, 0)),
        body[1],
    );
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
            rule_id: None,
        }
    }

    fn app(n: usize) -> App {
        App {
            findings: (0..n).map(|_| finding(Severity::High)).collect(),
            state: ListState::default(),
            status: String::new(),
            analyzing: false,
            detail_scroll: 0,
            stats: None,
            min_severity: Severity::Low,
        }
    }

    #[test]
    fn selection_wraps_in_both_directions() {
        let mut a = app(3);
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
        let mut a = app(0);
        a.select(1);
        assert_eq!(a.state.selected(), None, "must not index an empty list");
    }

    #[test]
    fn scrolling_the_detail_pane_never_underflows() {
        let mut a = app(1);
        a.detail_scroll = 0;
        a.detail_scroll = a.detail_scroll.saturating_sub(5);
        assert_eq!(a.detail_scroll, 0);
    }

    #[test]
    fn changing_selection_resets_the_detail_scroll() {
        let mut a = app(2);
        a.state.select(Some(0));
        a.detail_scroll = 12;
        a.select(1);
        assert_eq!(
            a.detail_scroll, 0,
            "stale scroll would hide the new finding's text"
        );
    }
}
