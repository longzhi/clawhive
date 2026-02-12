pub mod anthropic;
pub mod types;

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;

pub use anthropic::AnthropicProvider;
pub use types::*;

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn chat(&self, request: LlmRequest) -> Result<LlmResponse>;
    async fn stream(&self, _request: LlmRequest) -> Result<LlmResponse> {
        anyhow::bail!("streaming not supported by this provider")
    }
    async fn health(&self) -> Result<()> {
        Ok(())
    }
}

pub struct ProviderRegistry {
    providers: HashMap<String, Arc<dyn LlmProvider>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self {
            providers: HashMap::new(),
        }
    }

    pub fn register(&mut self, id: impl Into<String>, provider: Arc<dyn LlmProvider>) {
        self.providers.insert(id.into(), provider);
    }

    pub fn get(&self, id: &str) -> Result<Arc<dyn LlmProvider>> {
        self.providers
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("provider not found: {id}"))
    }
}

pub struct StubProvider;

#[async_trait]
impl LlmProvider for StubProvider {
    async fn chat(&self, request: LlmRequest) -> Result<LlmResponse> {
        let user_text = request
            .messages
            .last()
            .map(|m| m.content.clone())
            .unwrap_or_default();
        Ok(LlmResponse {
            text: format!("[stub:anthropic:{}] {}", request.model, user_text),
            input_tokens: None,
            output_tokens: None,
            stop_reason: Some("end_turn".into()),
        })
    }
}

pub fn register_builtin_providers(registry: &mut ProviderRegistry) {
    registry.register("anthropic", Arc::new(StubProvider));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_registry_get_registered_succeeds() {
        let mut registry = ProviderRegistry::new();
        registry.register("anthropic", Arc::new(StubProvider));

        let provider = registry.get("anthropic").unwrap();
        assert!(Arc::strong_count(&provider) >= 1);
    }

    #[test]
    fn provider_registry_get_unknown_fails() {
        let registry = ProviderRegistry::new();
        let err = registry.get("missing").err().unwrap();
        assert!(err.to_string().contains("provider not found: missing"));
    }
}
