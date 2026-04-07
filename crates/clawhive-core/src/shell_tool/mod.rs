mod exec_approval;
mod network;
mod sandbox;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use clawhive_bus::BusPublisher;
use clawhive_provider::{LlmMessage, ToolDef};

use super::access_gate::AccessGate;
use super::approval::ApprovalRegistry;
use super::config::{
    ExecAskMode, ExecSecurityConfig, ExecSecurityMode, SandboxNetworkMode, SandboxPolicyConfig,
};
use super::router::LlmRouter;
use super::tool::{ToolContext, ToolExecutor, ToolOutput};
use network::{domain_matches, extract_network_targets, package_manager_domains};
use sandbox::{make_sandbox, sandbox_with_broker};

pub use sandbox::{augment_path_like_host, default_path_candidates};

const MAX_OUTPUT_BYTES: usize = 20_000;

/// Lightweight handle for generating approval summaries via LLM.
#[derive(Clone)]
pub struct ApprovalSummarizer {
    router: LlmRouter,
    model: String,
    fallbacks: Vec<String>,
}

impl ApprovalSummarizer {
    pub fn new(router: LlmRouter, model: String, fallbacks: Vec<String>) -> Self {
        Self {
            router,
            model,
            fallbacks,
        }
    }

    pub async fn summarize(&self, command: &str, network_target: Option<&str>) -> Option<String> {
        let mut prompt = format!(
            "Describe what the following command does in one short sentence, in the same language as the command context. \
             Use plain language a non-technical user can understand. Do not include the command itself. \
             Output only the summary sentence, nothing else.\n\nCommand: {command}"
        );
        if let Some(target) = network_target {
            prompt.push_str(&format!("\nNetwork target: {target}"));
        }

        let result = tokio::time::timeout(
            Duration::from_secs(5),
            self.router.chat(
                &self.model,
                &self.fallbacks,
                Some(
                    "You are a concise command summarizer. Output only one summary sentence, nothing else."
                        .to_string(),
                ),
                vec![LlmMessage::user(&prompt)],
                128,
            ),
        )
        .await;

        match result {
            Ok(Ok(resp)) => {
                let text = resp.text.trim().to_string();
                if text.is_empty() {
                    None
                } else {
                    Some(text)
                }
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "approval summary LLM call failed");
                None
            }
            Err(_) => {
                tracing::warn!("approval summary LLM call timed out");
                None
            }
        }
    }
}

pub struct ExecuteCommandTool {
    workspace: PathBuf,
    default_timeout: u64,
    gate: Arc<AccessGate>,
    exec_security: ExecSecurityConfig,
    sandbox_config: SandboxPolicyConfig,
    approval_registry: Option<Arc<ApprovalRegistry>>,
    bus: Option<BusPublisher>,
    agent_id: String,
    approval_summarizer: Option<ApprovalSummarizer>,
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
        approval_summarizer: Option<ApprovalSummarizer>,
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
            approval_summarizer,
        }
    }
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
                            if ctx.is_scheduled_task() {
                                tracing::info!(
                                    target: "clawhive::audit::exec",
                                    agent_id = %self.agent_id,
                                    command = %command,
                                    "auto-approved: scheduled task execution"
                                );
                            } else if let Some(reason) =
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
                } else if self.exec_security.ask == ExecAskMode::Always
                    && !runtime_allowed
                    && !ctx.is_scheduled_task()
                {
                    if let Some(reason) = self.wait_for_approval(command, source_info).await? {
                        return Ok(ToolOutput {
                            content: reason,
                            is_error: true,
                        });
                    }
                }
            }
            ExecSecurityMode::Full => {
                let runtime_allowed = match self.approval_registry.as_ref() {
                    Some(registry) => registry.is_runtime_allowed(&self.agent_id, command).await,
                    None => false,
                };
                if self.exec_security.ask == ExecAskMode::Always
                    && !runtime_allowed
                    && !ctx.is_scheduled_task()
                {
                    if let Some(reason) = self.wait_for_approval(command, source_info).await? {
                        return Ok(ToolOutput {
                            content: reason,
                            is_error: true,
                        });
                    }
                }
            }
        }

        // Network approval flow (ask mode)
        if self.sandbox_config.network == SandboxNetworkMode::Ask {
            let targets = extract_network_targets(command);
            let pkg_domains = package_manager_domains(command);

            for (host, port) in &targets {
                let is_whitelisted = self
                    .sandbox_config
                    .network_allow
                    .iter()
                    .any(|pattern| domain_matches(pattern, host));

                // Hard baseline: block private/loopback/metadata targets (SSRF protection)
                // This is non-negotiable — cannot be bypassed via approval
                if !is_whitelisted && HardBaseline::network_denied(host, *port) {
                    tracing::warn!(
                        target: "clawhive::audit::network",
                        agent_id = %self.agent_id,
                        tool = "execute_command",
                        host = %host,
                        port = %port,
                        command = %command,
                        "network access denied by hard baseline (SSRF protection)"
                    );
                    return Ok(ToolOutput {
                        content: format!(
                            "Network access denied: {}:{} is a private/loopback address blocked by hard baseline. ", host, port
                        ),
                        is_error: true,
                    });
                }
                let is_pkg_manager = pkg_domains.iter().any(|d| domain_matches(d, host));
                let is_runtime_allowed = match self.approval_registry.as_ref() {
                    Some(reg) => reg.is_network_allowed(&self.agent_id, host, *port).await,
                    None => false,
                };

                if !is_whitelisted && !is_pkg_manager && !is_runtime_allowed {
                    if ctx.is_scheduled_task() {
                        tracing::info!(
                            target: "clawhive::audit::network",
                            agent_id = %self.agent_id,
                            host = %host,
                            port = %port,
                            "auto-approved: scheduled task network access"
                        );
                    } else if let Some(reason) = self
                        .wait_for_network_approval(command, host, *port, source_info)
                        .await?
                    {
                        tracing::warn!(
                            target: "clawhive::audit::network",
                            agent_id = %self.agent_id,
                            tool = "execute_command",
                            host = %host,
                            port = %port,
                            command = %command,
                            "network access denied"
                        );
                        return Ok(ToolOutput {
                            content: reason,
                            is_error: true,
                        });
                    } else {
                        tracing::info!(
                            target: "clawhive::audit::network",
                            agent_id = %self.agent_id,
                            tool = "execute_command",
                            host = %host,
                            port = %port,
                            command = %command,
                            "network access granted"
                        );
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
            )
            .with_module(module_path!());
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
            )
            .with_module(module_path!());
            entry.emit();
            return Ok(ToolOutput {
                content: "Command denied: not in allowed exec list for this skill".to_string(),
                is_error: true,
            });
        }

        let timeout = Duration::from_secs(timeout_secs.max(1));
        let start = Instant::now();

        // Log command execution details
        let command_preview = if command.len() > 200 {
            format!("{}...", &command[..command.floor_char_boundary(200)])
        } else {
            command.to_string()
        };
        tracing::info!(
            command = %command_preview,
            timeout_secs = timeout_secs,
            enable_reminders_service = enable_reminders_service,
            agent_id = %self.agent_id,
            "executing command in sandbox"
        );

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
                    let mut truncate_at = MAX_OUTPUT_BYTES;
                    while truncate_at > 0 && !combined.is_char_boundary(truncate_at) {
                        truncate_at -= 1;
                    }
                    combined.truncate(truncate_at);
                    combined.push_str("\n...(output truncated)");
                }

                let exit_code = output.exit_code;
                let mut is_error = !output.exit_code.eq(&0);

                tracing::debug!(
                    exit_code = exit_code,
                    duration_ms = duration_ms,
                    stdout_bytes = output.stdout.len(),
                    stderr_bytes = output.stderr.len(),
                    was_killed = output.was_killed,
                    "command execution completed"
                );

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
                )
                .with_module(module_path!());
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
                )
                .with_module(module_path!());
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
    use std::path::Path;

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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
        );

        assert!(tool.is_command_allowed("jq --version"));
        assert!(tool.is_command_allowed("/usr/bin/jq .foo data.json"));
        assert!(!tool.is_command_allowed("cat data.json"));
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
            None,
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
            None,
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
            None,
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
            None,
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
    async fn always_allow_normalizes_env_prefixed_command() {
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
            None,
        );
        let ctx = ToolContext::builtin();

        let first = tokio::spawn(async move {
            tool.execute(
                serde_json::json!({"command": "FOO=bar printf normalized"}),
                &ctx,
            )
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
            None,
        );
        let ctx2 = ToolContext::builtin();
        let second = tokio::spawn(async move {
            tool_again
                .execute(serde_json::json!({"command": "printf normalized"}), &ctx2)
                .await
                .unwrap()
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !approval_registry.has_pending().await,
            "normalized command should not require approval"
        );

        let second_output = second.await.unwrap();
        assert!(!second_output.is_error);
        assert!(second_output.content.contains("normalized"));
    }

    #[tokio::test]
    async fn ask_always_skips_repeat_prompt_when_runtime_allowed() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let approval_registry = Arc::new(ApprovalRegistry::new());
        approval_registry
            .add_runtime_allow_pattern("agent-test", "printf *".to_string())
            .await;

        let tool = ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            10,
            gate,
            ExecSecurityConfig {
                security: ExecSecurityMode::Allowlist,
                ask: ExecAskMode::Always,
                allowlist: vec![],
                safe_bins: vec![],
            },
            SandboxPolicyConfig::default(),
            Some(approval_registry.clone()),
            None,
            "agent-test".to_string(),
            None,
        );
        let ctx = ToolContext::builtin();

        let output = tool
            .execute(serde_json::json!({"command": "printf no-repeat"}), &ctx)
            .await
            .unwrap();

        assert!(!output.is_error);
        assert!(output.content.contains("no-repeat"));
        assert!(
            !approval_registry.has_pending().await,
            "runtime-allowed command should bypass ask=Always"
        );
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
            None,
        );
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(serde_json::json!({"command": "printf denied"}), &ctx)
            .await
            .unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("no approval UI available"));
    }

    #[tokio::test]
    async fn hard_baseline_blocks_localhost_in_network_ask_mode() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let sandbox = SandboxPolicyConfig {
            network: SandboxNetworkMode::Ask,
            ..Default::default()
        };
        let tool = ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            10,
            gate,
            ExecSecurityConfig {
                security: ExecSecurityMode::Full,
                ask: ExecAskMode::Off,
                allowlist: vec![],
                safe_bins: vec![],
            },
            sandbox,
            None,
            None,
            "agent-test".to_string(),
            None,
        );
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(
                serde_json::json!({"command": "curl -sS http://localhost:8001/health"}),
                &ctx,
            )
            .await
            .unwrap();

        assert!(
            result.is_error,
            "localhost should be blocked by hard baseline"
        );
        assert!(
            result.content.contains("hard baseline") || result.content.contains("denied"),
            "error should mention hard baseline, got: {}",
            result.content
        );
    }
}
