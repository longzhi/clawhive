use std::sync::Arc;

use clawhive_bus::BusPublisher;
use clawhive_memory::file_store::MemoryFileStore;
use clawhive_memory::search_index::SearchIndex;
use clawhive_memory::MemoryStore;
use clawhive_memory::{SessionReader, SessionWriter};
use clawhive_runtime::TaskExecutor;

use crate::approval::ApprovalRegistry;
use crate::config_view::ConfigView;
use crate::session::SessionManager;
use crate::skill::SkillRegistry;

use super::Orchestrator;

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
