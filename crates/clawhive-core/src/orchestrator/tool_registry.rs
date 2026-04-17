use std::collections::HashMap;
use std::sync::Arc;

use clawhive_bus::BusPublisher;
use clawhive_memory::embedding::EmbeddingProvider;
use clawhive_memory::file_store::MemoryFileStore;
use clawhive_memory::search_index::SearchIndex;
use clawhive_memory::MemoryStore;

use crate::access_gate::{AccessGate, GrantAccessTool, ListAccessTool, RevokeAccessTool};
use crate::approval::ApprovalRegistry;
use crate::config::{ExecSecurityConfig, FullAgentConfig, SandboxPolicyConfig};
use crate::file_tools::{EditFileTool, ReadFileTool, WriteFileTool};
use crate::image_tool::ImageTool;
use crate::memory_tools::{
    MemoryForgetTool, MemoryGetTool, MemorySearchTool, MemorySupersedeToolDef, MemoryWriteTool,
};
use crate::persona::Persona;
use crate::router::LlmRouter;
use crate::schedule_tool::ScheduleTool;
use crate::shell_tool::ExecuteCommandTool;
use crate::tool::ToolRegistry;
use crate::web_fetch_tool::WebFetchTool;
use crate::web_search::WebSearchTool;

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
    search_providers: Vec<std::sync::Arc<dyn crate::web_search::provider::SearchProvider>>,
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
    registry.register(Box::new(MemorySupersedeToolDef::new(
        fact_store.clone(),
        "default".to_string(),
    )));
    registry.register(Box::new(MemoryForgetTool::new(
        fact_store,
        "default".to_string(),
    )));
    let sub_agent_runner = Arc::new(crate::subagent::SubAgentRunner::new(
        Arc::new(router.clone()),
        agents_map,
        personas,
        3,
        vec![],
        5,
    ));
    registry.register(Box::new(crate::subagent_tool::SubAgentTool::new(
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
        None,
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
    if !search_providers.is_empty() {
        registry.register(Box::new(WebSearchTool::new(search_providers)));
    }
    registry
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use clawhive_bus::EventBus;
    use clawhive_memory::embedding::{EmbeddingProvider, StubEmbeddingProvider};
    use clawhive_memory::file_store::MemoryFileStore;
    use clawhive_memory::search_index::SearchIndex;
    use clawhive_memory::MemoryStore;
    use clawhive_provider::ProviderRegistry;
    use clawhive_scheduler::{ScheduleManager, SqliteStore};

    use crate::orchestrator::test_helpers::agent_with_memory_policy;
    use crate::router::LlmRouter;

    use super::*;

    #[tokio::test]
    async fn build_tool_registry_registers_memory_fact_tools() {
        let dir = tempfile::tempdir().unwrap();
        let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
        let file_store = MemoryFileStore::new(dir.path());
        let search_index = SearchIndex::new(memory.db(), "test-agent");
        let embedding_provider: Arc<dyn EmbeddingProvider> =
            Arc::new(StubEmbeddingProvider::new(8));
        let router = LlmRouter::new(ProviderRegistry::new(), HashMap::new(), vec![]);
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
            vec![],
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
}
