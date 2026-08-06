use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use sha2::{Digest, Sha256};
use std::cmp::Reverse;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

// ─── Model catalog ────────────────────────────────────────────────────────────

pub struct ModelInfo {
    pub id: &'static str,
    pub name: &'static str,
    /// One-line description shown in the picker
    pub description: &'static str,
    pub gguf_filename: &'static str,
    pub gguf_url: &'static str,
    /// Approximate compressed download size in GB
    pub size_gb: f64,
    /// Minimum total system RAM in GB (soft requirement — warns, does not block)
    pub min_ram_gb: u64,
    /// Minimum free disk space in GB (warns before download)
    pub min_disk_gb: u64,
}

/// All supported Qwen2.5-Coder models (Q4_K_M quantisation).
/// Coding-optimised only — no generic Qwen chat models.
///
/// `min_ram_gb` accounts for the weights plus the KV cache and runtime
/// overhead. It used to assume the file was the whole cost, which was
/// optimistic by roughly a factor of two because the CLI read the entire GGUF
/// onto the heap before candle copied out of it; the engine now memory-maps it.
pub const MODELS: &[ModelInfo] = &[
    ModelInfo {
        id: "0.5b",
        name: "Qwen2.5-Coder-0.5B",
        description: "Fastest — lint-style checks, great for CI or low-end hardware",
        gguf_filename: "qwen2.5-coder-0.5b-instruct-q4_k_m.gguf",
        gguf_url: "https://huggingface.co/Qwen/Qwen2.5-Coder-0.5B-Instruct-GGUF/resolve/main/qwen2.5-coder-0.5b-instruct-q4_k_m.gguf",
        size_gb: 0.4,
        min_ram_gb: 2,
        min_disk_gb: 1,
    },
    ModelInfo {
        id: "1.5b",
        name: "Qwen2.5-Coder-1.5B",
        description: "Recommended — balanced quality and speed for most developers",
        gguf_filename: "qwen2.5-coder-1.5b-instruct-q4_k_m.gguf",
        gguf_url: "https://huggingface.co/Qwen/Qwen2.5-Coder-1.5B-Instruct-GGUF/resolve/main/qwen2.5-coder-1.5b-instruct-q4_k_m.gguf",
        size_gb: 1.1,
        min_ram_gb: 4,
        min_disk_gb: 2,
    },
    ModelInfo {
        id: "3b",
        name: "Qwen2.5-Coder-3B",
        description: "Better — deeper reasoning, handles complex codebases well",
        gguf_filename: "qwen2.5-coder-3b-instruct-q4_k_m.gguf",
        gguf_url: "https://huggingface.co/Qwen/Qwen2.5-Coder-3B-Instruct-GGUF/resolve/main/qwen2.5-coder-3b-instruct-q4_k_m.gguf",
        size_gb: 2.1,
        min_ram_gb: 6,
        min_disk_gb: 3,
    },
    ModelInfo {
        id: "7b",
        name: "Qwen2.5-Coder-7B",
        description: "High quality — strong security analysis",
        gguf_filename: "qwen2.5-coder-7b-instruct-q4_k_m.gguf",
        gguf_url: "https://huggingface.co/Qwen/Qwen2.5-Coder-7B-Instruct-GGUF/resolve/main/qwen2.5-coder-7b-instruct-q4_k_m.gguf",
        size_gb: 4.7,
        min_ram_gb: 10,
        min_disk_gb: 6,
    },
    ModelInfo {
        id: "14b",
        name: "Qwen2.5-Coder-14B",
        description: "Expert — deep code understanding, workstation recommended",
        gguf_filename: "qwen2.5-coder-14b-instruct-q4_k_m.gguf",
        gguf_url: "https://huggingface.co/Qwen/Qwen2.5-Coder-14B-Instruct-GGUF/resolve/main/qwen2.5-coder-14b-instruct-q4_k_m.gguf",
        size_gb: 9.0,
        min_ram_gb: 18,
        min_disk_gb: 11,
    },
    ModelInfo {
        id: "32b",
        name: "Qwen2.5-Coder-32B",
        description: "Maximum — near human-level review quality, server-grade hardware",
        gguf_filename: "qwen2.5-coder-32b-instruct-q4_k_m.gguf",
        gguf_url: "https://huggingface.co/Qwen/Qwen2.5-Coder-32B-Instruct-GGUF/resolve/main/qwen2.5-coder-32b-instruct-q4_k_m.gguf",
        size_gb: 20.0,
        min_ram_gb: 40,
        min_disk_gb: 22,
    },
];

// Shared tokenizer for all Qwen2.5-Coder variants
const TOKENIZER_URL: &str =
    "https://huggingface.co/Qwen/Qwen2.5-Coder-1.5B-Instruct/resolve/main/tokenizer.json";

/// Look up a model by its short ID (e.g. "1.5b", "7b").
pub fn find_model(id: &str) -> Option<&'static ModelInfo> {
    MODELS.iter().find(|m| m.id == id)
}

pub fn model_ids() -> Vec<&'static str> {
    MODELS.iter().map(|m| m.id).collect()
}

/// Sidecar recording what was downloaded, so a later run can tell a complete
/// file from a truncated one without re-hashing gigabytes every startup.
#[derive(serde::Serialize, serde::Deserialize)]
struct ModelReceipt {
    filename: String,
    bytes: u64,
    sha256: String,
    url: String,
}

fn receipt_path(model_dir: &Path, filename: &str) -> PathBuf {
    model_dir.join(format!("{filename}.receipt.json"))
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
            m.size_gb,
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
pub fn download_file(url: &str, dest: &Path) -> Result<(u64, String)> {
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

        Ok((downloaded, format!("{:x}", hasher.finalize())))
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

fn write_receipt(model_dir: &Path, filename: &str, bytes: u64, sha256: &str, url: &str) {
    let receipt = ModelReceipt {
        filename: filename.to_string(),
        bytes,
        sha256: sha256.to_string(),
        url: url.to_string(),
    };
    if let Ok(json) = serde_json::to_string_pretty(&receipt) {
        let _ = fs::write(receipt_path(model_dir, filename), json);
    }
}

/// Verify a downloaded file against its receipt.
/// `Ok(None)` means there is no receipt to check against.
pub fn verify_file(model_dir: &Path, filename: &str) -> Result<Option<bool>> {
    let path = model_dir.join(filename);
    if !path.exists() {
        return Ok(Some(false));
    }
    let receipt_file = receipt_path(model_dir, filename);
    if !receipt_file.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&receipt_file)?;
    let receipt: ModelReceipt = serde_json::from_str(&raw)?;

    // Size is the cheap check and catches every truncation; only hash when it
    // passes, because hashing 20 GB on every startup is not acceptable.
    let actual_len = fs::metadata(&path)?.len();
    if actual_len != receipt.bytes {
        return Ok(Some(false));
    }
    let (_, digest) = hash_file(&path)?;
    Ok(Some(digest == receipt.sha256))
}

/// Quick integrity check run before loading: size only, so it costs nothing.
pub fn looks_truncated(model_dir: &Path, filename: &str) -> bool {
    let path = model_dir.join(filename);
    let receipt_file = receipt_path(model_dir, filename);
    let (Ok(meta), Ok(raw)) = (fs::metadata(&path), fs::read_to_string(&receipt_file)) else {
        return false;
    };
    serde_json::from_str::<ModelReceipt>(&raw)
        .map(|r| meta.len() != r.bytes)
        .unwrap_or(false)
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
    let tokenizer_path = model_dir.join("tokenizer.json");

    if force {
        for p in [&model_path, &tokenizer_path] {
            let _ = fs::remove_file(p);
        }
        let _ = fs::remove_file(receipt_path(model_dir, model.gguf_filename));
        let _ = fs::remove_file(receipt_path(model_dir, "tokenizer.json"));
        println!("Existing files removed. Re-downloading...\n");
    }

    // Tokenizer (shared across all models)
    if tokenizer_path.exists() && !looks_truncated(model_dir, "tokenizer.json") {
        println!("tokenizer.json already present (use --force to re-download).");
    } else {
        println!("Downloading tokenizer.json...");
        let (bytes, digest) = download_file(TOKENIZER_URL, &tokenizer_path)?;
        write_receipt(model_dir, "tokenizer.json", bytes, &digest, TOKENIZER_URL);
    }

    // Model weights
    if model_path.exists() && !looks_truncated(model_dir, model.gguf_filename) {
        println!(
            "{} already present (use --force to re-download).",
            model.gguf_filename
        );
    } else {
        if model_path.exists() {
            println!(
                "Existing {} is incomplete — re-downloading.",
                model.gguf_filename
            );
            let _ = fs::remove_file(&model_path);
        }
        println!(
            "Downloading {} ({:.1} GB)...",
            model.gguf_filename, model.size_gb
        );
        let (bytes, digest) = download_file(model.gguf_url, &model_path)?;
        write_receipt(
            model_dir,
            model.gguf_filename,
            bytes,
            &digest,
            model.gguf_url,
        );
        println!("\nModel ready: {}", model_path.display());
        println!("  sha256  {digest}");
    }

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
    for filename in [model.gguf_filename, "tokenizer.json"] {
        print!("  {filename} ... ");
        io::stdout().flush()?;
        match verify_file(model_dir, filename)? {
            Some(true) => println!("ok"),
            Some(false) => {
                println!("CORRUPT or MISSING");
                all_ok = false;
            }
            None => println!("no checksum on record (downloaded by an older version)"),
        }
    }

    if all_ok {
        println!("\n  All files verified.");
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
                m.gguf_url.ends_with(m.gguf_filename),
                "{}: url and filename disagree, so the receipt would be written \
                 against a file that was never fetched",
                m.id
            );
            assert!(
                m.min_disk_gb as f64 >= m.size_gb,
                "{}: disk floor below download size",
                m.id
            );
            assert!(
                m.min_ram_gb as f64 > m.size_gb,
                "{}: RAM floor below weights",
                m.id
            );
        }
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

    #[test]
    fn verify_reports_missing_files_as_failures() {
        let dir = std::env::temp_dir().join(format!("diffmind-verify-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(verify_file(&dir, "nope.gguf").unwrap(), Some(false));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_size_mismatch_is_detected_without_hashing() {
        let dir = std::env::temp_dir().join(format!("diffmind-trunc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("m.gguf"), b"12345").unwrap();
        write_receipt(&dir, "m.gguf", 999, "deadbeef", "http://x");

        assert!(looks_truncated(&dir, "m.gguf"));
        assert_eq!(verify_file(&dir, "m.gguf").unwrap(), Some(false));

        // A correct receipt passes both checks.
        let (bytes, digest) = hash_file(&dir.join("m.gguf")).unwrap();
        write_receipt(&dir, "m.gguf", bytes, &digest, "http://x");
        assert!(!looks_truncated(&dir, "m.gguf"));
        assert_eq!(verify_file(&dir, "m.gguf").unwrap(), Some(true));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_receipt_is_unknown_not_corrupt() {
        let dir = std::env::temp_dir().join(format!("diffmind-noreceipt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("m.gguf"), b"x").unwrap();
        assert_eq!(
            verify_file(&dir, "m.gguf").unwrap(),
            None,
            "a model from an older version must not be reported as corrupt"
        );
        assert!(!looks_truncated(&dir, "m.gguf"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
