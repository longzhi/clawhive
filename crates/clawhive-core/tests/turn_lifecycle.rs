use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use clawhive_bus::EventBus;
use clawhive_core::*;
use clawhive_memory::embedding::{EmbeddingProvider, StubEmbeddingProvider};
use clawhive_memory::file_store::MemoryFileStore;
use clawhive_memory::search_index::SearchIndex;
use clawhive_memory::{MemoryStore, SessionReader};
use clawhive_provider::{
    ContentBlock, LlmProvider, LlmRequest, LlmResponse, ProviderError, ProviderRegistry,
};
use clawhive_runtime::NativeExecutor;
use clawhive_scheduler::{ScheduleManager, SqliteStore};
use clawhive_schema::{InboundMessage, SessionKey};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

struct RecordingProvider {
    response_text: String,
    calls: AtomicUsize,
}

impl RecordingProvider {
    fn new(response_text: &str) -> Self {
        Self {
            response_text: response_text.to_string(),
            calls: AtomicUsize::new(0),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl LlmProvider for RecordingProvider {
    async fn chat(&self, _request: LlmRequest) -> Result<LlmResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let text = self.response_text.clone();
        Ok(LlmResponse {
            text: text.clone(),
            content: vec![ContentBlock::Text { text }],
            input_tokens: None,
            output_tokens: None,
            stop_reason: Some("end_turn".into()),
        })
    }
}

struct TranscriptProvider;

#[async_trait]
impl LlmProvider for TranscriptProvider {
    async fn chat(&self, request: LlmRequest) -> Result<LlmResponse, ProviderError> {
        let system_part = request
            .system
            .as_ref()
            .map(|system| format!("[system] {system}\n\n"))
            .unwrap_or_default();
        let messages_part = request
            .messages
            .iter()
            .map(|message| format!("[{}] {}", message.role, message.text()))
            .collect::<Vec<_>>()
            .join("\n\n");
        let text = format!("{system_part}{messages_part}");
        Ok(LlmResponse {
            text: text.clone(),
            content: vec![ContentBlock::Text { text }],
            input_tokens: None,
            output_tokens: None,
            stop_reason: Some("end_turn".into()),
        })
    }
}

struct TestToolDeps<'a> {
    publisher: &'a clawhive_bus::BusPublisher,
    workspace_root: &'a std::path::Path,
    schedule_manager: Arc<ScheduleManager>,
    memory: &'a Arc<MemoryStore>,
    file_store: &'a MemoryFileStore,
    search_index: &'a SearchIndex,
    embedding_provider: Arc<dyn EmbeddingProvider>,
}

fn build_test_config_view(
    agents: Vec<FullAgentConfig>,
    router: LlmRouter,
    deps: TestToolDeps<'_>,
) -> ConfigView {
    let personas = HashMap::new();
    let tool_registry = build_tool_registry(
        deps.file_store,
        deps.search_index,
        deps.memory,
        &deps.embedding_provider,
        deps.workspace_root,
        deps.workspace_root,
        &None,
        deps.publisher,
        deps.schedule_manager,
        vec![],
        &router,
        &agents,
        &personas,
    );

    ConfigView::new(
        0,
        agents,
        personas,
        RoutingConfig {
            default_agent_id: "clawhive-main".to_string(),
            bindings: vec![],
        },
        router,
        tool_registry,
        deps.embedding_provider,
    )
}

async fn make_orchestrator(
    registry: ProviderRegistry,
    aliases: HashMap<String, String>,
    agents: Vec<FullAgentConfig>,
) -> (Orchestrator, tempfile::TempDir, Arc<MemoryStore>) {
    let tmp = tempfile::TempDir::new().unwrap();
    let router = LlmRouter::new(registry, aliases, vec![]);
    let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
    let bus = EventBus::new(16);
    let publisher = bus.publisher();
    let file_store = MemoryFileStore::new(tmp.path());
    let search_index = SearchIndex::new(memory.db(), "clawhive-main");
    let embedding_provider: Arc<dyn EmbeddingProvider> = Arc::new(StubEmbeddingProvider::new(8));
    let schedule_manager = Arc::new(
        ScheduleManager::new(
            SqliteStore::open(&tmp.path().join("data/scheduler.db")).unwrap(),
            Arc::new(EventBus::new(16)),
        )
        .await
        .unwrap(),
    );
    let config_view = build_test_config_view(
        agents,
        router,
        TestToolDeps {
            publisher: &publisher,
            workspace_root: tmp.path(),
            schedule_manager: Arc::clone(&schedule_manager),
            memory: &memory,
            file_store: &file_store,
            search_index: &search_index,
            embedding_provider,
        },
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

fn test_full_agent(agent_id: &str, primary: &str) -> FullAgentConfig {
    FullAgentConfig {
        agent_id: agent_id.to_string(),
        enabled: true,
        security: SecurityMode::default(),
        identity: None,
        model_policy: ModelPolicy {
            primary: primary.to_string(),
            fallbacks: vec![],
            thinking_level: None,
            context_window: None,
            compaction_model: None,
        },
        tool_policy: None,
        memory_policy: None,
        sub_agent: None,
        workspace: Some(".".to_string()),
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

fn test_inbound(text: &str) -> InboundMessage {
    InboundMessage {
        trace_id: Uuid::new_v4(),
        channel_type: "telegram".into(),
        connector_id: "tg_main".into(),
        conversation_scope: "chat:turn-lifecycle".into(),
        user_scope: "user:turn-lifecycle".into(),
        text: text.into(),
        at: chrono::Utc::now(),
        thread_id: None,
        is_mention: false,
        mention_target: None,
        message_id: None,
        attachments: vec![],
        message_source: None,
    }
}

#[tokio::test]
async fn turn_lifecycle_pre_cancelled_token_returns_abort_message() {
    let provider = Arc::new(RecordingProvider::new("model reply should not appear"));
    let mut registry = ProviderRegistry::new();
    registry.register("recording", provider.clone());
    let aliases = HashMap::from([("recording".to_string(), "recording/model".to_string())]);
    let agents = vec![test_full_agent("clawhive-main", "recording")];
    let (orch, _tmp, _memory) = make_orchestrator(registry, aliases, agents).await;
    let cancel_token = CancellationToken::new();
    cancel_token.cancel();

    let outbound = orch
        .handle_inbound(test_inbound("stop now"), "clawhive-main", cancel_token)
        .await
        .unwrap();

    assert_eq!(outbound.text, "[Task stopped by user]");
    assert_eq!(provider.call_count(), 0);
}

#[tokio::test]
async fn turn_lifecycle_pre_cancelled_token_uses_abort_text_not_llm_output() {
    let provider = Arc::new(RecordingProvider::new("normal llm output"));
    let mut registry = ProviderRegistry::new();
    registry.register("recording", provider.clone());
    let aliases = HashMap::from([("recording".to_string(), "recording/model".to_string())]);
    let agents = vec![test_full_agent("clawhive-main", "recording")];
    let (orch, _tmp, _memory) = make_orchestrator(registry, aliases, agents).await;
    let cancel_token = CancellationToken::new();
    cancel_token.cancel();

    let outbound = orch
        .handle_inbound(
            test_inbound("cancel before model"),
            "clawhive-main",
            cancel_token,
        )
        .await
        .unwrap();

    assert!(outbound.text.contains("Task stopped by user"));
    assert!(!outbound.text.contains("normal llm output"));
    assert_eq!(provider.call_count(), 0);
}

#[tokio::test]
async fn turn_lifecycle_normal_token_returns_llm_output() {
    let provider = Arc::new(RecordingProvider::new("normal llm output"));
    let mut registry = ProviderRegistry::new();
    registry.register("recording", provider.clone());
    let aliases = HashMap::from([("recording".to_string(), "recording/model".to_string())]);
    let agents = vec![test_full_agent("clawhive-main", "recording")];
    let (orch, _tmp, _memory) = make_orchestrator(registry, aliases, agents).await;

    let outbound = orch
        .handle_inbound(
            test_inbound("normal turn"),
            "clawhive-main",
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(outbound.text, "normal llm output");
    assert!(provider.call_count() >= 1);
}

#[tokio::test]
async fn turn_lifecycle_abort_message_is_written_to_session_and_visible_next_turn() {
    let mut registry = ProviderRegistry::new();
    registry.register("trace", Arc::new(TranscriptProvider));
    let aliases = HashMap::from([("trace".to_string(), "trace/model".to_string())]);
    let agents = vec![test_full_agent("clawhive-main", "trace")];
    let (orch, tmp, memory) = make_orchestrator(registry, aliases, agents).await;

    let first_inbound = test_inbound("please stop");
    let session_key = SessionKey::from_inbound(&first_inbound);
    let cancel_token = CancellationToken::new();
    cancel_token.cancel();

    let abort_outbound = orch
        .handle_inbound(first_inbound, "clawhive-main", cancel_token)
        .await
        .unwrap();
    assert_eq!(abort_outbound.text, "[Task stopped by user]");

    let session = memory
        .get_session(&session_key.0)
        .await
        .unwrap()
        .expect("session should exist after aborted turn");
    let reader = SessionReader::new(tmp.path());
    let messages = reader
        .load_recent_messages(&session.session_id, 10)
        .await
        .unwrap();

    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, "user");
    assert_eq!(messages[0].content, "please stop");
    assert_eq!(messages[1].role, "assistant");
    assert_eq!(messages[1].content, "[Task stopped by user]");

    let next_outbound = orch
        .handle_inbound(
            test_inbound("what happened last turn?"),
            "clawhive-main",
            CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(next_outbound
        .text
        .contains("[assistant] [Task stopped by user]"));
    assert!(next_outbound
        .text
        .contains("[user] what happened last turn?"));
}
