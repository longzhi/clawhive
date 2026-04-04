use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::anyhow;
use async_trait::async_trait;
use clawhive_bus::EventBus;
use clawhive_core::*;
use clawhive_memory::embedding::{EmbeddingProvider, StubEmbeddingProvider};
use clawhive_memory::fact_store::{generate_fact_id, Fact, FactStore};
use clawhive_memory::memory_lineage::MemoryLineageStore;
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
struct SlowStructuredDailySummaryProvider;
struct StructuredDailySummaryProvider;
struct MemoryOnlySummaryProvider;
struct DuplicateKeyRewriteSummaryProvider {
    summary_calls: AtomicUsize,
}
struct StructuredPromotionSummaryProvider;
struct PromiseThenSummaryProvider;

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

#[async_trait]
impl LlmProvider for StructuredDailySummaryProvider {
    async fn chat(&self, request: LlmRequest) -> anyhow::Result<LlmResponse> {
        let text = if request
            .system
            .as_deref()
            .is_some_and(|system| system.starts_with("Summarize this conversation"))
        {
            r#"[{"content":"first turn","classification":"daily","topic":"General","importance":0.8,"duplicate_key":"first-turn"}]"#
                .to_string()
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

#[async_trait]
impl LlmProvider for SlowStructuredDailySummaryProvider {
    async fn chat(&self, request: LlmRequest) -> anyhow::Result<LlmResponse> {
        let text = if request
            .system
            .as_deref()
            .is_some_and(|system| system.starts_with("Summarize this conversation"))
        {
            tokio::time::sleep(Duration::from_millis(400)).await;
            r#"[{"content":"first turn","classification":"daily","topic":"General","importance":0.8,"duplicate_key":"first-turn"}]"#
                .to_string()
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

#[async_trait]
impl LlmProvider for MemoryOnlySummaryProvider {
    async fn chat(&self, request: LlmRequest) -> anyhow::Result<LlmResponse> {
        let text = if request
            .system
            .as_deref()
            .is_some_and(|system| system.starts_with("Summarize this conversation"))
        {
            r#"[{"content":"Memory refactor is now section-based","classification":"memory","topic":"Architecture","importance":0.8,"duplicate_key":"memory-refactor"}]"#
                .to_string()
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

#[async_trait]
impl LlmProvider for DuplicateKeyRewriteSummaryProvider {
    async fn chat(&self, request: LlmRequest) -> anyhow::Result<LlmResponse> {
        let text = if request
            .system
            .as_deref()
            .is_some_and(|system| system.starts_with("Summarize this conversation"))
        {
            match self.summary_calls.fetch_add(1, Ordering::SeqCst) {
                0 => {
                    r#"[{"content":"Memory refactor is now section-based","classification":"memory","topic":"Architecture","importance":0.8,"duplicate_key":"memory-refactor"}]"#
                }
                _ => {
                    r#"[{"content":"Memory redesign now uses section-based consolidation","classification":"memory","topic":"Architecture","importance":0.8,"duplicate_key":"memory-refactor"}]"#
                }
            }
            .to_string()
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

#[async_trait]
impl LlmProvider for StructuredPromotionSummaryProvider {
    async fn chat(&self, request: LlmRequest) -> anyhow::Result<LlmResponse> {
        let text = if request
            .system
            .as_deref()
            .is_some_and(|system| system.starts_with("Summarize this conversation"))
        {
            r#"[{"content":"User prefers Chinese replies","classification":"fact","topic":"Preferences","importance":0.9,"fact_type":"preference","duplicate_key":"reply-lang"},{"content":"Memory refactor is now section-based","classification":"memory","topic":"Architecture","importance":0.8,"duplicate_key":"memory-refactor"}]"#
                .to_string()
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

#[async_trait]
impl LlmProvider for PromiseThenSummaryProvider {
    async fn chat(&self, request: LlmRequest) -> anyhow::Result<LlmResponse> {
        let text = if request
            .system
            .as_deref()
            .is_some_and(|system| system.starts_with("Summarize this conversation"))
        {
            r#"[{"content":"first turn","classification":"daily","topic":"General","importance":0.8,"duplicate_key":"first-turn"}]"#
                .to_string()
        } else {
            "好，让我把所有内容整合起来：".to_string()
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

async fn wait_until<F, Fut>(timeout: Duration, mut predicate: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + timeout;
    loop {
        if predicate().await {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "condition not met within {:?}",
            timeout
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
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
        max_response_tokens: None,
        max_iterations: None,
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
    let default_agent_id = agents
        .first()
        .map(|agent| agent.agent_id.clone())
        .unwrap_or_else(|| "clawhive-main".to_string());
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
    let search_index = SearchIndex::new(memory.db(), "test-agent");
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

    let embedding_provider: Arc<dyn EmbeddingProvider> = Arc::new(StubEmbeddingProvider::new(8));
    let search_index = SearchIndex::new(memory.db(), "clawhive-main");
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
            memory: &memory,
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
    .session_reader(SessionReader::new(tmp.path()))
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
async fn orchestrator_injects_active_facts_into_memory_context() {
    let mut registry = ProviderRegistry::new();
    registry.register("trace", Arc::new(TranscriptProvider));
    let aliases = HashMap::from([("trace".to_string(), "trace/model".to_string())]);

    let tmp = tempfile::TempDir::new().unwrap();
    let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
    let bus = EventBus::new(16);
    let router = LlmRouter::new(registry, aliases, vec![]);
    let mut agent = test_full_agent("clawhive-main", "trace", vec![]);
    agent.workspace = Some(".".to_string());
    let agents = vec![agent];
    let file_store = MemoryFileStore::new(tmp.path());
    let session_writer = SessionWriter::new(tmp.path());
    let search_index = SearchIndex::new(memory.db(), "test-agent");
    let embedding_provider: Arc<dyn EmbeddingProvider> = Arc::new(StubEmbeddingProvider::new(8));
    let schedule_manager = Arc::new(
        ScheduleManager::new(
            SqliteStore::open(&tmp.path().join("data/scheduler.db")).unwrap(),
            Arc::new(EventBus::new(16)),
        )
        .await
        .unwrap(),
    );
    let fact_store = FactStore::new(memory.db());
    let now = chrono::Utc::now().to_rfc3339();
    let fact = Fact {
        id: generate_fact_id("clawhive-main", "User prefers dark mode"),
        agent_id: "clawhive-main".to_string(),
        content: "User prefers dark mode".to_string(),
        fact_type: "preference".to_string(),
        importance: 0.8,
        confidence: 1.0,
        status: "active".to_string(),
        occurred_at: None,
        recorded_at: now.clone(),
        source_type: "test".to_string(),
        source_session: None,
        access_count: 0,
        last_accessed: None,
        superseded_by: None,
        salience: 70,
        supersede_reason: None,
        affect: "neutral".to_string(),
        affect_intensity: 0.0,
        created_at: now.clone(),
        updated_at: now,
    };
    fact_store.insert_fact(&fact).await.unwrap();
    fact_store.record_add(&fact).await.unwrap();

    let config_view = build_test_config_view(
        agents,
        router,
        TestToolDeps {
            publisher: &bus.publisher(),
            workspace_root: tmp.path(),
            schedule_manager: Arc::clone(&schedule_manager),
            memory: &memory,
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
    .session_reader(SessionReader::new(tmp.path()))
    .search_index(search_index)
    .build();

    let out = orch
        .handle_inbound(test_inbound("what do you know about me?"), "clawhive-main")
        .await
        .unwrap();

    assert!(out.text.contains("## Known Facts"), "{}", out.text);
    assert!(out.text.contains("- [preference] User prefers dark mode"));
}

#[tokio::test]
async fn orchestrator_prefers_long_term_memory_and_can_drop_session_noise_in_prompt_context() {
    let mut registry = ProviderRegistry::new();
    registry.register("trace", Arc::new(TranscriptProvider));
    let aliases = HashMap::from([("trace".to_string(), "trace/model".to_string())]);

    let tmp = tempfile::TempDir::new().unwrap();
    let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
    let bus = EventBus::new(16);
    let router = LlmRouter::new(registry, aliases, vec![]);
    let agents = vec![test_full_agent("clawhive-main", "trace", vec![])];
    let file_store = MemoryFileStore::new(tmp.path());
    let session_writer = SessionWriter::new(tmp.path());
    let _session_reader = SessionReader::new(tmp.path());
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

    search_index
        .index_file(
            "MEMORY.md",
            "# Long Term\n\ncobalt architecture canonical plan",
            "long_term",
            embedding_provider.as_ref(),
        )
        .await
        .unwrap();
    search_index
        .index_file(
            "sessions/demo",
            "cobalt architecture draft notes from one session",
            "session",
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
            memory: &memory,
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
    .session_reader(SessionReader::new(tmp.path()))
    .search_index(search_index)
    .build();

    let out = orch
        .handle_inbound(test_inbound("cobalt architecture"), "clawhive-main")
        .await
        .unwrap();
    assert!(
        out.text.contains("### MEMORY.md"),
        "should include MEMORY.md hit"
    );
    assert!(
        !out.text.contains("### sessions/demo"),
        "long-term-biased context should be allowed to suppress session noise"
    );
}

#[tokio::test]
async fn orchestrator_dedupes_matching_fact_and_memory_chunk_in_prompt_context() {
    let mut registry = ProviderRegistry::new();
    registry.register("trace", Arc::new(TranscriptProvider));
    let aliases = HashMap::from([("trace".to_string(), "trace/model".to_string())]);

    let tmp = tempfile::TempDir::new().unwrap();
    let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
    let bus = EventBus::new(16);
    let router = LlmRouter::new(registry, aliases, vec![]);
    let agents = vec![test_full_agent("clawhive-main", "trace", vec![])];
    let file_store = MemoryFileStore::new(tmp.path());
    let session_writer = SessionWriter::new(tmp.path());
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
    let fact_store = FactStore::new(memory.db());
    let now = chrono::Utc::now().to_rfc3339();
    let content = "User prefers Chinese replies for all future answers";
    let fact = Fact {
        id: generate_fact_id("clawhive-main", content),
        agent_id: "clawhive-main".to_string(),
        content: content.to_string(),
        fact_type: "preference".to_string(),
        importance: 0.8,
        confidence: 1.0,
        status: "active".to_string(),
        occurred_at: None,
        recorded_at: now.clone(),
        source_type: "test".to_string(),
        source_session: None,
        access_count: 0,
        last_accessed: None,
        superseded_by: None,
        salience: 70,
        supersede_reason: None,
        affect: "neutral".to_string(),
        affect_intensity: 0.0,
        created_at: now.clone(),
        updated_at: now,
    };
    fact_store.insert_fact(&fact).await.unwrap();
    fact_store.record_add(&fact).await.unwrap();
    search_index
        .index_file(
            "MEMORY.md",
            content,
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
            memory: &memory,
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
    .session_reader(SessionReader::new(tmp.path()))
    .search_index(search_index)
    .build();

    let out = orch
        .handle_inbound(test_inbound("Chinese replies"), "clawhive-main")
        .await
        .unwrap();

    assert!(out.text.contains("## Known Facts"));
    assert_eq!(out.text.matches(content).count(), 1);
}

#[tokio::test]
async fn orchestrator_does_not_inject_irrelevant_facts_when_memory_hits_exist() {
    let mut registry = ProviderRegistry::new();
    registry.register("trace", Arc::new(TranscriptProvider));
    let aliases = HashMap::from([("trace".to_string(), "trace/model".to_string())]);

    let tmp = tempfile::TempDir::new().unwrap();
    let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
    let bus = EventBus::new(16);
    let router = LlmRouter::new(registry, aliases, vec![]);
    let agents = vec![test_full_agent("clawhive-main", "trace", vec![])];
    let file_store = MemoryFileStore::new(tmp.path());
    let session_writer = SessionWriter::new(tmp.path());
    let _session_reader = SessionReader::new(tmp.path());
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
    let fact_store = FactStore::new(memory.db());
    let now = chrono::Utc::now().to_rfc3339();
    let fact = Fact {
        id: generate_fact_id("clawhive-main", "User prefers dark mode"),
        agent_id: "clawhive-main".to_string(),
        content: "User prefers dark mode".to_string(),
        fact_type: "preference".to_string(),
        importance: 0.8,
        confidence: 1.0,
        status: "active".to_string(),
        occurred_at: None,
        recorded_at: now.clone(),
        source_type: "test".to_string(),
        source_session: None,
        access_count: 0,
        last_accessed: None,
        superseded_by: None,
        salience: 70,
        supersede_reason: None,
        affect: "neutral".to_string(),
        affect_intensity: 0.0,
        created_at: now.clone(),
        updated_at: now,
    };
    fact_store.insert_fact(&fact).await.unwrap();
    fact_store.record_add(&fact).await.unwrap();
    search_index
        .index_file(
            "MEMORY.md",
            "cobalt architecture canonical plan",
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
            memory: &memory,
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
    .session_reader(SessionReader::new(tmp.path()))
    .search_index(search_index)
    .build();

    let out = orch
        .handle_inbound(test_inbound("cobalt architecture"), "clawhive-main")
        .await
        .unwrap();

    assert!(out.text.contains("## Relevant Memory"));
    assert!(!out.text.contains("## Known Facts"));
    assert!(!out.text.contains("User prefers dark mode"));
}

#[tokio::test]
async fn orchestrator_keeps_file_fallback_when_facts_exist_but_search_misses() {
    let mut registry = ProviderRegistry::new();
    registry.register("trace", Arc::new(TranscriptProvider));
    let aliases = HashMap::from([("trace".to_string(), "trace/model".to_string())]);

    let tmp = tempfile::TempDir::new().unwrap();
    let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
    let bus = EventBus::new(16);
    let router = LlmRouter::new(registry, aliases, vec![]);
    let mut agent = test_full_agent("clawhive-main", "trace", vec![]);
    agent.workspace = Some(".".to_string());
    let agents = vec![agent];
    let file_store = MemoryFileStore::new(tmp.path());
    let session_writer = SessionWriter::new(tmp.path());
    let _session_reader = SessionReader::new(tmp.path());
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
    let fact_store = FactStore::new(memory.db());
    let now = chrono::Utc::now().to_rfc3339();
    let fact = Fact {
        id: generate_fact_id("clawhive-main", "User prefers dark mode"),
        agent_id: "clawhive-main".to_string(),
        content: "User prefers dark mode".to_string(),
        fact_type: "preference".to_string(),
        importance: 0.8,
        confidence: 1.0,
        status: "active".to_string(),
        occurred_at: None,
        recorded_at: now.clone(),
        source_type: "test".to_string(),
        source_session: None,
        access_count: 0,
        last_accessed: None,
        superseded_by: None,
        salience: 50,
        supersede_reason: None,
        affect: "neutral".to_string(),
        affect_intensity: 0.0,
        created_at: now.clone(),
        updated_at: now,
    };
    fact_store.insert_fact(&fact).await.unwrap();
    fact_store.record_add(&fact).await.unwrap();
    file_store
        .write_long_term("# Long Term\n\ncobalt architecture canonical plan")
        .await
        .unwrap();

    let config_view = build_test_config_view(
        agents,
        router,
        TestToolDeps {
            publisher: &bus.publisher(),
            workspace_root: tmp.path(),
            schedule_manager: Arc::clone(&schedule_manager),
            memory: &memory,
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
    .session_reader(SessionReader::new(tmp.path()))
    .search_index(search_index)
    .build();

    let out = orch
        .handle_inbound(test_inbound("cobalt architecture"), "clawhive-main")
        .await
        .unwrap();

    assert!(out.text.contains("[Memory Context]"));
    assert!(out.text.contains("From MEMORY.md:"));
    assert!(out.text.contains("cobalt architecture canonical plan"));
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
    agent.memory_policy = Some(MemoryPolicyConfig {
        mode: "auto".to_string(),
        write_scope: "workspace".to_string(),
        idle_minutes: Some(0),
        daily_at_hour: None,
        limit_history_turns: None,
        max_injected_chars: 6000,
        daily_summary_interval: 0,
    });
    let agents = vec![agent];
    let file_store = MemoryFileStore::new(tmp.path());
    let session_writer = SessionWriter::new(tmp.path());
    let search_index = SearchIndex::new(memory.db(), "test-agent");
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
            memory: &memory,
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
    .session_reader(SessionReader::new(tmp.path()))
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

    wait_until(Duration::from_secs(3), || {
        let file_store = file_store.clone();
        async move {
            file_store
                .read_daily(chrono::Utc::now().date_naive())
                .await
                .unwrap()
                .is_some_and(|daily| {
                    daily.contains("- fallback summary 1") && daily.contains("- fallback summary 2")
                })
        }
    })
    .await;

    let daily = file_store
        .read_daily(chrono::Utc::now().date_naive())
        .await
        .unwrap()
        .expect("daily file should exist");

    assert!(daily.contains("- fallback summary 1"));
    assert!(daily.contains("- fallback summary 2"));
}

#[tokio::test]
async fn fallback_summary_links_session_chunk_to_daily_canonical() {
    let mut registry = ProviderRegistry::new();
    registry.register("summary", Arc::new(StructuredDailySummaryProvider));
    let aliases = HashMap::from([("summary".to_string(), "summary/model".to_string())]);

    let tmp = tempfile::TempDir::new().unwrap();
    let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
    let bus = EventBus::new(16);
    let router = LlmRouter::new(registry, aliases, vec![]);
    let mut agent = test_full_agent("clawhive-main", "summary", vec![]);
    agent.workspace = Some(".".to_string());
    agent.memory_policy = Some(MemoryPolicyConfig {
        mode: "auto".to_string(),
        write_scope: "workspace".to_string(),
        idle_minutes: Some(0),
        daily_at_hour: None,
        limit_history_turns: None,
        max_injected_chars: 6000,
        daily_summary_interval: 0,
    });
    let agents = vec![agent];
    let file_store = MemoryFileStore::new(tmp.path());
    let session_writer = SessionWriter::new(tmp.path());
    let search_index = SearchIndex::new(memory.db(), "test-agent");
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
            memory: &memory,
            file_store: &file_store,
            search_index: &search_index,
            embedding_provider: Arc::clone(&embedding_provider),
        },
    );

    let orch = OrchestratorBuilder::new(
        config_view,
        bus.publisher(),
        Arc::clone(&memory),
        Arc::new(NativeExecutor),
        tmp.path().to_path_buf(),
        schedule_manager,
    )
    .session_mgr(SessionManager::new(Arc::clone(&memory), 0))
    .file_store(file_store.clone())
    .session_writer(session_writer)
    .session_reader(SessionReader::new(tmp.path()))
    .search_index(search_index)
    .build();

    let first = test_inbound("first turn");
    let session_key = SessionKey::from_inbound(&first);
    orch.handle_inbound(first, "clawhive-main").await.unwrap();
    let old_session = memory
        .get_session(&session_key.0)
        .await
        .unwrap()
        .expect("session should exist after first turn");
    let old_session_id = old_session.session_id.clone();
    let db = memory.db();
    let chunk_ids = {
        let conn = db.lock().unwrap();
        let mut stmt = conn
            .prepare("SELECT id FROM chunks WHERE agent_id = 'clawhive-main' AND path LIKE ?1")
            .unwrap();
        let chunk_prefix = format!("sessions/{}#%", old_session_id);
        stmt.query_map([chunk_prefix], |row| row.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    assert!(!chunk_ids.is_empty());
    orch.handle_inbound(test_inbound("second turn"), "clawhive-main")
        .await
        .unwrap();

    let canonical_id = clawhive_memory::memory_lineage::generate_canonical_id_with_key(
        "clawhive-main",
        "daily",
        Some("first-turn"),
        "first turn",
    );
    let lineage_store = MemoryLineageStore::new(memory.db());
    let daily_links = lineage_store
        .get_links_for_source(
            "clawhive-main",
            "daily_section",
            &format!(
                "memory/{}.md#{}",
                chrono::Utc::now().date_naive().format("%Y-%m-%d"),
                canonical_id
            ),
        )
        .await
        .unwrap();

    assert_eq!(daily_links.len(), 1);

    // Session-end-only flush is async — allow up to 10s for the full
    // flush pipeline (summary → daily write → chunk linkage) to complete.
    wait_until(Duration::from_secs(10), || {
        let lineage_store = lineage_store.clone();
        let chunk_ids = chunk_ids.clone();
        let canonical_id = canonical_id.clone();
        async move {
            for chunk_id in &chunk_ids {
                let chunk_links = lineage_store
                    .get_links_for_source("clawhive-main", "chunk", chunk_id)
                    .await
                    .unwrap();
                if chunk_links
                    .iter()
                    .any(|link| link.canonical_id == canonical_id)
                {
                    return true;
                }
            }
            false
        }
    })
    .await;
}

#[tokio::test]
async fn fallback_summary_routes_fact_candidates_into_facts_and_memory_candidates_into_daily() {
    let mut registry = ProviderRegistry::new();
    registry.register("summary", Arc::new(StructuredPromotionSummaryProvider));
    let aliases = HashMap::from([("summary".to_string(), "summary/model".to_string())]);

    let tmp = tempfile::TempDir::new().unwrap();
    let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
    let bus = EventBus::new(16);
    let router = LlmRouter::new(registry, aliases, vec![]);
    let mut agent = test_full_agent("clawhive-main", "summary", vec![]);
    agent.workspace = Some(".".to_string());
    agent.memory_policy = Some(MemoryPolicyConfig {
        mode: "auto".to_string(),
        write_scope: "workspace".to_string(),
        idle_minutes: Some(0),
        daily_at_hour: None,
        limit_history_turns: None,
        max_injected_chars: 6000,
        daily_summary_interval: 0,
    });
    let agents = vec![agent];
    let file_store = MemoryFileStore::new(tmp.path());
    let session_writer = SessionWriter::new(tmp.path());
    let search_index = SearchIndex::new(memory.db(), "test-agent");
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
            memory: &memory,
            file_store: &file_store,
            search_index: &search_index,
            embedding_provider: Arc::clone(&embedding_provider),
        },
    );

    let orch = OrchestratorBuilder::new(
        config_view,
        bus.publisher(),
        Arc::clone(&memory),
        Arc::new(NativeExecutor),
        tmp.path().to_path_buf(),
        schedule_manager,
    )
    .session_mgr(SessionManager::new(Arc::clone(&memory), 0))
    .file_store(file_store.clone())
    .session_writer(session_writer)
    .session_reader(SessionReader::new(tmp.path()))
    .search_index(search_index)
    .build();

    orch.handle_inbound(test_inbound("first turn"), "clawhive-main")
        .await
        .unwrap();
    orch.handle_inbound(test_inbound("second turn"), "clawhive-main")
        .await
        .unwrap();

    wait_until(Duration::from_secs(3), || {
        let file_store = file_store.clone();
        let memory = Arc::clone(&memory);
        async move {
            let daily_ready = file_store
                .read_daily(chrono::Utc::now().date_naive())
                .await
                .unwrap()
                .is_some_and(|daily| daily.contains("Memory refactor is now section-based"));
            let fact_ready = FactStore::new(memory.db())
                .find_by_content("clawhive-main", "User prefers Chinese replies")
                .await
                .unwrap()
                .is_some();
            daily_ready && fact_ready
        }
    })
    .await;

    let daily = file_store
        .read_daily(chrono::Utc::now().date_naive())
        .await
        .unwrap()
        .expect("daily file should exist");

    assert!(daily.contains("Memory refactor is now section-based"));

    let fact = FactStore::new(memory.db())
        .find_by_content("clawhive-main", "User prefers Chinese replies")
        .await
        .unwrap()
        .expect("fact candidate should be written into facts");
    assert_eq!(fact.fact_type, "preference");
}

#[tokio::test]
async fn fallback_summary_suppresses_content_already_explicitly_remembered() {
    let mut registry = ProviderRegistry::new();
    registry.register("summary", Arc::new(StructuredPromotionSummaryProvider));
    let aliases = HashMap::from([("summary".to_string(), "summary/model".to_string())]);

    let tmp = tempfile::TempDir::new().unwrap();
    let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
    let bus = EventBus::new(16);
    let router = LlmRouter::new(registry, aliases, vec![]);
    let mut agent = test_full_agent("clawhive-main", "summary", vec![]);
    agent.workspace = Some(".".to_string());
    agent.memory_policy = Some(MemoryPolicyConfig {
        mode: "auto".to_string(),
        write_scope: "workspace".to_string(),
        idle_minutes: Some(0),
        daily_at_hour: None,
        limit_history_turns: None,
        max_injected_chars: 6000,
        daily_summary_interval: 0,
    });
    let agents = vec![agent];
    let file_store = MemoryFileStore::new(tmp.path());
    let session_writer = SessionWriter::new(tmp.path());
    let search_index = SearchIndex::new(memory.db(), "test-agent");
    let embedding_provider: Arc<dyn EmbeddingProvider> = Arc::new(StubEmbeddingProvider::new(8));
    let schedule_manager = Arc::new(
        ScheduleManager::new(
            SqliteStore::open(&tmp.path().join("data/scheduler.db")).unwrap(),
            Arc::new(EventBus::new(16)),
        )
        .await
        .unwrap(),
    );

    let fact_store = FactStore::new(memory.db());
    let now = chrono::Utc::now().to_rfc3339();
    let fact = Fact {
        id: generate_fact_id("clawhive-main", "User prefers Chinese replies"),
        agent_id: "clawhive-main".to_string(),
        content: "User prefers Chinese replies".to_string(),
        fact_type: "preference".to_string(),
        importance: 0.8,
        confidence: 1.0,
        status: "active".to_string(),
        occurred_at: None,
        recorded_at: now.clone(),
        source_type: "explicit_user_memory".to_string(),
        source_session: None,
        access_count: 0,
        last_accessed: None,
        superseded_by: None,
        salience: 50,
        supersede_reason: None,
        affect: "neutral".to_string(),
        affect_intensity: 0.0,
        created_at: now.clone(),
        updated_at: now,
    };
    fact_store.insert_fact(&fact).await.unwrap();
    fact_store.record_add(&fact).await.unwrap();

    let config_view = build_test_config_view(
        agents,
        router,
        TestToolDeps {
            publisher: &bus.publisher(),
            workspace_root: tmp.path(),
            schedule_manager: Arc::clone(&schedule_manager),
            memory: &memory,
            file_store: &file_store,
            search_index: &search_index,
            embedding_provider: Arc::clone(&embedding_provider),
        },
    );

    let orch = OrchestratorBuilder::new(
        config_view,
        bus.publisher(),
        Arc::clone(&memory),
        Arc::new(NativeExecutor),
        tmp.path().to_path_buf(),
        schedule_manager,
    )
    .session_mgr(SessionManager::new(Arc::clone(&memory), 0))
    .file_store(file_store.clone())
    .session_writer(session_writer)
    .session_reader(SessionReader::new(tmp.path()))
    .search_index(search_index)
    .build();

    orch.handle_inbound(test_inbound("first turn"), "clawhive-main")
        .await
        .unwrap();
    orch.handle_inbound(test_inbound("second turn"), "clawhive-main")
        .await
        .unwrap();

    wait_until(Duration::from_secs(3), || {
        let file_store = file_store.clone();
        async move {
            file_store
                .read_daily(chrono::Utc::now().date_naive())
                .await
                .unwrap()
                .is_some_and(|daily| daily.contains("Memory refactor is now section-based"))
        }
    })
    .await;

    let daily = file_store
        .read_daily(chrono::Utc::now().date_naive())
        .await
        .unwrap()
        .expect("daily file should exist");

    assert!(!daily.contains("User prefers Chinese replies"));
    assert!(daily.contains("Memory refactor is now section-based"));
}

#[tokio::test]
async fn fallback_summary_suppresses_rewritten_memory_candidate_with_same_duplicate_key() {
    let mut registry = ProviderRegistry::new();
    registry.register(
        "summary",
        Arc::new(DuplicateKeyRewriteSummaryProvider {
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
    agent.memory_policy = Some(MemoryPolicyConfig {
        mode: "auto".to_string(),
        write_scope: "workspace".to_string(),
        idle_minutes: Some(0),
        daily_at_hour: None,
        limit_history_turns: None,
        max_injected_chars: 6000,
        daily_summary_interval: 0,
    });
    let agents = vec![agent];
    let file_store = MemoryFileStore::new(tmp.path());
    let session_writer = SessionWriter::new(tmp.path());
    let search_index = SearchIndex::new(memory.db(), "test-agent");
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
            memory: &memory,
            file_store: &file_store,
            search_index: &search_index,
            embedding_provider: Arc::clone(&embedding_provider),
        },
    );

    let orch = OrchestratorBuilder::new(
        config_view,
        bus.publisher(),
        Arc::clone(&memory),
        Arc::new(NativeExecutor),
        tmp.path().to_path_buf(),
        schedule_manager,
    )
    .session_mgr(SessionManager::new(Arc::clone(&memory), 0))
    .file_store(file_store.clone())
    .session_writer(session_writer)
    .session_reader(SessionReader::new(tmp.path()))
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

    wait_until(Duration::from_secs(3), || {
        let file_store = file_store.clone();
        async move {
            file_store
                .read_daily(chrono::Utc::now().date_naive())
                .await
                .unwrap()
                .is_some_and(|daily| daily.contains("Memory refactor is now section-based"))
        }
    })
    .await;

    let daily = file_store
        .read_daily(chrono::Utc::now().date_naive())
        .await
        .unwrap()
        .expect("daily file should exist");
    assert!(daily.contains("Memory refactor is now section-based"));
}

#[tokio::test]
async fn explicit_reset_flushes_before_clearing_previous_session() {
    let mut registry = ProviderRegistry::new();
    registry.register("summary", Arc::new(StructuredDailySummaryProvider));
    let aliases = HashMap::from([("summary".to_string(), "summary/model".to_string())]);

    let tmp = tempfile::TempDir::new().unwrap();
    let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
    let bus = EventBus::new(16);
    let router = LlmRouter::new(registry, aliases, vec![]);
    let mut agent = test_full_agent("clawhive-main", "summary", vec![]);
    agent.workspace = Some(".".to_string());
    agent.memory_policy = Some(MemoryPolicyConfig {
        mode: "auto".to_string(),
        write_scope: "workspace".to_string(),
        idle_minutes: Some(30),
        daily_at_hour: Some(4),
        limit_history_turns: None,
        max_injected_chars: 6000,
        daily_summary_interval: 0,
    });
    let agents = vec![agent];
    let file_store = MemoryFileStore::new(tmp.path());
    let session_writer = SessionWriter::new(tmp.path());
    let search_index = SearchIndex::new(memory.db(), "test-agent");
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
            memory: &memory,
            file_store: &file_store,
            search_index: &search_index,
            embedding_provider: Arc::clone(&embedding_provider),
        },
    );

    let orch = OrchestratorBuilder::new(
        config_view,
        bus.publisher(),
        Arc::clone(&memory),
        Arc::new(NativeExecutor),
        tmp.path().to_path_buf(),
        schedule_manager,
    )
    .file_store(file_store.clone())
    .session_writer(session_writer)
    .session_reader(SessionReader::new(tmp.path()))
    .search_index(search_index)
    .build();

    let initial = test_inbound("before reset");
    orch.handle_inbound(initial.clone(), "clawhive-main")
        .await
        .unwrap();

    let session_key = SessionKey::from_inbound(&initial);
    let previous_session = memory
        .get_session(&session_key.0)
        .await
        .unwrap()
        .expect("session should exist before reset");
    let previous_session_id = previous_session.session_id.clone();

    let reset_out = orch
        .handle_inbound(test_inbound("/reset"), "clawhive-main")
        .await
        .unwrap();
    assert!(!reset_out.text.is_empty());

    let current_session = memory
        .get_session(&session_key.0)
        .await
        .unwrap()
        .expect("fresh session should exist after reset");
    let session_reader = SessionReader::new(tmp.path());
    assert_ne!(current_session.session_id, previous_session_id);
    assert!(!session_reader.session_exists(&previous_session_id).await);
    assert!(memory
        .get_session_memory_state("clawhive-main", &previous_session_id)
        .await
        .unwrap()
        .is_none());

    let today = chrono::Utc::now().date_naive();
    let daily = file_store
        .read_daily(today)
        .await
        .unwrap()
        .expect("daily file should exist after explicit reset");
    assert!(daily.contains("first turn"));
}

#[tokio::test]
async fn stale_reset_boundary_flush_runs_async_and_marks_pending_flush() {
    let mut registry = ProviderRegistry::new();
    registry.register("summary", Arc::new(SlowStructuredDailySummaryProvider));
    let aliases = HashMap::from([("summary".to_string(), "summary/model".to_string())]);

    let tmp = tempfile::TempDir::new().unwrap();
    let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
    let bus = EventBus::new(16);
    let router = LlmRouter::new(registry, aliases, vec![]);
    let mut agent = test_full_agent("clawhive-main", "summary", vec![]);
    agent.workspace = Some(".".to_string());
    agent.memory_policy = Some(MemoryPolicyConfig {
        mode: "auto".to_string(),
        write_scope: "workspace".to_string(),
        idle_minutes: Some(0),
        daily_at_hour: None,
        limit_history_turns: None,
        max_injected_chars: 6000,
        daily_summary_interval: 0,
    });
    let agents = vec![agent];
    let file_store = MemoryFileStore::new(tmp.path());
    let session_writer = SessionWriter::new(tmp.path());
    let _session_reader = SessionReader::new(tmp.path());
    let search_index = SearchIndex::new(memory.db(), "test-agent");
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
            memory: &memory,
            file_store: &file_store,
            search_index: &search_index,
            embedding_provider: Arc::clone(&embedding_provider),
        },
    );

    let orch = OrchestratorBuilder::new(
        config_view,
        bus.publisher(),
        Arc::clone(&memory),
        Arc::new(NativeExecutor),
        tmp.path().to_path_buf(),
        schedule_manager,
    )
    .session_mgr(SessionManager::new(Arc::clone(&memory), 0))
    .file_store(file_store.clone())
    .session_writer(session_writer)
    .session_reader(SessionReader::new(tmp.path()))
    .search_index(search_index)
    .build();

    let first = test_inbound("first turn");
    let session_key = SessionKey::from_inbound(&first);
    orch.handle_inbound(first, "clawhive-main").await.unwrap();
    let old_session = memory
        .get_session(&session_key.0)
        .await
        .unwrap()
        .expect("session should exist after first inbound");
    let old_session_id = old_session.session_id.clone();
    let reader = SessionReader::new(tmp.path());

    let started = Instant::now();
    orch.handle_inbound(test_inbound("second turn"), "clawhive-main")
        .await
        .unwrap();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_millis(250),
        "stale reset should not block on slow boundary flush: {:?}",
        elapsed
    );

    let pending_state = memory
        .get_session_memory_state("clawhive-main", &old_session_id)
        .await
        .unwrap()
        .expect("pending state should be recorded for stale session");
    assert!(pending_state.pending_flush);
    assert_eq!(pending_state.last_flushed_turn, 0);
    assert!(reader.session_exists(&old_session_id).await);

    wait_until(Duration::from_secs(3), || {
        let file_store = file_store.clone();
        let memory = Arc::clone(&memory);
        let old_session_id = old_session_id.clone();
        let session_reader = SessionReader::new(file_store.workspace_dir());
        async move {
            let summary_done = file_store
                .read_daily(chrono::Utc::now().date_naive())
                .await
                .unwrap()
                .is_some_and(|daily| daily.contains("first turn"));
            let state_cleared = memory
                .get_session_memory_state("clawhive-main", &old_session_id)
                .await
                .unwrap()
                .is_none();
            let transcript_archived = !session_reader.session_exists(&old_session_id).await;
            summary_done && state_cleared && transcript_archived
        }
    })
    .await;
}

#[tokio::test]
async fn pending_boundary_flush_recovers_after_restart_on_next_inbound() {
    let mut registry = ProviderRegistry::new();
    registry.register("summary", Arc::new(StructuredDailySummaryProvider));
    let aliases = HashMap::from([("summary".to_string(), "summary/model".to_string())]);

    let tmp = tempfile::TempDir::new().unwrap();
    let db_path = tmp.path().join("memory.db");
    let memory = Arc::new(MemoryStore::open(db_path.to_str().unwrap()).unwrap());
    let bus = EventBus::new(16);
    let router = LlmRouter::new(registry, aliases, vec![]);
    let mut agent = test_full_agent("clawhive-main", "summary", vec![]);
    agent.workspace = Some(".".to_string());
    agent.memory_policy = Some(MemoryPolicyConfig {
        mode: "auto".to_string(),
        write_scope: "workspace".to_string(),
        idle_minutes: Some(30),
        daily_at_hour: Some(4),
        limit_history_turns: None,
        max_injected_chars: 6000,
        daily_summary_interval: 0,
    });
    let agents = vec![agent];
    let file_store = MemoryFileStore::new(tmp.path());
    let session_writer = SessionWriter::new(tmp.path());
    let _session_reader = SessionReader::new(tmp.path());
    let search_index = SearchIndex::new(memory.db(), "test-agent");
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
            memory: &memory,
            file_store: &file_store,
            search_index: &search_index,
            embedding_provider: Arc::clone(&embedding_provider),
        },
    );

    let orch = OrchestratorBuilder::new(
        config_view,
        bus.publisher(),
        Arc::clone(&memory),
        Arc::new(NativeExecutor),
        tmp.path().to_path_buf(),
        schedule_manager,
    )
    .file_store(file_store.clone())
    .session_writer(SessionWriter::new(tmp.path()))
    .session_reader(SessionReader::new(tmp.path()))
    .search_index(SearchIndex::new(memory.db(), "test-agent"))
    .build();

    let first = test_inbound("first turn");
    let session_key = SessionKey::from_inbound(&first);
    let stale_session_id = "stale-session-1";
    let current_session_id = "active-session-1";

    session_writer
        .start_session(stale_session_id, "clawhive-main")
        .await
        .unwrap();
    session_writer
        .append_message(stale_session_id, "user", "first turn")
        .await
        .unwrap();
    session_writer
        .append_message(stale_session_id, "assistant", "reply: first turn")
        .await
        .unwrap();
    session_writer
        .start_session(current_session_id, "clawhive-main")
        .await
        .unwrap();

    memory
        .upsert_session(clawhive_memory::SessionRecord {
            session_key: session_key.0.clone(),
            session_id: current_session_id.to_string(),
            agent_id: "clawhive-main".to_string(),
            created_at: chrono::Utc::now(),
            last_active: chrono::Utc::now(),
            ttl_seconds: 1800,
            interaction_count: 0,
        })
        .await
        .unwrap();
    memory
        .upsert_session_memory_state(clawhive_memory::SessionMemoryStateRecord {
            agent_id: "clawhive-main".to_string(),
            session_id: stale_session_id.to_string(),
            session_key: session_key.0.clone(),
            last_flushed_turn: 0,
            last_boundary_flush_at: None,
            pending_flush: true,
            flush_phase: "idle".to_string(),
            flush_phase_updated_at: None,
            flush_summary_cache: None,
            recent_explicit_writes: Vec::new(),
            open_episodes: Vec::new(),
        })
        .await
        .unwrap();

    let out = orch
        .handle_inbound(test_inbound("second turn after restart"), "clawhive-main")
        .await
        .unwrap();
    assert!(out.text.contains("second turn after restart"));

    wait_until(Duration::from_secs(3), || {
        let file_store = file_store.clone();
        let memory = Arc::clone(&memory);
        let session_reader = SessionReader::new(file_store.workspace_dir());
        async move {
            let summary_done = file_store
                .read_daily(chrono::Utc::now().date_naive())
                .await
                .unwrap()
                .is_some_and(|daily| daily.contains("first turn"));
            let state_cleared = memory
                .get_session_memory_state("clawhive-main", stale_session_id)
                .await
                .unwrap()
                .is_none();
            let transcript_archived = !session_reader.session_exists(stale_session_id).await;
            summary_done && state_cleared && transcript_archived
        }
    })
    .await;
}

#[tokio::test]
async fn daily_reset_flushes_pending_memory_candidates() {
    let mut registry = ProviderRegistry::new();
    registry.register("summary", Arc::new(StructuredDailySummaryProvider));
    let aliases = HashMap::from([("summary".to_string(), "summary/model".to_string())]);

    let tmp = tempfile::TempDir::new().unwrap();
    let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
    let bus = EventBus::new(16);
    let router = LlmRouter::new(registry, aliases, vec![]);
    let mut agent = test_full_agent("clawhive-main", "summary", vec![]);
    agent.workspace = Some(".".to_string());
    agent.memory_policy = Some(MemoryPolicyConfig {
        mode: "auto".to_string(),
        write_scope: "workspace".to_string(),
        idle_minutes: None,
        daily_at_hour: Some(4),
        limit_history_turns: None,
        max_injected_chars: 6000,
        daily_summary_interval: 0,
    });
    let agents = vec![agent];
    let file_store = MemoryFileStore::new(tmp.path());
    let session_writer = SessionWriter::new(tmp.path());
    let _session_reader = SessionReader::new(tmp.path());
    let search_index = SearchIndex::new(memory.db(), "test-agent");
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
            memory: &memory,
            file_store: &file_store,
            search_index: &search_index,
            embedding_provider: Arc::clone(&embedding_provider),
        },
    );

    let orch = OrchestratorBuilder::new(
        config_view,
        bus.publisher(),
        Arc::clone(&memory),
        Arc::new(NativeExecutor),
        tmp.path().to_path_buf(),
        schedule_manager,
    )
    .session_mgr(SessionManager::new(Arc::clone(&memory), 0))
    .file_store(file_store.clone())
    .session_writer(session_writer)
    .session_reader(SessionReader::new(tmp.path()))
    .search_index(search_index)
    .build();

    let first = test_inbound("first turn");
    let session_key = SessionKey::from_inbound(&first);
    orch.handle_inbound(first, "clawhive-main").await.unwrap();

    let mut old_session = memory
        .get_session(&session_key.0)
        .await
        .unwrap()
        .expect("session should exist after first inbound");
    old_session.last_active = chrono::Utc::now() - chrono::Duration::days(2);
    memory.upsert_session(old_session.clone()).await.unwrap();
    let old_session_id = old_session.session_id.clone();

    orch.handle_inbound(test_inbound("second turn"), "clawhive-main")
        .await
        .unwrap();

    let new_session = memory
        .get_session(&session_key.0)
        .await
        .unwrap()
        .expect("fresh session should exist after daily reset");
    assert_ne!(new_session.session_id, old_session_id);

    wait_until(Duration::from_secs(3), || {
        let file_store = file_store.clone();
        let memory = Arc::clone(&memory);
        let old_session_id = old_session_id.clone();
        async move {
            let summary_done = file_store
                .read_daily(chrono::Utc::now().date_naive())
                .await
                .unwrap()
                .is_some_and(|daily| daily.contains("first turn"));
            let state_cleared = memory
                .get_session_memory_state("clawhive-main", &old_session_id)
                .await
                .unwrap()
                .is_none();
            summary_done && state_cleared
        }
    })
    .await;
}

#[tokio::test]
async fn episode_closure_does_not_flush_before_session_end() {
    let mut registry = ProviderRegistry::new();
    registry.register("summary", Arc::new(StructuredDailySummaryProvider));
    let aliases = HashMap::from([("summary".to_string(), "summary/model".to_string())]);

    let tmp = tempfile::TempDir::new().unwrap();
    let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
    let bus = EventBus::new(16);
    let router = LlmRouter::new(registry, aliases, vec![]);
    let mut agent = test_full_agent("clawhive-main", "summary", vec![]);
    agent.workspace = Some(".".to_string());
    agent.memory_policy = Some(MemoryPolicyConfig {
        mode: "auto".to_string(),
        write_scope: "workspace".to_string(),
        idle_minutes: Some(30),
        daily_at_hour: Some(4),
        limit_history_turns: None,
        max_injected_chars: 6000,
        daily_summary_interval: 0,
    });
    let agents = vec![agent];
    let file_store = MemoryFileStore::new(tmp.path());
    let session_writer = SessionWriter::new(tmp.path());
    let _session_reader = SessionReader::new(tmp.path());
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
            publisher: &bus.publisher(),
            workspace_root: tmp.path(),
            schedule_manager: Arc::clone(&schedule_manager),
            memory: &memory,
            file_store: &file_store,
            search_index: &search_index,
            embedding_provider: Arc::clone(&embedding_provider),
        },
    );

    let orch = OrchestratorBuilder::new(
        config_view,
        bus.publisher(),
        Arc::clone(&memory),
        Arc::new(NativeExecutor),
        tmp.path().to_path_buf(),
        schedule_manager,
    )
    .session_mgr(SessionManager::new(Arc::clone(&memory), 0))
    .file_store(file_store.clone())
    .session_writer(session_writer)
    .session_reader(SessionReader::new(tmp.path()))
    .search_index(search_index)
    .build();

    let first = test_inbound("How do I use Rust Vec push?");
    let session_key = SessionKey::from_inbound(&first);
    orch.handle_inbound(first, "clawhive-main").await.unwrap();

    let session = memory
        .get_session(&session_key.0)
        .await
        .unwrap()
        .expect("session exists after first inbound");
    let session_id = session.session_id.clone();

    orch.handle_inbound(
        test_inbound("How do I inspect RunPod GPU usage?"),
        "clawhive-main",
    )
    .await
    .unwrap();

    tokio::time::sleep(Duration::from_millis(400)).await;

    let daily = file_store
        .read_daily(chrono::Utc::now().date_naive())
        .await
        .unwrap()
        .unwrap_or_default();
    assert!(
        !daily.contains("first turn"),
        "episode closure should not trigger immediate boundary flush"
    );

    let state = memory
        .get_session_memory_state("clawhive-main", &session_id)
        .await
        .unwrap()
        .expect("session memory state should exist");
    assert_eq!(state.last_flushed_turn, 0);
    assert!(
        state
            .open_episodes
            .iter()
            .any(|episode| episode.start_turn == 2),
        "next episode should remain tracked"
    );
}

#[tokio::test]
async fn episode_closure_respects_explicit_memory_precheck_for_fact_candidates() {
    let mut registry = ProviderRegistry::new();
    registry.register("summary", Arc::new(StructuredPromotionSummaryProvider));
    let aliases = HashMap::from([("summary".to_string(), "summary/model".to_string())]);

    let tmp = tempfile::TempDir::new().unwrap();
    let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
    let bus = EventBus::new(16);
    let router = LlmRouter::new(registry, aliases, vec![]);
    let mut agent = test_full_agent("clawhive-main", "summary", vec![]);
    agent.workspace = Some(".".to_string());
    agent.memory_policy = Some(MemoryPolicyConfig {
        mode: "auto".to_string(),
        write_scope: "workspace".to_string(),
        idle_minutes: Some(30),
        daily_at_hour: Some(4),
        limit_history_turns: None,
        max_injected_chars: 6000,
        daily_summary_interval: 0,
    });
    let agents = vec![agent];
    let file_store = MemoryFileStore::new(tmp.path());
    let session_writer = SessionWriter::new(tmp.path());
    let _session_reader = SessionReader::new(tmp.path());
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
            publisher: &bus.publisher(),
            workspace_root: tmp.path(),
            schedule_manager: Arc::clone(&schedule_manager),
            memory: &memory,
            file_store: &file_store,
            search_index: &search_index,
            embedding_provider: Arc::clone(&embedding_provider),
        },
    );

    let orch = OrchestratorBuilder::new(
        config_view,
        bus.publisher(),
        Arc::clone(&memory),
        Arc::new(NativeExecutor),
        tmp.path().to_path_buf(),
        schedule_manager,
    )
    .session_mgr(SessionManager::new(Arc::clone(&memory), 0))
    .file_store(file_store.clone())
    .session_writer(session_writer)
    .session_reader(SessionReader::new(tmp.path()))
    .search_index(search_index)
    .build();

    let first = test_inbound("Please remember that I prefer Chinese replies.");
    let session_key = SessionKey::from_inbound(&first);
    orch.handle_inbound(first, "clawhive-main").await.unwrap();
    let session = memory
        .get_session(&session_key.0)
        .await
        .unwrap()
        .expect("session exists after first inbound");

    let fact_store = FactStore::new(memory.db());
    let now = chrono::Utc::now().to_rfc3339();
    let fact = Fact {
        id: generate_fact_id("clawhive-main", "User prefers Chinese replies"),
        agent_id: "clawhive-main".to_string(),
        content: "User prefers Chinese replies".to_string(),
        fact_type: "preference".to_string(),
        importance: 0.9,
        confidence: 0.95,
        status: "active".to_string(),
        occurred_at: None,
        recorded_at: now.clone(),
        source_type: "explicit_user_memory".to_string(),
        source_session: Some(session.session_id.clone()),
        access_count: 0,
        last_accessed: None,
        superseded_by: None,
        salience: 50,
        supersede_reason: None,
        affect: "neutral".to_string(),
        affect_intensity: 0.0,
        created_at: now.clone(),
        updated_at: now,
    };
    fact_store.insert_fact(&fact).await.unwrap();

    orch.handle_inbound(
        test_inbound("What about RunPod GPU status?"),
        "clawhive-main",
    )
    .await
    .unwrap();

    wait_until(Duration::from_secs(3), || {
        let memory = Arc::clone(&memory);
        let file_store = file_store.clone();
        async move {
            let facts = FactStore::new(memory.db())
                .get_active_facts("clawhive-main")
                .await
                .unwrap();
            let daily = file_store
                .read_daily(chrono::Utc::now().date_naive())
                .await
                .unwrap()
                .unwrap_or_default();
            facts.len() == 1
                && facts[0].content == "User prefers Chinese replies"
                && !daily.contains("User prefers Chinese replies")
        }
    })
    .await;
}

#[tokio::test]
async fn incomplete_closed_episode_defers_write_until_session_boundary_flush() {
    let mut registry = ProviderRegistry::new();
    registry.register("summary", Arc::new(PromiseThenSummaryProvider));
    let aliases = HashMap::from([("summary".to_string(), "summary/model".to_string())]);

    let tmp = tempfile::TempDir::new().unwrap();
    let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
    let bus = EventBus::new(16);
    let router = LlmRouter::new(registry, aliases, vec![]);
    let mut agent = test_full_agent("clawhive-main", "summary", vec![]);
    agent.workspace = Some(".".to_string());
    agent.memory_policy = Some(MemoryPolicyConfig {
        mode: "auto".to_string(),
        write_scope: "workspace".to_string(),
        idle_minutes: Some(30),
        daily_at_hour: Some(4),
        limit_history_turns: None,
        max_injected_chars: 6000,
        daily_summary_interval: 0,
    });
    let agents = vec![agent];
    let file_store = MemoryFileStore::new(tmp.path());
    let session_writer = SessionWriter::new(tmp.path());
    let _session_reader = SessionReader::new(tmp.path());
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
            publisher: &bus.publisher(),
            workspace_root: tmp.path(),
            schedule_manager: Arc::clone(&schedule_manager),
            memory: &memory,
            file_store: &file_store,
            search_index: &search_index,
            embedding_provider: Arc::clone(&embedding_provider),
        },
    );

    let orch = OrchestratorBuilder::new(
        config_view,
        bus.publisher(),
        Arc::clone(&memory),
        Arc::new(NativeExecutor),
        tmp.path().to_path_buf(),
        schedule_manager,
    )
    .session_mgr(SessionManager::new(Arc::clone(&memory), 0))
    .file_store(file_store.clone())
    .session_writer(session_writer)
    .session_reader(SessionReader::new(tmp.path()))
    .search_index(search_index)
    .build();

    let first = test_inbound("请整理 memory 重构方案");
    let session_key = SessionKey::from_inbound(&first);
    orch.handle_inbound(first, "clawhive-main").await.unwrap();
    let session = memory
        .get_session(&session_key.0)
        .await
        .unwrap()
        .expect("session exists after first inbound");
    let session_id = session.session_id.clone();

    orch.handle_inbound(test_inbound("顺便看一下 RunPod GPU"), "clawhive-main")
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_millis(400)).await;
    let daily_before_reset = file_store
        .read_daily(chrono::Utc::now().date_naive())
        .await
        .unwrap()
        .unwrap_or_default();
    assert!(
        !daily_before_reset.contains("first turn"),
        "interrupted episode should not flush before session boundary"
    );

    let mut old_session = memory
        .get_session(&session_key.0)
        .await
        .unwrap()
        .expect("old session should still exist");
    old_session.last_active = chrono::Utc::now() - chrono::Duration::days(2);
    memory.upsert_session(old_session.clone()).await.unwrap();

    orch.handle_inbound(test_inbound("新的一天继续"), "clawhive-main")
        .await
        .unwrap();

    wait_until(Duration::from_secs(3), || {
        let file_store = file_store.clone();
        let memory = Arc::clone(&memory);
        let session_id = session_id.clone();
        async move {
            let summary_done = file_store
                .read_daily(chrono::Utc::now().date_naive())
                .await
                .unwrap()
                .is_some_and(|daily| daily.contains("first turn"));
            let state_cleared = memory
                .get_session_memory_state("clawhive-main", &session_id)
                .await
                .unwrap()
                .is_none();
            summary_done && state_cleared
        }
    })
    .await;
}

#[tokio::test]
async fn raw_session_is_indexed_into_chunks_after_inbound() {
    let mut registry = ProviderRegistry::new();
    registry.register("echo", Arc::new(EchoProvider));
    let aliases = HashMap::from([("echo".to_string(), "echo/model".to_string())]);

    let tmp = tempfile::TempDir::new().unwrap();
    let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
    let bus = EventBus::new(16);
    let router = LlmRouter::new(registry, aliases, vec![]);
    let mut agent = test_full_agent("clawhive-main", "echo", vec![]);
    agent.workspace = Some(".".to_string());
    agent.memory_policy = Some(MemoryPolicyConfig {
        mode: "auto".to_string(),
        write_scope: "workspace".to_string(),
        idle_minutes: Some(30),
        daily_at_hour: Some(4),
        limit_history_turns: None,
        max_injected_chars: 6000,
        daily_summary_interval: 0,
    });
    let agents = vec![agent];
    let file_store = MemoryFileStore::new(tmp.path());
    let session_writer = SessionWriter::new(tmp.path());
    let _session_reader = SessionReader::new(tmp.path());
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
            publisher: &bus.publisher(),
            workspace_root: tmp.path(),
            schedule_manager: Arc::clone(&schedule_manager),
            memory: &memory,
            file_store: &file_store,
            search_index: &search_index,
            embedding_provider: Arc::clone(&embedding_provider),
        },
    );

    let orch = OrchestratorBuilder::new(
        config_view,
        bus.publisher(),
        Arc::clone(&memory),
        Arc::new(NativeExecutor),
        tmp.path().to_path_buf(),
        schedule_manager,
    )
    .file_store(file_store)
    .session_writer(session_writer)
    .session_reader(SessionReader::new(tmp.path()))
    .search_index(search_index)
    .build();

    let inbound = test_inbound("How do I use Rust Vec push?");
    let session_key = SessionKey::from_inbound(&inbound);
    orch.handle_inbound(inbound, "clawhive-main").await.unwrap();

    let session = memory
        .get_session(&session_key.0)
        .await
        .unwrap()
        .expect("session should exist after inbound");
    let session_id = session.session_id.clone();
    let memory_for_check = Arc::clone(&memory);
    wait_until(Duration::from_secs(3), move || {
        let memory_for_check = Arc::clone(&memory_for_check);
        let session_id = session_id.clone();
        async move {
            let db = memory_for_check.db();
            let conn = db.lock().unwrap();
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM chunks WHERE agent_id = ?1 AND source = 'session' AND path LIKE ?2",
                    ("clawhive-main", format!("sessions/{session_id}#turn:%")),
                    |row| row.get(0),
                )
                .unwrap_or(0);
            count > 0
        }
    })
    .await;
}

#[tokio::test]
async fn boundary_flush_writes_daily_but_not_long_term_or_facts_before_consolidation() {
    let mut registry = ProviderRegistry::new();
    registry.register("summary", Arc::new(MemoryOnlySummaryProvider));
    let aliases = HashMap::from([("summary".to_string(), "summary/model".to_string())]);

    let tmp = tempfile::TempDir::new().unwrap();
    let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
    let bus = EventBus::new(16);
    let router = LlmRouter::new(registry, aliases, vec![]);
    let mut agent = test_full_agent("clawhive-main", "summary", vec![]);
    agent.workspace = Some(".".to_string());
    agent.memory_policy = Some(MemoryPolicyConfig {
        mode: "auto".to_string(),
        write_scope: "workspace".to_string(),
        idle_minutes: Some(0),
        daily_at_hour: None,
        limit_history_turns: None,
        max_injected_chars: 6000,
        daily_summary_interval: 0,
    });
    let agents = vec![agent];
    let file_store = MemoryFileStore::new(tmp.path());
    let session_writer = SessionWriter::new(tmp.path());
    let _session_reader = SessionReader::new(tmp.path());
    let search_index = SearchIndex::new(memory.db(), "test-agent");
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
            memory: &memory,
            file_store: &file_store,
            search_index: &search_index,
            embedding_provider: Arc::clone(&embedding_provider),
        },
    );

    let orch = OrchestratorBuilder::new(
        config_view,
        bus.publisher(),
        Arc::clone(&memory),
        Arc::new(NativeExecutor),
        tmp.path().to_path_buf(),
        schedule_manager,
    )
    .session_mgr(SessionManager::new(Arc::clone(&memory), 0))
    .file_store(file_store.clone())
    .session_writer(session_writer)
    .session_reader(SessionReader::new(tmp.path()))
    .search_index(search_index)
    .build();

    orch.handle_inbound(test_inbound("first turn"), "clawhive-main")
        .await
        .unwrap();
    orch.handle_inbound(test_inbound("second turn"), "clawhive-main")
        .await
        .unwrap();

    wait_until(Duration::from_secs(3), || {
        let file_store = file_store.clone();
        async move {
            file_store
                .read_daily(chrono::Utc::now().date_naive())
                .await
                .unwrap()
                .is_some_and(|daily| daily.contains("Memory refactor is now section-based"))
        }
    })
    .await;

    let long_term = file_store.read_long_term().await.unwrap();
    assert!(
        long_term.trim().is_empty(),
        "boundary flush should not promote directly into MEMORY.md before consolidation"
    );

    let facts = FactStore::new(memory.db())
        .get_active_facts("clawhive-main")
        .await
        .unwrap();
    assert!(
        facts.is_empty(),
        "memory-only boundary flush should not write facts before consolidation"
    );
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
    let search_index = SearchIndex::new(memory.db(), "test-agent");
    let embedding_provider: Arc<dyn EmbeddingProvider> = Arc::new(StubEmbeddingProvider::new(8));
    let config_view = build_test_config_view(
        agents,
        router,
        TestToolDeps {
            publisher: &bus.publisher(),
            workspace_root: tmp.path(),
            schedule_manager: Arc::clone(&schedule_manager),
            memory: &memory,
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
    let search_index = SearchIndex::new(memory.db(), "test-agent");
    let embedding_provider: Arc<dyn EmbeddingProvider> = Arc::new(StubEmbeddingProvider::new(8));
    let config_view = build_test_config_view(
        agents,
        router,
        TestToolDeps {
            publisher: &bus.publisher(),
            workspace_root: tmp.path(),
            schedule_manager: Arc::clone(&schedule_manager),
            memory: &memory,
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
