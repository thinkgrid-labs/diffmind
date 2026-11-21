//! The code graph: what is defined where, and what refers to it.
//!
//! Two layers. `extract` turns source text into definitions and references with
//! tree-sitter, and knows nothing about storage. `store` keeps them in SQLite
//! and answers the questions a reviewer has — what encloses this line, what does
//! this name refer to, and above all **what calls this**, which the regex index
//! it replaces could never answer at all.

pub mod extract;
pub mod link;
pub mod store;

pub use link::link_related;
pub use store::{Def, Graph};

use std::path::{Component, Path};

/// Largest file context assembly will read. Definitions and test files are
/// quoted by their first few dozen lines; nothing legitimate needs more.
const MAX_CONTEXT_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// Read `relative` from inside `project_root`, or `None`.
///
/// Context is sent to the model, and with a remote backend off the machine, so
/// what may be read is exactly "files of this repository". A diff path or a
/// conventional test-file name is not trusted to mean that:
///
/// - an absolute path, or one with `..`, is refused outright;
/// - a symlink is followed only if it resolves inside the repository. A branch
///   can commit `src/auth.test.ts -> /home/me/.aws/credentials`; without this
///   check the "test file for src/auth.ts" was that file, quoted into the
///   prompt where planted instructions could ask the model to repeat it;
/// - files over [`MAX_CONTEXT_FILE_BYTES`] are skipped rather than loaded.
pub fn read_repo_file(project_root: &Path, relative: &str) -> Option<String> {
    let rel = Path::new(relative);
    if rel.is_absolute()
        || rel
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return None;
    }
    let root = project_root.canonicalize().ok()?;
    let full = root.join(rel).canonicalize().ok()?;
    if !full.starts_with(&root) {
        return None;
    }
    let meta = std::fs::metadata(&full).ok()?;
    if !meta.is_file() || meta.len() > MAX_CONTEXT_FILE_BYTES {
        return None;
    }
    std::fs::read_to_string(&full).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(name: &str) -> std::path::PathBuf {
        let d =
            std::env::temp_dir().join(format!("diffmind-readrepo-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("src")).unwrap();
        d
    }

    #[test]
    fn ordinary_files_inside_the_repository_are_read() {
        let r = root("ok");
        std::fs::write(r.join("src/a.rs"), "fn a() {}").unwrap();
        assert_eq!(read_repo_file(&r, "src/a.rs").as_deref(), Some("fn a() {}"));
        assert_eq!(
            read_repo_file(&r, "./src/a.rs").as_deref(),
            Some("fn a() {}")
        );
        let _ = std::fs::remove_dir_all(&r);
    }

    #[test]
    fn traversal_and_absolute_paths_are_refused() {
        let r = root("traversal");
        let outside = r
            .parent()
            .unwrap()
            .join(format!("outside-{}", std::process::id()));
        std::fs::write(&outside, "SECRET").unwrap();
        let name = outside.file_name().unwrap().to_string_lossy().to_string();
        assert!(read_repo_file(&r, &format!("../{name}")).is_none());
        assert!(read_repo_file(&r, &format!("src/../../{name}")).is_none());
        assert!(read_repo_file(&r, outside.to_str().unwrap()).is_none());
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&r);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_repository_is_not_followed() {
        let r = root("symlink");
        let outside = r
            .parent()
            .unwrap()
            .join(format!("creds-{}", std::process::id()));
        std::fs::write(&outside, "aws_secret_access_key=SECRET").unwrap();
        std::os::unix::fs::symlink(&outside, r.join("src/auth.test.ts")).unwrap();
        assert!(read_repo_file(&r, "src/auth.test.ts").is_none());

        // A link that stays inside the repository is fine.
        std::fs::write(r.join("src/real.ts"), "ok").unwrap();
        std::os::unix::fs::symlink(r.join("src/real.ts"), r.join("src/alias.ts")).unwrap();
        assert_eq!(read_repo_file(&r, "src/alias.ts").as_deref(), Some("ok"));
        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&r);
    }

    #[test]
    fn a_huge_file_is_skipped() {
        let r = root("huge");
        let f = std::fs::File::create(r.join("src/big.rs")).unwrap();
        f.set_len(MAX_CONTEXT_FILE_BYTES + 1).unwrap();
        assert!(read_repo_file(&r, "src/big.rs").is_none());
        let _ = std::fs::remove_dir_all(&r);
    }
}
