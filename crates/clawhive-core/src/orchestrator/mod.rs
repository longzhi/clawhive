use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{anyhow, Context as _, Result};
use arc_swap::ArcSwap;
use clawhive_bus::BusPublisher;
use clawhive_memory::dirty_sources::{DirtySourceStore, DIRTY_KIND_DAILY_FILE, DIRTY_KIND_SESSION};
use clawhive_memory::file_store::MemoryFileStore;
use clawhive_memory::search_index::SearchIndex;
use clawhive_memory::MemoryStore;
use clawhive_memory::{SessionReader, SessionWriter};
use clawhive_provider::{ContentBlock, LlmMessage, StreamChunk};
use clawhive_runtime::TaskExecutor;
use clawhive_schema::*;
use futures_core::Stream;
use tokio_util::sync::CancellationToken;

use crate::config_view::ConfigView;

use super::language_prefs::{
    apply_language_policy_prompt, detect_response_language, is_language_guard_exempt,
    log_language_guard, LanguagePrefs,
};

use super::access_gate::AccessGate;
use super::approval::ApprovalRegistry;
use super::session::{SessionManager, SessionResetReason};
use super::skill::SkillRegistry;
use super::skill_install_state::SkillInstallState;
use super::workspace::Workspace;
use super::workspace_manager::{AgentWorkspaceManager, AgentWorkspaceState};

mod attachment;
use attachment::*;

mod predicates;
pub use predicates::detect_skill_install_intent;
use predicates::*;

mod tool_registry;
pub use tool_registry::build_tool_registry;

mod episode;
pub(crate) use episode::contains_correction_phrase;
use episode::*;

mod memory_context;
#[cfg(test)]
use memory_context::*;

mod summary;
use summary::*;

mod skill_commands;
use skill_commands::SKILL_INSTALL_USAGE_HINT;

mod tool_loop;

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
    pending_boundary_recoveries: Arc<tokio::sync::Mutex<HashSet<String>>>,
    compaction_locks: Arc<tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
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
            .unwrap_or_else(|| SearchIndex::new(self.memory.db(), ""));
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
        let search_config = search_index.config().clone();

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
                search_index: SearchIndex::new_with_config(
                    memory.db(),
                    agent_id,
                    search_config.clone(),
                ),
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
            pending_boundary_recoveries: Arc::new(tokio::sync::Mutex::new(HashSet::new())),
            compaction_locks: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
        }
    }

    async fn session_compaction_lock(&self, session_key: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.compaction_locks.lock().await;
        locks
            .entry(session_key.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Handle `/model provider/model` — validate, persist, and apply model change.
    fn handle_model_change(
        &self,
        view: &Arc<ConfigView>,
        agent_id: &str,
        new_model: &str,
    ) -> Result<String> {
        // 1. Parse provider/model
        let (provider_id, model_name) = new_model
            .split_once('/')
            .ok_or_else(|| anyhow!("格式错误，请使用 provider/model 格式，如: openai/gpt-5.2"))?;

        if provider_id.is_empty() || model_name.is_empty() {
            anyhow::bail!("provider 和 model 不能为空，请使用格式: provider/model");
        }

        // 2. Validate provider exists in registry
        if !view.router.has_provider(provider_id) {
            let mut available = view.router.provider_ids();
            available.sort();
            let available = available.join(", ");
            anyhow::bail!("未找到 provider \"{provider_id}\"\n可用 providers: {available}");
        }

        // 3. Validate model exists in presets (only if provider has presets with models)
        if let Some(preset) = clawhive_schema::provider_presets::preset_by_id(provider_id) {
            if !preset.models.is_empty() && !preset.models.iter().any(|m| m.id == model_name) {
                let available =
                    clawhive_schema::provider_presets::provider_models_for_id(provider_id)
                        .join(", ");
                anyhow::bail!(
                    "provider \"{provider_id}\" 中未找到模型 \"{model_name}\"\n可用模型: {available}"
                );
            }
        }

        // 4. Persist to YAML
        let agent_yaml_path = self
            .workspace_root
            .join("config/agents.d")
            .join(format!("{agent_id}.yaml"));

        let yaml_content = std::fs::read_to_string(&agent_yaml_path)
            .with_context(|| format!("读取 agent 配置失败: {}", agent_yaml_path.display()))?;

        let mut doc: serde_yaml::Value =
            serde_yaml::from_str(&yaml_content).with_context(|| "解析 agent YAML 失败")?;

        doc.get_mut("model_policy")
            .and_then(|mp| mp.get_mut("primary"))
            .map(|primary| *primary = serde_yaml::Value::String(new_model.to_string()))
            .ok_or_else(|| anyhow!("agent YAML 中未找到 model_policy.primary 字段"))?;

        let updated_yaml = serde_yaml::to_string(&doc)?;
        std::fs::write(&agent_yaml_path, &updated_yaml)
            .with_context(|| format!("写入 agent 配置失败: {}", agent_yaml_path.display()))?;

        // 5. Swap in-memory config
        let mut agents = view.agents.clone();
        if let Some(agent_arc) = agents.get_mut(agent_id) {
            let mut agent = agent_arc.as_ref().clone();
            agent.model_policy.primary = new_model.to_string();
            *agent_arc = Arc::new(agent);
        }

        let new_view = ConfigView {
            generation: view.generation + 1,
            agents,
            personas: view.personas.clone(),
            routing: view.routing.clone(),
            router: view.router.clone(),
            tool_registry: view.tool_registry.clone(),
            embedding_provider: Arc::clone(&view.embedding_provider),
        };
        self.config_view.store(Arc::new(new_view));

        tracing::info!(
            agent_id = %agent_id,
            new_model = %new_model,
            "model changed via /model command"
        );

        Ok(format!("✅ 模型已切换为 **{new_model}**（已保存）"))
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

    async fn enqueue_dirty_source(
        &self,
        agent_id: &str,
        source_kind: &str,
        source_ref: &str,
        reason: &str,
    ) {
        let dirty = DirtySourceStore::new(self.memory.db());
        if let Err(error) = dirty
            .enqueue(agent_id, source_kind, source_ref, reason)
            .await
        {
            tracing::warn!(
                agent_id = %agent_id,
                source_kind = %source_kind,
                source_ref = %source_ref,
                %error,
                "failed to enqueue dirty source"
            );
        }
    }

    async fn drain_dirty_sources(&self, view: &ConfigView, agent_id: &str, limit: usize) {
        let workspace = self.workspace_state_for(agent_id);
        let dirty = DirtySourceStore::new(self.memory.db());
        if let Err(error) = workspace
            .search_index
            .process_dirty_sources(
                &dirty,
                agent_id,
                &workspace.file_store,
                &workspace.session_reader,
                view.embedding_provider.as_ref(),
                limit,
            )
            .await
        {
            tracing::warn!(agent_id = %agent_id, %error, "failed to index dirty sources");
        }
    }

    async fn process_session_close_daily_dirty(
        &self,
        view: &ConfigView,
        agent_id: &str,
        session_date: chrono::NaiveDate,
    ) {
        let daily_path = format!("memory/{}.md", session_date.format("%Y-%m-%d"));
        self.enqueue_dirty_source(
            agent_id,
            DIRTY_KIND_DAILY_FILE,
            &daily_path,
            "session_close",
        )
        .await;
        self.drain_dirty_sources(view, agent_id, 8).await;
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
                search_index: SearchIndex::new(self.memory.db(), agent_id),
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
        if let Some(cmd) = super::slash_commands::parse_command(&inbound.text) {
            match cmd {
                super::slash_commands::SlashCommand::Model { new_model } => {
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
                super::slash_commands::SlashCommand::Stop => {
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
                super::slash_commands::SlashCommand::Reset => {
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
                let mode = super::persona::PromptMode::from_message_source(
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
                let mode = super::persona::PromptMode::from_message_source(
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use super::*;
    use crate::config::{FullAgentConfig, SecurityMode};
    use crate::memory_retrieval::{MemoryHit, MemoryRoutingBias};
    use crate::router::LlmRouter;
    use crate::session::Session;
    use crate::tool::ToolContext;
    use async_trait::async_trait;
    use chrono::{Duration, TimeZone, Utc};
    use clawhive_bus::EventBus;
    use clawhive_memory::embedding::EmbeddingProvider;
    use clawhive_memory::embedding::StubEmbeddingProvider;
    use clawhive_memory::fact_store::FactStore;
    use clawhive_memory::file_store::MemoryFileStore;
    use clawhive_memory::search_index::SearchResult;
    use clawhive_memory::{
        EpisodeStateRecord, EpisodeStatusRecord, EpisodeTaskStateRecord, MemoryStore,
        RecentExplicitMemoryWrite, SessionEntry, SessionMemoryStateRecord, SessionMessage,
        SessionReader, SessionRecord,
    };
    use clawhive_provider::{ContentBlock, LlmProvider, LlmRequest, LlmResponse, ProviderRegistry};
    use clawhive_runtime::NativeExecutor;
    use clawhive_scheduler::{ScheduleManager, SqliteStore};
    use serde_json::json;
    use tempfile::TempDir;

    use crate::RoutingConfig;

    struct CompactionOnlyProvider;

    struct FailingEmbeddingProvider;

    struct SequenceProvider {
        responses: tokio::sync::Mutex<Vec<LlmResponse>>,
        call_count: AtomicUsize,
    }

    #[async_trait]
    impl LlmProvider for CompactionOnlyProvider {
        async fn chat(&self, request: LlmRequest) -> anyhow::Result<LlmResponse> {
            let text = if request
                .system
                .as_deref()
                .is_some_and(|system| system.starts_with("You are a conversation summarizer"))
            {
                "compact summary".to_string()
            } else {
                "reply: ok".to_string()
            };

            Ok(LlmResponse {
                text: text.clone(),
                content: vec![ContentBlock::Text { text }],
                input_tokens: None,
                output_tokens: None,
                stop_reason: Some("end_turn".into()),
            })
        }
    }

    #[async_trait]
    impl EmbeddingProvider for FailingEmbeddingProvider {
        async fn embed(
            &self,
            _texts: &[String],
        ) -> anyhow::Result<clawhive_memory::embedding::EmbeddingResult> {
            Err(anyhow!("embedding unavailable"))
        }

        fn model_id(&self) -> &str {
            "failing"
        }

        fn dimensions(&self) -> usize {
            0
        }
    }

    #[async_trait]
    impl LlmProvider for SequenceProvider {
        async fn chat(&self, _request: LlmRequest) -> anyhow::Result<LlmResponse> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            let mut responses = self.responses.lock().await;
            if responses.is_empty() {
                return Err(anyhow!("unexpected llm call"));
            }
            Ok(responses.remove(0))
        }
    }

    impl SequenceProvider {
        fn new(responses: Vec<LlmResponse>) -> Self {
            Self {
                responses: tokio::sync::Mutex::new(responses),
                call_count: AtomicUsize::new(0),
            }
        }

        fn call_count(&self) -> usize {
            self.call_count.load(Ordering::SeqCst)
        }
    }

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
                compaction_model: None,
            },
            tool_policy: None,
            memory_policy,
            sub_agent: None,
            heartbeat: None,
            exec_security: None,
            sandbox: None,
            max_response_tokens: None,
            max_iterations: None,
            turn_timeout_secs: None,
            typing_ttl_secs: None,
            progress_delay_secs: None,
        }
    }

    fn test_full_agent(agent_id: &str) -> FullAgentConfig {
        FullAgentConfig {
            agent_id: agent_id.to_string(),
            ..agent_with_memory_policy(None)
        }
    }

    async fn make_memory_tool_orchestrator(
        agent_ids: &[&str],
    ) -> (Orchestrator, TempDir, Arc<MemoryStore>) {
        let tmp = tempfile::tempdir().unwrap();
        let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
        let bus = EventBus::new(16);
        let publisher = bus.publisher();
        let file_store = MemoryFileStore::new(tmp.path());
        let search_index = SearchIndex::new(memory.db(), "default");
        let embedding_provider: Arc<dyn EmbeddingProvider> =
            Arc::new(StubEmbeddingProvider::new(8));
        let schedule_manager = Arc::new(
            ScheduleManager::new(
                SqliteStore::open(&tmp.path().join("data/scheduler.db")).unwrap(),
                Arc::new(EventBus::new(16)),
            )
            .await
            .unwrap(),
        );
        let router = LlmRouter::new(ProviderRegistry::new(), HashMap::new(), vec![]);
        let agents = agent_ids
            .iter()
            .map(|agent_id| test_full_agent(agent_id))
            .collect::<Vec<_>>();
        let tool_registry = build_tool_registry(
            &file_store,
            &search_index,
            &memory,
            &embedding_provider,
            tmp.path(),
            tmp.path(),
            &None,
            &publisher,
            Arc::clone(&schedule_manager),
            None,
            &router,
            &agents,
            &HashMap::new(),
        );
        let config_view = ConfigView::new(
            0,
            agents,
            HashMap::new(),
            RoutingConfig {
                default_agent_id: agent_ids.first().unwrap_or(&"agent-a").to_string(),
                bindings: vec![],
            },
            router,
            tool_registry,
            embedding_provider,
        );

        let orchestrator = OrchestratorBuilder::new(
            config_view,
            publisher,
            Arc::clone(&memory),
            Arc::new(NativeExecutor),
            tmp.path().to_path_buf(),
            schedule_manager,
        )
        .build();

        (orchestrator, tmp, memory)
    }

    async fn make_tool_loop_test_orchestrator(
        provider: Arc<dyn LlmProvider>,
        max_iterations: Option<u32>,
    ) -> (Orchestrator, TempDir, Arc<MemoryStore>) {
        let tmp = tempfile::tempdir().unwrap();
        let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
        let bus = EventBus::new(16);
        let publisher = bus.publisher();
        let file_store = MemoryFileStore::new(tmp.path());
        let search_index = SearchIndex::new(memory.db(), "agent-a");
        let embedding_provider: Arc<dyn EmbeddingProvider> =
            Arc::new(StubEmbeddingProvider::new(8));
        let schedule_manager = Arc::new(
            ScheduleManager::new(
                SqliteStore::open(&tmp.path().join("data/scheduler.db")).unwrap(),
                Arc::new(EventBus::new(16)),
            )
            .await
            .unwrap(),
        );
        let mut registry = ProviderRegistry::new();
        registry.register("test", provider);
        let router = LlmRouter::new(
            registry,
            HashMap::from([("test".to_string(), "test/model".to_string())]),
            vec![],
        );

        let mut agent = test_full_agent("agent-a");
        agent.workspace = Some(".".to_string());
        agent.model_policy.primary = "test/model".to_string();
        agent.max_iterations = max_iterations;
        let agents = vec![agent];
        let tool_registry = build_tool_registry(
            &file_store,
            &search_index,
            &memory,
            &embedding_provider,
            tmp.path(),
            tmp.path(),
            &None,
            &publisher,
            Arc::clone(&schedule_manager),
            None,
            &router,
            &agents,
            &HashMap::new(),
        );
        let config_view = ConfigView::new(
            0,
            agents,
            HashMap::new(),
            RoutingConfig {
                default_agent_id: "agent-a".to_string(),
                bindings: vec![],
            },
            router,
            tool_registry,
            embedding_provider,
        );

        let orchestrator = OrchestratorBuilder::new(
            config_view,
            publisher,
            Arc::clone(&memory),
            Arc::new(NativeExecutor),
            tmp.path().to_path_buf(),
            schedule_manager,
        )
        .build();

        (orchestrator, tmp, memory)
    }

    async fn make_file_backed_test_orchestrator(
        agent_id: &str,
        db_path: &std::path::Path,
        workspace_root: &std::path::Path,
    ) -> (Orchestrator, Arc<MemoryStore>) {
        let memory = Arc::new(MemoryStore::open(db_path.to_str().expect("db path")).unwrap());
        let bus = EventBus::new(16);
        let publisher = bus.publisher();
        let file_store = MemoryFileStore::new(workspace_root);
        let search_index = SearchIndex::new(memory.db(), agent_id);
        let embedding_provider: Arc<dyn EmbeddingProvider> =
            Arc::new(StubEmbeddingProvider::new(8));
        let schedule_manager = Arc::new(
            ScheduleManager::new(
                SqliteStore::open(&workspace_root.join("data/scheduler.db")).unwrap(),
                Arc::new(EventBus::new(16)),
            )
            .await
            .unwrap(),
        );
        let router = LlmRouter::new(ProviderRegistry::new(), HashMap::new(), vec![]);
        let mut agent = test_full_agent(agent_id);
        agent.workspace = Some(".".to_string());
        let agents = vec![agent];
        let tool_registry = build_tool_registry(
            &file_store,
            &search_index,
            &memory,
            &embedding_provider,
            workspace_root,
            workspace_root,
            &None,
            &publisher,
            Arc::clone(&schedule_manager),
            None,
            &router,
            &agents,
            &HashMap::new(),
        );
        let config_view = ConfigView::new(
            0,
            agents,
            HashMap::new(),
            RoutingConfig {
                default_agent_id: agent_id.to_string(),
                bindings: vec![],
            },
            router,
            tool_registry,
            embedding_provider,
        );
        let orchestrator = OrchestratorBuilder::new(
            config_view,
            publisher,
            Arc::clone(&memory),
            Arc::new(NativeExecutor),
            workspace_root.to_path_buf(),
            schedule_manager,
        )
        .build();

        (orchestrator, memory)
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

    fn make_result(path: &str, source: &str, text: &str, score: f64) -> SearchResult {
        SearchResult {
            chunk_id: format!("{}:0-1:abc", path),
            path: path.to_string(),
            source: source.to_string(),
            start_line: 0,
            end_line: 1,
            snippet: text.to_string(),
            text: text.to_string(),
            score,
            score_breakdown: None,
            access_count: 0,
        }
    }

    fn llm_text_response(text: &str, stop_reason: &str) -> LlmResponse {
        LlmResponse {
            text: text.to_string(),
            content: vec![ContentBlock::Text {
                text: text.to_string(),
            }],
            input_tokens: None,
            output_tokens: None,
            stop_reason: Some(stop_reason.to_string()),
        }
    }

    fn llm_tool_use_response(id: &str, name: &str, input: serde_json::Value) -> LlmResponse {
        LlmResponse {
            text: format!("calling {name}"),
            content: vec![ContentBlock::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
                input,
            }],
            input_tokens: None,
            output_tokens: None,
            stop_reason: Some("tool_use".to_string()),
        }
    }

    async fn wait_for_call_count(provider: &SequenceProvider, expected: usize) {
        for _ in 0..50 {
            if provider.call_count() >= expected {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("provider did not reach call count {expected}");
    }

    #[test]
    fn test_clamp_to_budget_empty_results() {
        assert_eq!(clamp_to_budget(&[], 100), "");
    }

    #[tokio::test]
    async fn tool_use_loop_returns_abort_message_when_cancelled_before_first_iteration() {
        let provider = Arc::new(SequenceProvider::new(vec![llm_text_response(
            "should not be called",
            "end_turn",
        )]));
        let (orchestrator, _tmp, _memory) =
            make_tool_loop_test_orchestrator(provider.clone(), Some(1)).await;
        let view = orchestrator.config_view();
        let cancel_token = CancellationToken::new();
        cancel_token.cancel();

        let (resp, _messages, _attachments, meta) = orchestrator
            .tool_use_loop(
                view.as_ref(),
                "agent-a",
                "session-cancelled-before-first-iteration",
                "test/model",
                &[],
                None,
                vec![LlmMessage::user("stop now")],
                512,
                None,
                None,
                SecurityMode::default(),
                vec![],
                None,
                false,
                false,
                None,
                cancel_token,
            )
            .await
            .unwrap();

        assert_eq!(provider.call_count(), 0);
        assert_eq!(resp.text, "[Task stopped by user]");
        assert_eq!(resp.stop_reason.as_deref(), Some("cancelled"));
        assert!(meta.cancelled);
        assert_eq!(meta.successful_tool_calls, 0);
        assert_eq!(meta.final_stop_reason.as_deref(), Some("cancelled"));
    }

    #[tokio::test]
    async fn tool_use_loop_returns_abort_message_with_completed_tool_summaries() {
        let provider = Arc::new(SequenceProvider::new(vec![llm_tool_use_response(
            "tool-1",
            "read_file",
            json!({"path": "sample.txt"}),
        )]));
        let (orchestrator, tmp, _memory) =
            make_tool_loop_test_orchestrator(provider.clone(), Some(2)).await;
        std::fs::write(
            tmp.path().join("sample.txt"),
            "previewable contents\nsecond line",
        )
        .unwrap();
        let view = orchestrator.config_view();
        let cancel_token = CancellationToken::new();

        let cancel_after_first_llm = async {
            wait_for_call_count(provider.as_ref(), 1).await;
            cancel_token.cancel();
        };

        let (result, _) = tokio::join!(
            orchestrator.tool_use_loop(
                view.as_ref(),
                "agent-a",
                "session-cancelled-after-tool",
                "test/model",
                &[],
                None,
                vec![LlmMessage::user("read the file")],
                512,
                None,
                None,
                SecurityMode::default(),
                vec![],
                None,
                false,
                false,
                None,
                cancel_token.clone(),
            ),
            cancel_after_first_llm,
        );

        let (resp, _messages, _attachments, meta) = result.unwrap();
        assert_eq!(provider.call_count(), 1);
        assert!(meta.cancelled);
        assert_eq!(meta.successful_tool_calls, 1);
        assert_eq!(meta.final_stop_reason.as_deref(), Some("cancelled"));
        assert!(resp
            .text
            .starts_with("[Task stopped by user]\n\nCompleted:"));
        assert!(resp.text.contains("- read_file: 1: previewable contents"));
    }

    #[tokio::test]
    async fn tool_use_loop_sets_cancelled_false_on_normal_completion() {
        let provider = Arc::new(SequenceProvider::new(vec![llm_text_response(
            "done", "end_turn",
        )]));
        let (orchestrator, _tmp, _memory) =
            make_tool_loop_test_orchestrator(provider, Some(2)).await;
        let view = orchestrator.config_view();

        let (resp, _messages, _attachments, meta) = orchestrator
            .tool_use_loop(
                view.as_ref(),
                "agent-a",
                "session-normal-finish",
                "test/model",
                &[],
                None,
                vec![LlmMessage::user("finish")],
                512,
                None,
                None,
                SecurityMode::default(),
                vec![],
                None,
                false,
                false,
                None,
                CancellationToken::new(),
            )
            .await
            .unwrap();

        assert_eq!(resp.text, "done");
        assert!(!meta.cancelled);
        assert_eq!(meta.final_stop_reason.as_deref(), Some("end_turn"));
    }

    #[tokio::test]
    async fn tool_use_loop_prioritizes_cancellation_over_max_iteration_path() {
        let provider = Arc::new(SequenceProvider::new(vec![llm_text_response(
            "should not be called",
            "end_turn",
        )]));
        let (orchestrator, _tmp, _memory) =
            make_tool_loop_test_orchestrator(provider.clone(), Some(1)).await;
        let view = orchestrator.config_view();
        let cancel_token = CancellationToken::new();
        cancel_token.cancel();

        let (resp, _messages, _attachments, meta) = orchestrator
            .tool_use_loop(
                view.as_ref(),
                "agent-a",
                "session-cancel-priority",
                "test/model",
                &[],
                None,
                vec![LlmMessage::user("cancel")],
                512,
                None,
                None,
                SecurityMode::default(),
                vec![],
                None,
                false,
                false,
                None,
                cancel_token,
            )
            .await
            .unwrap();

        assert_eq!(provider.call_count(), 0);
        assert_eq!(resp.stop_reason.as_deref(), Some("cancelled"));
        assert!(meta.cancelled);
    }

    #[tokio::test]
    async fn handle_with_view_persists_abort_message_to_session() {
        let provider = Arc::new(SequenceProvider::new(vec![llm_text_response(
            "should not be called",
            "end_turn",
        )]));
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

    #[test]
    fn test_clamp_to_budget_within_limit() {
        let results = vec![
            make_result("memory/a.md", "daily", "first chunk", 0.91),
            make_result("memory/b.md", "daily", "second chunk", 0.83),
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
            "daily",
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
        let results = vec![make_result("memory/a.md", "daily", "first chunk", 0.91)];

        assert_eq!(clamp_to_budget(&results, 0), "");
    }

    #[test]
    fn build_memory_context_from_hits_long_term_query_suppresses_daily_and_session_noise() {
        let hits = vec![
            MemoryHit::Chunk(Box::new(make_result(
                "MEMORY.md",
                "long_term",
                "长期主线：重构记忆系统，采用分层记忆架构。",
                1.32,
            ))),
            MemoryHit::Chunk(Box::new(make_result(
                "memory/2026-03-29.md",
                "daily",
                "daily 细节：品牌命名还在候选阶段。",
                0.94,
            ))),
            MemoryHit::Chunk(Box::new(make_result(
                "sessions/demo#turn:1-2",
                "session",
                "session 讨论：列出一堆当前缺陷清单。",
                0.81,
            ))),
        ];

        let context = build_memory_context_from_hits(&hits, 4_000);

        assert!(context.contains("MEMORY.md"));
        assert!(context.contains("长期主线：重构记忆系统"));
        assert!(!context.contains("品牌命名还在候选阶段"));
        assert!(!context.contains("列出一堆当前缺陷清单"));
    }

    #[test]
    fn build_memory_context_from_hits_short_term_query_prefers_daily_over_long_term() {
        let hits = vec![
            MemoryHit::Chunk(Box::new(make_result(
                "memory/2026-03-30.md",
                "daily",
                "短期事项：品牌命名还在候选阶段。",
                1.28,
            ))),
            MemoryHit::Chunk(Box::new(make_result(
                "sessions/demo#turn:1",
                "session",
                "session 补充：刚确认了几个候选词。",
                1.04,
            ))),
            MemoryHit::Chunk(Box::new(make_result(
                "MEMORY.md",
                "long_term",
                "长期主线：重构记忆系统。",
                0.83,
            ))),
        ];

        let context = build_memory_context_from_hits(&hits, 4_000);

        let daily_pos = context.find("memory/2026-03-30.md").expect("daily hit");
        let long_term_pos = context.find("MEMORY.md").expect("long term hit");
        assert!(daily_pos < long_term_pos);
        assert!(context.contains("品牌命名还在候选阶段"));
    }

    #[test]
    fn should_use_long_term_fallback_only_when_long_term_query_has_no_fact_or_memory_hit() {
        let daily_hit = MemoryHit::Chunk(Box::new(make_result(
            "memory/2026-03-30.md",
            "daily",
            "短期事项：品牌命名还在候选阶段。",
            1.0,
        )));
        let long_term_hit = MemoryHit::Chunk(Box::new(make_result(
            "MEMORY.md",
            "long_term",
            "长期主线：重构记忆系统。",
            0.8,
        )));

        assert!(should_use_long_term_fallback(
            MemoryRoutingBias::LongTerm,
            std::slice::from_ref(&daily_hit),
        ));
        assert!(!should_use_long_term_fallback(
            MemoryRoutingBias::LongTerm,
            &[daily_hit, long_term_hit.clone()],
        ));
        assert!(!should_use_long_term_fallback(
            MemoryRoutingBias::ShortTerm,
            std::slice::from_ref(&long_term_hit),
        ));
    }

    #[tokio::test]
    async fn execute_tool_for_agent_scopes_memory_write_to_current_agent() {
        let (orchestrator, _tmp, memory) =
            make_memory_tool_orchestrator(&["agent-a", "agent-b"]).await;
        let view = orchestrator.config_view();
        let ctx = ToolContext::builtin();

        let output = orchestrator
            .execute_tool_for_agent(
                view.as_ref(),
                "agent-a",
                "memory_write",
                json!({
                    "content": "User prefers green tea",
                    "fact_type": "preference"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!output.is_error);

        let fact_store = FactStore::new(memory.db());
        assert!(fact_store
            .find_by_content("agent-a", "User prefers green tea")
            .await
            .unwrap()
            .is_some());
        assert!(fact_store
            .find_by_content("agent-b", "User prefers green tea")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn execute_tool_for_agent_scopes_memory_get_to_current_agent_workspace() {
        let (orchestrator, _tmp, _memory) =
            make_memory_tool_orchestrator(&["agent-a", "agent-b"]).await;
        let view = orchestrator.config_view();
        let ctx = ToolContext::builtin();

        orchestrator
            .file_store_for("agent-a")
            .write_long_term("# Agent A memory")
            .await
            .unwrap();
        orchestrator
            .file_store_for("agent-b")
            .write_long_term("# Agent B memory")
            .await
            .unwrap();

        let output = orchestrator
            .execute_tool_for_agent(
                view.as_ref(),
                "agent-a",
                "memory_get",
                json!({"key": "MEMORY.md"}),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!output.is_error);
        assert!(output.content.contains("Agent A memory"));
        assert!(!output.content.contains("Agent B memory"));
    }

    #[tokio::test]
    async fn execute_tool_for_agent_memory_search_returns_fact_hits() {
        let (orchestrator, _tmp, _memory) =
            make_memory_tool_orchestrator(&["agent-a", "agent-b"]).await;
        let view = orchestrator.config_view();
        let ctx = ToolContext::builtin();

        orchestrator
            .execute_tool_for_agent(
                view.as_ref(),
                "agent-a",
                "memory_write",
                json!({
                    "content": "User prefers Chinese replies",
                    "fact_type": "preference"
                }),
                &ctx,
            )
            .await
            .unwrap();

        let output = orchestrator
            .execute_tool_for_agent(
                view.as_ref(),
                "agent-a",
                "memory_search",
                json!({"query": "Chinese replies"}),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!output.is_error);
        assert!(output.content.contains("[fact:preference]"));
        assert!(output.content.contains("[fact]"));
        assert!(output.content.contains("Chinese replies"));
    }

    #[tokio::test]
    async fn build_tool_registry_registers_memory_fact_tools() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
        let file_store = MemoryFileStore::new(dir.path());
        let search_index = SearchIndex::new(memory.db(), "test-agent");
        let embedding_provider: Arc<dyn EmbeddingProvider> =
            Arc::new(StubEmbeddingProvider::new(8));
        let router = LlmRouter::new(
            clawhive_provider::ProviderRegistry::new(),
            HashMap::new(),
            vec![],
        );
        let bus = EventBus::new(16);
        let schedule_manager = Arc::new(
            ScheduleManager::new(
                SqliteStore::open(&dir.path().join("data/scheduler.db")).unwrap(),
                Arc::new(EventBus::new(16)),
            )
            .await
            .unwrap(),
        );
        let agents = vec![agent_with_memory_policy(None)];
        let personas = HashMap::new();

        let registry = build_tool_registry(
            &file_store,
            &search_index,
            &memory,
            &embedding_provider,
            dir.path(),
            dir.path(),
            &None,
            &bus.publisher(),
            schedule_manager,
            None,
            &router,
            &agents,
            &personas,
        );
        let tool_names: Vec<String> = registry
            .tool_defs()
            .into_iter()
            .map(|tool| tool.name)
            .collect();

        assert!(tool_names.iter().any(|name| name == "memory_write"));
        assert!(tool_names.iter().any(|name| name == "memory_forget"));
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
    fn collect_unflushed_boundary_episodes_only_returns_turns_after_checkpoint() {
        let entries = vec![
            SessionEntry::Message {
                id: "m1".to_string(),
                timestamp: Utc.with_ymd_and_hms(2026, 3, 30, 10, 0, 0).unwrap(),
                message: SessionMessage {
                    role: "user".to_string(),
                    content: "first user".to_string(),
                    timestamp: None,
                },
            },
            SessionEntry::Message {
                id: "m2".to_string(),
                timestamp: Utc.with_ymd_and_hms(2026, 3, 30, 10, 0, 1).unwrap(),
                message: SessionMessage {
                    role: "assistant".to_string(),
                    content: "first reply".to_string(),
                    timestamp: None,
                },
            },
            SessionEntry::Message {
                id: "m3".to_string(),
                timestamp: Utc.with_ymd_and_hms(2026, 3, 30, 10, 1, 0).unwrap(),
                message: SessionMessage {
                    role: "user".to_string(),
                    content: "second user".to_string(),
                    timestamp: None,
                },
            },
            SessionEntry::Message {
                id: "m4".to_string(),
                timestamp: Utc.with_ymd_and_hms(2026, 3, 30, 10, 1, 1).unwrap(),
                message: SessionMessage {
                    role: "assistant".to_string(),
                    content: "second reply".to_string(),
                    timestamp: None,
                },
            },
        ];

        let (episodes, turn_count) =
            collect_unflushed_boundary_episodes(entries, 1).expect("snapshot");

        assert_eq!(turn_count, 2);
        assert_eq!(episodes.len(), 1);
        assert_eq!(episodes[0].start_turn, 2);
        assert_eq!(episodes[0].end_turn, 2);
        assert_eq!(episodes[0].messages.len(), 2);
        assert_eq!(episodes[0].messages[0].content, "second user");
        assert_eq!(episodes[0].messages[1].content, "second reply");
    }

    #[test]
    fn collect_unflushed_boundary_episodes_groups_related_turns() {
        let entries = vec![
            SessionEntry::Message {
                id: "m1".to_string(),
                timestamp: Utc.with_ymd_and_hms(2026, 3, 30, 10, 0, 0).unwrap(),
                message: SessionMessage {
                    role: "user".to_string(),
                    content: "How do I use Rust Vec push?".to_string(),
                    timestamp: None,
                },
            },
            SessionEntry::Message {
                id: "m2".to_string(),
                timestamp: Utc.with_ymd_and_hms(2026, 3, 30, 10, 0, 1).unwrap(),
                message: SessionMessage {
                    role: "assistant".to_string(),
                    content: "Use Vec::push to append items.".to_string(),
                    timestamp: None,
                },
            },
            SessionEntry::Message {
                id: "m3".to_string(),
                timestamp: Utc.with_ymd_and_hms(2026, 3, 30, 10, 1, 0).unwrap(),
                message: SessionMessage {
                    role: "user".to_string(),
                    content: "What about Rust Vec insert?".to_string(),
                    timestamp: None,
                },
            },
            SessionEntry::Message {
                id: "m4".to_string(),
                timestamp: Utc.with_ymd_and_hms(2026, 3, 30, 10, 1, 1).unwrap(),
                message: SessionMessage {
                    role: "assistant".to_string(),
                    content: "Use Vec::insert for indexed insertion.".to_string(),
                    timestamp: None,
                },
            },
        ];

        let (episodes, turn_count) =
            collect_unflushed_boundary_episodes(entries, 0).expect("snapshot");

        assert_eq!(turn_count, 2);
        assert_eq!(episodes.len(), 1);
        assert_eq!(episodes[0].start_turn, 1);
        assert_eq!(episodes[0].end_turn, 2);
        assert_eq!(episodes[0].messages.len(), 4);
    }

    #[test]
    fn collect_unflushed_boundary_turns_does_not_truncate_long_unflushed_history() {
        let mut entries = Vec::new();
        for turn in 1..=60 {
            entries.push(SessionEntry::Message {
                id: format!("u-{turn}"),
                timestamp: Utc::now(),
                message: SessionMessage {
                    role: "user".to_string(),
                    content: format!("user turn {turn}"),
                    timestamp: None,
                },
            });
            entries.push(SessionEntry::Message {
                id: format!("a-{turn}"),
                timestamp: Utc::now(),
                message: SessionMessage {
                    role: "assistant".to_string(),
                    content: format!("assistant turn {turn}"),
                    timestamp: None,
                },
            });
        }

        let (turns, turn_count) = collect_unflushed_boundary_turns(entries, 0).expect("snapshot");
        assert_eq!(turn_count, 60);
        assert_eq!(turns.len(), 60);
        assert_eq!(turns.first().map(|turn| turn.start_turn), Some(1));
        assert_eq!(turns.last().map(|turn| turn.end_turn), Some(60));
    }

    #[tokio::test]
    async fn record_session_turn_episode_merges_related_turns_into_same_open_episode() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("memory.db");
        let session_id = "session-episode-1";
        let session_key = "telegram:tg:chat:episode-1";
        let agent_id = "agent-a";

        let (orchestrator, memory) =
            make_file_backed_test_orchestrator(agent_id, &db_path, tmp.path()).await;
        let session = Session {
            session_key: SessionKey(session_key.to_string()),
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
            created_at: Utc::now(),
            last_active: Utc::now(),
            ttl_seconds: 1800,
            interaction_count: 0,
        };

        orchestrator
            .record_session_turn_episode(
                agent_id,
                &session,
                EpisodeTurnInput {
                    turn_index: 1,
                    user_text: "How do I use Rust Vec push?",
                    assistant_text: "Use Vec::push to append items.",
                    successful_tool_calls: 0,
                    final_stop_reason: Some("end_turn"),
                },
            )
            .await;
        orchestrator
            .record_session_turn_episode(
                agent_id,
                &session,
                EpisodeTurnInput {
                    turn_index: 2,
                    user_text: "What about Rust Vec insert?",
                    assistant_text: "Use Vec::insert for indexed insertion.",
                    successful_tool_calls: 0,
                    final_stop_reason: Some("end_turn"),
                },
            )
            .await;

        let state = memory
            .get_session_memory_state(agent_id, session_id)
            .await
            .unwrap()
            .expect("session memory state");
        assert_eq!(state.open_episodes.len(), 1);
        let episode = &state.open_episodes[0];
        assert_eq!(episode.start_turn, 1);
        assert_eq!(episode.end_turn, 2);
        assert_eq!(episode.status, EpisodeStatusRecord::Open);
        assert_eq!(episode.task_state, EpisodeTaskStateRecord::Delivered);
    }

    #[tokio::test]
    async fn record_session_turn_episode_closes_previous_episode_on_topic_switch() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("memory.db");
        let session_id = "session-episode-closure";
        let session_key = "telegram:tg:chat:episode-closure";
        let agent_id = "agent-a";

        let (orchestrator, memory) =
            make_file_backed_test_orchestrator(agent_id, &db_path, tmp.path()).await;
        let session = Session {
            session_key: SessionKey(session_key.to_string()),
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
            created_at: Utc::now(),
            last_active: Utc::now(),
            ttl_seconds: 1800,
            interaction_count: 0,
        };

        orchestrator
            .record_session_turn_episode(
                agent_id,
                &session,
                EpisodeTurnInput {
                    turn_index: 1,
                    user_text: "How do I use Rust Vec push?",
                    assistant_text: "Use Vec::push to append items.",
                    successful_tool_calls: 0,
                    final_stop_reason: Some("end_turn"),
                },
            )
            .await;
        let closed = orchestrator
            .record_session_turn_episode(
                agent_id,
                &session,
                EpisodeTurnInput {
                    turn_index: 2,
                    user_text: "How do I inspect RunPod GPU usage?",
                    assistant_text: "Use nvidia-smi on the pod.",
                    successful_tool_calls: 0,
                    final_stop_reason: Some("end_turn"),
                },
            )
            .await
            .expect("closed episode");

        assert_eq!(closed.start_turn, 1);
        assert_eq!(closed.end_turn, 1);
        assert_eq!(closed.status, EpisodeStatusRecord::Closed);

        let state = memory
            .get_session_memory_state(agent_id, session_id)
            .await
            .unwrap()
            .expect("session memory state");
        assert_eq!(state.open_episodes.len(), 2);
        assert_eq!(state.open_episodes[0].status, EpisodeStatusRecord::Closed);
        assert_eq!(state.open_episodes[1].status, EpisodeStatusRecord::Open);
        assert_eq!(state.open_episodes[1].start_turn, 2);
    }

    #[test]
    fn infer_episode_task_state_distinguishes_structural_delivery_states() {
        assert_eq!(
            infer_episode_task_state("好，让我把所有内容整合起来：", 0, Some("end_turn")),
            EpisodeTaskStateRecord::Executing
        );
        assert_eq!(
            infer_episode_task_state("我现在就整理给你", 0, Some("end_turn")),
            EpisodeTaskStateRecord::Exploring
        );
        assert_eq!(
            infer_episode_task_state("整理好了，答案如下。", 0, Some("end_turn")),
            EpisodeTaskStateRecord::Delivered
        );
        assert_eq!(
            infer_episode_task_state("我现在就整理给你", 1, Some("end_turn")),
            EpisodeTaskStateRecord::Delivered
        );
        assert_eq!(
            infer_episode_task_state("整理到一半", 0, Some("length")),
            EpisodeTaskStateRecord::Executing
        );
    }

    #[test]
    fn decide_episode_turn_keeps_related_topics_in_same_episode() {
        let current = boundary_flush_topic_tokens_from_text("rust vec push");
        let next = boundary_flush_topic_tokens_from_text("rust vec capacity");

        let decision = decide_episode_turn(
            &current,
            &next,
            "整理好了，答案如下。",
            0,
            Some("end_turn"),
            EpisodeTaskStateRecord::Delivered,
            1,
        );

        assert_eq!(decision.boundary, EpisodeBoundaryDecision::ContinueCurrent);
        assert_eq!(decision.task_state, EpisodeTaskStateRecord::Delivered);
    }

    #[test]
    fn decide_episode_turn_splits_unrelated_topics_and_tracks_runtime_state() {
        let current = boundary_flush_topic_tokens_from_text("rust vec push");
        let next = boundary_flush_topic_tokens_from_text("runpod gpu inspect");

        let decision = decide_episode_turn(
            &current,
            &next,
            "我现在就整理给你",
            1,
            Some("end_turn"),
            EpisodeTaskStateRecord::Delivered,
            1,
        );

        assert_eq!(
            decision.boundary,
            EpisodeBoundaryDecision::CloseCurrentAndOpenNext
        );
        assert_eq!(decision.task_state, EpisodeTaskStateRecord::Delivered);
    }

    #[test]
    fn decide_episode_turn_splits_when_current_episode_reaches_turn_cap() {
        let current = boundary_flush_topic_tokens_from_text("rust vec push");
        let next = boundary_flush_topic_tokens_from_text("rust vec capacity");

        let decision = decide_episode_turn(
            &current,
            &next,
            "继续补充说明。",
            0,
            Some("end_turn"),
            EpisodeTaskStateRecord::Delivered,
            MAX_OPEN_EPISODE_TURNS,
        );

        assert_eq!(
            decision.boundary,
            EpisodeBoundaryDecision::CloseCurrentAndOpenNext
        );
    }

    #[test]
    fn fact_token_overlap_requires_high_similarity() {
        let overlap = fact_token_overlap_ratio(
            "User prefers Rust for backend services",
            "User prefers Rust for backend systems",
        );
        assert!(overlap > 0.6);

        let low_overlap = fact_token_overlap_ratio(
            "User prefers Rust for backend services",
            "User moved to Tokyo last month",
        );
        assert!(low_overlap < 0.6);
    }

    #[test]
    fn boundary_flush_conflict_requires_same_type_and_embedding_signal() {
        let old_fact = clawhive_memory::fact_store::Fact {
            id: "old".to_string(),
            agent_id: "agent-1".to_string(),
            content: "User prefers Rust for backend services".to_string(),
            fact_type: "preference".to_string(),
            importance: 0.7,
            confidence: 1.0,
            status: "active".to_string(),
            occurred_at: None,
            recorded_at: Utc::now().to_rfc3339(),
            source_type: "boundary_flush".to_string(),
            source_session: None,
            access_count: 0,
            last_accessed: None,
            superseded_by: None,
            salience: 50,
            supersede_reason: None,
            affect: "neutral".to_string(),
            affect_intensity: 0.0,
            created_at: Utc::now().to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
        };
        let different_type = clawhive_memory::fact_store::Fact {
            fact_type: "decision".to_string(),
            ..old_fact.clone()
        };

        assert!(boundary_flush_conflict_passes_two_step(
            "User prefers Rust for backend systems",
            "preference",
            &old_fact,
            Some(0.9)
        ));
        assert!(!boundary_flush_conflict_passes_two_step(
            "User prefers Rust for backend systems",
            "preference",
            &different_type,
            Some(0.9)
        ));
        assert!(!boundary_flush_conflict_passes_two_step(
            "User prefers Rust for backend systems",
            "preference",
            &old_fact,
            None
        ));
        assert!(boundary_flush_conflict_passes_two_step(
            "User no longer uses Python for backend systems",
            "preference",
            &old_fact,
            Some(0.9)
        ));
    }

    #[tokio::test]
    async fn boundary_flush_conflict_check_fallbacks_to_insert_on_embedding_failure() {
        let old_fact = clawhive_memory::fact_store::Fact {
            id: "old".to_string(),
            agent_id: "agent-1".to_string(),
            content: "User prefers Rust for backend services".to_string(),
            fact_type: "preference".to_string(),
            importance: 0.7,
            confidence: 1.0,
            status: "active".to_string(),
            occurred_at: None,
            recorded_at: Utc::now().to_rfc3339(),
            source_type: "boundary_flush".to_string(),
            source_session: None,
            access_count: 0,
            last_accessed: None,
            superseded_by: None,
            salience: 50,
            supersede_reason: None,
            affect: "neutral".to_string(),
            affect_intensity: 0.0,
            created_at: Utc::now().to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
        };

        let provider: Arc<dyn EmbeddingProvider> = Arc::new(FailingEmbeddingProvider);
        let conflict = find_boundary_flush_conflict(
            &provider,
            "User prefers Rust for backend systems",
            "preference",
            &[old_fact],
        )
        .await
        .unwrap_or_default();

        assert!(conflict.is_none());
    }

    #[tokio::test]
    async fn record_session_turn_episode_marks_open_episode_executing_for_structural_promise() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("memory.db");
        let session_id = "session-episode-task-state";
        let session_key = "telegram:tg:chat:episode-task-state";
        let agent_id = "agent-a";

        let (orchestrator, memory) =
            make_file_backed_test_orchestrator(agent_id, &db_path, tmp.path()).await;
        let session = Session {
            session_key: SessionKey(session_key.to_string()),
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
            created_at: Utc::now(),
            last_active: Utc::now(),
            ttl_seconds: 1800,
            interaction_count: 0,
        };

        orchestrator
            .record_session_turn_episode(
                agent_id,
                &session,
                EpisodeTurnInput {
                    turn_index: 1,
                    user_text: "请整理 memory 重构方案",
                    assistant_text: "好，让我把所有内容整合起来：",
                    successful_tool_calls: 0,
                    final_stop_reason: Some("end_turn"),
                },
            )
            .await;

        let state = memory
            .get_session_memory_state(agent_id, session_id)
            .await
            .unwrap()
            .expect("session memory state");
        assert_eq!(state.open_episodes.len(), 1);
        assert_eq!(
            state.open_episodes[0].task_state,
            EpisodeTaskStateRecord::Executing
        );
    }

    #[tokio::test]
    async fn record_session_turn_episode_marks_inconclusive_reply_delivered_after_tool_execution() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("memory.db");
        let session_id = "session-episode-task-state-tools";
        let session_key = "telegram:tg:chat:episode-task-state-tools";
        let agent_id = "agent-a";

        let (orchestrator, memory) =
            make_file_backed_test_orchestrator(agent_id, &db_path, tmp.path()).await;
        let session = Session {
            session_key: SessionKey(session_key.to_string()),
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
            created_at: Utc::now(),
            last_active: Utc::now(),
            ttl_seconds: 1800,
            interaction_count: 0,
        };

        orchestrator
            .record_session_turn_episode(
                agent_id,
                &session,
                EpisodeTurnInput {
                    turn_index: 1,
                    user_text: "请帮我检查 GPU 状态",
                    assistant_text: "我现在就整理给你",
                    successful_tool_calls: 1,
                    final_stop_reason: Some("end_turn"),
                },
            )
            .await;

        let state = memory
            .get_session_memory_state(agent_id, session_id)
            .await
            .unwrap()
            .expect("session memory state");
        assert_eq!(state.open_episodes.len(), 1);
        assert_eq!(
            state.open_episodes[0].task_state,
            EpisodeTaskStateRecord::Delivered
        );
    }

    #[tokio::test]
    async fn boundary_flush_snapshot_prefers_persisted_open_episode_ranges() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("memory.db");
        let session_id = "session-episode-2";
        let session_key = "telegram:tg:chat:episode-2";
        let agent_id = "agent-a";

        {
            let store = MemoryStore::open(db_path.to_str().expect("db path")).unwrap();
            store
                .upsert_session(SessionRecord {
                    session_key: session_key.to_string(),
                    session_id: session_id.to_string(),
                    agent_id: agent_id.to_string(),
                    created_at: Utc::now(),
                    last_active: Utc::now(),
                    ttl_seconds: 1800,
                    interaction_count: 2,
                })
                .await
                .unwrap();
            store
                .upsert_session_memory_state(SessionMemoryStateRecord {
                    agent_id: agent_id.to_string(),
                    session_id: session_id.to_string(),
                    session_key: session_key.to_string(),
                    last_flushed_turn: 0,
                    last_boundary_flush_at: None,
                    pending_flush: false,
                    flush_phase: "idle".to_string(),
                    flush_phase_updated_at: None,
                    flush_summary_cache: None,
                    recent_explicit_writes: Vec::new(),
                    open_episodes: vec![EpisodeStateRecord {
                        episode_id: format!("{session_id}:1"),
                        start_turn: 1,
                        end_turn: 2,
                        status: EpisodeStatusRecord::Open,
                        task_state: EpisodeTaskStateRecord::Delivered,
                        topic_sketch: "rust vec".to_string(),
                        last_activity_at: Utc::now(),
                    }],
                })
                .await
                .unwrap();
        }

        let writer = SessionWriter::new(tmp.path());
        writer
            .append_message(session_id, "user", "How do I use Rust Vec push?")
            .await
            .unwrap();
        writer
            .append_message(session_id, "assistant", "Use Vec::push to append items.")
            .await
            .unwrap();
        writer
            .append_message(session_id, "user", "Completely different new task")
            .await
            .unwrap();
        writer
            .append_message(session_id, "assistant", "Handled the unrelated task.")
            .await
            .unwrap();

        let (orchestrator, _memory) =
            make_file_backed_test_orchestrator(agent_id, &db_path, tmp.path()).await;
        let session = Session {
            session_key: SessionKey(session_key.to_string()),
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
            created_at: Utc::now(),
            last_active: Utc::now(),
            ttl_seconds: 1800,
            interaction_count: 2,
        };

        let snapshot = orchestrator
            .capture_boundary_flush_snapshot(agent_id, &session, &test_full_agent(agent_id))
            .await
            .expect("snapshot");

        assert_eq!(snapshot.episodes.len(), 1);
        assert_eq!(snapshot.episodes[0].start_turn, 1);
        assert_eq!(snapshot.episodes[0].end_turn, 2);
        assert_eq!(snapshot.episodes[0].messages.len(), 4);
    }

    #[tokio::test]
    async fn boundary_flush_snapshot_ignores_already_flushed_episode_ranges() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("memory.db");
        let session_id = "session-episode-3";
        let session_key = "telegram:tg:chat:episode-3";
        let agent_id = "agent-a";

        {
            let store = MemoryStore::open(db_path.to_str().expect("db path")).unwrap();
            store
                .upsert_session(SessionRecord {
                    session_key: session_key.to_string(),
                    session_id: session_id.to_string(),
                    agent_id: agent_id.to_string(),
                    created_at: Utc::now(),
                    last_active: Utc::now(),
                    ttl_seconds: 1800,
                    interaction_count: 2,
                })
                .await
                .unwrap();
            store
                .upsert_session_memory_state(SessionMemoryStateRecord {
                    agent_id: agent_id.to_string(),
                    session_id: session_id.to_string(),
                    session_key: session_key.to_string(),
                    last_flushed_turn: 1,
                    last_boundary_flush_at: Some(Utc::now()),
                    pending_flush: false,
                    flush_phase: "idle".to_string(),
                    flush_phase_updated_at: None,
                    flush_summary_cache: None,
                    recent_explicit_writes: Vec::new(),
                    open_episodes: vec![
                        EpisodeStateRecord {
                            episode_id: format!("{session_id}:1"),
                            start_turn: 1,
                            end_turn: 1,
                            status: EpisodeStatusRecord::Flushed,
                            task_state: EpisodeTaskStateRecord::Delivered,
                            topic_sketch: "rust vec".to_string(),
                            last_activity_at: Utc::now(),
                        },
                        EpisodeStateRecord {
                            episode_id: format!("{session_id}:2"),
                            start_turn: 2,
                            end_turn: 2,
                            status: EpisodeStatusRecord::Open,
                            task_state: EpisodeTaskStateRecord::Delivered,
                            topic_sketch: "runpod gpu".to_string(),
                            last_activity_at: Utc::now(),
                        },
                    ],
                })
                .await
                .unwrap();
        }

        let writer = SessionWriter::new(tmp.path());
        writer
            .append_message(session_id, "user", "How do I use Rust Vec push?")
            .await
            .unwrap();
        writer
            .append_message(session_id, "assistant", "Use Vec::push to append items.")
            .await
            .unwrap();
        writer
            .append_message(session_id, "user", "How do I inspect RunPod GPU usage?")
            .await
            .unwrap();
        writer
            .append_message(session_id, "assistant", "Use nvidia-smi on the pod.")
            .await
            .unwrap();

        let (orchestrator, _memory) =
            make_file_backed_test_orchestrator(agent_id, &db_path, tmp.path()).await;
        let session = Session {
            session_key: SessionKey(session_key.to_string()),
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
            created_at: Utc::now(),
            last_active: Utc::now(),
            ttl_seconds: 1800,
            interaction_count: 2,
        };

        let snapshot = orchestrator
            .capture_boundary_flush_snapshot(agent_id, &session, &test_full_agent(agent_id))
            .await
            .expect("snapshot");

        assert_eq!(snapshot.episodes.len(), 1);
        assert_eq!(snapshot.episodes[0].start_turn, 2);
        assert_eq!(snapshot.episodes[0].end_turn, 2);
        assert_eq!(snapshot.turn_count, 2);
    }

    #[tokio::test]
    async fn boundary_flush_snapshot_skips_recent_flush_pending_episode_ranges() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("memory.db");
        let session_id = "session-episode-pending";
        let session_key = "telegram:tg:chat:episode-pending";
        let agent_id = "agent-a";

        {
            let store = MemoryStore::open(db_path.to_str().expect("db path")).unwrap();
            store
                .upsert_session(SessionRecord {
                    session_key: session_key.to_string(),
                    session_id: session_id.to_string(),
                    agent_id: agent_id.to_string(),
                    created_at: Utc::now(),
                    last_active: Utc::now(),
                    ttl_seconds: 1800,
                    interaction_count: 2,
                })
                .await
                .unwrap();
            store
                .upsert_session_memory_state(SessionMemoryStateRecord {
                    agent_id: agent_id.to_string(),
                    session_id: session_id.to_string(),
                    session_key: session_key.to_string(),
                    last_flushed_turn: 0,
                    last_boundary_flush_at: None,
                    pending_flush: false,
                    flush_phase: "idle".to_string(),
                    flush_phase_updated_at: None,
                    flush_summary_cache: None,
                    recent_explicit_writes: Vec::new(),
                    open_episodes: vec![
                        EpisodeStateRecord {
                            episode_id: format!("{session_id}:1"),
                            start_turn: 1,
                            end_turn: 1,
                            status: EpisodeStatusRecord::FlushPending,
                            task_state: EpisodeTaskStateRecord::Delivered,
                            topic_sketch: "rust vec".to_string(),
                            last_activity_at: Utc::now(),
                        },
                        EpisodeStateRecord {
                            episode_id: format!("{session_id}:2"),
                            start_turn: 2,
                            end_turn: 2,
                            status: EpisodeStatusRecord::Closed,
                            task_state: EpisodeTaskStateRecord::Delivered,
                            topic_sketch: "runpod gpu".to_string(),
                            last_activity_at: Utc::now(),
                        },
                    ],
                })
                .await
                .unwrap();
        }

        let writer = SessionWriter::new(tmp.path());
        writer
            .append_message(session_id, "user", "How do I use Rust Vec push?")
            .await
            .unwrap();
        writer
            .append_message(session_id, "assistant", "Use Vec::push to append items.")
            .await
            .unwrap();
        writer
            .append_message(session_id, "user", "How do I inspect RunPod GPU usage?")
            .await
            .unwrap();
        writer
            .append_message(session_id, "assistant", "Use nvidia-smi on the pod.")
            .await
            .unwrap();

        let (orchestrator, _memory) =
            make_file_backed_test_orchestrator(agent_id, &db_path, tmp.path()).await;
        let session = Session {
            session_key: SessionKey(session_key.to_string()),
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
            created_at: Utc::now(),
            last_active: Utc::now(),
            ttl_seconds: 1800,
            interaction_count: 2,
        };

        let snapshot = orchestrator
            .capture_boundary_flush_snapshot(agent_id, &session, &test_full_agent(agent_id))
            .await
            .expect("snapshot");

        assert_eq!(snapshot.episodes.len(), 1);
        assert_eq!(snapshot.episodes[0].start_turn, 2);
        assert_eq!(snapshot.episodes[0].end_turn, 2);
        assert_eq!(snapshot.turn_count, 2);
    }

    #[tokio::test]
    async fn session_end_schedule_closes_open_episodes_before_flush() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("memory.db");
        let session_id = "session-episode-session-end";
        let session_key = "telegram:tg:chat:episode-session-end";
        let agent_id = "agent-a";

        {
            let store = MemoryStore::open(db_path.to_str().expect("db path")).unwrap();
            store
                .upsert_session(SessionRecord {
                    session_key: session_key.to_string(),
                    session_id: session_id.to_string(),
                    agent_id: agent_id.to_string(),
                    created_at: Utc::now(),
                    last_active: Utc::now(),
                    ttl_seconds: 1800,
                    interaction_count: 2,
                })
                .await
                .unwrap();
            store
                .upsert_session_memory_state(SessionMemoryStateRecord {
                    agent_id: agent_id.to_string(),
                    session_id: session_id.to_string(),
                    session_key: session_key.to_string(),
                    last_flushed_turn: 0,
                    last_boundary_flush_at: None,
                    pending_flush: false,
                    flush_phase: "idle".to_string(),
                    flush_phase_updated_at: None,
                    flush_summary_cache: None,
                    recent_explicit_writes: Vec::new(),
                    open_episodes: vec![
                        EpisodeStateRecord {
                            episode_id: format!("{session_id}:1"),
                            start_turn: 1,
                            end_turn: 1,
                            status: EpisodeStatusRecord::Closed,
                            task_state: EpisodeTaskStateRecord::Delivered,
                            topic_sketch: "rust vec".to_string(),
                            last_activity_at: Utc::now(),
                        },
                        EpisodeStateRecord {
                            episode_id: format!("{session_id}:2"),
                            start_turn: 2,
                            end_turn: 2,
                            status: EpisodeStatusRecord::Open,
                            task_state: EpisodeTaskStateRecord::Exploring,
                            topic_sketch: "runpod gpu".to_string(),
                            last_activity_at: Utc::now(),
                        },
                    ],
                })
                .await
                .unwrap();
        }

        let writer = SessionWriter::new(tmp.path());
        writer
            .append_message(session_id, "user", "How do I use Rust Vec push?")
            .await
            .unwrap();
        writer
            .append_message(session_id, "assistant", "Use Vec::push to append items.")
            .await
            .unwrap();
        writer
            .append_message(session_id, "user", "How do I inspect RunPod GPU usage?")
            .await
            .unwrap();
        writer
            .append_message(session_id, "assistant", "I need to check more details.")
            .await
            .unwrap();

        let (orchestrator, memory) =
            make_file_backed_test_orchestrator(agent_id, &db_path, tmp.path()).await;
        let session = Session {
            session_key: SessionKey(session_key.to_string()),
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
            created_at: Utc::now(),
            last_active: Utc::now(),
            ttl_seconds: 1800,
            interaction_count: 2,
        };
        let agent = test_full_agent(agent_id);
        let view = orchestrator.config_view();

        orchestrator
            .schedule_session_end_flush(view.as_ref(), agent_id, &session, &agent)
            .await;

        let state = memory
            .get_session_memory_state(agent_id, session_id)
            .await
            .unwrap()
            .expect("session memory state");
        assert_eq!(state.open_episodes.len(), 2);
        assert!(
            state
                .open_episodes
                .iter()
                .all(|episode| episode.status != EpisodeStatusRecord::Open),
            "session-end scheduling should close all open episodes before flush"
        );
    }

    #[tokio::test]
    async fn close_open_episodes_for_session_end_marks_remaining_open_episodes_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("memory.db");
        let session_id = "session-episode-4";
        let session_key = "telegram:tg:chat:episode-4";
        let agent_id = "agent-a";

        let memory = Arc::new(MemoryStore::open(db_path.to_str().expect("db path")).unwrap());
        let session = Session {
            session_key: SessionKey(session_key.to_string()),
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
            created_at: Utc::now(),
            last_active: Utc::now(),
            ttl_seconds: 1800,
            interaction_count: 2,
        };

        memory
            .upsert_session_memory_state(SessionMemoryStateRecord {
                agent_id: agent_id.to_string(),
                session_id: session_id.to_string(),
                session_key: session_key.to_string(),
                last_flushed_turn: 0,
                last_boundary_flush_at: None,
                pending_flush: false,
                flush_phase: "idle".to_string(),
                flush_phase_updated_at: None,
                flush_summary_cache: None,
                recent_explicit_writes: Vec::new(),
                open_episodes: vec![
                    EpisodeStateRecord {
                        episode_id: format!("{session_id}:1"),
                        start_turn: 1,
                        end_turn: 1,
                        status: EpisodeStatusRecord::Open,
                        task_state: EpisodeTaskStateRecord::Executing,
                        topic_sketch: "memory".to_string(),
                        last_activity_at: Utc::now(),
                    },
                    EpisodeStateRecord {
                        episode_id: format!("{session_id}:2"),
                        start_turn: 2,
                        end_turn: 2,
                        status: EpisodeStatusRecord::Closed,
                        task_state: EpisodeTaskStateRecord::Delivered,
                        topic_sketch: "runpod".to_string(),
                        last_activity_at: Utc::now(),
                    },
                    EpisodeStateRecord {
                        episode_id: format!("{session_id}:3"),
                        start_turn: 3,
                        end_turn: 3,
                        status: EpisodeStatusRecord::Flushed,
                        task_state: EpisodeTaskStateRecord::Delivered,
                        topic_sketch: "obsidian".to_string(),
                        last_activity_at: Utc::now(),
                    },
                ],
            })
            .await
            .unwrap();

        Orchestrator::close_open_episodes_for_session_end(&memory, agent_id, &session).await;

        let state = memory
            .get_session_memory_state(agent_id, session_id)
            .await
            .unwrap()
            .expect("session memory state");
        assert_eq!(state.open_episodes.len(), 3);
        assert_eq!(state.open_episodes[0].status, EpisodeStatusRecord::Closed);
        assert_eq!(state.open_episodes[1].status, EpisodeStatusRecord::Closed);
        assert_eq!(state.open_episodes[2].status, EpisodeStatusRecord::Flushed);
    }

    #[tokio::test]
    async fn update_closed_episode_flush_state_reverts_pending_episode_on_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("memory.db");
        let session_id = "session-episode-failure";
        let session_key = "telegram:tg:chat:episode-failure";
        let agent_id = "agent-a";

        let memory = Arc::new(MemoryStore::open(db_path.to_str().expect("db path")).unwrap());
        let session = Session {
            session_key: SessionKey(session_key.to_string()),
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
            created_at: Utc::now(),
            last_active: Utc::now(),
            ttl_seconds: 1800,
            interaction_count: 1,
        };

        memory
            .upsert_session_memory_state(SessionMemoryStateRecord {
                agent_id: agent_id.to_string(),
                session_id: session_id.to_string(),
                session_key: session_key.to_string(),
                last_flushed_turn: 0,
                last_boundary_flush_at: None,
                pending_flush: false,
                flush_phase: "idle".to_string(),
                flush_phase_updated_at: None,
                flush_summary_cache: None,
                recent_explicit_writes: Vec::new(),
                open_episodes: vec![EpisodeStateRecord {
                    episode_id: format!("{session_id}:1"),
                    start_turn: 1,
                    end_turn: 1,
                    status: EpisodeStatusRecord::FlushPending,
                    task_state: EpisodeTaskStateRecord::Delivered,
                    topic_sketch: "memory".to_string(),
                    last_activity_at: Utc::now(),
                }],
            })
            .await
            .unwrap();

        Orchestrator::update_closed_episode_flush_state(
            &memory,
            agent_id,
            &session,
            &format!("{session_id}:1"),
            false,
        )
        .await;

        let state = memory
            .get_session_memory_state(agent_id, session_id)
            .await
            .unwrap()
            .expect("session memory state");
        assert_eq!(state.open_episodes.len(), 1);
        assert_eq!(state.open_episodes[0].status, EpisodeStatusRecord::Closed);
    }

    #[tokio::test]
    async fn boundary_flush_snapshot_resumes_from_persisted_checkpoint_after_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("memory.db");
        let session_id = "session-1";
        let session_key = "telegram:tg:chat:1";
        let agent_id = "agent-a";

        {
            let store = MemoryStore::open(db_path.to_str().expect("db path")).unwrap();
            store
                .upsert_session(SessionRecord {
                    session_key: session_key.to_string(),
                    session_id: session_id.to_string(),
                    agent_id: agent_id.to_string(),
                    created_at: Utc::now(),
                    last_active: Utc::now(),
                    ttl_seconds: 1800,
                    interaction_count: 2,
                })
                .await
                .unwrap();
            store
                .upsert_session_memory_state(SessionMemoryStateRecord {
                    agent_id: agent_id.to_string(),
                    session_id: session_id.to_string(),
                    session_key: session_key.to_string(),
                    last_flushed_turn: 1,
                    last_boundary_flush_at: Some(Utc::now()),
                    pending_flush: false,
                    flush_phase: "idle".to_string(),
                    flush_phase_updated_at: None,
                    flush_summary_cache: None,
                    recent_explicit_writes: Vec::new(),
                    open_episodes: Vec::new(),
                })
                .await
                .unwrap();
            drop(store);
        }

        let writer = SessionWriter::new(tmp.path());
        writer
            .append_message(session_id, "user", "first user")
            .await
            .unwrap();
        writer
            .append_message(session_id, "assistant", "first reply")
            .await
            .unwrap();
        writer
            .append_message(session_id, "user", "second user")
            .await
            .unwrap();
        writer
            .append_message(session_id, "assistant", "second reply")
            .await
            .unwrap();

        let (orchestrator, _memory) =
            make_file_backed_test_orchestrator(agent_id, &db_path, tmp.path()).await;
        let session = Session {
            session_key: SessionKey(session_key.to_string()),
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
            created_at: Utc::now(),
            last_active: Utc::now(),
            ttl_seconds: 1800,
            interaction_count: 2,
        };

        let snapshot = orchestrator
            .capture_boundary_flush_snapshot(agent_id, &session, &test_full_agent(agent_id))
            .await
            .expect("snapshot");

        assert_eq!(snapshot.turn_count, 2);
        assert_eq!(snapshot.episodes.len(), 1);
        assert_eq!(snapshot.episodes[0].start_turn, 2);
        assert_eq!(snapshot.episodes[0].end_turn, 2);
        assert_eq!(snapshot.episodes[0].messages.len(), 2);
        assert_eq!(snapshot.episodes[0].messages[0].content, "second user");
        assert_eq!(snapshot.episodes[0].messages[1].content, "second reply");
    }

    #[tokio::test]
    async fn explicit_memory_marker_survives_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let db_path = tmp.path().join("memory.db");
        let session_id = "session-1";
        let session_key = "telegram:tg:chat:1";
        let agent_id = "agent-a";
        let recorded_at = Utc::now();

        {
            let store = MemoryStore::open(db_path.to_str().expect("db path")).unwrap();
            store
                .upsert_session(SessionRecord {
                    session_key: session_key.to_string(),
                    session_id: session_id.to_string(),
                    agent_id: agent_id.to_string(),
                    created_at: Utc::now(),
                    last_active: Utc::now(),
                    ttl_seconds: 1800,
                    interaction_count: 2,
                })
                .await
                .unwrap();
            store
                .upsert_session_memory_state(SessionMemoryStateRecord {
                    agent_id: agent_id.to_string(),
                    session_id: session_id.to_string(),
                    session_key: session_key.to_string(),
                    last_flushed_turn: 0,
                    last_boundary_flush_at: None,
                    pending_flush: false,
                    flush_phase: "idle".to_string(),
                    flush_phase_updated_at: None,
                    flush_summary_cache: None,
                    recent_explicit_writes: vec![RecentExplicitMemoryWrite {
                        turn_index: 1,
                        memory_ref: "fact-1".to_string(),
                        canonical_id: Some("canon-1".to_string()),
                        summary: "User prefers concise replies".to_string(),
                        recorded_at,
                    }],
                    open_episodes: Vec::new(),
                })
                .await
                .unwrap();
            drop(store);
        }

        let writer = SessionWriter::new(tmp.path());
        writer
            .append_message(session_id, "user", "first user")
            .await
            .unwrap();
        writer
            .append_message(session_id, "assistant", "first reply")
            .await
            .unwrap();

        let (orchestrator, _memory) =
            make_file_backed_test_orchestrator(agent_id, &db_path, tmp.path()).await;
        let session = Session {
            session_key: SessionKey(session_key.to_string()),
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
            created_at: Utc::now(),
            last_active: Utc::now(),
            ttl_seconds: 1800,
            interaction_count: 1,
        };

        let snapshot = orchestrator
            .capture_boundary_flush_snapshot(agent_id, &session, &test_full_agent(agent_id))
            .await
            .expect("snapshot");

        assert_eq!(snapshot.recent_explicit_writes.len(), 1);
        let marker = &snapshot.recent_explicit_writes[0];
        assert_eq!(marker.memory_ref, "fact-1");
        assert_eq!(marker.canonical_id.as_deref(), Some("canon-1"));
        assert_eq!(marker.summary, "User prefers concise replies");
        assert_eq!(marker.recorded_at, recorded_at);
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
        let ctx_mgr = crate::context::ContextManager::new(
            router,
            crate::context::ContextConfig::for_model(2_000),
        );

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

    #[test]
    fn history_message_limit_converts_turns() {
        let agent = agent_with_memory_policy(Some(crate::config::MemoryPolicyConfig {
            mode: "session".to_string(),
            write_scope: "session".to_string(),
            idle_minutes: Some(30),
            daily_at_hour: Some(4),
            limit_history_turns: Some(7),
            max_injected_chars: 6000,
            daily_summary_interval: 0,
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
    fn fabricated_response_skipped_in_conversation() {
        // Conversations have a human in the loop — never retry for fabrication
        assert!(!should_retry_fabricated_scheduled_response(
            false,
            0,
            0,
            0,
            "I created the file and saved it.",
        ));
        assert!(!should_retry_fabricated_scheduled_response(
            false,
            0,
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

    #[test]
    fn empty_promise_structural_detects_colon_endings() {
        assert_eq!(
            detect_empty_promise_structural(0, 0, "好，让我把所有内容整合起来："),
            EmptyPromiseVerdict::Structural,
        );
        assert_eq!(
            detect_empty_promise_structural(0, 0, "Here is the compiled content:"),
            EmptyPromiseVerdict::Structural,
        );
        assert_eq!(
            detect_empty_promise_structural(0, 0, "Let me compile everything..."),
            EmptyPromiseVerdict::Structural,
        );
        assert_eq!(
            detect_empty_promise_structural(0, 0, "整理如下——"),
            EmptyPromiseVerdict::Structural,
        );
    }

    #[test]
    fn empty_promise_structural_skips_long_responses() {
        let long_response = "x".repeat(500);
        assert_eq!(
            detect_empty_promise_structural(0, 0, &format!("{long_response}:")),
            EmptyPromiseVerdict::No,
        );
    }

    #[test]
    fn empty_promise_structural_skips_when_tools_called() {
        assert_eq!(
            detect_empty_promise_structural(0, 1, "好，让我整合："),
            EmptyPromiseVerdict::No,
        );
    }

    #[test]
    fn empty_promise_structural_still_detects_after_first_retry() {
        assert_eq!(
            detect_empty_promise_structural(1, 0, "好，让我整合："),
            EmptyPromiseVerdict::Structural,
        );
    }

    #[test]
    fn empty_promise_structural_skips_after_max_retries() {
        assert_eq!(
            detect_empty_promise_structural(2, 0, "好，让我整合："),
            EmptyPromiseVerdict::No,
        );
    }

    #[test]
    fn empty_promise_structural_inconclusive_for_short_no_ending_punctuation() {
        assert_eq!(
            detect_empty_promise_structural(0, 0, "我现在就整理给你"),
            EmptyPromiseVerdict::Inconclusive,
        );
        assert_eq!(
            detect_empty_promise_structural(0, 0, "Sure, I'll do that right away"),
            EmptyPromiseVerdict::Inconclusive,
        );
    }

    #[test]
    fn empty_promise_structural_no_for_complete_sentences() {
        assert_eq!(
            detect_empty_promise_structural(0, 0, "Hello from mock!"),
            EmptyPromiseVerdict::No,
        );
        assert_eq!(
            detect_empty_promise_structural(0, 0, "The answer is 42."),
            EmptyPromiseVerdict::No,
        );
        assert_eq!(
            detect_empty_promise_structural(0, 0, "你确定吗？"),
            EmptyPromiseVerdict::No,
        );
        assert_eq!(
            detect_empty_promise_structural(0, 0, "没问题。"),
            EmptyPromiseVerdict::No,
        );
    }

    fn sample_pdf_bytes(text: &str) -> Vec<u8> {
        use lopdf::content::{Content, Operation};
        use lopdf::{dictionary, Document, Object, Stream};

        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
        });
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! {
                "F1" => font_id,
            },
        });
        let content = Content {
            operations: vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 12.into()]),
                Operation::new("Td", vec![72.into(), 720.into()]),
                Operation::new("Tj", vec![Object::string_literal(text)]),
                Operation::new("ET", vec![]),
            ],
        };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
            "Resources" => resources_id,
            "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
        });

        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1,
            }),
        );
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);

        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn build_attachment_blocks_extracts_pdf_text() {
        use base64::Engine;

        let attachment = Attachment {
            kind: AttachmentKind::Document,
            url: base64::engine::general_purpose::STANDARD
                .encode(sample_pdf_bytes("Lease says landlord pays")),
            mime_type: Some("application/pdf".to_string()),
            file_name: Some("lease.pdf".to_string()),
            size: None,
        };

        let blocks = build_attachment_blocks(&[attachment]);
        assert_eq!(blocks.len(), 1);
        match &blocks[0] {
            ContentBlock::Text { text } => {
                assert!(text.contains("lease.pdf"));
                assert!(text.contains("Lease says landlord pays"));
            }
            other => panic!("expected text block, got {other:?}"),
        }
    }

    #[test]
    fn build_session_text_keeps_binary_attachment_placeholder() {
        let session_text = build_session_text(
            "请看合同",
            &[Attachment {
                kind: AttachmentKind::Document,
                url: "not-base64".to_string(),
                mime_type: Some(
                    "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
                        .to_string(),
                ),
                file_name: Some("lease.docx".to_string()),
                size: None,
            }],
        );

        assert!(session_text.contains("lease.docx"));
        assert!(session_text.contains("binary attachment uploaded"));
    }
}
