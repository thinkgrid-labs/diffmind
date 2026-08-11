//! Content-addressed cache of per-chunk review results.
//!
//! Re-running after touching one file should not re-infer the other nine.
//! Keyed on everything that can change the answer, so a stale hit is not
//! possible without a hash collision.

use crate::types::ReviewSummary;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize)]
struct CacheEntry {
    /// Guards against a format change silently deserializing into garbage.
    version: u32,
    summary: ReviewSummary,
}

const ENTRY_VERSION: u32 = 1;

pub struct ReviewCache {
    dir: PathBuf,
}

/// Everything that can change a chunk's review result. Missing a field here
/// means serving a cached answer the current settings would never produce.
pub struct CacheKeyInput<'a> {
    pub backend: &'a str,
    pub prompt_version: u32,
    pub chunk: &'a str,
    pub context: &'a str,
    pub languages: &'a str,
    /// Identity of the prose rule sets in force. Without it, editing a rulebook
    /// would serve findings produced by the previous wording — the edit would
    /// appear to do nothing at all.
    pub rulebook_digest: &'a str,
    pub requirements: &'a str,
    pub max_tokens: u32,
    pub temperature: f64,
    pub seed: u64,
}

impl ReviewCache {
    pub fn new(dir: PathBuf) -> Self {
        ReviewCache { dir }
    }

    /// Open a cache rooted at `<base>/cache`, creating it if needed.
    /// Returns `None` when the directory cannot be created — a cache is an
    /// optimisation, never a reason to fail a review.
    pub fn open(base: &Path) -> Option<Self> {
        let dir = base.join("cache");
        std::fs::create_dir_all(&dir).ok()?;
        Some(ReviewCache { dir })
    }

    pub fn key(input: &CacheKeyInput<'_>) -> String {
        let mut h = Sha256::new();
        for part in [
            input.backend,
            input.chunk,
            input.context,
            input.languages,
            input.rulebook_digest,
            input.requirements,
        ] {
            h.update(part.as_bytes());
            h.update(b"\x00");
        }
        h.update(input.prompt_version.to_le_bytes());
        h.update(input.max_tokens.to_le_bytes());
        h.update(input.temperature.to_le_bytes());
        h.update(input.seed.to_le_bytes());
        format!("{:x}", h.finalize())[..32].to_string()
    }

    fn path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }

    pub fn get(&self, key: &str) -> Option<ReviewSummary> {
        let raw = std::fs::read_to_string(self.path(key)).ok()?;
        let entry: CacheEntry = serde_json::from_str(&raw).ok()?;
        (entry.version == ENTRY_VERSION).then_some(entry.summary)
    }

    pub fn put(&self, key: &str, summary: &ReviewSummary) {
        let entry = CacheEntry {
            version: ENTRY_VERSION,
            summary: summary.clone(),
        };
        let Ok(json) = serde_json::to_string(&entry) else {
            return;
        };
        // Write-then-rename so a concurrent reader never sees a half-written
        // entry (two `diffmind` runs in one repo is normal).
        let tmp = self.path(&format!("{key}.tmp{}", std::process::id()));
        if std::fs::write(&tmp, json).is_ok() && std::fs::rename(&tmp, self.path(key)).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// Drop the oldest entries once the cache exceeds `max_entries`.
    pub fn prune(&self, max_entries: usize) {
        let Ok(read) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let mut entries: Vec<(std::time::SystemTime, PathBuf)> = read
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter_map(|e| {
                let modified = e.metadata().ok()?.modified().ok()?;
                Some((modified, e.path()))
            })
            .collect();

        if entries.len() <= max_entries {
            return;
        }
        entries.sort_by_key(|(t, _)| *t);
        for (_, path) in entries.iter().take(entries.len() - max_entries) {
            let _ = std::fs::remove_file(path);
        }
    }

    pub fn clear(&self) -> std::io::Result<()> {
        for entry in std::fs::read_dir(&self.dir)?.flatten() {
            if entry.path().extension().is_some_and(|x| x == "json") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Category, ReviewFinding, Severity};

    fn input<'a>(chunk: &'a str, backend: &'a str) -> CacheKeyInput<'a> {
        CacheKeyInput {
            backend,
            prompt_version: 1,
            chunk,
            context: "",
            languages: "Rust",
            rulebook_digest: "",
            requirements: "",
            max_tokens: 1024,
            temperature: 0.0,
            seed: 7,
        }
    }

    fn summary() -> ReviewSummary {
        ReviewSummary {
            findings: vec![ReviewFinding {
                file: "a.rs".into(),
                line: 1,
                severity: Severity::High,
                category: Category::Security,
                issue: "boom".into(),
                suggested_fix: "fix".into(),
                confidence: Some(0.9),
                rule_id: Some("DM001".into()),
                rule: None,
                unit_id: None,
            }],
            positives: vec!["nice".into()],
            suggestions: vec![],
        }
    }

    #[test]
    fn round_trips_a_summary() {
        let dir = std::env::temp_dir().join(format!("diffmind-cache-{}", std::process::id()));
        let cache = ReviewCache::open(&dir).unwrap();
        let key = ReviewCache::key(&input("diff", "m"));

        assert!(cache.get(&key).is_none());
        cache.put(&key, &summary());
        let got = cache.get(&key).expect("cached entry should be readable");
        assert_eq!(got.findings.len(), 1);
        assert_eq!(got.findings[0].issue, "boom");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_input_field_changes_the_key() {
        let base = ReviewCache::key(&input("diff", "m"));
        assert_ne!(base, ReviewCache::key(&input("other diff", "m")));
        assert_ne!(base, ReviewCache::key(&input("diff", "other-model")));

        let mut k = input("diff", "m");
        k.prompt_version = 2;
        assert_ne!(base, ReviewCache::key(&k), "a prompt edit must invalidate");

        let mut k = input("diff", "m");
        k.temperature = 0.7;
        assert_ne!(base, ReviewCache::key(&k));

        let mut k = input("diff", "m");
        k.requirements = "ticket text";
        assert_ne!(base, ReviewCache::key(&k));

        let mut k = input("diff", "m");
        k.seed = 8;
        assert_ne!(base, ReviewCache::key(&k));

        // Editing a rulebook must invalidate, or the edit appears to do nothing.
        let mut k = input("diff", "m");
        k.rulebook_digest = "abc123";
        assert_ne!(base, ReviewCache::key(&k));
    }

    #[test]
    fn key_is_stable_for_identical_input() {
        assert_eq!(
            ReviewCache::key(&input("diff", "m")),
            ReviewCache::key(&input("diff", "m"))
        );
    }

    #[test]
    fn prune_keeps_the_newest_entries() {
        let dir = std::env::temp_dir().join(format!("diffmind-prune-{}", std::process::id()));
        let cache = ReviewCache::open(&dir).unwrap();
        for i in 0..5 {
            cache.put(&format!("key{i}"), &summary());
            // Distinct mtimes; the filesystem's resolution can be coarse.
            std::thread::sleep(std::time::Duration::from_millis(12));
        }
        cache.prune(2);
        // `open` roots the cache at <base>/cache, so count entries there.
        let remaining = std::fs::read_dir(dir.join("cache"))
            .unwrap()
            .flatten()
            .count();
        assert_eq!(remaining, 2);
        assert!(cache.get("key4").is_some(), "newest should survive");
        assert!(cache.get("key0").is_none(), "oldest should be evicted");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
