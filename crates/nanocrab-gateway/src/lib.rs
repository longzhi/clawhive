use anyhow::Result;
use nanocrab_core::Orchestrator;
use nanocrab_schema::{InboundMessage, OutboundMessage};

pub struct Gateway {
    orchestrator: Orchestrator,
    default_agent_id: String,
}

impl Gateway {
    pub fn new(orchestrator: Orchestrator, default_agent_id: impl Into<String>) -> Self {
        Self {
            orchestrator,
            default_agent_id: default_agent_id.into(),
        }
    }

    pub async fn handle_inbound(&self, inbound: InboundMessage) -> Result<OutboundMessage> {
        self.orchestrator
            .handle_inbound(inbound, &self.default_agent_id)
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use nanocrab_core::{AgentConfig, LlmRouter, ModelPolicy, Orchestrator};
    use nanocrab_provider::{register_builtin_providers, ProviderRegistry};

    use super::*;

    #[tokio::test]
    async fn gateway_e2e_inbound_to_outbound() {
        let mut registry = ProviderRegistry::new();
        register_builtin_providers(&mut registry);

        let aliases = HashMap::from([(
            "sonnet".to_string(),
            "anthropic/claude-sonnet-4-5".to_string(),
        )]);

        let router = LlmRouter::new(registry, aliases, vec![]);
        let agent = AgentConfig {
            agent_id: "nanocrab-main".to_string(),
            enabled: true,
            model_policy: ModelPolicy {
                primary: "sonnet".to_string(),
                fallbacks: vec![],
            },
        };
        let orch = Orchestrator::new(router, vec![agent]);
        let gw = Gateway::new(orch, "nanocrab-main");

        let inbound = InboundMessage {
            trace_id: uuid::Uuid::new_v4(),
            channel_type: "telegram".to_string(),
            connector_id: "tg_main".to_string(),
            conversation_scope: "chat:100".to_string(),
            user_scope: "user:200".to_string(),
            text: "ping".to_string(),
            at: chrono::Utc::now(),
        };

        let out = gw.handle_inbound(inbound).await.unwrap();
        assert!(out.text.contains("stub:anthropic:claude-sonnet-4-5"));
    }
}
