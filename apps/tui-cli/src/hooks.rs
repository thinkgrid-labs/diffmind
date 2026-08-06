//! `diffmind install-hooks` — one command instead of a README snippet nobody
//! copies correctly.

use anyhow::{Context, Result};
use std::path::Path;

const MARKER: &str = "# installed by diffmind";

pub fn supported() -> &'static [&'static str] {
    &["pre-push", "pre-commit"]
}

fn script(hook: &str, min_severity: &str) -> String {
    let body = match hook {
        "pre-commit" => format!(
            "# Review staged changes. Nothing is blocked below {min_severity} severity.\n\
             diffmind --staged --min-severity {min_severity} --fail-on {min_severity} || exit 1\n"
        ),
        // pre-push receives ref updates on stdin; diffmind reads the branch
        // itself, so the input is drained to avoid a SIGPIPE on the git side.
        _ => format!(
            "cat > /dev/null\n\n\
             # Review this branch against its base. Nothing is blocked below {min_severity} severity.\n\
             diffmind --min-severity {min_severity} --fail-on {min_severity} || exit 1\n"
        ),
    };

    format!(
        "#!/bin/sh\n\
         {MARKER} — regenerate with `diffmind install-hooks --hook {hook}`\n\
         # Remove this file to uninstall.\n\n\
         # A machine without diffmind installed must still be able to commit.\n\
         command -v diffmind >/dev/null 2>&1 || exit 0\n\n\
         {body}"
    )
}

pub fn install(git_dir: &Path, hook: &str, min_severity: &str, force: bool) -> Result<()> {
    if !supported().contains(&hook) {
        anyhow::bail!(
            "unsupported hook '{hook}'. Choose one of: {}",
            supported().join(", ")
        );
    }

    let hooks_dir = git_dir.join("hooks");
    std::fs::create_dir_all(&hooks_dir)
        .with_context(|| format!("could not create {}", hooks_dir.display()))?;
    let path = hooks_dir.join(hook);

    if path.exists() {
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        // Overwriting someone's hand-written hook without asking would be
        // destructive; ours is safe to replace because we wrote it.
        if !existing.contains(MARKER) && !force {
            anyhow::bail!(
                "{} already exists and was not created by diffmind.\n\
                 Inspect it, then re-run with --force to replace it.",
                path.display()
            );
        }
    }

    std::fs::write(&path, script(hook, min_severity))
        .with_context(|| format!("could not write {}", path.display()))?;
    make_executable(&path)?;

    println!("  ✓  installed {}", path.display());
    println!("     blocks on {min_severity} severity findings");
    println!("     uninstall: rm {}", path.display());
    Ok(())
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    // A hook without the executable bit is silently ignored by git, which looks
    // exactly like diffmind finding nothing.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_git(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("diffmind-hooks-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("hooks")).unwrap();
        d
    }

    #[test]
    fn installs_an_executable_hook() {
        let git = tmp_git("install");
        install(&git, "pre-push", "high", false).unwrap();
        let path = git.join("hooks/pre-push");
        let body = std::fs::read_to_string(&path).unwrap();

        assert!(body.starts_with("#!/bin/sh"));
        assert!(body.contains("--min-severity high"));
        assert!(
            body.contains("command -v diffmind"),
            "a teammate without diffmind must still be able to push"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "git ignores a non-executable hook");
        }

        let _ = std::fs::remove_dir_all(&git);
    }

    #[test]
    fn pre_commit_reviews_staged_changes() {
        let git = tmp_git("staged");
        install(&git, "pre-commit", "medium", false).unwrap();
        let body = std::fs::read_to_string(git.join("hooks/pre-commit")).unwrap();
        assert!(body.contains("--staged"));
        assert!(
            !body.contains("cat > /dev/null"),
            "only pre-push gets stdin"
        );
        let _ = std::fs::remove_dir_all(&git);
    }

    #[test]
    fn pre_push_drains_stdin() {
        let git = tmp_git("drain");
        install(&git, "pre-push", "high", false).unwrap();
        let body = std::fs::read_to_string(git.join("hooks/pre-push")).unwrap();
        assert!(
            body.contains("cat > /dev/null"),
            "git writes ref updates to the hook's stdin; not draining it risks SIGPIPE"
        );
        let _ = std::fs::remove_dir_all(&git);
    }

    #[test]
    fn refuses_to_clobber_a_foreign_hook() {
        let git = tmp_git("foreign");
        std::fs::write(git.join("hooks/pre-push"), "#!/bin/sh\necho mine\n").unwrap();

        let err = install(&git, "pre-push", "high", false).unwrap_err();
        assert!(err.to_string().contains("--force"));
        assert!(
            std::fs::read_to_string(git.join("hooks/pre-push"))
                .unwrap()
                .contains("echo mine"),
            "the user's hook must survive"
        );

        install(&git, "pre-push", "high", true).unwrap();
        assert!(
            std::fs::read_to_string(git.join("hooks/pre-push"))
                .unwrap()
                .contains(MARKER)
        );

        let _ = std::fs::remove_dir_all(&git);
    }

    #[test]
    fn reinstalling_our_own_hook_needs_no_force() {
        let git = tmp_git("reinstall");
        install(&git, "pre-push", "high", false).unwrap();
        install(&git, "pre-push", "medium", false).unwrap();
        let body = std::fs::read_to_string(git.join("hooks/pre-push")).unwrap();
        assert!(body.contains("--min-severity medium"));
        let _ = std::fs::remove_dir_all(&git);
    }

    #[test]
    fn rejects_an_unknown_hook_name() {
        let git = tmp_git("unknown");
        assert!(install(&git, "post-merge", "high", false).is_err());
        let _ = std::fs::remove_dir_all(&git);
    }
}
