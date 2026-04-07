use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use corral_core::{
    start_broker, BrokerConfig, Permissions, PolicyEngine, Sandbox, SandboxConfig, ServiceHandler,
    ServicePermission,
};

use crate::access_gate::AccessLevel;
use crate::config::{SandboxNetworkMode, SandboxPolicyConfig};

struct RemindersHandler;

#[async_trait]
impl ServiceHandler for RemindersHandler {
    async fn handle(
        &self,
        method: &str,
        params: &serde_json::Value,
        policy: &PolicyEngine,
    ) -> Result<serde_json::Value> {
        match method {
            "list" => {
                let list = params.get("list").and_then(|v| v.as_str());
                if let Some(list_name) = list {
                    policy.check_reminders_scope_result(list_name)?;
                }
                policy.check_service_result("reminders", "list", params)?;

                let mut cmd = tokio::process::Command::new("remindctl");
                cmd.arg("list");
                if let Some(list_name) = list {
                    cmd.arg(list_name);
                }
                cmd.arg("--json").arg("--no-input");

                let output = cmd.output().await?;
                if !output.status.success() {
                    return Err(anyhow!(
                        "remindctl list failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                    ));
                }
                let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
                Ok(value)
            }
            "add" => {
                let list = params
                    .get("list")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("reminders.add requires 'list'"))?;
                let title = params
                    .get("title")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("reminders.add requires 'title'"))?;

                policy.check_service_result("reminders", "add", params)?;
                policy.check_reminders_scope_result(list)?;

                let mut cmd = tokio::process::Command::new("remindctl");
                cmd.arg("add")
                    .arg("--title")
                    .arg(title)
                    .arg("--list")
                    .arg(list)
                    .arg("--json")
                    .arg("--no-input");

                if let Some(due) = params.get("dueDate").and_then(|v| v.as_str()) {
                    cmd.arg("--due").arg(due);
                }
                if let Some(notes) = params.get("notes").and_then(|v| v.as_str()) {
                    cmd.arg("--notes").arg(notes);
                }
                if let Some(priority) = params.get("priority").and_then(|v| v.as_str()) {
                    cmd.arg("--priority").arg(priority);
                }

                let output = cmd.output().await?;
                if !output.status.success() {
                    return Err(anyhow!(
                        "remindctl add failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                    ));
                }
                let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
                Ok(value)
            }
            "update" => {
                let id = params
                    .get("id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("reminders.update requires 'id'"))?;

                policy.check_service_result("reminders", "update", params)?;
                if let Some(list_name) = params.get("list").and_then(|v| v.as_str()) {
                    policy.check_reminders_scope_result(list_name)?;
                }

                let mut cmd = tokio::process::Command::new("remindctl");
                cmd.arg("edit").arg(id).arg("--json").arg("--no-input");

                if let Some(title) = params.get("title").and_then(|v| v.as_str()) {
                    cmd.arg("--title").arg(title);
                }
                if let Some(list_name) = params.get("list").and_then(|v| v.as_str()) {
                    cmd.arg("--list").arg(list_name);
                }
                if let Some(due) = params.get("dueDate").and_then(|v| v.as_str()) {
                    cmd.arg("--due").arg(due);
                }
                if params.get("clearDue").and_then(|v| v.as_bool()) == Some(true) {
                    cmd.arg("--clear-due");
                }
                if let Some(notes) = params.get("notes").and_then(|v| v.as_str()) {
                    cmd.arg("--notes").arg(notes);
                }
                if let Some(priority) = params.get("priority").and_then(|v| v.as_str()) {
                    cmd.arg("--priority").arg(priority);
                }

                let output = cmd.output().await?;
                if !output.status.success() {
                    return Err(anyhow!(
                        "remindctl edit failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                    ));
                }
                let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
                Ok(value)
            }
            "complete" => {
                let id = params
                    .get("id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("reminders.complete requires 'id'"))?;

                policy.check_service_result("reminders", "complete", params)?;

                let output = tokio::process::Command::new("remindctl")
                    .arg("complete")
                    .arg(id)
                    .arg("--json")
                    .arg("--no-input")
                    .output()
                    .await?;

                if !output.status.success() {
                    return Err(anyhow!(
                        "remindctl complete failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                    ));
                }
                let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
                Ok(value)
            }
            "delete" => {
                let id = params
                    .get("id")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("reminders.delete requires 'id'"))?;

                policy.check_service_result("reminders", "delete", params)?;

                let output = tokio::process::Command::new("remindctl")
                    .arg("delete")
                    .arg(id)
                    .arg("--force")
                    .arg("--json")
                    .arg("--no-input")
                    .output()
                    .await?;

                if !output.status.success() {
                    return Err(anyhow!(
                        "remindctl delete failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                    ));
                }
                let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
                Ok(value)
            }
            _ => Err(anyhow!("Unknown reminders method: {method}")),
        }
    }

    fn namespace(&self) -> &str {
        "reminders"
    }
}

fn collect_env_vars(env_inherit: &[String]) -> HashMap<String, String> {
    let mut env_vars = HashMap::new();

    // Load all vars from ~/.clawhive/.env — operator-controlled, always trusted.
    if let Some(dotenv_path) = crate::dotenv::default_dotenv_path() {
        for (key, val) in crate::dotenv::read_dotenv(&dotenv_path) {
            env_vars.insert(key, val);
        }
    }

    for key in env_inherit {
        if key == "PATH" {
            let inherited = std::env::var("PATH").unwrap_or_default();
            let merged = augment_path_like_host(&inherited, &default_path_candidates());
            env_vars.insert(key.clone(), merged);
            continue;
        }
        if let Some(val) = crate::dotenv::resolve_env(key) {
            env_vars.insert(key.clone(), val);
        }
    }
    env_vars
}

pub fn default_path_candidates() -> Vec<String> {
    let mut candidates = vec![
        "/opt/homebrew/bin".to_string(),
        "/opt/homebrew/sbin".to_string(),
        "/usr/local/bin".to_string(),
        "/usr/local/sbin".to_string(),
        "/usr/bin".to_string(),
        "/bin".to_string(),
        "/usr/sbin".to_string(),
        "/sbin".to_string(),
    ];

    if let Ok(home) = std::env::var("HOME") {
        candidates.extend([
            format!("{home}/.clawhive/bin"),
            format!("{home}/.cargo/bin"),
            format!("{home}/.bun/bin"),
            format!("{home}/.local/bin"),
            format!("{home}/bin"),
        ]);
    }

    candidates
}

pub fn augment_path_like_host(current_path: &str, candidates: &[String]) -> String {
    let mut entries: Vec<PathBuf> = std::env::split_paths(current_path).collect();
    let mut seen: HashSet<OsString> = entries
        .iter()
        .map(|p| p.as_os_str().to_os_string())
        .collect();

    for candidate in candidates {
        if candidate.trim().is_empty() {
            continue;
        }
        let path = PathBuf::from(candidate);
        let key = path.as_os_str().to_os_string();
        if seen.insert(key) {
            entries.push(path);
        }
    }

    match std::env::join_paths(entries) {
        Ok(os) => os.to_string_lossy().into_owned(),
        Err(_) => current_path.to_string(),
    }
}

/// On macOS `/tmp`, `/var`, `/etc` are symlinks to `/private/{...}`.
/// Returns the alternate form so sandbox patterns cover both.
fn macos_symlink_alias(path: &str) -> Option<String> {
    for prefix in &["/tmp", "/var", "/etc"] {
        let private = format!("/private{prefix}");
        if let Some(rest) = path.strip_prefix(private.as_str()) {
            if rest.is_empty() || rest.starts_with('/') {
                return Some(format!("{prefix}{rest}"));
            }
        }
        if let Some(rest) = path.strip_prefix(prefix) {
            if rest.is_empty() || rest.starts_with('/') {
                return Some(format!("{private}{rest}"));
            }
        }
    }
    None
}

pub(super) fn base_permissions(
    workspace: &Path,
    extra_dirs: &[(PathBuf, AccessLevel)],
    exec_allow: &[String],
    network_allowed: bool,
    env_inherit: &[String],
) -> Permissions {
    let workspace_self = workspace.display().to_string();
    let workspace_pattern = format!("{workspace_self}/**");
    // Include the directory itself (for opendir) AND its contents (for files within)
    let mut read_patterns = vec![workspace_self.clone(), workspace_pattern.clone()];
    let mut write_patterns = vec![workspace_self, workspace_pattern];

    for (dir, level) in extra_dirs {
        let dir_self = dir.display().to_string();

        // Collect the canonical path and its symlink alias (if any).
        // On macOS /tmp, /var, /etc are symlinks to /private/{...}; the sandbox
        // checks literal strings, so we must include both forms.
        let mut paths = vec![dir_self.clone()];
        if let Some(alias) = macos_symlink_alias(&dir_self) {
            paths.push(alias);
        }

        for p in &paths {
            read_patterns.push(p.clone());
            read_patterns.push(format!("{p}/**"));
            if *level == AccessLevel::Rw {
                write_patterns.push(p.clone());
                write_patterns.push(format!("{p}/**"));
            }
        }
    }

    let mut builder = Permissions::builder()
        .fs_read(read_patterns)
        .fs_write(write_patterns)
        .exec_allow(exec_allow.iter().map(|s| s.as_str()));

    if network_allowed {
        builder = builder.network_allow(["*:*"]);
    } else {
        builder = builder.network_deny();
    }

    builder
        .env_allow(env_inherit.iter().map(|s| s.as_str()))
        .build()
}

pub(super) fn make_sandbox(
    workspace: &Path,
    extra_dirs: &[(PathBuf, AccessLevel)],
    sandbox_cfg: &SandboxPolicyConfig,
) -> Result<Sandbox> {
    tracing::debug!(
        workspace = %workspace.display(),
        extra_dirs_count = extra_dirs.len(),
        network_mode = ?sandbox_cfg.network,
        exec_allow_count = sandbox_cfg.exec_allow.len(),
        "building sandbox with permissions"
    );
    let network_allowed = match sandbox_cfg.network {
        SandboxNetworkMode::Allow | SandboxNetworkMode::Ask => true,
        SandboxNetworkMode::Deny => false,
    };
    let config = SandboxConfig {
        permissions: base_permissions(
            workspace,
            extra_dirs,
            &sandbox_cfg.exec_allow,
            network_allowed,
            &sandbox_cfg.env_inherit,
        ),
        work_dir: workspace.to_path_buf(),
        data_dir: None,
        timeout: Duration::from_secs(sandbox_cfg.timeout_secs),
        max_memory_mb: Some(sandbox_cfg.max_memory_mb),
        env_vars: collect_env_vars(&sandbox_cfg.env_inherit),
        broker_socket: None,
    };
    Sandbox::new(config)
}

pub(super) async fn sandbox_with_broker(
    workspace: &Path,
    timeout_secs: u64,
    reminders_lists: &[String],
    extra_dirs: &[(PathBuf, AccessLevel)],
    sandbox_cfg: &SandboxPolicyConfig,
) -> Result<Sandbox> {
    tracing::debug!(
        workspace = %workspace.display(),
        extra_dirs_count = extra_dirs.len(),
        network_mode = ?sandbox_cfg.network,
        exec_allow_count = sandbox_cfg.exec_allow.len(),
        reminders_lists_count = reminders_lists.len(),
        "building sandbox with broker and reminders service"
    );
    let network_allowed = match sandbox_cfg.network {
        SandboxNetworkMode::Allow | SandboxNetworkMode::Ask => true,
        SandboxNetworkMode::Deny => false,
    };
    let mut permissions = base_permissions(
        workspace,
        extra_dirs,
        &sandbox_cfg.exec_allow,
        network_allowed,
        &sandbox_cfg.env_inherit,
    );

    let mut scope = HashMap::new();
    if !reminders_lists.is_empty() {
        scope.insert("lists".to_string(), serde_json::json!(reminders_lists));
    }
    permissions.services.insert(
        "reminders".to_string(),
        ServicePermission {
            access: "readwrite".to_string(),
            scope,
        },
    );

    let mut broker_config = BrokerConfig::new(PolicyEngine::new(permissions.clone()));
    broker_config.register_handler(Arc::new(RemindersHandler));
    let broker_handle = start_broker(broker_config).await?;

    let config = SandboxConfig {
        permissions,
        work_dir: workspace.to_path_buf(),
        data_dir: None,
        timeout: Duration::from_secs(timeout_secs.max(1)),
        max_memory_mb: Some(sandbox_cfg.max_memory_mb),
        env_vars: collect_env_vars(&sandbox_cfg.env_inherit),
        broker_socket: Some(broker_handle.socket_path.clone()),
    };

    Sandbox::new(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    use tempfile::TempDir;

    #[test]
    fn collect_env_vars_uses_configured_keys_only() {
        let key = "CLAWHIVE_EXEC_TEST_ENV";
        unsafe { std::env::set_var(key, "ok") };

        let env = collect_env_vars(&[key.to_string()]);

        assert_eq!(env.get(key), Some(&"ok".to_string()));
        assert!(!env.contains_key("PATH"));
    }

    #[test]
    fn augment_path_like_host_preserves_existing_order_and_dedups() {
        let merged = augment_path_like_host(
            "/usr/bin:/bin:/opt/homebrew/bin",
            &["/opt/homebrew/bin".into(), "/usr/local/bin".into()],
        );
        assert_eq!(
            merged,
            "/usr/bin:/bin:/opt/homebrew/bin:/usr/local/bin".to_string()
        );
    }

    #[test]
    fn augment_path_like_host_adds_missing_candidates() {
        let merged = augment_path_like_host(
            "/usr/bin:/bin",
            &["/opt/homebrew/bin".into(), "/usr/local/bin".into()],
        );
        assert!(merged.contains("/opt/homebrew/bin"));
        assert!(merged.contains("/usr/local/bin"));
    }

    #[test]
    fn base_permissions_apply_exec_network_and_env_config() {
        let tmp = TempDir::new().unwrap();
        let perms = base_permissions(
            tmp.path(),
            &[],
            &["sh".into(), "jq".into()],
            true,
            &["PATH".into(), "HOME".into()],
        );

        assert_eq!(perms.exec, vec!["sh".to_string(), "jq".to_string()]);
        assert_eq!(perms.network.allow, vec!["*:*".to_string()]);
        assert_eq!(perms.env, vec!["PATH".to_string(), "HOME".to_string()]);
    }
}
