use std::sync::Arc;

use anyhow::Result;
use clawhive_provider::{ContentBlock, LlmMessage, LlmRequest};
use clawhive_schema::*;
use tokio_util::sync::CancellationToken;

use crate::access_gate::{AccessGate, GrantAccessTool, ListAccessTool, RevokeAccessTool};
use crate::config::{ExecSecurityConfig, SandboxPolicyConfig, SecurityMode};
use crate::config_view::ConfigView;
use crate::file_tools::{EditFileTool, ReadFileTool, WriteFileTool};
use crate::memory_tools::{
    MemoryForgetTool, MemoryGetTool, MemorySearchTool, MemorySupersedeToolDef, MemoryWriteTool,
};
use crate::shell_tool::ExecuteCommandTool;
use crate::tool::{ToolContext, ToolExecutor};

use super::memory_context::truncate_tool_result_preview;
use super::predicates::{
    collect_recent_messages, is_slow_latency_ms, repair_tool_pairing,
    should_inject_web_search_reminder, should_retry_fabricated_scheduled_response,
    should_retry_incomplete_scheduled_thought, SLOW_LLM_ROUND_WARN_MS, SLOW_TOOL_EXEC_WARN_MS,
};
use super::summary::{
    detect_empty_promise_by_llm, detect_empty_promise_structural, synthesize_cancelled_response,
    EmptyPromiseVerdict,
};
use super::Orchestrator;

#[derive(Debug, Clone, Default)]
pub(super) struct ToolLoopMeta {
    pub(super) successful_tool_calls: usize,
    pub(super) final_stop_reason: Option<String>,
    pub(super) cancelled: bool,
}

impl Orchestrator {
    pub(super) fn has_tool_registered(&self, view: &ConfigView, name: &str) -> bool {
        view.tool_registry
            .tool_defs()
            .iter()
            .any(|tool| tool.name == name)
    }

    pub(super) fn build_runtime_system_prompt(
        &self,
        agent_id: &str,
        model: &str,
        base_prompt: String,
    ) -> String {
        let workspace_root = self.workspace_root_for(agent_id);
        format!(
            "{base_prompt}\n\n## Execution Policy (OVERRIDES any general safety guidelines)\nWhen you have the tools to complete a task, execute them directly without asking for confirmation. Do not announce your intent — act. If a tool call is denied due to access restrictions, use the grant_access tool to request access and retry. The user will be asked to approve the access request. Only ask the user when you genuinely need information you cannot obtain yourself. This policy takes precedence over any 'ask first' or 'when in doubt' guidelines in your workspace files.\n\n### Action-Response Rule (MANDATORY)\nIf your response does not contain tool calls, it MUST NOT promise, commit to, or announce any future action. Either:\n- DO the action (include tool_use blocks in this response), or\n- REPORT what you know (text-only, no action promises)\nNever say 'I will do X', 'Let me do X', or 'I'll fix that' without immediately calling the relevant tool in the SAME response.\n\n## Tool Usage Efficiency\nYou have a limited budget of tool calls per response. Be efficient:\n- Combine multiple file reads into a single `cat file1 file2 file3` command.\n- Use `grep -r pattern dir/` to search across files instead of reading them one by one.\n- Chain related commands with `&&` in a single execute_command call.\n- Do NOT read files one at a time when you need to check multiple files.\n\nRuntime:\n- Model: {model}\n- Session: {agent_id}\n- Working directory: {}",
            workspace_root.display()
        )
    }

    pub(super) async fn execute_tool_for_agent(
        &self,
        view: &ConfigView,
        agent_id: &str,
        name: &str,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<crate::tool::ToolOutput> {
        let gate = self.access_gate_for(agent_id);
        let ws = self.workspace_root_for(agent_id);
        let (exec_security, mut sandbox_config) = view
            .agent(agent_id)
            .map(|agent| {
                (
                    agent.exec_security.clone().unwrap_or_default(),
                    agent.sandbox.clone().unwrap_or_default(),
                )
            })
            .unwrap_or_else(|| {
                (
                    ExecSecurityConfig::default(),
                    SandboxPolicyConfig::default(),
                )
            });

        if let Some(env_vars) = ctx.declared_env_vars() {
            for var in env_vars {
                if !sandbox_config.env_inherit.contains(var) {
                    sandbox_config.env_inherit.push(var.clone());
                }
            }
        }

        for skill in self.active_skill_registry().available() {
            if let Some(perms) = &skill.permissions {
                for var in &perms.env {
                    if !sandbox_config.env_inherit.contains(var) {
                        sandbox_config.env_inherit.push(var.clone());
                    }
                }
            }
        }

        match name {
            "memory_search" => {
                let fact_store = clawhive_memory::fact_store::FactStore::new(self.memory.db());
                MemorySearchTool::new(
                    fact_store,
                    self.search_index_for(agent_id),
                    view.embedding_provider.clone(),
                    agent_id.to_string(),
                )
                .execute(input, ctx)
                .await
            }
            "memory_get" => {
                MemoryGetTool::new(self.file_store_for(agent_id))
                    .execute(input, ctx)
                    .await
            }
            "memory_write" => {
                let fact_store = clawhive_memory::fact_store::FactStore::new(self.memory.db());
                MemoryWriteTool::new(
                    fact_store,
                    self.file_store_for(agent_id),
                    Arc::clone(&self.memory),
                    agent_id.to_string(),
                )
                .execute(input, ctx)
                .await
            }
            "memory_forget" => {
                let fact_store = clawhive_memory::fact_store::FactStore::new(self.memory.db());
                MemoryForgetTool::new(fact_store, agent_id.to_string())
                    .execute(input, ctx)
                    .await
            }
            "memory_supersede" => {
                let fact_store = clawhive_memory::fact_store::FactStore::new(self.memory.db());
                MemorySupersedeToolDef::new(fact_store, agent_id.to_string())
                    .execute(input, ctx)
                    .await
            }
            "read" | "read_file" => ReadFileTool::new(ws, gate).execute(input, ctx).await,
            "write" | "write_file" => WriteFileTool::new(ws, gate).execute(input, ctx).await,
            "edit" | "edit_file" => EditFileTool::new(ws, gate).execute(input, ctx).await,
            "exec" | "execute_command" => {
                let summarizer = view.agent(agent_id).map(|agent| {
                    crate::shell_tool::ApprovalSummarizer::new(
                        view.router.clone(),
                        agent.model_policy.primary.clone(),
                        agent.model_policy.fallbacks.clone(),
                    )
                });
                ExecuteCommandTool::new(
                    ws,
                    sandbox_config.timeout_secs,
                    gate,
                    exec_security,
                    sandbox_config,
                    self.approval_registry.clone(),
                    Some(self.bus.clone()),
                    agent_id.to_string(),
                    summarizer,
                )
                .execute(input, ctx)
                .await
            }
            "grant_access" => self.approve_then_grant(agent_id, &gate, input, ctx).await,
            "list_access" => ListAccessTool::new(gate).execute(input, ctx).await,
            "revoke_access" => RevokeAccessTool::new(gate).execute(input, ctx).await,
            _ => view.tool_registry.execute(name, input, ctx).await,
        }
    }

    /// Require human approval before granting filesystem access.
    pub(super) async fn approve_then_grant(
        &self,
        agent_id: &str,
        gate: &Arc<AccessGate>,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<crate::tool::ToolOutput> {
        let path_str = input["path"].as_str().unwrap_or("unknown");
        let level_str = input["level"].as_str().unwrap_or("unknown");
        let description = format!("grant_{level_str} {path_str}");

        if let Some(registry) = self.approval_registry.as_ref() {
            let trace_id = uuid::Uuid::new_v4();
            tracing::info!(%description, %trace_id, "requesting grant_access approval");

            let rx = registry
                .request(trace_id, description.clone(), agent_id.to_string())
                .await;

            if let (Some(ch), Some(conn), Some(scope)) = (
                ctx.source_channel_type(),
                ctx.source_connector_id(),
                ctx.source_conversation_scope(),
            ) {
                let _ = self
                    .bus
                    .publish(BusMessage::NeedHumanApproval {
                        trace_id,
                        reason: format!("Agent requests access: {description}"),
                        agent_id: agent_id.to_string(),
                        command: description.clone(),
                        network_target: None,
                        summary: None,
                        source_channel_type: Some(ch.to_string()),
                        source_connector_id: Some(conn.to_string()),
                        source_conversation_scope: Some(scope.to_string()),
                    })
                    .await;
            }

            let decision = tokio::time::timeout(std::time::Duration::from_secs(60), rx).await;

            match decision {
                Ok(Ok(ApprovalDecision::AllowOnce)) | Ok(Ok(ApprovalDecision::AlwaysAllow)) => {
                    GrantAccessTool::new(gate.clone()).execute(input, ctx).await
                }
                Ok(Ok(ApprovalDecision::Deny)) => Ok(crate::tool::ToolOutput {
                    content: format!("Access grant denied by user: {description}"),
                    is_error: true,
                }),
                _ => {
                    tracing::warn!(%description, "grant_access approval timed out or channel unavailable");
                    Ok(crate::tool::ToolOutput {
                        content: format!(
                            "Access grant timed out (no response within 60s): {description}"
                        ),
                        is_error: true,
                    })
                }
            }
        } else {
            // No approval channel (e.g. tests) — fall through
            GrantAccessTool::new(gate.clone()).execute(input, ctx).await
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn tool_use_loop(
        &self,
        view: &ConfigView,
        agent_id: &str,
        session_key: &str,
        primary: &str,
        fallbacks: &[String],
        system: Option<String>,
        initial_messages: Vec<LlmMessage>,
        max_tokens: u32,
        allowed_tools: Option<&[String]>,
        merged_permissions: Option<corral_core::Permissions>,
        security_mode: SecurityMode,
        private_network_overrides: Vec<String>,
        source_info: Option<(String, String, String, String)>, // (channel_type, connector_id, conversation_scope, user_scope)
        must_use_web_search: bool,
        is_scheduled_task: bool,
        thinking_level: Option<clawhive_provider::ThinkingLevel>,
        cancel_token: CancellationToken,
    ) -> Result<(
        clawhive_provider::LlmResponse,
        Vec<LlmMessage>,
        Vec<Attachment>,
        ToolLoopMeta,
    )> {
        let mut messages = initial_messages;
        let tool_defs: Vec<_> = match allowed_tools {
            Some(allow_list) => view
                .tool_registry
                .tool_defs()
                .into_iter()
                .filter(|t| allow_list.iter().any(|a| t.name.starts_with(a)))
                .collect(),
            None => view.tool_registry.tool_defs(),
        };
        let max_iterations = view
            .agents
            .get(agent_id)
            .and_then(|a| a.max_iterations)
            .unwrap_or(50) as usize;
        let mut web_search_reminder_injected = false;
        let mut web_search_called = false;
        let loop_started = std::time::Instant::now();
        let mut scheduled_task_retries: u32 = 0;
        let mut empty_promise_retries: u32 = 0;
        let mut total_tool_calls: usize = 0;
        let mut successful_tool_calls_total: usize = 0;
        let mut tool_summaries: Vec<(String, String)> = Vec::new();
        let mut last_intermediate_text = String::new();
        let attachment_collector: Arc<tokio::sync::Mutex<Vec<Attachment>>> =
            Arc::new(tokio::sync::Mutex::new(Vec::new()));

        for iteration in 0..max_iterations {
            let iteration_no = iteration + 1;
            if cancel_token.is_cancelled() {
                tracing::info!(
                    agent_id = %agent_id,
                    iteration = iteration_no,
                    successful_tool_calls = successful_tool_calls_total,
                    "tool_use_loop: cancellation detected before iteration"
                );
                let tool_attachments = attachment_collector.lock().await.drain(..).collect();
                let resp = synthesize_cancelled_response(&tool_summaries);
                return Ok((
                    resp.clone(),
                    messages,
                    tool_attachments,
                    ToolLoopMeta {
                        successful_tool_calls: successful_tool_calls_total,
                        final_stop_reason: resp.stop_reason.clone(),
                        cancelled: true,
                    },
                ));
            }
            tracing::debug!(
                agent_id = %agent_id,
                iteration = iteration_no,
                max_iterations,
                message_count = messages.len(),
                tool_def_count = tool_defs.len(),
                "tool_use_loop: iteration start"
            );

            repair_tool_pairing(&mut messages);

            // Resolve per-model context manager so each agent uses its own context window
            let ctx_mgr = {
                let parts: Vec<&str> = primary.splitn(2, '/').collect();
                if parts.len() == 2 {
                    if let Some(info) =
                        clawhive_schema::provider_presets::model_info(parts[0], parts[1])
                    {
                        self.context_manager
                            .for_context_window(info.context_window as usize)
                    } else {
                        self.context_manager.clone()
                    }
                } else {
                    self.context_manager.clone()
                }
            };

            let _ = ctx_mgr.check_context(&messages);

            let compaction_lock = self.session_compaction_lock(session_key).await;
            let guard = match compaction_lock.try_lock() {
                Ok(guard) => guard,
                Err(_) => {
                    tracing::debug!(session_key = %session_key, "Compaction already in progress, skipping");
                    continue;
                }
            };

            let messages_before_compaction = messages.clone();
            let result = tokio::time::timeout(
                std::time::Duration::from_secs(60),
                ctx_mgr.ensure_within_limits(primary, messages),
            )
            .await;

            drop(guard);

            let (compacted_messages, compaction_result) = match result {
                Ok(Ok(result)) => result,
                Ok(Err(error)) => {
                    tracing::error!(session_key = %session_key, "Compaction failed: {error}");
                    (messages_before_compaction, None)
                }
                Err(_) => {
                    tracing::error!(
                        session_key = %session_key,
                        "Compaction timed out after 60s, skipping"
                    );
                    (messages_before_compaction, None)
                }
            };
            messages = compacted_messages;

            if let Some(ref result) = compaction_result {
                tracing::info!(
                    "Auto-compacted {} messages, saved {} tokens",
                    result.compacted_count,
                    result.tokens_saved
                );
                self.memory
                    .record_trace(
                        agent_id,
                        "compaction",
                        &serde_json::json!({
                            "compacted_count": result.compacted_count,
                            "tokens_saved": result.tokens_saved,
                            "summary_len": result.summary.len(),
                        })
                        .to_string(),
                        None,
                    )
                    .await;
            }

            if cancel_token.is_cancelled() {
                tracing::info!(
                    agent_id = %agent_id,
                    iteration = iteration_no,
                    "tool_use_loop: cancellation detected after compaction"
                );
                let tool_attachments = attachment_collector.lock().await.drain(..).collect();
                let resp = synthesize_cancelled_response(&tool_summaries);
                return Ok((
                    resp.clone(),
                    messages,
                    tool_attachments,
                    ToolLoopMeta {
                        successful_tool_calls: successful_tool_calls_total,
                        final_stop_reason: resp.stop_reason.clone(),
                        cancelled: true,
                    },
                ));
            }

            let req = LlmRequest {
                model: primary.into(),
                system: system.clone(),
                messages: messages.clone(),
                max_tokens,
                tools: tool_defs.clone(),
                thinking_level,
            };

            let llm_started = std::time::Instant::now();
            let resp = view.router.chat_with_tools(primary, fallbacks, req).await?;
            let llm_round_ms = llm_started.elapsed().as_millis() as u64;

            if is_slow_latency_ms(llm_round_ms, SLOW_LLM_ROUND_WARN_MS) {
                tracing::warn!(
                    agent_id = %agent_id,
                    iteration = iteration_no,
                    llm_round_ms,
                    "tool_use_loop: slow LLM round"
                );
            }

            tracing::debug!(
                agent_id = %agent_id,
                iteration = iteration_no,
                llm_round_ms,
                text_len = resp.text.len(),
                content_blocks = resp.content.len(),
                stop_reason = ?resp.stop_reason,
                input_tokens = ?resp.input_tokens,
                output_tokens = ?resp.output_tokens,
                "tool_use_loop: LLM response"
            );

            let text_preview_end = resp.text.floor_char_boundary(300);
            tracing::debug!(
                agent_id = %agent_id,
                iteration = iteration_no,
                text_preview = &resp.text[..text_preview_end],
                "tool_use_loop: LLM response text"
            );

            let tool_uses: Vec<_> = resp
                .content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::ToolUse { id, name, input } => {
                        Some((id.clone(), name.clone(), input.clone()))
                    }
                    _ => None,
                })
                .collect();

            if tool_uses.is_empty() || resp.stop_reason.as_deref() != Some("tool_use") {
                if should_inject_web_search_reminder(
                    must_use_web_search,
                    web_search_reminder_injected,
                    web_search_called,
                    tool_uses.len(),
                ) {
                    web_search_reminder_injected = true;
                    tracing::info!(
                        agent_id = %agent_id,
                        iteration = iteration_no,
                        llm_round_ms,
                        "tool_use_loop: forcing web_search usage reminder"
                    );
                    messages.push(LlmMessage {
                        role: "assistant".into(),
                        content: resp.content.clone(),
                    });
                    messages.push(LlmMessage::user(
                        "You must call the web_search tool now and then provide the answer based on the tool result.",
                    ));
                    continue;
                }

                if should_retry_fabricated_scheduled_response(
                    is_scheduled_task,
                    scheduled_task_retries,
                    total_tool_calls,
                    tool_uses.len(),
                    &resp.text,
                ) {
                    scheduled_task_retries += 1;
                    let task_type = if is_scheduled_task {
                        "scheduled_task"
                    } else {
                        "conversation"
                    };
                    tracing::warn!(
                        agent_id = %agent_id,
                        iteration = iteration_no,
                        retry_count = scheduled_task_retries,
                        response_len = resp.text.len(),
                        task_type,
                        "tool_use_loop: fabricated response detected, nudging to use tools"
                    );
                    messages.push(LlmMessage {
                        role: "assistant".into(),
                        content: resp.content.clone(),
                    });
                    let nudge = if is_scheduled_task {
                        "[SYSTEM] You responded without making any tool calls. \
                         This is UNACCEPTABLE for a scheduled task. \
                         You MUST use tools to execute the task. \
                         Start with step 1 RIGHT NOW: call execute_command or read_file \
                         to begin the work. Do NOT reply with text — your next message \
                         MUST contain tool_use blocks."
                    } else {
                        "[SYSTEM] You claimed to have performed actions, but you did not \
                         make any tool calls. Do NOT fabricate results. Call the appropriate \
                         tool (execute_command, read_file, write_file, etc.) RIGHT NOW to \
                         actually carry out the action."
                    };
                    messages.push(LlmMessage::user(nudge));
                    continue;
                }

                {
                    let max_truncation_retries: u32 = 2;
                    if tool_uses.is_empty()
                        && resp.stop_reason.as_deref() == Some("length")
                        && scheduled_task_retries < max_truncation_retries
                    {
                        scheduled_task_retries += 1;
                        let task_type = if is_scheduled_task {
                            "scheduled_task"
                        } else {
                            "conversation"
                        };
                        tracing::warn!(
                            agent_id = %agent_id,
                            iteration = iteration_no,
                            retry_count = scheduled_task_retries,
                            response_len = resp.text.len(),
                            task_type,
                            "tool_use_loop: output truncated (stop_reason=length), continuing"
                        );
                        messages.push(LlmMessage {
                            role: "assistant".into(),
                            content: resp.content.clone(),
                        });
                        let nudge = if is_scheduled_task {
                            "[SYSTEM] Your output was truncated due to length limits. \
                             Do NOT repeat what you already wrote. Continue from where you left off \
                             and use tools (write_file, execute_command) to complete the remaining steps."
                        } else {
                            "[SYSTEM] Your response was cut short due to length limits. \
                             Summarize the key findings concisely. Do NOT repeat what you already wrote. \
                             Focus on the most important information only."
                        };
                        messages.push(LlmMessage::user(nudge));
                        continue;
                    }
                }

                if tool_uses.is_empty()
                    && should_retry_incomplete_scheduled_thought(
                        is_scheduled_task,
                        scheduled_task_retries,
                        total_tool_calls,
                        &resp.text,
                    )
                {
                    scheduled_task_retries += 1;
                    let task_type = if is_scheduled_task {
                        "scheduled_task"
                    } else {
                        "conversation"
                    };
                    tracing::warn!(
                        agent_id = %agent_id,
                        iteration = iteration_no,
                        retry_count = scheduled_task_retries,
                        response_len = resp.text.len(),
                        task_type,
                        "tool_use_loop: incomplete thought detected, nudging to use tools"
                    );
                    messages.push(LlmMessage {
                        role: "assistant".into(),
                        content: resp.content.clone(),
                    });
                    let nudge = if is_scheduled_task {
                        "[SYSTEM] You stopped mid-task with a planning statement instead of producing output. \
                         Continue executing — use tools to complete the task and produce the final deliverable."
                    } else {
                        "[SYSTEM] You announced your intent but did not act on it. \
                         Do NOT describe what you plan to do — call the appropriate tool NOW \
                         to actually do it."
                    };
                    messages.push(LlmMessage::user(nudge));
                    continue;
                }

                {
                    let verdict = detect_empty_promise_structural(
                        empty_promise_retries,
                        tool_uses.len(),
                        &resp.text,
                    );

                    let is_empty_promise = match verdict {
                        EmptyPromiseVerdict::Structural => true,
                        EmptyPromiseVerdict::Inconclusive => {
                            detect_empty_promise_by_llm(
                                &view.router,
                                primary,
                                fallbacks,
                                &resp.text,
                            )
                            .await
                        }
                        EmptyPromiseVerdict::No => false,
                    };

                    if is_empty_promise {
                        empty_promise_retries += 1;
                        let detection_type = match verdict {
                            EmptyPromiseVerdict::Structural => "structural",
                            _ => "llm",
                        };
                        tracing::warn!(
                            agent_id = %agent_id,
                            iteration = iteration_no,
                            retry_count = empty_promise_retries,
                            response_len = resp.text.len(),
                            detection_type,
                            "tool_use_loop: empty promise detected, nudging to deliver content"
                        );
                        messages.push(LlmMessage {
                            role: "assistant".into(),
                            content: resp.content.clone(),
                        });
                        messages.push(LlmMessage::user(
                            "[SYSTEM] Your response announced or promised content but did not \
                             deliver it. Output the actual content NOW. Do NOT repeat the \
                             introduction or announce what you plan to do — just produce the content.",
                        ));
                        continue;
                    }
                }

                tracing::debug!(
                    agent_id = %agent_id,
                    iteration = iteration_no,
                    llm_round_ms,
                    total_loop_ms = loop_started.elapsed().as_millis() as u64,
                    stop_reason = ?resp.stop_reason,
                    "tool_use_loop: returning final response"
                );
                let mut final_resp = resp.clone();
                if final_resp.text.is_empty() && !last_intermediate_text.is_empty() {
                    tracing::info!(
                        agent_id = %agent_id,
                        fallback_len = last_intermediate_text.len(),
                        "tool_use_loop: final response empty, recovering intermediate text"
                    );
                    final_resp.text = std::mem::take(&mut last_intermediate_text);
                }
                let tool_attachments = attachment_collector.lock().await.drain(..).collect();
                return Ok((
                    final_resp.clone(),
                    messages,
                    tool_attachments,
                    ToolLoopMeta {
                        successful_tool_calls: successful_tool_calls_total,
                        final_stop_reason: final_resp.stop_reason,
                        cancelled: false,
                    },
                ));
            }

            total_tool_calls += tool_uses.len();
            let tool_names: Vec<String> =
                tool_uses.iter().map(|(_, name, _)| name.clone()).collect();
            if tool_names.iter().any(|name| name == "web_search") {
                web_search_called = true;
            }
            if !resp.text.is_empty() {
                last_intermediate_text = resp.text.clone();
            }
            tracing::debug!(
                agent_id = %agent_id,
                iteration = iteration_no,
                tool_use_count = tool_names.len(),
                tool_names = ?tool_names,
                "tool_use_loop: tool calls requested"
            );

            messages.push(LlmMessage {
                role: "assistant".into(),
                content: resp.content.clone(),
            });
            web_search_reminder_injected = false;

            let recent_messages = collect_recent_messages(&messages, 20);
            // Build tool context based on whether we have skill permissions
            // - With permissions: external skill context (sandboxed)
            // - Without: builtin context (trusted, only hard baseline checks)
            let ctx = match merged_permissions.as_ref() {
                Some(perms) => ToolContext::external_with_security_and_private_overrides(
                    perms.clone(),
                    security_mode.clone(),
                    private_network_overrides.clone(),
                ),
                None => ToolContext::builtin_with_security_and_private_overrides(
                    security_mode.clone(),
                    private_network_overrides.clone(),
                ),
            }
            .with_recent_messages(recent_messages)
            .with_attachment_collector(attachment_collector.clone());
            let ctx = ctx
                .with_skill_registry(self.active_skill_registry())
                .with_agent_id(agent_id);
            let ctx = if let Some((ref ch, ref co, ref cv, ref us)) = source_info {
                ctx.with_source(ch.clone(), co.clone(), cv.clone())
                    .with_source_user_scope(us.clone())
            } else {
                ctx
            };
            let ctx = ctx.with_scheduled_task(is_scheduled_task);

            if cancel_token.is_cancelled() {
                tracing::info!(
                    agent_id = %agent_id,
                    iteration = iteration_no,
                    "tool_use_loop: cancellation detected before tool execution"
                );
                let tool_attachments = attachment_collector.lock().await.drain(..).collect();
                let resp = synthesize_cancelled_response(&tool_summaries);
                return Ok((
                    resp.clone(),
                    messages,
                    tool_attachments,
                    ToolLoopMeta {
                        successful_tool_calls: successful_tool_calls_total,
                        final_stop_reason: resp.stop_reason.clone(),
                        cancelled: true,
                    },
                ));
            }

            let tool_futures: Vec<_> = tool_uses
                .into_iter()
                .map(|(id, name, input)| {
                    let ctx = ctx.clone();
                    let agent_id = agent_id.to_string();
                    let tool_name = name.clone();
                    async move {
                        let input_str = input.to_string();
                        let input_preview_end = input_str.floor_char_boundary(300);
                        tracing::debug!(
                            agent_id = %agent_id,
                            tool_name = %tool_name,
                            input_preview = &input_str[..input_preview_end],
                            "tool_use_loop: tool input"
                        );
                        let input_bytes = input_str.len();
                        let tool_started = std::time::Instant::now();
                        match self
                            .execute_tool_for_agent(view, &agent_id, &name, input, &ctx)
                            .await
                        {
                            Ok(output) => {
                                let duration_ms = tool_started.elapsed().as_millis() as u64;
                                let output_preview_end = output.content.floor_char_boundary(200);
                                tracing::info!(
                                    agent_id = %agent_id,
                                    tool_name = %tool_name,
                                    duration_ms,
                                    is_error = output.is_error,
                                    output_preview = &output.content[..output_preview_end],
                                    "tool executed"
                                );
                                if is_slow_latency_ms(duration_ms, SLOW_TOOL_EXEC_WARN_MS) {
                                    tracing::warn!(
                                        agent_id = %agent_id,
                                        tool_name = %tool_name,
                                        duration_ms,
                                        "tool execution slow"
                                    );
                                }
                                ContentBlock::ToolResult {
                                    tool_use_id: id,
                                    content: output.content,
                                    is_error: output.is_error,
                                }
                            }
                            Err(e) => {
                                let duration_ms = tool_started.elapsed().as_millis() as u64;
                                tracing::warn!(
                                    agent_id = %agent_id,
                                    tool_name = %tool_name,
                                    duration_ms,
                                    input_bytes,
                                    error = %e,
                                    "tool_use_loop: tool execution failed"
                                );
                                ContentBlock::ToolResult {
                                    tool_use_id: id,
                                    content: format!("Tool execution error: {e}"),
                                    is_error: true,
                                }
                            }
                        }
                    }
                })
                .collect();

            let tools_started = std::time::Instant::now();
            let tool_results = futures::future::join_all(tool_futures).await;
            tool_summaries.extend(tool_names.iter().zip(tool_results.iter()).filter_map(
                |(name, result)| match result {
                    ContentBlock::ToolResult {
                        content, is_error, ..
                    } if !is_error => {
                        Some((name.clone(), truncate_tool_result_preview(content, 100)))
                    }
                    _ => None,
                },
            ));
            let successful_tool_calls = tool_results
                .iter()
                .filter(|result| {
                    matches!(
                        result,
                        ContentBlock::ToolResult {
                            is_error: false,
                            ..
                        }
                    )
                })
                .count();
            let tools_round_ms = tools_started.elapsed().as_millis() as u64;

            if is_slow_latency_ms(tools_round_ms, SLOW_LLM_ROUND_WARN_MS) {
                tracing::warn!(
                    agent_id = %agent_id,
                    iteration = iteration_no,
                    tools_round_ms,
                    "tool_use_loop: slow tool result round"
                );
            } else {
                tracing::debug!(
                    agent_id = %agent_id,
                    iteration = iteration_no,
                    tools_round_ms,
                    "tool_use_loop: tool results collected"
                );
            }

            successful_tool_calls_total += successful_tool_calls;

            if cancel_token.is_cancelled() {
                tracing::info!(
                    agent_id = %agent_id,
                    iteration = iteration_no,
                    successful_tool_calls = successful_tool_calls_total,
                    "tool_use_loop: cancellation detected after tool execution"
                );
                let tool_attachments = attachment_collector.lock().await.drain(..).collect();
                let resp = synthesize_cancelled_response(&tool_summaries);
                return Ok((
                    resp.clone(),
                    messages,
                    tool_attachments,
                    ToolLoopMeta {
                        successful_tool_calls: successful_tool_calls_total,
                        final_stop_reason: resp.stop_reason.clone(),
                        cancelled: true,
                    },
                ));
            }

            messages.push(LlmMessage {
                role: "user".into(),
                content: tool_results,
            });

            let _ = successful_tool_calls;

            let remaining = max_iterations - iteration_no;
            let threshold = max_iterations / 5; // warn at 80%
            if remaining > 0 && remaining <= threshold {
                messages.push(LlmMessage::user(format!(
                    "[SYSTEM: You have {remaining} tool call(s) remaining. \
                     Finish the current task now — do not start new exploratory work.]"
                )));
            }
        }

        // Loop exhausted — ask the LLM for a final answer without tools
        // so the user gets a response instead of an opaque error.
        tracing::warn!(
            agent_id = %agent_id,
            max_iterations,
            total_loop_ms = loop_started.elapsed().as_millis() as u64,
            "tool_use_loop exhausted iterations, requesting final answer without tools"
        );

        // Add a nudge so the LLM produces a text reply instead of empty content
        messages.push(LlmMessage::user(
            "You have reached the maximum number of tool iterations. \
             Please provide your final response to the user based on the information gathered above."
        ));

        let final_req = LlmRequest {
            model: primary.into(),
            system: system.clone(),
            messages: messages.clone(),
            max_tokens,
            tools: vec![],
            thinking_level,
        };
        let mut resp = view
            .router
            .chat_with_tools(primary, fallbacks, final_req)
            .await?;

        // Fallback: if the LLM still returned empty, extract the last successful
        // tool result so the user sees *something* useful.
        if resp.text.trim().is_empty() {
            tracing::warn!(
                agent_id = %agent_id,
                "final answer still empty after nudge, extracting last tool result as fallback"
            );
            let fallback = messages
                .iter()
                .rev()
                .flat_map(|m| m.content.iter())
                .find_map(|block| match block {
                    ContentBlock::ToolResult {
                        content, is_error, ..
                    } if !is_error && !content.trim().is_empty() => Some(content.clone()),
                    _ => None,
                });
            if let Some(text) = fallback {
                resp.text = text;
            }
        }

        let tool_attachments = attachment_collector.lock().await.drain(..).collect();
        Ok((
            resp.clone(),
            messages,
            tool_attachments,
            ToolLoopMeta {
                successful_tool_calls: successful_tool_calls_total,
                final_stop_reason: resp.stop_reason.clone(),
                cancelled: false,
            },
        ))
    }
}
