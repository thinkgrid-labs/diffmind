//! Persisted review history — `.diffmind/runs/`.
//!
//! Two stores with different jobs.
//!
//! `runs/<sha>/` holds the last run for a commit: the findings, and what the run
//! cost. It is a snapshot, overwritten when the same sha is reviewed again.
//!
//! `runs/verdicts.jsonl` is append-only and never overwritten. It is the corpus
//! that answers the only question that decides whether this tool is worth
//! running: **how often is a finding worth acting on?** A single accept-to-wrong
//! ratio over months of real reviews is worth more than any amount of reasoning
//! about prompt wording, and it cannot be reconstructed after the fact — which
//! is why the log exists before the keys that write to it.
//!
//! Both are local notes. Nothing here is meant to be committed; `ensure_gitignore`
//! sees to that.

use anyhow::Result;
use core_engine::{AnalysisStats, PrefilterReport, ReviewFinding, ReviewSummary};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Bumped if the on-disk shape changes, so a stale record is skipped rather
/// than half-deserialized into a misleading statistic.
const RUN_VERSION: u32 = 1;

pub fn runs_dir(project_root: &Path) -> PathBuf {
    project_root.join(".diffmind").join("runs")
}

fn verdict_log(project_root: &Path) -> PathBuf {
    runs_dir(project_root).join("verdicts.jsonl")
}

// ─── Run records ─────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub struct RunRecord {
    pub version: u32,
    /// Commit the review was taken against. `working-tree` when HEAD is unborn.
    pub sha: String,
    pub branch: Option<String>,
    pub created_at: String,
    pub backend: String,
    pub diffmind_version: String,
    pub cost: RunCost,
    pub filtered: FilterCounts,
    pub findings: Vec<ReviewFinding>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct RunCost {
    pub units_total: usize,
    pub units_cached: usize,
    pub inference_ms: u64,
    pub prompt_tokens: usize,
    pub completion_tokens: usize,
    /// Whether the token counts came from a real tokenizer.
    pub tokens_estimated: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct FilterCounts {
    pub hunks_total: usize,
    pub hunks_reviewed: usize,
}

impl RunRecord {
    pub fn new(
        sha: String,
        branch: Option<String>,
        backend: String,
        summary: &ReviewSummary,
        stats: &AnalysisStats,
        prefilter: &PrefilterReport,
    ) -> Self {
        RunRecord {
            version: RUN_VERSION,
            sha,
            branch,
            created_at: chrono::Utc::now().to_rfc3339(),
            backend,
            diffmind_version: crate::output::VERSION.to_string(),
            cost: RunCost {
                units_total: stats.units_total,
                units_cached: stats.units_cached,
                inference_ms: stats.inference_ms,
                prompt_tokens: stats.prompt_tokens,
                completion_tokens: stats.completion_tokens,
                tokens_estimated: stats.tokens_estimated,
            },
            filtered: FilterCounts {
                hunks_total: prefilter.hunks_total,
                hunks_reviewed: prefilter.hunks_kept,
            },
            findings: summary.findings.clone(),
        }
    }
}

/// Persist a run. Failure is reported but never fatal — a review that found real
/// problems must still be reported even if the notes could not be filed.
pub fn save(project_root: &Path, record: &RunRecord, rendered_markdown: &str) -> Result<PathBuf> {
    let dir = runs_dir(project_root).join(&record.sha);
    std::fs::create_dir_all(&dir)?;
    ensure_gitignore(project_root);

    std::fs::write(dir.join("run.json"), serde_json::to_string_pretty(record)?)?;
    std::fs::write(dir.join("review.md"), rendered_markdown)?;
    Ok(dir)
}

pub fn load_all(project_root: &Path) -> Vec<RunRecord> {
    let dir = runs_dir(project_root);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };

    let mut runs: Vec<RunRecord> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| std::fs::read_to_string(e.path().join("run.json")).ok())
        .filter_map(|raw| serde_json::from_str::<RunRecord>(&raw).ok())
        .filter(|r| r.version == RUN_VERSION)
        .collect();

    runs.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    runs
}

// ─── Verdicts ────────────────────────────────────────────────────────────────

/// What the reviewer did with a finding. The whole point of the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// Worth saying to the author.
    Accepted,
    /// Read, not worth raising. Neither a success nor a failure.
    Dismissed,
    /// The finding was incorrect. This is the number that must stay small.
    Wrong,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerdictEntry {
    pub at: String,
    pub sha: String,
    pub verdict: Verdict,
    /// Line-independent identity, so the same finding recurring after unrelated
    /// edits above it is recognisably the same finding.
    pub fingerprint: String,
    pub rule_id: String,
    pub file: String,
}

/// Append one verdict. Append-only and line-delimited so two concurrent runs
/// cannot corrupt each other's history, and so a truncated write costs one line
/// rather than the file.
pub fn record_verdict(
    project_root: &Path,
    sha: &str,
    finding: &ReviewFinding,
    verdict: Verdict,
) -> Result<()> {
    use std::io::Write;

    let path = verdict_log(project_root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    ensure_gitignore(project_root);

    let entry = VerdictEntry {
        at: chrono::Utc::now().to_rfc3339(),
        sha: sha.to_string(),
        verdict,
        fingerprint: finding.fingerprint(),
        rule_id: finding.rule_id(),
        file: finding.file.clone(),
    };

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    writeln!(file, "{}", serde_json::to_string(&entry)?)?;
    Ok(())
}

pub fn load_verdicts(project_root: &Path) -> Vec<VerdictEntry> {
    let Ok(raw) = std::fs::read_to_string(verdict_log(project_root)) else {
        return Vec::new();
    };
    raw.lines()
        // A partially-written final line is survivable; skip it rather than
        // discard a month of history.
        .filter_map(|l| serde_json::from_str::<VerdictEntry>(l).ok())
        .collect()
}

// ─── Aggregate ───────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct Summary {
    pub runs: usize,
    pub first_run: Option<String>,
    pub last_run: Option<String>,
    pub accepted: usize,
    pub dismissed: usize,
    pub wrong: usize,
    pub median_findings: usize,
    pub median_seconds: f64,
    pub median_tokens: usize,
    pub cache_hit_rate: f64,
    /// Rule IDs producing the most `wrong` verdicts — the things to mute or fix.
    pub worst_rules: Vec<(String, usize)>,
}

impl Summary {
    /// The single most important number: how often a finding was worth raising
    /// versus how often it was simply incorrect. Dismissals are excluded — a
    /// reviewer choosing not to raise a correct observation is not a failure.
    pub fn accept_to_wrong(&self) -> Option<f64> {
        (self.wrong > 0).then(|| self.accepted as f64 / self.wrong as f64)
    }
}

pub fn summarize(project_root: &Path) -> Summary {
    let runs = load_all(project_root);
    let verdicts = load_verdicts(project_root);

    let mut s = Summary {
        runs: runs.len(),
        first_run: runs.first().map(|r| r.created_at.clone()),
        last_run: runs.last().map(|r| r.created_at.clone()),
        ..Summary::default()
    };

    for v in &verdicts {
        match v.verdict {
            Verdict::Accepted => s.accepted += 1,
            Verdict::Dismissed => s.dismissed += 1,
            Verdict::Wrong => s.wrong += 1,
        }
    }

    let mut by_rule: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for v in verdicts.iter().filter(|v| v.verdict == Verdict::Wrong) {
        *by_rule.entry(v.rule_id.as_str()).or_default() += 1;
    }
    let mut worst: Vec<(String, usize)> = by_rule
        .into_iter()
        .map(|(k, n)| (k.to_string(), n))
        .collect();
    worst.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    worst.truncate(5);
    s.worst_rules = worst;

    if runs.is_empty() {
        return s;
    }

    s.median_findings = median(runs.iter().map(|r| r.findings.len()).collect());
    s.median_tokens = median(
        runs.iter()
            .map(|r| r.cost.prompt_tokens + r.cost.completion_tokens)
            .collect(),
    );
    s.median_seconds = median(runs.iter().map(|r| r.cost.inference_ms).collect()) as f64 / 1000.0;

    let (cached, total) = runs.iter().fold((0usize, 0usize), |(c, t), r| {
        (c + r.cost.units_cached, t + r.cost.units_total)
    });
    s.cache_hit_rate = if total > 0 {
        cached as f64 / total as f64
    } else {
        0.0
    };

    s
}

fn median<T: Ord + Copy + Default>(mut values: Vec<T>) -> T {
    if values.is_empty() {
        return T::default();
    }
    values.sort_unstable();
    values[values.len() / 2]
}

// ─── Housekeeping ────────────────────────────────────────────────────────────

/// Keep diffmind's own generated files out of git without touching the
/// repository's root `.gitignore`.
///
/// `rules/`, `rules.toml`, `config.toml` and `baseline.json` are deliberately
/// *not* listed: a team's review standards and accepted baseline belong in the
/// repo. Runs and verdicts are a reviewer's private notes and do not.
pub fn ensure_gitignore(project_root: &Path) {
    let path = project_root.join(".diffmind").join(".gitignore");
    if path.exists() {
        return;
    }
    let body = "\
# Written by diffmind. Generated state — not worth committing.
# Deliberately absent: rules/, rules.toml, config.toml, baseline.json.
cache/
runs/
models/
graph.db
graph.db-wal
graph.db-shm
symbols.json
daemon.json
";
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, body);
}

/// Delete recorded runs. Verdicts survive: they are the accumulated judgement,
/// and are worth far more than the snapshots.
pub fn clear_runs(project_root: &Path) -> Result<usize> {
    let dir = runs_dir(project_root);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(0);
    };
    let mut removed = 0;
    for entry in entries.filter_map(|e| e.ok()) {
        if entry.path().is_dir() {
            std::fs::remove_dir_all(entry.path())?;
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_engine::{Category, Severity};

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("diffmind-runs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn finding(issue: &str, rule: &str) -> ReviewFinding {
        ReviewFinding {
            file: "src/a.rs".into(),
            line: 1,
            severity: Severity::High,
            category: Category::Quality,
            issue: issue.into(),
            suggested_fix: String::new(),
            confidence: None,
            rule: None,
            unit_id: None,
            rule_id: Some(rule.into()),
        }
    }

    fn record(sha: &str, findings: Vec<ReviewFinding>, ms: u64, cached: usize) -> RunRecord {
        let summary = ReviewSummary {
            findings,
            ..Default::default()
        };
        let stats = AnalysisStats {
            units_total: 4,
            units_cached: cached,
            inference_ms: ms,
            prompt_tokens: 100,
            completion_tokens: 50,
            ..Default::default()
        };
        RunRecord::new(
            sha.into(),
            Some("feat/x".into()),
            "test-backend".into(),
            &summary,
            &stats,
            &PrefilterReport::default(),
        )
    }

    #[test]
    fn a_run_round_trips() {
        let dir = tmpdir("roundtrip");
        let rec = record("abc123", vec![finding("boom", "DM001")], 1500, 1);
        save(&dir, &rec, "# review").unwrap();

        let loaded = load_all(&dir);
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].sha, "abc123");
        assert_eq!(loaded[0].findings.len(), 1);
        assert_eq!(loaded[0].cost.inference_ms, 1500);
        assert!(dir.join(".diffmind/runs/abc123/review.md").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn re_reviewing_a_sha_replaces_its_snapshot() {
        let dir = tmpdir("replace");
        save(
            &dir,
            &record("abc", vec![finding("first", "DM001")], 10, 0),
            "",
        )
        .unwrap();
        save(
            &dir,
            &record("abc", vec![finding("second", "DM001")], 20, 0),
            "",
        )
        .unwrap();

        let loaded = load_all(&dir);
        assert_eq!(loaded.len(), 1, "one directory per sha");
        assert_eq!(loaded[0].findings[0].issue, "second");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verdicts_accumulate_across_runs_rather_than_being_replaced() {
        // The accept:wrong ratio is only meaningful over months. Losing history
        // when a sha is re-reviewed would make the number unanswerable.
        let dir = tmpdir("verdicts");
        let f = finding("boom", "DM900.quality");

        record_verdict(&dir, "sha1", &f, Verdict::Accepted).unwrap();
        record_verdict(&dir, "sha1", &f, Verdict::Wrong).unwrap();
        record_verdict(&dir, "sha2", &f, Verdict::Accepted).unwrap();

        let all = load_verdicts(&dir);
        assert_eq!(all.len(), 3);

        let s = summarize(&dir);
        assert_eq!(s.accepted, 2);
        assert_eq!(s.wrong, 1);
        assert_eq!(s.accept_to_wrong(), Some(2.0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_truncated_final_line_does_not_discard_the_history() {
        use std::io::Write;
        let dir = tmpdir("truncated");
        let f = finding("boom", "DM001");
        record_verdict(&dir, "sha1", &f, Verdict::Accepted).unwrap();

        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(verdict_log(&dir))
            .unwrap();
        write!(file, "{{\"at\":\"2026-01-01\",\"sha\":\"tru").unwrap();
        drop(file);

        assert_eq!(load_verdicts(&dir).len(), 1, "the good line must survive");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dismissals_do_not_count_against_the_ratio() {
        // Choosing not to raise a correct observation is not the tool being
        // wrong, and folding it in would understate the signal.
        let dir = tmpdir("dismiss");
        let f = finding("boom", "DM001");
        record_verdict(&dir, "s", &f, Verdict::Accepted).unwrap();
        for _ in 0..10 {
            record_verdict(&dir, "s", &f, Verdict::Dismissed).unwrap();
        }
        let s = summarize(&dir);
        assert_eq!(s.dismissed, 10);
        assert_eq!(s.accept_to_wrong(), None, "no wrong verdicts, no ratio");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_worst_rules_are_ranked_by_how_often_they_are_wrong() {
        let dir = tmpdir("worst");
        for _ in 0..3 {
            record_verdict(
                &dir,
                "s",
                &finding("a", "DM900.maintainability"),
                Verdict::Wrong,
            )
            .unwrap();
        }
        record_verdict(&dir, "s", &finding("b", "DM001"), Verdict::Wrong).unwrap();
        record_verdict(&dir, "s", &finding("c", "DM002"), Verdict::Accepted).unwrap();

        let s = summarize(&dir);
        assert_eq!(s.worst_rules[0], ("DM900.maintainability".into(), 3));
        assert_eq!(s.worst_rules[1], ("DM001".into(), 1));
        assert!(
            !s.worst_rules.iter().any(|(id, _)| id == "DM002"),
            "an accepted finding is not evidence against its rule"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn medians_and_cache_rate_come_from_every_run() {
        let dir = tmpdir("medians");
        save(&dir, &record("a", vec![], 1000, 0), "").unwrap();
        save(&dir, &record("b", vec![finding("x", "DM001")], 3000, 4), "").unwrap();
        save(&dir, &record("c", vec![], 2000, 2), "").unwrap();

        let s = summarize(&dir);
        assert_eq!(s.runs, 3);
        assert_eq!(s.median_seconds, 2.0);
        // 6 of 12 units served from cache.
        assert!((s.cache_hit_rate - 0.5).abs() < 1e-9);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_store_summarises_to_zero_rather_than_panicking() {
        let dir = tmpdir("empty");
        let s = summarize(&dir);
        assert_eq!(s.runs, 0);
        assert_eq!(s.accept_to_wrong(), None);
        assert_eq!(s.median_seconds, 0.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_gitignore_protects_notes_but_not_standards() {
        let dir = tmpdir("gitignore");
        ensure_gitignore(&dir);
        let body = std::fs::read_to_string(dir.join(".diffmind/.gitignore")).unwrap();

        for private in ["runs/", "cache/", "symbols.json"] {
            assert!(body.contains(private), "{private} should be ignored");
        }
        for shared in ["rules/", "baseline.json", "config.toml"] {
            assert!(
                !body.lines().any(|l| l.trim() == shared),
                "{shared} belongs in the repository and must not be ignored"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_existing_gitignore_is_never_overwritten() {
        let dir = tmpdir("gitignore-keep");
        std::fs::create_dir_all(dir.join(".diffmind")).unwrap();
        std::fs::write(dir.join(".diffmind/.gitignore"), "# mine\n").unwrap();
        ensure_gitignore(&dir);
        assert_eq!(
            std::fs::read_to_string(dir.join(".diffmind/.gitignore")).unwrap(),
            "# mine\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clearing_runs_keeps_the_verdict_history() {
        let dir = tmpdir("clear");
        save(&dir, &record("a", vec![], 10, 0), "").unwrap();
        record_verdict(&dir, "a", &finding("x", "DM001"), Verdict::Accepted).unwrap();

        assert_eq!(clear_runs(&dir).unwrap(), 1);
        assert!(load_all(&dir).is_empty());
        assert_eq!(
            load_verdicts(&dir).len(),
            1,
            "accumulated judgement is worth more than the snapshots"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
