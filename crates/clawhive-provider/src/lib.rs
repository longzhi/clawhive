pub mod anthropic;
pub mod gemini;
pub mod openai;
pub mod openai_chatgpt;
pub mod openai_compat;
pub mod types;

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use futures_core::Stream;
use tokio_stream::iter as stream_iter;

pub use anthropic::AnthropicProvider;
pub use gemini::GeminiProvider;
pub use openai::OpenAiProvider;
pub use openai_chatgpt::OpenAiChatGptProvider;
pub use openai_compat::{custom, deepseek, fireworks, groq, ollama, ollama_with_base, openrouter, together};
pub use types::StreamChunk;
pub use types::*;

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn chat(&self, request: LlmRequest) -> Result<LlmResponse>;
    async fn stream(
        &self,
        _request: LlmRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>> {
        anyhow::bail!("streaming not supported by this provider")
    }
    async fn health(&self) -> Result<()> {
        Ok(())
    }
}

#[derive(Default)]
pub struct ProviderRegistry {
    providers: HashMap<String, Arc<dyn LlmProvider>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
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
            .map(|m| m.text())
            .unwrap_or_default();
        let full_text = format!("[stub:anthropic:{}] {} [finish]", request.model, user_text);
        Ok(LlmResponse {
            text: full_text.clone(),
            content: vec![ContentBlock::Text { text: full_text }],
            input_tokens: None,
            output_tokens: None,
            stop_reason: Some("end_turn".into()),
        })
    }

    async fn stream(
        &self,
        request: LlmRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>> {
        let user_text = request
            .messages
            .last()
            .map(|m| m.text())
            .unwrap_or_default();
        let full_text = format!("[stub:stream:{}] {}", request.model, user_text);
        let words: Vec<String> = full_text
            .split_whitespace()
            .map(|word| format!("{word} "))
            .collect();

        let mut chunks: Vec<Result<StreamChunk>> = words
            .into_iter()
            .map(|word| {
                Ok(StreamChunk {
                    delta: word,
                    is_final: false,
                    input_tokens: None,
                    output_tokens: None,
                    stop_reason: None,
                    content_blocks: vec![],
                })
            })
            .collect();

        chunks.push(Ok(StreamChunk {
            delta: String::new(),
            is_final: true,
            input_tokens: Some(10),
            output_tokens: Some(20),
            stop_reason: Some("end_turn".into()),
            content_blocks: vec![],
        }));

        let stream = stream_iter(chunks);
        Ok(Box::pin(stream))
    }
}

pub fn register_builtin_providers(registry: &mut ProviderRegistry) {
    registry.register("anthropic", Arc::new(StubProvider));
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_stream::StreamExt;

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

    #[tokio::test]
    async fn stub_provider_stream_yields_chunks() {
        let provider = StubProvider;
        let req = LlmRequest::simple("test-model".into(), None, "hello world".into());
        let mut stream = provider.stream(req).await.unwrap();
        let mut collected = String::new();
        let mut got_final = false;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.unwrap();
            if chunk.is_final {
                got_final = true;
                assert!(chunk.stop_reason.is_some());
            } else {
                collected.push_str(&chunk.delta);
            }
        }
        assert!(got_final);
        assert!(collected.contains("stub:stream"));
    }

    #[tokio::test]
    async fn stub_provider_chat_returns_expected_format() {
        let provider = StubProvider;
        let req = LlmRequest::simple("my-model".into(), None, "ping".into());
        let resp = provider.chat(req).await.unwrap();
        assert!(resp.text.contains("stub:anthropic:my-model"));
        assert!(resp.text.contains("ping"));
        assert!(resp.text.contains("[finish]"));
        assert_eq!(resp.stop_reason.as_deref(), Some("end_turn"));
    }

    #[tokio::test]
    async fn stub_provider_chat_empty_messages() {
        let provider = StubProvider;
        let req = LlmRequest {
            model: "m".into(),
            system: None,
            messages: vec![],
            max_tokens: 100,
            tools: vec![],
        };
        let resp = provider.chat(req).await.unwrap();
        assert!(resp.text.contains("stub:anthropic:m"));
    }

    #[tokio::test]
    async fn default_health_returns_ok() {
        let provider = StubProvider;
        assert!(provider.health().await.is_ok());
    }
}
