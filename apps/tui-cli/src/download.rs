use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
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
        description: "High quality — strong security analysis, needs 8 GB+ RAM",
        gguf_filename: "qwen2.5-coder-7b-instruct-q4_k_m.gguf",
        gguf_url: "https://huggingface.co/Qwen/Qwen2.5-Coder-7B-Instruct-GGUF/resolve/main/qwen2.5-coder-7b-instruct-q4_k_m.gguf",
        size_gb: 4.7,
        min_ram_gb: 8,
        min_disk_gb: 6,
    },
    ModelInfo {
        id: "14b",
        name: "Qwen2.5-Coder-14B",
        description: "Expert — deep code understanding, workstation recommended",
        gguf_filename: "qwen2.5-coder-14b-instruct-q4_k_m.gguf",
        gguf_url: "https://huggingface.co/Qwen/Qwen2.5-Coder-14B-Instruct-GGUF/resolve/main/qwen2.5-coder-14b-instruct-q4_k_m.gguf",
        size_gb: 9.0,
        min_ram_gb: 16,
        min_disk_gb: 11,
    },
    ModelInfo {
        id: "32b",
        name: "Qwen2.5-Coder-32B",
        description: "Maximum — near human-level review quality, server-grade hardware",
        gguf_filename: "qwen2.5-coder-32b-instruct-q4_k_m.gguf",
        gguf_url: "https://huggingface.co/Qwen/Qwen2.5-Coder-32B-Instruct-GGUF/resolve/main/qwen2.5-coder-32b-instruct-q4_k_m.gguf",
        size_gb: 20.0,
        min_ram_gb: 32,
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

// ─── Interactive model picker ─────────────────────────────────────────────────

fn prompt_model_selection() -> Result<&'static ModelInfo> {
    println!("\nAvailable models — Qwen2.5-Coder (coding-optimised, Q4_K_M):\n");
    println!(
        "  {:<4}  {:<26}  {:>7}  {:>8}  {}",
        "#", "Model", "Size", "Min RAM", "Description"
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
            .context("Please enter a number")?
            .checked_sub(1)
            .ok_or_else(|| anyhow::anyhow!("Selection must be at least 1"))?
    };

    MODELS.get(idx).ok_or_else(|| {
        anyhow::anyhow!(
            "Invalid selection '{}'. Enter a number between 1 and {}.",
            trimmed,
            MODELS.len()
        )
    })
}

// ─── Hardware requirements check ─────────────────────────────────────────────

/// Reads system RAM and free disk space, prints a requirements table, and asks
/// the user to confirm if any requirement is not met.
/// Returns `Err` only if the user explicitly declines to proceed.
fn check_requirements(model: &ModelInfo, model_dir: &Path) -> Result<()> {
    use sysinfo::{Disks, System};

    // RAM
    let mut sys = System::new();
    sys.refresh_memory();
    let total_ram_gb = sys.total_memory() as f64 / 1_073_741_824.0; // bytes → GB

    // Disk — find the most-specific mount point that contains model_dir
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
        // Prefer the most-specific (longest) mount point
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
            "\n  WARNING: Only {:.1} GB disk space free, {:.1} GB needed for {}.",
            free_disk_gb, model.min_disk_gb as f64, model.name
        );
        println!("  Free up space or choose a smaller model.");
    }

    if !ram_ok || !disk_ok {
        print!("\n  Proceed with download anyway? [y/N]: ");
        io::stdout().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !answer.trim().eq_ignore_ascii_case("y") {
            return Err(anyhow::anyhow!("Download cancelled."));
        }
    } else {
        println!("  All requirements met.\n");
    }

    Ok(())
}

// ─── File download ─────────────────────────────────────────────────────────────

pub fn download_file(url: &str, dest: &Path) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("diffmind/0.5.0")
        .build()?;

    let mut response = client.get(url).send()?;
    if !response.status().is_success() {
        return Err(anyhow::anyhow!(
            "Failed to download {}: {}",
            url,
            response.status()
        ));
    }

    let total_size = response
        .content_length()
        .context("Server did not return Content-Length")?;

    let pb = ProgressBar::new(total_size);
    pb.set_style(
        ProgressStyle::default_bar()
            .template(
                "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta})",
            )?
            .progress_chars("#>-"),
    );

    let mut file = fs::File::create(dest)?;
    let mut buffer = [0u8; 8192];
    let mut downloaded = 0u64;

    while let Ok(n) = response.read(&mut buffer) {
        if n == 0 {
            break;
        }
        file.write_all(&buffer[..n])?;
        downloaded += n as u64;
        pb.set_position(downloaded);
    }

    pb.finish_with_message("done");
    Ok(())
}

// ─── Public entry point ───────────────────────────────────────────────────────

/// Download model files to `model_dir`.
///
/// - `model_id = None`  → show interactive picker
/// - `model_id = Some`  → skip picker, validate ID, download directly
/// - `force = true`     → delete existing files and re-download
pub fn ensure_model_files(model_id: Option<&str>, model_dir: &Path, force: bool) -> Result<()> {
    if !model_dir.exists() {
        fs::create_dir_all(model_dir)?;
    }

    // Resolve which model to use
    let model: &ModelInfo = match model_id {
        Some(id) => find_model(id).ok_or_else(|| {
            let valid: Vec<_> = MODELS.iter().map(|m| m.id).collect();
            anyhow::anyhow!(
                "Unknown model '{}'. Valid options: {}",
                id,
                valid.join(", ")
            )
        })?,
        None => prompt_model_selection()?,
    };

    println!("\nSelected: {} — {}", model.name, model.description);

    // Hardware check
    check_requirements(model, model_dir)?;

    let model_path = model_dir.join(model.gguf_filename);
    let tokenizer_path = model_dir.join("tokenizer.json");

    // Handle --force
    if force {
        if model_path.exists() {
            fs::remove_file(&model_path)?;
        }
        if tokenizer_path.exists() {
            fs::remove_file(&tokenizer_path)?;
        }
        println!("Existing files removed. Re-downloading...\n");
    }

    // Tokenizer (shared across all models)
    if !tokenizer_path.exists() {
        println!("Downloading tokenizer.json...");
        download_file(TOKENIZER_URL, &tokenizer_path)?;
    } else if !force {
        println!("tokenizer.json already present (use --force to re-download).");
    }

    // Model weights
    if !model_path.exists() {
        println!("Downloading {} ({:.1} GB)...", model.gguf_filename, model.size_gb);
        download_file(model.gguf_url, &model_path)?;
        println!("\nModel ready: {}", model_path.display());
    } else if !force {
        println!(
            "{} already present (use --force to re-download).",
            model.gguf_filename
        );
    }

    Ok(())
}
