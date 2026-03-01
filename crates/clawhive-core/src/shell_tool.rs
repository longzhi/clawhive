use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use clawhive_bus::BusPublisher;
use clawhive_provider::ToolDef;
use clawhive_schema::{ApprovalDecision, BusMessage};
use corral_core::{
    start_broker, BrokerConfig, Permissions, PolicyEngine, Sandbox, SandboxConfig, ServiceHandler,
    ServicePermission,
};

use super::access_gate::{AccessGate, AccessLevel};
use super::approval::ApprovalRegistry;
use super::config::{ExecAskMode, ExecSecurityConfig, ExecSecurityMode, SandboxPolicyConfig};
use super::tool::{ToolContext, ToolExecutor, ToolOutput};

const MAX_OUTPUT_BYTES: usize = 50_000;

pub struct ExecuteCommandTool {
    workspace: PathBuf,
    default_timeout: u64,
    gate: Arc<AccessGate>,
    exec_security: ExecSecurityConfig,
    sandbox_config: SandboxPolicyConfig,
    approval_registry: Option<Arc<ApprovalRegistry>>,
    bus: Option<BusPublisher>,
    agent_id: String,
}

impl ExecuteCommandTool {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        workspace: PathBuf,
        default_timeout: u64,
        gate: Arc<AccessGate>,
        exec_security: ExecSecurityConfig,
        sandbox_config: SandboxPolicyConfig,
        approval_registry: Option<Arc<ApprovalRegistry>>,
        bus: Option<BusPublisher>,
        agent_id: String,
    ) -> Self {
        Self {
            workspace,
            default_timeout,
            gate,
            exec_security,
            sandbox_config,
            approval_registry,
            bus,
            agent_id,
        }
    }

    async fn wait_for_approval(
        &self,
        command: &str,
        source_info: Option<(&str, &str, &str)>,
    ) -> Result<Option<String>> {
        let Some(registry) = self.approval_registry.as_ref() else {
            return Ok(Some(
                "Command not in allowlist and no approval UI available".to_string(),
            ));
        };

        let trace_id = uuid::Uuid::new_v4();
        tracing::info!(command, %trace_id, "requesting exec approval");

        let rx = registry
            .request(trace_id, command.to_string(), self.agent_id.clone())
            .await;

        if let (Some(bus), Some((ch_type, conn_id, conv_scope))) = (self.bus.as_ref(), source_info)
        {
            let _ = bus
                .publish(BusMessage::NeedHumanApproval {
                    trace_id,
                    reason: format!("Command requires approval: {command}"),
                    agent_id: self.agent_id.clone(),
                    command: command.to_string(),
                    source_channel_type: Some(ch_type.to_string()),
                    source_connector_id: Some(conn_id.to_string()),
                    source_conversation_scope: Some(conv_scope.to_string()),
                })
                .await;
        }

        match tokio::time::timeout(Duration::from_secs(60), rx).await {
            Ok(Ok(ApprovalDecision::AllowOnce)) => Ok(None),
            Ok(Ok(ApprovalDecision::AlwaysAllow)) => {
                let first_token = command.split_whitespace().next().unwrap_or(command);
                let pattern = format!("{first_token} *");
                registry
                    .add_runtime_allow_pattern(&self.agent_id, pattern.clone())
                    .await;
                tracing::info!(pattern, "adding to exec allowlist");
                Ok(None)
            }
            Ok(Ok(ApprovalDecision::Deny)) | Ok(Err(_)) => {
                Ok(Some("Command denied by user".to_string()))
            }
            Err(_) => Ok(Some("Exec approval timed out (60s)".to_string())),
        }
    }

    fn is_command_allowed(&self, command: &str) -> bool {
        let cmd_lower = command.to_lowercase();
        let first_token = command.split_whitespace().next().unwrap_or("");
        let basename = std::path::Path::new(first_token)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(first_token);

        if self.exec_security.safe_bins.iter().any(|b| b == basename) {
            return true;
        }

        self.exec_security.allowlist.iter().any(|pattern| {
            if pattern.ends_with(" *") {
                let prefix = &pattern[..pattern.len() - 2];
                basename == prefix || first_token == prefix
            } else {
                cmd_lower == pattern.to_lowercase() || basename == pattern
            }
        })
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

fn collect_env_vars(env_inherit: &[String]) -> HashMap<String, String> {
    let mut env_vars = HashMap::new();
    for key in env_inherit {
        if let Ok(val) = std::env::var(key) {
            env_vars.insert(key.clone(), val);
        }
    }
    env_vars
}

fn base_permissions(
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
        let pattern = format!("{dir_self}/**");
        read_patterns.push(dir_self.clone());
        read_patterns.push(pattern.clone());
        if *level == AccessLevel::Rw {
            write_patterns.push(dir_self);
            write_patterns.push(pattern);
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

fn make_sandbox(
    workspace: &Path,
    extra_dirs: &[(PathBuf, AccessLevel)],
    sandbox_cfg: &SandboxPolicyConfig,
) -> Result<Sandbox> {
    let network_allowed = sandbox_cfg.network.unwrap_or(cfg!(target_os = "macos"));
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

async fn sandbox_with_broker(
    workspace: &Path,
    timeout_secs: u64,
    reminders_lists: &[String],
    extra_dirs: &[(PathBuf, AccessLevel)],
    sandbox_cfg: &SandboxPolicyConfig,
) -> Result<Sandbox> {
    let network_allowed = sandbox_cfg.network.unwrap_or(cfg!(target_os = "macos"));
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
        let source_info = ctx
            .source_channel_type()
            .zip(ctx.source_connector_id())
            .zip(ctx.source_conversation_scope())
            .map(|((channel_type, connector_id), conversation_scope)| {
                (channel_type, connector_id, conversation_scope)
            });

        match &self.exec_security.security {
            ExecSecurityMode::Deny => {
                return Ok(ToolOutput {
                    content: "Command denied: exec is disabled for this agent".to_string(),
                    is_error: true,
                });
            }
            ExecSecurityMode::Allowlist => {
                let runtime_allowed = match self.approval_registry.as_ref() {
                    Some(registry) => registry.is_runtime_allowed(&self.agent_id, command).await,
                    None => false,
                };
                let is_allowed = self.is_command_allowed(command) || runtime_allowed;
                if !is_allowed {
                    match self.exec_security.ask {
                        ExecAskMode::Off => {
                            return Ok(ToolOutput {
                                content: format!(
                                    "Command not in allowlist. To run this command, add a matching pattern to exec_security.allowlist in agent config. Command: {command}"
                                ),
                                is_error: true,
                            });
                        }
                        ExecAskMode::OnMiss | ExecAskMode::Always => {
                            if let Some(reason) =
                                self.wait_for_approval(command, source_info).await?
                            {
                                return Ok(ToolOutput {
                                    content: if reason.contains("no approval UI available") {
                                        format!("{reason}: {command}")
                                    } else {
                                        reason
                                    },
                                    is_error: true,
                                });
                            }
                        }
                    }
                } else if self.exec_security.ask == ExecAskMode::Always {
                    if let Some(reason) = self.wait_for_approval(command, source_info).await? {
                        return Ok(ToolOutput {
                            content: reason,
                            is_error: true,
                        });
                    }
                }
            }
            ExecSecurityMode::Full => {
                if self.exec_security.ask == ExecAskMode::Always {
                    if let Some(reason) = self.wait_for_approval(command, source_info).await? {
                        return Ok(ToolOutput {
                            content: reason,
                            is_error: true,
                        });
                    }
                }
            }
        }

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
            let sandbox = sandbox_with_broker(
                &self.workspace,
                timeout_secs,
                &reminders_lists,
                &extra_dirs,
                &self.sandbox_config,
            )
            .await?;
            sandbox.execute_with_timeout(command, timeout).await
        } else {
            let sandbox = make_sandbox(&self.workspace, &extra_dirs, &self.sandbox_config)?;
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
    use crate::approval::ApprovalRegistry;
    use crate::config::{ExecAskMode, ExecSecurityConfig, ExecSecurityMode, SandboxPolicyConfig};
    use clawhive_schema::ApprovalDecision;
    use tempfile::TempDir;

    fn make_gate(workspace: &Path) -> Arc<AccessGate> {
        Arc::new(AccessGate::in_memory(workspace.to_path_buf()))
    }

    fn make_tool(tmp: &TempDir) -> ExecuteCommandTool {
        let gate = make_gate(tmp.path());
        ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            10,
            gate,
            ExecSecurityConfig::default(),
            SandboxPolicyConfig::default(),
            None,
            None,
            "test-agent".to_string(),
        )
    }

    fn make_full_mode_tool(tmp: &TempDir, timeout: u64) -> ExecuteCommandTool {
        let gate = make_gate(tmp.path());
        ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            timeout,
            gate,
            ExecSecurityConfig {
                security: ExecSecurityMode::Full,
                ..ExecSecurityConfig::default()
            },
            SandboxPolicyConfig::default(),
            None,
            None,
            "test-agent".to_string(),
        )
    }

    #[tokio::test]
    async fn echo_command() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);
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
        let tool = make_full_mode_tool(&tmp, 10);
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
        let tool = make_full_mode_tool(&tmp, 1);
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
        let tool = make_tool(&tmp);
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

        let tool = make_tool(&tmp);

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
        let tool = make_tool(&tmp);

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
        let tool = make_full_mode_tool(&tmp, 10);

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
        let tool = make_tool(&tmp);
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

    #[tokio::test]
    async fn exec_security_deny_blocks_all_commands() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let tool = ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            10,
            gate,
            ExecSecurityConfig {
                security: ExecSecurityMode::Deny,
                ..ExecSecurityConfig::default()
            },
            SandboxPolicyConfig::default(),
            None,
            None,
            "test-agent".to_string(),
        );
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(serde_json::json!({"command": "echo denied"}), &ctx)
            .await
            .unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("exec is disabled"));
    }

    #[tokio::test]
    async fn exec_security_allowlist_blocks_unlisted_commands() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let tool = ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            10,
            gate,
            ExecSecurityConfig {
                security: ExecSecurityMode::Allowlist,
                allowlist: vec!["git *".into()],
                safe_bins: vec![],
                ..ExecSecurityConfig::default()
            },
            SandboxPolicyConfig::default(),
            None,
            None,
            "test-agent".to_string(),
        );
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(serde_json::json!({"command": "python --version"}), &ctx)
            .await
            .unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("not in allowlist"));
    }

    #[tokio::test]
    async fn exec_security_full_allows_non_baseline_command() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let tool = ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            10,
            gate,
            ExecSecurityConfig {
                security: ExecSecurityMode::Full,
                allowlist: vec![],
                safe_bins: vec![],
                ..ExecSecurityConfig::default()
            },
            SandboxPolicyConfig::default(),
            None,
            None,
            "test-agent".to_string(),
        );
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(serde_json::json!({"command": "echo allowed"}), &ctx)
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("allowed"));
    }

    #[test]
    fn is_command_allowed_matches_allowlist_patterns() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let tool = ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            10,
            gate,
            ExecSecurityConfig {
                security: ExecSecurityMode::Allowlist,
                allowlist: vec!["git *".into(), "pwd".into()],
                safe_bins: vec![],
                ..ExecSecurityConfig::default()
            },
            SandboxPolicyConfig::default(),
            None,
            None,
            "test-agent".to_string(),
        );

        assert!(tool.is_command_allowed("git status"));
        assert!(tool.is_command_allowed("git"));
        assert!(tool.is_command_allowed("pwd"));
        assert!(!tool.is_command_allowed("ls -la"));
    }

    #[test]
    fn is_command_allowed_accepts_safe_bins() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let tool = ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            10,
            gate,
            ExecSecurityConfig {
                security: ExecSecurityMode::Allowlist,
                allowlist: vec![],
                safe_bins: vec!["jq".into()],
                ..ExecSecurityConfig::default()
            },
            SandboxPolicyConfig::default(),
            None,
            None,
            "test-agent".to_string(),
        );

        assert!(tool.is_command_allowed("jq --version"));
        assert!(tool.is_command_allowed("/usr/bin/jq .foo data.json"));
        assert!(!tool.is_command_allowed("cat data.json"));
    }

    #[test]
    fn collect_env_vars_uses_configured_keys_only() {
        let key = "CLAWHIVE_EXEC_TEST_ENV";
        std::env::set_var(key, "ok");

        let env = collect_env_vars(&[key.to_string()]);

        assert_eq!(env.get(key), Some(&"ok".to_string()));
        assert!(!env.contains_key("PATH"));
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

    #[tokio::test]
    async fn allowlist_onmiss_waits_for_allow_once_and_executes() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let approval_registry = Arc::new(ApprovalRegistry::new());
        let tool = ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            10,
            gate,
            ExecSecurityConfig {
                security: ExecSecurityMode::Allowlist,
                ask: ExecAskMode::OnMiss,
                allowlist: vec![],
                safe_bins: vec![],
            },
            SandboxPolicyConfig::default(),
            Some(approval_registry.clone()),
            None,
            "agent-test".to_string(),
        );
        let ctx = ToolContext::builtin();

        let tool_task = tokio::spawn(async move {
            tool.execute(serde_json::json!({"command": "printf approved"}), &ctx)
                .await
                .unwrap()
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(approval_registry.has_pending().await);

        let pending = approval_registry.pending_list().await;
        let (trace_id, _, _) = pending.first().unwrap();
        approval_registry
            .resolve(*trace_id, ApprovalDecision::AllowOnce)
            .await
            .unwrap();

        let output = tool_task.await.unwrap();
        assert!(!output.is_error);
        assert!(output.content.contains("approved"));
    }

    #[tokio::test]
    async fn allowlist_onmiss_deny_blocks_execution() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let approval_registry = Arc::new(ApprovalRegistry::new());
        let tool = ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            10,
            gate,
            ExecSecurityConfig {
                security: ExecSecurityMode::Allowlist,
                ask: ExecAskMode::OnMiss,
                allowlist: vec![],
                safe_bins: vec![],
            },
            SandboxPolicyConfig::default(),
            Some(approval_registry.clone()),
            None,
            "agent-test".to_string(),
        );
        let ctx = ToolContext::builtin();

        let tool_task = tokio::spawn(async move {
            tool.execute(serde_json::json!({"command": "printf denied"}), &ctx)
                .await
                .unwrap()
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        let pending = approval_registry.pending_list().await;
        let (trace_id, _, _) = pending.first().unwrap();
        approval_registry
            .resolve(*trace_id, ApprovalDecision::Deny)
            .await
            .unwrap();

        let output = tool_task.await.unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("denied"));
    }

    #[tokio::test]
    async fn always_allow_persists_for_same_agent_via_registry() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let approval_registry = Arc::new(ApprovalRegistry::new());

        let tool = ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            10,
            gate.clone(),
            ExecSecurityConfig {
                security: ExecSecurityMode::Allowlist,
                ask: ExecAskMode::OnMiss,
                allowlist: vec![],
                safe_bins: vec![],
            },
            SandboxPolicyConfig::default(),
            Some(approval_registry.clone()),
            None,
            "agent-test".to_string(),
        );
        let ctx = ToolContext::builtin();

        let first = tokio::spawn(async move {
            tool.execute(serde_json::json!({"command": "printf persist"}), &ctx)
                .await
                .unwrap()
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let pending = approval_registry.pending_list().await;
        let (trace_id, _, _) = pending.first().unwrap();
        approval_registry
            .resolve(*trace_id, ApprovalDecision::AlwaysAllow)
            .await
            .unwrap();
        let first_output = first.await.unwrap();
        assert!(!first_output.is_error);

        let tool_again = ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            10,
            gate,
            ExecSecurityConfig {
                security: ExecSecurityMode::Allowlist,
                ask: ExecAskMode::OnMiss,
                allowlist: vec![],
                safe_bins: vec![],
            },
            SandboxPolicyConfig::default(),
            Some(approval_registry.clone()),
            None,
            "agent-test".to_string(),
        );
        let ctx2 = ToolContext::builtin();
        let second = tokio::spawn(async move {
            tool_again
                .execute(serde_json::json!({"command": "printf persist"}), &ctx2)
                .await
                .unwrap()
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !approval_registry.has_pending().await,
            "second execution should not require approval"
        );

        let second_output = second.await.unwrap();
        assert!(!second_output.is_error);
        assert!(second_output.content.contains("persist"));
    }

    #[tokio::test]
    async fn allowlist_onmiss_without_registry_denies() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let tool = ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            10,
            gate,
            ExecSecurityConfig {
                security: ExecSecurityMode::Allowlist,
                ask: ExecAskMode::OnMiss,
                allowlist: vec![],
                safe_bins: vec![],
            },
            SandboxPolicyConfig::default(),
            None,
            None,
            "agent-test".to_string(),
        );
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(serde_json::json!({"command": "printf denied"}), &ctx)
            .await
            .unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("no approval UI available"));
    }
}
