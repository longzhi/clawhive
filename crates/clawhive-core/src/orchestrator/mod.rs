use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use arc_swap::ArcSwap;
use clawhive_bus::BusPublisher;
use clawhive_memory::file_store::MemoryFileStore;
use clawhive_memory::search_index::SearchIndex;
use clawhive_memory::MemoryStore;
use clawhive_memory::{SessionReader, SessionWriter};
use clawhive_runtime::TaskExecutor;

use crate::config_view::ConfigView;

use super::language_prefs::LanguagePrefs;

use super::access_gate::AccessGate;
use super::approval::ApprovalRegistry;
use super::session::SessionManager;
use super::skill::SkillRegistry;
use super::skill_install_state::SkillInstallState;
use super::workspace::Workspace;
use super::workspace_manager::{AgentWorkspaceManager, AgentWorkspaceState};

mod attachment;

mod predicates;
pub use predicates::detect_skill_install_intent;

mod tool_registry;
pub use tool_registry::build_tool_registry;

mod episode;

mod memory_context;

mod summary;
pub(crate) use summary::contains_correction_phrase;
use summary::{detect_empty_promise_structural, EmptyPromiseVerdict};

mod skill_commands;

mod tool_loop;

mod builder;
pub use builder::OrchestratorBuilder;

mod inbound;
mod session_helpers;
#[cfg(test)]
mod test_helpers;

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
}
