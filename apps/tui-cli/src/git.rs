use std::process::Command;
use anyhow::{Result, Context};

pub fn get_diff(branch: &str, paths: &[String]) -> Result<String> {
    let branch_arg = format!("{}...HEAD", branch);
    let mut args = vec!["diff", &branch_arg, "--"];
    for path in paths {
        args.push(path);
    }
    
    args.extend_from_slice(&[
        ":!node_modules",
        ":!*-lock.json",
        ":!pnpm-lock.yaml",
        ":!package-lock.json",
        ":!yarn.lock",
        ":!dist",
        ":!build",
        ":!.next",
        ":!.cache",
        ":!*.map",
        ":!*.min.js",
        ":!*.min.css",
    ]);

    let output = Command::new("git")
        .args(&args)
        .output()
        .context("Failed to execute git command")?;

    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!("Git error: {}", err));
    }

    let diff = String::from_utf8_lossy(&output.stdout).to_string();
    
    let size_kb = diff.len() / 1024;
    if size_kb > 1500 {
        return Err(anyhow::anyhow!("Diff too large to process ({}KB). Try reviewing specific files.", size_kb));
    }

    Ok(diff)
}

