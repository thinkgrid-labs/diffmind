//! Content-addressed cache of per-chunk review results.
//!
//! Re-running after touching one file should not re-infer the other nine.
//!
//! # What a key covers
//!
//! The key is derived from the **exact prompt text** the backend would receive,
//! plus the backend's [`cache identity`](crate::ReviewBackend::cache_identity)
//! and every sampling option. Keying on the rendered prompt rather than on a
//! list of its ingredients is deliberate: an ingredient list has to be kept in
//! step with the prompt builder by hand, and it was not — the context and rule
//! sets actually rendered depend on the backend's window and on the recursion
//! depth, neither of which the old list included. Anything that changes what
//! the model is asked changes the key, by construction.
//!
//! # What a key cannot cover
//!
//! A remote server can swap the weights behind a model name. Where the server
//! reports a revision (Ollama's model digest) it is part of the identity;
//! where it does not (OpenAI-compatible servers), the cache cannot detect the
//! swap and `diffmind cache clear` is the remedy. This is documented rather
//! than papered over.
//!
//! # Trust
//!
//! The cache directory lives inside the repository under review, so a branch
//! can ship files into it. Without a secret, anyone who can predict a key —
//! and every input to it is public — could commit an entry saying "no
//! findings" for their own change. Keys are therefore an HMAC under a
//! per-user secret kept outside the repository ([`ReviewCache::with_secret`]),
//! and every entry records the key it was written for, which is checked on
//! read.

use crate::types::ReviewSummary;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Serialize, Deserialize)]
struct CacheEntry {
    /// Guards against a format change silently deserializing into garbage.
    version: u32,
    /// The key this entry was written under. A file renamed onto another key,
    /// or planted under a guessed name, is rejected rather than served.
    #[serde(default)]
    key: String,
    summary: ReviewSummary,
}

/// Bumped when the entry format *or the key derivation* changes. Version 2
/// keys on the rendered prompt and the backend's cache identity.
const ENTRY_VERSION: u32 = 2;

/// Domain separator mixed into every key, bumped together with
/// [`ENTRY_VERSION`] so old and new keys can never coincide.
const KEY_DOMAIN: &[u8] = b"diffmind-review-cache-v2";

/// A cache entry larger than this is not one diffmind wrote. Reading it would
/// only cost memory, so it is treated as a miss.
const MAX_ENTRY_BYTES: u64 = 8 * 1024 * 1024;

/// Entries kept once a run finishes. Each is a few KB, so this bounds the
/// directory at tens of MB however long a repository is worked in.
pub const DEFAULT_MAX_ENTRIES: usize = 20_000;

/// Distinguishes temporary files written concurrently by threads of one
/// process; the pid alone does not.
static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

pub struct ReviewCache {
    dir: PathBuf,
    /// Per-user HMAC key. `None` falls back to a plain hash, which is fine for
    /// a trusted directory but not for one a branch can write into.
    secret: Option<[u8; 32]>,
}

/// Everything that can change a chunk's review result. Missing a field here
/// means serving a cached answer the current settings would never produce.
pub struct CacheKeyInput<'a> {
    /// [`crate::ReviewBackend::cache_identity`]: model, weights revision where
    /// known, device, and anything else about the backend that shapes output.
    pub backend: &'a str,
    pub prompt_version: u32,
    /// The complete prompt as the backend will see it — system and user halves.
    /// Covers the chunk, its context, languages, rule sets and requirements
    /// *as rendered*, after every budget decision.
    pub prompt: &'a str,
    pub max_tokens: u32,
    pub temperature: f64,
    pub seed: u64,
    /// Remaining generation options (repeat penalty, its window, JSON mode).
    /// They are constants today; keying on them means changing one cannot
    /// replay answers produced under the old value.
    pub repeat_penalty: f32,
    pub repeat_last_n: usize,
    pub json: bool,
}

/// HMAC-SHA256 (RFC 2104). Implemented over `sha2` rather than pulling in a
/// crate for twenty lines; the construction is the standard one.
fn hmac_sha256(key: &[u8; 32], message_parts: &[&[u8]]) -> [u8; 32] {
    const BLOCK: usize = 64;
    // A 32-byte key is shorter than the block, so it is zero-padded as is.
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for ((i, o), kb) in ipad.iter_mut().zip(opad.iter_mut()).zip(key.iter()) {
        *i ^= kb;
        *o ^= kb;
    }
    let mut inner = Sha256::new();
    inner.update(ipad);
    for part in message_parts {
        inner.update(part);
    }
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner);
    outer.finalize().into()
}

impl ReviewCache {
    pub fn new(dir: PathBuf) -> Self {
        ReviewCache { dir, secret: None }
    }

    /// Open a cache rooted at `<base>/cache`, creating it if needed.
    /// Returns `None` when the directory cannot be created — a cache is an
    /// optimisation, never a reason to fail a review.
    pub fn open(base: &Path) -> Option<Self> {
        let dir = base.join("cache");
        std::fs::create_dir_all(&dir).ok()?;
        Some(ReviewCache { dir, secret: None })
    }

    /// Key entries with an HMAC under `secret`, so files a branch commits into
    /// the cache directory can never be mistaken for this user's results.
    pub fn with_secret(mut self, secret: [u8; 32]) -> Self {
        self.secret = Some(secret);
        self
    }

    /// The key for one chunk. Length-prefixed fields, so no two distinct
    /// inputs serialize to the same byte string.
    pub fn key(&self, input: &CacheKeyInput<'_>) -> String {
        let mut material: Vec<u8> = Vec::with_capacity(input.prompt.len() + 256);
        let mut field = |bytes: &[u8]| {
            material.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
            material.extend_from_slice(bytes);
        };
        field(KEY_DOMAIN);
        field(input.backend.as_bytes());
        field(&input.prompt_version.to_le_bytes());
        field(input.prompt.as_bytes());
        field(&input.max_tokens.to_le_bytes());
        field(&input.temperature.to_le_bytes());
        field(&input.seed.to_le_bytes());
        field(&input.repeat_penalty.to_le_bytes());
        field(&(input.repeat_last_n as u64).to_le_bytes());
        field(&[u8::from(input.json)]);

        let digest: [u8; 32] = match &self.secret {
            Some(secret) => hmac_sha256(secret, &[&material]),
            None => Sha256::digest(&material).into(),
        };
        // 128 bits is ample against accidental collision, and the stored key
        // is re-checked on read.
        digest.iter().take(16).map(|b| format!("{b:02x}")).collect()
    }

    fn path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }

    /// A cached summary, or `None` on any miss — absent, unreadable, oversized,
    /// truncated, corrupt, from another format version, or written for a
    /// different key. Every one of those means "infer it again", never "fail".
    pub fn get(&self, key: &str) -> Option<ReviewSummary> {
        let path = self.path(key);
        let meta = std::fs::metadata(&path).ok()?;
        if !meta.is_file() || meta.len() > MAX_ENTRY_BYTES {
            return None;
        }
        let raw = std::fs::read(&path).ok()?;
        let entry: CacheEntry = serde_json::from_slice(&raw).ok()?;
        (entry.version == ENTRY_VERSION && entry.key == key).then_some(entry.summary)
    }

    pub fn put(&self, key: &str, summary: &ReviewSummary) {
        let entry = CacheEntry {
            version: ENTRY_VERSION,
            key: key.to_string(),
            summary: summary.clone(),
        };
        let Ok(json) = serde_json::to_vec(&entry) else {
            return;
        };
        // Write-then-rename so a concurrent reader never sees a half-written
        // entry (two `diffmind` runs in one repo is normal). The temporary is
        // unique per process *and* per call, so two threads cannot interleave
        // writes into one file either.
        let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = self
            .dir
            .join(format!(".{key}.tmp.{}.{n}", std::process::id()));
        if std::fs::write(&tmp, json).is_ok() && std::fs::rename(&tmp, self.path(key)).is_ok() {
            return;
        }
        let _ = std::fs::remove_file(&tmp);
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
        let excess = entries.len().saturating_sub(max_entries);
        for (_, path) in entries.iter().take(excess) {
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

    fn input<'a>(prompt: &'a str, backend: &'a str) -> CacheKeyInput<'a> {
        CacheKeyInput {
            backend,
            prompt_version: 1,
            prompt,
            max_tokens: 1024,
            temperature: 0.0,
            seed: 7,
            repeat_penalty: 1.1,
            repeat_last_n: 64,
            json: true,
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

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("diffmind-cache-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn round_trips_a_summary() {
        let dir = tmp("roundtrip");
        let cache = ReviewCache::open(&dir).unwrap();
        let key = cache.key(&input("diff", "m"));

        assert!(cache.get(&key).is_none());
        cache.put(&key, &summary());
        let got = cache.get(&key).expect("cached entry should be readable");
        assert_eq!(got.findings.len(), 1);
        assert_eq!(got.findings[0].issue, "boom");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_input_field_changes_the_key() {
        let c = ReviewCache::new(PathBuf::from("unused"));
        let base = c.key(&input("diff", "m"));
        assert_ne!(base, c.key(&input("other diff", "m")));
        assert_ne!(base, c.key(&input("diff", "other-model")));

        type Mutation = Box<dyn Fn(&mut CacheKeyInput<'_>)>;
        let variants: Vec<(&str, Mutation)> = vec![
            ("prompt version", Box::new(|k| k.prompt_version = 2)),
            ("temperature", Box::new(|k| k.temperature = 0.7)),
            ("seed", Box::new(|k| k.seed = 8)),
            ("max tokens", Box::new(|k| k.max_tokens = 2048)),
            ("repeat penalty", Box::new(|k| k.repeat_penalty = 1.0)),
            ("repeat window", Box::new(|k| k.repeat_last_n = 32)),
            ("json mode", Box::new(|k| k.json = false)),
        ];
        for (name, change) in variants {
            let mut k = input("diff", "m");
            change(&mut k);
            assert_ne!(base, c.key(&k), "{name} must change the key");
        }
    }

    /// Length-prefixing: moving bytes between adjacent fields must not produce
    /// the same key, which a plain concatenation would allow.
    #[test]
    fn field_boundaries_are_unambiguous() {
        let c = ReviewCache::new(PathBuf::from("unused"));
        assert_ne!(
            c.key(&input("bc", "a")),
            c.key(&input("c", "ab")),
            "backend/prompt boundary must be part of the key"
        );
    }

    #[test]
    fn key_is_stable_for_identical_input() {
        let c = ReviewCache::new(PathBuf::from("unused"));
        assert_eq!(c.key(&input("diff", "m")), c.key(&input("diff", "m")));
        let keyed = ReviewCache::new(PathBuf::from("unused")).with_secret([7; 32]);
        assert_eq!(
            keyed.key(&input("diff", "m")),
            keyed.key(&input("diff", "m"))
        );
    }

    /// A branch can commit files into `.diffmind/cache/`. Every input to an
    /// unkeyed key is public, so an attacker could precompute the key for their
    /// own change and ship an entry saying "no findings". Under a per-user
    /// secret the key is unpredictable, and a planted file is never served.
    #[test]
    fn a_planted_entry_is_not_served_to_a_cache_with_a_secret() {
        let dir = tmp("planted");
        let attacker = ReviewCache::open(&dir).unwrap();
        let victim = ReviewCache::open(&dir).unwrap().with_secret([42; 32]);

        // The attacker computes the unkeyed key and writes "nothing found".
        let attacker_key = attacker.key(&input("malicious diff", "m"));
        attacker.put(&attacker_key, &ReviewSummary::default());

        let victim_key = victim.key(&input("malicious diff", "m"));
        assert_ne!(attacker_key, victim_key);
        assert!(victim.get(&victim_key).is_none(), "planted entry served");

        // Even a file dropped under the victim's exact key name is refused
        // unless it was written for that key.
        std::fs::write(
            victim.path(&victim_key),
            serde_json::to_vec(&CacheEntry {
                version: ENTRY_VERSION,
                key: "someone-elses-key".into(),
                summary: ReviewSummary::default(),
            })
            .unwrap(),
        )
        .unwrap();
        assert!(victim.get(&victim_key).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn different_secrets_produce_different_keys() {
        let a = ReviewCache::new(PathBuf::from("u")).with_secret([1; 32]);
        let b = ReviewCache::new(PathBuf::from("u")).with_secret([2; 32]);
        assert_ne!(a.key(&input("diff", "m")), b.key(&input("diff", "m")));
    }

    /// RFC 4231 test case 2 cannot be used directly (its key is not 32 bytes),
    /// so check the construction against an independent property instead: the
    /// HMAC must differ from a plain hash of key||message.
    #[test]
    fn hmac_is_not_a_plain_prefix_hash() {
        let key = [9u8; 32];
        let mac = hmac_sha256(&key, &[b"message"]);
        let mut naive = Sha256::new();
        naive.update(key);
        naive.update(b"message");
        let naive: [u8; 32] = naive.finalize().into();
        assert_ne!(mac, naive);
        // And it is deterministic.
        assert_eq!(mac, hmac_sha256(&key, &[b"mess", b"age"]));
    }

    #[test]
    fn corrupt_truncated_and_foreign_entries_are_misses_not_errors() {
        let dir = tmp("corrupt");
        let cache = ReviewCache::open(&dir).unwrap();
        let key = cache.key(&input("diff", "m"));
        cache.put(&key, &summary());
        let full = std::fs::read(cache.path(&key)).unwrap();

        // Truncated mid-write (e.g. a crash on a filesystem without atomic
        // rename semantics).
        std::fs::write(cache.path(&key), &full[..full.len() / 2]).unwrap();
        assert!(cache.get(&key).is_none());

        // Not JSON at all.
        std::fs::write(cache.path(&key), b"\x00\xffgarbage").unwrap();
        assert!(cache.get(&key).is_none());

        // An older format version.
        std::fs::write(
            cache.path(&key),
            br#"{"version":1,"summary":{"findings":[],"positives":[],"suggestions":[]}}"#,
        )
        .unwrap();
        assert!(cache.get(&key).is_none());

        // A directory where the entry should be.
        std::fs::remove_file(cache.path(&key)).unwrap();
        std::fs::create_dir_all(cache.path(&key)).unwrap();
        assert!(cache.get(&key).is_none());
        std::fs::remove_dir_all(cache.path(&key)).unwrap();

        // And the next write repairs it.
        cache.put(&key, &summary());
        assert!(cache.get(&key).is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_oversized_entry_is_ignored() {
        let dir = tmp("oversized");
        let cache = ReviewCache::open(&dir).unwrap();
        let key = cache.key(&input("diff", "m"));
        let f = std::fs::File::create(cache.path(&key)).unwrap();
        f.set_len(MAX_ENTRY_BYTES + 1).unwrap();
        assert!(cache.get(&key).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn put_into_an_unwritable_location_is_silent() {
        // A cache whose directory vanished must not panic or error the review.
        let cache = ReviewCache::new(tmp("vanished").join("does/not/exist"));
        cache.put("k", &summary());
        assert!(cache.get("k").is_none());
    }

    /// Two runs in one repository is normal. Readers racing writers must only
    /// ever see a complete entry or a miss — never a parse of half a file.
    #[test]
    fn concurrent_readers_and_writers_never_observe_a_partial_entry() {
        let dir = tmp("concurrent");
        let _ = ReviewCache::open(&dir).unwrap();
        let key = ReviewCache::new(PathBuf::new()).key(&input("shared", "m"));

        // A large summary makes a torn write likely if writes were not atomic.
        let mut big = summary();
        big.positives = (0..2000).map(|i| format!("positive number {i}")).collect();

        let mut handles = Vec::new();
        for w in 0..4 {
            let (dir, key, big) = (dir.clone(), key.clone(), big.clone());
            handles.push(std::thread::spawn(move || {
                let c = ReviewCache::open(&dir).unwrap();
                for _ in 0..50 {
                    c.put(&key, &big);
                }
                w
            }));
        }
        for _ in 0..4 {
            let (dir, key) = (dir.clone(), key.clone());
            handles.push(std::thread::spawn(move || {
                let c = ReviewCache::open(&dir).unwrap();
                for _ in 0..200 {
                    if let Some(s) = c.get(&key) {
                        assert_eq!(s.positives.len(), 2000, "observed a partial entry");
                    }
                }
                0
            }));
        }
        for h in handles {
            h.join().expect("no thread may panic");
        }
        let c = ReviewCache::open(&dir).unwrap();
        assert_eq!(c.get(&key).unwrap().positives.len(), 2000);
        // No temporaries left behind.
        let stray = std::fs::read_dir(dir.join("cache"))
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .count();
        assert_eq!(stray, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn prune_keeps_the_newest_entries() {
        let dir = tmp("prune");
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
