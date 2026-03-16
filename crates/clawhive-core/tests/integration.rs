use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::anyhow;
use async_trait::async_trait;
use clawhive_bus::EventBus;
use clawhive_core::*;
use clawhive_memory::embedding::{EmbeddingProvider, StubEmbeddingProvider};
use clawhive_memory::search_index::SearchIndex;
use clawhive_memory::MemoryStore;
use clawhive_memory::{file_store::MemoryFileStore, SessionReader, SessionWriter};
use clawhive_provider::{
    register_builtin_providers, ContentBlock, LlmProvider, LlmRequest, LlmResponse,
    ProviderRegistry,
};
use clawhive_runtime::NativeExecutor;
use clawhive_scheduler::{ScheduleManager, SqliteStore};
use clawhive_schema::{BusMessage, InboundMessage, SessionKey};
use uuid::Uuid;

struct FailProvider;
struct EchoProvider;
struct ThinkingEchoProvider;
struct TranscriptProvider;
struct MultiSessionSummaryProvider {
    summary_calls: AtomicUsize,
}

#[async_trait]
impl LlmProvider for FailProvider {
    async fn chat(&self, _request: LlmRequest) -> anyhow::Result<LlmResponse> {
        Err(anyhow!("forced failure"))
    }
}

#[async_trait]
impl LlmProvider for EchoProvider {
    async fn chat(&self, request: LlmRequest) -> anyhow::Result<LlmResponse> {
        let text = request
            .messages
            .last()
            .map(|m| m.text())
            .unwrap_or_default();
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
impl LlmProvider for ThinkingEchoProvider {
    async fn chat(&self, _request: LlmRequest) -> anyhow::Result<LlmResponse> {
        let text = "[think] still processing".to_string();
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
impl LlmProvider for TranscriptProvider {
    async fn chat(&self, request: LlmRequest) -> anyhow::Result<LlmResponse> {
        // Include system prompt in output for testing
        let system_part = request
            .system
            .as_ref()
            .map(|s| format!("[system] {}\n\n", s))
            .unwrap_or_default();
        let messages_part = request
            .messages
            .iter()
            .map(|m| format!("[{}] {}", m.role, m.text()))
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

#[async_trait]
impl LlmProvider for MultiSessionSummaryProvider {
    async fn chat(&self, request: LlmRequest) -> anyhow::Result<LlmResponse> {
        let text = if request
            .system
            .as_deref()
            .is_some_and(|system| system.starts_with("Summarize this conversation"))
        {
            let call = self.summary_calls.fetch_add(1, Ordering::SeqCst) + 1;
            format!("- fallback summary {call}")
        } else {
            request
                .messages
                .last()
                .map(|message| format!("reply: {}", message.text()))
                .unwrap_or_else(|| "reply: <empty>".to_string())
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

fn test_inbound(text: &str) -> InboundMessage {
    InboundMessage {
        trace_id: Uuid::new_v4(),
        channel_type: "telegram".into(),
        connector_id: "tg_main".into(),
        conversation_scope: "chat:1".into(),
        user_scope: "user:1".into(),
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

fn test_agent(primary: &str, fallbacks: Vec<&str>) -> AgentConfig {
    AgentConfig {
        agent_id: "clawhive-main".to_string(),
        enabled: true,
        model_policy: ModelPolicy {
            primary: primary.to_string(),
            fallbacks: fallbacks.into_iter().map(|s| s.to_string()).collect(),
            thinking_level: None,
            context_window: None,
        },
    }
}

fn test_full_agent(agent_id: &str, primary: &str, fallbacks: Vec<&str>) -> FullAgentConfig {
    FullAgentConfig {
        agent_id: agent_id.to_string(),
        enabled: true,
        security: SecurityMode::default(),
        identity: None,
        model_policy: ModelPolicy {
            primary: primary.to_string(),
            fallbacks: fallbacks.into_iter().map(|s| s.to_string()).collect(),
            thinking_level: None,
            context_window: None,
        },
        tool_policy: None,
        memory_policy: None,
        sub_agent: None,
        workspace: None,
        heartbeat: None,
        exec_security: None,
        sandbox: None,
    }
}

struct TestToolDeps<'a> {
    publisher: &'a clawhive_bus::BusPublisher,
    workspace_root: &'a std::path::Path,
    schedule_manager: Arc<ScheduleManager>,
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
    let default_agent_id = agents
        .first()
        .map(|agent| agent.agent_id.clone())
        .unwrap_or_else(|| "clawhive-main".to_string());
    let tool_registry = build_tool_registry(
        deps.file_store,
        deps.search_index,
        &deps.embedding_provider,
        deps.workspace_root,
        deps.workspace_root,
        &None,
        deps.publisher,
        deps.schedule_manager,
        None,
        &router,
        &agents,
        &personas,
    );

    ConfigView::new(
        0,
        agents,
        personas,
        RoutingConfig {
            default_agent_id,
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
) -> (Orchestrator, tempfile::TempDir) {
    let tmp = tempfile::TempDir::new().unwrap();
    let router = LlmRouter::new(registry, aliases, vec![]);
    let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
    let bus = EventBus::new(16);
    let publisher = bus.publisher();
    let file_store = MemoryFileStore::new(tmp.path());
    let search_index = SearchIndex::new(memory.db());
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
            file_store: &file_store,
            search_index: &search_index,
            embedding_provider,
        },
    );
    (
        OrchestratorBuilder::new(
            config_view,
            publisher,
            memory,
            Arc::new(NativeExecutor),
            tmp.path().to_path_buf(),
            schedule_manager,
        )
        .build(),
        tmp,
    )
}

#[tokio::test]
async fn orchestrator_uses_search_index_for_memory_context() {
    let mut registry = ProviderRegistry::new();
    registry.register("trace", Arc::new(TranscriptProvider));
    let aliases = HashMap::from([("trace".to_string(), "trace/model".to_string())]);

    let tmp = tempfile::TempDir::new().unwrap();
    let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
    let bus = EventBus::new(16);
    let _session_mgr = SessionManager::new(memory.clone(), 1800);
    let router = LlmRouter::new(registry, aliases, vec![]);
    let agents = vec![test_full_agent("clawhive-main", "trace", vec![])];
    let file_store = MemoryFileStore::new(tmp.path());
    let session_writer = SessionWriter::new(tmp.path());
    let session_reader = SessionReader::new(tmp.path());

    let embedding_provider: Arc<dyn EmbeddingProvider> = Arc::new(StubEmbeddingProvider::new(8));
    let search_index = SearchIndex::new(memory.db());
    let schedule_manager = Arc::new(
        ScheduleManager::new(
            SqliteStore::open(&tmp.path().join("data/scheduler.db")).unwrap(),
            Arc::new(EventBus::new(16)),
        )
        .await
        .unwrap(),
    );
    let memory_text = "# Plans\n\ncobalt migration architecture details";
    file_store.write_long_term(memory_text).await.unwrap();
    search_index
        .index_file(
            "MEMORY.md",
            memory_text,
            "long_term",
            embedding_provider.as_ref(),
        )
        .await
        .unwrap();

    let config_view = build_test_config_view(
        agents,
        router,
        TestToolDeps {
            publisher: &bus.publisher(),
            workspace_root: tmp.path(),
            schedule_manager: Arc::clone(&schedule_manager),
            file_store: &file_store,
            search_index: &search_index,
            embedding_provider: Arc::clone(&embedding_provider),
        },
    );

    let orch = OrchestratorBuilder::new(
        config_view,
        bus.publisher(),
        memory,
        Arc::new(NativeExecutor),
        tmp.path().to_path_buf(),
        schedule_manager,
    )
    .file_store(file_store)
    .session_writer(session_writer)
    .session_reader(session_reader)
    .search_index(search_index)
    .build();

    let out = orch
        .handle_inbound(test_inbound("cobalt architecture"), "clawhive-main")
        .await
        .unwrap();
    assert!(out.text.contains("## Relevant Memory"));
    assert!(out.text.contains("MEMORY.md (score:"));
}

#[tokio::test]
async fn fallback_summary_appends_for_multiple_expired_sessions_on_same_day() {
    let mut registry = ProviderRegistry::new();
    registry.register(
        "summary",
        Arc::new(MultiSessionSummaryProvider {
            summary_calls: AtomicUsize::new(0),
        }),
    );
    let aliases = HashMap::from([("summary".to_string(), "summary/model".to_string())]);

    let tmp = tempfile::TempDir::new().unwrap();
    let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
    let bus = EventBus::new(16);
    let router = LlmRouter::new(registry, aliases, vec![]);
    let mut agent = test_full_agent("clawhive-main", "summary", vec![]);
    agent.workspace = Some(".".to_string());
    let agents = vec![agent];
    let file_store = MemoryFileStore::new(tmp.path());
    let session_writer = SessionWriter::new(tmp.path());
    let session_reader = SessionReader::new(tmp.path());
    let search_index = SearchIndex::new(memory.db());
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
            publisher: &bus.publisher(),
            workspace_root: tmp.path(),
            schedule_manager: Arc::clone(&schedule_manager),
            file_store: &file_store,
            search_index: &search_index,
            embedding_provider: Arc::clone(&embedding_provider),
        },
    );

    let orch = OrchestratorBuilder::new(
        config_view,
        bus.publisher(),
        memory.clone(),
        Arc::new(NativeExecutor),
        tmp.path().to_path_buf(),
        schedule_manager,
    )
    .session_mgr(SessionManager::new(memory, 0))
    .file_store(file_store.clone())
    .session_writer(session_writer)
    .session_reader(session_reader)
    .search_index(search_index)
    .build();

    orch.handle_inbound(test_inbound("first turn"), "clawhive-main")
        .await
        .unwrap();
    orch.handle_inbound(test_inbound("second turn"), "clawhive-main")
        .await
        .unwrap();
    orch.handle_inbound(test_inbound("third turn"), "clawhive-main")
        .await
        .unwrap();

    let daily = file_store
        .read_daily(chrono::Utc::now().date_naive())
        .await
        .unwrap()
        .expect("daily file should exist");

    assert!(daily.contains("- fallback summary 1"));
    assert!(daily.contains("- fallback summary 2"));
}

#[tokio::test]
async fn disabled_agents_are_not_available_to_the_orchestrator() {
    let mut registry = ProviderRegistry::new();
    register_builtin_providers(&mut registry);
    let aliases = HashMap::from([(
        "sonnet".to_string(),
        "anthropic/claude-sonnet-4-5".to_string(),
    )]);
    let mut agent = test_full_agent("clawhive-main", "sonnet", vec![]);
    agent.enabled = false;

    let (orch, _tmp) = make_orchestrator(registry, aliases, vec![agent]).await;
    let err = orch
        .handle_inbound(test_inbound("hello"), "clawhive-main")
        .await
        .expect_err("disabled agents should not be available");

    assert!(err.to_string().contains("agent not found: clawhive-main"));
}

#[tokio::test]
async fn reply_uses_alias_and_success() {
    let mut registry = ProviderRegistry::new();
    register_builtin_providers(&mut registry);

    let aliases = HashMap::from([(
        "sonnet".to_string(),
        "anthropic/claude-sonnet-4-5".to_string(),
    )]);

    let router = LlmRouter::new(registry, aliases, vec![]);
    let agent = test_agent("sonnet", vec![]);

    let out = router.reply(&agent, "hello").await.unwrap();
    assert!(out.contains("stub:anthropic:claude-sonnet-4-5"));
}

#[tokio::test]
async fn reply_fallbacks_to_second_candidate() {
    let mut registry = ProviderRegistry::new();
    registry.register("broken", Arc::new(FailProvider));
    register_builtin_providers(&mut registry);

    let aliases = HashMap::from([
        ("bad".to_string(), "broken/model-a".to_string()),
        (
            "sonnet".to_string(),
            "anthropic/claude-sonnet-4-5".to_string(),
        ),
    ]);

    let router = LlmRouter::new(registry, aliases, vec![]);
    let agent = test_agent("bad", vec!["sonnet"]);

    let out = router.reply(&agent, "fallback test").await.unwrap();
    assert!(out.contains("claude-sonnet-4-5"));
}

#[tokio::test]
async fn unknown_alias_returns_error() {
    let registry = ProviderRegistry::new();
    let router = LlmRouter::new(registry, HashMap::new(), vec![]);
    let agent = test_agent("unknown_alias", vec![]);

    let err = router.reply(&agent, "x").await.err().unwrap();
    let err_str = err.to_string();
    assert!(
        err_str.contains("unknown model alias") || err_str.contains("all model candidates failed"),
        "Unexpected error: {}",
        err_str
    );
}

#[tokio::test]
async fn orchestrator_handles_inbound_to_outbound() {
    let mut registry = ProviderRegistry::new();
    register_builtin_providers(&mut registry);
    let aliases = HashMap::from([(
        "sonnet".to_string(),
        "anthropic/claude-sonnet-4-5".to_string(),
    )]);
    let agents = vec![test_full_agent("clawhive-main", "sonnet", vec![])];
    let (orch, _tmp) = make_orchestrator(registry, aliases, agents).await;

    let out = orch
        .handle_inbound(test_inbound("hello"), "clawhive-main")
        .await
        .unwrap();
    assert!(out.text.contains("stub:anthropic:claude-sonnet-4-5"));
}

#[tokio::test]
async fn tool_use_loop_returns_directly_without_tool_calls() {
    let mut registry = ProviderRegistry::new();
    registry.register("echo", Arc::new(ThinkingEchoProvider));

    let aliases = HashMap::from([("echo".to_string(), "echo/model".to_string())]);
    let agents = vec![test_full_agent("clawhive-main", "echo", vec![])];
    let (orch, _tmp) = make_orchestrator(registry, aliases, agents).await;

    let out = orch
        .handle_inbound(test_inbound("loop"), "clawhive-main")
        .await
        .unwrap();
    assert!(out.text.contains("[think] still processing"));
}

#[tokio::test]
async fn orchestrator_new_with_full_deps() {
    let mut registry = ProviderRegistry::new();
    register_builtin_providers(&mut registry);
    let aliases = HashMap::from([(
        "sonnet".to_string(),
        "anthropic/claude-sonnet-4-5".to_string(),
    )]);
    let agents = vec![test_full_agent("clawhive-main", "sonnet", vec![])];
    let (orch, _tmp) = make_orchestrator(registry, aliases, agents).await;

    let out = orch
        .handle_inbound(test_inbound("hello"), "clawhive-main")
        .await
        .unwrap();
    assert!(out.text.contains("stub:anthropic:claude-sonnet-4-5"));
}

#[tokio::test]
async fn orchestrator_creates_session() {
    let mut registry = ProviderRegistry::new();
    register_builtin_providers(&mut registry);
    let aliases = HashMap::from([(
        "sonnet".to_string(),
        "anthropic/claude-sonnet-4-5".to_string(),
    )]);
    let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
    let bus = EventBus::new(16);
    let agents = vec![test_full_agent("clawhive-main", "sonnet", vec![])];
    let router = LlmRouter::new(registry, aliases, vec![]);
    let tmp = tempfile::TempDir::new().unwrap();
    let schedule_manager = Arc::new(
        ScheduleManager::new(
            SqliteStore::open(&tmp.path().join("data/scheduler.db")).unwrap(),
            Arc::new(EventBus::new(16)),
        )
        .await
        .unwrap(),
    );
    let file_store = MemoryFileStore::new(tmp.path());
    let search_index = SearchIndex::new(memory.db());
    let embedding_provider: Arc<dyn EmbeddingProvider> = Arc::new(StubEmbeddingProvider::new(8));
    let config_view = build_test_config_view(
        agents,
        router,
        TestToolDeps {
            publisher: &bus.publisher(),
            workspace_root: tmp.path(),
            schedule_manager: Arc::clone(&schedule_manager),
            file_store: &file_store,
            search_index: &search_index,
            embedding_provider,
        },
    );
    let orch = OrchestratorBuilder::new(
        config_view,
        bus.publisher(),
        memory.clone(),
        Arc::new(NativeExecutor),
        tmp.path().to_path_buf(),
        schedule_manager,
    )
    .build();

    let inbound = test_inbound("hello");
    let key = SessionKey::from_inbound(&inbound);
    let _ = orch.handle_inbound(inbound, "clawhive-main").await.unwrap();

    let session = memory.get_session(&key.0).await.unwrap();
    assert!(session.is_some());
}

#[tokio::test]
async fn echo_provider_returns_user_input() {
    let mut registry = ProviderRegistry::new();
    registry.register("echo", Arc::new(EchoProvider));
    let aliases = HashMap::from([("echo".to_string(), "echo/model".to_string())]);
    let router = LlmRouter::new(registry, aliases, vec![]);
    let agent = test_agent("echo", vec![]);

    let out = router.reply(&agent, "echo this back").await.unwrap();
    assert_eq!(out, "echo this back");
}

#[tokio::test]
async fn orchestrator_unknown_agent_returns_error() {
    let mut registry = ProviderRegistry::new();
    register_builtin_providers(&mut registry);
    let aliases = HashMap::from([(
        "sonnet".to_string(),
        "anthropic/claude-sonnet-4-5".to_string(),
    )]);
    let agents = vec![test_full_agent("clawhive-main", "sonnet", vec![])];
    let (orch, _tmp) = make_orchestrator(registry, aliases, agents).await;

    let err = orch
        .handle_inbound(test_inbound("hello"), "nonexistent-agent")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("agent not found"));
}

#[tokio::test]
async fn orchestrator_publishes_reply_ready() {
    let mut registry = ProviderRegistry::new();
    register_builtin_providers(&mut registry);
    let aliases = HashMap::from([(
        "sonnet".to_string(),
        "anthropic/claude-sonnet-4-5".to_string(),
    )]);
    let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
    let bus = EventBus::new(16);
    let mut rx = bus.subscribe(clawhive_bus::Topic::ReplyReady).await;
    let agents = vec![test_full_agent("clawhive-main", "sonnet", vec![])];
    let router = LlmRouter::new(registry, aliases, vec![]);
    let tmp = tempfile::TempDir::new().unwrap();
    let schedule_manager = Arc::new(
        ScheduleManager::new(
            SqliteStore::open(&tmp.path().join("data/scheduler.db")).unwrap(),
            Arc::new(EventBus::new(16)),
        )
        .await
        .unwrap(),
    );
    let file_store = MemoryFileStore::new(tmp.path());
    let search_index = SearchIndex::new(memory.db());
    let embedding_provider: Arc<dyn EmbeddingProvider> = Arc::new(StubEmbeddingProvider::new(8));
    let config_view = build_test_config_view(
        agents,
        router,
        TestToolDeps {
            publisher: &bus.publisher(),
            workspace_root: tmp.path(),
            schedule_manager: Arc::clone(&schedule_manager),
            file_store: &file_store,
            search_index: &search_index,
            embedding_provider,
        },
    );
    let orch = OrchestratorBuilder::new(
        config_view,
        bus.publisher(),
        memory,
        Arc::new(NativeExecutor),
        tmp.path().to_path_buf(),
        schedule_manager,
    )
    .build();

    let _ = orch
        .handle_inbound(test_inbound("hello"), "clawhive-main")
        .await
        .unwrap();

    let event = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(event, BusMessage::ReplyReady { .. }));
}

#[tokio::test]
async fn handle_inbound_stream_yields_chunks() {
    use clawhive_provider::StubProvider;
    use tokio_stream::StreamExt;

    let mut registry = ProviderRegistry::new();
    registry.register("stub", Arc::new(StubProvider));
    let aliases = HashMap::from([("stub".to_string(), "stub/model".to_string())]);
    let agents = vec![test_full_agent("clawhive-main", "stub", vec![])];
    let (orch, _tmp) = make_orchestrator(registry, aliases, agents).await;

    let inbound = test_inbound("hello stream");
    let mut stream = orch
        .handle_inbound_stream(inbound, "clawhive-main")
        .await
        .unwrap();

    let mut collected = String::new();
    let mut got_final = false;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.unwrap();
        if chunk.is_final {
            got_final = true;
        } else {
            collected.push_str(&chunk.delta);
        }
    }
    assert!(got_final);
    assert!(!collected.is_empty());
}
