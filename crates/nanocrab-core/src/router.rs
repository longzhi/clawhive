use std::collections::HashMap;

use anyhow::{anyhow, Result};
use nanocrab_provider::{LlmMessage, LlmRequest, LlmResponse, ProviderRegistry};

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

    pub async fn chat(
        &self,
        primary: &str,
        fallbacks: &[String],
        system: Option<String>,
        messages: Vec<LlmMessage>,
        max_tokens: u32,
    ) -> Result<LlmResponse> {
        let mut candidates = vec![primary.to_string()];
        candidates.extend(fallbacks.iter().cloned());
        candidates.extend(self.global_fallbacks.clone());

        let mut last_err: Option<anyhow::Error> = None;

        for candidate in candidates {
            let resolved = self.resolve_model(&candidate)?;
            let (provider_id, model_id) = parse_provider_model(&resolved)?;
            let provider = self.registry.get(&provider_id)?;

            let req = LlmRequest {
                model: model_id,
                system: system.clone(),
                messages: messages.clone(),
                max_tokens,
            };

            match provider.chat(req).await {
                Ok(resp) => return Ok(resp),
                Err(err) => {
                    tracing::warn!("provider {provider_id} failed: {err}");
                    last_err = Some(err);
                }
            }
        }

        Err(last_err.unwrap_or_else(|| anyhow!("no model candidate available")))
    }

    pub async fn reply(&self, agent: &super::AgentConfig, user_text: &str) -> Result<String> {
        let messages = vec![LlmMessage {
            role: "user".into(),
            content: user_text.to_string(),
        }];
        let resp = self
            .chat(
                &agent.model_policy.primary,
                &agent.model_policy.fallbacks,
                Some(format!("agent_id={}", agent.agent_id)),
                messages,
                2048,
            )
            .await?;
        Ok(resp.text)
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
