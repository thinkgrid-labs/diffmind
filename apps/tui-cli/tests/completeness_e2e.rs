//! End-to-end exit-code contract for incomplete and failed reviews, over the
//! real binary and a scripted OpenAI-compatible stub (no model weights).
//!
//! | Exit | Meaning                                              |
//! | ---- | ---------------------------------------------------- |
//! | 0    | review completed, nothing at or above `--fail-on`    |
//! | 1    | review completed, blocking findings                  |
//! | 2    | diffmind failed, or the review is incomplete and the |
//! |      | run requires it to be complete                       |
//!
//! The defect these pin: before v0.9.3 a unit whose model output could not be
//! parsed was counted and otherwise ignored, so a CI gate reported "no
//! findings, exit 0" for code nobody reviewed.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const DIFF: &str = "\
diff --git a/src/one.rs b/src/one.rs
--- a/src/one.rs
+++ b/src/one.rs
@@ -1,2 +1,2 @@
-let a = 1;
+let a = 2;
diff --git a/src/two.rs b/src/two.rs
--- a/src/two.rs
+++ b/src/two.rs
@@ -10,2 +10,2 @@
-let b = 1;
+let b = 2;
";

/// What the stub says to each request, in order; the last entry repeats.
#[derive(Clone)]
enum Say {
    /// A well-formed review with no findings.
    Clean,
    /// A review with one high finding on whichever file the prompt is about.
    High,
    /// 200 OK whose content is prose, not a review.
    Prose,
    /// An HTTP error status.
    Status(u16),
}

fn stub(script: Vec<Say>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for (i, conn) in listener.incoming().enumerate().take(64) {
            let Ok(mut stream) = conn else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut length = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                if line == "\r\n" {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; length];
            let _ = reader.read_exact(&mut body);
            let body = String::from_utf8_lossy(&body).into_owned();
            let file = if body.contains("src/two.rs") {
                "src/two.rs"
            } else {
                "src/one.rs"
            };

            let say = script
                .get(i)
                .or(script.last())
                .cloned()
                .unwrap_or(Say::Clean);
            let (status, content) = match say {
                Say::Clean => (
                    200,
                    r#"{"findings":[],"positives":[],"suggestions":[]}"#.to_string(),
                ),
                Say::High => (
                    200,
                    serde_json::json!({"findings": [{
                        "file": file, "line": 1, "severity": "high", "category": "quality",
                        "issue": "bad", "suggested_fix": "fix"
                    }]})
                    .to_string(),
                ),
                Say::Prose => (200, "I am unable to review this code.".to_string()),
                Say::Status(code) => (code, String::new()),
            };
            let payload = if status == 200 {
                serde_json::json!({"choices": [{"message": {"content": content}}]}).to_string()
            } else {
                r#"{"error":{"message":"stub failure"}}"#.to_string()
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
        }
    });
    format!("http://127.0.0.1:{port}")
}

struct Run {
    stdout: String,
    stderr: String,
    code: i32,
}

fn run(dir: &Path, url: &str, extra: &[&str]) -> Run {
    let mut child = Command::new(env!("CARGO_BIN_EXE_diffmind"))
        .current_dir(dir)
        .env("DIFFMIND_HOME", dir)
        .env("NO_COLOR", "1")
        // Each test states its completeness policy; the host CI must not.
        .env_remove("CI")
        .args([
            "--stdin",
            "--no-daemon",
            "--no-index",
            "--no-cache",
            "--triage",
            "off",
        ])
        .args(["--backend", "openai-compatible", "--backend-url", url])
        .args(["--backend-model", "stub", "--backend-timeout", "10"])
        .args(["--fail-on", "high"])
        .args(if extra.contains(&"--format") {
            &[][..]
        } else {
            &["--format", "json"][..]
        })
        .args(extra)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(DIFF.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    Run {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        code: out.status.code().unwrap_or(-1),
    }
}

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("diffmind-e2e-c-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn json(r: &Run) -> serde_json::Value {
    serde_json::from_str(&r.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout not JSON ({e}):\n{}\nstderr:\n{}",
            r.stdout, r.stderr
        )
    })
}

#[test]
fn a_complete_clean_review_exits_0_and_says_complete() {
    let d = tmpdir("clean");
    let r = run(&d, &stub(vec![Say::Clean]), &["--require-complete"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(json(&r)["completeness"]["status"], "complete");
    assert_eq!(json(&r)["completeness"]["trustworthy"], true);
}

#[test]
fn a_complete_review_with_blocking_findings_exits_1() {
    let d = tmpdir("blocking");
    let r = run(&d, &stub(vec![Say::High]), &["--require-complete"]);
    assert_eq!(r.code, 1, "{}", r.stderr);
}

#[test]
fn an_unparseable_unit_fails_a_strict_gate_with_exit_2() {
    let d = tmpdir("strict");
    let r = run(
        &d,
        &stub(vec![Say::Clean, Say::Prose]),
        &["--require-complete"],
    );
    assert_eq!(r.code, 2, "stderr:\n{}", r.stderr);
    let v = json(&r);
    assert_eq!(v["completeness"]["status"], "incomplete");
    assert_eq!(v["completeness"]["units_skipped"], 1);
    assert_eq!(v["completeness"]["skipped"][0]["reason"], "unparseable");
    assert!(r.stderr.contains("review incomplete"), "{}", r.stderr);
}

/// `CI` set and no flag: strict by default.
#[test]
fn ci_environments_are_strict_by_default() {
    let d = tmpdir("ci-default");
    let url = stub(vec![Say::Clean, Say::Prose]);
    let mut child = Command::new(env!("CARGO_BIN_EXE_diffmind"))
        .current_dir(&d)
        .env("DIFFMIND_HOME", &d)
        .env("CI", "true")
        .args([
            "--stdin",
            "--no-daemon",
            "--no-index",
            "--no-cache",
            "--triage",
            "off",
        ])
        .args(["--backend", "openai-compatible", "--backend-url", &url])
        .args(["--backend-model", "stub", "--format", "json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(DIFF.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn allow_incomplete_reports_but_lets_the_gate_decide() {
    let d = tmpdir("lenient");
    let r = run(
        &d,
        &stub(vec![Say::Clean, Say::Prose]),
        &["--allow-incomplete"],
    );
    assert_eq!(r.code, 0, "{}", r.stderr);
    let v = json(&r);
    assert_eq!(v["completeness"]["status"], "incomplete");
    assert_eq!(v["completeness"]["trustworthy"], false);
    assert_eq!(v["completeness"]["required"], false);
}

/// A repository's config cannot relax a gate the command line made strict —
/// the property the GitHub Action relies on for pull requests.
#[test]
fn repository_config_cannot_relax_require_complete() {
    let d = tmpdir("config-relax");
    std::fs::create_dir_all(d.join(".diffmind")).unwrap();
    std::fs::write(
        d.join(".diffmind/config.toml"),
        "[review]\nrequire_complete = false\n",
    )
    .unwrap();
    let r = run(
        &d,
        &stub(vec![Say::Clean, Say::Prose]),
        &["--require-complete"],
    );
    assert_eq!(r.code, 2, "{}", r.stderr);
}

/// The server dies part-way. Exit 2 whatever the gate settings — and the
/// report is still written, with the deterministic findings and the record of
/// what was not reviewed, instead of a bare error.
#[test]
fn a_backend_failure_exits_2_and_still_writes_the_report() {
    let d = tmpdir("backend-fail");
    let report = d.join("report.json");
    let r = run(
        &d,
        &stub(vec![Say::Clean, Say::Status(400)]),
        &["--allow-incomplete", "--output", report.to_str().unwrap()],
    );
    assert_eq!(r.code, 2, "stderr:\n{}", r.stderr);
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&report).expect("report written")).unwrap();
    assert_eq!(v["completeness"]["status"], "failed");
    assert!(
        v["completeness"]["backend_error"]
            .as_str()
            .unwrap_or("")
            .contains("HTTP 400"),
        "{v}"
    );
    assert_eq!(v["completeness"]["skipped_backend_failed"], 1);
}

#[test]
fn sarif_marks_an_incomplete_run_as_not_successful() {
    let d = tmpdir("sarif");
    let url = stub(vec![Say::Clean, Say::Prose]);
    let r = run(&d, &url, &["--allow-incomplete", "--format", "sarif"]);
    assert_eq!(r.code, 0, "{}", r.stderr);
    let v = json(&r);
    assert_eq!(
        v["runs"][0]["invocations"][0]["executionSuccessful"], false,
        "{v}"
    );
    assert_eq!(
        v["runs"][0]["properties"]["completeness"]["status"],
        "incomplete"
    );
}

#[test]
fn the_help_output_documents_the_completeness_flags() {
    let out = Command::new(env!("CARGO_BIN_EXE_diffmind"))
        .arg("--help")
        .output()
        .unwrap();
    let help = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success());
    assert!(help.contains("--require-complete") && help.contains("--allow-incomplete"));
}
