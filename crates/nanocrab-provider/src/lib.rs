use std::collections::HashMap;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmRequest {
    pub model: String,
    pub system: Option<String>,
    pub user: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmResponse {
    pub text: String,
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn chat(&self, request: LlmRequest) -> Result<LlmResponse>;
    async fn health(&self) -> Result<()> {
        Ok(())
    }
}

pub type ProviderFactory = Box<dyn Fn() -> Box<dyn LlmProvider> + Send + Sync>;

pub struct ProviderRegistry {
    factories: HashMap<String, ProviderFactory>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self {
            factories: HashMap::new(),
        }
    }

    pub fn register<F>(&mut self, provider_id: impl Into<String>, factory: F)
    where
        F: Fn() -> Box<dyn LlmProvider> + Send + Sync + 'static,
    {
        self.factories.insert(provider_id.into(), Box::new(factory));
    }

    pub fn build(&self, provider_id: &str) -> Result<Box<dyn LlmProvider>> {
        let factory = self
            .factories
            .get(provider_id)
            .ok_or_else(|| anyhow!("provider not found: {provider_id}"))?;
        Ok(factory())
    }
}

pub struct AnthropicProvider;

#[async_trait]
impl LlmProvider for AnthropicProvider {
    async fn chat(&self, request: LlmRequest) -> Result<LlmResponse> {
        let text = format!(
            "[stub:anthropic:{}] {}",
            request.model,
            request.user
        );
        Ok(LlmResponse { text })
    }
}

pub fn register_builtin_providers(registry: &mut ProviderRegistry) {
    registry.register("anthropic", || Box::new(AnthropicProvider));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn registry_builds_anthropic_and_returns_stub() {
        let mut registry = ProviderRegistry::new();
        register_builtin_providers(&mut registry);

        let provider = registry.build("anthropic").unwrap();
        let out = provider
            .chat(LlmRequest {
                model: "claude-sonnet-4-5".to_string(),
                system: None,
                user: "hello".to_string(),
            })
            .await
            .unwrap();

        assert!(out.text.contains("stub:anthropic:claude-sonnet-4-5"));
    }
}
