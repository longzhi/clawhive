use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use clawhive_provider::ToolDef;
use corral_core::{
    start_broker, BrokerConfig, Permissions, PolicyEngine, Sandbox, SandboxConfig, ServiceHandler,
    ServicePermission,
};

use super::access_gate::{AccessGate, AccessLevel};
use super::tool::{ToolContext, ToolExecutor, ToolOutput};

const MAX_OUTPUT_BYTES: usize = 50_000;

pub struct ExecuteCommandTool {
    workspace: PathBuf,
    default_timeout: u64,
    gate: Arc<AccessGate>,
}

impl ExecuteCommandTool {
    pub fn new(workspace: PathBuf, default_timeout: u64, gate: Arc<AccessGate>) -> Self {
        Self {
            workspace,
            default_timeout,
            gate,
        }
    }
}

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

fn collect_env_vars() -> HashMap<String, String> {
    let mut env_vars = HashMap::new();
    for key in ["PATH", "HOME", "TMPDIR"] {
        if let Ok(val) = std::env::var(key) {
            env_vars.insert(key.to_string(), val);
        }
    }
    env_vars
}

fn base_permissions(
    workspace: &Path,
    extra_dirs: &[(PathBuf, AccessLevel)],
) -> Permissions {
    let workspace_pattern = format!("{}/**", workspace.display());
    let mut read_patterns = vec![workspace_pattern.clone()];
    let mut write_patterns = vec![workspace_pattern];

    for (dir, level) in extra_dirs {
        let pattern = format!("{}/**", dir.display());
        read_patterns.push(pattern.clone());
        if *level == AccessLevel::Rw {
            write_patterns.push(pattern);
        }
    }

    Permissions::builder()
        .fs_read(read_patterns)
        .fs_write(write_patterns)
        .exec_allow(["sh"])
        .network_deny()
        .env_allow(["PATH", "HOME", "TMPDIR"])
        .build()
}

fn make_sandbox(
    workspace: &Path,
    extra_dirs: &[(PathBuf, AccessLevel)],
) -> Result<Sandbox> {
    let config = SandboxConfig {
        permissions: base_permissions(workspace, extra_dirs),
        work_dir: workspace.to_path_buf(),
        data_dir: None,
        timeout: Duration::from_secs(30),
        max_memory_mb: Some(512),
        env_vars: collect_env_vars(),
        broker_socket: None,
    };
    Sandbox::new(config)
}

async fn sandbox_with_broker(
    workspace: &Path,
    timeout_secs: u64,
    reminders_lists: &[String],
    extra_dirs: &[(PathBuf, AccessLevel)],
) -> Result<Sandbox> {
    let mut permissions = base_permissions(workspace, extra_dirs);

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
        max_memory_mb: Some(512),
        env_vars: collect_env_vars(),
        broker_socket: Some(broker_handle.socket_path.clone()),
    };

    Sandbox::new(config)
}

#[async_trait]
impl ToolExecutor for ExecuteCommandTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "execute_command".into(),
            description: "Execute a shell command in a Corral sandbox scoped to the workspace directory. Returns stdout and stderr. Optional broker-backed reminders service can be enabled with explicit permission.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute (passed to sh -c)"
                    },
                    "timeout_seconds": {
                        "type": "integer",
                        "description": "Timeout in seconds (default: 30)"
                    },
                    "enable_reminders_service": {
                        "type": "boolean",
                        "description": "Enable broker-backed reminders service for this execution (default: false)"
                    },
                    "reminders_lists": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Optional allowed reminder lists when reminders service is enabled"
                    }
                },
                "required": ["command"]
            }),
        }
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        use super::audit::ToolAuditEntry;
        use super::policy::HardBaseline;
        use std::time::Instant;

        let command = input["command"]
            .as_str()
            .ok_or_else(|| anyhow!("missing 'command' field"))?;
        let timeout_secs = input["timeout_seconds"]
            .as_u64()
            .unwrap_or(self.default_timeout);
        let enable_reminders_service = input["enable_reminders_service"].as_bool().unwrap_or(false);
        let reminders_lists = input["reminders_lists"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        // Hard baseline check - applies to ALL tool origins
        if HardBaseline::exec_denied(command) {
            let entry = ToolAuditEntry::denied(
                "execute_command",
                ctx.origin(),
                &input,
                "command blocked by hard baseline",
            );
            entry.emit();
            return Ok(ToolOutput {
                content: "Command denied: matches dangerous pattern (hard baseline)".to_string(),
                is_error: true,
            });
        }

        // Policy context check (external skills need exec permission)
        if !ctx.check_exec(command) {
            let entry = ToolAuditEntry::denied(
                "execute_command",
                ctx.origin(),
                &input,
                "command not in allowed exec list",
            );
            entry.emit();
            return Ok(ToolOutput {
                content: "Command denied: not in allowed exec list for this skill".to_string(),
                is_error: true,
            });
        }

        let timeout = Duration::from_secs(timeout_secs.max(1));
        let start = Instant::now();

        // Build sandbox dynamically to include current allowlist
        let extra_dirs = self.gate.allowed_dirs().await;
        let result = if enable_reminders_service {
            let sandbox =
                sandbox_with_broker(&self.workspace, timeout_secs, &reminders_lists, &extra_dirs)
                    .await?;
            sandbox.execute_with_timeout(command, timeout).await
        } else {
            let sandbox = make_sandbox(&self.workspace, &extra_dirs)?;
            sandbox.execute_with_timeout(command, timeout).await
        };

        let duration_ms = start.elapsed().as_millis() as u64;

        match result {
            Ok(output) => {
                let mut combined = String::new();

                if !output.stdout.is_empty() {
                    combined.push_str(&output.stdout);
                }
                if !output.stderr.is_empty() {
                    if !combined.is_empty() {
                        combined.push('\n');
                    }
                    combined.push_str("[stderr]\n");
                    combined.push_str(&output.stderr);
                }

                if combined.len() > MAX_OUTPUT_BYTES {
                    combined.truncate(MAX_OUTPUT_BYTES);
                    combined.push_str("\n...(output truncated)");
                }

                let exit_code = output.exit_code;
                let mut is_error = !output.exit_code.eq(&0);

                if output.was_killed {
                    is_error = true;
                    if !combined.is_empty() {
                        combined.push('\n');
                    }
                    combined.push_str("[killed: timeout exceeded]");
                }

                if exit_code != 0 {
                    combined.push_str(&format!("\n[exit code: {exit_code}]"));
                }

                let content = if combined.is_empty() {
                    format!("[exit code: {exit_code}]")
                } else {
                    combined
                };

                // Audit log successful execution
                let entry = ToolAuditEntry::success(
                    "execute_command",
                    ctx.origin(),
                    &input,
                    &content,
                    duration_ms,
                );
                entry.emit();

                Ok(ToolOutput { content, is_error })
            }
            Err(e) => {
                // Audit log failed execution
                let entry = ToolAuditEntry::error(
                    "execute_command",
                    ctx.origin(),
                    &input,
                    e.to_string(),
                    duration_ms,
                );
                entry.emit();

                Ok(ToolOutput {
                    content: format!("Failed to execute command: {e}"),
                    is_error: true,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_gate(workspace: &Path) -> Arc<AccessGate> {
        Arc::new(AccessGate::in_memory(workspace.to_path_buf()))
    }

    #[tokio::test]
    async fn echo_command() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let tool = ExecuteCommandTool::new(tmp.path().to_path_buf(), 10, gate);
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(serde_json::json!({"command": "echo hello"}), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("hello"));
    }

    #[tokio::test]
    async fn failing_command() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let tool = ExecuteCommandTool::new(tmp.path().to_path_buf(), 10, gate);
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(serde_json::json!({"command": "exit 1"}), &ctx)
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("exit code: 1"));
    }

    #[tokio::test]
    async fn timeout_command() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let tool = ExecuteCommandTool::new(tmp.path().to_path_buf(), 1, gate);
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(
                serde_json::json!({"command": "sleep 10", "timeout_seconds": 1}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("killed") || result.content.contains("Timeout"));
    }

    #[tokio::test]
    async fn runs_in_workspace_dir() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("marker.txt"), "found").unwrap();
        let gate = make_gate(tmp.path());
        let tool = ExecuteCommandTool::new(tmp.path().to_path_buf(), 10, gate);
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(serde_json::json!({"command": "cat marker.txt"}), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("found"));
    }

    #[tokio::test]
    async fn external_context_requires_exec_permission() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("data.txt"), "hello").unwrap();

        let gate = make_gate(tmp.path());
        let tool = ExecuteCommandTool::new(tmp.path().to_path_buf(), 10, gate);

        // External context with cat allowed
        let perms = corral_core::Permissions {
            fs: corral_core::FsPermissions {
                read: vec![format!("{}/**", tmp.path().display())],
                write: vec![],
            },
            network: corral_core::NetworkPermissions { allow: vec![] },
            exec: vec!["cat".into()],
            env: vec![],
            services: Default::default(),
        };
        let ctx = ToolContext::external(perms);

        let result = tool
            .execute(serde_json::json!({"command": "cat data.txt"}), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("hello"));
    }

    #[tokio::test]
    async fn external_context_denies_unlisted_command() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let tool = ExecuteCommandTool::new(tmp.path().to_path_buf(), 10, gate);

        // External context with only echo allowed
        let perms = corral_core::Permissions {
            fs: corral_core::FsPermissions::default(),
            network: corral_core::NetworkPermissions { allow: vec![] },
            exec: vec!["echo".into()],
            env: vec![],
            services: Default::default(),
        };
        let ctx = ToolContext::external(perms);

        // Try to run ls (not in exec list)
        let result = tool
            .execute(serde_json::json!({"command": "ls"}), &ctx)
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("denied"));
    }

    #[tokio::test]
    async fn hard_baseline_blocks_dangerous_command() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let tool = ExecuteCommandTool::new(tmp.path().to_path_buf(), 10, gate);

        // Even builtin context should block dangerous commands
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(serde_json::json!({"command": "rm -rf /"}), &ctx)
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("denied"));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn denies_network_by_default_on_linux() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let tool = ExecuteCommandTool::new(tmp.path().to_path_buf(), 10, gate);
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(
                serde_json::json!({"command": "curl -sS https://example.com", "timeout_seconds": 5}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(result.is_error);
    }
}
