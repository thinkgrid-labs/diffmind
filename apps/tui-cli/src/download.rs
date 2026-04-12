use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use std::fs;
use std::io::{Read, Write};
use std::path::Path;

pub fn download_file(url: &str, dest: &Path) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .user_agent("diffmind/0.1.0")
        .build()?;

    let mut response = client.get(url).send()?;
    if !response.status().is_success() {
        return Err(anyhow::anyhow!("Failed to download {}: {}", url, response.status()));
    }

    let total_size = response
        .content_length()
        .context("Failed to get content length")?;

    let pb = ProgressBar::new(total_size);
    pb.set_style(ProgressStyle::default_bar()
        .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta})")?
        .progress_chars("#>-"));

    let mut file = fs::File::create(dest)?;
    let mut buffer = [0; 8192];
    let mut downloaded = 0;

    while let Ok(n) = response.read(&mut buffer) {
        if n == 0 {
            break;
        }
        file.write_all(&buffer[..n])?;
        downloaded += n as u64;
        pb.set_position(downloaded);
    }

    pb.finish_with_message("Download complete");
    Ok(())
}

pub fn ensure_model_files(model_id: &str, model_dir: &Path, force: bool) -> Result<()> {
    if !model_dir.exists() {
        fs::create_dir_all(model_dir)?;
    }

    let (model_url, model_filename) = match model_id {
        "1.5b" => (
            "https://huggingface.co/Qwen/Qwen2.5-Coder-1.5B-Instruct-GGUF/resolve/main/qwen2.5-coder-1.5b-instruct-q4_k_m.gguf",
            "qwen2.5-coder-1.5b-instruct-q4_k_m.gguf"
        ),
        "3b" => (
            "https://huggingface.co/Qwen/Qwen2.5-Coder-3B-Instruct-GGUF/resolve/main/qwen2.5-coder-3b-instruct-q4_k_m.gguf",
            "qwen2.5-coder-3b-instruct-q4_k_m.gguf"
        ),
        _ => return Err(anyhow::anyhow!("Unsupported model: {}. Valid options: 1.5b, 3b", model_id)),
    };

    let model_path = model_dir.join(model_filename);
    let tokenizer_path = model_dir.join("tokenizer.json");
    let tokenizer_url = "https://huggingface.co/Qwen/Qwen2.5-Coder-1.5B-Instruct/resolve/main/tokenizer.json";

    if force {
        if tokenizer_path.exists() {
            fs::remove_file(&tokenizer_path)?;
        }
        if model_path.exists() {
            fs::remove_file(&model_path)?;
        }
        println!("Existing model files removed. Re-downloading...");
    }

    if !tokenizer_path.exists() {
        println!("Downloading tokenizer.json...");
        download_file(tokenizer_url, &tokenizer_path)?;
    } else {
        println!("tokenizer.json already exists, skipping. Use --force to re-download.");
    }

    if !model_path.exists() {
        println!("Downloading {}...", model_filename);
        download_file(model_url, &model_path)?;
    } else {
        println!("{} already exists, skipping. Use --force to re-download.", model_filename);
    }

    Ok(())
}
