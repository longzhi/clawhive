use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub(crate) fn run(root: &Path, lines: usize) -> Result<()> {
    let log_dir = root.join("logs");
    if !log_dir.exists() {
        anyhow::bail!(
            "Log directory not found: {}. Has clawhive been started yet?",
            log_dir.display()
        );
    }

    // Collect all files that start with "clawhive.log"
    let mut log_files: Vec<PathBuf> = std::fs::read_dir(&log_dir)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("clawhive.log"))
                .unwrap_or(false)
        })
        .collect();

    if log_files.is_empty() {
        anyhow::bail!("No log files found in {}", log_dir.display());
    }

    // Daily rolling files are named "clawhive.log.YYYY-MM-DD",
    // so lexicographic sort gives us the latest file last.
    log_files.sort();
    let latest = log_files.last().unwrap();

    eprintln!("Following: {}", latest.display());

    let status = std::process::Command::new("tail")
        .arg(format!("-n{lines}"))
        .arg("-f")
        .arg(latest)
        .status()
        .context("Failed to run `tail`. Make sure it is installed.")?;

    if !status.success() {
        anyhow::bail!("`tail` exited with status: {status}");
    }

    Ok(())
}
