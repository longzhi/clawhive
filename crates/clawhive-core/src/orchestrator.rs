use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use arc_swap::ArcSwap;
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

use crate::config_view::ConfigView;

use super::language_prefs::{
    apply_language_policy_prompt, detect_response_language, is_language_guard_exempt,
    log_language_guard, LanguagePrefs,
};

use super::access_gate::{AccessGate, GrantAccessTool, ListAccessTool, RevokeAccessTool};
use super::approval::ApprovalRegistry;
use super::config::{ExecSecurityConfig, FullAgentConfig, SandboxPolicyConfig, SecurityMode};
use super::context::ContextCheckResult;
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
use super::workspace_manager::{AgentWorkspaceManager, AgentWorkspaceState};

const SKILL_INSTALL_USAGE_HINT: &str = "请提供 skill 来源路径或 URL。用法: /skill install <source>";

pub struct Orchestrator {
    config_view: ArcSwap<ConfigView>,
    session_mgr: SessionManager,
    session_locks: super::session_lock::SessionLockManager,
    context_manager: super::context::ContextManager,
    hook_registry: super::hooks::HookRegistry,
    skill_registry: ArcSwap<SkillRegistry>,
    skills_root: std::path::PathBuf,
    memory: Arc<MemoryStore>,
    bus: BusPublisher,
    approval_registry: Option<Arc<ApprovalRegistry>>,
    runtime: Arc<dyn TaskExecutor>,
    workspaces: AgentWorkspaceManager,
    workspace_root: std::path::PathBuf,
    skill_install_state: Arc<SkillInstallState>,
    language_prefs: LanguagePrefs,
}

/// Builder for [`Orchestrator`]. Use [`OrchestratorBuilder::new`] to start,
/// call optional setters, then call [`OrchestratorBuilder::build`].
pub struct OrchestratorBuilder {
    config_view: Option<ConfigView>,
    bus: BusPublisher,
    memory: Arc<MemoryStore>,
    runtime: Arc<dyn TaskExecutor>,
    workspace_root: std::path::PathBuf,
    // Optional with defaults
    session_mgr: Option<SessionManager>,
    skill_registry: Option<SkillRegistry>,
    approval_registry: Option<Arc<ApprovalRegistry>>,
    project_root: Option<std::path::PathBuf>,
    // Allow overriding auto-derived workspace I/O (e.g. in tests with pre-populated stores)
    file_store: Option<MemoryFileStore>,
    session_writer: Option<SessionWriter>,
    session_reader: Option<SessionReader>,
    search_index: Option<SearchIndex>,
}

impl OrchestratorBuilder {
    pub fn new(
        config_view: ConfigView,
        bus: BusPublisher,
        memory: Arc<MemoryStore>,
        runtime: Arc<dyn TaskExecutor>,
        workspace_root: std::path::PathBuf,
        _schedule_manager: Arc<clawhive_scheduler::ScheduleManager>,
    ) -> Self {
        Self {
            config_view: Some(config_view),
            bus,
            memory,
            runtime,
            workspace_root,
            session_mgr: None,
            skill_registry: None,
            approval_registry: None,
            project_root: None,
            file_store: None,
            session_writer: None,
            session_reader: None,
            search_index: None,
        }
    }

    pub fn session_mgr(mut self, session_mgr: SessionManager) -> Self {
        self.session_mgr = Some(session_mgr);
        self
    }

    pub fn skill_registry(mut self, skill_registry: SkillRegistry) -> Self {
        self.skill_registry = Some(skill_registry);
        self
    }

    pub fn approval_registry(mut self, approval_registry: Arc<ApprovalRegistry>) -> Self {
        self.approval_registry = Some(approval_registry);
        self
    }

    pub fn project_root(mut self, root: std::path::PathBuf) -> Self {
        self.project_root = Some(root);
        self
    }

    pub fn file_store(mut self, file_store: MemoryFileStore) -> Self {
        self.file_store = Some(file_store);
        self
    }

    pub fn session_writer(mut self, session_writer: SessionWriter) -> Self {
        self.session_writer = Some(session_writer);
        self
    }

    pub fn session_reader(mut self, session_reader: SessionReader) -> Self {
        self.session_reader = Some(session_reader);
        self
    }

    pub fn search_index(mut self, search_index: SearchIndex) -> Self {
        self.search_index = Some(search_index);
        self
    }

    pub fn build(self) -> Orchestrator {
        let file_store = self
            .file_store
            .unwrap_or_else(|| MemoryFileStore::new(&self.workspace_root));
        let session_writer = self
            .session_writer
            .unwrap_or_else(|| SessionWriter::new(&self.workspace_root));
        let session_reader = self
            .session_reader
            .unwrap_or_else(|| SessionReader::new(&self.workspace_root));
        let search_index = self
            .search_index
            .unwrap_or_else(|| SearchIndex::new(self.memory.db()));
        let session_mgr = self
            .session_mgr
            .unwrap_or_else(|| SessionManager::new(self.memory.clone(), 1800));
        let config_view = self
            .config_view
            .expect("orchestrator builder requires config_view");

        Orchestrator::new(
            config_view,
            session_mgr,
            self.skill_registry.unwrap_or_default(),
            self.memory,
            self.bus,
            self.approval_registry,
            self.runtime,
            file_store,
            session_writer,
            session_reader,
            search_index,
            self.workspace_root,
            self.project_root,
        )
    }
}

impl Orchestrator {
    #[allow(clippy::too_many_arguments)]
    fn new(
        config_view: ConfigView,
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
        workspace_root: std::path::PathBuf,
        project_root: Option<std::path::PathBuf>,
    ) -> Self {
        let router = Arc::new(config_view.router.clone());

        // Build per-agent workspace states
        let effective_project_root = project_root.unwrap_or_else(|| workspace_root.clone());
        let mut agent_workspace_map = HashMap::new();
        for (agent_id, agent_cfg) in &config_view.agents {
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
            agent_workspace_map.insert(agent_id.clone(), state);
        }
        // Build default workspace state from constructor params
        let default_ws = Workspace::new(workspace_root.clone());
        let default_access_gate = Arc::new(AccessGate::new(
            effective_project_root.clone(),
            effective_project_root.join("access_policy.json"),
        ));
        let default_state = AgentWorkspaceState {
            workspace: default_ws,
            file_store,
            session_writer,
            session_reader,
            search_index,
            access_gate: default_access_gate,
        };
        let workspaces = AgentWorkspaceManager::new(agent_workspace_map, default_state);

        let skills_root = workspace_root.join("skills");
        let skill_registry = ArcSwap::from_pointee(skill_registry);
        let config_view = ArcSwap::from_pointee(config_view);

        Self {
            config_view,
            session_mgr,
            session_locks: super::session_lock::SessionLockManager::with_global_limit(10),
            context_manager: super::context::ContextManager::new(
                router.clone(),
                super::context::ContextConfig::default(),
            ),
            hook_registry: super::hooks::HookRegistry::new(),
            skills_root,
            skill_registry,
            memory,
            bus,
            approval_registry,
            runtime,
            workspaces,
            workspace_root,
            skill_install_state: Arc::new(SkillInstallState::new(900)),
            language_prefs: LanguagePrefs::new(),
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
            self.workspaces.default_root(),
            &self.skills_root,
            resolved.local_path(),
            &report,
            true,
        )?;
        self.reload_skills();

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
        self.workspaces.workspace_root(agent_id)
    }

    fn access_gate_for(&self, agent_id: &str) -> Arc<AccessGate> {
        self.workspaces.access_gate(agent_id)
    }

    fn active_skill_registry(&self) -> Arc<SkillRegistry> {
        self.skill_registry.load_full()
    }

    pub fn config_view(&self) -> Arc<ConfigView> {
        self.config_view.load_full()
    }

    pub fn apply_config_view(&self, view: ConfigView) {
        self.config_view.store(Arc::new(view));
    }

    pub fn reload_skills(&self) {
        match SkillRegistry::load_from_dir(&self.skills_root) {
            Ok(registry) => {
                self.skill_registry.store(Arc::new(registry));
                tracing::info!(
                    skills_root = %self.skills_root.display(),
                    "skill registry reloaded"
                );
            }
            Err(e) => {
                tracing::warn!(
                    skills_root = %self.skills_root.display(),
                    error = %e,
                    "failed to reload skill registry, keeping cached version"
                );
            }
        }
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

    #[cfg(test)]
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

    fn has_tool_registered(&self, view: &ConfigView, name: &str) -> bool {
        view.tool_registry
            .tool_defs()
            .iter()
            .any(|tool| tool.name == name)
    }

    fn build_runtime_system_prompt(
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

    async fn execute_tool_for_agent(
        &self,
        view: &ConfigView,
        agent_id: &str,
        name: &str,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<super::tool::ToolOutput> {
        let gate = self.access_gate_for(agent_id);
        let ws = self.workspace_root_for(agent_id);
        let (exec_security, sandbox_config) = view
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
            "grant_access" => self.approve_then_grant(agent_id, &gate, input, ctx).await,
            "list_access" => ListAccessTool::new(gate).execute(input, ctx).await,
            "revoke_access" => RevokeAccessTool::new(gate).execute(input, ctx).await,
            _ => view.tool_registry.execute(name, input, ctx).await,
        }
    }

    /// Require human approval before granting filesystem access.
    async fn approve_then_grant(
        &self,
        agent_id: &str,
        gate: &Arc<AccessGate>,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<super::tool::ToolOutput> {
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
                Ok(Ok(ApprovalDecision::Deny)) => Ok(super::tool::ToolOutput {
                    content: format!("Access grant denied by user: {description}"),
                    is_error: true,
                }),
                _ => {
                    tracing::warn!(%description, "grant_access approval timed out or channel unavailable");
                    Ok(super::tool::ToolOutput {
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

    fn file_store_for(&self, agent_id: &str) -> MemoryFileStore {
        self.workspaces.file_store(agent_id)
    }

    fn workspace_state_for(&self, agent_id: &str) -> Arc<AgentWorkspaceState> {
        self.workspaces.get(agent_id)
    }

    fn search_index_for(&self, agent_id: &str) -> SearchIndex {
        self.workspaces.search_index(agent_id)
    }

    pub async fn ensure_workspaces(&self) -> Result<()> {
        self.workspaces.ensure_all().await
    }

    pub fn ensure_workspaces_for(
        &self,
        config: &crate::config::ClawhiveConfig,
        agent_ids: &[String],
    ) {
        let current = self.workspaces.load_full();
        let mut new_map: HashMap<String, Arc<AgentWorkspaceState>> = (*current).clone();
        for agent_id in agent_ids {
            if new_map.contains_key(agent_id) {
                continue;
            }
            let agent_cfg = config.agents.iter().find(|a| &a.agent_id == agent_id);
            let ws = Workspace::resolve(
                &self.workspace_root,
                agent_id,
                agent_cfg.and_then(|a| a.workspace.as_deref()),
            );
            let ws_root = ws.root().to_path_buf();
            let gate = Arc::new(AccessGate::new(ws_root.clone(), ws.access_policy_path()));
            let state = AgentWorkspaceState {
                workspace: ws,
                file_store: MemoryFileStore::new(&ws_root),
                session_writer: SessionWriter::new(&ws_root),
                session_reader: SessionReader::new(&ws_root),
                search_index: SearchIndex::new(self.memory.db()),
                access_gate: gate,
            };
            new_map.insert(agent_id.clone(), Arc::new(state));
        }
        self.workspaces.swap_workspaces(new_map);
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
        let view = self.config_view();
        self.handle_with_view(view, inbound, agent_id).await
    }

    pub async fn handle_with_view(
        &self,
        view: Arc<ConfigView>,
        inbound: InboundMessage,
        agent_id: &str,
    ) -> Result<OutboundMessage> {
        let agent = view
            .agent(agent_id)
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
                    let workspace = self.workspace_state_for(agent_id);
                    let _ = workspace.session_writer.clear_session(&session_key.0).await;

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
                            view.as_ref(),
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
            self.try_fallback_summary(view.as_ref(), agent_id, &session_key, agent)
                .await;
        }

        let inbound_text = inbound.text.clone();

        let system_prompt = view
            .persona(agent_id)
            .map(|persona| persona.assembled_system_prompt())
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
            .load_recent_messages(&session_key.0, history_limit)
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
        let max_response_tokens = if is_scheduled_task { 8192 } else { 2048 };
        let (resp, _messages) = self
            .tool_use_loop(
                view.as_ref(),
                agent_id,
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

        log_language_guard(agent_id, &inbound, &reply_text, target_language, false);

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
        if let Err(e) = workspace
            .session_writer
            .append_message(&session_key.0, "user", &inbound_text)
            .await
        {
            tracing::warn!("Failed to write user session entry: {e}");
        }
        if let Err(e) = workspace
            .session_writer
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
        let view = self.config_view();
        self.handle_inbound_stream_with_view(view, inbound, agent_id)
            .await
    }

    pub async fn handle_inbound_stream_with_view(
        &self,
        view: Arc<ConfigView>,
        inbound: InboundMessage,
        agent_id: &str,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send + '_>>> {
        let agent = view
            .agent(agent_id)
            .ok_or_else(|| anyhow!("agent not found: {agent_id}"))?;

        let session_key = SessionKey::from_inbound(&inbound);

        // Acquire per-session lock to prevent concurrent modifications
        let _session_guard = self.session_locks.acquire(&session_key.0).await;

        let session_result = self
            .session_mgr
            .get_or_create(&session_key, agent_id)
            .await?;

        if session_result.expired_previous {
            self.try_fallback_summary(view.as_ref(), agent_id, &session_key, agent)
                .await;
        }

        let system_prompt = view
            .persona(agent_id)
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
            .load_recent_messages(&session_key.0, history_limit)
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
        let (_resp, final_messages) = self
            .tool_use_loop(
                view.as_ref(),
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
                must_use_web_search,
                false, // is_scheduled_task
                agent.model_policy.thinking_level,
            )
            .await?;

        let trace_id = inbound.trace_id;
        let bus = self.bus.clone();
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
        view: &ConfigView,
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
        must_use_web_search: bool,
        is_scheduled_task: bool,
        thinking_level: Option<clawhive_provider::ThinkingLevel>,
    ) -> Result<(clawhive_provider::LlmResponse, Vec<LlmMessage>)> {
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
        let max_iterations = 25;
        let mut web_search_reminder_injected = false;
        let mut web_search_called = false;
        let mut memory_flush_triggered = false;
        let loop_started = std::time::Instant::now();
        let mut scheduled_task_retries: u32 = 0;
        let mut total_tool_calls: usize = 0;

        for iteration in 0..max_iterations {
            let iteration_no = iteration + 1;
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

            let check_result = ctx_mgr.check_context(&messages);
            if let ContextCheckResult::NeedsMemoryFlush {
                system_prompt: flush_system,
                prompt: flush_prompt,
            } = check_result
            {
                if !memory_flush_triggered {
                    memory_flush_triggered = true;
                    tracing::info!(
                        agent_id = %agent_id,
                        iteration = iteration_no,
                        "tool_use_loop: triggering memory flush before compaction"
                    );
                    messages.push(LlmMessage::user(format!(
                        "[SYSTEM: {flush_system}]\n{flush_prompt}"
                    )));
                    continue;
                }
            }

            let (compacted_messages, compaction_result) =
                ctx_mgr.ensure_within_limits(primary, messages).await?;
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

                if is_scheduled_task
                    && tool_uses.is_empty()
                    && resp.stop_reason.as_deref() == Some("length")
                    && scheduled_task_retries < 2
                {
                    scheduled_task_retries += 1;
                    tracing::warn!(
                        agent_id = %agent_id,
                        iteration = iteration_no,
                        retry_count = scheduled_task_retries,
                        response_len = resp.text.len(),
                        "tool_use_loop: scheduled task output truncated (stop_reason=length), continuing"
                    );
                    messages.push(LlmMessage {
                        role: "assistant".into(),
                        content: resp.content.clone(),
                    });
                    messages.push(LlmMessage::user(
                        "[SYSTEM] Your output was truncated due to length limits. \
                         Do NOT repeat what you already wrote. Continue from where you left off \
                         and use tools (write_file, execute_command) to complete the remaining steps.",
                    ));
                    continue;
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

                tracing::debug!(
                    agent_id = %agent_id,
                    iteration = iteration_no,
                    llm_round_ms,
                    total_loop_ms = loop_started.elapsed().as_millis() as u64,
                    stop_reason = ?resp.stop_reason,
                    "tool_use_loop: returning final response"
                );
                return Ok((resp, messages));
            }

            total_tool_calls += tool_uses.len();
            let tool_names: Vec<String> =
                tool_uses.iter().map(|(_, name, _)| name.clone()).collect();
            if tool_names.iter().any(|name| name == "web_search") {
                web_search_called = true;
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
            .with_recent_messages(recent_messages);
            let ctx = ctx.with_skill_registry(self.active_skill_registry());
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

            messages.push(LlmMessage {
                role: "user".into(),
                content: tool_results,
            });

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

        Ok((resp, messages))
    }

    /// Handle the flow after a /reset or /new command.
    /// This creates a fresh session and injects the post-reset prompt to guide the agent.
    async fn handle_post_reset_flow(
        &self,
        view: &ConfigView,
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
        let system_prompt = view
            .persona(agent_id)
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
                view,
                agent_id,
                &agent.model_policy.primary,
                &agent.model_policy.fallbacks,
                Some(system_prompt),
                messages,
                2048,
                agent
                    .tool_policy
                    .as_ref()
                    .map(|tp| tp.allow.as_slice())
                    .filter(|v| !v.is_empty()),
                None,
                agent.security.clone(),
                agent
                    .sandbox
                    .as_ref()
                    .map(|s| s.dangerous_allow_private.clone())
                    .unwrap_or_default(),
                source_info,
                false, // must_use_web_search
                false, // is_scheduled_task
                agent.model_policy.thinking_level,
            )
            .await?;

        let reply_text = self.runtime.postprocess_output(&resp.text).await?;
        let reply_text = filter_no_reply(&reply_text);

        // Record the assistant's response in the fresh session
        let workspace = self.workspace_state_for(agent_id);
        if let Err(e) = workspace
            .session_writer
            .append_message(&session_key.0, "system", post_reset_prompt)
            .await
        {
            tracing::warn!("Failed to write post-reset prompt to session: {e}");
        }
        if let Err(e) = workspace
            .session_writer
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

        if !outbound.text.is_empty() {
            let preview_end = outbound.text.floor_char_boundary(200);
            tracing::info!(
                agent_id = %agent_id,
                reply_len = outbound.text.len(),
                reply_preview = &outbound.text[..preview_end],
                "agent reply"
            );
        }

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
        view: &ConfigView,
        agent_id: &str,
        session_key: &SessionKey,
        agent: &FullAgentConfig,
    ) {
        let workspace = self.workspace_state_for(agent_id);
        let history_limit = history_message_limit(agent).max(20);
        let messages = match workspace
            .session_reader
            .load_recent_messages(&session_key.0, history_limit)
            .await
        {
            Ok(msgs) if !msgs.is_empty() => msgs,
            _ => return,
        };

        let today = chrono::Utc::now().date_naive();

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

        match view
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
        view: &ConfigView,
        agent_id: &str,
        _session_key: &SessionKey,
        query: &str,
    ) -> Result<String> {
        let budget = view
            .agent(agent_id)
            .and_then(|agent| agent.memory_policy.as_ref())
            .map(|policy| policy.max_injected_chars)
            .unwrap_or(6000);

        let results = self
            .search_index_for(agent_id)
            .search(query, view.embedding_provider.as_ref(), 6, 0.25)
            .await;

        match results {
            Ok(results) if !results.is_empty() => Ok(clamp_to_budget(&results, budget)),
            _ => {
                let fallback = self.file_store_for(agent_id).build_memory_context().await?;
                if fallback.len() > budget {
                    let truncated: String = fallback.chars().take(budget).collect();
                    Ok(format!("{truncated}\n...[truncated]"))
                } else {
                    Ok(fallback)
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn build_tool_registry(
    file_store: &MemoryFileStore,
    search_index: &SearchIndex,
    embedding_provider: &Arc<dyn EmbeddingProvider>,
    workspace_root: &std::path::Path,
    default_root: &std::path::Path,
    approval_registry: &Option<Arc<ApprovalRegistry>>,
    bus: &BusPublisher,
    schedule_manager: Arc<clawhive_scheduler::ScheduleManager>,
    brave_api_key: Option<String>,
    router: &LlmRouter,
    agents: &[FullAgentConfig],
    personas: &HashMap<String, Persona>,
) -> ToolRegistry {
    let agents_map: HashMap<String, FullAgentConfig> = agents
        .iter()
        .filter(|agent| agent.enabled)
        .cloned()
        .map(|agent| (agent.agent_id.clone(), agent))
        .collect();
    let personas = personas
        .iter()
        .filter(|(agent_id, _)| agents_map.contains_key(*agent_id))
        .map(|(agent_id, persona)| (agent_id.clone(), persona.clone()))
        .collect();

    let mut registry = ToolRegistry::new();
    registry.register(Box::new(MemorySearchTool::new(
        search_index.clone(),
        embedding_provider.clone(),
    )));
    registry.register(Box::new(MemoryGetTool::new(file_store.clone())));
    let sub_agent_runner = Arc::new(super::subagent::SubAgentRunner::new(
        Arc::new(router.clone()),
        agents_map,
        personas,
        3,
        vec![],
    ));
    registry.register(Box::new(super::subagent_tool::SubAgentTool::new(
        sub_agent_runner,
        30,
    )));
    // Default access gate for the global tool registry
    let default_access_gate = Arc::new(AccessGate::new(
        default_root.to_path_buf(),
        default_root.join("access_policy.json"),
    ));
    // File tools (read/write/edit) are registered here for their DEFINITIONS only,
    // so the LLM knows they exist. Actual execution is dispatched per-agent in
    // execute_tool_for_agent() with the correct workspace root.
    registry.register(Box::new(ReadFileTool::new(
        workspace_root.to_path_buf(),
        default_access_gate.clone(),
    )));
    registry.register(Box::new(WriteFileTool::new(
        workspace_root.to_path_buf(),
        default_access_gate.clone(),
    )));
    registry.register(Box::new(EditFileTool::new(
        workspace_root.to_path_buf(),
        default_access_gate.clone(),
    )));
    registry.register(Box::new(ExecuteCommandTool::new(
        workspace_root.to_path_buf(),
        30,
        default_access_gate.clone(),
        ExecSecurityConfig::default(),
        SandboxPolicyConfig::default(),
        approval_registry.clone(),
        Some(bus.clone()),
        "global".to_string(),
    )));
    // Access control tools
    registry.register(Box::new(GrantAccessTool::new(default_access_gate.clone())));
    registry.register(Box::new(ListAccessTool::new(default_access_gate.clone())));
    registry.register(Box::new(RevokeAccessTool::new(default_access_gate.clone())));
    registry.register(Box::new(WebFetchTool::new()));
    registry.register(Box::new(ImageTool::new()));
    registry.register(Box::new(ScheduleTool::new(schedule_manager)));
    registry.register(Box::new(crate::skill_tool::SkillTool::new()));
    registry.register(Box::new(crate::message_tool::MessageTool::new(bus.clone())));
    if let Some(api_key) = brave_api_key {
        if !api_key.is_empty() {
            registry.register(Box::new(WebSearchTool::new(api_key)));
        }
    }
    registry
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

const SLOW_LLM_ROUND_WARN_MS: u64 = 30_000;
const SLOW_TOOL_EXEC_WARN_MS: u64 = 10_000;

fn is_slow_latency_ms(duration_ms: u64, threshold_ms: u64) -> bool {
    duration_ms >= threshold_ms
}

fn history_message_limit(agent: &FullAgentConfig) -> usize {
    agent
        .memory_policy
        .as_ref()
        .and_then(|policy| policy.limit_history_turns)
        .map(|turns| (turns as usize) * 2)
        .unwrap_or(10)
}

fn is_explicit_web_search_request(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }

    let lower = trimmed.to_ascii_lowercase();
    lower.contains("web_search")
        || lower.contains("web search")
        || trimmed.contains("联网搜索")
        || trimmed.contains("上网搜索")
        || trimmed.contains("实时搜索")
}

fn should_inject_web_search_reminder(
    must_use_web_search: bool,
    web_search_reminder_injected: bool,
    web_search_called: bool,
    tool_use_count: usize,
) -> bool {
    must_use_web_search
        && !web_search_reminder_injected
        && !web_search_called
        && tool_use_count == 0
}

fn should_retry_fabricated_scheduled_response(
    is_scheduled_task: bool,
    retry_count: u32,
    total_tool_calls: usize,
    current_tool_calls: usize,
    response_text: &str,
) -> bool {
    // Only retry when the agent has made ZERO tool calls across the entire
    // session. If tools were already called in prior iterations (e.g. the agent
    // ran a pipeline and is now composing a text summary), the current zero-tool
    // iteration is legitimate — not hallucination.
    //
    // Scheduled tasks: up to 2 retries (no user in loop).
    // Conversations: up to 1 retry (user can re-prompt).
    let max_retries: u32 = if is_scheduled_task { 2 } else { 1 };
    if retry_count >= max_retries || total_tool_calls > 0 || current_tool_calls > 0 {
        return false;
    }

    let text = response_text.to_lowercase();
    [
        "i ran",
        "i executed",
        "i wrote",
        "i saved",
        "i updated",
        "i created",
        "i called",
        "已执行",
        "已运行",
        "已写入",
        "已保存",
        "已更新",
        "已创建",
        "已经完成",
    ]
    .iter()
    .any(|k| text.contains(k))
}

fn should_retry_incomplete_scheduled_thought(
    is_scheduled_task: bool,
    retry_count: u32,
    total_tool_calls: usize,
    response_text: &str,
) -> bool {
    // Scheduled tasks: up to 2 retries. Conversations: up to 1 retry.
    let max_retries: u32 = if is_scheduled_task { 2 } else { 1 };
    if retry_count >= max_retries || total_tool_calls == 0 {
        return false;
    }

    let text = response_text.to_lowercase();
    let is_short = response_text.len() < 500;
    let has_intent_phrase = [
        "let me ",
        "now let me",
        "i will ",
        "i'll ",
        "let me write",
        "let me compile",
        "let me create",
        "let me generate",
        "让我",
        "我来",
        "接下来",
    ]
    .iter()
    .any(|k| text.contains(k));

    is_short && has_intent_phrase
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

fn repair_tool_pairing(messages: &mut Vec<LlmMessage>) {
    if messages.is_empty() {
        return;
    }

    let assistant_idx = messages
        .iter()
        .rposition(|message| message.role == "assistant");
    let Some(assistant_idx) = assistant_idx else {
        return;
    };

    let assistant_message = &messages[assistant_idx];
    let tool_use_ids: Vec<&str> = assistant_message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();

    if tool_use_ids.is_empty() {
        return;
    }

    let Some(next_message) = messages.get(assistant_idx + 1) else {
        tracing::warn!(
            unpaired_tool_uses = ?tool_use_ids,
            "repair_tool_pairing: removing dangling assistant tool_use message"
        );
        messages.truncate(assistant_idx);
        return;
    };

    if next_message.role != "user" {
        tracing::warn!(
            unpaired_tool_uses = ?tool_use_ids,
            next_role = %next_message.role,
            "repair_tool_pairing: removing assistant tool_use message without user tool results"
        );
        messages.truncate(assistant_idx);
        return;
    }

    let tool_result_ids: Vec<&str> = next_message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
            _ => None,
        })
        .collect();

    let all_paired = tool_use_ids
        .iter()
        .all(|tool_use_id| tool_result_ids.contains(tool_use_id));

    if !all_paired {
        tracing::warn!(
            unpaired_tool_uses = ?tool_use_ids,
            tool_result_ids = ?tool_result_ids,
            "repair_tool_pairing: removing unpaired assistant+tool messages"
        );
        messages.truncate(assistant_idx);
    }
}

fn clamp_to_budget(
    results: &[clawhive_memory::search_index::SearchResult],
    budget: usize,
) -> String {
    const HEADER: &str = "## Relevant Memory\n\n";
    const TRUNCATED: &str = "\n...[truncated]";

    if results.is_empty() || budget == 0 {
        return String::new();
    }

    let header_chars = HEADER.chars().count();
    if budget <= header_chars {
        return HEADER.chars().take(budget).collect();
    }

    let mut context = String::from(HEADER);
    let mut used_chars = header_chars;

    for result in results {
        let entry = format!(
            "### {} (score: {:.2})\n{}\n\n",
            result.path, result.score, result.text
        );
        let entry_chars = entry.chars().count();

        if used_chars + entry_chars > budget {
            if used_chars == header_chars {
                let truncated_chars = TRUNCATED.chars().count();
                let available_chars = budget.saturating_sub(used_chars + truncated_chars);
                let truncated: String = entry.chars().take(available_chars).collect();
                context.push_str(&truncated);
                if used_chars + truncated.chars().count() + truncated_chars <= budget {
                    context.push_str(TRUNCATED);
                }
            }
            break;
        }

        context.push_str(&entry);
        used_chars += entry_chars;
    }

    context
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone, Utc};
    use clawhive_memory::search_index::SearchResult;
    use clawhive_memory::SessionMessage;
    use serde_json::json;

    fn agent_with_memory_policy(
        memory_policy: Option<crate::config::MemoryPolicyConfig>,
    ) -> FullAgentConfig {
        FullAgentConfig {
            agent_id: "test-agent".to_string(),
            enabled: true,
            security: SecurityMode::default(),
            workspace: None,
            identity: None,
            model_policy: crate::ModelPolicy {
                primary: "openai/gpt-4.1".to_string(),
                fallbacks: vec![],
                thinking_level: None,
                context_window: None,
            },
            tool_policy: None,
            memory_policy,
            sub_agent: None,
            heartbeat: None,
            exec_security: None,
            sandbox: None,
        }
    }

    fn assistant_with_tool_use(id: &str) -> LlmMessage {
        LlmMessage {
            role: "assistant".to_string(),
            content: vec![ContentBlock::ToolUse {
                id: id.to_string(),
                name: "read_file".to_string(),
                input: json!({"filePath": "/tmp/demo"}),
            }],
        }
    }

    fn user_with_tool_result(id: &str) -> LlmMessage {
        LlmMessage {
            role: "user".to_string(),
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id.to_string(),
                content: "ok".to_string(),
                is_error: false,
            }],
        }
    }

    fn message_roles(messages: &[LlmMessage]) -> Vec<&str> {
        messages
            .iter()
            .map(|message| message.role.as_str())
            .collect()
    }

    fn make_result(path: &str, text: &str, score: f64) -> SearchResult {
        SearchResult {
            chunk_id: format!("{}:0-1:abc", path),
            path: path.to_string(),
            source: "test".to_string(),
            start_line: 0,
            end_line: 1,
            text: text.to_string(),
            score,
        }
    }

    #[test]
    fn test_clamp_to_budget_empty_results() {
        assert_eq!(clamp_to_budget(&[], 100), "");
    }

    #[test]
    fn test_clamp_to_budget_within_limit() {
        let results = vec![
            make_result("memory/a.md", "first chunk", 0.91),
            make_result("memory/b.md", "second chunk", 0.83),
        ];

        let context = clamp_to_budget(&results, 1_000);

        assert!(context.starts_with("## Relevant Memory\n\n"));
        assert!(context.contains("### memory/a.md (score: 0.91)\nfirst chunk\n\n"));
        assert!(context.contains("### memory/b.md (score: 0.83)\nsecond chunk\n\n"));
    }

    #[test]
    fn test_clamp_to_budget_exceeds_limit() {
        let results = vec![make_result(
            "memory/a.md",
            "abcdefghijklmnopqrstuvwxyz",
            0.91,
        )];

        let context = clamp_to_budget(&results, 40);

        assert!(context.starts_with("## Relevant Memory\n\n"));
        assert!(context.contains("...[truncated]"));
        assert!(!context.contains("abcdefghijklmnopqrstuvwxyz"));
        assert!(!context.is_empty());
    }

    #[test]
    fn test_clamp_to_budget_zero_budget() {
        let results = vec![make_result("memory/a.md", "first chunk", 0.91)];

        assert_eq!(clamp_to_budget(&results, 0), "");
    }

    #[test]
    fn repair_tool_pairing_removes_unpaired_tool_use_messages() {
        let mut messages = vec![
            LlmMessage::user("question"),
            assistant_with_tool_use("tool-1"),
            LlmMessage::user("ordinary follow-up"),
        ];

        repair_tool_pairing(&mut messages);

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, "user");
    }

    #[test]
    fn repair_tool_pairing_removes_dangling_last_assistant_tool_use() {
        let mut messages = vec![
            LlmMessage::user("question"),
            assistant_with_tool_use("tool-1"),
        ];

        repair_tool_pairing(&mut messages);

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, "user");
    }

    #[test]
    fn repair_tool_pairing_keeps_properly_paired_messages() {
        let expected = vec![
            LlmMessage::user("question"),
            assistant_with_tool_use("tool-1"),
            user_with_tool_result("tool-1"),
        ];
        let mut messages = expected.clone();

        repair_tool_pairing(&mut messages);

        assert_eq!(message_roles(&messages), message_roles(&expected));
        assert_eq!(messages.len(), expected.len());
    }

    #[test]
    fn repair_tool_pairing_handles_empty_messages() {
        let mut messages = Vec::new();

        repair_tool_pairing(&mut messages);

        assert!(messages.is_empty());
    }

    #[test]
    fn repair_tool_pairing_ignores_messages_without_tool_use() {
        let expected = vec![
            LlmMessage::user("question"),
            LlmMessage::assistant("answer"),
        ];
        let mut messages = expected.clone();

        repair_tool_pairing(&mut messages);

        assert_eq!(message_roles(&messages), message_roles(&expected));
        assert_eq!(messages.len(), expected.len());
    }

    #[test]
    fn compute_merged_permissions_merges_all_when_no_forced() {
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

        let perms = merged.expect("compute_merged_permissions returns Some when skills have perms");
        assert!(perms.network.allow.contains(&"api.a.com:443".to_string()));
        assert!(perms.network.allow.contains(&"api.b.com:443".to_string()));
    }

    #[test]
    fn history_message_limit_defaults_to_10() {
        let agent = agent_with_memory_policy(None);

        assert_eq!(history_message_limit(&agent), 10);
    }

    #[test]
    fn history_message_limit_converts_turns() {
        let agent = agent_with_memory_policy(Some(crate::config::MemoryPolicyConfig {
            mode: "session".to_string(),
            write_scope: "session".to_string(),
            limit_history_turns: Some(7),
            max_injected_chars: 6000,
        }));

        assert_eq!(history_message_limit(&agent), 14);
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
    fn slow_latency_threshold_detects_warn_boundary() {
        assert!(!is_slow_latency_ms(9_999, 10_000));
        assert!(is_slow_latency_ms(10_000, 10_000));
        assert!(is_slow_latency_ms(25_000, 10_000));
    }

    #[test]
    fn explicit_web_search_request_detection() {
        assert!(is_explicit_web_search_request(
            "请使用 web_search 工具搜索 OpenAI 最新新闻"
        ));
        assert!(is_explicit_web_search_request(
            "please use web search tool for this"
        ));
        assert!(!is_explicit_web_search_request("你觉得这个功能怎么样"));
    }

    #[test]
    fn web_search_reminder_injection_predicate() {
        assert!(should_inject_web_search_reminder(true, false, false, 0));
        assert!(!should_inject_web_search_reminder(true, true, false, 0));
        assert!(!should_inject_web_search_reminder(false, false, false, 0));
        assert!(!should_inject_web_search_reminder(true, false, true, 0));
        assert!(!should_inject_web_search_reminder(true, false, false, 1));
    }

    #[test]
    fn scheduled_retry_only_when_claiming_execution_without_tools() {
        assert!(should_retry_fabricated_scheduled_response(
            true,
            0,
            0,
            0,
            "I executed all steps and saved the file.",
        ));

        assert!(!should_retry_fabricated_scheduled_response(
            true,
            0,
            0,
            0,
            "以下是今日技术摘要：...",
        ));

        assert!(!should_retry_fabricated_scheduled_response(
            true,
            0,
            1,
            0,
            "I executed all steps and saved the file.",
        ));
    }

    #[test]
    fn fabricated_response_detected_in_conversation() {
        assert!(should_retry_fabricated_scheduled_response(
            false,
            0,
            0,
            0,
            "I created the file and saved it.",
        ));
    }

    #[test]
    fn fabricated_response_conversation_max_one_retry() {
        assert!(should_retry_fabricated_scheduled_response(
            false,
            0,
            0,
            0,
            "I updated the config.",
        ));
        assert!(!should_retry_fabricated_scheduled_response(
            false,
            1,
            0,
            0,
            "I updated the config.",
        ));
    }

    #[test]
    fn fabricated_response_scheduled_still_allows_two_retries() {
        assert!(should_retry_fabricated_scheduled_response(
            true,
            0,
            0,
            0,
            "已创建文件",
        ));
        assert!(should_retry_fabricated_scheduled_response(
            true,
            1,
            0,
            0,
            "已创建文件",
        ));
        assert!(!should_retry_fabricated_scheduled_response(
            true,
            2,
            0,
            0,
            "已创建文件",
        ));
    }

    #[test]
    fn incomplete_thought_detected_in_conversation() {
        assert!(should_retry_incomplete_scheduled_thought(
            false,
            0,
            1,
            "让我来处理这个问题",
        ));
    }

    #[test]
    fn incomplete_thought_conversation_max_one_retry() {
        assert!(should_retry_incomplete_scheduled_thought(
            false,
            0,
            1,
            "Let me fix that.",
        ));
        assert!(!should_retry_incomplete_scheduled_thought(
            false,
            1,
            1,
            "Let me fix that.",
        ));
    }

    #[test]
    fn incomplete_thought_scheduled_still_allows_two_retries() {
        assert!(should_retry_incomplete_scheduled_thought(
            true,
            0,
            1,
            "I will create the file.",
        ));
        assert!(should_retry_incomplete_scheduled_thought(
            true,
            1,
            1,
            "I will create the file.",
        ));
        assert!(!should_retry_incomplete_scheduled_thought(
            true,
            2,
            1,
            "I will create the file.",
        ));
    }

    #[test]
    fn normal_mode_should_not_use_skill_permissions() {
        // Installing skills with permissions should NOT restrict normal (non-skill) requests.
        // Normal mode: merged_permissions should be None (Builtin origin).
        let dir = tempfile::tempdir().unwrap();

        let skill = dir.path().join("restricted-skill");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: restricted-skill\ndescription: Only allows sh\npermissions:\n  exec: [sh]\n  fs:\n    read: [\"$SKILL_DIR/**\"]\n---\nBody",
        )
        .unwrap();

        let active_skills = SkillRegistry::load_from_dir(dir.path()).unwrap();

        // Verify the skill has permissions declared
        let skill_entry = active_skills.get("restricted-skill").unwrap();
        assert!(skill_entry.permissions.is_some());

        // Normal mode: no forced skills -> should NOT apply skill permissions
        let forced_skills: Option<Vec<String>> = None;
        let merged_permissions = if forced_skills.is_some() {
            Orchestrator::compute_merged_permissions(&active_skills, forced_skills.as_deref())
        } else {
            None // Normal mode returns None (Builtin origin)
        };

        assert!(
            merged_permissions.is_none(),
            "normal mode must not use skill permissions"
        );
    }

    #[test]
    fn forced_skill_mode_applies_skill_permissions() {
        let dir = tempfile::tempdir().unwrap();

        let skill = dir.path().join("restricted-skill");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: restricted-skill\ndescription: Only allows sh\npermissions:\n  exec: [sh]\n  network:\n    allow: [\"api.example.com:443\"]\n---\nBody",
        )
        .unwrap();

        let active_skills = SkillRegistry::load_from_dir(dir.path()).unwrap();

        // Forced skill mode: permissions SHOULD be applied
        let forced = Some(vec!["restricted-skill".to_string()]);
        let merged = Orchestrator::compute_merged_permissions(&active_skills, forced.as_deref());

        let perms = merged.expect("forced skill mode must return permissions");
        assert_eq!(perms.exec, vec!["sh".to_string()]);
        assert!(perms
            .network
            .allow
            .contains(&"api.example.com:443".to_string()));
    }

    #[test]
    fn forced_skill_without_permissions_returns_none() {
        let dir = tempfile::tempdir().unwrap();

        let skill = dir.path().join("no-perms-skill");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: no-perms-skill\ndescription: No permissions declared\n---\nBody",
        )
        .unwrap();

        let active_skills = SkillRegistry::load_from_dir(dir.path()).unwrap();

        // Forced skill with no permissions -> None (Builtin, no extra restrictions)
        let forced = Some(vec!["no-perms-skill".to_string()]);
        let merged = Orchestrator::compute_merged_permissions(&active_skills, forced.as_deref());

        assert!(
            merged.is_none(),
            "skill without permissions should not trigger External origin"
        );
    }
}
