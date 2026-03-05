use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use clawhive_bus::BusPublisher;
use clawhive_memory::embedding::EmbeddingProvider;
use clawhive_memory::file_store::MemoryFileStore;
use clawhive_memory::search_index::SearchIndex;
use clawhive_memory::{MemoryStore, SessionMessage};
use clawhive_memory::{SessionReader, SessionWriter};
use clawhive_provider::{ContentBlock, LlmMessage, LlmRequest, StreamChunk};
use clawhive_runtime::TaskExecutor;
use clawhive_schema::*;
use futures_core::Stream;

use super::access_gate::{AccessGate, GrantAccessTool, ListAccessTool, RevokeAccessTool};
use super::approval::ApprovalRegistry;
use super::config::{ExecSecurityConfig, FullAgentConfig, SandboxPolicyConfig, SecurityMode};
use super::file_tools::{EditFileTool, ReadFileTool, WriteFileTool};
use super::image_tool::ImageTool;
use super::memory_tools::{MemoryGetTool, MemorySearchTool};
use super::persona::Persona;
use super::router::LlmRouter;
use super::schedule_tool::ScheduleTool;
use super::session::SessionManager;
use super::shell_tool::ExecuteCommandTool;
use super::skill::SkillRegistry;
use super::skill_install_state::SkillInstallState;
use super::tool::{ConversationMessage, ToolContext, ToolExecutor, ToolRegistry};
use super::web_fetch_tool::WebFetchTool;
use super::web_search_tool::WebSearchTool;
use super::workspace::Workspace;

const SKILL_INSTALL_USAGE_HINT: &str = "请提供 skill 来源路径或 URL。用法: /skill install <source>";

/// Per-agent workspace runtime state: file store, session I/O, search index.
struct AgentWorkspaceState {
    workspace: Workspace,
    file_store: MemoryFileStore,
    session_writer: SessionWriter,
    session_reader: SessionReader,
    search_index: SearchIndex,
    access_gate: Arc<AccessGate>,
}

pub struct Orchestrator {
    router: Arc<LlmRouter>,
    agents: HashMap<String, FullAgentConfig>,
    personas: HashMap<String, Persona>,
    session_mgr: SessionManager,
    session_locks: super::session_lock::SessionLockManager,
    context_manager: super::context::ContextManager,
    hook_registry: super::hooks::HookRegistry,
    skill_registry: SkillRegistry,
    skills_root: std::path::PathBuf,
    #[allow(dead_code)]
    memory: Arc<MemoryStore>,
    bus: BusPublisher,
    approval_registry: Option<Arc<ApprovalRegistry>>,
    runtime: Arc<dyn TaskExecutor>,
    #[allow(dead_code)]
    workspace_root: std::path::PathBuf,
    /// Per-agent workspace state, keyed by agent_id
    agent_workspaces: HashMap<String, AgentWorkspaceState>,
    /// Fallback for agents without a dedicated workspace
    file_store: MemoryFileStore,
    session_writer: SessionWriter,
    session_reader: SessionReader,
    search_index: SearchIndex,
    embedding_provider: Arc<dyn EmbeddingProvider>,
    tool_registry: ToolRegistry,
    default_workspace_root: std::path::PathBuf,
    default_access_gate: Arc<AccessGate>,
    skill_install_state: Arc<SkillInstallState>,
}

impl Orchestrator {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        router: LlmRouter,
        agents: Vec<FullAgentConfig>,
        personas: HashMap<String, Persona>,
        session_mgr: SessionManager,
        skill_registry: SkillRegistry,
        memory: Arc<MemoryStore>,
        bus: BusPublisher,
        approval_registry: Option<Arc<ApprovalRegistry>>,
        runtime: Arc<dyn TaskExecutor>,
        file_store: MemoryFileStore,
        session_writer: SessionWriter,
        session_reader: SessionReader,
        search_index: SearchIndex,
        embedding_provider: Arc<dyn EmbeddingProvider>,
        workspace_root: std::path::PathBuf,
        brave_api_key: Option<String>,
        project_root: Option<std::path::PathBuf>,
        schedule_manager: Arc<clawhive_scheduler::ScheduleManager>,
    ) -> Self {
        let router = Arc::new(router);
        let bus_for_tools = bus.clone();
        let agents_map: HashMap<String, FullAgentConfig> = agents
            .into_iter()
            .map(|a| (a.agent_id.clone(), a))
            .collect();
        let personas_for_subagent = personas.clone();

        // Build per-agent workspace states
        let effective_project_root = project_root.unwrap_or_else(|| workspace_root.clone());
        let mut agent_workspaces = HashMap::new();
        for (agent_id, agent_cfg) in &agents_map {
            let ws = Workspace::resolve(
                &effective_project_root,
                agent_id,
                agent_cfg.workspace.as_deref(),
            );
            let ws_root = ws.root().to_path_buf();
            let gate = Arc::new(AccessGate::new(ws_root.clone(), ws.access_policy_path()));
            let state = AgentWorkspaceState {
                workspace: ws,
                file_store: MemoryFileStore::new(&ws_root),
                session_writer: SessionWriter::new(&ws_root),
                session_reader: SessionReader::new(&ws_root),
                search_index: SearchIndex::new(memory.db()),
                access_gate: gate,
            };
            agent_workspaces.insert(agent_id.clone(), state);
        }

        let mut tool_registry = ToolRegistry::new();
        tool_registry.register(Box::new(MemorySearchTool::new(
            search_index.clone(),
            embedding_provider.clone(),
        )));
        tool_registry.register(Box::new(MemoryGetTool::new(file_store.clone())));
        let sub_agent_runner = Arc::new(super::subagent::SubAgentRunner::new(
            router.clone(),
            agents_map.clone(),
            personas_for_subagent,
            3,
            vec![],
        ));
        tool_registry.register(Box::new(super::subagent_tool::SubAgentTool::new(
            sub_agent_runner,
            30,
        )));
        // Default access gate for the global tool registry
        let default_access_gate = Arc::new(AccessGate::new(
            effective_project_root.clone(),
            effective_project_root.join("access_policy.json"),
        ));
        // File tools (read/write/edit) are registered here for their DEFINITIONS only,
        // so the LLM knows they exist. Actual execution is dispatched per-agent in
        // execute_tool_for_agent() with the correct workspace root.
        tool_registry.register(Box::new(ReadFileTool::new(
            workspace_root.clone(),
            default_access_gate.clone(),
        )));
        tool_registry.register(Box::new(WriteFileTool::new(
            workspace_root.clone(),
            default_access_gate.clone(),
        )));
        tool_registry.register(Box::new(EditFileTool::new(
            workspace_root.clone(),
            default_access_gate.clone(),
        )));
        tool_registry.register(Box::new(ExecuteCommandTool::new(
            workspace_root.clone(),
            30,
            default_access_gate.clone(),
            ExecSecurityConfig::default(),
            SandboxPolicyConfig::default(),
            approval_registry.clone(),
            Some(bus_for_tools.clone()),
            "global".to_string(),
        )));
        // Access control tools
        tool_registry.register(Box::new(GrantAccessTool::new(default_access_gate.clone())));
        tool_registry.register(Box::new(ListAccessTool::new(default_access_gate.clone())));
        tool_registry.register(Box::new(RevokeAccessTool::new(default_access_gate.clone())));
        tool_registry.register(Box::new(WebFetchTool::new()));
        tool_registry.register(Box::new(ImageTool::new()));
        tool_registry.register(Box::new(ScheduleTool::new(schedule_manager)));
        tool_registry.register(Box::new(crate::skill_tool::SkillTool::new(
            workspace_root.join("skills"),
        )));
        tool_registry.register(Box::new(crate::message_tool::MessageTool::new(
            bus_for_tools.clone(),
        )));
        if let Some(api_key) = brave_api_key {
            if !api_key.is_empty() {
                tool_registry.register(Box::new(WebSearchTool::new(api_key)));
            }
        }

        Self {
            router: router.clone(),
            agents: agents_map,
            personas,
            session_mgr,
            session_locks: super::session_lock::SessionLockManager::with_global_limit(10),
            context_manager: super::context::ContextManager::new(
                router.clone(),
                super::context::ContextConfig::default(),
            ),
            hook_registry: super::hooks::HookRegistry::new(),
            skills_root: workspace_root.join("skills"),
            skill_registry,
            memory,
            bus,
            approval_registry,
            runtime,
            workspace_root,
            agent_workspaces,
            file_store,
            session_writer,
            session_reader,
            search_index,
            embedding_provider,
            tool_registry,
            default_workspace_root: effective_project_root,
            default_access_gate,
            skill_install_state: Arc::new(SkillInstallState::new(900)),
        }
    }

    async fn handle_skill_analyze_or_install_command(
        &self,
        inbound: InboundMessage,
        source: String,
        install_requested: bool,
    ) -> Result<OutboundMessage> {
        let resolved = super::skill_install::resolve_skill_source(&source).await?;
        let report = super::skill_install::analyze_skill_source(resolved.local_path())?;
        let token = self
            .skill_install_state
            .create_pending(
                source,
                report.clone(),
                inbound.user_scope.clone(),
                inbound.conversation_scope.clone(),
            )
            .await;

        let mode_text = if install_requested {
            "Install request analyzed."
        } else {
            "Analyze complete."
        };
        let analysis = super::skill_install::render_skill_analysis(&report);
        let text = format!("{mode_text}\n\n{analysis}\n\nTo continue, run: /skill confirm {token}");

        // Publish bus message so Discord/Telegram can render confirm buttons
        let _ = self
            .bus
            .publish(clawhive_schema::BusMessage::DeliverSkillConfirm {
                channel_type: inbound.channel_type.clone(),
                connector_id: inbound.connector_id.clone(),
                conversation_scope: inbound.conversation_scope.clone(),
                token: token.clone(),
                skill_name: report.skill_name.clone(),
                analysis_text: analysis,
            })
            .await;

        Ok(OutboundMessage {
            trace_id: inbound.trace_id,
            channel_type: inbound.channel_type,
            connector_id: inbound.connector_id,
            conversation_scope: inbound.conversation_scope,
            text,
            at: chrono::Utc::now(),
            reply_to: None,
            attachments: vec![],
        })
    }
    async fn handle_skill_confirm_command(
        &self,
        inbound: InboundMessage,
        agent_id: &str,
        token: String,
    ) -> Result<OutboundMessage> {
        if !self
            .skill_install_state
            .is_scope_allowed(&inbound.user_scope)
        {
            return Ok(OutboundMessage {
                trace_id: inbound.trace_id,
                channel_type: inbound.channel_type,
                connector_id: inbound.connector_id,
                conversation_scope: inbound.conversation_scope,
                text: "You are not authorized to install skills in this environment.".to_string(),
                at: chrono::Utc::now(),
                reply_to: None,
                attachments: vec![],
            });
        }

        let Some(pending) = self.skill_install_state.take_if_valid(&token).await else {
            return Ok(OutboundMessage {
                trace_id: inbound.trace_id,
                channel_type: inbound.channel_type,
                connector_id: inbound.connector_id,
                conversation_scope: inbound.conversation_scope,
                text: "Invalid or expired skill install confirmation token.".to_string(),
                at: chrono::Utc::now(),
                reply_to: None,
                attachments: vec![],
            });
        };

        if pending.user_scope != inbound.user_scope
            || pending.conversation_scope != inbound.conversation_scope
        {
            return Ok(OutboundMessage {
                trace_id: inbound.trace_id,
                channel_type: inbound.channel_type,
                connector_id: inbound.connector_id,
                conversation_scope: inbound.conversation_scope,
                text: "This token belongs to a different user or conversation.".to_string(),
                at: chrono::Utc::now(),
                reply_to: None,
                attachments: vec![],
            });
        }

        let super::skill_install_state::PendingSkillInstall {
            source,
            report,
            user_scope: _,
            conversation_scope: _,
            created_at: _,
        } = pending;

        if super::skill_install::has_high_risk_findings(&report) {
            let Some(registry) = self.approval_registry.as_ref() else {
                return Ok(OutboundMessage {
                    trace_id: inbound.trace_id,
                    channel_type: inbound.channel_type,
                    connector_id: inbound.connector_id,
                    conversation_scope: inbound.conversation_scope,
                    text:
                        "High-risk skill install requires approval but no approval UI is available."
                            .to_string(),
                    at: chrono::Utc::now(),
                    reply_to: None,
                    attachments: vec![],
                });
            };

            let command = format!("skill install {}", report.skill_name);
            let trace_id = uuid::Uuid::new_v4();
            let rx = registry
                .request(trace_id, command.clone(), agent_id.to_string())
                .await;

            let _ = self
                .bus
                .publish(BusMessage::NeedHumanApproval {
                    trace_id,
                    reason: format!(
                        "High-risk skill install requires approval: {}",
                        report.skill_name
                    ),
                    agent_id: agent_id.to_string(),
                    command,
                    network_target: None,
                    source_channel_type: Some(inbound.channel_type.clone()),
                    source_connector_id: Some(inbound.connector_id.clone()),
                    source_conversation_scope: Some(inbound.conversation_scope.clone()),
                })
                .await;

            match rx.await {
                Ok(ApprovalDecision::AllowOnce) | Ok(ApprovalDecision::AlwaysAllow) => {}
                Ok(ApprovalDecision::Deny) | Err(_) => {
                    return Ok(OutboundMessage {
                        trace_id: inbound.trace_id,
                        channel_type: inbound.channel_type,
                        connector_id: inbound.connector_id,
                        conversation_scope: inbound.conversation_scope,
                        text: "Skill install denied by user.".to_string(),
                        at: chrono::Utc::now(),
                        reply_to: None,
                        attachments: vec![],
                    });
                }
            }
        }

        let resolved = super::skill_install::resolve_skill_source(&source).await?;
        let installed = super::skill_install::install_skill_from_analysis(
            &self.workspace_root,
            &self.skills_root,
            resolved.local_path(),
            &report,
            true,
        )?;

        let text = format!(
            "Installed skill '{}' to {} (findings: {}, high-risk: {}).",
            report.skill_name,
            installed.target.display(),
            report.findings.len(),
            installed.high_risk
        );

        Ok(OutboundMessage {
            trace_id: inbound.trace_id,
            channel_type: inbound.channel_type,
            connector_id: inbound.connector_id,
            conversation_scope: inbound.conversation_scope,
            text,
            at: chrono::Utc::now(),
            reply_to: None,
            attachments: vec![],
        })
    }

    fn workspace_root_for(&self, agent_id: &str) -> std::path::PathBuf {
        self.agent_workspaces
            .get(agent_id)
            .map(|ws| ws.workspace.root().to_path_buf())
            .unwrap_or_else(|| self.default_workspace_root.clone())
    }

    fn access_gate_for(&self, agent_id: &str) -> Arc<AccessGate> {
        self.agent_workspaces
            .get(agent_id)
            .map(|ws| ws.access_gate.clone())
            .unwrap_or_else(|| self.default_access_gate.clone())
    }

    fn active_skill_registry(&self) -> SkillRegistry {
        SkillRegistry::load_from_dir(&self.skills_root).unwrap_or_else(|e| {
            tracing::warn!(
                "Failed to reload skills from {}: {e}",
                self.skills_root.display()
            );
            self.skill_registry.clone()
        })
    }

    fn forced_skill_names(input: &str) -> Option<Vec<String>> {
        let trimmed = input.trim();
        let rest = trimmed.strip_prefix("/skill ")?;
        let names_part = rest.split_whitespace().next()?.trim();
        if names_part.is_empty() {
            return None;
        }

        let names: Vec<String> = names_part
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();

        if names.is_empty() {
            None
        } else {
            Some(names)
        }
    }

    fn merge_permissions(
        perms: impl IntoIterator<Item = corral_core::Permissions>,
    ) -> Option<corral_core::Permissions> {
        let mut list: Vec<corral_core::Permissions> = perms.into_iter().collect();
        if list.is_empty() {
            return None;
        }

        let mut merged = corral_core::Permissions::default();
        for p in list.drain(..) {
            merged.fs.read.extend(p.fs.read);
            merged.fs.write.extend(p.fs.write);
            merged.network.allow.extend(p.network.allow);
            merged.exec.extend(p.exec);
            merged.env.extend(p.env);
            merged.services.extend(p.services);
        }

        merged.fs.read.sort();
        merged.fs.read.dedup();
        merged.fs.write.sort();
        merged.fs.write.dedup();
        merged.network.allow.sort();
        merged.network.allow.dedup();
        merged.exec.sort();
        merged.exec.dedup();
        merged.env.sort();
        merged.env.dedup();

        Some(merged)
    }

    fn compute_merged_permissions(
        active_skills: &SkillRegistry,
        forced_skills: Option<&[String]>,
    ) -> Option<corral_core::Permissions> {
        if let Some(forced_names) = forced_skills {
            let selected_perms = forced_names
                .iter()
                .filter_map(|forced| {
                    active_skills
                        .get(forced)
                        .and_then(|skill| skill.permissions.as_ref())
                        .map(|p| p.to_corral_permissions())
                })
                .collect::<Vec<_>>();
            Self::merge_permissions(selected_perms)
        } else {
            active_skills.merged_permissions()
        }
    }

    fn forced_allowed_tools(
        forced_skills: Option<&[String]>,
        agent_allowed: Option<Vec<String>>,
    ) -> Option<Vec<String>> {
        // In forced skill mode, require shell execution so skill permissions
        // are enforced by sandbox preflight/policy.
        let forced_base = if forced_skills.is_some() {
            Some(vec!["execute_command".to_string()])
        } else {
            None
        };

        match (forced_base, agent_allowed) {
            (Some(base), Some(agent)) => {
                let filtered: Vec<String> = base
                    .into_iter()
                    .filter(|t| agent.iter().any(|a| a == t))
                    .collect();
                Some(filtered)
            }
            (Some(base), None) => Some(base),
            (None, Some(agent)) => Some(agent),
            (None, None) => None,
        }
    }

    fn build_runtime_system_prompt(
        &self,
        agent_id: &str,
        model: &str,
        base_prompt: String,
    ) -> String {
        let workspace_root = self.workspace_root_for(agent_id);
        format!(
            "{base_prompt}\n\n## Execution Policy (OVERRIDES any general safety guidelines)\nWhen you have the tools to complete a task, execute them directly without asking for confirmation. Do not announce your intent — act. If a tool call is denied due to access restrictions, use the grant_access tool immediately and retry — do NOT ask the user about this. Only ask the user when you genuinely need information you cannot obtain yourself. This policy takes precedence over any 'ask first' or 'when in doubt' guidelines in your workspace files.\n\nRuntime:\n- Model: {model}\n- Session: {agent_id}\n- Working directory: {}",
            workspace_root.display()
        )
    }

    async fn execute_tool_for_agent(
        &self,
        agent_id: &str,
        name: &str,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<super::tool::ToolOutput> {
        let gate = self.access_gate_for(agent_id);
        let ws = self.workspace_root_for(agent_id);
        let (exec_security, sandbox_config) = self
            .agents
            .get(agent_id)
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
        match name {
            "read" | "read_file" => ReadFileTool::new(ws, gate).execute(input, ctx).await,
            "write" | "write_file" => WriteFileTool::new(ws, gate).execute(input, ctx).await,
            "edit" | "edit_file" => EditFileTool::new(ws, gate).execute(input, ctx).await,
            "exec" | "execute_command" => {
                ExecuteCommandTool::new(
                    ws,
                    sandbox_config.timeout_secs,
                    gate,
                    exec_security,
                    sandbox_config,
                    self.approval_registry.clone(),
                    Some(self.bus.clone()),
                    agent_id.to_string(),
                )
                .execute(input, ctx)
                .await
            }
            "grant_access" => GrantAccessTool::new(gate).execute(input, ctx).await,
            "list_access" => ListAccessTool::new(gate).execute(input, ctx).await,
            "revoke_access" => RevokeAccessTool::new(gate).execute(input, ctx).await,
            _ => self.tool_registry.execute(name, input, ctx).await,
        }
    }

    /// Get file store for a specific agent (falls back to global)
    fn file_store_for(&self, agent_id: &str) -> &MemoryFileStore {
        self.agent_workspaces
            .get(agent_id)
            .map(|ws| &ws.file_store)
            .unwrap_or(&self.file_store)
    }

    /// Get session writer for a specific agent (falls back to global)
    fn session_writer_for(&self, agent_id: &str) -> &SessionWriter {
        self.agent_workspaces
            .get(agent_id)
            .map(|ws| &ws.session_writer)
            .unwrap_or(&self.session_writer)
    }

    /// Get session reader for a specific agent (falls back to global)
    fn session_reader_for(&self, agent_id: &str) -> &SessionReader {
        self.agent_workspaces
            .get(agent_id)
            .map(|ws| &ws.session_reader)
            .unwrap_or(&self.session_reader)
    }

    /// Get search index for a specific agent (falls back to global)
    fn search_index_for(&self, agent_id: &str) -> &SearchIndex {
        self.agent_workspaces
            .get(agent_id)
            .map(|ws| &ws.search_index)
            .unwrap_or(&self.search_index)
    }

    /// Ensure workspace directories exist for all agents
    pub async fn ensure_workspaces(&self) -> Result<()> {
        for state in self.agent_workspaces.values() {
            state.workspace.ensure_dirs().await?;
        }
        Ok(())
    }

    /// Get a reference to the hook registry for registering hooks.
    pub fn hook_registry(&self) -> &super::hooks::HookRegistry {
        &self.hook_registry
    }

    pub async fn handle_inbound(
        &self,
        inbound: InboundMessage,
        agent_id: &str,
    ) -> Result<OutboundMessage> {
        let agent = self
            .agents
            .get(agent_id)
            .ok_or_else(|| anyhow!("agent not found: {agent_id}"))?;

        let session_key = SessionKey::from_inbound(&inbound);

        // Acquire per-session lock to prevent concurrent modifications
        let _session_guard = self.session_locks.acquire(&session_key.0).await;

        // Handle slash commands before LLM
        if let Some(cmd) = super::slash_commands::parse_command(&inbound.text) {
            match cmd {
                super::slash_commands::SlashCommand::Model => {
                    return Ok(OutboundMessage {
                        trace_id: inbound.trace_id,
                        channel_type: inbound.channel_type,
                        connector_id: inbound.connector_id,
                        conversation_scope: inbound.conversation_scope,
                        text: format!(
                            "Model: **{}**\nSession: **{}**",
                            agent.model_policy.primary, session_key.0
                        ),
                        at: chrono::Utc::now(),
                        reply_to: None,
                        attachments: vec![],
                    });
                }
                super::slash_commands::SlashCommand::Status => {
                    return Ok(OutboundMessage {
                        trace_id: inbound.trace_id,
                        channel_type: inbound.channel_type,
                        connector_id: inbound.connector_id,
                        conversation_scope: inbound.conversation_scope,
                        text: super::slash_commands::format_status_response(
                            agent_id,
                            &agent.model_policy.primary,
                            &session_key.0,
                        ),
                        at: chrono::Utc::now(),
                        reply_to: None,
                        attachments: vec![],
                    });
                }
                super::slash_commands::SlashCommand::SkillAnalyze { source } => {
                    return self
                        .handle_skill_analyze_or_install_command(inbound, source, false)
                        .await;
                }
                super::slash_commands::SlashCommand::SkillInstall { source } => {
                    return self
                        .handle_skill_analyze_or_install_command(inbound, source, true)
                        .await;
                }
                super::slash_commands::SlashCommand::SkillConfirm { token } => {
                    return self
                        .handle_skill_confirm_command(inbound, agent_id, token)
                        .await;
                }
                super::slash_commands::SlashCommand::SkillUsageHint { subcommand } => {
                    let hint = match subcommand.as_str() {
                        "analyze" => "Usage: /skill analyze <url-or-path>\nExample: /skill analyze https://example.com/my-skill.zip",
                        "install" => "Usage: /skill install <url-or-path>\nExample: /skill install https://example.com/my-skill.zip",
                        "confirm" => "Usage: /skill confirm <token>\nThe token is provided after running /skill analyze or /skill install.",
                        _ => "Usage:\n  /skill analyze <source> — Analyze a skill before installing\n  /skill install <source> — Install a skill\n  /skill confirm <token> — Confirm a pending installation",
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
                super::slash_commands::SlashCommand::New { model_hint } => {
                    // Reset the session: clear history and start fresh
                    let _ = self.session_mgr.reset(&session_key).await;
                    let _ = self
                        .session_writer_for(agent_id)
                        .clear_session(&session_key.0)
                        .await;

                    // Build post-reset prompt
                    let post_reset_prompt =
                        super::slash_commands::build_post_reset_prompt(agent_id);

                    // Log the model hint if provided (for future model switching)
                    if let Some(ref hint) = model_hint {
                        tracing::info!("Session reset with model hint: {hint}");
                    }

                    // Continue with normal flow but inject the post-reset prompt
                    return self
                        .handle_post_reset_flow(
                            inbound,
                            agent_id,
                            agent,
                            &session_key,
                            &post_reset_prompt,
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
            .get_or_create(&session_key, agent_id)
            .await?;

        if session_result.expired_previous {
            self.try_fallback_summary(agent_id, &session_key, agent)
                .await;
            // Clear stale JSONL so expired sessions start fresh
            let _ = self
                .session_writer_for(agent_id)
                .clear_session(&session_key.0)
                .await;
        }

        let inbound_text = inbound.text.clone();

        let system_prompt = self
            .personas
            .get(agent_id)
            .map(|p| {
                // Inject group members context if available
                if let Some(ref group_ctx) = inbound.group_context {
                    let group_md = format_group_context_md(group_ctx, &self.personas);
                    if !group_md.is_empty() {
                        let mut persona_clone = p.clone();
                        persona_clone.group_members_context = group_md;
                        return persona_clone.assembled_system_prompt();
                    }
                }
                p.assembled_system_prompt()
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
            Self::compute_merged_permissions(&active_skills, None)
        };

        let memory_context = self
            .build_memory_context(agent_id, &session_key, &inbound.text)
            .await?;

        // Build system prompt with memory context injected (not fake dialogue)
        let system_prompt = if memory_context.is_empty() {
            self.build_runtime_system_prompt(agent_id, &agent.model_policy.primary, system_prompt)
        } else {
            let base_prompt = self.build_runtime_system_prompt(
                agent_id,
                &agent.model_policy.primary,
                system_prompt,
            );
            format!("{base_prompt}\n\n## Relevant Memory\n{memory_context}")
        };

        let history_messages = match self
            .session_reader_for(agent_id)
            .load_recent_messages(&session_key.0, 10)
            .await
        {
            Ok(msgs) => msgs,
            Err(e) => {
                tracing::warn!("Failed to load session history: {e}");
                Vec::new()
            }
        };

        // Build messages from history (no fake memory dialogue)
        let mut messages = build_messages_from_history(&history_messages);
        {
            let preprocessed = self.runtime.preprocess_input(&inbound.text).await?;
            let image_blocks: Vec<ContentBlock> = inbound
                .attachments
                .iter()
                .filter(|a| a.kind == clawhive_schema::AttachmentKind::Image)
                .map(|a| {
                    let media_type = a
                        .mime_type
                        .clone()
                        .unwrap_or_else(|| "image/jpeg".to_string());
                    ContentBlock::Image {
                        data: a.url.clone(),
                        media_type,
                    }
                })
                .collect();

            if image_blocks.is_empty() {
                messages.push(LlmMessage::user(preprocessed));
            } else {
                let mut content = vec![ContentBlock::Text { text: preprocessed }];
                content.extend(image_blocks);
                messages.push(LlmMessage {
                    role: "user".into(),
                    content,
                });
            }
        }

        let allowed = Self::forced_allowed_tools(
            forced_skills.as_deref(),
            agent.tool_policy.as_ref().map(|tp| tp.allow.clone()),
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
        let (resp, _messages) = self
            .tool_use_loop(
                agent_id,
                &agent.model_policy.primary,
                &agent.model_policy.fallbacks,
                Some(system_prompt),
                messages,
                2048,
                allowed.as_deref(),
                merged_permissions,
                agent.security.clone(),
                private_network_overrides,
                source_info,
            )
            .await?;
        let reply_text = self.runtime.postprocess_output(&resp.text).await?;

        // Check for NO_REPLY suppression
        let reply_text = filter_no_reply(&reply_text);

        if reply_text.is_empty() {
            tracing::warn!(
                raw_text_len = resp.text.len(),
                raw_text_preview = &resp.text[..resp.text.len().min(200)],
                stop_reason = ?resp.stop_reason,
                content_blocks = resp.content.len(),
                "handle_inbound: final reply is empty"
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
            attachments: vec![],
        };

        // Record session messages (JSONL)
        if let Err(e) = self
            .session_writer_for(agent_id)
            .append_message(&session_key.0, "user", &inbound_text)
            .await
        {
            tracing::warn!("Failed to write user session entry: {e}");
        }
        if let Err(e) = self
            .session_writer_for(agent_id)
            .append_message(&session_key.0, "assistant", &outbound.text)
            .await
        {
            tracing::warn!("Failed to write assistant session entry: {e}");
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
        let agent = self
            .agents
            .get(agent_id)
            .ok_or_else(|| anyhow!("agent not found: {agent_id}"))?;

        let session_key = SessionKey::from_inbound(&inbound);

        // Acquire per-session lock to prevent concurrent modifications
        let _session_guard = self.session_locks.acquire(&session_key.0).await;

        let session_result = self
            .session_mgr
            .get_or_create(&session_key, agent_id)
            .await?;

        if session_result.expired_previous {
            self.try_fallback_summary(agent_id, &session_key, agent)
                .await;
            // Clear stale JSONL so expired sessions start fresh
            let _ = self
                .session_writer_for(agent_id)
                .clear_session(&session_key.0)
                .await;
        }

        let system_prompt = self
            .personas
            .get(agent_id)
            .map(|p| p.assembled_system_prompt())
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
            Self::compute_merged_permissions(&active_skills, None)
        };

        let memory_context = self
            .build_memory_context(agent_id, &session_key, &inbound.text)
            .await?;

        // Build system prompt with memory context injected (stream variant)
        let system_prompt = if memory_context.is_empty() {
            self.build_runtime_system_prompt(agent_id, &agent.model_policy.primary, system_prompt)
        } else {
            let base_prompt = self.build_runtime_system_prompt(
                agent_id,
                &agent.model_policy.primary,
                system_prompt,
            );
            format!("{base_prompt}\n\n## Relevant Memory\n{memory_context}")
        };

        let history_messages = match self
            .session_reader_for(agent_id)
            .load_recent_messages(&session_key.0, 10)
            .await
        {
            Ok(msgs) => msgs,
            Err(e) => {
                tracing::warn!("Failed to load session history: {e}");
                Vec::new()
            }
        };

        // Build messages from history (no fake memory dialogue, stream variant)
        let mut messages = build_messages_from_history(&history_messages);
        {
            let preprocessed = self.runtime.preprocess_input(&inbound.text).await?;
            let image_blocks: Vec<ContentBlock> = inbound
                .attachments
                .iter()
                .filter(|a| a.kind == clawhive_schema::AttachmentKind::Image)
                .map(|a| {
                    let media_type = a
                        .mime_type
                        .clone()
                        .unwrap_or_else(|| "image/jpeg".to_string());
                    ContentBlock::Image {
                        data: a.url.clone(),
                        media_type,
                    }
                })
                .collect();

            if image_blocks.is_empty() {
                messages.push(LlmMessage::user(preprocessed));
            } else {
                let mut content = vec![ContentBlock::Text { text: preprocessed }];
                content.extend(image_blocks);
                messages.push(LlmMessage {
                    role: "user".into(),
                    content,
                });
            }
        }

        let allowed_stream = Self::forced_allowed_tools(
            forced_skills.as_deref(),
            agent.tool_policy.as_ref().map(|tp| tp.allow.clone()),
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
        let (_resp, final_messages) = self
            .tool_use_loop(
                agent_id,
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
            )
            .await?;

        let trace_id = inbound.trace_id;
        let bus = self.bus.clone();

        let stream = self
            .router
            .stream(
                &agent.model_policy.primary,
                &agent.model_policy.fallbacks,
                Some(system_prompt),
                final_messages,
                2048,
            )
            .await?;

        let mapped = tokio_stream::StreamExt::map(stream, move |chunk_result| {
            if let Ok(ref chunk) = chunk_result {
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

    /// Runs the tool-use loop: sends messages to the LLM, executes any
    /// requested tools, appends tool results, and repeats until the LLM
    /// produces a final (non-tool-use) response.
    ///
    /// Returns both the final LLM response **and** the accumulated messages
    /// (including all intermediate assistant/tool_result turns). Callers that
    /// need the full conversation context (e.g. `handle_inbound_stream`)
    /// should use the returned messages instead of the original input.
    #[allow(clippy::too_many_arguments)]
    async fn tool_use_loop(
        &self,
        agent_id: &str,
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
    ) -> Result<(clawhive_provider::LlmResponse, Vec<LlmMessage>)> {
        let mut messages = initial_messages;
        let tool_defs: Vec<_> = match allowed_tools {
            Some(allow_list) => self
                .tool_registry
                .tool_defs()
                .into_iter()
                .filter(|t| allow_list.iter().any(|a| t.name.starts_with(a)))
                .collect(),
            None => self.tool_registry.tool_defs(),
        };
        let max_iterations = 10;
        let mut continuation_injected = false;

        for _iteration in 0..max_iterations {
            // Check if we need to compact context
            let (compacted_messages, compaction_result) = self
                .context_manager
                .ensure_within_limits(primary, messages)
                .await?;
            messages = compacted_messages;

            if let Some(ref result) = compaction_result {
                tracing::info!(
                    "Auto-compacted {} messages, saved {} tokens",
                    result.compacted_count,
                    result.tokens_saved
                );
            }

            let req = LlmRequest {
                model: primary.into(),
                system: system.clone(),
                messages: messages.clone(),
                max_tokens,
                tools: tool_defs.clone(),
            };

            let resp = self.router.chat_with_tools(primary, fallbacks, req).await?;

            tracing::debug!(
                text_len = resp.text.len(),
                content_blocks = resp.content.len(),
                stop_reason = ?resp.stop_reason,
                input_tokens = ?resp.input_tokens,
                output_tokens = ?resp.output_tokens,
                "tool_use_loop: LLM response"
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
                // Continuation detection: if the LLM just acknowledged the user's
                // confirmation without calling any tools, nudge it to continue.
                // This prevents the "收到" + stop pattern.
                if !continuation_injected
                    && tool_uses.is_empty()
                    && Self::should_inject_continuation(&messages, &resp.text)
                {
                    continuation_injected = true;
                    tracing::info!(
                        text_len = resp.text.len(),
                        "detected acknowledgment-without-action, injecting continuation"
                    );
                    messages.push(LlmMessage {
                        role: "assistant".into(),
                        content: resp.content.clone(),
                    });
                    messages.push(LlmMessage::user(
                        "OK, go ahead. Proceed with the next step.",
                    ));
                    continue;
                }
                return Ok((resp, messages));
            }

            messages.push(LlmMessage {
                role: "assistant".into(),
                content: resp.content.clone(),
            });

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
            .with_recent_messages(recent_messages);
            let ctx = if let Some((ref ch, ref co, ref cv, ref us)) = source_info {
                ctx.with_source(ch.clone(), co.clone(), cv.clone())
                    .with_source_user_scope(us.clone())
            } else {
                ctx
            };

            // Execute tools in parallel
            let tool_futures: Vec<_> = tool_uses
                .into_iter()
                .map(|(id, name, input)| {
                    let ctx = ctx.clone();
                    let agent_id = agent_id.to_string();
                    async move {
                        match self
                            .execute_tool_for_agent(&agent_id, &name, input, &ctx)
                            .await
                        {
                            Ok(output) => ContentBlock::ToolResult {
                                tool_use_id: id,
                                content: output.content,
                                is_error: output.is_error,
                            },
                            Err(e) => ContentBlock::ToolResult {
                                tool_use_id: id,
                                content: format!("Tool execution error: {e}"),
                                is_error: true,
                            },
                        }
                    }
                })
                .collect();

            let tool_results = futures::future::join_all(tool_futures).await;

            messages.push(LlmMessage {
                role: "user".into(),
                content: tool_results,
            });
            continuation_injected = false;
        }

        // Loop exhausted — ask the LLM for a final answer without tools
        // so the user gets a response instead of an opaque error.
        tracing::warn!("tool_use_loop exhausted {max_iterations} iterations, requesting final answer without tools");
        let final_req = LlmRequest {
            model: primary.into(),
            system: system.clone(),
            messages: messages.clone(),
            max_tokens,
            tools: vec![],
        };
        let resp = self
            .router
            .chat_with_tools(primary, fallbacks, final_req)
            .await?;
        Ok((resp, messages))
    }

    /// Detect if the LLM just acknowledged the user's response without acting.
    /// Returns true when:
    /// 1. The current response is short (< 200 chars) — a real answer is longer
    /// 2. The previous assistant message was asking/confirming something
    /// 3. There's a recent user message (the confirmation) right before this response
    fn should_inject_continuation(messages: &[LlmMessage], response_text: &str) -> bool {
        // Short response only — anything substantial is likely a real final answer
        if response_text.chars().count() > 200 {
            return false;
        }

        // Need at least 2 messages: [..., assistant (question), user (confirmation)]
        if messages.len() < 2 {
            return false;
        }

        // Find the last assistant message
        let last_assistant = messages.iter().rev().find(|m| m.role == "assistant");
        let Some(assistant_msg) = last_assistant else {
            return false;
        };

        // Extract assistant text
        let assistant_text: String = assistant_msg
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");

        // Was the assistant asking a question or requesting confirmation?
        let question_signals = [
            '?', '\u{ff1f}', // ? and ？
        ];
        let confirmation_keywords = [
            // Chinese
            "确认",
            "选择",
            "请",
            "建议",
            "哪",
            "还是",
            "同意",
            "如果你",
            "是否",
            "要不要",
            "可以吗",
            "你觉得",
            "你看",
            "要我",
            "需要你",
            // English
            "choose",
            "confirm",
            "select",
            "which",
            "option",
            "prefer",
            "should",
            "want me to",
            "would you",
            "if you",
            "do you",
            "shall i",
            "let me know",
        ];

        let has_question_mark = question_signals.iter().any(|&c| assistant_text.contains(c));
        let has_confirmation_keyword = confirmation_keywords
            .iter()
            .any(|kw| assistant_text.to_lowercase().contains(kw));

        if !has_question_mark && !has_confirmation_keyword {
            return false;
        }

        // Check the response looks like an acknowledgment (short, no real content)
        let ack_patterns = [
            "收到",
            "好的",
            "明白",
            "了解",
            "OK",
            "ok",
            "Got it",
            "已",
            "没问题",
            "可以",
            "Understood",
            "Sure",
        ];
        let looks_like_ack = ack_patterns.iter().any(|p| response_text.contains(p))
            || response_text.chars().count() < 80;

        looks_like_ack
    }

    /// Handle the flow after a /reset or /new command.
    /// This creates a fresh session and injects the post-reset prompt to guide the agent.
    async fn handle_post_reset_flow(
        &self,
        inbound: InboundMessage,
        agent_id: &str,
        agent: &FullAgentConfig,
        session_key: &SessionKey,
        post_reset_prompt: &str,
    ) -> Result<OutboundMessage> {
        // Create a fresh session
        let _ = self
            .session_mgr
            .get_or_create(session_key, agent_id)
            .await?;

        // Build system prompt with post-reset context
        let system_prompt = self
            .personas
            .get(agent_id)
            .map(|p| p.assembled_system_prompt())
            .unwrap_or_default();
        let active_skills = self.active_skill_registry();
        let skill_summary = active_skills.summary_prompt();
        let system_prompt = if skill_summary.is_empty() {
            system_prompt
        } else {
            format!("{system_prompt}\n\n{skill_summary}")
        };
        let system_prompt =
            self.build_runtime_system_prompt(agent_id, &agent.model_policy.primary, system_prompt);

        // Build messages with post-reset prompt
        let messages = vec![LlmMessage::user(post_reset_prompt.to_string())];

        let source_info = Some((
            inbound.channel_type.clone(),
            inbound.connector_id.clone(),
            inbound.conversation_scope.clone(),
            inbound.user_scope.clone(),
        ));

        // Run the tool-use loop
        let (resp, _messages) = self
            .tool_use_loop(
                agent_id,
                &agent.model_policy.primary,
                &agent.model_policy.fallbacks,
                Some(system_prompt),
                messages,
                2048,
                agent.tool_policy.as_ref().map(|tp| tp.allow.as_slice()),
                None,
                agent.security.clone(),
                agent
                    .sandbox
                    .as_ref()
                    .map(|s| s.dangerous_allow_private.clone())
                    .unwrap_or_default(),
                source_info,
            )
            .await?;

        let reply_text = self.runtime.postprocess_output(&resp.text).await?;
        let reply_text = filter_no_reply(&reply_text);

        // Record the assistant's response in the fresh session
        if let Err(e) = self
            .session_writer_for(agent_id)
            .append_message(&session_key.0, "system", post_reset_prompt)
            .await
        {
            tracing::warn!("Failed to write post-reset prompt to session: {e}");
        }
        if let Err(e) = self
            .session_writer_for(agent_id)
            .append_message(&session_key.0, "assistant", &reply_text)
            .await
        {
            tracing::warn!("Failed to write assistant session entry: {e}");
        }

        let outbound = OutboundMessage {
            trace_id: inbound.trace_id,
            channel_type: inbound.channel_type,
            connector_id: inbound.connector_id,
            conversation_scope: inbound.conversation_scope,
            text: reply_text,
            at: chrono::Utc::now(),
            reply_to: None,
            attachments: vec![],
        };

        let _ = self
            .bus
            .publish(BusMessage::ReplyReady {
                outbound: outbound.clone(),
            })
            .await;

        Ok(outbound)
    }

    async fn try_fallback_summary(
        &self,
        agent_id: &str,
        session_key: &SessionKey,
        agent: &FullAgentConfig,
    ) {
        let messages = match self
            .session_reader_for(agent_id)
            .load_recent_messages(&session_key.0, 20)
            .await
        {
            Ok(msgs) if !msgs.is_empty() => msgs,
            _ => return,
        };

        let today = chrono::Utc::now().date_naive();
        if let Ok(Some(_)) = self.file_store_for(agent_id).read_daily(today).await {
            return;
        }

        let conversation = messages
            .iter()
            .map(|m| format!("{}: {}", m.role, m.content))
            .collect::<Vec<_>>()
            .join("\n");

        let system = "Summarize this conversation in 2-4 bullet points. \
            Focus on key facts, decisions, and user preferences. \
            Output Markdown bullet points only, no preamble."
            .to_string();

        let llm_messages = vec![LlmMessage::user(conversation)];

        match self
            .router
            .chat(
                &agent.model_policy.primary,
                &agent.model_policy.fallbacks,
                Some(system),
                llm_messages,
                512,
            )
            .await
        {
            Ok(resp) => {
                if let Err(e) = self
                    .file_store_for(agent_id)
                    .append_daily(today, &resp.text)
                    .await
                {
                    tracing::warn!("Failed to write fallback summary: {e}");
                } else {
                    tracing::info!("Wrote fallback summary for expired session");
                }
            }
            Err(e) => {
                tracing::warn!("Failed to generate fallback summary: {e}");
            }
        }
    }

    async fn build_memory_context(
        &self,
        agent_id: &str,
        _session_key: &SessionKey,
        query: &str,
    ) -> Result<String> {
        let results = self
            .search_index_for(agent_id)
            .search(query, self.embedding_provider.as_ref(), 6, 0.25)
            .await;

        match results {
            Ok(results) if !results.is_empty() => {
                let mut context = String::from("## Relevant Memory\n\n");
                for result in &results {
                    context.push_str(&format!(
                        "### {} (score: {:.2})\n{}\n\n",
                        result.path, result.score, result.text
                    ));
                }
                Ok(context)
            }
            _ => self.file_store_for(agent_id).build_memory_context().await,
        }
    }
}

fn build_messages_from_history(history_messages: &[SessionMessage]) -> Vec<LlmMessage> {
    let mut messages = Vec::new();
    let mut prev_timestamp = None;

    for hist_msg in history_messages {
        if let (Some(prev_ts), Some(curr_ts)) = (prev_timestamp, hist_msg.timestamp) {
            let gap: chrono::TimeDelta = curr_ts - prev_ts;
            if gap.num_minutes() >= 30 {
                let gap_text = format_time_gap(gap);
                messages.push(LlmMessage {
                    role: "user".to_string(),
                    content: vec![ContentBlock::Text {
                        text: format!(
                            "[{gap_text} of inactivity has passed since the last message]"
                        ),
                    }],
                });
            }
        }

        prev_timestamp = hist_msg.timestamp;

        messages.push(LlmMessage {
            role: hist_msg.role.clone(),
            content: vec![ContentBlock::Text {
                text: hist_msg.content.clone(),
            }],
        });
    }

    messages
}

fn format_time_gap(gap: chrono::TimeDelta) -> String {
    let hours = gap.num_hours();
    let minutes = gap.num_minutes();
    if hours >= 24 {
        let days = hours / 24;
        format!("{days} day(s)")
    } else if hours >= 1 {
        format!("{hours} hour(s)")
    } else {
        format!("{minutes} minute(s)")
    }
}

fn extract_source_after_prefix(text: &str, prefix: &str) -> Option<String> {
    let rest = text[prefix.len()..]
        .trim_start_matches([' ', ':', '\u{ff1a}'])
        .trim();
    if rest.is_empty() {
        None
    } else {
        Some(rest.to_string())
    }
}

fn has_install_skill_intent_prefix(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }

    let lower = trimmed.to_ascii_lowercase();
    let en_prefixes = ["install skill from", "install this skill", "install skill"];
    if en_prefixes.iter().any(|prefix| lower.starts_with(prefix)) {
        return true;
    }

    let cn_prefixes = [
        "安装这个skill:",
        "安装这个 skill:",
        "安装skill:",
        "安装 skill:",
        "安装技能:",
        "安装这个skill",
        "安装这个 skill",
        "安装skill",
        "安装 skill",
        "安装技能",
    ];
    cn_prefixes.iter().any(|prefix| trimmed.starts_with(prefix))
}

fn is_skill_install_intent_without_source(text: &str) -> bool {
    if !has_install_skill_intent_prefix(text) {
        return false;
    }
    detect_skill_install_intent(text).is_none()
}

pub fn detect_skill_install_intent(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    let lower = trimmed.to_ascii_lowercase();
    let en_prefixes = ["install skill from", "install this skill", "install skill"];
    for prefix in en_prefixes {
        if lower.starts_with(prefix) {
            return extract_source_after_prefix(trimmed, prefix);
        }
    }

    let cn_prefixes = [
        "安装这个skill:",
        "安装这个 skill:",
        "安装skill:",
        "安装 skill:",
        "安装技能:",
        "安装这个skill",
        "安装这个 skill",
        "安装skill",
        "安装 skill",
        "安装技能",
    ];
    for prefix in cn_prefixes {
        if trimmed.starts_with(prefix) {
            return extract_source_after_prefix(trimmed, prefix);
        }
    }

    None
}

/// Filter NO_REPLY responses.
/// Returns empty string if the response is just "NO_REPLY" (with optional whitespace).
/// Also strips leading/trailing "NO_REPLY" from responses.
fn filter_no_reply(text: &str) -> String {
    let trimmed = text.trim();

    // Exact match
    if trimmed == "NO_REPLY" || trimmed == "HEARTBEAT_OK" {
        return String::new();
    }

    // Strip from beginning or end
    let text = trimmed
        .strip_prefix("NO_REPLY")
        .unwrap_or(trimmed)
        .strip_suffix("NO_REPLY")
        .unwrap_or(trimmed)
        .trim();

    // Also handle HEARTBEAT_OK
    let text = text
        .strip_prefix("HEARTBEAT_OK")
        .unwrap_or(text)
        .strip_suffix("HEARTBEAT_OK")
        .unwrap_or(text)
        .trim();

    text.to_string()
}

fn collect_recent_messages(messages: &[LlmMessage], limit: usize) -> Vec<ConversationMessage> {
    let mut collected = Vec::new();

    for message in messages.iter().rev() {
        let mut parts = Vec::new();
        for block in &message.content {
            if let ContentBlock::Text { text } = block {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    parts.push(trimmed.to_string());
                }
            }
        }

        if !parts.is_empty() {
            collected.push(ConversationMessage {
                role: message.role.clone(),
                content: parts.join("\n"),
            });
            if collected.len() >= limit {
                break;
            }
        }
    }

    collected.reverse();
    collected
}

/// Format group context as markdown for injection into system prompt.
fn format_group_context_md(
    group_ctx: &GroupContext,
    personas: &HashMap<String, Persona>,
) -> String {
    if !group_ctx.is_group || group_ctx.members.is_empty() {
        return String::new();
    }

    let mut lines = vec!["## 当前群聊成员 / Current Group Members".to_string()];
    lines.push(String::new());

    // Separate agents and humans
    let mut agents_in_group = Vec::new();
    let mut humans_in_group = Vec::new();

    for member in &group_ctx.members {
        if member.is_bot {
            // Try to find matching persona by name
            let agent_id = personas
                .iter()
                .find(|(_, p)| p.name == member.name)
                .map(|(id, _)| id.clone());
            agents_in_group.push((member, agent_id));
        } else {
            humans_in_group.push(member);
        }
    }

    if !agents_in_group.is_empty() {
        lines.push("**Agents in this chat:**".to_string());
        for (member, agent_id) in &agents_in_group {
            let id_info = agent_id
                .as_ref()
                .map(|id| format!(" (agent: {})", id))
                .unwrap_or_default();
            lines.push(format!("- 🤖 {}{}", member.name, id_info));
        }
        lines.push(String::new());
    }

    if !humans_in_group.is_empty() {
        lines.push("**Humans in this chat:**".to_string());
        for member in &humans_in_group {
            lines.push(format!("- 👤 {}", member.name));
        }
        lines.push(String::new());
    }

    // List agents NOT in this chat (from known personas)
    let agents_in_chat: std::collections::HashSet<_> = agents_in_group
        .iter()
        .filter_map(|(_, id)| id.as_ref())
        .collect();
    let agents_not_in_chat: Vec<_> = personas
        .iter()
        .filter(|(id, _)| !agents_in_chat.contains(id))
        .collect();

    if !agents_not_in_chat.is_empty() {
        lines.push("**Other agents (not in this chat, @ to collaborate):**".to_string());
        for (agent_id, persona) in agents_not_in_chat {
            let emoji = persona.emoji.as_deref().unwrap_or("🤖");
            lines.push(format!("- {} {} ({})", emoji, persona.name, agent_id));
        }
    }

    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone, Utc};
    use clawhive_memory::SessionMessage;

    #[test]
    fn merged_permissions_in_normal_mode_use_all_active_skills() {
        let dir = tempfile::tempdir().unwrap();

        let skill_a = dir.path().join("skill-a");
        std::fs::create_dir_all(&skill_a).unwrap();
        std::fs::write(
            skill_a.join("SKILL.md"),
            r#"---
name: skill-a
description: A
permissions:
  network:
    allow: ["api.a.com:443"]
---
Body"#,
        )
        .unwrap();

        let skill_b = dir.path().join("skill-b");
        std::fs::create_dir_all(&skill_b).unwrap();
        std::fs::write(
            skill_b.join("SKILL.md"),
            r#"---
name: skill-b
description: B
permissions:
  network:
    allow: ["api.b.com:443"]
---
Body"#,
        )
        .unwrap();

        let active_skills = SkillRegistry::load_from_dir(dir.path()).unwrap();
        let merged = Orchestrator::compute_merged_permissions(&active_skills, None);

        let perms = merged.expect("expected merged permissions in normal mode");
        assert!(perms.network.allow.contains(&"api.a.com:443".to_string()));
        assert!(perms.network.allow.contains(&"api.b.com:443".to_string()));
    }

    #[test]
    fn format_time_gap_prefers_days_hours_minutes() {
        assert_eq!(format_time_gap(Duration::minutes(45)), "45 minute(s)");
        assert_eq!(format_time_gap(Duration::hours(3)), "3 hour(s)");
        assert_eq!(format_time_gap(Duration::hours(49)), "2 day(s)");
    }

    #[test]
    fn build_history_messages_inserts_inactivity_markers() {
        let history = vec![
            SessionMessage {
                role: "user".to_string(),
                content: "first".to_string(),
                timestamp: Some(Utc.with_ymd_and_hms(2026, 1, 1, 10, 0, 0).unwrap()),
            },
            SessionMessage {
                role: "assistant".to_string(),
                content: "second".to_string(),
                timestamp: Some(Utc.with_ymd_and_hms(2026, 1, 1, 10, 40, 0).unwrap()),
            },
            SessionMessage {
                role: "user".to_string(),
                content: "third".to_string(),
                timestamp: Some(Utc.with_ymd_and_hms(2026, 1, 1, 10, 50, 0).unwrap()),
            },
        ];

        let messages = build_messages_from_history(&history);

        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[1].role, "user");
        assert_eq!(
            messages[1].content,
            vec![ContentBlock::Text {
                text: "[40 minute(s) of inactivity has passed since the last message]".to_string()
            }]
        );
        assert_eq!(messages[2].role, "assistant");
        assert_eq!(messages[3].role, "user");
    }

    #[test]
    fn should_inject_continuation_detects_acknowledgment_after_question() {
        // Setup: assistant asks a question, user confirms with short ack
        let messages = vec![
            LlmMessage {
                role: "assistant".into(),
                content: vec![ContentBlock::Text {
                    text: "Do you want me to proceed?".into(),
                }],
            },
            LlmMessage {
                role: "user".into(),
                content: vec![ContentBlock::Text { text: "yes".into() }],
            },
        ];

        // LLM responds with short acknowledgment
        let response_text = "OK";

        // Should detect this as acknowledgment-without-action
        assert!(Orchestrator::should_inject_continuation(
            &messages,
            response_text
        ));
    }

    #[test]
    fn should_inject_continuation_ignores_long_responses() {
        let messages = vec![
            LlmMessage {
                role: "assistant".into(),
                content: vec![ContentBlock::Text {
                    text: "Do you want me to proceed?".into(),
                }],
            },
            LlmMessage {
                role: "user".into(),
                content: vec![ContentBlock::Text { text: "yes".into() }],
            },
        ];

        // Long response (> 200 chars) should not trigger injection
        let response_text = "I understand. Let me proceed with the installation. First, I'll download the package, then configure it, and finally run the setup script. This process typically takes a few minutes depending on your system speed.";

        assert!(!Orchestrator::should_inject_continuation(
            &messages,
            response_text
        ));
    }

    #[test]
    fn should_inject_continuation_requires_question_or_confirmation() {
        let messages = vec![
            LlmMessage {
                role: "assistant".into(),
                content: vec![ContentBlock::Text {
                    text: "Here is the result.".into(),
                }],
            },
            LlmMessage {
                role: "user".into(),
                content: vec![ContentBlock::Text {
                    text: "thanks".into(),
                }],
            },
        ];

        // No question/confirmation in assistant message, should not inject
        let response_text = "OK";

        assert!(!Orchestrator::should_inject_continuation(
            &messages,
            response_text
        ));
    }

    #[test]
    fn continuation_injection_scenario_multi_step() {
        // Scenario: Simulate a multi-step task where continuation_injected should reset
        // This test verifies that should_inject_continuation can fire multiple times
        // when there are multiple question-acknowledgment cycles

        // Round 1: Assistant asks a question
        let mut messages = vec![
            LlmMessage {
                role: "user".into(),
                content: vec![ContentBlock::Text {
                    text: "Install three packages".into(),
                }],
            },
            LlmMessage {
                role: "assistant".into(),
                content: vec![ContentBlock::Text {
                    text: "Should I install package A first?".into(),
                }],
            },
        ];

        // User confirms
        messages.push(LlmMessage {
            role: "user".into(),
            content: vec![ContentBlock::Text { text: "yes".into() }],
        });

        // LLM responds with short ack (should trigger injection)
        let response_1 = "OK";
        assert!(Orchestrator::should_inject_continuation(
            &messages, response_1
        ));

        // After injection, assistant message is added
        messages.push(LlmMessage {
            role: "assistant".into(),
            content: vec![ContentBlock::Text {
                text: response_1.into(),
            }],
        });

        // Continuation prompt is added
        messages.push(LlmMessage {
            role: "user".into(),
            content: vec![ContentBlock::Text {
                text: "OK, go ahead. Proceed with the next step.".into(),
            }],
        });

        // LLM responds with tool use (simulated)
        messages.push(LlmMessage {
            role: "assistant".into(),
            content: vec![ContentBlock::Text {
                text: "Installing package A...".into(),
            }],
        });

        // Tool results are pushed
        messages.push(LlmMessage {
            role: "user".into(),
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tool_1".into(),
                content: "Package A installed".into(),
                is_error: false,
            }],
        });

        // Round 2: Assistant asks another question
        messages.push(LlmMessage {
            role: "assistant".into(),
            content: vec![ContentBlock::Text {
                text: "Now should I install package B?".into(),
            }],
        });

        // User confirms again
        messages.push(LlmMessage {
            role: "user".into(),
            content: vec![ContentBlock::Text { text: "yes".into() }],
        });

        // LLM responds with short ack again
        // This should ALSO trigger injection because continuation_injected was reset
        let response_2 = "OK";
        assert!(Orchestrator::should_inject_continuation(
            &messages, response_2
        ));
    }

    #[test]
    fn continuation_injection_no_infinite_loop() {
        // Scenario: Verify that continuation injection doesn't fire infinitely
        // when LLM keeps responding with short acks without tool calls

        let mut messages = vec![
            LlmMessage {
                role: "user".into(),
                content: vec![ContentBlock::Text {
                    text: "Do something".into(),
                }],
            },
            LlmMessage {
                role: "assistant".into(),
                content: vec![ContentBlock::Text {
                    text: "Should I proceed?".into(),
                }],
            },
        ];

        // User confirms
        messages.push(LlmMessage {
            role: "user".into(),
            content: vec![ContentBlock::Text { text: "yes".into() }],
        });

        // First short response - should trigger injection
        let response_1 = "OK";
        assert!(Orchestrator::should_inject_continuation(
            &messages, response_1
        ));

        // After injection, assistant message and continuation prompt are added
        messages.push(LlmMessage {
            role: "assistant".into(),
            content: vec![ContentBlock::Text {
                text: response_1.into(),
            }],
        });

        messages.push(LlmMessage {
            role: "user".into(),
            content: vec![ContentBlock::Text {
                text: "OK, go ahead. Proceed with the next step.".into(),
            }],
        });

        // Second short response (no tool call, no tool results)
        // This should NOT trigger injection again because:
        // 1. The continuation_injected flag should have been reset after tool execution
        // 2. But in this case, there was NO tool execution
        // So the logic is: injection fires once per round (after tool execution)
        // If there's no tool execution, the loop should exit
        let response_2 = "OK";
        // This response should NOT trigger injection because:
        // - The previous assistant message (response_1) was not asking a question
        // - It was just an acknowledgment
        assert!(!Orchestrator::should_inject_continuation(
            &messages, response_2
        ));
    }
}
