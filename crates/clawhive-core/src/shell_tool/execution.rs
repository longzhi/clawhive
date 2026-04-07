use std::time::Duration;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use clawhive_provider::ToolDef;

use crate::audit::ToolAuditEntry;
use crate::config::{ExecAskMode, ExecSecurityMode, SandboxNetworkMode};
use crate::policy::HardBaseline;
use crate::tool::{ToolContext, ToolExecutor, ToolOutput};

use super::network::{domain_matches, extract_network_targets, package_manager_domains};
use super::sandbox::{make_sandbox, sandbox_with_broker};
use super::{ExecuteCommandTool, MAX_OUTPUT_BYTES};

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
