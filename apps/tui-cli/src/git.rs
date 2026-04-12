use anyhow::{Context, Result};
use std::process::Command;

/// Returns the name of the currently checked-out branch, or `None` if git
/// is unavailable or the repo is in a detached-HEAD state.
pub fn current_branch() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if output.status.success() {
        let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
        // "HEAD" means detached state — not useful to show
        if branch == "HEAD" {
            None
        } else {
            Some(branch)
        }
    } else {
        None
    }
}

/// Returns the diff for the most recent commit only (HEAD~1..HEAD).
pub fn get_last_commit_diff(paths: &[String]) -> Result<String> {
    let mut args = vec!["diff", "HEAD~1..HEAD", "--"];
    for path in paths {
        args.push(path);
    }
    args.extend_from_slice(&[
        ":!node_modules",
        ":!*-lock.json",
        ":!pnpm-lock.yaml",
        ":!package-lock.json",
        ":!yarn.lock",
        ":!dist",
        ":!build",
        ":!.next",
        ":!.cache",
        ":!*.map",
        ":!*.min.js",
        ":!*.min.css",
    ]);

    let output = Command::new("git")
        .args(&args)
        .output()
        .context("Failed to execute git command")?;

    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!("Git error: {}", err));
    }

    let diff = String::from_utf8_lossy(&output.stdout).to_string();
    let size_kb = diff.len() / 1024;
    if size_kb > 1500 {
        return Err(anyhow::anyhow!(
            "Diff too large to process ({}KB). Try reviewing specific files.",
            size_kb
        ));
    }
    Ok(diff)
}

pub fn get_diff(branch: &str, paths: &[String]) -> Result<String> {
    let branch_arg = format!("{}...HEAD", branch);
    let mut args = vec!["diff", &branch_arg, "--"];
    for path in paths {
        args.push(path);
    }

    args.extend_from_slice(&[
        ":!node_modules",
        ":!*-lock.json",
        ":!pnpm-lock.yaml",
        ":!package-lock.json",
        ":!yarn.lock",
        ":!dist",
        ":!build",
        ":!.next",
        ":!.cache",
        ":!*.map",
        ":!*.min.js",
        ":!*.min.css",
    ]);

    let output = Command::new("git")
        .args(&args)
        .output()
        .context("Failed to execute git command")?;

    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!("Git error: {}", err));
    }

    let diff = String::from_utf8_lossy(&output.stdout).to_string();

    let size_kb = diff.len() / 1024;
    if size_kb > 1500 {
        return Err(anyhow::anyhow!(
            "Diff too large to process ({}KB). Try reviewing specific files.",
            size_kb
        ));
    }

    Ok(diff)
}
