use std::collections::HashMap as StdHashMap;
use std::sync::Arc;

use anyhow::Result;
use nanocrab_bus::BusPublisher;
use nanocrab_core::{Orchestrator, RoutingConfig};
use nanocrab_schema::*;
use tokio::sync::Mutex as TokioMutex;

#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    pub requests_per_minute: u32,
    pub burst: u32,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            requests_per_minute: 30,
            burst: 10,
        }
    }
}

struct TokenBucket {
    tokens: f64,
    max_tokens: f64,
    refill_rate: f64,
    last_refill: chrono::DateTime<chrono::Utc>,
}

impl TokenBucket {
    fn new(config: &RateLimitConfig) -> Self {
        Self {
            tokens: config.burst as f64,
            max_tokens: config.burst as f64,
            refill_rate: config.requests_per_minute as f64 / 60.0,
            last_refill: chrono::Utc::now(),
        }
    }

    fn try_consume(&mut self) -> bool {
        let now = chrono::Utc::now();
        let elapsed = (now - self.last_refill).num_milliseconds() as f64 / 1000.0;
        self.tokens = (self.tokens + elapsed * self.refill_rate).min(self.max_tokens);
        self.last_refill = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

pub struct RateLimiter {
    buckets: Arc<TokioMutex<StdHashMap<String, TokenBucket>>>,
    config: RateLimitConfig,
}

impl RateLimiter {
    pub fn new(config: RateLimitConfig) -> Self {
        Self {
            buckets: Arc::new(TokioMutex::new(StdHashMap::new())),
            config,
        }
    }

    pub async fn check(&self, key: &str) -> bool {
        let mut buckets = self.buckets.lock().await;
        let bucket = buckets
            .entry(key.to_string())
            .or_insert_with(|| TokenBucket::new(&self.config));
        bucket.try_consume()
    }
}

pub struct Gateway {
    orchestrator: Arc<Orchestrator>,
    routing: RoutingConfig,
    bus: BusPublisher,
    rate_limiter: RateLimiter,
}

impl Gateway {
    pub fn new(
        orchestrator: Arc<Orchestrator>,
        routing: RoutingConfig,
        bus: BusPublisher,
        rate_limiter: RateLimiter,
    ) -> Self {
        Self {
            orchestrator,
            routing,
            bus,
            rate_limiter,
        }
    }

    pub fn resolve_agent(&self, inbound: &InboundMessage) -> String {
        for binding in &self.routing.bindings {
            if binding.channel_type == inbound.channel_type
                && binding.connector_id == inbound.connector_id
            {
                match binding.match_rule.kind.as_str() {
                    "dm" if !inbound.conversation_scope.contains("group") => {
                        return binding.agent_id.clone();
                    }
                    "mention" if inbound.is_mention => {
                        if let Some(pattern) = &binding.match_rule.pattern {
                            if inbound.mention_target.as_deref() == Some(pattern.as_str()) {
                                return binding.agent_id.clone();
                            }
                        }
                    }
                    "group" => {
                        return binding.agent_id.clone();
                    }
                    _ => {}
                }
            }
        }
        self.routing.default_agent_id.clone()
    }

    pub async fn handle_inbound(&self, inbound: InboundMessage) -> Result<OutboundMessage> {
        if !self.rate_limiter.check(&inbound.user_scope).await {
            return Err(anyhow::anyhow!("rate limited: too many requests"));
        }

        let agent_id = self.resolve_agent(&inbound);
        let trace_id = inbound.trace_id;

        let _ = self
            .bus
            .publish(BusMessage::HandleIncomingMessage {
                inbound: inbound.clone(),
                resolved_agent_id: agent_id.clone(),
            })
            .await;

        let _ = self
            .bus
            .publish(BusMessage::MessageAccepted { trace_id })
            .await;

        match self.orchestrator.handle_inbound(inbound, &agent_id).await {
            Ok(outbound) => Ok(outbound),
            Err(err) => {
                let _ = self
                    .bus
                    .publish(BusMessage::TaskFailed {
                        trace_id,
                        error: err.to_string(),
                    })
                    .await;
                Err(err)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use nanocrab_bus::{EventBus, Topic};
    use nanocrab_core::*;
    use nanocrab_memory::embedding::{EmbeddingProvider, StubEmbeddingProvider};
    use nanocrab_memory::search_index::SearchIndex;
    use nanocrab_memory::MemoryStore;
    use nanocrab_memory::{file_store::MemoryFileStore, SessionReader, SessionWriter};
    use nanocrab_provider::{register_builtin_providers, ProviderRegistry};
    use nanocrab_runtime::NativeExecutor;
    use nanocrab_schema::{BusMessage, InboundMessage};

    use super::*;

    fn make_gateway() -> (Gateway, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut registry = ProviderRegistry::new();
        register_builtin_providers(&mut registry);
        let aliases = HashMap::from([(
            "sonnet".to_string(),
            "anthropic/claude-sonnet-4-5".to_string(),
        )]);
        let router = LlmRouter::new(registry, aliases, vec![]);
        let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
        let bus = EventBus::new(16);
        let publisher = bus.publisher();
        let session_mgr = SessionManager::new(memory.clone(), 1800);
        let file_store = MemoryFileStore::new(tmp.path());
        let session_writer = SessionWriter::new(tmp.path());
        let session_reader = SessionReader::new(tmp.path());
        let search_index = SearchIndex::new(memory.db());
        let embedding_provider: Arc<dyn EmbeddingProvider> =
            Arc::new(StubEmbeddingProvider::new(8));
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
        let orch = Arc::new(Orchestrator::new(
            router,
            agents,
            HashMap::new(),
            session_mgr,
            SkillRegistry::new(),
            memory,
            publisher.clone(),
            Arc::new(NativeExecutor),
            file_store,
            session_writer,
            session_reader,
            search_index,
            embedding_provider,
            tmp.path().to_path_buf(),
            None,
        ));
        let routing = RoutingConfig {
            default_agent_id: "nanocrab-main".into(),
            bindings: vec![],
        };
        let rate_limiter = RateLimiter::new(RateLimitConfig::default());
        (Gateway::new(orch, routing, publisher, rate_limiter), tmp)
    }

    async fn make_gateway_with_receivers() -> (
        Gateway,
        tokio::sync::mpsc::Receiver<BusMessage>,
        tokio::sync::mpsc::Receiver<BusMessage>,
        tempfile::TempDir,
    ) {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut registry = ProviderRegistry::new();
        register_builtin_providers(&mut registry);
        let aliases = HashMap::from([(
            "sonnet".to_string(),
            "anthropic/claude-sonnet-4-5".to_string(),
        )]);
        let router = LlmRouter::new(registry, aliases, vec![]);
        let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
        let bus = EventBus::new(16);
        let handle_incoming_rx = bus.subscribe(Topic::HandleIncomingMessage).await;
        let message_accepted_rx = bus.subscribe(Topic::MessageAccepted).await;
        let publisher = bus.publisher();
        let session_mgr = SessionManager::new(memory.clone(), 1800);
        let file_store = MemoryFileStore::new(tmp.path());
        let session_writer = SessionWriter::new(tmp.path());
        let session_reader = SessionReader::new(tmp.path());
        let search_index = SearchIndex::new(memory.db());
        let embedding_provider: Arc<dyn EmbeddingProvider> =
            Arc::new(StubEmbeddingProvider::new(8));
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
        let orch = Arc::new(Orchestrator::new(
            router,
            agents,
            HashMap::new(),
            session_mgr,
            SkillRegistry::new(),
            memory,
            publisher.clone(),
            Arc::new(NativeExecutor),
            file_store,
            session_writer,
            session_reader,
            search_index,
            embedding_provider,
            tmp.path().to_path_buf(),
            None,
        ));
        let routing = RoutingConfig {
            default_agent_id: "nanocrab-main".into(),
            bindings: vec![],
        };
        let rate_limiter = RateLimiter::new(RateLimitConfig::default());
        (
            Gateway::new(orch, routing, publisher, rate_limiter),
            handle_incoming_rx,
            message_accepted_rx,
            tmp,
        )
    }

    #[tokio::test]
    async fn gateway_e2e_inbound_to_outbound() {
        let (gw, _tmp) = make_gateway();
        let inbound = InboundMessage {
            trace_id: uuid::Uuid::new_v4(),
            channel_type: "telegram".into(),
            connector_id: "tg_main".into(),
            conversation_scope: "chat:100".into(),
            user_scope: "user:200".into(),
            text: "ping".into(),
            at: chrono::Utc::now(),
            thread_id: None,
            is_mention: false,
            mention_target: None,
        };
        let out = gw.handle_inbound(inbound).await.unwrap();
        assert!(out.text.contains("stub:anthropic:claude-sonnet-4-5"));
    }

    #[tokio::test]
    async fn resolve_agent_default() {
        let (gw, _tmp) = make_gateway();
        let inbound = InboundMessage {
            trace_id: uuid::Uuid::new_v4(),
            channel_type: "telegram".into(),
            connector_id: "tg_main".into(),
            conversation_scope: "chat:1".into(),
            user_scope: "user:1".into(),
            text: "test".into(),
            at: chrono::Utc::now(),
            thread_id: None,
            is_mention: false,
            mention_target: None,
        };
        assert_eq!(gw.resolve_agent(&inbound), "nanocrab-main");
    }

    #[tokio::test]
    async fn resolve_agent_mention_binding() {
        let (mut gw, _tmp) = make_gateway();
        gw.routing.bindings.push(RoutingBinding {
            channel_type: "telegram".into(),
            connector_id: "tg_main".into(),
            match_rule: MatchRule {
                kind: "mention".into(),
                pattern: Some("@mybot".into()),
            },
            agent_id: "nanocrab-builder".into(),
        });
        let inbound = InboundMessage {
            trace_id: uuid::Uuid::new_v4(),
            channel_type: "telegram".into(),
            connector_id: "tg_main".into(),
            conversation_scope: "chat:1".into(),
            user_scope: "user:1".into(),
            text: "@mybot hello".into(),
            at: chrono::Utc::now(),
            thread_id: None,
            is_mention: true,
            mention_target: Some("@mybot".into()),
        };
        assert_eq!(gw.resolve_agent(&inbound), "nanocrab-builder");
    }

    #[tokio::test]
    async fn rate_limiter_allows_within_limit() {
        let limiter = RateLimiter::new(RateLimitConfig {
            requests_per_minute: 60,
            burst: 5,
        });
        for _ in 0..5 {
            assert!(limiter.check("user:1").await);
        }
    }

    #[tokio::test]
    async fn rate_limiter_blocks_after_burst() {
        let limiter = RateLimiter::new(RateLimitConfig {
            requests_per_minute: 60,
            burst: 2,
        });
        assert!(limiter.check("user:1").await);
        assert!(limiter.check("user:1").await);
        assert!(!limiter.check("user:1").await);
    }

    #[tokio::test]
    async fn rate_limiter_different_users_independent() {
        let limiter = RateLimiter::new(RateLimitConfig {
            requests_per_minute: 60,
            burst: 1,
        });
        assert!(limiter.check("user:1").await);
        assert!(limiter.check("user:2").await);
        assert!(!limiter.check("user:1").await);
    }

    #[tokio::test]
    async fn resolve_agent_dm_binding() {
        let (mut gw, _tmp) = make_gateway();
        gw.routing.bindings.push(RoutingBinding {
            channel_type: "telegram".into(),
            connector_id: "tg_main".into(),
            match_rule: MatchRule {
                kind: "dm".into(),
                pattern: None,
            },
            agent_id: "nanocrab-dm".into(),
        });
        let inbound = InboundMessage {
            trace_id: uuid::Uuid::new_v4(),
            channel_type: "telegram".into(),
            connector_id: "tg_main".into(),
            conversation_scope: "chat:private_1".into(),
            user_scope: "user:1".into(),
            text: "dm msg".into(),
            at: chrono::Utc::now(),
            thread_id: None,
            is_mention: false,
            mention_target: None,
        };
        assert_eq!(gw.resolve_agent(&inbound), "nanocrab-dm");
    }

    #[tokio::test]
    async fn resolve_agent_dm_binding_skips_group() {
        let (mut gw, _tmp) = make_gateway();
        gw.routing.bindings.push(RoutingBinding {
            channel_type: "telegram".into(),
            connector_id: "tg_main".into(),
            match_rule: MatchRule {
                kind: "dm".into(),
                pattern: None,
            },
            agent_id: "nanocrab-dm".into(),
        });
        let inbound = InboundMessage {
            trace_id: uuid::Uuid::new_v4(),
            channel_type: "telegram".into(),
            connector_id: "tg_main".into(),
            conversation_scope: "group:chat:123".into(),
            user_scope: "user:1".into(),
            text: "group msg".into(),
            at: chrono::Utc::now(),
            thread_id: None,
            is_mention: false,
            mention_target: None,
        };
        assert_eq!(gw.resolve_agent(&inbound), "nanocrab-main");
    }

    #[tokio::test]
    async fn resolve_agent_group_binding() {
        let (mut gw, _tmp) = make_gateway();
        gw.routing.bindings.push(RoutingBinding {
            channel_type: "telegram".into(),
            connector_id: "tg_main".into(),
            match_rule: MatchRule {
                kind: "group".into(),
                pattern: None,
            },
            agent_id: "nanocrab-group".into(),
        });
        let inbound = InboundMessage {
            trace_id: uuid::Uuid::new_v4(),
            channel_type: "telegram".into(),
            connector_id: "tg_main".into(),
            conversation_scope: "chat:999".into(),
            user_scope: "user:1".into(),
            text: "any msg".into(),
            at: chrono::Utc::now(),
            thread_id: None,
            is_mention: false,
            mention_target: None,
        };
        assert_eq!(gw.resolve_agent(&inbound), "nanocrab-group");
    }

    #[tokio::test]
    async fn handle_inbound_rejects_when_rate_limited() {
        let (mut gw, _tmp) = make_gateway();
        gw.rate_limiter = RateLimiter::new(RateLimitConfig {
            requests_per_minute: 60,
            burst: 1,
        });
        let make_inbound = || InboundMessage {
            trace_id: uuid::Uuid::new_v4(),
            channel_type: "telegram".into(),
            connector_id: "tg_main".into(),
            conversation_scope: "chat:1".into(),
            user_scope: "user:ratelimit_test".into(),
            text: "ping".into(),
            at: chrono::Utc::now(),
            thread_id: None,
            is_mention: false,
            mention_target: None,
        };

        let first = gw.handle_inbound(make_inbound()).await;
        assert!(first.is_ok());

        let second = gw.handle_inbound(make_inbound()).await;
        assert!(second.is_err());
        assert!(second.unwrap_err().to_string().contains("rate limited"));
    }

    #[tokio::test]
    async fn handle_inbound_publishes_handle_incoming_before_accept() {
        let (gw, mut incoming_rx, mut accepted_rx, _tmp) = make_gateway_with_receivers().await;
        let inbound = InboundMessage {
            trace_id: uuid::Uuid::new_v4(),
            channel_type: "telegram".into(),
            connector_id: "tg_main".into(),
            conversation_scope: "chat:pubtest".into(),
            user_scope: "user:pubtest".into(),
            text: "ping".into(),
            at: chrono::Utc::now(),
            thread_id: None,
            is_mention: false,
            mention_target: None,
        };

        let expected_trace = inbound.trace_id;
        let expected_conv = inbound.conversation_scope.clone();
        let expected_user = inbound.user_scope.clone();

        let _ = gw.handle_inbound(inbound).await.unwrap();

        let incoming =
            tokio::time::timeout(std::time::Duration::from_millis(200), incoming_rx.recv())
                .await
                .unwrap()
                .unwrap();
        match incoming {
            BusMessage::HandleIncomingMessage {
                inbound,
                resolved_agent_id,
            } => {
                assert_eq!(inbound.trace_id, expected_trace);
                assert_eq!(inbound.conversation_scope, expected_conv);
                assert_eq!(inbound.user_scope, expected_user);
                assert_eq!(resolved_agent_id, "nanocrab-main");
            }
            _ => panic!("expected HandleIncomingMessage event"),
        }

        let accepted =
            tokio::time::timeout(std::time::Duration::from_millis(200), accepted_rx.recv())
                .await
                .unwrap()
                .unwrap();
        assert!(matches!(
            accepted,
            BusMessage::MessageAccepted { trace_id } if trace_id == expected_trace
        ));
    }

    #[test]
    fn rate_limit_config_default_values() {
        let config = RateLimitConfig::default();
        assert_eq!(config.requests_per_minute, 30);
        assert_eq!(config.burst, 10);
    }

    #[tokio::test]
    async fn resolve_agent_mismatched_connector_uses_default() {
        let (mut gw, _tmp) = make_gateway();
        gw.routing.bindings.push(RoutingBinding {
            channel_type: "telegram".into(),
            connector_id: "tg_other".into(),
            match_rule: MatchRule {
                kind: "dm".into(),
                pattern: None,
            },
            agent_id: "nanocrab-other".into(),
        });
        let inbound = InboundMessage {
            trace_id: uuid::Uuid::new_v4(),
            channel_type: "telegram".into(),
            connector_id: "tg_main".into(),
            conversation_scope: "chat:1".into(),
            user_scope: "user:1".into(),
            text: "test".into(),
            at: chrono::Utc::now(),
            thread_id: None,
            is_mention: false,
            mention_target: None,
        };
        assert_eq!(gw.resolve_agent(&inbound), "nanocrab-main");
    }
}
