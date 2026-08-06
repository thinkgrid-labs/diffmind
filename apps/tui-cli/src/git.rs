use anyhow::{Context, Result};
use std::process::Command;

/// Paths never worth spending inference on. Kept in one place so every diff
/// entry point (branch, last commit, staged) excludes the same things.
const EXCLUDES: &[&str] = &[
    ":!node_modules",
    ":!vendor",
    ":!target",
    ":!*-lock.json",
    ":!pnpm-lock.yaml",
    ":!package-lock.json",
    ":!yarn.lock",
    ":!Cargo.lock",
    ":!poetry.lock",
    ":!go.sum",
    ":!composer.lock",
    ":!dist",
    ":!build",
    ":!.next",
    ":!.cache",
    ":!*.map",
    ":!*.min.js",
    ":!*.min.css",
    ":!*.snap",
    ":!*.svg",
    ":!*.png",
    ":!*.jpg",
    ":!*.gif",
    ":!*.pdf",
    ":!*.woff",
    ":!*.woff2",
];

/// Refuse diffs larger than this. Past it, chunking alone would take longer
/// than a human review.
const MAX_DIFF_KB: usize = 1500;

fn git(args: &[&str]) -> Result<std::process::Output> {
    Command::new("git")
        .args(args)
        .output()
        .context("failed to run git — is it installed and on PATH?")
}

pub fn is_repo() -> bool {
    git(&["rev-parse", "--is-inside-work-tree"])
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Returns the name of the currently checked-out branch, or `None` if git
/// is unavailable or the repo is in a detached-HEAD state.
pub fn current_branch() -> Option<String> {
    let output = git(&["rev-parse", "--abbrev-ref", "HEAD"]).ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (branch != "HEAD").then_some(branch)
}

/// Work out what to diff against when the user did not say.
///
/// Hardcoding `main` failed outright on every repo that uses `master`,
/// `develop`, or `trunk`, with nothing but a raw git error to explain it.
/// Preference order: the remote's published HEAD, then a local branch that
/// actually exists, then `main` so the error message at least names something.
pub fn default_branch() -> String {
    // What `origin` says its HEAD is — authoritative when the ref is present.
    if let Ok(out) = git(&["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"])
        && out.status.success()
    {
        let full = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if let Some(name) = full.rsplit('/').next()
            && !name.is_empty()
        {
            return name.to_string();
        }
    }

    // Fall back to whichever conventional name resolves in this repo.
    for candidate in ["main", "master", "develop", "trunk"] {
        for reference in [
            format!("refs/heads/{candidate}"),
            format!("refs/remotes/origin/{candidate}"),
        ] {
            if git(&["rev-parse", "--verify", "--quiet", &reference])
                .map(|o| o.status.success())
                .unwrap_or(false)
            {
                return candidate.to_string();
            }
        }
    }

    "main".to_string()
}

/// True when `rev` resolves in this repository.
fn rev_exists(rev: &str) -> bool {
    git(&["rev-parse", "--verify", "--quiet", rev])
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn run_diff(range: Option<&str>, paths: &[String], cached: bool) -> Result<String> {
    let mut args: Vec<String> = vec!["diff".into()];
    if cached {
        args.push("--cached".into());
    }
    if let Some(r) = range {
        args.push(r.to_string());
    }
    args.push("--".into());
    args.extend(paths.iter().cloned());
    args.extend(EXCLUDES.iter().map(|s| s.to_string()));

    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    let output = git(&borrowed)?;

    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(anyhow::anyhow!("git error: {err}"));
    }

    let diff = String::from_utf8_lossy(&output.stdout).to_string();
    let size_kb = diff.len() / 1024;
    if size_kb > MAX_DIFF_KB {
        return Err(anyhow::anyhow!(
            "diff is too large to review ({size_kb} KB, limit {MAX_DIFF_KB} KB). \
             Review specific paths instead, e.g. `diffmind src/`."
        ));
    }
    Ok(diff)
}

/// Returns the diff for the most recent commit only.
pub fn get_last_commit_diff(paths: &[String]) -> Result<String> {
    // An initial commit has no parent, so `HEAD~1` does not resolve; diff
    // against the empty tree instead of failing with a cryptic git message.
    let range = if rev_exists("HEAD~1") {
        "HEAD~1..HEAD".to_string()
    } else {
        let empty_tree = git(&["hash-object", "-t", "tree", "/dev/null"])
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|| "4b825dc642cb6eb9a060e54bf8d69288fbee4904".into());
        format!("{empty_tree}..HEAD")
    };
    run_diff(Some(&range), paths, false)
}

/// Returns the diff of currently staged changes.
pub fn get_staged_diff(paths: &[String]) -> Result<String> {
    let diff = run_diff(None, paths, true)?;
    if diff.trim().is_empty() {
        return Err(anyhow::anyhow!(
            "no staged changes found. Run `git add <files>` first."
        ));
    }
    Ok(diff)
}

/// Returns the diff of this branch against `branch`.
pub fn get_diff(branch: &str, paths: &[String]) -> Result<String> {
    // Prefer the merge-base form so unrelated commits landing on the base
    // branch do not show up as this branch's work.
    let candidates = [
        format!("{branch}...HEAD"),
        format!("origin/{branch}...HEAD"),
    ];

    let resolved = candidates
        .iter()
        .find(|r| rev_exists(r.split("...").next().unwrap_or(branch)))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "base branch '{branch}' not found. Pass --branch <name>, or set \
                 review.branch in .diffmind/config.toml. Available: {}",
                local_branches().join(", ")
            )
        })?;

    run_diff(Some(resolved), paths, false)
}

fn local_branches() -> Vec<String> {
    git(&["branch", "--format=%(refname:short)"])
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .take(10)
                .collect()
        })
        .unwrap_or_default()
}

/// Diff of the working tree against HEAD — what `baseline create` reviews.
pub fn get_working_tree_diff(paths: &[String]) -> Result<String> {
    run_diff(Some("HEAD"), paths, false)
}

/// Absolute path to the repository root.
pub fn repo_root() -> Option<std::path::PathBuf> {
    let out = git(&["rev-parse", "--show-toplevel"]).ok()?;
    out.status
        .success()
        .then(|| std::path::PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string()))
}

/// Path to the `.git` directory, honouring worktrees and `GIT_DIR`.
pub fn git_dir() -> Option<std::path::PathBuf> {
    let out = git(&["rev-parse", "--git-dir"]).ok()?;
    out.status
        .success()
        .then(|| std::path::PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excludes_cover_the_usual_noise() {
        for pattern in [
            ":!node_modules",
            ":!pnpm-lock.yaml",
            ":!Cargo.lock",
            ":!*.min.js",
        ] {
            assert!(EXCLUDES.contains(&pattern), "{pattern} should be excluded");
        }
    }

    #[test]
    fn default_branch_always_returns_something_usable() {
        // Runs inside this repo (or none at all); either way the contract is
        // that a caller never receives an empty string to pass to git.
        let b = default_branch();
        assert!(!b.is_empty());
        assert!(!b.contains('/'), "should be a bare branch name, got {b}");
    }

    #[test]
    fn current_branch_never_reports_detached_head() {
        // `HEAD` is git's sentinel for detached state and is useless as a label.
        assert_ne!(current_branch().as_deref(), Some("HEAD"));
    }
}
