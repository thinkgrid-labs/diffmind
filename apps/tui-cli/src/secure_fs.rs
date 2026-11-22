//! Small filesystem primitives for files that hold secrets.
//!
//! Two files diffmind writes are credentials in all but name: the daemon's
//! bearer token (`~/.diffmind/daemon.json`) and the per-user review-cache
//! secret (`~/.diffmind/cache-secret`). Both must be private from the first
//! byte and must never be observed half-written.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Distinguishes temporary files written concurrently by threads of one
/// process; the pid alone does not.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A sibling temporary path unique to this process and call.
fn tmp_sibling(path: &Path) -> PathBuf {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    path.with_file_name(format!(".{name}.tmp.{}.{n}", std::process::id()))
}

/// Create `path` exclusively with owner-only permissions.
fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    // On Windows a new file under the user profile inherits the profile's
    // per-user ACL, which is the equivalent protection.
    opts.open(path)
}

/// Write `bytes` to `path` atomically, readable by the owner only.
///
/// The temporary file is created `0600` with `O_EXCL`, filled, flushed to disk
/// and renamed over `path`. A concurrent reader sees the old file or the new
/// one, never a prefix, and there is no moment at which the content exists with
/// looser permissions.
pub fn write_private_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = tmp_sibling(path);
    let result = (|| {
        let mut file = create_private(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        // Best effort: the temporary may not exist if creation itself failed.
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Fill a buffer from the operating system's CSPRNG.
///
/// There is deliberately no fallback. A token or key derived from anything
/// else (pid, clock, addresses) is guessable, and a guessable credential is
/// worse than a clear failure.
pub fn os_random<const N: usize>() -> std::io::Result<[u8; N]> {
    let mut buf = [0u8; N];
    getrandom::fill(&mut buf).map_err(|e| {
        std::io::Error::other(format!("the OS random number generator failed: {e}"))
    })?;
    Ok(buf)
}

/// Lower-case hex, for tokens that travel as JSON strings.
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        // Writing to a String cannot fail.
        let _ = write!(out, "{b:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("diffmind-securefs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[cfg(unix)]
    #[test]
    fn a_private_file_is_owner_only_even_under_a_permissive_umask() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmpdir("perms");
        let p = d.join("secret");
        write_private_atomic(&p, b"s3cret").unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode was {mode:o}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn an_atomic_write_replaces_the_previous_content_and_leaves_no_temporaries() {
        let d = tmpdir("replace");
        let p = d.join("info.json");
        write_private_atomic(&p, b"old").unwrap();
        write_private_atomic(&p, b"new").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"new");
        let leftovers: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "temporary files left behind");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn os_random_produces_distinct_full_width_values() {
        let a: [u8; 32] = os_random().unwrap();
        let b: [u8; 32] = os_random().unwrap();
        assert_ne!(a, b);
        assert_eq!(hex(&a).len(), 64);
        assert!(hex(&a).chars().all(|c| c.is_ascii_hexdigit()));
    }
}
