use std::sync::Arc;

use anyhow::Result;
use nanocrab_bus::BusPublisher;
use nanocrab_core::{Orchestrator, RoutingConfig};
use nanocrab_schema::*;

pub struct Gateway {
    orchestrator: Arc<Orchestrator>,
    routing: RoutingConfig,
    bus: BusPublisher,
}

impl Gateway {
    pub fn new(orchestrator: Arc<Orchestrator>, routing: RoutingConfig, bus: BusPublisher) -> Self {
        Self {
            orchestrator,
            routing,
            bus,
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
            memory,
            publisher.clone(),
        ));
        let routing = RoutingConfig {
            default_agent_id: "nanocrab-main".into(),
            bindings: vec![],
        };
        Gateway::new(orch, routing, publisher)
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
}
