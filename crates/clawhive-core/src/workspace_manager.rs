use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
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
    workspaces: ArcSwap<HashMap<String, Arc<AgentWorkspaceState>>>,
    default: Arc<AgentWorkspaceState>,
}

impl AgentWorkspaceManager {
    pub fn new(agents: HashMap<String, AgentWorkspaceState>, default: AgentWorkspaceState) -> Self {
        let manager = Self {
            workspaces: ArcSwap::from_pointee(HashMap::new()),
            default: Arc::new(default),
        };
        manager.swap_workspaces(
            agents
                .into_iter()
                .map(|(agent_id, state)| (agent_id, Arc::new(state)))
                .collect(),
        );
        manager
    }

    pub fn get(&self, agent_id: &str) -> Arc<AgentWorkspaceState> {
        self.workspaces
            .load()
            .get(agent_id)
            .cloned()
            .unwrap_or_else(|| self.default.clone())
    }

    pub fn file_store(&self, agent_id: &str) -> MemoryFileStore {
        self.get(agent_id).file_store.clone()
    }

    pub fn search_index(&self, agent_id: &str) -> SearchIndex {
        self.get(agent_id).search_index.clone()
    }

    pub fn workspace_root(&self, agent_id: &str) -> PathBuf {
        self.get(agent_id).workspace.root().to_path_buf()
    }

    pub fn access_gate(&self, agent_id: &str) -> Arc<AccessGate> {
        self.get(agent_id).access_gate.clone()
    }

    pub fn load_full(&self) -> Arc<HashMap<String, Arc<AgentWorkspaceState>>> {
        self.workspaces.load_full()
    }

    pub fn swap_workspaces(&self, new_map: HashMap<String, Arc<AgentWorkspaceState>>) {
        self.workspaces.store(Arc::new(new_map));
    }

    pub fn default_root(&self) -> &Path {
        self.default.workspace.root()
    }

    pub async fn ensure_all(&self) -> anyhow::Result<()> {
        let workspaces = self.workspaces.load();
        for state in workspaces.values() {
            state.workspace.ensure_dirs().await?;
        }
        self.default.workspace.ensure_dirs().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use clawhive_memory::file_store::MemoryFileStore;
    use clawhive_memory::search_index::SearchIndex;
    use clawhive_memory::{MemoryStore, SessionReader, SessionWriter};

    use super::*;

    fn workspace_state(root: &Path) -> AgentWorkspaceState {
        let memory = MemoryStore::open_in_memory().unwrap();
        AgentWorkspaceState {
            workspace: Workspace::new(root),
            file_store: MemoryFileStore::new(root),
            session_writer: SessionWriter::new(root),
            session_reader: SessionReader::new(root),
            search_index: SearchIndex::new(memory.db()),
            access_gate: Arc::new(AccessGate::new(
                root.to_path_buf(),
                root.join("access_policy.json"),
            )),
        }
    }

    #[test]
    fn get_returns_swapped_workspace_snapshot() {
        let tmp = tempfile::TempDir::new().unwrap();
        let original_root = tmp.path().join("original");
        let default_root = tmp.path().join("default");
        let replacement_root = tmp.path().join("replacement");

        let manager = AgentWorkspaceManager::new(
            HashMap::from([(
                "agent-a".to_string(),
                workspace_state(original_root.as_path()),
            )]),
            workspace_state(default_root.as_path()),
        );

        assert_eq!(manager.workspace_root("agent-a"), original_root);

        manager.swap_workspaces(HashMap::from([(
            "agent-a".to_string(),
            Arc::new(workspace_state(replacement_root.as_path())),
        )]));

        assert_eq!(manager.workspace_root("agent-a"), replacement_root);
    }
}
