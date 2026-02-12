use std::collections::HashMap;

use anyhow::{anyhow, Result};
use nanocrab_provider::{LlmRequest, ProviderRegistry};
use nanocrab_schema::{InboundMessage, OutboundMessage};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPolicy {
    pub primary: String,
    #[serde(default)]
    pub fallbacks: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub agent_id: String,
    pub enabled: bool,
    pub model_policy: ModelPolicy,
}

pub struct LlmRouter {
    registry: ProviderRegistry,
    aliases: HashMap<String, String>,
    global_fallbacks: Vec<String>,
}

impl LlmRouter {
    pub fn new(
        registry: ProviderRegistry,
        aliases: HashMap<String, String>,
        global_fallbacks: Vec<String>,
    ) -> Self {
        Self {
            registry,
            aliases,
            global_fallbacks,
        }
    }

    pub async fn reply(&self, agent: &AgentConfig, user_text: &str) -> Result<String> {
        let mut candidates = vec![agent.model_policy.primary.clone()];
        candidates.extend(agent.model_policy.fallbacks.clone());
        candidates.extend(self.global_fallbacks.clone());

        let mut last_err: Option<anyhow::Error> = None;

        for candidate in candidates {
            let resolved = self.resolve_model(&candidate)?;
            let (provider_id, model_id) = parse_provider_model(&resolved)?;
            let provider = self.registry.build(&provider_id)?;

            let req = LlmRequest {
                model: model_id,
                system: Some(format!("agent_id={}", agent.agent_id)),
                user: user_text.to_string(),
            };

            match provider.chat(req).await {
                Ok(resp) => return Ok(resp.text),
                Err(err) => last_err = Some(err),
            }
        }

        Err(last_err.unwrap_or_else(|| anyhow!("no model candidate available")))
    }

    fn resolve_model(&self, raw: &str) -> Result<String> {
        if raw.contains('/') {
            return Ok(raw.to_string());
        }
        self.aliases
            .get(raw)
            .cloned()
            .ok_or_else(|| anyhow!("unknown model alias: {raw}"))
    }
}

fn parse_provider_model(input: &str) -> Result<(String, String)> {
    let mut parts = input.splitn(2, '/');
    let provider = parts
        .next()
        .ok_or_else(|| anyhow!("invalid model format: {input}"))?;
    let model = parts
        .next()
        .ok_or_else(|| anyhow!("invalid model format: {input}"))?;
    Ok((provider.to_string(), model.to_string()))
}

pub struct Orchestrator {
    router: LlmRouter,
    agents: HashMap<String, AgentConfig>,
}

impl Orchestrator {
    pub fn new(router: LlmRouter, agents: Vec<AgentConfig>) -> Self {
        let mut map = HashMap::new();
        for a in agents {
            map.insert(a.agent_id.clone(), a);
        }
        Self { router, agents: map }
    }

    pub async fn handle_inbound(&self, inbound: InboundMessage, agent_id: &str) -> Result<OutboundMessage> {
        let agent = self
            .agents
            .get(agent_id)
            .ok_or_else(|| anyhow!("agent not found: {agent_id}"))?;
        let text = self.router.reply(agent, &inbound.text).await?;

        Ok(OutboundMessage {
            trace_id: inbound.trace_id,
            channel_type: inbound.channel_type,
            connector_id: inbound.connector_id,
            conversation_scope: inbound.conversation_scope,
            text,
            at: chrono::Utc::now(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use async_trait::async_trait;
    use nanocrab_provider::{register_builtin_providers, LlmProvider, LlmResponse};
    use nanocrab_schema::InboundMessage;
    use uuid::Uuid;

    struct FailProvider;

    #[async_trait]
    impl LlmProvider for FailProvider {
        async fn chat(&self, _request: LlmRequest) -> Result<LlmResponse> {
            Err(anyhow!("forced failure"))
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
        registry.register("broken", || Box::new(FailProvider));
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
        let router = LlmRouter::new(registry, aliases, vec![]);
        let orch = Orchestrator::new(router, vec![test_agent("sonnet", vec![])]);

        let inbound = InboundMessage {
            trace_id: Uuid::new_v4(),
            channel_type: "telegram".to_string(),
            connector_id: "tg_main".to_string(),
            conversation_scope: "chat:1".to_string(),
            user_scope: "user:1".to_string(),
            text: "hello".to_string(),
            at: chrono::Utc::now(),
        };

        let out = orch.handle_inbound(inbound, "nanocrab-main").await.unwrap();
        assert!(out.text.contains("stub:anthropic:claude-sonnet-4-5"));
    }
}
