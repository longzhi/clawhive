//! Directory-level access control for workspace-external paths.
//!
//! `AccessGate` replaces the old workspace-containment check with a flexible
//! allow-list approach:
//!
//! 1. Paths inside the workspace → always allowed
//! 2. Paths denied by `HardBaseline` → always denied
//! 3. Paths in the persistent allow-list with sufficient level → allowed
//! 4. Everything else → returns `NeedGrant` so the agent can ask the user

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use clawhive_provider::ToolDef;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use super::policy::HardBaseline;
use super::tool::{ToolContext, ToolExecutor, ToolOutput};

// ───────────────────────────── Types ─────────────────────────────

/// Access level: read-only or read-write.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AccessLevel {
    Ro,
    Rw,
}

impl AccessLevel {
    /// Returns `true` when `self` satisfies `need`.
    pub fn satisfies(self, need: AccessLevel) -> bool {
        match (self, need) {
            (AccessLevel::Rw, _) => true,
            (AccessLevel::Ro, AccessLevel::Ro) => true,
            (AccessLevel::Ro, AccessLevel::Rw) => false,
        }
    }
}

impl std::fmt::Display for AccessLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AccessLevel::Ro => write!(f, "ro"),
            AccessLevel::Rw => write!(f, "rw"),
        }
    }
}

/// One entry in the allow-list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AllowEntry {
    pub path: String,
    pub level: AccessLevel,
}

/// Persistent policy document (serialised as `access_policy.json`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AccessPolicy {
    #[serde(default)]
    pub allowed: Vec<AllowEntry>,
}

/// Result of an access check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessResult {
    Allowed,
    Denied(String),
    NeedGrant { dir: String, need: AccessLevel },
}

// ───────────────────────────── AccessGate ─────────────────────────

pub struct AccessGate {
    workspace: PathBuf,
    policy: RwLock<AccessPolicy>,
    policy_path: PathBuf,
}

impl AccessGate {
    /// Create a new gate, loading the policy file if it exists.
    pub fn new(workspace: PathBuf, policy_path: PathBuf) -> Self {
        let policy = if policy_path.exists() {
            match std::fs::read_to_string(&policy_path) {
                Ok(data) => serde_json::from_str(&data).unwrap_or_default(),
                Err(_) => AccessPolicy::default(),
            }
        } else {
            AccessPolicy::default()
        };
        Self {
            workspace,
            policy: RwLock::new(policy),
            policy_path,
        }
    }

    /// Create a gate for tests (no persistence).
    #[cfg(test)]
    pub fn in_memory(workspace: PathBuf) -> Self {
        Self {
            workspace: workspace.clone(),
            policy: RwLock::new(AccessPolicy::default()),
            policy_path: workspace.join("access_policy.json"),
        }
    }

    /// Check whether `path` can be accessed at the requested `need` level.
    pub async fn check(&self, path: &Path, need: AccessLevel) -> AccessResult {
        // 1. HardBaseline — always first
        match need {
            AccessLevel::Ro => {
                if HardBaseline::path_read_denied(path) {
                    return AccessResult::Denied(format!(
                        "Read access denied: sensitive file (hard baseline): {}",
                        path.display()
                    ));
                }
            }
            AccessLevel::Rw => {
                if HardBaseline::path_write_denied(path) {
                    return AccessResult::Denied(format!(
                        "Write access denied: sensitive path (hard baseline): {}",
                        path.display()
                    ));
                }
                // Also check read-deny for writes (if you can't read it, you can't write it)
                if HardBaseline::path_read_denied(path) {
                    return AccessResult::Denied(format!(
                        "Access denied: sensitive file (hard baseline): {}",
                        path.display()
                    ));
                }
            }
        }

        // 2. Workspace sub-path → always allowed
        if let Ok(ws_canon) = self.workspace.canonicalize() {
            if let Ok(p_canon) = path.canonicalize() {
                if p_canon.starts_with(&ws_canon) {
                    return AccessResult::Allowed;
                }
            }
            // For paths that don't exist yet, do logical check
            let normalized = normalize_path(path);
            if normalized.starts_with(&ws_canon) {
                return AccessResult::Allowed;
            }
        }
        // Fallback: string prefix check for non-canonicalisable workspace
        if path.starts_with(&self.workspace) {
            return AccessResult::Allowed;
        }

        // 3. Allow-list lookup
        let policy = self.policy.read().await;
        let canonical_path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let path_str = canonical_path.to_string_lossy();
        for entry in &policy.allowed {
            if path_str.starts_with(&entry.path) || path_str.as_ref() == entry.path.as_str() {
                if entry.level.satisfies(need) {
                    return AccessResult::Allowed;
                } else {
                    // Have ro but need rw
                    return AccessResult::NeedGrant {
                        dir: entry.path.clone(),
                        need,
                    };
                }
            }
        }

        // 4. Not found — need grant
        // Walk up to the nearest real directory for a sensible suggestion
        let suggest_dir = nearest_dir(path);
        AccessResult::NeedGrant {
            dir: suggest_dir,
            need,
        }
    }

    /// Add (or upgrade) a directory to the allow-list and persist.
    pub async fn grant(&self, dir: &Path, level: AccessLevel) -> Result<()> {
        // Canonicalize so the stored path matches what resolve_path produces
        let canonical = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        let dir_str = canonical.to_string_lossy().to_string();

        // Block granting to hard-baseline paths (check both original and canonical)
        if HardBaseline::path_write_denied(dir)
            || HardBaseline::path_read_denied(dir)
            || HardBaseline::path_write_denied(&canonical)
            || HardBaseline::path_read_denied(&canonical)
        {
            return Err(anyhow!(
                "Cannot grant access to sensitive path: {}",
                dir.display()
            ));
        }

        let mut policy = self.policy.write().await;
        // Update existing or insert
        if let Some(entry) = policy.allowed.iter_mut().find(|e| e.path == dir_str) {
            entry.level = level;
        } else {
            policy.allowed.push(AllowEntry {
                path: dir_str,
                level,
            });
        }
        self.persist(&policy).await
    }

    /// Try to automatically grant access to a directory.
    /// Returns Ok(()) when the path is safe to auto-grant, Err when it requires human approval.
    pub async fn try_auto_grant(&self, path: &Path, level: AccessLevel) -> Result<(), String> {
        self.grant(path, level).await.map_err(|e| e.to_string())
    }

    /// Remove a directory from the allow-list and persist.
    pub async fn revoke(&self, dir: &Path) -> Result<()> {
        let canonical = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        let dir_str = canonical.to_string_lossy().to_string();
        let mut policy = self.policy.write().await;
        let before = policy.allowed.len();
        policy.allowed.retain(|e| e.path != dir_str);
        if policy.allowed.len() == before {
            return Err(anyhow!("Path not in allow-list: {}", dir.display()));
        }
        self.persist(&policy).await
    }

    /// List all allowed entries.
    pub async fn list(&self) -> Vec<AllowEntry> {
        self.policy.read().await.allowed.clone()
    }

    /// Get the allowed directories with their paths and levels (for sandbox building).
    pub async fn allowed_dirs(&self) -> Vec<(PathBuf, AccessLevel)> {
        self.policy
            .read()
            .await
            .allowed
            .iter()
            .map(|e| (PathBuf::from(&e.path), e.level))
            .collect()
    }

    async fn persist(&self, policy: &AccessPolicy) -> Result<()> {
        if let Some(parent) = self.policy_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let data = serde_json::to_string_pretty(policy)?;
        tokio::fs::write(&self.policy_path, data).await?;
        Ok(())
    }
}

// ───────────────────────────── Helpers ────────────────────────────

/// Normalize a path logically (resolve `.` and `..`) without touching the filesystem.
pub fn normalize_path(path: &Path) -> PathBuf {
    let mut components = Vec::new();
    for comp in path.components() {
        match comp {
            std::path::Component::ParentDir => {
                if !components.is_empty() {
                    components.pop();
                }
            }
            std::path::Component::CurDir => {}
            other => components.push(other),
        }
    }
    components.iter().collect()
}

/// Resolve a requested path string into a normalised `PathBuf`.
///
/// This is a pure path-normalisation function — **no** workspace containment
/// check.  Relative paths are resolved against `workspace`.
pub fn resolve_path(workspace: &Path, requested: &str) -> Result<PathBuf> {
    if requested.is_empty() {
        return Err(anyhow!("path must not be empty"));
    }

    // Expand ~ to home directory
    let expanded = if requested == "~" || requested.starts_with("~/") {
        match std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")) {
            Ok(home) => {
                if requested == "~" {
                    home
                } else {
                    format!("{}{}", home, &requested[1..])
                }
            }
            Err(_) => requested.to_string(),
        }
    } else {
        requested.to_string()
    };

    let candidate = if Path::new(&expanded).is_absolute() {
        PathBuf::from(&expanded)
    } else {
        let ws_canon = workspace
            .canonicalize()
            .unwrap_or_else(|_| workspace.to_path_buf());
        ws_canon.join(&expanded)
    };

    // If the file already exists, canonicalize to resolve symlinks
    if let Ok(resolved) = candidate.canonicalize() {
        return Ok(resolved);
    }

    // For non-existing paths, normalize logically
    Ok(normalize_path(&candidate))
}

/// Walk up from `path` until we find an existing directory (or use the path itself).
fn nearest_dir(path: &Path) -> String {
    let mut p = path.to_path_buf();
    loop {
        if p.is_dir() {
            return p.to_string_lossy().to_string();
        }
        if !p.pop() {
            return path.to_string_lossy().to_string();
        }
    }
}

// ───────────────────────────── Tools ─────────────────────────────

/// Tool: grant_access — allow the agent to access a directory outside the workspace.
pub struct GrantAccessTool {
    gate: Arc<AccessGate>,
}

impl GrantAccessTool {
    pub fn new(gate: Arc<AccessGate>) -> Self {
        Self { gate }
    }
}

#[async_trait]
impl ToolExecutor for GrantAccessTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "grant_access".into(),
            description:
                "Grant the agent read or read-write access to a directory outside the workspace."
                    .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute path to the directory to grant access to"
                    },
                    "level": {
                        "type": "string",
                        "enum": ["ro", "rw"],
                        "description": "Access level: 'ro' for read-only, 'rw' for read-write"
                    }
                },
                "required": ["path", "level"]
            }),
        }
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let path_str = input["path"]
            .as_str()
            .ok_or_else(|| anyhow!("missing 'path' field"))?;
        let level_str = input["level"]
            .as_str()
            .ok_or_else(|| anyhow!("missing 'level' field"))?;

        let level = match level_str {
            "ro" => AccessLevel::Ro,
            "rw" => AccessLevel::Rw,
            other => {
                return Ok(ToolOutput {
                    content: format!("Invalid access level: {other}. Use 'ro' or 'rw'."),
                    is_error: true,
                });
            }
        };

        let path = Path::new(path_str);
        if !path.is_absolute() {
            return Ok(ToolOutput {
                content: "Path must be absolute.".into(),
                is_error: true,
            });
        }

        match self.gate.grant(path, level).await {
            Ok(()) => Ok(ToolOutput {
                content: format!("Granted {level} access to {path_str}"),
                is_error: false,
            }),
            Err(e) => Ok(ToolOutput {
                content: format!("Failed to grant access: {e}"),
                is_error: true,
            }),
        }
    }
}

/// Tool: list_access — show all directories in the allow-list.
pub struct ListAccessTool {
    gate: Arc<AccessGate>,
}

impl ListAccessTool {
    pub fn new(gate: Arc<AccessGate>) -> Self {
        Self { gate }
    }
}

#[async_trait]
impl ToolExecutor for ListAccessTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "list_access".into(),
            description:
                "List all directories the agent has been granted access to outside the workspace."
                    .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        }
    }

    async fn execute(&self, _input: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let entries = self.gate.list().await;
        if entries.is_empty() {
            return Ok(ToolOutput {
                content: "No external directories are currently authorized.".into(),
                is_error: false,
            });
        }
        let mut out = String::from("Authorized directories:\n");
        for e in &entries {
            out.push_str(&format!("  {} ({})\n", e.path, e.level));
        }
        Ok(ToolOutput {
            content: out,
            is_error: false,
        })
    }
}

/// Tool: revoke_access — remove a directory from the allow-list.
pub struct RevokeAccessTool {
    gate: Arc<AccessGate>,
}

impl RevokeAccessTool {
    pub fn new(gate: Arc<AccessGate>) -> Self {
        Self { gate }
    }
}

#[async_trait]
impl ToolExecutor for RevokeAccessTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "revoke_access".into(),
            description: "Revoke the agent's access to a directory outside the workspace.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute path to the directory to revoke access from"
                    }
                },
                "required": ["path"]
            }),
        }
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let path_str = input["path"]
            .as_str()
            .ok_or_else(|| anyhow!("missing 'path' field"))?;

        let path = Path::new(path_str);
        match self.gate.revoke(path).await {
            Ok(()) => Ok(ToolOutput {
                content: format!("Revoked access to {path_str}"),
                is_error: false,
            }),
            Err(e) => Ok(ToolOutput {
                content: format!("Failed to revoke access: {e}"),
                is_error: true,
            }),
        }
    }
}

// ───────────────────────────── Tests ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn setup() -> (TempDir, Arc<AccessGate>) {
        let tmp = TempDir::new().unwrap();
        let gate = Arc::new(AccessGate::in_memory(tmp.path().to_path_buf()));
        (tmp, gate)
    }

    #[tokio::test]
    async fn workspace_path_always_allowed() {
        let (_tmp, gate) = setup();
        let ws = gate.workspace.clone();
        std::fs::write(ws.join("file.txt"), "hello").unwrap();
        let result = gate.check(&ws.join("file.txt"), AccessLevel::Rw).await;
        assert_eq!(result, AccessResult::Allowed);
    }

    #[tokio::test]
    async fn hard_baseline_read_denied() {
        let (_tmp, gate) = setup();
        let result = gate
            .check(Path::new("/home/user/.ssh/id_rsa"), AccessLevel::Ro)
            .await;
        assert!(matches!(result, AccessResult::Denied(_)));
    }

    #[tokio::test]
    async fn hard_baseline_write_denied() {
        let (_tmp, gate) = setup();
        let result = gate.check(Path::new("/etc/passwd"), AccessLevel::Rw).await;
        assert!(matches!(result, AccessResult::Denied(_)));
    }

    #[tokio::test]
    async fn unknown_path_needs_grant() {
        let (_tmp, gate) = setup();
        let result = gate
            .check(Path::new("/some/other/project/file.rs"), AccessLevel::Ro)
            .await;
        assert!(matches!(result, AccessResult::NeedGrant { .. }));
    }

    #[tokio::test]
    async fn grant_then_check_allowed() {
        let (_tmp, gate) = setup();
        let dir = Path::new("/some/other/project");
        gate.grant(dir, AccessLevel::Rw).await.unwrap();
        let result = gate
            .check(Path::new("/some/other/project/file.rs"), AccessLevel::Rw)
            .await;
        assert_eq!(result, AccessResult::Allowed);
    }

    #[tokio::test]
    async fn grant_ro_check_rw_needs_upgrade() {
        let (_tmp, gate) = setup();
        let dir = Path::new("/some/other/project");
        gate.grant(dir, AccessLevel::Ro).await.unwrap();
        let result = gate
            .check(Path::new("/some/other/project/file.rs"), AccessLevel::Rw)
            .await;
        assert!(matches!(
            result,
            AccessResult::NeedGrant {
                need: AccessLevel::Rw,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn grant_ro_check_ro_allowed() {
        let (_tmp, gate) = setup();
        let dir = Path::new("/some/other/project");
        gate.grant(dir, AccessLevel::Ro).await.unwrap();
        let result = gate
            .check(Path::new("/some/other/project/file.rs"), AccessLevel::Ro)
            .await;
        assert_eq!(result, AccessResult::Allowed);
    }

    #[tokio::test]
    async fn revoke_then_need_grant() {
        let (_tmp, gate) = setup();
        let dir = Path::new("/some/other/project");
        gate.grant(dir, AccessLevel::Rw).await.unwrap();
        gate.revoke(dir).await.unwrap();
        let result = gate
            .check(Path::new("/some/other/project/file.rs"), AccessLevel::Ro)
            .await;
        assert!(matches!(result, AccessResult::NeedGrant { .. }));
    }

    #[tokio::test]
    async fn list_returns_entries() {
        let (_tmp, gate) = setup();
        gate.grant(Path::new("/a"), AccessLevel::Ro).await.unwrap();
        gate.grant(Path::new("/b"), AccessLevel::Rw).await.unwrap();
        let entries = gate.list().await;
        assert_eq!(entries.len(), 2);
    }

    #[tokio::test]
    async fn grant_sensitive_path_blocked() {
        let (_tmp, gate) = setup();
        let result = gate
            .grant(Path::new("/etc/something"), AccessLevel::Ro)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn try_auto_grant_safe_path_allowed() {
        let (_tmp, gate) = setup();
        let dir = Path::new("/some/other/project");
        let result = gate.try_auto_grant(dir, AccessLevel::Ro).await;
        assert!(result.is_ok());
        let check = gate
            .check(Path::new("/some/other/project/file.rs"), AccessLevel::Ro)
            .await;
        assert_eq!(check, AccessResult::Allowed);
    }

    #[tokio::test]
    async fn try_auto_grant_sensitive_path_blocked() {
        let (_tmp, gate) = setup();
        let result = gate
            .try_auto_grant(Path::new("/etc/something"), AccessLevel::Ro)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn persistence_round_trip() {
        let tmp = TempDir::new().unwrap();
        let policy_path = tmp.path().join("access_policy.json");

        // Create gate, grant access, drop
        {
            let gate = AccessGate::new(tmp.path().to_path_buf(), policy_path.clone());
            gate.grant(Path::new("/foo/bar"), AccessLevel::Rw)
                .await
                .unwrap();
        }

        // Re-create gate, verify loaded
        {
            let gate = AccessGate::new(tmp.path().to_path_buf(), policy_path);
            let entries = gate.list().await;
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].path, "/foo/bar");
            assert_eq!(entries[0].level, AccessLevel::Rw);
        }
    }

    #[tokio::test]
    async fn grant_upgrade_ro_to_rw() {
        let (_tmp, gate) = setup();
        let dir = Path::new("/some/project");
        gate.grant(dir, AccessLevel::Ro).await.unwrap();
        gate.grant(dir, AccessLevel::Rw).await.unwrap();
        let entries = gate.list().await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].level, AccessLevel::Rw);
    }

    #[tokio::test]
    async fn resolve_path_relative() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("test.txt"), "hi").unwrap();
        let resolved = resolve_path(tmp.path(), "test.txt").unwrap();
        assert!(resolved.is_absolute());
        assert!(resolved.ends_with("test.txt"));
    }

    #[tokio::test]
    async fn resolve_path_absolute() {
        let resolved = resolve_path(Path::new("/tmp"), "/usr/local/bin").unwrap();
        assert_eq!(resolved, PathBuf::from("/usr/local/bin"));
    }

    #[tokio::test]
    async fn resolve_path_empty_rejected() {
        let result = resolve_path(Path::new("/tmp"), "");
        assert!(result.is_err());
    }

    #[test]
    fn access_level_satisfies() {
        assert!(AccessLevel::Rw.satisfies(AccessLevel::Ro));
        assert!(AccessLevel::Rw.satisfies(AccessLevel::Rw));
        assert!(AccessLevel::Ro.satisfies(AccessLevel::Ro));
        assert!(!AccessLevel::Ro.satisfies(AccessLevel::Rw));
    }

    #[tokio::test]
    async fn grant_access_tool_works() {
        let (_tmp, gate) = setup();
        let tool = GrantAccessTool::new(gate.clone());
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(
                serde_json::json!({"path": "/some/dir", "level": "rw"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("Granted rw access"));
    }

    #[tokio::test]
    async fn grant_access_tool_rejects_sensitive() {
        let (_tmp, gate) = setup();
        let tool = GrantAccessTool::new(gate.clone());
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(
                serde_json::json!({"path": "/etc/passwd", "level": "ro"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn grant_access_tool_rejects_relative() {
        let (_tmp, gate) = setup();
        let tool = GrantAccessTool::new(gate.clone());
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(
                serde_json::json!({"path": "relative/path", "level": "ro"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("absolute"));
    }

    #[tokio::test]
    async fn list_access_tool_works() {
        let (_tmp, gate) = setup();
        gate.grant(Path::new("/foo"), AccessLevel::Ro)
            .await
            .unwrap();
        let tool = ListAccessTool::new(gate.clone());
        let ctx = ToolContext::builtin();
        let result = tool.execute(serde_json::json!({}), &ctx).await.unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("/foo"));
    }

    #[tokio::test]
    async fn revoke_access_tool_works() {
        let (_tmp, gate) = setup();
        gate.grant(Path::new("/foo"), AccessLevel::Ro)
            .await
            .unwrap();
        let tool = RevokeAccessTool::new(gate.clone());
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(serde_json::json!({"path": "/foo"}), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("Revoked"));
        assert!(gate.list().await.is_empty());
    }

    #[tokio::test]
    async fn resolve_path_tilde_expands_to_home() {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap();
        let resolved = resolve_path(Path::new("/tmp"), "~/.zshrc").unwrap();
        assert!(resolved.is_absolute());
        assert!(resolved.to_string_lossy().starts_with(&home));
        assert!(resolved.to_string_lossy().ends_with(".zshrc"));
        // Must NOT contain the workspace path
        assert!(!resolved.to_string_lossy().contains("/tmp/~"));
    }

    #[tokio::test]
    async fn resolve_path_tilde_alone() {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap();
        let resolved = resolve_path(Path::new("/tmp"), "~").unwrap();
        assert_eq!(resolved.to_string_lossy().as_ref(), &home);
    }
}
