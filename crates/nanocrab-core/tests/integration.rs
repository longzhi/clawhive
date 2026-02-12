use std::collections::HashMap;
use std::sync::Arc;

use anyhow::anyhow;
use async_trait::async_trait;
use nanocrab_bus::EventBus;
use nanocrab_core::*;
use nanocrab_memory::MemoryStore;
use nanocrab_provider::{
    register_builtin_providers, LlmProvider, LlmRequest, LlmResponse, ProviderRegistry,
};
use nanocrab_runtime::NativeExecutor;
use nanocrab_schema::{BusMessage, InboundMessage, SessionKey};
use uuid::Uuid;

struct FailProvider;
struct EchoProvider;
struct ThinkingEchoProvider;

#[async_trait]
impl LlmProvider for FailProvider {
    async fn chat(&self, _request: LlmRequest) -> anyhow::Result<LlmResponse> {
        Err(anyhow!("forced failure"))
    }
}

#[async_trait]
impl LlmProvider for EchoProvider {
    async fn chat(&self, request: LlmRequest) -> anyhow::Result<LlmResponse> {
        Ok(LlmResponse {
            text: request
                .messages
                .last()
                .map(|m| m.content.clone())
                .unwrap_or_default(),
            input_tokens: None,
            output_tokens: None,
            stop_reason: Some("end_turn".into()),
        })
    }
}

#[async_trait]
impl LlmProvider for ThinkingEchoProvider {
    async fn chat(&self, _request: LlmRequest) -> anyhow::Result<LlmResponse> {
        Ok(LlmResponse {
            text: "[think] still processing".to_string(),
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
    }
}

fn test_agent(primary: &str, fallbacks: Vec<&str>) -> AgentConfig {
    AgentConfig {
        agent_id: "nanocrab-main".to_string(),
        enabled: true,
        model_policy: ModelPolicy {
            primary: primary.to_string(),
            fallbacks: fallbacks.into_iter().map(|s| s.to_string()).collect(),
        },
    }
}

fn test_full_agent(agent_id: &str, primary: &str, fallbacks: Vec<&str>) -> FullAgentConfig {
    FullAgentConfig {
        agent_id: agent_id.to_string(),
        enabled: true,
        identity: None,
        model_policy: ModelPolicy {
            primary: primary.to_string(),
            fallbacks: fallbacks.into_iter().map(|s| s.to_string()).collect(),
        },
        tool_policy: None,
        memory_policy: None,
        sub_agent: None,
    }
}

fn make_orchestrator(
    registry: ProviderRegistry,
    aliases: HashMap<String, String>,
    agents: Vec<FullAgentConfig>,
) -> Orchestrator {
    let router = LlmRouter::new(registry, aliases, vec![]);
    let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
    let bus = EventBus::new(16);
    let publisher = bus.publisher();
    let session_mgr = SessionManager::new(memory.clone(), 1800);

    Orchestrator::new(
        router,
        agents,
        HashMap::new(),
        session_mgr,
        SkillRegistry::new(),
        memory,
        publisher,
        Arc::new(NativeExecutor),
    )
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
    assert!(err.to_string().contains("unknown model alias"));
}

#[tokio::test]
async fn orchestrator_handles_inbound_to_outbound() {
    let mut registry = ProviderRegistry::new();
    register_builtin_providers(&mut registry);
    let aliases = HashMap::from([(
        "sonnet".to_string(),
        "anthropic/claude-sonnet-4-5".to_string(),
    )]);
    let agents = vec![test_full_agent("nanocrab-main", "sonnet", vec![])];
    let orch = make_orchestrator(registry, aliases, agents);

    let out = orch
        .handle_inbound(test_inbound("hello"), "nanocrab-main")
        .await
        .unwrap();
    assert!(out.text.contains("stub:anthropic:claude-sonnet-4-5"));
}

#[tokio::test]
async fn weak_react_stops_on_repeat_guard() {
    let mut registry = ProviderRegistry::new();
    registry.register("echo", Arc::new(ThinkingEchoProvider));

    let aliases = HashMap::from([("echo".to_string(), "echo/model".to_string())]);
    let agents = vec![test_full_agent("nanocrab-main", "echo", vec![])];
    let orch = make_orchestrator(registry, aliases, agents).with_react_config(WeakReActConfig {
        max_steps: 4,
        repeat_guard: 1,
    });

    let out = orch
        .handle_inbound(test_inbound("loop"), "nanocrab-main")
        .await
        .unwrap();
    assert!(out.text.contains("weak-react"));
}

#[tokio::test]
async fn orchestrator_new_with_full_deps() {
    let mut registry = ProviderRegistry::new();
    register_builtin_providers(&mut registry);
    let aliases = HashMap::from([(
        "sonnet".to_string(),
        "anthropic/claude-sonnet-4-5".to_string(),
    )]);
    let agents = vec![test_full_agent("nanocrab-main", "sonnet", vec![])];
    let orch = make_orchestrator(registry, aliases, agents);

    let out = orch
        .handle_inbound(test_inbound("hello"), "nanocrab-main")
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
    let session_mgr = SessionManager::new(memory.clone(), 1800);
    let agents = vec![test_full_agent("nanocrab-main", "sonnet", vec![])];
    let router = LlmRouter::new(registry, aliases, vec![]);
    let orch = Orchestrator::new(
        router,
        agents,
        HashMap::new(),
        session_mgr,
        SkillRegistry::new(),
        memory.clone(),
        bus.publisher(),
        Arc::new(NativeExecutor),
    );

    let inbound = test_inbound("hello");
    let key = SessionKey::from_inbound(&inbound);
    let _ = orch.handle_inbound(inbound, "nanocrab-main").await.unwrap();

    let session = memory.get_session(&key.0).await.unwrap();
    assert!(session.is_some());
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
    let mut rx = bus.subscribe(nanocrab_bus::Topic::ReplyReady).await;
    let session_mgr = SessionManager::new(memory.clone(), 1800);
    let agents = vec![test_full_agent("nanocrab-main", "sonnet", vec![])];
    let router = LlmRouter::new(registry, aliases, vec![]);
    let orch = Orchestrator::new(
        router,
        agents,
        HashMap::new(),
        session_mgr,
        SkillRegistry::new(),
        memory,
        bus.publisher(),
        Arc::new(NativeExecutor),
    );

    let _ = orch
        .handle_inbound(test_inbound("hello"), "nanocrab-main")
        .await
        .unwrap();

    let event = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(event, BusMessage::ReplyReady { .. }));
}
