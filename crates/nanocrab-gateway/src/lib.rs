use std::sync::Arc;
use std::collections::HashMap as StdHashMap;

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

    use nanocrab_bus::EventBus;
    use nanocrab_core::*;
    use nanocrab_memory::MemoryStore;
    use nanocrab_provider::{register_builtin_providers, ProviderRegistry};
    use nanocrab_schema::InboundMessage;

    use super::*;

    fn make_gateway() -> Gateway {
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
        ));
        let routing = RoutingConfig {
            default_agent_id: "nanocrab-main".into(),
            bindings: vec![],
        };
        let rate_limiter = RateLimiter::new(RateLimitConfig::default());
        Gateway::new(orch, routing, publisher, rate_limiter)
    }

    #[tokio::test]
    async fn gateway_e2e_inbound_to_outbound() {
        let gw = make_gateway();
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
        let gw = make_gateway();
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
        let mut gw = make_gateway();
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
}
