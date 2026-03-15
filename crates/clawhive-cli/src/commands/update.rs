use anyhow::{bail, Context, Result};
use semver::Version;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::time::Duration;

const REPO: &str = "longzhi/clawhive";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const TARGET: &str = env!("TARGET");

fn channel_from_version(ver: &Version) -> &str {
    let pre = ver.pre.as_str();
    if pre.starts_with("alpha") {
        "alpha"
    } else if pre.starts_with("beta") {
        "beta"
    } else if pre.starts_with("rc") {
        "rc"
    } else {
        "stable"
    }
}

fn version_matches_channel(ver: &Version, channel: &str) -> bool {
    channel_from_version(ver) == channel
}

#[derive(Debug)]
#[allow(dead_code)]
struct ReleaseInfo {
    version: Version,
    tag: String,
    asset_url: String,
    sha256_url: String,
}

async fn fetch_releases(channel: &str) -> Result<Vec<ReleaseInfo>> {
    let url = format!("https://api.github.com/repos/{REPO}/releases");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()?;
    let resp = client
        .get(&url)
        .header("user-agent", format!("clawhive/{CURRENT_VERSION}"))
        .header("accept", "application/vnd.github+json")
        .send()
        .await
        .context("failed to fetch releases from GitHub")?;

    if !resp.status().is_success() {
        bail!("GitHub API returned HTTP {}", resp.status());
    }

    let releases: Vec<serde_json::Value> = resp.json().await?;
    let asset_suffix = format!("-{TARGET}.tar.gz");
    let sha_suffix = format!("-{TARGET}.sha256");

    let mut matched: Vec<ReleaseInfo> = Vec::new();
    for release in &releases {
        let tag = release["tag_name"].as_str().unwrap_or_default();
        let ver_str = tag.strip_prefix('v').unwrap_or(tag);
        let Ok(ver) = Version::parse(ver_str) else {
            continue;
        };
        if !version_matches_channel(&ver, channel) {
            continue;
        }
        let Some(assets) = release["assets"].as_array() else {
            continue;
        };
        let mut asset_url = None;
        let mut sha256_url = None;
        for asset in assets {
            let name = asset["name"].as_str().unwrap_or_default();
            let dl = asset["browser_download_url"].as_str().unwrap_or_default();
            if name.ends_with(&asset_suffix) {
                asset_url = Some(dl.to_string());
            }
            if name.ends_with(&sha_suffix) {
                sha256_url = Some(dl.to_string());
            }
        }
        if let (Some(asset_url), Some(sha256_url)) = (asset_url, sha256_url) {
            matched.push(ReleaseInfo {
                version: ver,
                tag: tag.to_string(),
                asset_url,
                sha256_url,
            });
        }
    }

    matched.sort_by(|a, b| b.version.cmp(&a.version));
    Ok(matched)
}

async fn download_and_replace(release: &ReleaseInfo) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()?;

    println!("Downloading checksum...");
    let sha256_resp = client
        .get(&release.sha256_url)
        .header("user-agent", format!("clawhive/{CURRENT_VERSION}"))
        .send()
        .await
        .context("failed to download checksum")?;
    let sha256_text = sha256_resp.text().await?;
    let expected_hash = sha256_text
        .split_whitespace()
        .next()
        .context("malformed sha256 file")?
        .to_lowercase();

    println!("Downloading clawhive {}...", release.version);
    let tar_resp = client
        .get(&release.asset_url)
        .header("user-agent", format!("clawhive/{CURRENT_VERSION}"))
        .send()
        .await
        .context("failed to download release")?;
    let tar_bytes = tar_resp.bytes().await?;

    let mut hasher = Sha256::new();
    hasher.update(&tar_bytes);
    let actual_hash = format!("{:x}", hasher.finalize());
    if actual_hash != expected_hash {
        bail!("SHA256 mismatch!\n  expected: {expected_hash}\n  actual:   {actual_hash}");
    }
    println!("Checksum verified.");

    let tmp_dir = tempfile::tempdir().context("failed to create temp dir")?;
    let decoder = flate2::read::GzDecoder::new(std::io::Cursor::new(&tar_bytes));
    let mut archive = tar::Archive::new(decoder);
    let mut found_binary = false;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_path_buf();
        let file_name = path.file_name().unwrap_or_default().to_string_lossy();
        if file_name == "clawhive" {
            let dest = tmp_dir.path().join("clawhive");
            entry.unpack(&dest)?;
            found_binary = true;
            break;
        }
    }
    if !found_binary {
        bail!("Binary 'clawhive' not found in archive");
    }

    let new_binary = tmp_dir.path().join("clawhive");
    self_replace::self_replace(&new_binary).context("failed to replace binary")?;

    Ok(())
}

pub async fn handle_update(
    root: &std::path::Path,
    check_only: bool,
    channel_override: Option<String>,
    target_version: Option<String>,
    yes: bool,
) -> Result<()> {
    let current = Version::parse(CURRENT_VERSION).context("failed to parse current version")?;
    let channel = channel_override
        .as_deref()
        .unwrap_or_else(|| channel_from_version(&current));

    println!("Current version: {current} (channel: {channel}, target: {TARGET})");

    if let Some(ref target_ver) = target_version {
        let ver = Version::parse(target_ver).context("invalid version format")?;
        let releases = fetch_releases(channel_from_version(&ver)).await?;
        let Some(release) = releases.into_iter().find(|r| r.version == ver) else {
            bail!("Version {ver} not found for target {TARGET}");
        };
        if check_only {
            println!("Version {ver} is available.");
            return Ok(());
        }
        println!("Update available: {current} → {ver}");
        if !yes {
            print!("Proceed? [y/N] ");
            std::io::stdout().flush()?;
            let mut input = String::new();
            std::io::stdin().read_line(&mut input)?;
            if !input.trim().eq_ignore_ascii_case("y") {
                println!("Aborted.");
                return Ok(());
            }
        }
        download_and_replace(&release).await?;
        println!("Updated successfully: {current} → {}", release.version);
        restart_if_running(root).await?;
        return Ok(());
    }

    let releases = fetch_releases(channel).await?;
    let Some(latest) = releases.first() else {
        println!("No releases found for channel '{channel}' on {TARGET}.");
        return Ok(());
    };

    if latest.version <= current {
        println!("Already up to date ({current}).");
        return Ok(());
    }

    if check_only {
        println!("Update available: {current} → {}", latest.version);
        return Ok(());
    }

    println!("Update available: {current} → {}", latest.version);
    if !yes {
        print!("Proceed? [y/N] ");
        std::io::stdout().flush()?;
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Aborted.");
            return Ok(());
        }
    }

    download_and_replace(latest).await?;
    println!("Updated successfully: {current} → {}", latest.version);
    restart_if_running(root).await?;
    Ok(())
}

async fn restart_if_running(root: &std::path::Path) -> Result<()> {
    use crate::runtime::pid::{is_process_running, read_pid_file, read_port_file};

    let pid = read_pid_file(root)?;
    let running = pid.is_some_and(is_process_running);
    if !running {
        return Ok(());
    }

    println!("Restarting daemon...");
    let port = read_port_file(root)?.unwrap_or(8848);
    let security_override = None;
    crate::commands::start::run_restart(root, port, security_override).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_channel_from_version() {
        let cases = [
            ("0.1.0-alpha.15", "alpha"),
            ("0.1.0-alpha.1", "alpha"),
            ("1.0.0-beta.3", "beta"),
            ("1.0.0-rc.1", "rc"),
            ("1.0.0", "stable"),
            ("0.2.0", "stable"),
        ];
        for (ver_str, expected) in cases {
            let ver = Version::parse(ver_str).unwrap();
            assert_eq!(channel_from_version(&ver), expected, "failed for {ver_str}");
        }
    }

    #[test]
    fn test_version_matches_channel() {
        let alpha = Version::parse("0.1.0-alpha.15").unwrap();
        assert!(version_matches_channel(&alpha, "alpha"));
        assert!(!version_matches_channel(&alpha, "stable"));
        assert!(!version_matches_channel(&alpha, "beta"));

        let stable = Version::parse("1.0.0").unwrap();
        assert!(version_matches_channel(&stable, "stable"));
        assert!(!version_matches_channel(&stable, "alpha"));
    }
}
