use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clawhive_memory::file_store::MemoryFileStore;
use clawhive_memory::search_index::SearchIndex;
use clawhive_memory::{SessionReader, SessionWriter};

use super::access_gate::AccessGate;
use super::workspace::Workspace;

/// Per-agent workspace runtime state: file store, session I/O, search index.
pub(crate) struct AgentWorkspaceState {
    pub workspace: Workspace,
    pub file_store: MemoryFileStore,
    pub session_writer: SessionWriter,
    pub session_reader: SessionReader,
    pub search_index: SearchIndex,
    pub access_gate: Arc<AccessGate>,
}

/// Manages per-agent workspaces with fallback to a default workspace.
pub(crate) struct AgentWorkspaceManager {
    agents: HashMap<String, AgentWorkspaceState>,
    default: AgentWorkspaceState,
}

impl AgentWorkspaceManager {
    pub fn new(agents: HashMap<String, AgentWorkspaceState>, default: AgentWorkspaceState) -> Self {
        Self { agents, default }
    }

    pub fn get(&self, agent_id: &str) -> &AgentWorkspaceState {
        self.agents.get(agent_id).unwrap_or(&self.default)
    }

    pub fn file_store(&self, agent_id: &str) -> &MemoryFileStore {
        &self.get(agent_id).file_store
    }

    pub fn session_writer(&self, agent_id: &str) -> &SessionWriter {
        &self.get(agent_id).session_writer
    }

    pub fn session_reader(&self, agent_id: &str) -> &SessionReader {
        &self.get(agent_id).session_reader
    }

    pub fn search_index(&self, agent_id: &str) -> &SearchIndex {
        &self.get(agent_id).search_index
    }

    pub fn workspace_root(&self, agent_id: &str) -> PathBuf {
        self.get(agent_id).workspace.root().to_path_buf()
    }

    pub fn access_gate(&self, agent_id: &str) -> Arc<AccessGate> {
        self.get(agent_id).access_gate.clone()
    }

    pub fn default_root(&self) -> &Path {
        self.default.workspace.root()
    }

    pub async fn ensure_all(&self) -> anyhow::Result<()> {
        for state in self.agents.values() {
            state.workspace.ensure_dirs().await?;
        }
        self.default.workspace.ensure_dirs().await?;
        Ok(())
    }
}
