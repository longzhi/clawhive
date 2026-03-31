use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{anyhow, Context as _, Result};
use arc_swap::ArcSwap;
use chrono::Utc;
use clawhive_bus::BusPublisher;
use clawhive_memory::dirty_sources::{DirtySourceStore, DIRTY_KIND_DAILY_FILE, DIRTY_KIND_SESSION};
use clawhive_memory::embedding::EmbeddingProvider;
use clawhive_memory::fact_store::FactStore;
use clawhive_memory::file_store::MemoryFileStore;
use clawhive_memory::memory_lineage::generate_canonical_id_with_key;
use clawhive_memory::memory_lineage::MemoryLineageStore;
use clawhive_memory::search_index::SearchIndex;
use clawhive_memory::{
    EpisodeStateRecord, EpisodeStatusRecord, EpisodeTaskStateRecord, MemoryStore,
    RecentExplicitMemoryWrite, SessionEntry, SessionMemoryStateRecord, SessionMessage,
};
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
use super::memory_document::MemoryDocument;
use super::memory_summary::{
    build_summary_prompt, group_daily_candidates, merge_daily_blocks, parse_candidates,
    retain_summary_candidates, SummaryClass,
};

use super::access_gate::{AccessGate, GrantAccessTool, ListAccessTool, RevokeAccessTool};
use super::approval::ApprovalRegistry;
use super::config::{ExecSecurityConfig, FullAgentConfig, SandboxPolicyConfig, SecurityMode};
use super::file_tools::{EditFileTool, ReadFileTool, WriteFileTool};
use super::image_tool::ImageTool;
use super::memory_retrieval::{
    filter_duplicate_chunks_against_facts, infer_memory_routing_bias, is_matching_memory_content,
    search_memory, MemoryHit, MemoryRoutingBias, MemorySourceKind,
};
use super::memory_tools::{MemoryForgetTool, MemoryGetTool, MemorySearchTool, MemoryWriteTool};
use super::persona::Persona;
use super::router::LlmRouter;
use super::schedule_tool::ScheduleTool;
use super::session::{Session, SessionManager, SessionResetReason};
use super::shell_tool::ExecuteCommandTool;
use super::skill::SkillRegistry;
use super::skill_install_state::SkillInstallState;
use super::tool::{ConversationMessage, ToolContext, ToolExecutor, ToolRegistry};
use super::web_fetch_tool::WebFetchTool;
use super::web_search_tool::WebSearchTool;
use super::workspace::Workspace;
use super::workspace_manager::{AgentWorkspaceManager, AgentWorkspaceState};

const SKILL_INSTALL_USAGE_HINT: &str = "请提供 skill 来源路径或 URL。用法: /skill install <source>";
const EPISODE_FLUSH_PENDING_GRACE_SECS: i64 = 30;
const MAX_OPEN_EPISODE_TURNS: u64 = 4;

#[derive(Debug, Clone)]
struct BoundaryFlushEpisode {
    start_turn: u64,
    end_turn: u64,
    messages: Vec<SessionMessage>,
}

#[derive(Debug, Clone)]
struct BoundaryFlushSnapshot {
    episodes: Vec<BoundaryFlushEpisode>,
    turn_count: u64,
    recent_explicit_writes: Vec<RecentExplicitMemoryWrite>,
}

#[derive(Debug, Clone, Default)]
struct ToolLoopMeta {
    successful_tool_calls: usize,
    final_stop_reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EpisodeBoundaryDecision {
    ContinueCurrent,
    CloseCurrentAndOpenNext,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EpisodeTurnDecision {
    task_state: EpisodeTaskStateRecord,
    boundary: EpisodeBoundaryDecision,
}

struct EpisodeTurnInput<'a> {
    turn_index: u64,
    user_text: &'a str,
    assistant_text: &'a str,
    successful_tool_calls: usize,
    final_stop_reason: Option<&'a str>,
}

struct SummaryGenerationRequest<'a> {
    router: &'a LlmRouter,
    file_store: &'a clawhive_memory::file_store::MemoryFileStore,
    memory: &'a Arc<MemoryStore>,
    embedding_provider: &'a Arc<dyn EmbeddingProvider>,
    agent_id: &'a str,
    session: &'a Session,
    agent: &'a FullAgentConfig,
    source: &'a str,
    messages: Vec<SessionMessage>,
    recent_explicit_writes: Vec<RecentExplicitMemoryWrite>,
}

fn normalized_duplicate_key(candidate: &super::memory_summary::SummaryCandidate) -> Option<String> {
    candidate
        .duplicate_key
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn normalized_candidate_fact_type(
    candidate: &super::memory_summary::SummaryCandidate,
) -> &'static str {
    match candidate.fact_type.as_deref().map(str::trim) {
        Some("preference") => "preference",
        Some("decision") => "decision",
        Some("event") => "event",
        Some("person") => "person",
        Some("rule") => "rule",
        _ => "decision",
    }
}

fn boundary_flush_topic_tokens(messages: &[SessionMessage]) -> std::collections::HashSet<String> {
    messages
        .iter()
        .filter(|message| message.role == "user")
        .flat_map(|message| {
            message
                .content
                .split(|ch: char| !ch.is_alphanumeric())
                .map(str::trim)
                .filter(|token| token.len() >= 3)
                .map(|token| token.to_ascii_lowercase())
                .collect::<Vec<_>>()
        })
        .collect()
}

fn boundary_flush_topic_tokens_from_text(text: &str) -> std::collections::HashSet<String> {
    text.split(|ch: char| !ch.is_alphanumeric())
        .map(str::trim)
        .filter(|token| token.len() >= 3)
        .map(|token| token.to_ascii_lowercase())
        .collect()
}

fn build_episode_topic_sketch(text: &str) -> String {
    let mut tokens = boundary_flush_topic_tokens_from_text(text)
        .into_iter()
        .collect::<Vec<_>>();
    tokens.sort();
    tokens.truncate(8);

    if tokens.is_empty() {
        text.trim()
            .chars()
            .take(96)
            .collect::<String>()
            .trim()
            .to_string()
    } else {
        tokens.join(" ")
    }
}

fn boundary_flush_topics_are_related(
    current: &std::collections::HashSet<String>,
    next: &std::collections::HashSet<String>,
) -> bool {
    if current.is_empty() || next.is_empty() {
        return false;
    }

    current.intersection(next).count() >= 2
}

fn infer_episode_task_state(
    assistant_text: &str,
    successful_tool_calls: usize,
    final_stop_reason: Option<&str>,
) -> EpisodeTaskStateRecord {
    if final_stop_reason == Some("length") {
        return EpisodeTaskStateRecord::Executing;
    }

    match detect_empty_promise_structural(0, 0, assistant_text) {
        EmptyPromiseVerdict::Structural => EpisodeTaskStateRecord::Executing,
        EmptyPromiseVerdict::Inconclusive => {
            if successful_tool_calls > 0 {
                EpisodeTaskStateRecord::Delivered
            } else {
                EpisodeTaskStateRecord::Exploring
            }
        }
        EmptyPromiseVerdict::No => EpisodeTaskStateRecord::Delivered,
    }
}

fn decide_episode_turn(
    current_tokens: &std::collections::HashSet<String>,
    next_tokens: &std::collections::HashSet<String>,
    assistant_text: &str,
    successful_tool_calls: usize,
    final_stop_reason: Option<&str>,
    current_task_state: EpisodeTaskStateRecord,
    current_turn_count: u64,
) -> EpisodeTurnDecision {
    let task_state =
        infer_episode_task_state(assistant_text, successful_tool_calls, final_stop_reason);
    let boundary = if current_turn_count >= MAX_OPEN_EPISODE_TURNS {
        EpisodeBoundaryDecision::CloseCurrentAndOpenNext
    } else if boundary_flush_topics_are_related(current_tokens, next_tokens) {
        EpisodeBoundaryDecision::ContinueCurrent
    } else {
        match current_task_state {
            EpisodeTaskStateRecord::Delivered
            | EpisodeTaskStateRecord::Executing
            | EpisodeTaskStateRecord::Exploring => EpisodeBoundaryDecision::CloseCurrentAndOpenNext,
        }
    };

    EpisodeTurnDecision {
        task_state,
        boundary,
    }
}

fn episode_status_ready_for_boundary_flush(
    episode: &EpisodeStateRecord,
    now: chrono::DateTime<Utc>,
) -> bool {
    match episode.status {
        EpisodeStatusRecord::Open | EpisodeStatusRecord::Closed => true,
        EpisodeStatusRecord::Flushed => false,
        EpisodeStatusRecord::FlushPending => {
            now.signed_duration_since(episode.last_activity_at)
                .num_seconds()
                >= EPISODE_FLUSH_PENDING_GRACE_SECS
        }
    }
}

fn collect_unflushed_boundary_turns(
    entries: Vec<SessionEntry>,
    last_flushed_turn: u64,
    history_limit: usize,
) -> Option<(Vec<BoundaryFlushEpisode>, u64)> {
    let mut turn_count = 0_u64;
    let mut include_current_turn = false;
    let mut turns = Vec::new();
    let mut current_turn_messages = Vec::new();

    for entry in entries {
        let SessionEntry::Message {
            message, timestamp, ..
        } = entry
        else {
            continue;
        };

        if message.role == "user" {
            if include_current_turn && !current_turn_messages.is_empty() {
                turns.push(BoundaryFlushEpisode {
                    start_turn: turn_count,
                    end_turn: turn_count,
                    messages: std::mem::take(&mut current_turn_messages),
                });
            }
            turn_count = turn_count.saturating_add(1);
            include_current_turn = turn_count > last_flushed_turn;
        }

        if include_current_turn {
            current_turn_messages.push(SessionMessage {
                timestamp: Some(timestamp),
                ..message
            });
        }
    }

    if include_current_turn && !current_turn_messages.is_empty() {
        turns.push(BoundaryFlushEpisode {
            start_turn: turn_count,
            end_turn: turn_count,
            messages: current_turn_messages,
        });
    }

    if turns.is_empty() {
        return None;
    }

    if turns.len() > history_limit {
        let start = turns.len() - history_limit;
        turns = turns.split_off(start);
    }

    Some((turns, turn_count))
}

fn collect_unflushed_boundary_episodes(
    entries: Vec<SessionEntry>,
    last_flushed_turn: u64,
    history_limit: usize,
) -> Option<(Vec<BoundaryFlushEpisode>, u64)> {
    let (turns, turn_count) =
        collect_unflushed_boundary_turns(entries, last_flushed_turn, history_limit)?;

    const MAX_EPISODE_TURNS: usize = 3;
    const MAX_EPISODE_CHARS: usize = 1600;

    let mut episodes = Vec::new();
    let mut current = BoundaryFlushEpisode {
        start_turn: turns[0].start_turn,
        end_turn: turns[0].end_turn,
        messages: turns[0].messages.clone(),
    };
    let mut current_tokens = boundary_flush_topic_tokens(&current.messages);

    for next in turns.into_iter().skip(1) {
        let next_tokens = boundary_flush_topic_tokens(&next.messages);
        let current_turns = current.end_turn.saturating_sub(current.start_turn) + 1;
        let current_chars = current
            .messages
            .iter()
            .map(|message| message.content.len())
            .sum::<usize>();
        let next_chars = next
            .messages
            .iter()
            .map(|message| message.content.len())
            .sum::<usize>();
        let can_merge = current.end_turn + 1 == next.start_turn
            && current_turns < MAX_EPISODE_TURNS as u64
            && current_chars + next_chars <= MAX_EPISODE_CHARS
            && boundary_flush_topics_are_related(&current_tokens, &next_tokens);

        if can_merge {
            current.end_turn = next.end_turn;
            current.messages.extend(next.messages);
            current_tokens.extend(next_tokens);
        } else {
            episodes.push(current);
            current = BoundaryFlushEpisode {
                start_turn: next.start_turn,
                end_turn: next.end_turn,
                messages: next.messages,
            };
            current_tokens = next_tokens;
        }
    }

    episodes.push(current);

    Some((episodes, turn_count))
}

fn build_boundary_episodes_from_state(
    turns: &[BoundaryFlushEpisode],
    open_episodes: &[EpisodeStateRecord],
) -> Vec<BoundaryFlushEpisode> {
    let mut episodes = Vec::new();
    let now = Utc::now();

    let mut ranges = open_episodes
        .iter()
        .filter(|episode| episode_status_ready_for_boundary_flush(episode, now))
        .cloned()
        .collect::<Vec<_>>();
    ranges.sort_by_key(|episode| (episode.start_turn, episode.end_turn));

    for episode in ranges {
        let mut messages = Vec::new();
        for turn in turns.iter().filter(|turn| {
            turn.start_turn >= episode.start_turn && turn.end_turn <= episode.end_turn
        }) {
            messages.extend(turn.messages.clone());
        }

        if !messages.is_empty() {
            episodes.push(BoundaryFlushEpisode {
                start_turn: episode.start_turn,
                end_turn: episode.end_turn,
                messages,
            });
        }
    }

    episodes
}

fn collect_boundary_episode_for_range(
    entries: Vec<SessionEntry>,
    start_turn: u64,
    end_turn: u64,
) -> Option<BoundaryFlushEpisode> {
    let mut turn_count = 0_u64;
    let mut current_turn_messages = Vec::new();
    let mut current_turn_start = None;
    let mut matched_turns = Vec::new();

    for entry in entries {
        let SessionEntry::Message { message, .. } = entry else {
            continue;
        };

        if message.role == "user" {
            if let Some(turn_start) = current_turn_start.take() {
                matched_turns.push(BoundaryFlushEpisode {
                    start_turn: turn_start,
                    end_turn: turn_start,
                    messages: std::mem::take(&mut current_turn_messages),
                });
            }
            turn_count = turn_count.saturating_add(1);
            current_turn_start = Some(turn_count);
        }

        if current_turn_start.is_some() {
            current_turn_messages.push(message);
        }
    }

    if let Some(turn_start) = current_turn_start.take() {
        matched_turns.push(BoundaryFlushEpisode {
            start_turn: turn_start,
            end_turn: turn_start,
            messages: current_turn_messages,
        });
    }

    let mut messages = Vec::new();
    for turn in matched_turns
        .into_iter()
        .filter(|turn| turn.start_turn >= start_turn && turn.end_turn <= end_turn)
    {
        messages.extend(turn.messages);
    }

    if messages.is_empty() {
        None
    } else {
        Some(BoundaryFlushEpisode {
            start_turn,
            end_turn,
            messages,
        })
    }
}

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
                search_index: SearchIndex::new(memory.db(), agent_id),
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
        }
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

        let mut text = format!(
            "Installed skill '{}' to {} (findings: {}, high-risk: {}).",
            report.skill_name,
            installed.target.display(),
            report.findings.len(),
            installed.high_risk
        );

        let env_vars = report.all_required_env_vars();
        let missing = crate::dotenv::missing_env_vars(&env_vars);
        if !missing.is_empty() {
            text.push_str(&format!(
                "\n\n⚠️ Missing environment variables: {}\nAdd them to ~/.clawhive/.env (KEY=value format) for this skill to work.",
                missing.join(", ")
            ));
        }

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
        if let Err(error) = workspace
            .search_index
            .index_dirty(
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
        }

        let session_text = build_session_text(&inbound.text, &inbound.attachments);

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
        if let Err(e) = workspace
            .session_writer
            .append_message(&session_result.session.session_id, "user", &session_text)
            .await
        {
            tracing::warn!("Failed to write user session entry: {e}");
        } else {
            session_changed = true;
        }
        if let Err(e) = workspace
            .session_writer
            .append_message(
                &session_result.session.session_id,
                "assistant",
                &outbound.text,
            )
            .await
        {
            tracing::warn!("Failed to write assistant session entry: {e}");
        } else {
            session_changed = true;
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
        if let Some(closed_episode) =
            closed_episode.filter(|episode| episode.task_state == EpisodeTaskStateRecord::Delivered)
        {
            self.spawn_closed_episode_flush(
                view.as_ref(),
                agent_id,
                &session_result.session,
                agent,
                closed_episode,
            )
            .await;
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
        let (_resp, final_messages, _tool_attachments, _tool_meta) = self
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
        let attachment_collector: Arc<tokio::sync::Mutex<Vec<Attachment>>> =
            Arc::new(tokio::sync::Mutex::new(Vec::new()));

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

            let _ = ctx_mgr.check_context(&messages);

            let (compacted_messages, compaction_result) =
                ctx_mgr.ensure_within_limits(primary, messages).await?;
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
                let tool_attachments = attachment_collector.lock().await.drain(..).collect();
                return Ok((
                    resp.clone(),
                    messages,
                    tool_attachments,
                    ToolLoopMeta {
                        successful_tool_calls: successful_tool_calls_total,
                        final_stop_reason: resp.stop_reason.clone(),
                    },
                ));
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

            messages.push(LlmMessage {
                role: "user".into(),
                content: tool_results,
            });
            successful_tool_calls_total += successful_tool_calls;

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
            },
        ))
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
        let fresh_session = self
            .session_mgr
            .get_or_create_with_policy(session_key, agent_id, Some(session_reset_policy_for(agent)))
            .await?
            .session;

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

        let (resp, _messages, _tool_attachments, _tool_meta) = self
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
        let mut session_changed = false;
        if let Err(e) = workspace
            .session_writer
            .append_message(&fresh_session.session_id, "system", post_reset_prompt)
            .await
        {
            tracing::warn!("Failed to write post-reset prompt to session: {e}");
        } else {
            session_changed = true;
        }
        if let Err(e) = workspace
            .session_writer
            .append_message(&fresh_session.session_id, "assistant", &reply_text)
            .await
        {
            tracing::warn!("Failed to write assistant session entry: {e}");
        } else {
            session_changed = true;
        }
        if session_changed {
            self.enqueue_dirty_source(
                agent_id,
                DIRTY_KIND_SESSION,
                &fresh_session.session_id,
                "session_reset",
            )
            .await;
            self.drain_dirty_sources(view, agent_id, 8).await;
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

    async fn capture_boundary_flush_snapshot(
        &self,
        agent_id: &str,
        session: &Session,
        agent: &FullAgentConfig,
    ) -> Option<BoundaryFlushSnapshot> {
        let workspace = self.workspace_state_for(agent_id);
        let state = self
            .memory
            .get_session_memory_state(agent_id, &session.session_id)
            .await
            .ok()
            .flatten();
        let entries = workspace
            .session_reader
            .load_all_entries(&session.session_id)
            .await
            .ok()?;
        if entries.is_empty() {
            return None;
        }

        let history_limit = history_message_limit(agent).max(20);
        let last_flushed_turn = state
            .as_ref()
            .map(|state| state.last_flushed_turn)
            .unwrap_or(0);
        let (turns, turn_count) =
            collect_unflushed_boundary_turns(entries.clone(), last_flushed_turn, history_limit)?;
        let state_episodes = state
            .as_ref()
            .map(|state| {
                let now = Utc::now();
                state
                    .open_episodes
                    .iter()
                    .filter(|episode| {
                        episode.end_turn > last_flushed_turn
                            && episode_status_ready_for_boundary_flush(episode, now)
                    })
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let episodes = if state_episodes.is_empty() {
            collect_unflushed_boundary_episodes(entries, last_flushed_turn, history_limit)
                .map(|(episodes, _)| episodes)?
        } else {
            let episodes = build_boundary_episodes_from_state(&turns, &state_episodes);
            if episodes.is_empty() {
                collect_unflushed_boundary_episodes(entries, last_flushed_turn, history_limit)
                    .map(|(episodes, _)| episodes)?
            } else {
                episodes
            }
        };
        Some(BoundaryFlushSnapshot {
            episodes,
            turn_count,
            recent_explicit_writes: state
                .map(|state| state.recent_explicit_writes)
                .unwrap_or_default(),
        })
    }

    async fn close_open_episodes_for_session_end(
        memory: &Arc<MemoryStore>,
        agent_id: &str,
        session: &Session,
    ) {
        let current = memory
            .get_session_memory_state(agent_id, &session.session_id)
            .await
            .unwrap_or(None);
        let Some(mut state) = current else {
            return;
        };

        let mut changed = false;
        let now = Utc::now();
        for episode in &mut state.open_episodes {
            if episode.status == EpisodeStatusRecord::Open {
                episode.status = EpisodeStatusRecord::Closed;
                episode.last_activity_at = now;
                changed = true;
            }
        }

        if changed {
            if let Err(error) = memory.upsert_session_memory_state(state).await {
                tracing::warn!(
                    %error,
                    %agent_id,
                    session_key = %session.session_key.0,
                    session_id = %session.session_id,
                    "Failed to close open episodes for session-end boundary flush"
                );
            }
        }
    }

    async fn update_boundary_flush_state(
        &self,
        agent_id: &str,
        session: &Session,
        turn_count: Option<u64>,
        success: bool,
    ) {
        Self::persist_boundary_flush_state(&self.memory, agent_id, session, turn_count, success)
            .await;
    }

    async fn persist_boundary_flush_state(
        memory: &Arc<MemoryStore>,
        agent_id: &str,
        session: &Session,
        turn_count: Option<u64>,
        success: bool,
    ) {
        let current = memory
            .get_session_memory_state(agent_id, &session.session_id)
            .await
            .unwrap_or(None);
        let mut state = current.unwrap_or(SessionMemoryStateRecord {
            agent_id: agent_id.to_string(),
            session_id: session.session_id.clone(),
            session_key: session.session_key.0.clone(),
            last_flushed_turn: 0,
            last_boundary_flush_at: None,
            pending_flush: false,
            recent_explicit_writes: Vec::new(),
            open_episodes: Vec::new(),
        });

        if success {
            if let Some(turn_count) = turn_count {
                state.last_flushed_turn = turn_count;
                state
                    .recent_explicit_writes
                    .retain(|marker| marker.turn_index > turn_count);
                state
                    .open_episodes
                    .retain(|episode| episode.end_turn > turn_count);
            }
            state.last_boundary_flush_at = Some(Utc::now());
            state.pending_flush = false;
        } else {
            state.pending_flush = true;
        }

        if let Err(error) = memory.upsert_session_memory_state(state).await {
            tracing::warn!(
                %error,
                %agent_id,
                session_key = %session.session_key.0,
                session_id = %session.session_id,
                "Failed to persist session memory state"
            );
        }
    }

    async fn record_session_turn_episode(
        &self,
        agent_id: &str,
        session: &Session,
        turn: EpisodeTurnInput<'_>,
    ) -> Option<EpisodeStateRecord> {
        let current = match self
            .memory
            .get_session_memory_state(agent_id, &session.session_id)
            .await
        {
            Ok(state) => state,
            Err(error) => {
                tracing::warn!(
                    %error,
                    %agent_id,
                    session_id = %session.session_id,
                    "Failed to load session memory state for episode tracking"
                );
                return None;
            }
        };

        let mut state = current.unwrap_or(SessionMemoryStateRecord {
            agent_id: agent_id.to_string(),
            session_id: session.session_id.clone(),
            session_key: session.session_key.0.clone(),
            last_flushed_turn: 0,
            last_boundary_flush_at: None,
            pending_flush: false,
            recent_explicit_writes: Vec::new(),
            open_episodes: Vec::new(),
        });

        let sketch = build_episode_topic_sketch(turn.user_text);
        let current_tokens = boundary_flush_topic_tokens_from_text(&sketch);
        let now = Utc::now();
        let mut closed_episode = None;

        if let Some(last) = state
            .open_episodes
            .iter_mut()
            .rev()
            .find(|episode| episode.status == EpisodeStatusRecord::Open)
        {
            let last_tokens = boundary_flush_topic_tokens_from_text(&last.topic_sketch);
            let decision = decide_episode_turn(
                &last_tokens,
                &current_tokens,
                turn.assistant_text,
                turn.successful_tool_calls,
                turn.final_stop_reason,
                last.task_state.clone(),
                last.end_turn.saturating_sub(last.start_turn) + 1,
            );
            match decision.boundary {
                EpisodeBoundaryDecision::ContinueCurrent => {
                    last.end_turn = turn.turn_index;
                    if !sketch.is_empty() {
                        last.topic_sketch = sketch;
                    }
                    last.task_state = decision.task_state.clone();
                    last.last_activity_at = now;
                }
                EpisodeBoundaryDecision::CloseCurrentAndOpenNext => {
                    last.status = EpisodeStatusRecord::Closed;
                    last.last_activity_at = now;
                    closed_episode = Some(last.clone());
                    state.open_episodes.push(EpisodeStateRecord {
                        episode_id: format!("{}:{}", session.session_id, turn.turn_index),
                        start_turn: turn.turn_index,
                        end_turn: turn.turn_index,
                        status: EpisodeStatusRecord::Open,
                        task_state: decision.task_state.clone(),
                        topic_sketch: sketch,
                        last_activity_at: now,
                    });
                }
            }
        } else {
            let task_state = infer_episode_task_state(
                turn.assistant_text,
                turn.successful_tool_calls,
                turn.final_stop_reason,
            );
            state.open_episodes.push(EpisodeStateRecord {
                episode_id: format!("{}:{}", session.session_id, turn.turn_index),
                start_turn: turn.turn_index,
                end_turn: turn.turn_index,
                status: EpisodeStatusRecord::Open,
                task_state,
                topic_sketch: sketch,
                last_activity_at: now,
            });
        }

        if let Err(error) = self.memory.upsert_session_memory_state(state).await {
            tracing::warn!(
                %error,
                %agent_id,
                session_id = %session.session_id,
                assistant_len = turn.assistant_text.len(),
                "Failed to persist open episode state"
            );
        }

        closed_episode
    }

    async fn capture_closed_episode_snapshot(
        &self,
        agent_id: &str,
        session: &Session,
        episode: &EpisodeStateRecord,
    ) -> Option<(BoundaryFlushEpisode, Vec<RecentExplicitMemoryWrite>)> {
        let workspace = self.workspace_state_for(agent_id);
        let entries = workspace
            .session_reader
            .load_all_entries(&session.session_id)
            .await
            .ok()?;
        let state = self
            .memory
            .get_session_memory_state(agent_id, &session.session_id)
            .await
            .ok()
            .flatten();
        let boundary_episode =
            collect_boundary_episode_for_range(entries, episode.start_turn, episode.end_turn)?;
        Some((
            boundary_episode,
            state
                .map(|state| state.recent_explicit_writes)
                .unwrap_or_default(),
        ))
    }

    async fn update_closed_episode_flush_state(
        memory: &Arc<MemoryStore>,
        agent_id: &str,
        session: &Session,
        episode_id: &str,
        success: bool,
    ) {
        let current = memory
            .get_session_memory_state(agent_id, &session.session_id)
            .await
            .unwrap_or(None);
        let Some(mut state) = current else {
            return;
        };

        if let Some(episode) = state
            .open_episodes
            .iter_mut()
            .find(|episode| episode.episode_id == episode_id)
        {
            if success {
                episode.status = EpisodeStatusRecord::Flushed;
            } else if episode.status == EpisodeStatusRecord::FlushPending {
                episode.status = EpisodeStatusRecord::Closed;
                episode.last_activity_at = Utc::now();
            }
        }

        if success {
            let mut checkpoint = state.last_flushed_turn;
            loop {
                let next = state
                    .open_episodes
                    .iter()
                    .filter(|episode| {
                        episode.status == EpisodeStatusRecord::Flushed
                            && episode.start_turn == checkpoint.saturating_add(1)
                    })
                    .min_by_key(|episode| episode.start_turn)
                    .cloned();

                let Some(next) = next else {
                    break;
                };
                checkpoint = checkpoint.max(next.end_turn);
            }

            if checkpoint > state.last_flushed_turn {
                state.last_flushed_turn = checkpoint;
                state
                    .recent_explicit_writes
                    .retain(|marker| marker.turn_index > checkpoint);
                state.open_episodes.retain(|episode| {
                    !(episode.status == EpisodeStatusRecord::Flushed
                        && episode.end_turn <= checkpoint)
                });
            }
            state.last_boundary_flush_at = Some(Utc::now());
        }

        if let Err(error) = memory.upsert_session_memory_state(state).await {
            tracing::warn!(
                %error,
                %agent_id,
                session_id = %session.session_id,
                episode_id,
                "Failed to persist closed episode flush state"
            );
        }
    }

    async fn spawn_closed_episode_flush(
        &self,
        view: &ConfigView,
        agent_id: &str,
        session: &Session,
        agent: &FullAgentConfig,
        episode: EpisodeStateRecord,
    ) {
        let Some((boundary_episode, recent_explicit_writes)) = self
            .capture_closed_episode_snapshot(agent_id, session, &episode)
            .await
        else {
            return;
        };

        let current = self
            .memory
            .get_session_memory_state(agent_id, &session.session_id)
            .await
            .unwrap_or(None);
        let Some(mut state) = current else {
            return;
        };
        if let Some(current_episode) = state
            .open_episodes
            .iter_mut()
            .find(|current_episode| current_episode.episode_id == episode.episode_id)
        {
            current_episode.status = EpisodeStatusRecord::FlushPending;
            current_episode.last_activity_at = Utc::now();
        }
        if let Err(error) = self.memory.upsert_session_memory_state(state).await {
            tracing::warn!(
                %error,
                %agent_id,
                session_id = %session.session_id,
                episode_id = %episode.episode_id,
                "Failed to persist flush-pending episode state"
            );
            return;
        }

        let router = view.router.clone();
        let file_store = self.file_store_for(agent_id);
        let memory = Arc::clone(&self.memory);
        let embedding_provider = Arc::clone(&view.embedding_provider);
        let agent_id = agent_id.to_string();
        let session = session.clone();
        let agent = agent.clone();
        tokio::spawn(async move {
            let source = format!("episode_closure:{}", episode.episode_id);
            let success =
                Orchestrator::generate_summary_from_messages_static(SummaryGenerationRequest {
                    router: &router,
                    file_store: &file_store,
                    memory: &memory,
                    embedding_provider: &embedding_provider,
                    agent_id: &agent_id,
                    session: &session,
                    agent: &agent,
                    source: &source,
                    messages: boundary_episode.messages,
                    recent_explicit_writes,
                })
                .await;

            Orchestrator::update_closed_episode_flush_state(
                &memory,
                &agent_id,
                &session,
                &episode.episode_id,
                success,
            )
            .await;
        });
    }

    async fn schedule_delivered_episode_flushes_for_session_end(
        &self,
        view: &ConfigView,
        agent_id: &str,
        session: &Session,
        agent: &FullAgentConfig,
    ) {
        let current = self
            .memory
            .get_session_memory_state(agent_id, &session.session_id)
            .await
            .unwrap_or(None);
        let Some(state) = current else {
            return;
        };

        let mut delivered_closed = state
            .open_episodes
            .into_iter()
            .filter(|episode| {
                episode.status == EpisodeStatusRecord::Closed
                    && episode.task_state == EpisodeTaskStateRecord::Delivered
            })
            .collect::<Vec<_>>();
        delivered_closed.sort_by_key(|episode| (episode.start_turn, episode.end_turn));

        for episode in delivered_closed {
            self.spawn_closed_episode_flush(view, agent_id, session, agent, episode)
                .await;
        }
    }

    async fn finalize_stale_boundary_flush(
        memory: &Arc<MemoryStore>,
        agent_id: &str,
        session: &Session,
        file_store: &MemoryFileStore,
        embedding_provider: &Arc<dyn EmbeddingProvider>,
    ) {
        let session_writer = SessionWriter::new(file_store.workspace_dir());
        if let Err(error) = session_writer.archive_session(&session.session_id).await {
            tracing::warn!(
                %error,
                %agent_id,
                session_key = %session.session_key.0,
                session_id = %session.session_id,
                "Failed to archive stale session transcript after boundary flush"
            );
            return;
        }

        let _ = memory
            .delete_session_memory_state(agent_id, &session.session_id)
            .await;

        let dirty = DirtySourceStore::new(memory.db());
        if let Err(error) = dirty
            .enqueue(
                agent_id,
                DIRTY_KIND_SESSION,
                &session.session_id,
                "session_archived_after_reset",
            )
            .await
        {
            tracing::warn!(
                %error,
                %agent_id,
                session_id = %session.session_id,
                "Failed to enqueue archived stale session for reindex"
            );
            return;
        }

        let session_reader = SessionReader::new(file_store.workspace_dir());
        let search_index = SearchIndex::new(memory.db(), agent_id);
        if let Err(error) = search_index
            .index_dirty(file_store, &session_reader, embedding_provider.as_ref(), 8)
            .await
        {
            tracing::warn!(
                %error,
                %agent_id,
                session_id = %session.session_id,
                "Failed to drain archived stale session dirty source"
            );
        }
    }

    async fn recover_pending_boundary_flushes_for_session_key(
        &self,
        view: Arc<ConfigView>,
        agent_id: &str,
        session_key: &SessionKey,
        agent: &FullAgentConfig,
    ) {
        let pending = match self
            .memory
            .list_pending_session_memory_states_for_session_key(agent_id, &session_key.0, 8)
            .await
        {
            Ok(states) => states,
            Err(error) => {
                tracing::warn!(
                    %error,
                    %agent_id,
                    session_key = %session_key.0,
                    "Failed to load pending boundary flush state"
                );
                return;
            }
        };
        if pending.is_empty() {
            return;
        }

        let workspace = self.workspace_state_for(agent_id);
        for state in pending {
            if !workspace
                .session_reader
                .session_exists(&state.session_id)
                .await
            {
                tracing::warn!(
                    %agent_id,
                    session_key = %state.session_key,
                    session_id = %state.session_id,
                    "Pending boundary flush transcript is missing; keeping state for manual repair"
                );
                continue;
            }

            let mut in_flight = self.pending_boundary_recoveries.lock().await;
            if !in_flight.insert(state.session_id.clone()) {
                continue;
            }
            drop(in_flight);

            let recovery_session = Session {
                session_key: SessionKey(state.session_key.clone()),
                session_id: state.session_id.clone(),
                agent_id: agent_id.to_string(),
                created_at: Utc::now(),
                last_active: Utc::now(),
                ttl_seconds: 0,
                interaction_count: 0,
            };

            tracing::info!(
                %agent_id,
                session_key = %recovery_session.session_key.0,
                session_id = %recovery_session.session_id,
                "Recovering pending boundary flush after restart"
            );

            self.schedule_stale_boundary_flush_with_guard(
                view.clone(),
                agent_id,
                &recovery_session,
                agent,
                Some(Arc::clone(&self.pending_boundary_recoveries)),
            )
            .await;
        }
    }

    async fn schedule_stale_boundary_flush(
        &self,
        view: Arc<ConfigView>,
        agent_id: &str,
        session: &Session,
        agent: &FullAgentConfig,
    ) {
        self.schedule_stale_boundary_flush_with_guard(view, agent_id, session, agent, None)
            .await;
    }

    async fn schedule_stale_boundary_flush_with_guard(
        &self,
        view: Arc<ConfigView>,
        agent_id: &str,
        session: &Session,
        agent: &FullAgentConfig,
        recovery_guard: Option<Arc<tokio::sync::Mutex<HashSet<String>>>>,
    ) {
        Self::close_open_episodes_for_session_end(&self.memory, agent_id, session).await;
        self.schedule_delivered_episode_flushes_for_session_end(&view, agent_id, session, agent)
            .await;
        let Some(snapshot) = self
            .capture_boundary_flush_snapshot(agent_id, session, agent)
            .await
        else {
            self.update_boundary_flush_state(agent_id, session, None, true)
                .await;
            if let Some(guard) = recovery_guard {
                let mut in_flight = guard.lock().await;
                in_flight.remove(&session.session_id);
            }
            return;
        };

        Self::persist_boundary_flush_state(&self.memory, agent_id, session, None, false).await;

        let agent_id = agent_id.to_string();
        let session = session.clone();
        let agent = agent.clone();
        let router = view.router.clone();
        let embedding_provider = Arc::clone(&view.embedding_provider);
        let file_store = self.file_store_for(&agent_id);
        let memory = Arc::clone(&self.memory);
        let recovery_session_id = session.session_id.clone();
        tokio::spawn(async move {
            let mut success = true;
            for (idx, episode) in snapshot.episodes.iter().enumerate() {
                let episode_source = format!("fallback_summary:episode:{}", idx + 1);
                let episode_success =
                    Orchestrator::generate_summary_from_messages_static(SummaryGenerationRequest {
                        router: &router,
                        file_store: &file_store,
                        memory: &memory,
                        embedding_provider: &embedding_provider,
                        agent_id: &agent_id,
                        session: &session,
                        agent: &agent,
                        source: &episode_source,
                        messages: episode.messages.clone(),
                        recent_explicit_writes: snapshot.recent_explicit_writes.clone(),
                    })
                    .await;
                success &= episode_success;
            }
            Orchestrator::persist_boundary_flush_state(
                &memory,
                &agent_id,
                &session,
                Some(snapshot.turn_count),
                success,
            )
            .await;

            if success {
                Orchestrator::finalize_stale_boundary_flush(
                    &memory,
                    &agent_id,
                    &session,
                    &file_store,
                    &embedding_provider,
                )
                .await;
            } else {
                tracing::warn!(
                    %agent_id,
                    session_key = %session.session_key.0,
                    session_id = %session.session_id,
                    "Asynchronous boundary flush failed for stale session; keeping transcript in place for retry"
                );
            }

            if let Some(guard) = recovery_guard {
                let mut in_flight = guard.lock().await;
                in_flight.remove(&recovery_session_id);
            }
        });
    }

    async fn run_boundary_flush_snapshot(
        &self,
        view: &ConfigView,
        agent_id: &str,
        session: &Session,
        agent: &FullAgentConfig,
        source: &str,
        snapshot: BoundaryFlushSnapshot,
    ) -> bool {
        let mut handles = Vec::with_capacity(snapshot.episodes.len());
        for (idx, episode) in snapshot.episodes.iter().enumerate() {
            let episode_source = format!("{source}:episode:{}", idx + 1);
            let router = view.router.clone();
            let file_store = self.file_store_for(agent_id);
            let memory = Arc::clone(&self.memory);
            let embedding_provider = Arc::clone(&view.embedding_provider);
            let agent_id_owned = agent_id.to_string();
            let session_clone = session.clone();
            let agent_clone = agent.clone();
            let messages = episode.messages.clone();
            let recent_writes = snapshot.recent_explicit_writes.clone();
            handles.push(tokio::spawn(async move {
                Self::generate_summary_from_messages_static(SummaryGenerationRequest {
                    router: &router,
                    file_store: &file_store,
                    memory: &memory,
                    embedding_provider: &embedding_provider,
                    agent_id: &agent_id_owned,
                    session: &session_clone,
                    agent: &agent_clone,
                    source: &episode_source,
                    messages,
                    recent_explicit_writes: recent_writes,
                })
                .await
            }));
        }
        let mut success = true;
        for handle in handles {
            match handle.await {
                Ok(result) => success &= result,
                Err(error) => {
                    tracing::warn!(%error, "boundary flush episode task panicked");
                    success = false;
                }
            }
        }
        self.update_boundary_flush_state(agent_id, session, Some(snapshot.turn_count), success)
            .await;
        success
    }

    async fn handle_explicit_session_reset(
        &self,
        view: &ConfigView,
        inbound: InboundMessage,
        agent_id: &str,
        agent: &FullAgentConfig,
        session_key: &SessionKey,
        model_hint: Option<&str>,
    ) -> Result<OutboundMessage> {
        let previous_session = self.session_mgr.get(session_key).await?;
        if let Some(previous_session) = previous_session.as_ref() {
            Self::close_open_episodes_for_session_end(&self.memory, agent_id, previous_session)
                .await;
            self.schedule_delivered_episode_flushes_for_session_end(
                view,
                agent_id,
                previous_session,
                agent,
            )
            .await;
            if let Some(snapshot) = self
                .capture_boundary_flush_snapshot(agent_id, previous_session, agent)
                .await
            {
                let _ = self
                    .run_boundary_flush_snapshot(
                        view,
                        agent_id,
                        previous_session,
                        agent,
                        "explicit_reset",
                        snapshot,
                    )
                    .await;
            }
        }

        let _ = self.session_mgr.reset(session_key).await;
        if let Some(previous_session) = previous_session.as_ref() {
            let workspace = self.workspace_state_for(agent_id);
            let _ = workspace
                .session_writer
                .clear_session(&previous_session.session_id)
                .await;
            let _ = self
                .memory
                .delete_session_memory_state(agent_id, &previous_session.session_id)
                .await;
        }

        let post_reset_prompt = super::slash_commands::build_post_reset_prompt(agent_id);
        if let Some(hint) = model_hint {
            tracing::info!("Session reset with model hint: {hint}");
        }

        self.handle_post_reset_flow(
            view,
            inbound,
            agent_id,
            agent,
            session_key,
            &post_reset_prompt,
        )
        .await
    }

    async fn generate_summary_from_messages_static(request: SummaryGenerationRequest<'_>) -> bool {
        let SummaryGenerationRequest {
            router,
            file_store,
            memory,
            embedding_provider,
            agent_id,
            session,
            agent,
            source,
            messages,
            recent_explicit_writes,
        } = request;
        if messages.is_empty() {
            return false;
        }
        let reader = clawhive_memory::session::SessionReader::new(file_store.workspace_dir());

        let today = chrono::Utc::now().date_naive();

        let conversation = messages
            .iter()
            .map(|m| format!("{}: {}", m.role, m.content))
            .collect::<Vec<_>>()
            .join("\n");

        let system = build_summary_prompt();

        let llm_messages = vec![LlmMessage::user(conversation)];

        match router
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
                let Some(candidates) = parse_candidates(&resp.text) else {
                    tracing::warn!(
                        source,
                        raw_len = resp.text.len(),
                        raw_preview = %resp.text.chars().take(300).collect::<String>(),
                        "Failed to parse structured session summary JSON"
                    );
                    return false;
                };
                let retained = retain_summary_candidates(candidates);
                let fact_store = FactStore::new(memory.db());
                let lineage_store = MemoryLineageStore::new(memory.db());
                let mut active_facts = match fact_store.get_active_facts(agent_id).await {
                    Ok(facts) => facts,
                    Err(error) => {
                        tracing::warn!(source, %error, "Failed to load active facts for summary precheck");
                        Vec::new()
                    }
                };
                let existing_memory_items = match file_store.read_long_term().await {
                    Ok(long_term) if !long_term.trim().is_empty() => {
                        let doc = MemoryDocument::parse(&long_term);
                        crate::memory_document::MEMORY_SECTION_ORDER
                            .iter()
                            .flat_map(|heading| doc.section_items(heading))
                            .collect::<Vec<_>>()
                    }
                    Ok(_) => Vec::new(),
                    Err(error) => {
                        tracing::warn!(
                            source,
                            %error,
                            "Failed to load long-term memory items for summary precheck"
                        );
                        Vec::new()
                    }
                };

                for candidate in &retained.facts {
                    let duplicate_key = normalized_duplicate_key(candidate);
                    let already_recorded = super::memory_retrieval::find_matching_fact(
                        &active_facts,
                        &candidate.content,
                    )
                    .is_some()
                        || recent_explicit_writes.iter().any(|marker| {
                            is_matching_memory_content(&marker.summary, &candidate.content)
                        })
                        || existing_memory_items
                            .iter()
                            .any(|item| is_matching_memory_content(item, &candidate.content))
                        || if let Some(duplicate_key) = duplicate_key.as_deref() {
                            let canonical_id = generate_canonical_id_with_key(
                                agent_id,
                                "fact",
                                Some(duplicate_key),
                                &candidate.content,
                            );
                            lineage_store
                                .get_canonical(&canonical_id)
                                .await
                                .ok()
                                .flatten()
                                .is_some()
                        } else {
                            false
                        };
                    if already_recorded {
                        continue;
                    }

                    let now = chrono::Utc::now().to_rfc3339();
                    let fact = clawhive_memory::fact_store::Fact {
                        id: clawhive_memory::fact_store::generate_fact_id(
                            agent_id,
                            &candidate.content,
                        ),
                        agent_id: agent_id.to_string(),
                        content: candidate.content.clone(),
                        fact_type: normalized_candidate_fact_type(candidate).to_string(),
                        importance: f64::from(candidate.importance.clamp(0.0, 1.0)),
                        confidence: 0.9,
                        status: "active".to_string(),
                        occurred_at: None,
                        recorded_at: now.clone(),
                        source_type: "boundary_flush".to_string(),
                        source_session: Some(session.session_id.clone()),
                        access_count: 0,
                        last_accessed: None,
                        superseded_by: None,
                        created_at: now.clone(),
                        updated_at: now,
                    };
                    match fact_store
                        .insert_fact_with_canonical_key(&fact, duplicate_key.as_deref())
                        .await
                    {
                        Ok(()) => {
                            let _ = fact_store.record_add(&fact).await;
                            active_facts.push(fact);
                        }
                        Err(error) => {
                            tracing::warn!(
                                source,
                                content = %candidate.content,
                                error = %error,
                                "Failed to persist boundary fact candidate"
                            );
                        }
                    }
                }

                let mut retained_for_daily = Vec::new();
                for mut candidate in retained.daily.iter().chain(retained.memory.iter()).cloned() {
                    if super::memory_retrieval::find_matching_fact(
                        &active_facts,
                        &candidate.content,
                    )
                    .is_some()
                    {
                        continue;
                    }
                    if recent_explicit_writes.iter().any(|marker| {
                        is_matching_memory_content(&marker.summary, &candidate.content)
                    }) {
                        continue;
                    }
                    if existing_memory_items
                        .iter()
                        .any(|item| is_matching_memory_content(item, &candidate.content))
                    {
                        continue;
                    }

                    let duplicate_hit =
                        if let Some(duplicate_key) = normalized_duplicate_key(&candidate) {
                            let daily_canonical_id = generate_canonical_id_with_key(
                                agent_id,
                                "daily",
                                Some(duplicate_key.as_str()),
                                &candidate.content,
                            );
                            let daily_exists = lineage_store
                                .get_canonical(&daily_canonical_id)
                                .await
                                .ok()
                                .flatten()
                                .is_some();

                            if daily_exists {
                                true
                            } else if candidate.classification == SummaryClass::Memory {
                                let memory_canonical_id = generate_canonical_id_with_key(
                                    agent_id,
                                    "memory",
                                    Some(duplicate_key.as_str()),
                                    &candidate.content,
                                );
                                lineage_store
                                    .get_canonical(&memory_canonical_id)
                                    .await
                                    .ok()
                                    .flatten()
                                    .is_some()
                            } else {
                                false
                            }
                        } else {
                            false
                        };
                    if duplicate_hit {
                        continue;
                    }

                    candidate.classification = SummaryClass::Daily;
                    retained_for_daily.push(candidate);
                }
                if retained_for_daily.is_empty() {
                    tracing::info!(source, "No daily-worthy summary candidates retained");
                    return true;
                }
                let grouped = group_daily_candidates(&retained_for_daily);
                let grouped_for_write = grouped.clone();
                let rendered = match file_store
                    .update_daily(today, move |existing| {
                        Ok(merge_daily_blocks(
                            today,
                            existing.as_deref(),
                            &grouped_for_write,
                        ))
                    })
                    .await
                {
                    Ok(rendered) => rendered,
                    Err(error) => {
                        tracing::warn!(source, %error, "Failed to update daily file");
                        return false;
                    }
                };
                let Some(_rendered) = rendered else {
                    tracing::info!(
                        source,
                        "Structured session summary produced no daily changes"
                    );
                    return true;
                };
                {
                    let relative_path = format!("memory/{}.md", today.format("%Y-%m-%d"));
                    let dirty = DirtySourceStore::new(memory.db());
                    let mut daily_reindexed = false;
                    if let Err(error) = dirty
                        .enqueue(agent_id, DIRTY_KIND_DAILY_FILE, &relative_path, source)
                        .await
                    {
                        tracing::warn!(source, %error, "Failed to enqueue daily dirty source");
                    } else {
                        let search_index = SearchIndex::new(memory.db(), agent_id);
                        if let Err(error) = search_index
                            .index_dirty(file_store, &reader, embedding_provider.as_ref(), 4)
                            .await
                        {
                            tracing::warn!(source, %error, "Failed to drain daily dirty source");
                        } else {
                            daily_reindexed = true;
                        }
                    }

                    let session_path_prefix = format!("sessions/{}#", session.session_id);
                    for candidate in &retained_for_daily {
                        let canonical_key = candidate
                            .duplicate_key
                            .as_deref()
                            .map(str::trim)
                            .filter(|value| !value.is_empty());
                        let canonical = match lineage_store
                            .ensure_canonical_with_key(
                                agent_id,
                                "daily",
                                canonical_key,
                                &candidate.content,
                            )
                            .await
                        {
                            Ok(canonical) => canonical,
                            Err(error) => {
                                tracing::warn!(
                                    source,
                                    content = %candidate.content,
                                    error = %error,
                                    "Failed to ensure daily canonical"
                                );
                                continue;
                            }
                        };

                        if let Err(error) = lineage_store
                            .attach_source(
                                agent_id,
                                &canonical.canonical_id,
                                "daily_section",
                                &format!("{relative_path}#{}", canonical.canonical_id),
                                "summary",
                            )
                            .await
                        {
                            tracing::warn!(
                                source,
                                canonical_id = %canonical.canonical_id,
                                error = %error,
                                "Failed to record daily section lineage"
                            );
                        }

                        if let Err(error) = lineage_store
                            .attach_matching_chunks_by_prefix(
                                agent_id,
                                &canonical.canonical_id,
                                &session_path_prefix,
                                &candidate.content,
                                "raw",
                            )
                            .await
                        {
                            tracing::warn!(
                                source,
                                canonical_id = %canonical.canonical_id,
                                error = %error,
                                "Failed to record session chunk lineage for daily candidate"
                            );
                        }

                        if daily_reindexed {
                            if let Err(error) = lineage_store
                                .attach_matching_chunks(
                                    agent_id,
                                    &canonical.canonical_id,
                                    &relative_path,
                                    &candidate.content,
                                    "summary",
                                )
                                .await
                            {
                                tracing::warn!(
                                    source,
                                    canonical_id = %canonical.canonical_id,
                                    error = %error,
                                    "Failed to record daily chunk lineage for daily candidate"
                                );
                            }
                        }
                    }
                    tracing::info!(
                        source,
                        blocks = grouped.len(),
                        "Wrote structured session summary"
                    );
                    let topics = grouped
                        .iter()
                        .map(|block| block.topic.clone())
                        .collect::<Vec<String>>();
                    memory
                        .record_trace(
                            agent_id,
                            "write",
                            &serde_json::json!({
                                "source": source,
                                "target": format!("memory/{}.md", today.format("%Y-%m-%d")),
                                "topics": topics,
                            })
                            .to_string(),
                            None,
                        )
                        .await;
                }
                true
            }
            Err(e) => {
                tracing::warn!("Failed to generate {source}: {e}");
                false
            }
        }
    }

    async fn try_fallback_summary(
        &self,
        view: &ConfigView,
        agent_id: &str,
        session: &Session,
        agent: &FullAgentConfig,
        reason: SessionResetReason,
    ) {
        let Some(snapshot) = self
            .capture_boundary_flush_snapshot(agent_id, session, agent)
            .await
        else {
            self.update_boundary_flush_state(agent_id, session, None, true)
                .await;
            return;
        };
        let success = self
            .run_boundary_flush_snapshot(
                view,
                agent_id,
                session,
                agent,
                "fallback_summary",
                snapshot,
            )
            .await;
        if matches!(reason, SessionResetReason::Idle | SessionResetReason::Daily) {
            if success {
                let workspace = self.workspace_state_for(agent_id);
                if let Err(error) = workspace
                    .session_writer
                    .archive_session(&session.session_id)
                    .await
                {
                    tracing::warn!(
                        %error,
                        %agent_id,
                        session_key = %session.session_key.0,
                        session_id = %session.session_id,
                        "Failed to archive stale session transcript after boundary flush"
                    );
                } else {
                    let _ = self
                        .memory
                        .delete_session_memory_state(agent_id, &session.session_id)
                        .await;
                    self.enqueue_dirty_source(
                        agent_id,
                        DIRTY_KIND_SESSION,
                        &session.session_id,
                        "session_archived_after_reset",
                    )
                    .await;
                    self.drain_dirty_sources(view, agent_id, 8).await;
                }
            } else {
                tracing::warn!(
                    %agent_id,
                    session_key = %session.session_key.0,
                    session_id = %session.session_id,
                    "Boundary flush failed for stale session; keeping transcript in place to avoid losing retry source"
                );
            }
        } else if !success {
            tracing::warn!(
                %agent_id,
                session_key = %session.session_key.0,
                session_id = %session.session_id,
                "Boundary flush failed before explicit reset"
            );
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

        let fact_store = clawhive_memory::fact_store::FactStore::new(self.memory.db());
        let facts = fact_store
            .get_active_facts(agent_id)
            .await
            .unwrap_or_default();

        let search_start = std::time::Instant::now();
        let results = search_memory(
            &fact_store,
            &self.search_index_for(agent_id),
            view.embedding_provider.as_ref(),
            agent_id,
            query,
            6,
            0.25,
        )
        .await;
        let search_ms = search_start.elapsed().as_millis() as i64;

        match results {
            Ok(results) if !results.is_empty() => {
                let matched_facts = results
                    .iter()
                    .filter_map(|hit| match hit {
                        MemoryHit::Fact(hit) => Some(hit.fact.clone()),
                        MemoryHit::Chunk(_) => None,
                    })
                    .collect::<Vec<_>>();
                let routing_bias = infer_memory_routing_bias(&results);
                let context = if should_use_long_term_fallback(routing_bias, &results) {
                    let long_term = self.file_store_for(agent_id).read_long_term().await?;
                    if long_term.trim().is_empty() {
                        build_memory_context_from_hits(&results, budget)
                    } else {
                        build_memory_context_from_fallback(&matched_facts, &long_term, budget)
                    }
                } else {
                    build_memory_context_from_hits(&results, budget)
                };
                self.memory.record_trace(
                    agent_id,
                    "search",
                    &serde_json::json!({
                        "query": query.chars().take(200).collect::<String>(),
                        "candidates": results.len(),
                        "scores": results.iter().map(|r| format!("{:.2}", r.score())).collect::<Vec<_>>(),
                        "sources": results.iter().map(|r| format!("{:?}", r.source_kind())).collect::<Vec<_>>(),
                    }).to_string(),
                    Some(search_ms),
                ).await;
                self.memory
                    .record_trace(
                        agent_id,
                        "inject",
                        &serde_json::json!({
                            "budget": budget,
                            "injected_chars": context.len(),
                            "result_count": results.len(),
                        })
                        .to_string(),
                        None,
                    )
                    .await;
                Ok(context)
            }
            _ => {
                let fallback = self.file_store_for(agent_id).build_memory_context().await?;
                if !fallback.trim().is_empty() {
                    let context = build_memory_context_from_fallback(&facts, &fallback, budget);
                    self.memory
                        .record_trace(
                            agent_id,
                            "inject",
                            &serde_json::json!({
                                "budget": budget,
                                "injected_chars": context.len(),
                                "source": if facts.is_empty() { "fallback" } else { "facts_plus_fallback" },
                            })
                            .to_string(),
                            Some(search_ms),
                        )
                        .await;
                    return Ok(context);
                }

                if facts.is_empty() {
                    return Ok(String::new());
                }

                let context = build_known_facts_section(&facts, budget);
                self.memory
                    .record_trace(
                        agent_id,
                        "inject",
                        &serde_json::json!({
                            "budget": budget,
                            "injected_chars": context.len(),
                            "source": "facts_only",
                        })
                        .to_string(),
                        Some(search_ms),
                    )
                    .await;
                Ok(context)
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn build_tool_registry(
    file_store: &MemoryFileStore,
    search_index: &SearchIndex,
    memory: &Arc<MemoryStore>,
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
    let fact_store = clawhive_memory::fact_store::FactStore::new(memory.db());
    registry.register(Box::new(MemorySearchTool::new(
        fact_store.clone(),
        search_index.clone(),
        embedding_provider.clone(),
        "default".to_string(),
    )));
    registry.register(Box::new(MemoryGetTool::new(file_store.clone())));
    registry.register(Box::new(MemoryWriteTool::new(
        fact_store.clone(),
        file_store.clone(),
        Arc::clone(memory),
        "default".to_string(),
    )));
    registry.register(Box::new(MemoryForgetTool::new(
        fact_store,
        "default".to_string(),
    )));
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
    registry.register(Box::new(crate::send_file_tool::SendFileTool::new()));
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

fn is_text_mime(mime: &str) -> bool {
    mime.starts_with("text/")
        || mime == "application/json"
        || mime == "application/xml"
        || mime == "application/javascript"
        || mime == "application/x-yaml"
        || mime == "application/yaml"
        || mime == "application/toml"
        || mime == "application/x-sh"
}

fn build_user_content(text: String, attachment_blocks: Vec<ContentBlock>) -> Vec<ContentBlock> {
    let mut content = Vec::with_capacity(1 + attachment_blocks.len());
    if !text.is_empty() {
        content.push(ContentBlock::Text { text });
    }
    content.extend(attachment_blocks);
    content
}

fn build_attachment_blocks(attachments: &[clawhive_schema::Attachment]) -> Vec<ContentBlock> {
    use base64::Engine;

    let mut blocks = Vec::new();
    for a in attachments {
        match a.kind {
            clawhive_schema::AttachmentKind::Image => {
                let media_type = a
                    .mime_type
                    .clone()
                    .unwrap_or_else(|| "image/jpeg".to_string());
                blocks.push(ContentBlock::Image {
                    data: a.url.clone(),
                    media_type,
                });
            }
            _ => {
                let mime = a.mime_type.as_deref().unwrap_or("application/octet-stream");
                if is_text_mime(mime) {
                    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&a.url) {
                        if let Ok(text) = String::from_utf8(bytes) {
                            let label = a.file_name.as_deref().unwrap_or("attachment");
                            blocks.push(ContentBlock::Text {
                                text: format!(
                                    "<attachment name=\"{label}\" type=\"{mime}\">\n{text}\n</attachment>"
                                ),
                            });
                        }
                    }
                }
            }
        }
    }
    blocks
}

fn build_session_text(user_text: &str, attachments: &[clawhive_schema::Attachment]) -> String {
    use base64::Engine;

    let mut parts = vec![user_text.to_string()];
    for a in attachments {
        if matches!(a.kind, clawhive_schema::AttachmentKind::Image) {
            continue;
        }
        let mime = a.mime_type.as_deref().unwrap_or("application/octet-stream");
        if is_text_mime(mime) {
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&a.url) {
                if let Ok(text) = String::from_utf8(bytes) {
                    let label = a.file_name.as_deref().unwrap_or("attachment");
                    parts.push(format!(
                        "<attachment name=\"{label}\" type=\"{mime}\">\n{text}\n</attachment>"
                    ));
                }
            }
        }
    }
    parts.join("\n\n")
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

fn session_reset_policy_for(agent: &FullAgentConfig) -> crate::session::SessionResetPolicy {
    let policy = agent.memory_policy.as_ref();
    let default_policy = crate::session::SessionResetPolicy::default();
    crate::session::SessionResetPolicy {
        idle_minutes: policy
            .and_then(|memory| memory.idle_minutes)
            .or(default_policy.idle_minutes),
        daily_at_hour: policy
            .and_then(|memory| memory.daily_at_hour)
            .or(default_policy.daily_at_hour),
    }
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
    // Conversations have a human in the loop who can see fabricated responses
    // and re-prompt. Keyword matching is too coarse for conversation context
    // (e.g. "已创建" appears naturally when discussing plans).
    if !is_scheduled_task {
        return false;
    }

    // Only retry when the agent has made ZERO tool calls across the entire
    // session. If tools were already called in prior iterations (e.g. the agent
    // ran a pipeline and is now composing a text summary), the current zero-tool
    // iteration is legitimate — not hallucination.
    if retry_count >= 2 || total_tool_calls > 0 || current_tool_calls > 0 {
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

fn detect_empty_promise_structural(
    retry_count: u32,
    current_tool_calls: usize,
    response_text: &str,
) -> EmptyPromiseVerdict {
    if retry_count >= 2 || current_tool_calls > 0 {
        return EmptyPromiseVerdict::No;
    }

    let trimmed = response_text.trim();
    if trimmed.len() >= 500 {
        return EmptyPromiseVerdict::No;
    }

    let ends_with_continuation = trimmed.ends_with(':')
        || trimmed.ends_with('\u{ff1a}') // ：
        || trimmed.ends_with("——")
        || trimmed.ends_with("—")
        || trimmed.ends_with("...")
        || trimmed.ends_with('\u{2026}') // …
        || trimmed.ends_with("\u{2026}\u{2026}"); // ……

    if ends_with_continuation {
        return EmptyPromiseVerdict::Structural;
    }

    let ends_with_sentence_ending = trimmed.ends_with('.')
        || trimmed.ends_with('!')
        || trimmed.ends_with('?')
        || trimmed.ends_with('\u{3002}') // 。
        || trimmed.ends_with('\u{ff01}') // ！
        || trimmed.ends_with('\u{ff1f}') // ？
        || trimmed.ends_with('"')
        || trimmed.ends_with('\u{201d}') // "
        || trimmed.ends_with(')')
        || trimmed.ends_with('\u{ff09}'); // ）

    if ends_with_sentence_ending {
        return EmptyPromiseVerdict::No;
    }

    EmptyPromiseVerdict::Inconclusive
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmptyPromiseVerdict {
    Structural,
    Inconclusive,
    No,
}

async fn detect_empty_promise_by_llm(
    router: &LlmRouter,
    primary: &str,
    fallbacks: &[String],
    response_text: &str,
) -> bool {
    let prompt = format!(
        "An AI assistant produced the following response to a user:\n\
         ---\n{response_text}\n---\n\
         Did the assistant promise or announce that it would produce content \
         (compile, write, generate, summarize, etc.) without actually providing \
         that content in the response? Answer only YES or NO."
    );
    let result = router
        .chat(
            primary,
            fallbacks,
            Some("You are a binary classifier. Answer only YES or NO.".to_string()),
            vec![LlmMessage::user(prompt)],
            16,
        )
        .await;
    match result {
        Ok(resp) => resp.text.trim().to_uppercase().starts_with("YES"),
        Err(e) => {
            tracing::warn!("empty promise LLM detection failed, skipping: {e}");
            false
        }
    }
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

fn build_memory_context_from_hits(hits: &[MemoryHit], budget: usize) -> String {
    if hits.is_empty() || budget == 0 {
        return String::new();
    }

    let routing_bias = infer_memory_routing_bias(hits);
    let facts = hits
        .iter()
        .filter_map(|hit| match hit {
            MemoryHit::Fact(hit) => Some(hit.fact.clone()),
            MemoryHit::Chunk(_) => None,
        })
        .collect::<Vec<_>>();
    let chunks = hits
        .iter()
        .filter_map(|hit| match hit {
            MemoryHit::Chunk(hit) => Some(hit.clone()),
            MemoryHit::Fact(_) => None,
        })
        .collect::<Vec<_>>();
    let chunks = select_chunks_for_context(
        filter_duplicate_chunks_against_facts(chunks, &facts),
        routing_bias,
    );

    let facts_budget = if facts.is_empty() || chunks.is_empty() {
        budget
    } else {
        match routing_bias {
            MemoryRoutingBias::LongTerm => (budget / 4).min(1500),
            MemoryRoutingBias::ShortTerm => (budget / 6).min(900),
            MemoryRoutingBias::Neutral => (budget / 3).min(1500),
        }
    };
    let facts_section = build_known_facts_section(&facts, facts_budget);
    let facts_chars = facts_section.chars().count();
    let remaining_budget = budget.saturating_sub(facts_chars);
    let chunks_section = clamp_to_budget(&chunks, remaining_budget);

    format!("{facts_section}{chunks_section}")
}

fn build_memory_context_from_fallback(
    facts: &[clawhive_memory::fact_store::Fact],
    fallback: &str,
    budget: usize,
) -> String {
    if budget == 0 {
        return String::new();
    }

    let fallback = truncate_text_to_budget(fallback, budget);
    if facts.is_empty() {
        return fallback;
    }
    if fallback.trim().is_empty() {
        return build_known_facts_section(facts, budget);
    }

    let facts_budget = (budget / 3).min(1800);
    let facts_section = build_known_facts_section(facts, facts_budget);
    let remaining_budget = budget.saturating_sub(facts_section.chars().count());
    let fallback_section = truncate_text_to_budget(fallback.trim(), remaining_budget);
    format!("{facts_section}{fallback_section}")
}

fn should_use_long_term_fallback(routing_bias: MemoryRoutingBias, hits: &[MemoryHit]) -> bool {
    routing_bias != MemoryRoutingBias::ShortTerm
        && !hits.iter().any(|hit| {
            matches!(
                hit.source_kind(),
                MemorySourceKind::Fact | MemorySourceKind::LongTerm
            )
        })
}

fn select_chunks_for_context(
    chunks: Vec<clawhive_memory::search_index::SearchResult>,
    routing_bias: MemoryRoutingBias,
) -> Vec<clawhive_memory::search_index::SearchResult> {
    let mut long_term = Vec::new();
    let mut daily = Vec::new();
    let mut session = Vec::new();
    let mut other = Vec::new();

    for chunk in chunks {
        match super::memory_retrieval::classify_chunk_source(&chunk.source, &chunk.path) {
            MemorySourceKind::LongTerm => long_term.push(chunk),
            MemorySourceKind::Daily => daily.push(chunk),
            MemorySourceKind::Session => session.push(chunk),
            _ => other.push(chunk),
        }
    }

    let has_long_term = !long_term.is_empty();
    let mut selected = Vec::new();
    match routing_bias {
        MemoryRoutingBias::LongTerm => {
            selected.extend(long_term.into_iter().take(4));
            if !has_long_term {
                selected.extend(daily.into_iter().take(3));
                selected.extend(session.into_iter().take(1));
            }
        }
        MemoryRoutingBias::ShortTerm => {
            selected.extend(daily.into_iter().take(4));
            selected.extend(session.into_iter().take(2));
            selected.extend(long_term.into_iter().take(1));
        }
        MemoryRoutingBias::Neutral => {
            selected.extend(long_term.into_iter().take(2));
            selected.extend(daily.into_iter().take(2));
            selected.extend(session.into_iter().take(1));
        }
    }
    selected.extend(other);
    selected
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

fn truncate_text_to_budget(text: &str, budget: usize) -> String {
    const TRUNCATED: &str = "\n...[truncated]";

    if budget == 0 {
        return String::new();
    }

    if text.chars().count() <= budget {
        return text.to_string();
    }

    let truncated_chars = TRUNCATED.chars().count();
    if budget <= truncated_chars {
        return text.chars().take(budget).collect();
    }

    let available_chars = budget.saturating_sub(truncated_chars);
    let truncated: String = text.chars().take(available_chars).collect();
    format!("{truncated}{TRUNCATED}")
}

fn build_known_facts_section(facts: &[clawhive_memory::fact_store::Fact], budget: usize) -> String {
    if facts.is_empty() || budget == 0 {
        return String::new();
    }

    let mut section = String::from("## Known Facts\n\n");
    for fact in facts {
        section.push_str(&format!("- [{}] {}\n", fact.fact_type, fact.content));
    }
    section.push('\n');

    truncate_text_to_budget(&section, budget)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use super::*;
    use async_trait::async_trait;
    use chrono::{Duration, TimeZone, Utc};
    use clawhive_bus::EventBus;
    use clawhive_memory::embedding::StubEmbeddingProvider;
    use clawhive_memory::fact_store::FactStore;
    use clawhive_memory::file_store::MemoryFileStore;
    use clawhive_memory::search_index::SearchResult;
    use clawhive_memory::{MemoryStore, SessionEntry, SessionMessage, SessionRecord};
    use clawhive_provider::{ContentBlock, LlmProvider, LlmRequest, LlmResponse, ProviderRegistry};
    use clawhive_runtime::NativeExecutor;
    use clawhive_scheduler::{ScheduleManager, SqliteStore};
    use serde_json::json;
    use tempfile::TempDir;

    use crate::RoutingConfig;

    struct CompactionOnlyProvider;

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
            max_response_tokens: None,
            max_iterations: None,
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
            MemoryHit::Chunk(make_result(
                "MEMORY.md",
                "long_term",
                "长期主线：重构记忆系统，采用分层记忆架构。",
                1.32,
            )),
            MemoryHit::Chunk(make_result(
                "memory/2026-03-29.md",
                "daily",
                "daily 细节：品牌命名还在候选阶段。",
                0.94,
            )),
            MemoryHit::Chunk(make_result(
                "sessions/demo#turn:1-2",
                "session",
                "session 讨论：列出一堆当前缺陷清单。",
                0.81,
            )),
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
            MemoryHit::Chunk(make_result(
                "memory/2026-03-30.md",
                "daily",
                "短期事项：品牌命名还在候选阶段。",
                1.28,
            )),
            MemoryHit::Chunk(make_result(
                "sessions/demo#turn:1",
                "session",
                "session 补充：刚确认了几个候选词。",
                1.04,
            )),
            MemoryHit::Chunk(make_result(
                "MEMORY.md",
                "long_term",
                "长期主线：重构记忆系统。",
                0.83,
            )),
        ];

        let context = build_memory_context_from_hits(&hits, 4_000);

        let daily_pos = context.find("memory/2026-03-30.md").expect("daily hit");
        let long_term_pos = context.find("MEMORY.md").expect("long term hit");
        assert!(daily_pos < long_term_pos);
        assert!(context.contains("品牌命名还在候选阶段"));
    }

    #[test]
    fn should_use_long_term_fallback_only_when_long_term_query_has_no_fact_or_memory_hit() {
        let daily_hit = MemoryHit::Chunk(make_result(
            "memory/2026-03-30.md",
            "daily",
            "短期事项：品牌命名还在候选阶段。",
            1.0,
        ));
        let long_term_hit = MemoryHit::Chunk(make_result(
            "MEMORY.md",
            "long_term",
            "长期主线：重构记忆系统。",
            0.8,
        ));

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
            collect_unflushed_boundary_episodes(entries, 1, 20).expect("snapshot");

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
            collect_unflushed_boundary_episodes(entries, 0, 20).expect("snapshot");

        assert_eq!(turn_count, 2);
        assert_eq!(episodes.len(), 1);
        assert_eq!(episodes[0].start_turn, 1);
        assert_eq!(episodes[0].end_turn, 2);
        assert_eq!(episodes[0].messages.len(), 4);
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
    async fn session_end_schedules_delivered_closed_episodes_before_fallback_snapshot() {
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
                            status: EpisodeStatusRecord::Closed,
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
            .schedule_delivered_episode_flushes_for_session_end(
                view.as_ref(),
                agent_id,
                &session,
                &agent,
            )
            .await;

        let snapshot = orchestrator
            .capture_boundary_flush_snapshot(agent_id, &session, &agent)
            .await
            .expect("snapshot");
        assert_eq!(snapshot.episodes.len(), 1);
        assert_eq!(snapshot.episodes[0].start_turn, 2);
        assert_eq!(snapshot.episodes[0].end_turn, 2);

        let state = memory
            .get_session_memory_state(agent_id, session_id)
            .await
            .unwrap()
            .expect("session memory state");
        assert_eq!(state.open_episodes.len(), 2);
        assert_ne!(state.open_episodes[0].status, EpisodeStatusRecord::Open);
        assert_eq!(state.open_episodes[1].status, EpisodeStatusRecord::Closed);
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
}
