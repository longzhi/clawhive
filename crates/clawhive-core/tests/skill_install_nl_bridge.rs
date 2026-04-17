use std::collections::HashMap;
use std::sync::Arc;

use clawhive_bus::EventBus;
use clawhive_core::{
    build_tool_registry, detect_skill_install_intent, ConfigView, FullAgentConfig, LlmRouter,
    ModelPolicy, Orchestrator, OrchestratorBuilder, RoutingConfig, SecurityMode,
};
use clawhive_memory::embedding::{EmbeddingProvider, StubEmbeddingProvider};
use clawhive_memory::file_store::MemoryFileStore;
use clawhive_memory::search_index::SearchIndex;
use clawhive_memory::MemoryStore;
use clawhive_provider::ProviderRegistry;
use clawhive_runtime::NativeExecutor;
use clawhive_scheduler::{ScheduleManager, SqliteStore};
use clawhive_schema::InboundMessage;
use uuid::Uuid;

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

fn test_full_agent() -> FullAgentConfig {
    FullAgentConfig {
        agent_id: "clawhive-main".to_string(),
        enabled: true,
        security: SecurityMode::default(),
        identity: None,
        model_policy: ModelPolicy {
            primary: "stub".to_string(),
            fallbacks: vec![],
            thinking_level: None,
            context_window: None,
            compaction_model: None,
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
        turn_timeout_secs: None,
        typing_ttl_secs: None,
        progress_delay_secs: None,
    }
}

async fn make_orchestrator() -> (Orchestrator, tempfile::TempDir) {
    let tmp = tempfile::TempDir::new().unwrap();
    let router = LlmRouter::new(ProviderRegistry::new(), HashMap::new(), vec![]);
    let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
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
    let publisher = EventBus::new(16).publisher();
    let agents = vec![test_full_agent()];
    let personas = HashMap::new();
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
        &personas,
    );
    let config_view = ConfigView::new(
        0,
        agents,
        personas,
        RoutingConfig {
            default_agent_id: "clawhive-main".to_string(),
            bindings: vec![],
        },
        router,
        tool_registry,
        embedding_provider,
    );

    let orchestrator = OrchestratorBuilder::new(
        config_view,
        publisher,
        memory,
        Arc::new(NativeExecutor),
        tmp.path().to_path_buf(),
        schedule_manager,
    )
    .build();

    (orchestrator, tmp)
}

fn create_skill(root: &std::path::Path, name: &str) -> std::path::PathBuf {
    let skill_dir = root.join(name);
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: Test skill\n---\n\nThis is a test skill."),
    )
    .unwrap();
    std::fs::write(skill_dir.join("run.sh"), "#!/bin/sh\nprintf 'hello'\n").unwrap();
    skill_dir
}

#[test]
fn detects_chinese_install_skill_with_url() {
    let source = detect_skill_install_intent("安装这个 skill: https://example.com/skill.zip");
    assert_eq!(source.as_deref(), Some("https://example.com/skill.zip"));
}

#[test]
fn detects_english_install_skill_from_path() {
    let source = detect_skill_install_intent("install skill from /tmp/my-skill");
    assert_eq!(source.as_deref(), Some("/tmp/my-skill"));
}

#[test]
fn detects_english_install_this_skill_with_url() {
    let source = detect_skill_install_intent("install this skill https://example.com/test.tar.gz");
    assert_eq!(source.as_deref(), Some("https://example.com/test.tar.gz"));
}

#[tokio::test]
async fn no_source_returns_usage_hint_without_side_effects() {
    let (orchestrator, tmp) = make_orchestrator().await;
    let out = orchestrator
        .handle_inbound(
            test_inbound("install skill"),
            "clawhive-main",
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();

    assert_eq!(
        out.text,
        "请提供 skill 来源路径或 URL。用法: /skill install <source>"
    );
    assert!(!tmp.path().join("skills").exists());
}

#[test]
fn normal_messages_are_not_detected() {
    assert_eq!(detect_skill_install_intent("hello"), None);
    assert_eq!(detect_skill_install_intent("tell me a joke"), None);
}

#[tokio::test]
async fn detected_nl_install_routes_to_analyze_flow() {
    let (orchestrator, tmp) = make_orchestrator().await;
    let source = create_skill(tmp.path(), "nl-bridge-skill");
    let msg = format!("install skill from {}", source.display());

    let out = orchestrator
        .handle_inbound(
            test_inbound(&msg),
            "clawhive-main",
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();

    assert!(out.text.contains("Install request analyzed."));
    assert!(out.text.contains("/skill confirm"));
}
