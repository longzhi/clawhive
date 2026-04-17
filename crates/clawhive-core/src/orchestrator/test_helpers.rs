use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::anyhow;
use async_trait::async_trait;
use clawhive_bus::EventBus;
use clawhive_memory::embedding::{EmbeddingProvider, StubEmbeddingProvider};
use clawhive_memory::file_store::MemoryFileStore;
use clawhive_memory::search_index::{SearchIndex, SearchResult};
use clawhive_memory::MemoryStore;
use clawhive_provider::{
    ContentBlock, LlmMessage, LlmProvider, LlmRequest, LlmResponse, ProviderError, ProviderRegistry,
};
use clawhive_runtime::NativeExecutor;
use clawhive_scheduler::{ScheduleManager, SqliteStore};
use serde_json::json;
use tempfile::TempDir;

use crate::config::{FullAgentConfig, SecurityMode};
use crate::config_view::ConfigView;
use crate::RoutingConfig;

use super::{build_tool_registry, Orchestrator, OrchestratorBuilder};

pub(super) struct CompactionOnlyProvider;

pub(super) struct FailingEmbeddingProvider;

pub(super) struct SequenceProvider {
    responses: tokio::sync::Mutex<Vec<LlmResponse>>,
    call_count: AtomicUsize,
}

#[async_trait]
impl LlmProvider for CompactionOnlyProvider {
    async fn chat(&self, request: LlmRequest) -> Result<LlmResponse, ProviderError> {
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
    async fn chat(&self, _request: LlmRequest) -> Result<LlmResponse, ProviderError> {
        self.call_count.fetch_add(1, Ordering::SeqCst);
        let mut responses = self.responses.lock().await;
        if responses.is_empty() {
            return Err(ProviderError::Other(anyhow!("unexpected llm call")));
        }
        Ok(responses.remove(0))
    }
}

impl SequenceProvider {
    pub(super) fn new(responses: Vec<LlmResponse>) -> Self {
        Self {
            responses: tokio::sync::Mutex::new(responses),
            call_count: AtomicUsize::new(0),
        }
    }

    pub(super) fn call_count(&self) -> usize {
        self.call_count.load(Ordering::SeqCst)
    }
}

pub(super) fn agent_with_memory_policy(
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

pub(super) fn test_full_agent(agent_id: &str) -> FullAgentConfig {
    FullAgentConfig {
        agent_id: agent_id.to_string(),
        ..agent_with_memory_policy(None)
    }
}

pub(super) async fn make_memory_tool_orchestrator(
    agent_ids: &[&str],
) -> (Orchestrator, TempDir, Arc<MemoryStore>) {
    let tmp = tempfile::tempdir().unwrap();
    let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
    let bus = EventBus::new(16);
    let publisher = bus.publisher();
    let file_store = MemoryFileStore::new(tmp.path());
    let search_index = SearchIndex::new(memory.db(), "default");
    let embedding_provider: Arc<dyn EmbeddingProvider> = Arc::new(StubEmbeddingProvider::new(8));
    let schedule_manager = Arc::new(
        ScheduleManager::new(
            SqliteStore::open(&tmp.path().join("data/scheduler.db")).unwrap(),
            Arc::new(EventBus::new(16)),
        )
        .await
        .unwrap(),
    );
    let router = crate::router::LlmRouter::new(ProviderRegistry::new(), HashMap::new(), vec![]);
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
        vec![],
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

pub(super) async fn make_tool_loop_test_orchestrator(
    provider: Arc<dyn LlmProvider>,
    max_iterations: Option<u32>,
) -> (Orchestrator, TempDir, Arc<MemoryStore>) {
    let tmp = tempfile::tempdir().unwrap();
    let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
    let bus = EventBus::new(16);
    let publisher = bus.publisher();
    let file_store = MemoryFileStore::new(tmp.path());
    let search_index = SearchIndex::new(memory.db(), "agent-a");
    let embedding_provider: Arc<dyn EmbeddingProvider> = Arc::new(StubEmbeddingProvider::new(8));
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
    let router = crate::router::LlmRouter::new(
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
        vec![],
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

pub(super) async fn make_file_backed_test_orchestrator(
    agent_id: &str,
    db_path: &std::path::Path,
    workspace_root: &std::path::Path,
) -> (Orchestrator, Arc<MemoryStore>) {
    let memory = Arc::new(MemoryStore::open(db_path.to_str().expect("db path")).unwrap());
    let bus = EventBus::new(16);
    let publisher = bus.publisher();
    let file_store = MemoryFileStore::new(workspace_root);
    let search_index = SearchIndex::new(memory.db(), agent_id);
    let embedding_provider: Arc<dyn EmbeddingProvider> = Arc::new(StubEmbeddingProvider::new(8));
    let schedule_manager = Arc::new(
        ScheduleManager::new(
            SqliteStore::open(&workspace_root.join("data/scheduler.db")).unwrap(),
            Arc::new(EventBus::new(16)),
        )
        .await
        .unwrap(),
    );
    let router = crate::router::LlmRouter::new(ProviderRegistry::new(), HashMap::new(), vec![]);
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
        vec![],
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

pub(super) fn assistant_with_tool_use(id: &str) -> LlmMessage {
    LlmMessage {
        role: "assistant".to_string(),
        content: vec![ContentBlock::ToolUse {
            id: id.to_string(),
            name: "read_file".to_string(),
            input: json!({"filePath": "/tmp/demo"}),
        }],
    }
}

pub(super) fn user_with_tool_result(id: &str) -> LlmMessage {
    LlmMessage {
        role: "user".to_string(),
        content: vec![ContentBlock::ToolResult {
            tool_use_id: id.to_string(),
            content: "ok".to_string(),
            is_error: false,
        }],
    }
}

pub(super) fn message_roles(messages: &[LlmMessage]) -> Vec<&str> {
    messages
        .iter()
        .map(|message| message.role.as_str())
        .collect()
}

pub(super) fn make_result(path: &str, source: &str, text: &str, score: f64) -> SearchResult {
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

pub(super) fn llm_text_response(text: &str, stop_reason: &str) -> LlmResponse {
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

pub(super) fn llm_tool_use_response(id: &str, name: &str, input: serde_json::Value) -> LlmResponse {
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

pub(super) async fn wait_for_call_count(provider: &SequenceProvider, expected: usize) {
    for _ in 0..50 {
        if provider.call_count() >= expected {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("provider did not reach call count {expected}");
}
