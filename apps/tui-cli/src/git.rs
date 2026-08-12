use anyhow::{Context, Result};
use std::process::Command;

/// Directories that are never worth even *reading*: dependency trees and build
/// output, where a single `git diff` can run to hundreds of megabytes.
///
/// This list used to also carry lockfiles, minified bundles, snapshots and
/// binary assets. Those moved to `core_engine::prefilter`, because a pathspec
/// exclusion is invisible: the file never enters the diff, so there is no way
/// to tell the reviewer *what* was skipped. "312 hunks → 74 reviewable
/// (238 filtered: lockfiles, generated, formatting)" is the line that makes the
/// filtering trustworthy, and it can only be produced in process.
///
/// What stays here is what we would not want to pay to read, ever.
const EXCLUDES: &[&str] = &[
    ":!node_modules",
    ":!vendor",
    ":!target",
    ":!dist",
    ":!build",
    ":!.next",
    ":!.cache",
];

/// Hard ceiling on the *raw* diff, before filtering. Only a backstop against
/// pathological input eating memory — the reviewable-size limit is applied to
/// the post-filter diff by the caller, since a 40 MB lockfile change should
/// leave a perfectly reviewable branch behind.
const MAX_RAW_DIFF_MB: usize = 64;

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
    let size_mb = diff.len() / (1024 * 1024);
    if size_mb > MAX_RAW_DIFF_MB {
        return Err(anyhow::anyhow!(
            "diff is too large to process ({size_mb} MB, limit {MAX_RAW_DIFF_MB} MB). \
             Review specific paths instead, e.g. `diffmind src/`."
        ));
    }
    Ok(diff)
}

/// Repo-relative paths git marks `linguist-generated` in `.gitattributes`.
///
/// Asked in one batch: `check-attr` is a process spawn, and a 40-file diff
/// should not cost 40 of them.
pub fn linguist_generated(paths: &[String]) -> std::collections::HashSet<String> {
    let mut generated = std::collections::HashSet::new();
    if paths.is_empty() {
        return generated;
    }

    let mut args: Vec<&str> = vec!["check-attr", "linguist-generated", "--"];
    args.extend(paths.iter().map(String::as_str));

    let Ok(output) = git(&args) else {
        return generated;
    };
    if !output.status.success() {
        return generated;
    }

    // `<path>: linguist-generated: set` — the path may itself contain ": ",
    // so split from the right.
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Some((path, value)) = line.rsplit_once(": ") else {
            continue;
        };
        if value.trim() != "set" {
            continue;
        }
        if let Some((path, _)) = path.rsplit_once(": ") {
            generated.insert(path.to_string());
        }
    }
    generated
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

/// Split a `a..b` / `a...b` range into its endpoints, with git's own defaulting
/// of an omitted side to `HEAD`. Returns `None` when this is not a range at all.
fn range_endpoints(range: &str) -> Option<(String, String)> {
    let (left, right) = match range.split_once("...") {
        Some(pair) => pair,
        None => range.split_once("..")?,
    };
    // A third dot, or any further `..`, is not a range git will accept.
    if right.contains("..") {
        return None;
    }
    let side = |s: &str| if s.is_empty() { "HEAD" } else { s }.to_string();
    Some((side(left), side(right)))
}

/// Does `candidate` name a diff range that resolves in this repository?
///
/// Used to tell `diffmind main...HEAD` from `diffmind src/auth/` without making
/// the user say which they meant. Deliberately strict: both endpoints must
/// resolve, so a relative path like `../lib/x.rs` is never mistaken for a range.
pub fn looks_like_range(candidate: &str) -> bool {
    // An existing path always wins. A file really named `a..b` is odd but real,
    // and silently reviewing something else would be worse than odd.
    if std::path::Path::new(candidate).exists() {
        return false;
    }
    match range_endpoints(candidate) {
        Some((a, b)) => rev_exists(&a) && rev_exists(&b),
        None => false,
    }
}

/// Returns the diff for an explicit `a..b` or `a...b` range.
pub fn get_range_diff(range: &str, paths: &[String]) -> Result<String> {
    // `git diff` takes the range positionally, so a value beginning with `-`
    // would be read as an option rather than a revision.
    if range.starts_with('-') {
        anyhow::bail!("'{range}' is not a valid revision range");
    }

    let Some((left, right)) = range_endpoints(range) else {
        anyhow::bail!(
            "'{range}' is not a revision range. Use `a..b` (or `a...b` for changes \
             since the branches diverged)."
        );
    };
    for rev in [&left, &right] {
        if !rev_exists(rev) {
            anyhow::bail!("revision '{rev}' not found in this repository");
        }
    }

    run_diff(Some(range), paths, false)
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

/// Short SHA of HEAD, or `None` on an unborn branch.
pub fn head_sha() -> Option<String> {
    let out = git(&["rev-parse", "--short", "HEAD"]).ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
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
    fn excludes_cover_only_the_never_read_directories() {
        for pattern in [":!node_modules", ":!target", ":!dist"] {
            assert!(EXCLUDES.contains(&pattern), "{pattern} should be excluded");
        }
        // Lockfiles and friends must reach the diff so the pre-filter can drop
        // them *and say so*. Excluding them here would make the count a lie.
        for pattern in [":!pnpm-lock.yaml", ":!Cargo.lock", ":!*.min.js", ":!*.svg"] {
            assert!(
                !EXCLUDES.contains(&pattern),
                "{pattern} belongs in the pre-filter, where it can be counted"
            );
        }
    }

    #[test]
    fn linguist_generated_is_empty_rather_than_failing_without_paths() {
        assert!(linguist_generated(&[]).is_empty());
    }

    #[test]
    fn range_endpoints_handles_both_dot_forms() {
        assert_eq!(
            range_endpoints("v1.0..HEAD"),
            Some(("v1.0".into(), "HEAD".into()))
        );
        assert_eq!(
            range_endpoints("main...HEAD"),
            Some(("main".into(), "HEAD".into()))
        );
    }

    #[test]
    fn an_omitted_side_defaults_to_head_as_git_does() {
        assert_eq!(
            range_endpoints("main.."),
            Some(("main".into(), "HEAD".into()))
        );
        assert_eq!(
            range_endpoints("..main"),
            Some(("HEAD".into(), "main".into()))
        );
    }

    #[test]
    fn a_plain_revision_is_not_a_range() {
        assert_eq!(range_endpoints("HEAD"), None);
        assert_eq!(range_endpoints("v1.2.0"), None);
        assert_eq!(range_endpoints("src/auth"), None);
    }

    #[test]
    fn a_relative_path_is_not_mistaken_for_a_range() {
        // `../lib/x.rs` splits on `..` but neither side is a revision, and the
        // path may well exist. Reviewing a different set of changes than the
        // user named would be a silent, confusing failure.
        assert!(!looks_like_range("../lib/x.rs"));
        assert!(!looks_like_range("./src"));
        assert!(!looks_like_range("a/../b"));
    }

    #[test]
    fn a_range_with_extra_dots_is_rejected() {
        assert_eq!(range_endpoints("a..b..c"), None);
    }

    #[test]
    fn a_range_beginning_with_a_dash_is_refused() {
        // `git diff` takes the range positionally, so this would otherwise be
        // read as a git option rather than a revision.
        let err = get_range_diff("--upload-pack=x..HEAD", &[]).unwrap_err();
        assert!(err.to_string().contains("not a valid revision range"));
    }

    #[test]
    fn a_non_range_argument_is_reported_as_such() {
        let err = get_range_diff("HEAD", &[]).unwrap_err();
        assert!(err.to_string().contains("not a revision range"), "{err}");
    }

    #[test]
    fn an_unknown_revision_names_the_side_that_failed() {
        let err = get_range_diff("definitely-not-a-ref..HEAD", &[]).unwrap_err();
        assert!(
            err.to_string().contains("definitely-not-a-ref"),
            "the message should say which end is wrong: {err}"
        );
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
