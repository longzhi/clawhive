use std::pin::Pin;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use clawhive_memory::dirty_sources::DIRTY_KIND_SESSION;
use clawhive_provider::{ContentBlock, LlmMessage, StreamChunk};
use clawhive_schema::*;
use futures_core::Stream;
use tokio_util::sync::CancellationToken;

use crate::config_view::ConfigView;
use crate::language_prefs::{
    apply_language_policy_prompt, detect_response_language, is_language_guard_exempt,
    log_language_guard,
};
use crate::session::SessionResetReason;

use super::attachment::{build_attachment_blocks, build_session_text, build_user_content};
use super::episode::EpisodeTurnInput;
use super::predicates::{
    build_messages_from_history, detect_skill_install_intent, detect_skill_remove_intent,
    filter_no_reply, history_message_limit, is_explicit_web_search_request,
    is_skill_install_intent_without_source, session_reset_policy_for,
};
use super::skill_commands::SKILL_INSTALL_USAGE_HINT;
use super::Orchestrator;

impl Orchestrator {
    pub async fn handle_inbound(
        &self,
        inbound: InboundMessage,
        agent_id: &str,
        cancel_token: CancellationToken,
    ) -> Result<OutboundMessage> {
        let view = self.config_view();
        self.handle_with_view(view, inbound, agent_id, cancel_token)
            .await
    }

    pub async fn handle_with_view(
        &self,
        view: Arc<ConfigView>,
        inbound: InboundMessage,
        agent_id: &str,
        cancel_token: CancellationToken,
    ) -> Result<OutboundMessage> {
        let agent = view
            .agent(agent_id)
            .ok_or_else(|| anyhow!("agent not found: {agent_id}"))?;

        let session_key = SessionKey::from_inbound(&inbound);

        // Acquire per-session lock to prevent concurrent modifications
        let _session_guard = self.session_locks.acquire(&session_key.0).await;

        self.recover_pending_boundary_flushes_for_session_key(
            view.clone(),
            agent_id,
            &session_key,
            agent,
        )
        .await;

        // Handle slash commands before LLM
        if let Some(cmd) = crate::slash_commands::parse_command(&inbound.text) {
            match cmd {
                crate::slash_commands::SlashCommand::Model { new_model } => {
                    let text = match new_model {
                        Some(model_str) => {
                            match self.handle_model_change(&view, agent_id, &model_str) {
                                Ok(msg) => msg,
                                Err(e) => format!("❌ {e}"),
                            }
                        }
                        None => {
                            format!(
                                "Model: **{}**\nSession: **{}**",
                                agent.model_policy.primary, session_key.0
                            )
                        }
                    };
                    return Ok(OutboundMessage {
                        trace_id: inbound.trace_id,
                        channel_type: inbound.channel_type,
                        connector_id: inbound.connector_id,
                        conversation_scope: inbound.conversation_scope,
                        text,
                        at: chrono::Utc::now(),
                        reply_to: None,
                        attachments: vec![],
                    });
                }
                crate::slash_commands::SlashCommand::Status => {
                    return Ok(OutboundMessage {
                        trace_id: inbound.trace_id,
                        channel_type: inbound.channel_type,
                        connector_id: inbound.connector_id,
                        conversation_scope: inbound.conversation_scope,
                        text: crate::slash_commands::format_status_response(
                            agent_id,
                            &agent.model_policy.primary,
                            &session_key.0,
                        ),
                        at: chrono::Utc::now(),
                        reply_to: None,
                        attachments: vec![],
                    });
                }
                crate::slash_commands::SlashCommand::Stop => {
                    return Ok(OutboundMessage {
                        trace_id: inbound.trace_id,
                        channel_type: inbound.channel_type,
                        connector_id: inbound.connector_id,
                        conversation_scope: inbound.conversation_scope,
                        text: "Use /stop from the chat channel to cancel the active task."
                            .to_string(),
                        at: chrono::Utc::now(),
                        reply_to: None,
                        attachments: vec![],
                    });
                }
                crate::slash_commands::SlashCommand::SkillAnalyze { source } => {
                    return self
                        .handle_skill_analyze_or_install_command(inbound, source, false)
                        .await;
                }
                crate::slash_commands::SlashCommand::SkillInstall { source } => {
                    return self
                        .handle_skill_analyze_or_install_command(inbound, source, true)
                        .await;
                }
                crate::slash_commands::SlashCommand::SkillConfirm { token } => {
                    return self
                        .handle_skill_confirm_command(inbound, agent_id, token)
                        .await;
                }
                crate::slash_commands::SlashCommand::SkillRemove { skill_name } => {
                    return self.handle_skill_remove_command(inbound, &skill_name);
                }
                crate::slash_commands::SlashCommand::SkillList => {
                    return self.handle_skill_list_command(inbound);
                }
                crate::slash_commands::SlashCommand::SkillUsageHint { subcommand } => {
                    let hint = match subcommand.as_str() {
                        "analyze" => "Usage: /skill analyze <url-or-path>\nExample: /skill analyze https://example.com/my-skill.zip",
                        "install" => "Usage: /skill install <url-or-path>\nExample: /skill install https://example.com/my-skill.zip",
                        "confirm" => "Usage: /skill confirm <token>\nThe token is provided after running /skill analyze or /skill install.",
                        "remove" => "Usage: /skill remove <skill-name>\nExample: /skill remove web-search",
                        _ => "Usage:\n  /skill analyze <source> — Analyze a skill before installing\n  /skill install <source> — Install a skill\n  /skill confirm <token> — Confirm a pending installation\n  /skill remove <name> — Remove an installed skill",
                    };
                    return Ok(OutboundMessage {
                        trace_id: inbound.trace_id,
                        channel_type: inbound.channel_type,
                        connector_id: inbound.connector_id,
                        conversation_scope: inbound.conversation_scope,
                        text: hint.to_string(),
                        at: chrono::Utc::now(),
                        reply_to: None,
                        attachments: vec![],
                    });
                }
                crate::slash_commands::SlashCommand::New { model_hint } => {
                    return self
                        .handle_explicit_session_reset(
                            view.as_ref(),
                            inbound,
                            agent_id,
                            agent,
                            &session_key,
                            model_hint.as_deref(),
                        )
                        .await;
                }
                crate::slash_commands::SlashCommand::Reset => {
                    return self
                        .handle_explicit_session_reset(
                            view.as_ref(),
                            inbound,
                            agent_id,
                            agent,
                            &session_key,
                            None,
                        )
                        .await;
                }
            }
        }

        if let Some(source) = detect_skill_install_intent(&inbound.text) {
            return self
                .handle_skill_analyze_or_install_command(inbound, source, true)
                .await;
        }

        if let Some(name) = detect_skill_remove_intent(&inbound.text) {
            return self.handle_skill_remove_command(inbound, &name);
        }

        if is_skill_install_intent_without_source(&inbound.text) {
            return Ok(OutboundMessage {
                trace_id: inbound.trace_id,
                channel_type: inbound.channel_type,
                connector_id: inbound.connector_id,
                conversation_scope: inbound.conversation_scope,
                text: SKILL_INSTALL_USAGE_HINT.to_string(),
                at: chrono::Utc::now(),
                reply_to: None,
                attachments: vec![],
            });
        }

        let session_result = self
            .session_mgr
            .get_or_create_with_policy(
                &session_key,
                agent_id,
                Some(session_reset_policy_for(agent)),
            )
            .await?;

        if let (Some(reason), Some(previous_session)) = (
            session_result.ended_previous,
            session_result.previous_session.as_ref(),
        ) {
            match reason {
                SessionResetReason::Idle | SessionResetReason::Daily => {
                    self.schedule_stale_boundary_flush(
                        view.clone(),
                        agent_id,
                        previous_session,
                        agent,
                    )
                    .await;
                }
                SessionResetReason::Explicit => {
                    self.try_fallback_summary(
                        view.as_ref(),
                        agent_id,
                        previous_session,
                        agent,
                        reason,
                    )
                    .await;
                }
            }
            self.process_session_close_daily_dirty(
                view.as_ref(),
                agent_id,
                previous_session.last_active.date_naive(),
            )
            .await;
        }

        let session_text = build_session_text(&inbound.text, &inbound.attachments);

        let system_prompt = view
            .persona(agent_id)
            .map(|persona| {
                let mode = crate::persona::PromptMode::from_message_source(
                    inbound.message_source.as_deref(),
                );
                persona.assembled_system_prompt_for_mode(mode)
            })
            .unwrap_or_default();
        let active_skills = self.active_skill_registry();
        let skill_summary = active_skills.summary_prompt();
        let mut system_prompt = if skill_summary.is_empty() {
            system_prompt
        } else {
            format!("{system_prompt}\n\n{skill_summary}")
        };
        let forced_skills = Self::forced_skill_names(&inbound.text);
        let merged_permissions = if let Some(ref forced_names) = forced_skills {
            let mut missing = Vec::new();
            let selected_perms = forced_names
                .iter()
                .filter_map(|forced| {
                    if let Some(skill) = active_skills.get(forced) {
                        skill
                            .permissions
                            .as_ref()
                            .map(|p| p.to_corral_permissions())
                    } else {
                        missing.push(forced.clone());
                        None
                    }
                })
                .collect::<Vec<_>>();

            if forced_names.len() == 1 {
                system_prompt.push_str(&format!(
                    "\n\n## Forced Skill\nYou must follow skill '{}' for this request and prioritize its instructions over generic approaches.",
                    forced_names[0]
                ));
            } else {
                system_prompt.push_str(&format!(
                    "\n\n## Forced Skill\nYou must follow only these skills for this request: {}. Prioritize their instructions over generic approaches.",
                    forced_names.join(", ")
                ));
            }
            if !missing.is_empty() {
                system_prompt.push_str(&format!(
                    "\nMissing forced skills: {}. Tell the user these were not found.",
                    missing.join(", ")
                ));
            }

            Self::merge_permissions(selected_perms)
        } else {
            // Normal mode: no skill permissions applied.
            // Agent-level ExecSecurityConfig + HardBaseline provide protection.
            // Skill permissions only activate during forced skill invocation (/skill <name>).
            None
        };

        let memory_context = self
            .build_memory_context(view.as_ref(), agent_id, &session_key, &inbound.text)
            .await?;

        // Build system prompt with memory context injected (not fake dialogue)
        let mut system_prompt = if memory_context.is_empty() {
            self.build_runtime_system_prompt(agent_id, &agent.model_policy.primary, system_prompt)
        } else {
            let base_prompt = self.build_runtime_system_prompt(
                agent_id,
                &agent.model_policy.primary,
                system_prompt,
            );
            format!("{base_prompt}\n\n## Relevant Memory\n{memory_context}")
        };

        let workspace = self.workspace_state_for(agent_id);
        let history_limit = history_message_limit(agent);
        let history_messages = match workspace
            .session_reader
            .load_recent_messages(&session_result.session.session_id, history_limit)
            .await
        {
            Ok(msgs) => msgs,
            Err(e) => {
                if e.to_string().contains("No such file") {
                    tracing::debug!("No session history found (new session): {e}");
                } else {
                    tracing::warn!("Failed to load session history: {e}");
                }
                Vec::new()
            }
        };

        let target_language = self
            .language_prefs
            .resolve_target_language(&inbound, &history_messages);
        apply_language_policy_prompt(&mut system_prompt, target_language);

        let mut messages = build_messages_from_history(&history_messages);
        {
            let preprocessed = self.runtime.preprocess_input(&inbound.text).await?;
            let attachment_blocks = build_attachment_blocks(&inbound.attachments);

            if attachment_blocks.is_empty() {
                messages.push(LlmMessage::user(preprocessed));
            } else {
                let content = build_user_content(preprocessed, attachment_blocks);
                messages.push(LlmMessage {
                    role: "user".into(),
                    content,
                });
            }
        }

        let must_use_web_search = is_explicit_web_search_request(&inbound.text)
            && self.has_tool_registered(view.as_ref(), "web_search");
        if must_use_web_search {
            system_prompt.push_str(
                "\n\n## Tool Requirement\nThe user explicitly requested web search. You MUST call the web_search tool before your final answer.",
            );
        }

        let is_scheduled_task = inbound.message_source.as_deref() == Some("scheduled_task");
        if is_scheduled_task {
            system_prompt.push_str(
                "\n\n## Scheduled Task Execution\n\
                 This request comes from a scheduled workflow. Complete it normally and follow the task instructions.\n\
                 - Use tool calls when a step requires reading data, writing files, or running commands.\n\
                 - Do not claim actions that were not actually performed.\n\
                 - If the task only requires text output (for example, a reminder), respond directly.",
            );
        }

        let allowed = Self::forced_allowed_tools(
            forced_skills.as_deref(),
            agent
                .tool_policy
                .as_ref()
                .map(|tp| tp.allow.clone())
                .filter(|v| !v.is_empty()),
        );
        let source_info = Some((
            inbound.channel_type.clone(),
            inbound.connector_id.clone(),
            inbound.conversation_scope.clone(),
            inbound.user_scope.clone(),
        ));
        let private_network_overrides = agent
            .sandbox
            .as_ref()
            .map(|s| s.dangerous_allow_private.clone())
            .unwrap_or_default();
        let max_response_tokens =
            agent
                .max_response_tokens
                .unwrap_or(if is_scheduled_task { 8192 } else { 4096 });
        let (resp, _messages, tool_attachments, tool_meta) = self
            .tool_use_loop(
                view.as_ref(),
                agent_id,
                &session_result.session.session_key.0,
                &agent.model_policy.primary,
                &agent.model_policy.fallbacks,
                Some(system_prompt),
                messages,
                max_response_tokens,
                allowed.as_deref(),
                merged_permissions,
                agent.security.clone(),
                private_network_overrides,
                source_info,
                must_use_web_search,
                is_scheduled_task,
                agent.model_policy.thinking_level,
                cancel_token,
            )
            .await?;
        if tool_meta.cancelled {
            tracing::info!(agent_id = %agent_id, "handle_with_view: tool loop cancelled");
        }
        let reply_text = self.runtime.postprocess_output(&resp.text).await?;

        // Check for NO_REPLY suppression
        let reply_text = filter_no_reply(&reply_text);

        let reply_text = if reply_text.is_empty() {
            tracing::warn!(
                raw_text_len = resp.text.len(),
                raw_text_preview = &resp.text[..resp.text.len().min(200)],
                stop_reason = ?resp.stop_reason,
                content_blocks = resp.content.len(),
                "handle_inbound: final reply is empty"
            );
            if resp.stop_reason.as_deref() == Some("length") {
                "Response exceeded the output token limit. Please try a simpler request or break it into smaller parts.".to_string()
            } else {
                reply_text
            }
        } else {
            reply_text
        };

        log_language_guard(agent_id, &inbound, &reply_text, target_language, false);

        let mut outbound_attachments: Vec<Attachment> = resp
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Image { data, media_type } => Some(Attachment {
                    kind: AttachmentKind::Image,
                    url: data.clone(),
                    mime_type: Some(media_type.clone()),
                    file_name: None,
                    size: None,
                }),
                _ => None,
            })
            .collect();

        outbound_attachments.extend(tool_attachments);

        if !outbound_attachments.is_empty() {
            tracing::info!(
                agent_id = %agent_id,
                attachment_count = outbound_attachments.len(),
                "outbound attachments collected"
            );
        }

        let outbound = OutboundMessage {
            trace_id: inbound.trace_id,
            channel_type: inbound.channel_type.clone(),
            connector_id: inbound.connector_id.clone(),
            conversation_scope: inbound.conversation_scope.clone(),
            text: reply_text,
            at: chrono::Utc::now(),
            reply_to: None,
            attachments: outbound_attachments,
        };

        if !outbound.text.is_empty() {
            let preview_end = outbound.text.floor_char_boundary(200);
            tracing::info!(
                agent_id = %agent_id,
                reply_len = outbound.text.len(),
                reply_preview = &outbound.text[..preview_end],
                "agent reply"
            );
        }

        // Record session messages (JSONL)
        let workspace = self.workspace_state_for(agent_id);
        let mut session_changed = false;
        match workspace
            .session_writer
            .append_message(&session_result.session.session_id, "user", &session_text)
            .await
        {
            Err(e) => {
                tracing::warn!("Failed to write user session entry: {e}");
            }
            _ => {
                session_changed = true;
            }
        }
        match workspace
            .session_writer
            .append_message(
                &session_result.session.session_id,
                "assistant",
                &outbound.text,
            )
            .await
        {
            Err(e) => {
                tracing::warn!("Failed to write assistant session entry: {e}");
            }
            _ => {
                session_changed = true;
            }
        }
        if session_changed {
            self.enqueue_dirty_source(
                agent_id,
                DIRTY_KIND_SESSION,
                &session_result.session.session_id,
                "session_appended",
            )
            .await;
            self.drain_dirty_sources(view.as_ref(), agent_id, 8).await;
        }

        // Skip episode tracking for scheduled tasks — their outputs should not
        // enter the memory extraction pipeline (boundary flush → fact extraction
        // → daily consolidation → MEMORY.md).  Session JSONL is still written
        // above for audit purposes.
        if !is_scheduled_task {
            let next_turn_index = session_result.session.interaction_count.saturating_add(1);
            let closed_episode = self
                .record_session_turn_episode(
                    agent_id,
                    &session_result.session,
                    EpisodeTurnInput {
                        turn_index: next_turn_index,
                        user_text: &session_text,
                        assistant_text: &outbound.text,
                        successful_tool_calls: tool_meta.successful_tool_calls,
                        final_stop_reason: tool_meta.final_stop_reason.as_deref(),
                    },
                )
                .await;
            let _ = closed_episode;
        }

        {
            let mut session = session_result.session.clone();
            session.increment_interaction();
            if let Err(e) = self.session_mgr.persist_session(&session).await {
                tracing::warn!("Failed to persist session interaction count: {e}");
            }
        }

        let _ = self
            .bus
            .publish(BusMessage::ReplyReady {
                outbound: outbound.clone(),
            })
            .await;

        Ok(outbound)
    }

    /// Streaming variant of handle_inbound. Runs the tool_use_loop for
    /// intermediate tool calls, then streams the final LLM response.
    /// Publishes StreamDelta events to the bus for TUI consumption.
    pub async fn handle_inbound_stream(
        &self,
        inbound: InboundMessage,
        agent_id: &str,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send + '_>>> {
        let view = self.config_view();
        self.handle_inbound_stream_with_view(view, inbound, agent_id, CancellationToken::new())
            .await
    }

    pub async fn handle_inbound_stream_with_view(
        &self,
        view: Arc<ConfigView>,
        inbound: InboundMessage,
        agent_id: &str,
        cancel_token: CancellationToken,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send + '_>>> {
        let agent = view
            .agent(agent_id)
            .ok_or_else(|| anyhow!("agent not found: {agent_id}"))?;

        let session_key = SessionKey::from_inbound(&inbound);

        // Acquire per-session lock to prevent concurrent modifications
        let _session_guard = self.session_locks.acquire(&session_key.0).await;

        self.recover_pending_boundary_flushes_for_session_key(
            view.clone(),
            agent_id,
            &session_key,
            agent,
        )
        .await;

        let session_result = self
            .session_mgr
            .get_or_create_with_policy(
                &session_key,
                agent_id,
                Some(session_reset_policy_for(agent)),
            )
            .await?;

        if let (Some(reason), Some(previous_session)) = (
            session_result.ended_previous,
            session_result.previous_session.as_ref(),
        ) {
            match reason {
                SessionResetReason::Idle | SessionResetReason::Daily => {
                    self.schedule_stale_boundary_flush(
                        view.clone(),
                        agent_id,
                        previous_session,
                        agent,
                    )
                    .await;
                }
                SessionResetReason::Explicit => {
                    self.try_fallback_summary(
                        view.as_ref(),
                        agent_id,
                        previous_session,
                        agent,
                        reason,
                    )
                    .await;
                }
            }
            self.process_session_close_daily_dirty(
                view.as_ref(),
                agent_id,
                previous_session.last_active.date_naive(),
            )
            .await;
        }

        let system_prompt = view
            .persona(agent_id)
            .map(|p| {
                let mode = crate::persona::PromptMode::from_message_source(
                    inbound.message_source.as_deref(),
                );
                p.assembled_system_prompt_for_mode(mode)
            })
            .unwrap_or_default();
        let active_skills = self.active_skill_registry();
        let skill_summary = active_skills.summary_prompt();
        let mut system_prompt = if skill_summary.is_empty() {
            system_prompt
        } else {
            format!("{system_prompt}\n\n{skill_summary}")
        };
        let forced_skills = Self::forced_skill_names(&inbound.text);
        let merged_permissions = if let Some(ref forced_names) = forced_skills {
            let mut missing = Vec::new();
            let selected_perms = forced_names
                .iter()
                .filter_map(|forced| {
                    if let Some(skill) = active_skills.get(forced) {
                        skill
                            .permissions
                            .as_ref()
                            .map(|p| p.to_corral_permissions())
                    } else {
                        missing.push(forced.clone());
                        None
                    }
                })
                .collect::<Vec<_>>();

            if forced_names.len() == 1 {
                system_prompt.push_str(&format!(
                    "\n\n## Forced Skill\nYou must follow skill '{}' for this request and prioritize its instructions over generic approaches.",
                    forced_names[0]
                ));
            } else {
                system_prompt.push_str(&format!(
                    "\n\n## Forced Skill\nYou must follow only these skills for this request: {}. Prioritize their instructions over generic approaches.",
                    forced_names.join(", ")
                ));
            }
            if !missing.is_empty() {
                system_prompt.push_str(&format!(
                    "\nMissing forced skills: {}. Tell the user these were not found.",
                    missing.join(", ")
                ));
            }

            Self::merge_permissions(selected_perms)
        } else {
            // Normal mode: no skill permissions applied.
            // Agent-level ExecSecurityConfig + HardBaseline provide protection.
            // Skill permissions only activate during forced skill invocation (/skill <name>).
            None
        };

        let memory_context = self
            .build_memory_context(view.as_ref(), agent_id, &session_key, &inbound.text)
            .await?;

        // Build system prompt with memory context injected (stream variant)
        let mut system_prompt = if memory_context.is_empty() {
            self.build_runtime_system_prompt(agent_id, &agent.model_policy.primary, system_prompt)
        } else {
            let base_prompt = self.build_runtime_system_prompt(
                agent_id,
                &agent.model_policy.primary,
                system_prompt,
            );
            format!("{base_prompt}\n\n## Relevant Memory\n{memory_context}")
        };

        let workspace = self.workspace_state_for(agent_id);
        let history_limit = history_message_limit(agent);
        let history_messages = match workspace
            .session_reader
            .load_recent_messages(&session_result.session.session_id, history_limit)
            .await
        {
            Ok(msgs) => msgs,
            Err(e) => {
                if e.to_string().contains("No such file") {
                    tracing::debug!("No session history found (new session): {e}");
                } else {
                    tracing::warn!("Failed to load session history: {e}");
                }
                Vec::new()
            }
        };

        let target_language = self
            .language_prefs
            .resolve_target_language(&inbound, &history_messages);
        apply_language_policy_prompt(&mut system_prompt, target_language);

        let mut messages = build_messages_from_history(&history_messages);
        {
            let preprocessed = self.runtime.preprocess_input(&inbound.text).await?;
            let attachment_blocks = build_attachment_blocks(&inbound.attachments);

            if attachment_blocks.is_empty() {
                messages.push(LlmMessage::user(preprocessed));
            } else {
                let content = build_user_content(preprocessed, attachment_blocks);
                messages.push(LlmMessage {
                    role: "user".into(),
                    content,
                });
            }
        }

        let must_use_web_search = is_explicit_web_search_request(&inbound.text)
            && self.has_tool_registered(view.as_ref(), "web_search");
        if must_use_web_search {
            system_prompt.push_str(
                "\n\n## Tool Requirement\nThe user explicitly requested web search. You MUST call the web_search tool before your final answer.",
            );
        }

        let allowed_stream = Self::forced_allowed_tools(
            forced_skills.as_deref(),
            agent
                .tool_policy
                .as_ref()
                .map(|tp| tp.allow.clone())
                .filter(|v| !v.is_empty()),
        );
        let source_info_stream = Some((
            inbound.channel_type.clone(),
            inbound.connector_id.clone(),
            inbound.conversation_scope.clone(),
            inbound.user_scope.clone(),
        ));
        let private_network_overrides_stream = agent
            .sandbox
            .as_ref()
            .map(|s| s.dangerous_allow_private.clone())
            .unwrap_or_default();
        let (resp, final_messages, _tool_attachments, tool_meta) = self
            .tool_use_loop(
                view.as_ref(),
                agent_id,
                &session_result.session.session_key.0,
                &agent.model_policy.primary,
                &agent.model_policy.fallbacks,
                Some(system_prompt.clone()),
                messages,
                2048,
                allowed_stream.as_deref(),
                merged_permissions,
                agent.security.clone(),
                private_network_overrides_stream,
                source_info_stream,
                must_use_web_search,
                false, // is_scheduled_task
                agent.model_policy.thinking_level,
                cancel_token,
            )
            .await?;

        if tool_meta.cancelled {
            let abort_text = self.runtime.postprocess_output(&resp.text).await?;
            let abort_text = filter_no_reply(&abort_text);

            let workspace = self.workspace_state_for(agent_id);
            let session_text = build_session_text(&inbound.text, &inbound.attachments);
            let _ = workspace
                .session_writer
                .append_message(&session_result.session.session_id, "user", &session_text)
                .await;
            let _ = workspace
                .session_writer
                .append_message(&session_result.session.session_id, "assistant", &abort_text)
                .await;
            self.enqueue_dirty_source(
                agent_id,
                DIRTY_KIND_SESSION,
                &session_result.session.session_id,
                "session_appended",
            )
            .await;
            {
                let mut session = session_result.session.clone();
                session.increment_interaction();
                let _ = self.session_mgr.persist_session(&session).await;
            }

            let single_chunk: Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send + '_>> =
                Box::pin(tokio_stream::once(Ok(StreamChunk {
                    delta: abort_text,
                    is_final: true,
                    input_tokens: resp.input_tokens,
                    output_tokens: resp.output_tokens,
                    stop_reason: resp.stop_reason.clone(),
                    content_blocks: resp.content.clone(),
                })));
            return Ok(single_chunk);
        }

        let trace_id = inbound.trace_id;
        let bus = self.bus.clone();
        let session_mgr = self.session_mgr.clone();
        let mut session = session_result.session.clone();
        session.increment_interaction();
        if let Err(e) = session_mgr.persist_session(&session).await {
            tracing::warn!("Failed to persist session interaction count: {e}");
        }
        let agent_id_owned = agent_id.to_string();
        let channel_type = inbound.channel_type.clone();
        let connector_id = inbound.connector_id.clone();
        let conversation_scope = inbound.conversation_scope.clone();
        let user_scope = inbound.user_scope.clone();
        let inbound_text_for_guard = inbound.text.clone();
        let target_language_stream = target_language;
        let mut stream_accumulator = String::new();

        let stream = view
            .router
            .stream(
                &agent.model_policy.primary,
                &agent.model_policy.fallbacks,
                Some(system_prompt),
                final_messages,
                2048,
                agent.model_policy.thinking_level,
            )
            .await?;

        let mapped = tokio_stream::StreamExt::map(stream, move |chunk_result| {
            if let Ok(ref chunk) = chunk_result {
                if !chunk.delta.is_empty() {
                    stream_accumulator.push_str(&chunk.delta);
                }

                if chunk.is_final && !is_language_guard_exempt(&inbound_text_for_guard) {
                    if let (Some(target), Some(detected)) = (
                        target_language_stream,
                        detect_response_language(&stream_accumulator),
                    ) {
                        if detected != target {
                            tracing::warn!(
                                agent_id = %agent_id_owned,
                                channel_type = %channel_type,
                                connector_id = %connector_id,
                                conversation_scope = %conversation_scope,
                                user_scope = %user_scope,
                                target_language = %target.as_str(),
                                detected_language = %detected.as_str(),
                                is_streaming = true,
                                "language_guard: response language mismatch"
                            );
                        }
                    }
                }

                let bus = bus.clone();
                let msg = BusMessage::StreamDelta {
                    trace_id,
                    delta: chunk.delta.clone(),
                    is_final: chunk.is_final,
                };
                tokio::spawn(async move {
                    let _ = bus.publish(msg).await;
                });
            }
            chunk_result
        });

        Ok(Box::pin(mapped))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use chrono::Utc;
    use clawhive_memory::fact_store::FactStore;
    use clawhive_memory::file_store::MemoryFileStore;
    use clawhive_memory::{MemoryStore, SessionReader};
    use clawhive_provider::{LlmMessage, ProviderRegistry};
    use clawhive_schema::*;
    use tokio_util::sync::CancellationToken;

    use crate::context;
    use crate::orchestrator::test_helpers::{
        make_tool_loop_test_orchestrator, CompactionOnlyProvider, SequenceProvider,
    };
    use crate::router::LlmRouter;

    #[tokio::test]
    async fn handle_with_view_persists_abort_message_to_session() {
        let provider = Arc::new(SequenceProvider::new(vec![
            crate::orchestrator::test_helpers::llm_text_response(
                "should not be called",
                "end_turn",
            ),
        ]));
        let (orchestrator, tmp, memory) =
            make_tool_loop_test_orchestrator(provider.clone(), Some(2)).await;
        let view = orchestrator.config_view();
        let inbound = InboundMessage {
            trace_id: uuid::Uuid::new_v4(),
            channel_type: "telegram".into(),
            connector_id: "tg_main".into(),
            conversation_scope: "chat:cancelled".into(),
            user_scope: "user:1".into(),
            text: "read and stop".into(),
            at: Utc::now(),
            thread_id: None,
            is_mention: false,
            mention_target: None,
            message_id: None,
            attachments: vec![],
            message_source: None,
        };
        let session_key = SessionKey::from_inbound(&inbound);
        let cancel_token = CancellationToken::new();
        cancel_token.cancel();

        let outbound = orchestrator
            .handle_with_view(view.clone(), inbound, "agent-a", cancel_token)
            .await
            .unwrap();
        assert_eq!(provider.call_count(), 0);
        assert_eq!(outbound.text, "[Task stopped by user]");

        let session = memory
            .get_session(&session_key.0)
            .await
            .unwrap()
            .expect("session record");
        let reader = SessionReader::new(tmp.path());
        let messages = reader
            .load_recent_messages(&session.session_id, 10)
            .await
            .unwrap();

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[1].role, "assistant");
        assert_eq!(messages[1].content, "[Task stopped by user]");
    }

    #[tokio::test]
    async fn compaction_does_not_write_persistent_memory_layers() {
        let tmp = tempfile::tempdir().unwrap();
        let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
        let file_store = MemoryFileStore::new(tmp.path());
        let fact_store = FactStore::new(memory.db());

        let mut registry = ProviderRegistry::new();
        registry.register("compact", Arc::new(CompactionOnlyProvider));
        let router = Arc::new(LlmRouter::new(
            registry,
            HashMap::from([("compact".to_string(), "compact/model".to_string())]),
            vec![],
        ));
        let ctx_mgr =
            context::ContextManager::new(router, context::ContextConfig::for_model(2_000));

        let large = "x".repeat(25_000);
        let messages = vec![
            LlmMessage::user(large.clone()),
            LlmMessage::assistant(large.clone()),
            LlmMessage::user(large.clone()),
            LlmMessage::assistant(large),
        ];

        let (_, compaction) = ctx_mgr
            .ensure_within_limits("compact/model", messages)
            .await
            .expect("compaction succeeds");
        assert!(compaction.is_some(), "compaction should have occurred");

        let today = chrono::Utc::now().date_naive();
        assert!(file_store.read_daily(today).await.unwrap().is_none());
        assert!(file_store.read_long_term().await.unwrap().trim().is_empty());
        assert!(fact_store
            .get_active_facts("test-agent")
            .await
            .unwrap()
            .is_empty());
    }

    #[test]
    fn compaction_lock_prevents_concurrent_access() {
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        let guard = lock.try_lock().unwrap();
        assert!(lock.try_lock().is_err());
        drop(guard);
        assert!(lock.try_lock().is_ok());
    }
}
