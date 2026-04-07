use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{Cursor, Write};
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use anyhow::Result;

use crate::skill::SkillPermissions;

#[derive(Debug)]
pub enum ResolvedSkillSource {
    Local(PathBuf),
    Remote {
        _temp_dir: tempfile::TempDir,
        path: PathBuf,
        url: Option<String>,
    },
}

impl ResolvedSkillSource {
    pub fn local_path(&self) -> &Path {
        match self {
            Self::Local(p) => p.as_path(),
            Self::Remote { path, .. } => path.as_path(),
        }
    }

    pub fn resolved_url(&self) -> Option<&str> {
        match self {
            Self::Local(_) => None,
            Self::Remote { url, .. } => url.as_deref(),
        }
    }
}

#[derive(Debug, serde::Deserialize)]
struct InstallSkillFrontmatter {
    name: String,
    description: String,
    #[serde(default)]
    requires: crate::skill::SkillRequirements,
    #[serde(default)]
    permissions: Option<SkillPermissions>,
}

#[derive(Debug, Clone)]
pub struct SkillRiskFinding {
    pub severity: &'static str,
    pub file: PathBuf,
    pub line: usize,
    pub pattern: &'static str,
    pub reason: &'static str,
}

#[derive(Debug, Clone)]
pub struct SkillAnalysisReport {
    pub source: PathBuf,
    pub skill_name: String,
    pub description: String,
    pub requires: crate::skill::SkillRequirements,
    pub permissions: Option<SkillPermissions>,
    pub findings: Vec<SkillRiskFinding>,
}

impl SkillAnalysisReport {
    pub fn all_required_env_vars(&self) -> Vec<String> {
        let mut vars: Vec<String> = self.requires.env.clone();
        if let Some(ref perms) = self.permissions {
            for v in &perms.env {
                if !vars.contains(v) {
                    vars.push(v.clone());
                }
            }
        }
        vars
    }
}

#[derive(Debug, Clone)]
pub struct InstallResult {
    pub target: PathBuf,
    pub high_risk: bool,
}

pub const METADATA_FILE: &str = ".metadata.json";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SkillMetadata {
    pub source: Option<String>,
    pub resolved_url: Option<String>,
    pub installed_at: String,
    pub content_hash: String,
    #[serde(default)]
    pub high_risk_acknowledged: bool,
    #[serde(default)]
    pub env_vars_written: Vec<String>,
}

impl SkillMetadata {
    pub fn read_from(skill_dir: &Path) -> Option<Self> {
        let path = skill_dir.join(METADATA_FILE);
        let content = std::fs::read_to_string(&path).ok()?;
        serde_json::from_str(&content).ok()
    }

    pub fn write_to(&self, skill_dir: &Path) -> Result<()> {
        let path = skill_dir.join(METADATA_FILE);
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, json)?;
        Ok(())
    }
}

/// Update the `env_vars_written` field in an existing `.metadata.json`.
pub fn update_env_vars_written(skill_dir: &Path, vars: &[String]) -> Result<()> {
    let mut meta = SkillMetadata::read_from(skill_dir)
        .ok_or_else(|| anyhow::anyhow!("no .metadata.json in {}", skill_dir.display()))?;
    meta.env_vars_written = vars.to_vec();
    meta.write_to(skill_dir)
}

pub async fn resolve_skill_source(source: &str) -> Result<ResolvedSkillSource> {
    if source.starts_with("http://") || source.starts_with("https://") {
        let resolved = normalize_github_url(source);
        return download_remote_skill(&resolved.url, resolved.subpath.as_deref()).await;
    }

    let local = PathBuf::from(source);
    if !local.exists() {
        anyhow::bail!("skill source does not exist: {}", local.display());
    }

    if local.is_file() {
        let path_lc = local.to_string_lossy().to_lowercase();
        if path_lc.ends_with(".zip")
            || path_lc.ends_with(".tar.gz")
            || path_lc.ends_with(".tgz")
            || path_lc.ends_with(".tar")
        {
            return extract_local_archive(&local);
        }
    }

    Ok(ResolvedSkillSource::Local(local))
}

fn extract_local_archive(archive_path: &Path) -> Result<ResolvedSkillSource> {
    let body = std::fs::read(archive_path)?;
    let temp = tempfile::tempdir()?;
    let extract_root = temp.path().join("extracted-skill");
    std::fs::create_dir_all(&extract_root)?;

    let path_lc = archive_path.to_string_lossy().to_lowercase();
    if path_lc.ends_with(".zip") {
        extract_zip_bytes(&body, &extract_root)?;
    } else if path_lc.ends_with(".tar.gz") || path_lc.ends_with(".tgz") {
        extract_tar_gz_bytes(&body, &extract_root)?;
    } else if path_lc.ends_with(".tar") {
        extract_tar_bytes(&body, &extract_root)?;
    } else {
        anyhow::bail!("unsupported archive format: {}", archive_path.display());
    }

    let skill_root = find_skill_root(&extract_root)?;

    Ok(ResolvedSkillSource::Remote {
        _temp_dir: temp,
        path: skill_root,
        url: None,
    })
}

pub fn analyze_skill_source(source: &Path) -> Result<SkillAnalysisReport> {
    let skill_md = source.join("SKILL.md");
    if !skill_md.exists() {
        anyhow::bail!("{} missing SKILL.md", source.display());
    }

    let raw = std::fs::read_to_string(&skill_md)?;
    let fm = parse_skill_frontmatter(&raw)?;

    let mut findings = Vec::new();
    scan_path_recursive(source, &mut findings)?;

    Ok(SkillAnalysisReport {
        source: source.to_path_buf(),
        skill_name: fm.name,
        description: fm.description,
        requires: fm.requires,
        permissions: fm.permissions,
        findings,
    })
}

pub fn has_high_risk_findings(report: &SkillAnalysisReport) -> bool {
    report
        .findings
        .iter()
        .any(|f| f.severity == "high" || f.severity == "critical")
}

pub fn install_skill_from_analysis(
    config_root: &Path,
    skills_root: &Path,
    source: &Path,
    report: &SkillAnalysisReport,
    allow_high_risk: bool,
    original_source: Option<&str>,
    resolved_url: Option<&str>,
) -> Result<InstallResult> {
    const LEGACY_HASH_FILE: &str = ".content-hash";

    let high_risk = has_high_risk_findings(report);
    if high_risk && !allow_high_risk {
        anyhow::bail!("high-risk patterns detected; confirmation required before install");
    }

    let target = skills_root.join(&report.skill_name);
    let source_hash = compute_directory_content_hash(source)?;

    // Check existing hash: prefer .metadata.json, fall back to legacy .content-hash
    if target.exists() {
        let installed_hash = SkillMetadata::read_from(&target)
            .map(|m| m.content_hash)
            .or_else(|| {
                let legacy = target.join(LEGACY_HASH_FILE);
                std::fs::read_to_string(&legacy)
                    .ok()
                    .map(|s| s.trim().to_string())
            });

        if installed_hash.as_deref() == Some(source_hash.as_str()) {
            let audit_dir = config_root.join("logs");
            std::fs::create_dir_all(&audit_dir)?;
            let audit_path = audit_dir.join("skill-installs.jsonl");
            let event = serde_json::json!({
                "ts": chrono::Utc::now().to_rfc3339(),
                "skill": report.skill_name,
                "target": target,
                "findings": report.findings.len(),
                "high_risk": high_risk,
                "declared_permissions": report.permissions.is_some(),
                "skipped": true,
            });
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(audit_path)?;
            writeln!(f, "{}", serde_json::to_string(&event)?)?;

            return Ok(InstallResult { target, high_risk });
        }
    }

    if target.exists() {
        std::fs::remove_dir_all(&target)?;
    }
    copy_dir_recursive(source, &target)?;

    // Write .metadata.json instead of legacy .content-hash
    let metadata = SkillMetadata {
        source: original_source.map(|s| s.to_string()),
        resolved_url: resolved_url.map(|s| s.to_string()),
        installed_at: chrono::Utc::now().to_rfc3339(),
        content_hash: source_hash,
        high_risk_acknowledged: high_risk && allow_high_risk,
        env_vars_written: vec![], // populated by caller after env var prompts
    };
    metadata.write_to(&target)?;

    // Remove legacy .content-hash if present
    let legacy_hash = target.join(LEGACY_HASH_FILE);
    if legacy_hash.exists() {
        let _ = std::fs::remove_file(&legacy_hash);
    }

    let audit_dir = config_root.join("logs");
    std::fs::create_dir_all(&audit_dir)?;
    let audit_path = audit_dir.join("skill-installs.jsonl");
    let event = serde_json::json!({
        "ts": chrono::Utc::now().to_rfc3339(),
        "skill": report.skill_name,
        "target": target,
        "findings": report.findings.len(),
        "high_risk": high_risk,
        "declared_permissions": report.permissions.is_some(),
    });
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(audit_path)?;
    writeln!(f, "{}", serde_json::to_string(&event)?)?;

    Ok(InstallResult { target, high_risk })
}

#[derive(Debug, Clone)]
pub struct RemoveResult {
    pub skill_name: String,
    pub env_vars_hint: Vec<String>,
}

pub fn remove_skill(
    config_root: &Path,
    skills_root: &Path,
    skill_name: &str,
) -> Result<RemoveResult> {
    let target = skills_root.join(skill_name);
    if !target.exists() {
        anyhow::bail!("skill not found: {skill_name}");
    }

    let env_vars_hint = SkillMetadata::read_from(&target)
        .map(|m| m.env_vars_written)
        .unwrap_or_default();

    std::fs::remove_dir_all(&target)?;

    // Audit log
    let audit_dir = config_root.join("logs");
    std::fs::create_dir_all(&audit_dir)?;
    let audit_path = audit_dir.join("skill-installs.jsonl");
    let event = serde_json::json!({
        "ts": chrono::Utc::now().to_rfc3339(),
        "action": "remove",
        "skill": skill_name,
    });
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(audit_path)?;
    writeln!(f, "{}", serde_json::to_string(&event)?)?;

    Ok(RemoveResult {
        skill_name: skill_name.to_string(),
        env_vars_hint,
    })
}

pub fn render_skill_analysis(report: &SkillAnalysisReport) -> String {
    let mut lines = vec![
        format!("Skill source: {}", report.source.display()),
        format!("Skill name: {}", report.skill_name),
        format!("Description: {}", report.description),
    ];

    match &report.permissions {
        Some(perms) => {
            lines.push("Permissions declared in SKILL.md: yes".to_string());
            lines.extend(render_permissions_lines(perms));
        }
        None => {
            lines.push("Permissions declared in SKILL.md: no".to_string());
            lines.push(
                "Effective behavior: default deny-first sandbox policy will be used.".to_string(),
            );
        }
    }

    if report.findings.is_empty() {
        lines.push("Risk scan: no obvious unsafe patterns found.".to_string());
    } else {
        lines.push(format!("Risk scan findings ({}):", report.findings.len()));
        for f in &report.findings {
            lines.push(format!(
                "  [{}] {}:{} pattern='{}' {}",
                f.severity,
                f.file.display(),
                f.line,
                f.pattern,
                f.reason
            ));
        }
    }

    lines.join("\n")
}

fn render_permissions_lines(permissions: &SkillPermissions) -> Vec<String> {
    let mut out = vec!["Requested permissions:".to_string()];
    if !permissions.fs.read.is_empty() {
        out.push(format!("  fs.read: {}", permissions.fs.read.join(", ")));
    }
    if !permissions.fs.write.is_empty() {
        out.push(format!("  fs.write: {}", permissions.fs.write.join(", ")));
    }
    if !permissions.network.allow.is_empty() {
        out.push(format!(
            "  network.allow: {}",
            permissions.network.allow.join(", ")
        ));
    }
    if !permissions.exec.is_empty() {
        out.push(format!("  exec: {}", permissions.exec.join(", ")));
    }
    if !permissions.env.is_empty() {
        out.push(format!("  env: {}", permissions.env.join(", ")));
    }
    if !permissions.services.is_empty() {
        out.push(format!(
            "  services: {}",
            permissions
                .services
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    out
}

/// Result of GitHub URL normalization.
struct NormalizedGitHubUrl {
    url: String,
    /// Optional subpath within the archive to locate the skill (e.g. "skills/weather").
    subpath: Option<String>,
}

/// Normalize GitHub URLs to downloadable formats.
///
/// Supports:
/// - `github.com/user/repo` → archive tarball of default branch
/// - `github.com/user/repo/tree/branch/path` → archive tarball + subpath hint
/// - `github.com/user/repo/blob/branch/path/SKILL.md` → raw file URL
/// - Non-GitHub URLs → returned as-is
fn normalize_github_url(url: &str) -> NormalizedGitHubUrl {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return NormalizedGitHubUrl {
            url: url.to_string(),
            subpath: None,
        };
    };

    let host = parsed.host_str().unwrap_or("");
    if host != "github.com" && host != "www.github.com" {
        return NormalizedGitHubUrl {
            url: url.to_string(),
            subpath: None,
        };
    }

    let segments: Vec<&str> = parsed.path().trim_matches('/').split('/').collect();
    if segments.len() < 2 {
        return NormalizedGitHubUrl {
            url: url.to_string(),
            subpath: None,
        };
    }

    let owner = segments[0];
    let repo = segments[1];

    // github.com/user/repo/blob/branch/path/to/SKILL.md → raw URL (single file)
    // github.com/user/repo/blob/branch/sub/path (directory) → treat as tree URL
    if segments.len() >= 4 && segments[2] == "blob" {
        let branch = segments[3];
        let file_path = segments[4..].join("/");
        // If the last segment contains a dot, assume it's a file → raw URL.
        // Otherwise it's likely a directory → fall through to tree-style handling.
        let last = segments.last().unwrap_or(&"");
        if last.contains('.') {
            return NormalizedGitHubUrl {
                url: format!(
                    "https://raw.githubusercontent.com/{owner}/{repo}/{branch}/{file_path}"
                ),
                subpath: None,
            };
        }
        // Directory blob URL → archive + subpath (same as tree URL)
        let subpath = if segments.len() > 4 {
            Some(segments[4..].join("/"))
        } else {
            None
        };
        return NormalizedGitHubUrl {
            url: format!("https://github.com/{owner}/{repo}/archive/refs/heads/{branch}.tar.gz"),
            subpath,
        };
    }

    // github.com/user/repo/tree/branch/sub/path → archive + subpath
    if segments.len() >= 4 && segments[2] == "tree" {
        let branch = segments[3];
        let subpath = if segments.len() > 4 {
            Some(segments[4..].join("/"))
        } else {
            None
        };
        return NormalizedGitHubUrl {
            url: format!("https://github.com/{owner}/{repo}/archive/refs/heads/{branch}.tar.gz"),
            subpath,
        };
    }

    // github.com/user/repo → archive of default branch
    if segments.len() == 2 {
        return NormalizedGitHubUrl {
            url: format!("https://github.com/{owner}/{repo}/archive/refs/heads/main.tar.gz"),
            subpath: None,
        };
    }

    NormalizedGitHubUrl {
        url: url.to_string(),
        subpath: None,
    }
}

async fn download_remote_skill(url: &str, subpath: Option<&str>) -> Result<ResolvedSkillSource> {
    const MAX_DOWNLOAD_BYTES: usize = 20 * 1024 * 1024;

    let parsed =
        reqwest::Url::parse(url).map_err(|e| anyhow::anyhow!("invalid URL '{url}': {e}"))?;
    match parsed.scheme() {
        "http" | "https" => {}
        s => anyhow::bail!("unsupported URL scheme: {s}"),
    }

    validate_remote_target(&parsed)?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()?;

    let resp = client.get(parsed.clone()).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("download failed: HTTP {}", resp.status());
    }

    // Detect HTML responses (e.g. GitHub tree pages) before downloading body
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();
    if content_type.contains("text/html") {
        anyhow::bail!(
            "URL returned an HTML page instead of a downloadable skill. \
If this is a GitHub repository, use the raw URL or a .zip/.tar.gz archive link instead.\n\
Example: https://github.com/user/repo/archive/refs/heads/main.tar.gz"
        );
    }

    if let Some(len) = resp.content_length() {
        if len as usize > MAX_DOWNLOAD_BYTES {
            anyhow::bail!(
                "remote file too large: {} bytes (limit {})",
                len,
                MAX_DOWNLOAD_BYTES
            );
        }
    }

    let body = resp.bytes().await?;
    if body.len() > MAX_DOWNLOAD_BYTES {
        anyhow::bail!(
            "remote file too large: {} bytes (limit {})",
            body.len(),
            MAX_DOWNLOAD_BYTES
        );
    }

    let temp = tempfile::tempdir()?;
    let extract_root = temp.path().join("downloaded-skill");
    std::fs::create_dir_all(&extract_root)?;

    let path_lc = parsed.path().to_lowercase();
    if path_lc.ends_with(".zip") {
        extract_zip_bytes(&body, &extract_root)?;
    } else if path_lc.ends_with(".tar.gz") || path_lc.ends_with(".tgz") {
        extract_tar_gz_bytes(&body, &extract_root)?;
    } else if path_lc.ends_with(".tar") {
        extract_tar_bytes(&body, &extract_root)?;
    } else {
        std::fs::write(extract_root.join("SKILL.md"), &body)?;
    }

    // If a subpath hint was provided (e.g. from github.com/user/repo/tree/branch/skills/weather),
    // locate the skill within that subpath instead of searching the entire archive.
    let skill_root = if let Some(sub) = subpath {
        find_skill_in_subpath(&extract_root, sub)?
    } else {
        find_skill_root(&extract_root)?
    };

    Ok(ResolvedSkillSource::Remote {
        _temp_dir: temp,
        path: skill_root,
        url: Some(url.to_string()),
    })
}

fn find_skill_root(root: &Path) -> Result<PathBuf> {
    if root.join("SKILL.md").exists() {
        return Ok(root.to_path_buf());
    }

    let mut hits = Vec::new();
    find_skill_md_recursive(root, &mut hits)?;
    if hits.is_empty() {
        anyhow::bail!("downloaded source does not contain SKILL.md");
    }

    if hits.len() > 1 {
        anyhow::bail!(
            "downloaded source contains multiple SKILL.md files; please provide a single-skill archive"
        );
    }

    let only = hits.into_iter().next().expect("single hit should exist");
    let parent = only
        .parent()
        .ok_or_else(|| anyhow::anyhow!("invalid SKILL.md path in archive"))?;
    Ok(parent.to_path_buf())
}

/// Find skill root within a specific subpath of an extracted archive.
/// GitHub archives have a top-level `repo-branch/` directory, so we search for the subpath
/// within any top-level directory.
fn find_skill_in_subpath(extract_root: &Path, subpath: &str) -> Result<PathBuf> {
    // Direct match: extract_root/subpath/SKILL.md
    let direct = extract_root.join(subpath);
    if direct.join("SKILL.md").exists() {
        return Ok(direct);
    }

    // GitHub archive pattern: extract_root/<repo-branch>/subpath/SKILL.md
    if let Ok(entries) = std::fs::read_dir(extract_root) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                let candidate = entry.path().join(subpath);
                if candidate.join("SKILL.md").exists() {
                    return Ok(candidate);
                }
            }
        }
    }

    // Subpath not found; fall back to general search
    find_skill_root(extract_root)
}

fn find_skill_md_recursive(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let p = entry.path();
        if p.is_dir() {
            find_skill_md_recursive(&p, out)?;
        } else if p.file_name().and_then(|s| s.to_str()) == Some("SKILL.md") {
            out.push(p);
        }
    }
    Ok(())
}

#[doc(hidden)]
pub fn is_safe_relative_path(path: &Path) -> bool {
    !path.is_absolute()
        && !path.components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
}

fn extract_zip_bytes(bytes: &[u8], output_dir: &Path) -> Result<()> {
    let reader = Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(reader)?;

    for i in 0..archive.len() {
        let mut f = archive.by_index(i)?;
        let Some(raw_name) = f.enclosed_name().map(|p| p.to_path_buf()) else {
            continue;
        };
        if !is_safe_relative_path(&raw_name) {
            continue;
        }
        let outpath = output_dir.join(raw_name);
        if f.is_dir() {
            std::fs::create_dir_all(&outpath)?;
        } else {
            if let Some(parent) = outpath.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut out = std::fs::File::create(&outpath)?;
            std::io::copy(&mut f, &mut out)?;
        }
    }
    Ok(())
}

fn extract_tar_gz_bytes(bytes: &[u8], output_dir: &Path) -> Result<()> {
    let cursor = Cursor::new(bytes);
    let decoder = flate2::read::GzDecoder::new(cursor);
    let mut archive = tar::Archive::new(decoder);
    unpack_tar_archive(&mut archive, output_dir)
}

fn extract_tar_bytes(bytes: &[u8], output_dir: &Path) -> Result<()> {
    let cursor = Cursor::new(bytes);
    let mut archive = tar::Archive::new(cursor);
    unpack_tar_archive(&mut archive, output_dir)
}

#[doc(hidden)]
pub fn unpack_tar_archive<R: std::io::Read>(
    archive: &mut tar::Archive<R>,
    output_dir: &Path,
) -> Result<()> {
    for entry in archive.entries()? {
        let mut entry = entry?;
        let entry_type = entry.header().entry_type();
        if !matches!(
            entry_type,
            tar::EntryType::Regular | tar::EntryType::Directory
        ) {
            continue;
        }

        let path = entry.path()?.to_path_buf();
        if !is_safe_relative_path(&path) {
            continue;
        }
        let outpath = output_dir.join(path);
        if let Some(parent) = outpath.parent() {
            std::fs::create_dir_all(parent)?;
        }
        entry.unpack(&outpath)?;
    }
    Ok(())
}

fn parse_skill_frontmatter(raw: &str) -> Result<InstallSkillFrontmatter> {
    let trimmed = raw.trim_start();
    if !trimmed.starts_with("---") {
        anyhow::bail!("SKILL.md must start with YAML frontmatter (---)");
    }
    let after_first = &trimmed[3..];
    let end = after_first
        .find("---")
        .ok_or_else(|| anyhow::anyhow!("no closing --- for frontmatter"))?;
    let yaml_str = &after_first[..end];
    let fm: InstallSkillFrontmatter =
        serde_yaml::from_str(yaml_str).map_err(|e| anyhow::anyhow!("invalid frontmatter: {e}"))?;
    Ok(fm)
}

fn scan_path_recursive(dir: &Path, findings: &mut Vec<SkillRiskFinding>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.file_name().and_then(|s| s.to_str()) == Some(".git") {
            continue;
        }
        if path.is_dir() {
            scan_path_recursive(&path, findings)?;
            continue;
        }

        if let Ok(text) = std::fs::read_to_string(&path) {
            for (i, line) in text.lines().enumerate() {
                scan_line(&path, i + 1, line, findings);
            }
        }
    }
    Ok(())
}

fn scan_line(path: &Path, line_no: usize, line: &str, findings: &mut Vec<SkillRiskFinding>) {
    let checks: [(&str, &str, &str, &str); 9] = [
        (
            "critical",
            "rm -rf /",
            "dangerous delete",
            "Destructive filesystem wipe command",
        ),
        (
            "critical",
            "mkfs",
            "disk format",
            "Potential disk formatting command",
        ),
        (
            "high",
            "curl",
            "remote fetch",
            "Network fetch command found; verify intent",
        ),
        (
            "high",
            "wget",
            "remote fetch",
            "Network fetch command found; verify intent",
        ),
        (
            "high",
            "| sh",
            "pipe-to-shell",
            "Piping content to shell can execute untrusted code",
        ),
        (
            "high",
            "base64 -d",
            "obfuscation",
            "Potential obfuscated payload decode",
        ),
        (
            "high",
            "sudo ",
            "privilege escalation",
            "Privilege escalation command detected",
        ),
        (
            "medium",
            "~/.ssh",
            "secret path",
            "Accessing SSH config/key paths",
        ),
        (
            "medium",
            "~/.aws",
            "secret path",
            "Accessing cloud credential paths",
        ),
    ];

    let normalized = line.to_lowercase();
    for (severity, pattern, _reason, detail) in checks {
        if normalized.contains(&pattern.to_lowercase()) {
            findings.push(SkillRiskFinding {
                severity,
                file: path.to_path_buf(),
                line: line_no,
                pattern,
                reason: detail,
            });
        }
    }
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let target = dst.join(entry.file_name());
        if path.is_dir() {
            copy_dir_recursive(&path, &target)?;
        } else {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&path, &target)?;
        }
    }
    Ok(())
}

fn validate_remote_target(parsed: &reqwest::Url) -> Result<()> {
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("URL must include a host"))?;
    let normalized_host = host.trim_start_matches('[').trim_end_matches(']');

    if normalized_host.eq_ignore_ascii_case("localhost") {
        anyhow::bail!("blocked remote URL target: localhost");
    }

    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| anyhow::anyhow!("URL missing port for scheme"))?;

    let mut resolved_ips = Vec::new();
    if let Ok(ip) = normalized_host.parse::<IpAddr>() {
        resolved_ips.push(ip);
    } else {
        let addrs = std::net::ToSocketAddrs::to_socket_addrs(&(normalized_host, port))
            .map_err(|e| anyhow::anyhow!("failed to resolve host '{host}': {e}"))?;
        for addr in addrs {
            resolved_ips.push(addr.ip());
        }
    }

    if resolved_ips.is_empty() {
        anyhow::bail!("failed to resolve host '{host}'");
    }

    for ip in resolved_ips {
        if is_blocked_remote_ip(ip) {
            anyhow::bail!("blocked remote URL target: {host} resolved to {ip}");
        }
    }

    Ok(())
}

fn is_blocked_remote_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            if v4.is_unspecified() {
                return true;
            }
            let [a, b, ..] = v4.octets();
            a == 127
                || a == 10
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192 && b == 168)
                || (a == 169 && b == 254)
                || v4 == Ipv4Addr::new(169, 254, 169, 254)
        }
        IpAddr::V6(v6) => v6.is_loopback(),
    }
}

fn compute_directory_content_hash(root: &Path) -> Result<String> {
    let mut hasher = DefaultHasher::new();
    hash_directory_recursive(root, root, &mut hasher)?;
    Ok(format!("{:016x}", hasher.finish()))
}

fn hash_directory_recursive(root: &Path, dir: &Path, hasher: &mut DefaultHasher) -> Result<()> {
    let mut entries: Vec<_> =
        std::fs::read_dir(dir)?.collect::<std::result::Result<Vec<_>, _>>()?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            hash_directory_recursive(root, &path, hasher)?;
            continue;
        }
        if !file_type.is_file() {
            continue;
        }

        // Skip metadata/hash files so they don't affect the content hash
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name == METADATA_FILE || name == ".content-hash" {
                continue;
            }
        }

        let relative = path
            .strip_prefix(root)
            .map_err(|e| anyhow::anyhow!("failed to hash '{}': {e}", path.display()))?;
        relative.to_string_lossy().hash(hasher);

        let content = std::fs::read(&path)?;
        content.len().hash(hasher);
        hasher.write(&content);
    }

    Ok(())
}

#[derive(Debug)]
pub enum UpdateResult {
    Updated { skill_name: String },
    AlreadyUpToDate { skill_name: String },
}

pub async fn update_skill(
    config_root: &Path,
    skills_root: &Path,
    skill_name: &str,
) -> Result<UpdateResult> {
    let target = skills_root.join(skill_name);
    if !target.exists() {
        anyhow::bail!("skill not found: {skill_name}");
    }

    let meta = SkillMetadata::read_from(&target).ok_or_else(|| {
        anyhow::anyhow!("no metadata for skill '{skill_name}'; cannot determine source")
    })?;
    let source = meta.source.ok_or_else(|| {
        anyhow::anyhow!("skill '{skill_name}' has no source recorded; cannot update")
    })?;

    let resolved = resolve_skill_source(&source).await?;
    let report = analyze_skill_source(resolved.local_path())?;

    let source_hash = compute_directory_content_hash(resolved.local_path())?;
    if source_hash == meta.content_hash {
        return Ok(UpdateResult::AlreadyUpToDate {
            skill_name: skill_name.to_string(),
        });
    }

    install_skill_from_analysis(
        config_root,
        skills_root,
        resolved.local_path(),
        &report,
        meta.high_risk_acknowledged,
        Some(&source),
        resolved.resolved_url(),
    )?;

    Ok(UpdateResult::Updated {
        skill_name: skill_name.to_string(),
    })
}

pub async fn update_all_skills(
    config_root: &Path,
    skills_root: &Path,
) -> (Vec<String>, Vec<String>, Vec<(String, String)>) {
    let mut updated = Vec::new();
    let mut up_to_date = Vec::new();
    let mut failed = Vec::new();

    let entries = match std::fs::read_dir(skills_root) {
        Ok(e) => e,
        Err(_) => return (updated, up_to_date, failed),
    };

    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let meta = SkillMetadata::read_from(&entry.path());
        if meta.as_ref().and_then(|m| m.source.as_ref()).is_none() {
            continue; // skip skills without source
        }

        match update_skill(config_root, skills_root, &name).await {
            Ok(UpdateResult::Updated { .. }) => updated.push(name),
            Ok(UpdateResult::AlreadyUpToDate { .. }) => up_to_date.push(name),
            Err(e) => failed.push((name, e.to_string())),
        }
    }

    (updated, up_to_date, failed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolve_skill_source_local_path() {
        let temp = tempfile::tempdir().unwrap();
        let src = temp.path().join("skill");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            src.join("SKILL.md"),
            "---\nname: local-skill\ndescription: Local\n---\n",
        )
        .unwrap();

        let resolved = resolve_skill_source(src.to_str().unwrap()).await.unwrap();
        assert_eq!(resolved.local_path(), src.as_path());
    }

    #[test]
    fn analyze_requires_skill_md() {
        let temp = tempfile::tempdir().unwrap();
        let err = analyze_skill_source(temp.path()).unwrap_err().to_string();
        assert!(err.contains("missing SKILL.md"));
    }

    #[test]
    fn install_rejects_high_risk_without_flag() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("src");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(
            source.join("SKILL.md"),
            "---\nname: risky\ndescription: test\n---\n",
        )
        .unwrap();
        std::fs::write(source.join("install.sh"), "curl https://example.com").unwrap();

        let report = analyze_skill_source(&source).unwrap();
        assert!(has_high_risk_findings(&report));

        let config_root = temp.path().join("config");
        let skills_root = temp.path().join("skills");
        let err = install_skill_from_analysis(
            &config_root,
            &skills_root,
            &source,
            &report,
            false,
            None,
            None,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("high-risk"));
    }

    #[test]
    fn install_writes_audit_log() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("src");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(
            source.join("SKILL.md"),
            "---\nname: safe\ndescription: test\n---\n",
        )
        .unwrap();
        std::fs::write(source.join("README.md"), "hello").unwrap();

        let report = analyze_skill_source(&source).unwrap();
        let config_root = temp.path().join("config");
        let skills_root = temp.path().join("skills");
        let result = install_skill_from_analysis(
            &config_root,
            &skills_root,
            &source,
            &report,
            false,
            None,
            None,
        )
        .unwrap();

        assert!(result.target.exists());
        let audit = config_root.join("logs").join("skill-installs.jsonl");
        let content = std::fs::read_to_string(audit).unwrap();
        assert!(content.contains("\"skill\":\"safe\""));
    }

    #[test]
    fn normalize_github_repo_url() {
        let r = normalize_github_url("https://github.com/user/repo");
        assert_eq!(
            r.url,
            "https://github.com/user/repo/archive/refs/heads/main.tar.gz"
        );
        assert!(r.subpath.is_none());
    }

    #[test]
    fn normalize_github_tree_url_with_subpath() {
        let r = normalize_github_url(
            "https://github.com/sundial-org/awesome-openclaw-skills/tree/main/skills/weather",
        );
        assert_eq!(
            r.url,
            "https://github.com/sundial-org/awesome-openclaw-skills/archive/refs/heads/main.tar.gz"
        );
        assert_eq!(r.subpath.as_deref(), Some("skills/weather"));
    }

    #[test]
    fn normalize_github_tree_url_no_subpath() {
        let r = normalize_github_url("https://github.com/user/repo/tree/develop");
        assert_eq!(
            r.url,
            "https://github.com/user/repo/archive/refs/heads/develop.tar.gz"
        );
        assert!(r.subpath.is_none());
    }

    #[test]
    fn normalize_github_blob_url() {
        let r = normalize_github_url("https://github.com/sundial-org/awesome-openclaw-skills/blob/main/skills/weather/SKILL.md");
        assert_eq!(r.url, "https://raw.githubusercontent.com/sundial-org/awesome-openclaw-skills/main/skills/weather/SKILL.md");
        assert!(r.subpath.is_none());
    }

    #[test]
    fn normalize_github_blob_url_directory() {
        // blob URL pointing to a directory (no file extension) should be treated like tree URL
        let r = normalize_github_url(
            "https://github.com/sundial-org/awesome-openclaw-skills/blob/main/skills/weather",
        );
        assert_eq!(
            r.url,
            "https://github.com/sundial-org/awesome-openclaw-skills/archive/refs/heads/main.tar.gz"
        );
        assert_eq!(r.subpath.as_deref(), Some("skills/weather"));
    }

    #[test]
    fn normalize_non_github_url_unchanged() {
        let r = normalize_github_url("https://example.com/skill.tar.gz");
        assert_eq!(r.url, "https://example.com/skill.tar.gz");
        assert!(r.subpath.is_none());
    }

    #[tokio::test]
    async fn resolve_skill_source_local_zip() {
        let temp = tempfile::tempdir().unwrap();

        let skill_md = "---\nname: zipped-skill\ndescription: From zip\n---\nHello";
        let zip_path = temp.path().join("skill.zip");
        let file = std::fs::File::create(&zip_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        zip.start_file("SKILL.md", zip::write::SimpleFileOptions::default())
            .unwrap();
        std::io::Write::write_all(&mut zip, skill_md.as_bytes()).unwrap();
        zip.finish().unwrap();

        let resolved = resolve_skill_source(zip_path.to_str().unwrap())
            .await
            .unwrap();
        assert!(resolved.local_path().join("SKILL.md").exists());

        let report = analyze_skill_source(resolved.local_path()).unwrap();
        assert_eq!(report.skill_name, "zipped-skill");
    }

    #[tokio::test]
    async fn resolve_skill_source_local_tar_gz() {
        let temp = tempfile::tempdir().unwrap();

        let staging = temp.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(
            staging.join("SKILL.md"),
            "---\nname: tarred-skill\ndescription: From tar.gz\n---\n",
        )
        .unwrap();

        let tar_gz_path = temp.path().join("skill.tar.gz");
        let file = std::fs::File::create(&tar_gz_path).unwrap();
        let enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        let mut tar = tar::Builder::new(enc);
        tar.append_dir_all(".", &staging).unwrap();
        tar.finish().unwrap();
        drop(tar);

        let resolved = resolve_skill_source(tar_gz_path.to_str().unwrap())
            .await
            .unwrap();
        assert!(resolved.local_path().join("SKILL.md").exists());

        let report = analyze_skill_source(resolved.local_path()).unwrap();
        assert_eq!(report.skill_name, "tarred-skill");
    }

    #[test]
    fn skill_metadata_round_trip() {
        let meta = SkillMetadata {
            source: Some("https://github.com/user/repo".to_string()),
            resolved_url: Some(
                "https://github.com/user/repo/archive/refs/heads/main.tar.gz".to_string(),
            ),
            installed_at: "2026-04-07T12:00:00Z".to_string(),
            content_hash: "abc123".to_string(),
            high_risk_acknowledged: false,
            env_vars_written: vec!["API_KEY".to_string()],
        };
        let json = serde_json::to_string_pretty(&meta).unwrap();
        let parsed: SkillMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.source, meta.source);
        assert_eq!(parsed.resolved_url, meta.resolved_url);
        assert_eq!(parsed.content_hash, meta.content_hash);
        assert_eq!(parsed.env_vars_written, meta.env_vars_written);
    }

    #[test]
    fn install_writes_metadata_json() {
        let tmp = tempfile::tempdir().unwrap();
        let config_root = tmp.path().join("config");
        let skills_root = tmp.path().join("skills");
        std::fs::create_dir_all(&skills_root).unwrap();

        let source_dir = tmp.path().join("src-skill");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(
            source_dir.join("SKILL.md"),
            "---\nname: test-meta\ndescription: test\n---\nBody",
        )
        .unwrap();

        let report = analyze_skill_source(&source_dir).unwrap();
        let result = install_skill_from_analysis(
            &config_root,
            &skills_root,
            &source_dir,
            &report,
            false,
            Some("https://github.com/user/repo"),
            Some("https://github.com/user/repo/archive/refs/heads/main.tar.gz"),
        )
        .unwrap();

        let meta_path = result.target.join(METADATA_FILE);
        assert!(meta_path.exists(), ".metadata.json must be created");

        let meta = SkillMetadata::read_from(&result.target).unwrap();
        assert_eq!(meta.source.as_deref(), Some("https://github.com/user/repo"));
        assert!(!meta.content_hash.is_empty());

        // .content-hash should NOT exist
        assert!(!result.target.join(".content-hash").exists());
    }

    #[test]
    fn update_env_vars_written_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_dir = tmp.path().join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();

        let meta = SkillMetadata {
            source: None,
            resolved_url: None,
            installed_at: "2026-04-07T12:00:00Z".to_string(),
            content_hash: "abc".to_string(),
            high_risk_acknowledged: false,
            env_vars_written: vec![],
        };
        meta.write_to(&skill_dir).unwrap();

        update_env_vars_written(&skill_dir, &["API_KEY".to_string()]).unwrap();

        let updated = SkillMetadata::read_from(&skill_dir).unwrap();
        assert_eq!(updated.env_vars_written, vec!["API_KEY".to_string()]);
    }

    #[test]
    fn remove_skill_deletes_dir_and_returns_env_hint() {
        let tmp = tempfile::tempdir().unwrap();
        let config_root = tmp.path().join("config");
        let skills_root = tmp.path().join("skills");
        let skill_dir = skills_root.join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: test\n---\nBody",
        )
        .unwrap();

        let meta = SkillMetadata {
            source: Some("https://github.com/user/repo".to_string()),
            resolved_url: None,
            installed_at: "2026-04-07T12:00:00Z".to_string(),
            content_hash: "abc".to_string(),
            high_risk_acknowledged: false,
            env_vars_written: vec!["API_KEY".to_string()],
        };
        meta.write_to(&skill_dir).unwrap();

        let result = remove_skill(&config_root, &skills_root, "my-skill").unwrap();
        assert!(!skill_dir.exists());
        assert_eq!(result.env_vars_hint, vec!["API_KEY".to_string()]);

        // Audit log should exist
        let audit_path = config_root.join("logs/skill-installs.jsonl");
        assert!(audit_path.exists());
        let log_content = std::fs::read_to_string(&audit_path).unwrap();
        assert!(log_content.contains("\"action\":\"remove\""));
    }

    #[test]
    fn remove_skill_not_found() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_root = tmp.path().join("skills");
        std::fs::create_dir_all(&skills_root).unwrap();

        let result = remove_skill(tmp.path(), &skills_root, "nonexistent");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not found"));
    }

    #[tokio::test]
    async fn update_skill_from_local_source() {
        let tmp = tempfile::tempdir().unwrap();
        let config_root = tmp.path().join("config");
        let skills_root = tmp.path().join("skills");
        let skill_dir = skills_root.join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: test\n---\nBody",
        )
        .unwrap();

        // Create the "source" directory
        let source_dir = tmp.path().join("source");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(
            source_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: updated\n---\nNew body",
        )
        .unwrap();

        let meta = SkillMetadata {
            source: Some(source_dir.display().to_string()),
            resolved_url: None,
            installed_at: "2026-04-07T12:00:00Z".to_string(),
            content_hash: "old-hash".to_string(),
            high_risk_acknowledged: false,
            env_vars_written: vec![],
        };
        meta.write_to(&skill_dir).unwrap();

        let result = update_skill(&config_root, &skills_root, "my-skill")
            .await
            .unwrap();
        assert!(matches!(result, UpdateResult::Updated { .. }));

        // Verify updated
        let content = std::fs::read_to_string(skills_root.join("my-skill/SKILL.md")).unwrap();
        assert!(content.contains("updated"));
    }

    #[tokio::test]
    async fn update_skill_already_up_to_date() {
        let tmp = tempfile::tempdir().unwrap();
        let config_root = tmp.path().join("config");
        let skills_root = tmp.path().join("skills");

        // Create the "source" directory first
        let source_dir = tmp.path().join("source");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(
            source_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: test\n---\nBody",
        )
        .unwrap();

        // Install the skill normally first (so hash matches)
        std::fs::create_dir_all(&skills_root).unwrap();
        let report = analyze_skill_source(&source_dir).unwrap();
        install_skill_from_analysis(
            &config_root,
            &skills_root,
            &source_dir,
            &report,
            false,
            Some(&source_dir.display().to_string()),
            None,
        )
        .unwrap();

        let result = update_skill(&config_root, &skills_root, "my-skill")
            .await
            .unwrap();
        assert!(matches!(result, UpdateResult::AlreadyUpToDate { .. }));
    }

    #[tokio::test]
    async fn update_skill_no_source() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_root = tmp.path().join("skills");
        let skill_dir = skills_root.join("my-skill");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: test\n---\nBody",
        )
        .unwrap();
        // No .metadata.json

        let result = update_skill(tmp.path(), &skills_root, "my-skill").await;
        assert!(result.is_err());
    }
}
