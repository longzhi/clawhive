# `clawhive update` Self-Update Command Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add a `clawhive update` / `clawhive upgrade` command that self-updates the binary from GitHub Releases with strict channel isolation and SHA256 verification.

**Architecture:** Flat CLI command (no subcommands) with `--check`, `--channel`, `--version`, `--yes` flags. Fetches GitHub Releases API, filters by semver channel, downloads asset + sha256 sidecar, verifies, and atomically replaces via `self-replace` crate. Target triple baked in at compile time via `build.rs`.

**Tech Stack:** `semver` (version parsing), `sha2` (hash verification), `self-replace` (atomic binary replacement), `reqwest` (already present), `serde_json` (already present)

**Spec:** See `docs/plans/../../Obsidian` or the TL;DR in this file's appendix.

---

### Task 1: Add new dependencies

**Files:**
- Modify: `Cargo.toml` (workspace root, `[workspace.dependencies]`)
- Modify: `crates/clawhive-cli/Cargo.toml` (`[dependencies]`)

**Step 1: Add workspace dependencies**

In root `Cargo.toml`, add to `[workspace.dependencies]` section:

```toml
semver = "1"
sha2 = "0.10"
self-replace = "1"
```

**Step 2: Add CLI crate dependencies**

In `crates/clawhive-cli/Cargo.toml`, add to `[dependencies]`:

```toml
semver.workspace = true
sha2.workspace = true
self-replace.workspace = true
```

**Step 3: Verify it compiles**

Run: `cargo check -p clawhive-cli`
Expected: OK (no usage yet, just dep resolution)

---

### Task 2: Add `build.rs` to expose TARGET triple

**Files:**
- Create: `crates/clawhive-cli/build.rs`

**Step 1: Create build.rs**

```rust
fn main() {
    println!(
        "cargo:rustc-env=TARGET={}",
        std::env::var("TARGET").unwrap()
    );
}
```

This makes `env!("TARGET")` available at compile time (e.g. `"aarch64-apple-darwin"`).

**Step 2: Verify**

Run: `cargo check -p clawhive-cli`
Expected: OK

---

### Task 3: Add `Update` variant to CLI Commands enum

**Files:**
- Modify: `crates/clawhive-cli/src/main.rs`

**Step 1: Add Update variant to `Commands` enum (after `Setup` variant, before closing `}`)**

Around line 145 (after `Setup { force: bool }`), add:

```rust
    #[command(about = "Update clawhive to the latest version", alias = "upgrade")]
    Update {
        #[arg(long, help = "Check for updates without installing")]
        check: bool,
        #[arg(long, help = "Update channel (alpha, beta, rc, stable)")]
        channel: Option<String>,
        #[arg(long, help = "Install a specific version")]
        version: Option<String>,
        #[arg(long, short = 'y', help = "Skip confirmation prompt")]
        yes: bool,
    },
```

**Step 2: Add dispatch arm in the `match command` block**

After `Commands::Setup` arm (around line 1021), before the closing `}`:

```rust
        Commands::Update {
            check,
            channel,
            version,
            yes,
        } => {
            commands::update::handle_update(check, channel, version, yes).await?;
        }
```

**Step 3: Verify it compiles (will fail — module doesn't exist yet)**

Run: `cargo check -p clawhive-cli`
Expected: error about missing `commands::update` module — that's correct, we create it next.

---

### Task 4: Create `commands/update.rs` — module skeleton + channel parsing

**Files:**
- Create: `crates/clawhive-cli/src/commands/update.rs`
- Modify: `crates/clawhive-cli/src/commands/mod.rs`

**Step 1: Register the module**

In `commands/mod.rs`, add:

```rust
pub mod update;
```

**Step 2: Create update.rs with skeleton + channel parsing**

```rust
use anyhow::{bail, Context, Result};
use semver::Version;

const REPO: &str = "longzhi/clawhive";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const TARGET: &str = env!("TARGET");

/// Derive update channel from semver pre-release identifier.
/// "0.1.0-alpha.15" → "alpha", "1.0.0" → "stable"
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

/// Check if a release version belongs to the given channel.
fn version_matches_channel(ver: &Version, channel: &str) -> bool {
    channel_from_version(ver) == channel
}

pub async fn handle_update(
    check_only: bool,
    channel_override: Option<String>,
    target_version: Option<String>,
    yes: bool,
) -> Result<()> {
    let current = Version::parse(CURRENT_VERSION)
        .context("failed to parse current version")?;
    let channel = channel_override
        .as_deref()
        .unwrap_or_else(|| channel_from_version(&current));

    println!("Current version: {current} (channel: {channel}, target: {TARGET})");

    // TODO: fetch releases, filter, download, verify, replace
    println!("Update command not yet fully implemented.");

    Ok(())
}
```

**Step 3: Verify it compiles**

Run: `cargo check -p clawhive-cli`
Expected: OK with warnings about unused variables

---

### Task 5: Implement GitHub Releases fetching + version filtering

**Files:**
- Modify: `crates/clawhive-cli/src/commands/update.rs`

**Step 1: Add release fetching and filtering**

Add these types and functions to update.rs:

```rust
use std::time::Duration;

#[derive(Debug)]
struct ReleaseInfo {
    version: Version,
    tag: String,
    asset_url: String,
    sha256_url: String,
}

/// Fetch all releases from GitHub, filter by channel, return sorted descending.
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
        .context("failed to fetch releases")?;

    if !resp.status().is_success() {
        bail!("GitHub API returned HTTP {}", resp.status());
    }

    let releases: Vec<serde_json::Value> = resp.json().await?;
    let asset_suffix = format!("-{TARGET}.tar.gz");

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
        // Find matching platform asset
        let assets = release["assets"].as_array();
        let Some(assets) = assets else { continue };
        let mut asset_url = None;
        let mut sha256_url = None;
        for asset in assets {
            let name = asset["name"].as_str().unwrap_or_default();
            let dl = asset["browser_download_url"].as_str().unwrap_or_default();
            if name.ends_with(&asset_suffix) {
                asset_url = Some(dl.to_string());
            }
            if name.ends_with(&format!("-{TARGET}.sha256")) {
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
```

**Step 2: Wire into handle_update**

Replace the TODO in `handle_update`:

```rust
    // If specific version requested, find it
    if let Some(ref target_ver) = target_version {
        let ver = Version::parse(target_ver)
            .context("invalid version format")?;
        let releases = fetch_releases(channel_from_version(&ver)).await?;
        let Some(release) = releases.into_iter().find(|r| r.version == ver) else {
            bail!("Version {ver} not found for target {TARGET}");
        };
        if check_only {
            println!("Version {ver} is available.");
            return Ok(());
        }
        return do_update(&current, &release, yes).await;
    }

    // Find latest in channel
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

    do_update(&current, latest, yes).await
```

**Step 3: Add placeholder do_update**

```rust
async fn do_update(current: &Version, release: &ReleaseInfo, yes: bool) -> Result<()> {
    println!("Update available: {current} → {}", release.version);
    // TODO: confirm, download, verify, replace
    Ok(())
}
```

**Step 4: Verify**

Run: `cargo check -p clawhive-cli`
Expected: OK

---

### Task 6: Implement download + SHA256 verification + binary replacement

**Files:**
- Modify: `crates/clawhive-cli/src/commands/update.rs`

**Step 1: Add imports**

```rust
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
```

**Step 2: Implement do_update fully**

```rust
async fn do_update(current: &Version, release: &ReleaseInfo, yes: bool) -> Result<()> {
    println!("Update available: {current} → {}", release.version);

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

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(300))
        .build()?;

    // 1. Download sha256
    println!("Downloading checksum...");
    let sha256_resp = client
        .get(&release.sha256_url)
        .header("user-agent", format!("clawhive/{CURRENT_VERSION}"))
        .send()
        .await
        .context("failed to download sha256")?;
    let sha256_text = sha256_resp.text().await?;
    let expected_hash = sha256_text
        .split_whitespace()
        .next()
        .context("malformed sha256 file")?
        .to_lowercase();

    // 2. Download tarball
    println!("Downloading clawhive {}...", release.version);
    let tar_resp = client
        .get(&release.asset_url)
        .header("user-agent", format!("clawhive/{CURRENT_VERSION}"))
        .send()
        .await
        .context("failed to download release")?;
    let tar_bytes = tar_resp.bytes().await?;

    // 3. Verify SHA256
    let mut hasher = Sha256::new();
    hasher.update(&tar_bytes);
    let actual_hash = format!("{:x}", hasher.finalize());
    if actual_hash != expected_hash {
        bail!(
            "SHA256 mismatch!\n  expected: {expected_hash}\n  actual:   {actual_hash}"
        );
    }
    println!("Checksum verified.");

    // 4. Extract binary from tarball to temp dir
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

    // 5. Atomic self-replace
    let new_binary = tmp_dir.path().join("clawhive");
    self_replace::self_replace(&new_binary)
        .context("failed to replace binary")?;

    println!(
        "Updated successfully: {current} → {}\nRestart clawhive for the new version to take effect.",
        release.version
    );
    Ok(())
}
```

**Step 3: Verify**

Run: `cargo check -p clawhive-cli`
Expected: OK

---

### Task 7: Add tests for channel parsing

**Files:**
- Modify: `crates/clawhive-cli/src/commands/update.rs`

**Step 1: Add test module at bottom of update.rs**

```rust
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
            assert_eq!(
                channel_from_version(&ver),
                expected,
                "failed for {ver_str}"
            );
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
```

**Step 2: Run tests**

Run: `cargo test -p clawhive-cli -- update`
Expected: 2 tests pass

---

### Task 8: Full build + manual verification

**Step 1: Full workspace build**

Run: `cargo build --workspace`
Expected: OK

**Step 2: Run all tests**

Run: `cargo test --workspace`
Expected: All pass

**Step 3: Verify CLI help shows update command**

Run: `cargo run -- --help`
Expected: `update` appears in Commands list, `upgrade` works as alias

**Step 4: Verify --check flag**

Run: `cargo run -- update --check`
Expected: Checks GitHub and prints version info (or "already up to date")

---

## Appendix: Spec Summary

- Command: `clawhive update` (main), `clawhive upgrade` (alias)
- Flags: `--check`, `--channel <channel>`, `--version <semver>`, `--yes` / `-y`
- Channels: strict isolation — alpha/beta/rc/stable, auto-inferred from current version
- Source: GitHub Releases API (`longzhi/clawhive`)
- Asset format: `clawhive-{tag}-{target}.tar.gz` + `.sha256`
- Security: HTTPS + SHA256 verification
- Replacement: `self-replace` crate (atomic, cross-platform)
- No auto-restart: print message, user restarts manually
