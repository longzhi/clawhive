use std::collections::HashMap;
use std::sync::Arc;

use nanocrab_bus::{EventBus, Topic};
use nanocrab_core::*;
use nanocrab_memory::embedding::{EmbeddingProvider, StubEmbeddingProvider};
use nanocrab_memory::search_index::SearchIndex;
use nanocrab_memory::MemoryStore;
use nanocrab_memory::{file_store::MemoryFileStore, SessionReader, SessionWriter};
use nanocrab_provider::{AnthropicProvider, LlmMessage, LlmProvider, LlmRequest, ProviderRegistry};
use nanocrab_runtime::NativeExecutor;
use nanocrab_schema::{BusMessage, InboundMessage, SessionKey};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn test_inbound(text: &str) -> InboundMessage {
    InboundMessage {
        trace_id: uuid::Uuid::new_v4(),
        channel_type: "telegram".into(),
        connector_id: "tg_main".into(),
        conversation_scope: "chat:1".into(),
        user_scope: "user:1".into(),
        text: text.into(),
        at: chrono::Utc::now(),
        thread_id: None,
        is_mention: false,
        mention_target: None,
    }
}

fn mock_anthropic_response(text: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "msg_test123",
        "type": "message",
        "role": "assistant",
        "content": [{"type": "text", "text": text}],
        "model": "claude-sonnet-4-5",
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 10, "output_tokens": 20}
    })
}

fn mock_anthropic_error(status: u16, message: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(serde_json::json!({
        "type": "error",
        "error": {
            "type": "api_error",
            "message": message
        }
    }))
}

fn make_orchestrator_with_provider(
    provider: Arc<dyn LlmProvider>,
    memory: Arc<MemoryStore>,
    bus: &EventBus,
) -> (Orchestrator, tempfile::TempDir) {
    let tmp = tempfile::TempDir::new().unwrap();
    let mut registry = ProviderRegistry::new();
    registry.register("anthropic", provider);
    let aliases = HashMap::from([(
        "sonnet".to_string(),
        "anthropic/claude-sonnet-4-5".to_string(),
    )]);
    let router = LlmRouter::new(registry, aliases, vec![]);
    let session_mgr = SessionManager::new(memory.clone(), 1800);
    let agents = vec![FullAgentConfig {
        agent_id: "nanocrab-main".into(),
        enabled: true,
        identity: None,
        model_policy: ModelPolicy {
            primary: "sonnet".into(),
            fallbacks: vec![],
        },
        tool_policy: None,
        memory_policy: None,
        sub_agent: None,
    }];
    let file_store = MemoryFileStore::new(tmp.path());
    let session_writer = SessionWriter::new(tmp.path());
    let session_reader = SessionReader::new(tmp.path());
    let search_index = SearchIndex::new(memory.db());
    let embedding_provider: Arc<dyn EmbeddingProvider> = Arc::new(StubEmbeddingProvider::new(8));
    (
        Orchestrator::new(
            router,
            agents,
            HashMap::new(),
            session_mgr,
            SkillRegistry::new(),
            memory,
            bus.publisher(),
            Arc::new(NativeExecutor),
            file_store,
            session_writer,
            session_reader,
            search_index,
            embedding_provider,
            tmp.path().to_path_buf(),
            None,
        )
        .with_react_config(WeakReActConfig {
            max_steps: 1,
            repeat_guard: 1,
        }),
        tmp,
    )
}

async fn mount_success(server: &MockServer, text: &str) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mock_anthropic_response(text)))
        .mount(server)
        .await;
}

#[tokio::test]
async fn mock_server_e2e_chat() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "test-key"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(mock_anthropic_response("Hello from mock!")),
        )
        .expect(1)
        .mount(&server)
        .await;

    let provider = Arc::new(AnthropicProvider::new("test-key", server.uri()));
    let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
    let bus = EventBus::new(16);
    let (orch, _tmp) = make_orchestrator_with_provider(provider, memory, &bus);

    let out = orch
        .handle_inbound(test_inbound("hi"), "nanocrab-main")
        .await
        .unwrap();
    assert!(out.text.contains("Hello from mock!"));
}

#[tokio::test]
async fn mock_server_records_episodes() {
    let server = MockServer::start().await;
    mount_success(&server, "episode reply").await;

    let provider = Arc::new(AnthropicProvider::new("test-key", server.uri()));
    let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
    let bus = EventBus::new(16);
    let (orch, _tmp) = make_orchestrator_with_provider(provider, memory.clone(), &bus);

    let inbound = test_inbound("episode input");
    let key = SessionKey::from_inbound(&inbound);
    let out = orch.handle_inbound(inbound, "nanocrab-main").await.unwrap();
    assert!(out.text.contains("episode reply"));

    let episodes = memory.recent_episodes(&key.0, 10).await.unwrap();
    assert_eq!(episodes.len(), 2);
    assert!(episodes
        .iter()
        .any(|e| e.speaker == "user" && e.text == "episode input"));
    assert!(episodes
        .iter()
        .any(|e| e.speaker == "nanocrab-main" && e.text.contains("episode reply")));
}

#[tokio::test]
async fn mock_server_creates_session() {
    let server = MockServer::start().await;
    mount_success(&server, "session reply").await;

    let provider = Arc::new(AnthropicProvider::new("test-key", server.uri()));
    let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
    let bus = EventBus::new(16);
    let (orch, _tmp) = make_orchestrator_with_provider(provider, memory.clone(), &bus);

    let inbound = test_inbound("session input");
    let key = SessionKey::from_inbound(&inbound);
    let _ = orch.handle_inbound(inbound, "nanocrab-main").await.unwrap();

    let session = memory.get_session(&key.0).await.unwrap();
    assert!(session.is_some());
    assert_eq!(key.0, "telegram:tg_main:chat:1:user:1");
}

#[tokio::test]
async fn mock_server_publishes_bus_events() {
    let server = MockServer::start().await;
    mount_success(&server, "bus reply").await;

    let provider = Arc::new(AnthropicProvider::new("test-key", server.uri()));
    let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
    let bus = EventBus::new(16);
    let mut rx = bus.subscribe(Topic::ReplyReady).await;
    let (orch, _tmp) = make_orchestrator_with_provider(provider, memory, &bus);

    let _ = orch
        .handle_inbound(test_inbound("bus input"), "nanocrab-main")
        .await
        .unwrap();

    let event = tokio::time::timeout(std::time::Duration::from_millis(500), rx.recv())
        .await
        .unwrap()
        .unwrap();
    match event {
        BusMessage::ReplyReady { outbound } => {
            assert!(outbound.text.contains("bus reply"));
        }
        _ => panic!("unexpected event"),
    }
}

#[tokio::test]
async fn mock_server_handles_api_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(mock_anthropic_error(500, "upstream failure"))
        .mount(&server)
        .await;

    let provider = Arc::new(AnthropicProvider::new("test-key", server.uri()));
    let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
    let bus = EventBus::new(16);
    let (orch, _tmp) = make_orchestrator_with_provider(provider, memory, &bus);

    let err = orch
        .handle_inbound(test_inbound("error input"), "nanocrab-main")
        .await
        .unwrap_err();
    let err_text = err.to_string();
    assert!(err_text.contains("anthropic api error"));
    assert!(err_text.contains("retryable"));
}

#[tokio::test]
async fn mock_server_handles_rate_limit() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(mock_anthropic_error(429, "rate limited"))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(mock_anthropic_response("retry success")),
        )
        .mount(&server)
        .await;

    let mut registry = ProviderRegistry::new();
    registry.register(
        "anthropic",
        Arc::new(AnthropicProvider::new("test-key", server.uri())),
    );
    let aliases = HashMap::from([(
        "sonnet".to_string(),
        "anthropic/claude-sonnet-4-5".to_string(),
    )]);
    let router = LlmRouter::new(registry, aliases, vec![]);

    let resp = router
        .chat(
            "sonnet",
            &[],
            None,
            vec![LlmMessage::user("please retry")],
            128,
        )
        .await
        .unwrap();

    assert!(resp.text.contains("retry success"));
}

#[tokio::test]
async fn mock_server_fallback_on_failure() {
    let primary = MockServer::start().await;
    let fallback = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(mock_anthropic_error(500, "primary failed"))
        .mount(&primary)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(mock_anthropic_response("fallback success")),
        )
        .expect(1)
        .mount(&fallback)
        .await;

    let mut registry = ProviderRegistry::new();
    registry.register(
        "primary",
        Arc::new(AnthropicProvider::new("test-key", primary.uri())),
    );
    registry.register(
        "fallback",
        Arc::new(AnthropicProvider::new("test-key", fallback.uri())),
    );

    let agent = AgentConfig {
        agent_id: "nanocrab-main".to_string(),
        enabled: true,
        model_policy: ModelPolicy {
            primary: "primary/claude-sonnet-4-5".to_string(),
            fallbacks: vec!["fallback/claude-sonnet-4-5".to_string()],
        },
    };

    let router = LlmRouter::new(registry, HashMap::new(), vec![]);
    let out = router.reply(&agent, "fallback please").await.unwrap();
    assert!(out.contains("fallback success"));
}

#[tokio::test]
async fn mock_server_validates_request_headers() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "test-key"))
        .and(header("anthropic-version", "2023-06-01"))
        .and(header("content-type", "application/json"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(mock_anthropic_response("header ok")),
        )
        .expect(1)
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new("test-key", server.uri());
    let resp = provider
        .chat(LlmRequest {
            model: "claude-sonnet-4-5".into(),
            system: Some("sys".into()),
            messages: vec![LlmMessage::user("check headers")],
            max_tokens: 128,
            tools: vec![],
        })
        .await
        .unwrap();

    assert!(resp.text.contains("header ok"));
}

#[tokio::test]
async fn mock_server_handles_connection_error() {
    let provider = AnthropicProvider::new("test-key", "http://127.0.0.1:9");
    let err = provider
        .chat(LlmRequest {
            model: "claude-sonnet-4-5".into(),
            system: None,
            messages: vec![LlmMessage::user("ping")],
            max_tokens: 64,
            tools: vec![],
        })
        .await
        .unwrap_err();

    let err_text = err.to_string();
    assert!(err_text.contains("anthropic api error (connect)"));
    assert!(err_text.contains("[retryable]"));
}

#[tokio::test]
async fn mock_server_multi_turn_session() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(mock_anthropic_response("multi turn")),
        )
        .mount(&server)
        .await;

    let provider = Arc::new(AnthropicProvider::new("test-key", server.uri()));
    let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
    let bus = EventBus::new(16);
    let (orch, _tmp) = make_orchestrator_with_provider(provider, memory.clone(), &bus);

    let first = test_inbound("first");
    let key = SessionKey::from_inbound(&first);
    let first_out = orch.handle_inbound(first, "nanocrab-main").await.unwrap();
    assert!(first_out.text.contains("multi turn"));

    let second = test_inbound("second");
    let second_out = orch.handle_inbound(second, "nanocrab-main").await.unwrap();
    assert!(second_out.text.contains("multi turn"));

    let episodes = memory.recent_episodes(&key.0, 10).await.unwrap();
    assert_eq!(episodes.len(), 4);
    assert_eq!(episodes.iter().filter(|e| e.speaker == "user").count(), 2);
    assert_eq!(
        episodes
            .iter()
            .filter(|e| e.speaker == "nanocrab-main")
            .count(),
        2
    );
}

#[tokio::test]
async fn mock_server_includes_session_history() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(mock_anthropic_response("reply with history")),
        )
        .mount(&server)
        .await;

    let provider = Arc::new(AnthropicProvider::new("test-key", server.uri()));
    let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
    let bus = EventBus::new(16);
    let (orch, tmp) = make_orchestrator_with_provider(provider, memory.clone(), &bus);

    // First turn
    let first = test_inbound("hello");
    let _ = orch.handle_inbound(first, "nanocrab-main").await.unwrap();

    // Second turn — session history should now include the first turn
    let second = test_inbound("follow up");
    let _ = orch.handle_inbound(second, "nanocrab-main").await.unwrap();

    // Verify: the session JSONL should have 4 messages (user+assistant x2)
    let reader = SessionReader::new(tmp.path());
    let key_str = "telegram:tg_main:chat:1:user:1";
    let messages = reader.load_recent_messages(key_str, 20).await.unwrap();
    assert_eq!(
        messages.len(),
        4,
        "Should have 4 messages: 2 user + 2 assistant"
    );
}

#[tokio::test]
async fn mock_server_tool_use_loop() {
    let server = MockServer::start().await;

    let tool_use_response = serde_json::json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "content": [
            {"type": "text", "text": "Let me search memory..."},
            {"type": "tool_use", "id": "toolu_1", "name": "memory_search", "input": {"query": "test"}}
        ],
        "model": "claude-sonnet-4-5",
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 10, "output_tokens": 20}
    });

    let final_response = mock_anthropic_response("Here is what I found in memory.");

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(tool_use_response))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(final_response))
        .mount(&server)
        .await;

    let provider = Arc::new(AnthropicProvider::new("test-key", server.uri()));
    let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
    let bus = EventBus::new(16);
    let (orch, _tmp) = make_orchestrator_with_provider(provider, memory, &bus);

    let out = orch
        .handle_inbound(test_inbound("search my memory"), "nanocrab-main")
        .await
        .unwrap();
    assert!(out.text.contains("Here is what I found"));
}
