//! End-to-end over the real binary: a diff arrives on stdin, findings come out
//! of stdout, and the exit code reflects the gate.
//!
//! # Why this exists
//!
//! The `--stdin` path had no coverage at all, and it hid four separate copies of
//! the same defect — "where does a file start in a diff" was answered
//! independently in `parse_diff`, `prefilter`, `unit::split_files` and again for
//! path lines inside a hunk. Each had unit tests; none of them tested the four
//! together, so a diff piped from `diff -u` was parsed as one merged file with no
//! path, every finding was attributed to the wrong file, and the pre-filter
//! silently stopped filtering. Four unit tests written after the fact would not
//! have caught it either — only running the whole pipeline does.
//!
//! # Why the binary, and why a stub server
//!
//! Spawning the real executable is the only way to cover `cli::parse`, stdin
//! capture, project-root resolution and the exit code — none of which a library
//! test can reach.
//!
//! Inference is supplied by a stub HTTP endpoint rather than the bundled GGUF:
//! a 1.1 GB download is not a test dependency, and `--backend
//! openai-compatible` is a shipped, supported path, so the stub exercises real
//! code rather than a test-only seam. The stub answers each request with a
//! finding naming whichever file it can see in the prompt, which is precisely
//! the property that was broken.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

/// A multi-file diff with no `diff --git` lines — what `diff -u`,
/// `git format-patch` and most review tools produce.
const PLAIN_DIFF: &str = "\
--- a/src/one.rs
+++ b/src/one.rs
@@ -1,2 +1,2 @@
-let a = 1;
+let a = 2;
--- a/src/two.rs
+++ b/src/two.rs
@@ -10,2 +10,2 @@
-let b = 1;
+let b = 2;
";

// ─── Stub inference endpoint ─────────────────────────────────────────────────

struct Stub {
    port: u16,
    /// The prompt bodies the binary sent, so a test can assert on what the model
    /// was actually shown rather than only on what came back.
    prompts: Arc<Mutex<Vec<String>>>,
}

impl Stub {
    /// Serves at most `calls` requests, then stops. A bounded accept loop means
    /// the thread ends on its own and a hung test fails on the assertion rather
    /// than by timing out.
    fn spawn(calls: usize) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub");
        let port = listener.local_addr().expect("stub addr").port();
        let prompts = Arc::new(Mutex::new(Vec::new()));

        let seen = Arc::clone(&prompts);
        std::thread::spawn(move || {
            for _ in 0..calls {
                match listener.accept() {
                    Ok((stream, _)) => serve_one(stream, &seen),
                    Err(_) => return,
                }
            }
        });

        Stub { port, prompts }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn prompts(&self) -> Vec<String> {
        self.prompts.lock().expect("stub prompts").clone()
    }
}

fn serve_one(mut stream: TcpStream, seen: &Mutex<Vec<String>>) {
    let body = match read_http_body(&mut stream) {
        Some(b) => b,
        None => return,
    };
    seen.lock().expect("stub prompts").push(body.clone());

    // Answer about whichever file this request is actually about. A backend that
    // ignored the prompt and always named the same file would let the bug this
    // test exists for pass unnoticed.
    let path = ["src/one.rs", "src/two.rs"]
        .into_iter()
        .find(|p| body.contains(p))
        .unwrap_or("unknown");

    // Line 1 deliberately: it is a real changed line in `one.rs` but not in
    // `two.rs`, so anchoring has to snap the second finding to line 10. That
    // only works if the parser kept the two files' hunks apart.
    let review = serde_json::json!({
        "findings": [{
            "file": path,
            "line": 1,
            "severity": "high",
            "category": "quality",
            "issue": format!("something is wrong in {path}"),
            "suggested_fix": "fix it",
        }],
        "positives": [],
        "suggestions": [],
    })
    .to_string();

    let payload = serde_json::json!({
        "choices": [{ "message": { "content": review } }]
    })
    .to_string();

    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{payload}",
        payload.len()
    );
    let _ = stream.flush();
}

/// Enough HTTP to read one request: headers to the blank line, then
/// `Content-Length` bytes of body.
fn read_http_body(stream: &mut TcpStream) -> Option<String> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut length = 0usize;

    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse().ok()?;
        }
    }

    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).ok()?;
    String::from_utf8(body).ok()
}

// ─── Harness ─────────────────────────────────────────────────────────────────

struct Run {
    stdout: String,
    stderr: String,
    code: i32,
}

/// Run the real binary with `diff` on stdin, inside an isolated directory.
///
/// The working directory matters: `run()` anchors `.diffmind/` at the repository
/// root, so without this the test would write its cache and run history into
/// diffmind's own checkout. `DIFFMIND_HOME` is redirected for the same reason.
fn review_stdin(dir: &Path, diff: &str, extra: &[&str]) -> Run {
    let stub_args: Vec<&str> = extra.to_vec();
    let mut child = Command::new(env!("CARGO_BIN_EXE_diffmind"))
        .current_dir(dir)
        .env("DIFFMIND_HOME", dir)
        // Colour codes in the middle of a path would defeat every assertion.
        .env("NO_COLOR", "1")
        .args(["--stdin", "--no-daemon", "--no-index", "--no-cache"])
        .args(stub_args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn diffmind");

    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(diff.as_bytes())
        .expect("write diff to stdin");

    let out = child.wait_with_output().expect("wait for diffmind");
    Run {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        code: out.status.code().unwrap_or(-1),
    }
}

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("diffmind-e2e-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).expect("create temp dir");
    d
}

fn findings(stdout: &str) -> Vec<serde_json::Value> {
    let parsed: serde_json::Value = serde_json::from_str(stdout)
        .unwrap_or_else(|e| panic!("stdout was not JSON: {e}\n{stdout}"));
    parsed["findings"].as_array().cloned().unwrap_or_default()
}

// ─── Tests ───────────────────────────────────────────────────────────────────

/// The whole point. Two files in, two files out, each finding on its own file.
#[test]
fn a_piped_multi_file_diff_reports_each_file_separately() {
    let dir = tmpdir("multi-file");
    let stub = Stub::spawn(4);

    let run = review_stdin(
        &dir,
        PLAIN_DIFF,
        &[
            "--format",
            "json",
            "--backend",
            "openai-compatible",
            "--backend-model",
            "stub",
            "--backend-url",
            &stub.url(),
        ],
    );

    let found = findings(&run.stdout);
    let mut located: Vec<(String, u64)> = found
        .iter()
        .map(|f| {
            (
                f["file"].as_str().unwrap_or_default().to_string(),
                f["line"].as_u64().unwrap_or_default(),
            )
        })
        .collect();
    located.sort();

    assert_eq!(
        located,
        vec![
            ("src/one.rs".to_string(), 1),
            ("src/two.rs".to_string(), 10)
        ],
        "each file must get its own finding, anchored to its own changed line.\n\
         stdout: {}\nstderr: {}",
        run.stdout,
        run.stderr
    );

    // And the model was genuinely shown one file per request, not one merged
    // blob — the failure mode that made every finding land on the last path.
    let prompts = stub.prompts();
    assert_eq!(prompts.len(), 2, "one request per file");
    assert_eq!(
        prompts.iter().filter(|p| p.contains("src/one.rs")).count(),
        1,
        "src/one.rs should appear in exactly one prompt"
    );
    assert_eq!(
        prompts.iter().filter(|p| p.contains("src/two.rs")).count(),
        1
    );

    // Findings at or above the fail threshold mean exit 1, distinct from the
    // exit 2 that means diffmind could not run.
    assert_eq!(run.code, 1, "high findings should trip the gate");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Noise filtering is path-based, so it only works if the piped diff's paths
/// survive parsing. They did not: a lockfile arriving on stdin was reviewed.
#[test]
fn a_piped_lockfile_is_filtered_and_the_code_beside_it_is_not() {
    let dir = tmpdir("lockfile");
    let stub = Stub::spawn(2);

    let diff = "\
--- a/pnpm-lock.yaml
+++ b/pnpm-lock.yaml
@@ -1,2 +1,2 @@
-  integrity: sha512-aaa
+  integrity: sha512-bbb
--- a/src/two.rs
+++ b/src/two.rs
@@ -10,2 +10,2 @@
-let b = 1;
+let b = 2;
";

    let run = review_stdin(
        &dir,
        diff,
        &[
            "--format",
            "json",
            "--backend",
            "openai-compatible",
            "--backend-model",
            "stub",
            "--backend-url",
            &stub.url(),
        ],
    );

    let prompts = stub.prompts();
    assert_eq!(
        prompts.len(),
        1,
        "the lockfile must not cost an inference pass.\nstderr: {}",
        run.stderr
    );
    // Assert on the lockfile's *content*, not its path. When the header lines
    // were being swallowed as `-`/`+` content, the path string still turned up
    // in the merged prompt and a path-only assertion passed while the lockfile's
    // body was in fact being reviewed.
    assert!(
        !prompts[0].contains("sha512-"),
        "the lockfile's content reached the model:\n{}",
        prompts[0]
    );
    assert!(
        !prompts[0].contains("pnpm-lock.yaml"),
        "the lockfile reached the model:\n{}",
        prompts[0]
    );
    assert!(prompts[0].contains("src/two.rs"));

    let found = findings(&run.stdout);
    let files: Vec<&str> = found.iter().filter_map(|f| f["file"].as_str()).collect();
    assert_eq!(files, ["src/two.rs"]);

    let _ = std::fs::remove_dir_all(&dir);
}

/// A diff that is entirely noise must not be reported as a clean review, and
/// must not reach the backend at all — this path returns before a model is even
/// loaded, which is why it needs no stub.
#[test]
fn a_piped_diff_that_is_entirely_noise_says_so_and_costs_nothing() {
    let dir = tmpdir("all-noise");
    let diff = "\
--- a/pnpm-lock.yaml
+++ b/pnpm-lock.yaml
@@ -1,2 +1,2 @@
-  integrity: sha512-aaa
+  integrity: sha512-bbb
";

    // Pointed at a dead port on purpose. Reaching a backend at all is the
    // failure this asserts against, and a refused connection fails fast and
    // loudly — where omitting the backend entirely would fall through to the
    // local model and, on a machine that has one downloaded, quietly run a real
    // inference pass instead of failing.
    let run = review_stdin(
        &dir,
        diff,
        &[
            "--format",
            "json",
            "--backend",
            "openai-compatible",
            "--backend-model",
            "stub",
            "--backend-url",
            "http://127.0.0.1:1",
            "--backend-timeout",
            "5",
        ],
    );

    assert_eq!(run.code, 0, "nothing reviewable is not a failure");
    assert!(findings(&run.stdout).is_empty(), "stdout: {}", run.stdout);

    let _ = std::fs::remove_dir_all(&dir);
}

/// An empty diff on stdin is a passing review, not an error — a clean branch
/// piped into CI must exit 0.
#[test]
fn an_empty_diff_on_stdin_exits_zero() {
    let dir = tmpdir("empty");
    let run = review_stdin(&dir, "", &["--format", "json"]);

    assert_eq!(run.code, 0, "stderr: {}", run.stderr);
    assert!(findings(&run.stdout).is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

/// `--stdin` bypasses git entirely, so it has to work outside a repository —
/// which is the whole reason the flag exists.
#[test]
fn stdin_works_outside_a_git_repository() {
    let dir = tmpdir("no-repo");
    assert!(!dir.join(".git").exists());

    let stub = Stub::spawn(2);
    let run = review_stdin(
        &dir,
        "\
--- a/src/one.rs
+++ b/src/one.rs
@@ -1,2 +1,2 @@
-let a = 1;
+let a = 2;
",
        &[
            "--format",
            "json",
            "--backend",
            "openai-compatible",
            "--backend-model",
            "stub",
            "--backend-url",
            &stub.url(),
        ],
    );

    assert!(
        !run.stderr.contains("not inside a git repository"),
        "stderr: {}",
        run.stderr
    );
    assert_ne!(run.code, 2, "exit 2 means it could not run: {}", run.stderr);
    let found = findings(&run.stdout);
    let files: Vec<&str> = found.iter().filter_map(|f| f["file"].as_str()).collect();
    assert_eq!(files, ["src/one.rs"]);

    let _ = std::fs::remove_dir_all(&dir);
}

/// The gate is what CI reads. Reporting everything while failing only on `high`
/// must still exit 0 when nothing reaches the threshold.
#[test]
fn the_exit_code_follows_the_fail_threshold_not_the_finding_count() {
    let dir = tmpdir("gate");
    let stub = Stub::spawn(2);

    let run = review_stdin(
        &dir,
        "\
--- a/src/one.rs
+++ b/src/one.rs
@@ -1,2 +1,2 @@
-let a = 1;
+let a = 2;
",
        &[
            "--format",
            "json",
            "--min-severity",
            "low",
            // The stub reports `high`, so raising the gate above it is the only
            // way to be sure the exit code follows the gate and not the count.
            "--fail-on",
            "high",
            "--backend",
            "openai-compatible",
            "--backend-model",
            "stub",
            "--backend-url",
            &stub.url(),
        ],
    );

    assert_eq!(findings(&run.stdout).len(), 1, "the finding is reported");
    assert_eq!(run.code, 1, "and a high finding trips a high gate");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Agent config, ignore files and prose must not cost an inference pass — and
/// must not reach the model at all.
///
/// Reported from the field on 0.9.0: every file under `.claude/` came back as a
/// HIGH finding. Those files are imperative English about credentials, shell
/// commands and permissions, which is exactly what a reviewer prompt primed for
/// "exposed secrets, disabled auth" is looking for. The content assertions
/// matter more than the path ones here: a path can vanish from the prompt while
/// the body is still being reviewed under the previous file's header.
#[test]
fn agent_config_ignore_files_and_docs_never_reach_the_model() {
    let dir = tmpdir("toolconfig");
    let stub = Stub::spawn(2);

    let diff = "\
--- a/.claude/skills/deploy.md
+++ b/.claude/skills/deploy.md
@@ -1,2 +1,3 @@
 # Deploy
+Always export AWS_SECRET_ACCESS_KEY before deploying.
--- a/.gitignore
+++ b/.gitignore
@@ -1,2 +1,2 @@
-.env
+.env.local
--- a/.prettierrc.json
+++ b/.prettierrc.json
@@ -1,1 +1,1 @@
-{ \"semi\": true }
+{ \"semi\": false }
--- a/README.md
+++ b/README.md
@@ -1,1 +1,2 @@
 # Project
+Put your token in .env
--- a/src/two.rs
+++ b/src/two.rs
@@ -10,2 +10,2 @@
-let b = verify(token);
+let b = true;
";

    let run = review_stdin(
        &dir,
        diff,
        &[
            "--format",
            "json",
            "--backend",
            "openai-compatible",
            "--backend-model",
            "stub",
            "--backend-url",
            &stub.url(),
        ],
    );

    let prompts = stub.prompts();
    assert_eq!(
        prompts.len(),
        1,
        "only src/two.rs is reviewable.\nstderr: {}",
        run.stderr
    );

    for leaked in [
        "AWS_SECRET_ACCESS_KEY",
        ".claude",
        ".gitignore",
        ".env.local",
        ".prettierrc",
        "semi",
        "README.md",
        "Put your token",
    ] {
        assert!(
            !prompts[0].contains(leaked),
            "{leaked:?} reached the model:\n{}",
            prompts[0]
        );
    }
    assert!(prompts[0].contains("src/two.rs"));

    let found = findings(&run.stdout);
    let files: Vec<&str> = found.iter().filter_map(|f| f["file"].as_str()).collect();
    assert_eq!(files, ["src/two.rs"], "only the code file is reported");

    let _ = std::fs::remove_dir_all(&dir);
}

/// The opt-in exists for teams whose docs carry contracts. It reopens prose
/// only — agent config stays out regardless.
#[test]
fn include_docs_reopens_prose_but_not_agent_config() {
    let dir = tmpdir("includedocs");
    let stub = Stub::spawn(3);

    let diff = "\
--- a/.claude/skills/deploy.md
+++ b/.claude/skills/deploy.md
@@ -1,2 +1,3 @@
 # Deploy
+Always export AWS_SECRET_ACCESS_KEY before deploying.
--- a/docs/api.md
+++ b/docs/api.md
@@ -1,1 +1,2 @@
 # API
+POST /v1/charge is idempotent.
";

    let run = review_stdin(
        &dir,
        diff,
        &[
            "--include-docs",
            "--format",
            "json",
            "--backend",
            "openai-compatible",
            "--backend-model",
            "stub",
            "--backend-url",
            &stub.url(),
        ],
    );

    let prompts = stub.prompts();
    assert_eq!(
        prompts.len(),
        1,
        "docs are reviewed, agent config is not.\nstderr: {}",
        run.stderr
    );
    assert!(prompts[0].contains("docs/api.md"));
    assert!(
        !prompts[0].contains("AWS_SECRET_ACCESS_KEY"),
        "--include-docs must not reopen .claude/:\n{}",
        prompts[0]
    );

    let _ = std::fs::remove_dir_all(&dir);
}
