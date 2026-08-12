use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use sha2::{Digest, Sha256};
use std::cmp::Reverse;
use std::fs;
use std::io::{self, Read, Write};
use std::path::Path;

// ─── Model catalog ────────────────────────────────────────────────────────────

pub struct ModelInfo {
    pub id: &'static str,
    pub name: &'static str,
    /// One-line description shown in the picker
    pub description: &'static str,
    /// HuggingFace repository, and the exact commit within it. Never a branch:
    /// see [`ModelInfo::gguf_url`].
    pub repo: &'static str,
    pub revision: &'static str,
    pub gguf_filename: &'static str,
    /// SHA-256 of the file at `revision`, taken from the repository's Git LFS
    /// object id. Checked before the download is moved into place, so weights
    /// that are not byte-for-byte what this build expects never get loaded.
    pub sha256: &'static str,
    /// Exact size in bytes, so a truncated file is caught without hashing.
    pub bytes: u64,
    /// Minimum total system RAM in GB (soft requirement — warns, does not block)
    pub min_ram_gb: u64,
    /// Minimum free disk space in GB (warns before download)
    pub min_disk_gb: u64,
}

impl ModelInfo {
    /// Download URL, pinned to an immutable commit.
    ///
    /// This used to be `resolve/main/…`. A branch is a moving target: the same
    /// `diffmind download` a month apart could fetch different weights, and the
    /// same model id could mean different things on two developers' machines —
    /// which quietly undoes the reproducibility the whole gate is built on. A
    /// commit sha cannot move, and `sha256` proves what arrived.
    pub fn gguf_url(&self) -> String {
        format!(
            "https://huggingface.co/{}/resolve/{}/{}",
            self.repo, self.revision, self.gguf_filename
        )
    }

    /// Download size in GB, derived from `bytes` rather than stated separately —
    /// a hand-maintained second copy is a number that drifts.
    ///
    /// Decimal GB, not GiB: this is a download size, and it should agree with
    /// what HuggingFace shows next to the file the user is about to fetch.
    pub fn size_gb(&self) -> f64 {
        self.bytes as f64 / 1_000_000_000.0
    }

    fn expected(&self) -> Expected<'_> {
        Expected {
            sha256: self.sha256,
            bytes: self.bytes,
        }
    }
}

/// What a downloaded file must turn out to be.
#[derive(Clone, Copy)]
struct Expected<'a> {
    sha256: &'a str,
    bytes: u64,
}

/// All supported Qwen2.5-Coder models (Q4_K_M quantisation).
/// Coding-optimised only — no generic Qwen chat models.
///
/// `min_ram_gb` accounts for the weights plus the KV cache and runtime
/// overhead. It used to assume the file was the whole cost, which was
/// optimistic by roughly a factor of two because the CLI read the entire GGUF
/// onto the heap before candle copied out of it; the engine now memory-maps it.
///
/// ## Updating an entry, or adding a model
///
/// `revision`, `sha256` and `bytes` must be taken together from one commit, or
/// the download will refuse the file it fetched. All three come from the
/// HuggingFace API without downloading anything — the LFS object id *is* the
/// content SHA-256:
///
/// ```text
/// curl -s https://huggingface.co/api/models/<repo>/revision/main | jq -r .sha
/// curl -s 'https://huggingface.co/api/models/<repo>/tree/main?recursive=true' \
///   | jq -r '.[] | select(.path=="<file>") | "\(.lfs.oid) \(.lfs.size)"'
/// ```
pub const MODELS: &[ModelInfo] = &[
    ModelInfo {
        id: "0.5b",
        name: "Qwen2.5-Coder-0.5B",
        description: "Fastest — lint-style checks, great for CI or low-end hardware",
        repo: "Qwen/Qwen2.5-Coder-0.5B-Instruct-GGUF",
        revision: "ebb2015119c907b064c512bf053e945850b5875f",
        gguf_filename: "qwen2.5-coder-0.5b-instruct-q4_k_m.gguf",
        sha256: "1d9614638d18024d0fbb36575a15f1302a3adf044df10345688ec4f6e1c4ff32",
        bytes: 491_400_064,
        min_ram_gb: 2,
        min_disk_gb: 1,
    },
    ModelInfo {
        id: "1.5b",
        name: "Qwen2.5-Coder-1.5B",
        description: "Recommended — balanced quality and speed for most developers",
        repo: "Qwen/Qwen2.5-Coder-1.5B-Instruct-GGUF",
        revision: "f86cb2c1fa58255f8052cc32aeede1b7482d4361",
        gguf_filename: "qwen2.5-coder-1.5b-instruct-q4_k_m.gguf",
        sha256: "cc324af070c2ecbfd324a30884d2f951a7ff756aba85cb811a6ec436933bb046",
        bytes: 1_117_320_768,
        min_ram_gb: 4,
        min_disk_gb: 2,
    },
    ModelInfo {
        id: "3b",
        name: "Qwen2.5-Coder-3B",
        description: "Better — deeper reasoning, handles complex codebases well",
        repo: "Qwen/Qwen2.5-Coder-3B-Instruct-GGUF",
        revision: "f74adce6aa16316c625447af059dbebe4983757c",
        gguf_filename: "qwen2.5-coder-3b-instruct-q4_k_m.gguf",
        sha256: "724fb256bec1ff062b2f65e4569e871ad2e95ab2a3989723d1769c54294730b7",
        bytes: 2_104_932_800,
        min_ram_gb: 6,
        min_disk_gb: 3,
    },
    ModelInfo {
        id: "7b",
        name: "Qwen2.5-Coder-7B",
        description: "High quality — strong security analysis",
        repo: "Qwen/Qwen2.5-Coder-7B-Instruct-GGUF",
        revision: "13fb94bfda8c8cf22497dc57b78f391a9acb426a",
        gguf_filename: "qwen2.5-coder-7b-instruct-q4_k_m.gguf",
        sha256: "509287f78cb4d4cf6b3843734733b914b2c158e43e22a7f4bf5e963800894d3c",
        bytes: 4_683_073_536,
        min_ram_gb: 10,
        min_disk_gb: 6,
    },
    ModelInfo {
        id: "14b",
        name: "Qwen2.5-Coder-14B",
        description: "Expert — deep code understanding, workstation recommended",
        repo: "Qwen/Qwen2.5-Coder-14B-Instruct-GGUF",
        revision: "d0a692ef765eefbf2fabb130b3cb2e8917e3d225",
        gguf_filename: "qwen2.5-coder-14b-instruct-q4_k_m.gguf",
        sha256: "c1e659736d89ac1065fb495330fb824d94001974a4bfa78e7270e43476a8d940",
        bytes: 8_988_110_272,
        min_ram_gb: 18,
        min_disk_gb: 11,
    },
    ModelInfo {
        id: "32b",
        name: "Qwen2.5-Coder-32B",
        description: "Maximum — near human-level review quality, server-grade hardware",
        repo: "Qwen/Qwen2.5-Coder-32B-Instruct-GGUF",
        revision: "9d3053fce650fe1cdbdb75998c2a87add9d178ef",
        gguf_filename: "qwen2.5-coder-32b-instruct-q4_k_m.gguf",
        sha256: "4d64b316b5e6319d9613e0d97935d9ebd631fc7e334da400d00085eca749d085",
        bytes: 19_851_335_872,
        min_ram_gb: 40,
        min_disk_gb: 22,
    },
];

// Shared tokenizer for all Qwen2.5-Coder variants. Pinned and verified for the
// same reason the weights are: a tokenizer that disagrees with the model shifts
// every token id, which does not fail loudly — it just reviews badly.
//
// Note for whoever updates this: the `.lfs.oid` recipe above does **not** apply.
// At 7 MB this file is below HuggingFace's LFS threshold, so it is a plain Git
// blob — the API reports a sha1 `oid`, and `X-Linked-ETag` on the download is
// that same sha1. The sha256 below was obtained the only way available, by
// fetching the file and hashing it:
//
// ```text
// curl -sL <TOKENIZER_URL> | shasum -a 256
// ```
pub const TOKENIZER_FILENAME: &str = "tokenizer.json";
const TOKENIZER_URL: &str = "https://huggingface.co/Qwen/Qwen2.5-Coder-1.5B-Instruct/resolve/2e1fd397ee46e1388853d2af2c993145b0f1098a/tokenizer.json";
const TOKENIZER_EXPECTED: Expected<'static> = Expected {
    sha256: "c0382117ea329cdf097041132f6d735924b697924d6f6fc3945713e96ce87539",
    bytes: 7_031_645,
};

/// Look up a model by its short ID (e.g. "1.5b", "7b").
pub fn find_model(id: &str) -> Option<&'static ModelInfo> {
    MODELS.iter().find(|m| m.id == id)
}

pub fn model_ids() -> Vec<&'static str> {
    MODELS.iter().map(|m| m.id).collect()
}

// ─── Interactive model picker ─────────────────────────────────────────────────

fn prompt_model_selection() -> Result<&'static ModelInfo> {
    println!("\nAvailable models — Qwen2.5-Coder (coding-optimised, Q4_K_M):\n");
    println!(
        "  {:<4}  {:<26}  {:>7}  {:>8}  Description",
        "#", "Model", "Size", "Min RAM"
    );
    println!("  {}", "─".repeat(82));

    for (i, m) in MODELS.iter().enumerate() {
        let marker = if m.id == "1.5b" { "*" } else { " " };
        println!(
            "  [{}] {} {:<26}  {:>5.1} GB  {:>5} GB   {}",
            i + 1,
            marker,
            m.name,
            m.size_gb(),
            m.min_ram_gb,
            m.description
        );
    }

    println!("\n  * recommended\n");
    print!("Select model [1-{}] (default: 2 — 1.5b): ", MODELS.len());
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let trimmed = input.trim();

    let idx: usize = if trimmed.is_empty() {
        1 // 1.5b is index 1
    } else {
        trimmed
            .parse::<usize>()
            .context("please enter a number")?
            .checked_sub(1)
            .ok_or_else(|| anyhow::anyhow!("selection must be at least 1"))?
    };

    MODELS.get(idx).ok_or_else(|| {
        anyhow::anyhow!(
            "invalid selection '{}'. Enter a number between 1 and {}.",
            trimmed,
            MODELS.len()
        )
    })
}

// ─── Hardware requirements check ─────────────────────────────────────────────

fn check_requirements(model: &ModelInfo, model_dir: &Path, assume_yes: bool) -> Result<()> {
    use sysinfo::{Disks, System};

    let mut sys = System::new();
    sys.refresh_memory();
    let total_ram_gb = sys.total_memory() as f64 / 1_073_741_824.0;

    let disks = Disks::new_with_refreshed_list();
    let check_path = model_dir
        .ancestors()
        .find(|p| p.exists())
        .unwrap_or(Path::new(if cfg!(windows) { "C:\\" } else { "/" }));

    let free_disk_gb = {
        let mut matching: Vec<_> = disks
            .iter()
            .filter(|d| check_path.starts_with(d.mount_point()))
            .collect();
        matching.sort_by_key(|d| Reverse(d.mount_point().as_os_str().len()));
        matching
            .first()
            .map(|d| d.available_space() as f64 / 1_073_741_824.0)
            .unwrap_or(0.0)
    };

    let ram_ok = total_ram_gb >= model.min_ram_gb as f64;
    let disk_ok = free_disk_gb >= model.min_disk_gb as f64;

    println!("\n  Requirements for {}:", model.name);
    println!("  {}", "─".repeat(52));
    println!(
        "  {}  RAM :  {:.1} GB detected   /  {} GB required",
        if ram_ok { "✓" } else { "✗" },
        total_ram_gb,
        model.min_ram_gb
    );
    println!(
        "  {}  Disk:  {:.1} GB free       /  {:.1} GB required",
        if disk_ok { "✓" } else { "✗" },
        free_disk_gb,
        model.min_disk_gb as f64
    );

    if !ram_ok {
        println!(
            "\n  WARNING: {:.1} GB RAM detected but {} GB is the minimum for {}.",
            total_ram_gb, model.min_ram_gb, model.name
        );
        println!("  Inference may be extremely slow or crash. Consider a smaller model.");
    }
    if !disk_ok {
        println!(
            "\n  WARNING: only {:.1} GB disk space free, {:.1} GB needed for {}.",
            free_disk_gb, model.min_disk_gb as f64, model.name
        );
        println!("  Free up space or choose a smaller model.");
    }

    if !ram_ok || !disk_ok {
        // A non-interactive run (CI) must not block on a prompt that nobody
        // will ever answer.
        if assume_yes || !std::io::IsTerminal::is_terminal(&io::stdin()) {
            println!("\n  Proceeding anyway (non-interactive).");
            return Ok(());
        }
        print!("\n  Proceed with download anyway? [y/N]: ");
        io::stdout().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !answer.trim().eq_ignore_ascii_case("y") {
            return Err(anyhow::anyhow!("download cancelled."));
        }
    } else {
        println!("  All requirements met.\n");
    }

    Ok(())
}

// ─── File download ─────────────────────────────────────────────────────────────

fn http_client() -> Result<reqwest::blocking::Client> {
    reqwest::blocking::Client::builder()
        // Was pinned at 0.5.0 while the crate was on 0.7.1; HuggingFace logs
        // this, and a stale version makes any future rate-limit report useless.
        .user_agent(concat!("diffmind/", env!("CARGO_PKG_VERSION")))
        .timeout(None)
        .build()
        .context("could not build HTTP client")
}

/// Download `url` to `dest`, atomically and verifiably.
///
/// The previous implementation wrote straight to the final path inside
/// `while let Ok(n) = response.read(..)`, which treats a mid-stream network
/// error as a clean EOF. An interrupted download therefore left a truncated
/// `.gguf` that `exists()` happily accepted forever after, surfacing later as
/// an inscrutable GGUF parse error. Now: read errors propagate, bytes are
/// counted against `Content-Length`, the file is hashed, and only a complete
/// download is renamed into place.
///
/// `expect` closes the remaining gap. Counting bytes proves the transfer
/// finished; only the digest proves the *right file* arrived. Without it the
/// only recorded hash was whatever the server happened to send, which could
/// confirm the file had not rotted on disk and nothing more.
fn download_file(url: &str, dest: &Path, expect: Expected<'_>) -> Result<(u64, String)> {
    let client = http_client()?;
    let mut response = client
        .get(url)
        .send()
        .with_context(|| format!("could not reach {url}"))?;

    if !response.status().is_success() {
        return Err(anyhow::anyhow!(
            "failed to download {url}: HTTP {}",
            response.status()
        ));
    }

    let expected_len = response.content_length();

    let pb = match expected_len {
        Some(total) => {
            let pb = ProgressBar::new(total);
            pb.set_style(
                ProgressStyle::default_bar()
                    .template("  {spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta})")?
                    .progress_chars("#>-"),
            );
            pb
        }
        None => {
            let pb = ProgressBar::new_spinner();
            pb.set_style(
                ProgressStyle::default_spinner()
                    .template("  {spinner:.green} {bytes} downloaded")?,
            );
            pb
        }
    };

    // Write beside the destination so the rename is atomic (same filesystem).
    let part = dest.with_extension(format!(
        "{}.part",
        dest.extension().and_then(|e| e.to_str()).unwrap_or("tmp")
    ));
    if let Some(parent) = part.parent() {
        fs::create_dir_all(parent)?;
    }

    let result = (|| -> Result<(u64, String)> {
        let mut file = fs::File::create(&part)
            .with_context(|| format!("could not create {}", part.display()))?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; 1 << 16];
        let mut downloaded = 0u64;

        loop {
            // A read error here used to be silently swallowed as end-of-stream.
            let n = response
                .read(&mut buffer)
                .context("connection failed while downloading")?;
            if n == 0 {
                break;
            }
            file.write_all(&buffer[..n])?;
            hasher.update(&buffer[..n]);
            downloaded += n as u64;
            pb.set_position(downloaded);
        }

        file.flush()?;
        // Without this the rename can publish a file whose contents are still
        // in the page cache when the machine loses power.
        file.sync_all()?;
        drop(file);

        if let Some(expected) = expected_len
            && downloaded != expected
        {
            return Err(anyhow::anyhow!(
                "download incomplete: got {downloaded} bytes, expected {expected}. \
                 Re-run `diffmind download --force`."
            ));
        }

        // Against the catalog, not against the server. `Content-Length` only
        // says the transfer matched what this response promised.
        if downloaded != expect.bytes {
            return Err(anyhow::anyhow!(
                "unexpected file size: got {downloaded} bytes, expected {}. \
                 This is not the file diffmind pinned — check your network for a \
                 proxy that rewrites downloads.",
                expect.bytes
            ));
        }

        let digest = format!("{:x}", hasher.finalize());
        if !digest.eq_ignore_ascii_case(expect.sha256) {
            return Err(anyhow::anyhow!(
                "checksum mismatch — refusing to install these weights.\n\
                 \x20 expected  {}\n\
                 \x20 got       {digest}\n\
                 The download was corrupted in transit or the file is not what \
                 diffmind pinned. Re-run `diffmind download --force`; if it fails \
                 again, report it rather than working around it.",
                expect.sha256
            ));
        }

        Ok((downloaded, digest))
    })();

    pb.finish_and_clear();

    match result {
        Ok((bytes, digest)) => {
            fs::rename(&part, dest)
                .with_context(|| format!("could not move {} into place", part.display()))?;
            Ok((bytes, digest))
        }
        Err(e) => {
            // Never leave a partial file where `exists()` will trust it.
            let _ = fs::remove_file(&part);
            Err(e)
        }
    }
}

fn hash_file(path: &Path) -> Result<(u64, String)> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 16];
    let mut total = 0u64;
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
        total += n as u64;
    }
    Ok((total, format!("{:x}", hasher.finalize())))
}

/// Verify a file on disk against what the catalog pins.
///
/// This used to compare against a sidecar receipt written at download time,
/// which made it a tautology: the receipt held whatever digest the server's
/// bytes produced, so the check could prove the file had not rotted on disk and
/// never that it was the right file. It also had a third state — "no checksum on
/// record" — which is gone, because the expectation now ships in the binary.
fn verify_file(path: &Path, expect: Expected<'_>) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    // Size is the cheap check and catches every truncation; only hash when it
    // passes, because hashing 20 GB on every startup is not acceptable.
    if fs::metadata(path)?.len() != expect.bytes {
        return Ok(false);
    }
    let (_, digest) = hash_file(path)?;
    Ok(digest.eq_ignore_ascii_case(expect.sha256))
}

/// Quick integrity check run before loading: size only, so it costs nothing.
///
/// Catches both a truncated download and a file left behind by a build that
/// pinned a different revision — the latter would otherwise load happily and
/// review with weights nobody asked for.
fn wrong_size(path: &Path, expect: Expected<'_>) -> bool {
    fs::metadata(path).is_ok_and(|m| m.len() != expect.bytes)
}

/// Is the model on disk the one this build pins? Size only — cheap enough to
/// run before every load.
pub fn looks_truncated(model_dir: &Path, model: &ModelInfo) -> bool {
    wrong_size(&model_dir.join(model.gguf_filename), model.expected())
        || wrong_size(&model_dir.join(TOKENIZER_FILENAME), TOKENIZER_EXPECTED)
}

// ─── Public entry point ───────────────────────────────────────────────────────

/// Download model files to `model_dir`.
///
/// - `model_id = None`  → show interactive picker
/// - `model_id = Some`  → skip picker, validate ID, download directly
/// - `force = true`     → delete existing files and re-download
pub fn ensure_model_files(model_id: Option<&str>, model_dir: &Path, force: bool) -> Result<()> {
    fs::create_dir_all(model_dir)?;

    let model: &ModelInfo = match model_id {
        Some(id) => find_model(id).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown model '{id}'. Valid options: {}",
                model_ids().join(", ")
            )
        })?,
        None => prompt_model_selection()?,
    };

    println!("\nSelected: {} — {}", model.name, model.description);

    check_requirements(model, model_dir, force)?;

    let model_path = model_dir.join(model.gguf_filename);
    let tokenizer_path = model_dir.join(TOKENIZER_FILENAME);

    if force {
        for p in [&model_path, &tokenizer_path] {
            let _ = fs::remove_file(p);
        }
        println!("Existing files removed. Re-downloading...\n");
    }

    // Each file is fetched only if it is absent or is not what this build pins.
    // A file left by a build pinning a different revision counts as absent:
    // loading it would review with weights nobody chose.
    let fetch = |what: &str, url: &str, path: &Path, expect: Expected<'_>| -> Result<()> {
        if path.exists() && !wrong_size(path, expect) {
            println!("{what} already present (use --force to re-download).");
            return Ok(());
        }
        if path.exists() {
            println!("Existing {what} is not the pinned file — re-downloading.");
            let _ = fs::remove_file(path);
        }
        println!("Downloading {what}...");
        let (_, digest) = download_file(url, path, expect)?;
        println!("  sha256  {digest}  (verified)");
        Ok(())
    };

    // Tokenizer first: it is small, and a failure there is cheap to discover.
    fetch(
        TOKENIZER_FILENAME,
        TOKENIZER_URL,
        &tokenizer_path,
        TOKENIZER_EXPECTED,
    )?;
    fetch(
        &format!("{} ({:.1} GB)", model.gguf_filename, model.size_gb()),
        &model.gguf_url(),
        &model_path,
        model.expected(),
    )?;

    println!("\nModel ready: {}", model_path.display());
    Ok(())
}

/// `diffmind download --verify`
pub fn verify_model_files(model_id: &str, model_dir: &Path) -> Result<bool> {
    let model = find_model(model_id).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown model '{model_id}'. Valid options: {}",
            model_ids().join(", ")
        )
    })?;

    let mut all_ok = true;
    for (filename, expect) in [
        (model.gguf_filename, model.expected()),
        (TOKENIZER_FILENAME, TOKENIZER_EXPECTED),
    ] {
        print!("  {filename} ... ");
        io::stdout().flush()?;
        if verify_file(&model_dir.join(filename), expect)? {
            println!("ok");
        } else {
            println!("MISSING, CORRUPT, or not the pinned revision");
            all_ok = false;
        }
    }

    if all_ok {
        println!("\n  All files match the checksums pinned in this build.");
    } else {
        println!("\n  Re-download with: diffmind download --model {model_id} --force");
    }
    Ok(all_ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_catalog_entry_is_self_consistent() {
        for m in MODELS {
            assert!(
                m.gguf_url().ends_with(m.gguf_filename),
                "{}: url and filename disagree, so the download would verify a \
                 file that was never fetched",
                m.id
            );
            assert!(
                m.min_disk_gb as f64 >= m.size_gb(),
                "{}: disk floor below download size",
                m.id
            );
            assert!(
                m.min_ram_gb as f64 > m.size_gb(),
                "{}: RAM floor below weights",
                m.id
            );
        }
    }

    /// Every download is refused unless it hashes to the pinned value, so a
    /// malformed entry here does not fail safe — it makes the model unusable.
    /// Cheaper to catch in CI than in a bug report.
    #[test]
    fn every_pinned_digest_is_wellformed() {
        let sha_ok = |s: &str| s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit());
        let rev_ok = |s: &str| s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit());

        for m in MODELS {
            assert!(sha_ok(m.sha256), "{}: sha256 is not 64 hex chars", m.id);
            assert!(
                rev_ok(m.revision),
                "{}: revision must be a full commit sha, not `{}` — a branch or \
                 tag can move, and then the pinned digest is simply wrong",
                m.id,
                m.revision
            );
            assert!(m.bytes > 0, "{}: bytes must be the real file size", m.id);
        }
        assert!(sha_ok(TOKENIZER_EXPECTED.sha256));
        assert!(TOKENIZER_URL.contains("/resolve/"));
    }

    /// A moving ref is the whole defect: `resolve/main/…` meant the same
    /// `diffmind download` a month apart could fetch different weights, which no
    /// checksum can protect against because there is nothing to compare to.
    #[test]
    fn no_download_url_points_at_a_branch() {
        let urls: Vec<String> = MODELS
            .iter()
            .map(|m| m.gguf_url())
            .chain([TOKENIZER_URL.to_string()])
            .collect();
        for url in urls {
            assert!(
                !url.contains("/resolve/main/") && !url.contains("/resolve/master/"),
                "unpinned URL: {url}"
            );
            assert!(url.starts_with("https://"), "insecure URL: {url}");
        }
    }

    /// The displayed size comes from the pinned byte count rather than a
    /// separately maintained number that drifts away from it.
    #[test]
    fn sizes_are_derived_from_the_pinned_byte_counts() {
        let m = find_model("1.5b").unwrap();
        assert_eq!(m.bytes, 1_117_320_768);
        assert!(
            (m.size_gb() - 1.117).abs() < 0.001,
            "decimal GB, to agree with what HuggingFace displays; got {}",
            m.size_gb()
        );
    }

    #[test]
    fn model_ids_are_unique() {
        let mut ids = model_ids();
        let before = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), before);
    }

    #[test]
    fn find_model_matches_the_documented_ids() {
        assert!(find_model("1.5b").is_some());
        assert!(find_model("32b").is_some());
        assert!(find_model("99b").is_none());
    }

    fn tmpdir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("diffmind-dl-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// `b"12345"`, so a test can pin a real expectation.
    fn known() -> Expected<'static> {
        Expected {
            sha256: "5994471abb01112afcc18159f6cc74b4f511b99806da59b3caf5a9c173cacfc5",
            bytes: 5,
        }
    }

    #[test]
    fn verify_reports_missing_files_as_failures() {
        let dir = tmpdir("missing");
        assert!(!verify_file(&dir.join("nope.gguf"), known()).unwrap());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_size_mismatch_is_detected_without_hashing() {
        let dir = tmpdir("trunc");
        let path = dir.join("m.gguf");
        fs::write(&path, b"12345").unwrap();

        let wrong_len = Expected {
            sha256: known().sha256,
            bytes: 999,
        };
        assert!(wrong_size(&path, wrong_len));
        assert!(!verify_file(&path, wrong_len).unwrap());

        assert!(!wrong_size(&path, known()));
        assert!(verify_file(&path, known()).unwrap());

        let _ = fs::remove_dir_all(&dir);
    }

    /// The check that the receipt could never make: right length, wrong bytes.
    ///
    /// Comparing against a digest recorded at download time was a tautology — it
    /// held whatever the server sent. Only a digest that ships in the binary can
    /// tell "the file I have" from "the file I asked for".
    #[test]
    fn a_file_of_the_right_length_but_the_wrong_content_is_rejected() {
        let dir = tmpdir("swapped");
        let path = dir.join("m.gguf");
        fs::write(&path, b"54321").unwrap();

        assert!(
            !wrong_size(&path, known()),
            "same length, so the cheap check cannot see it"
        );
        assert!(
            !verify_file(&path, known()).unwrap(),
            "substituted content must be caught by the digest"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// A file left behind by a build that pinned a different revision is not
    /// "already present" — loading it would review with weights nobody chose.
    #[test]
    fn a_model_from_another_revision_is_not_accepted_as_present() {
        let dir = tmpdir("stale-revision");
        let model = find_model("1.5b").unwrap();
        // Whatever an older build downloaded, at a plausible-looking size.
        fs::write(dir.join(model.gguf_filename), vec![0u8; 1024]).unwrap();
        fs::write(dir.join(TOKENIZER_FILENAME), b"{}").unwrap();

        assert!(
            looks_truncated(&dir, model),
            "the pre-load check must refuse a file that is not the pinned one"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
