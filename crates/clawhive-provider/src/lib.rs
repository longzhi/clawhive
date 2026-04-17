pub mod anthropic;
pub mod azure_openai;
pub mod bedrock;
pub mod error;
pub mod gemini;
pub mod openai;
pub mod openai_chatgpt;
pub mod openai_compat;
pub mod types;

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use futures_core::Stream;
use serde::{Deserialize, Serialize};
use tokio_stream::iter as stream_iter;

pub use anthropic::AnthropicProvider;
pub use azure_openai::AzureOpenAiProvider;
pub use bedrock::sigv4::AwsCredentials;
pub use bedrock::BedrockProvider;
pub use error::ProviderError;
pub use gemini::GeminiProvider;
pub use openai::OpenAiProvider;
pub use openai_chatgpt::OpenAiChatGptProvider;
pub use openai_compat::{
    custom, deepseek, fireworks, groq, minimax, moonshot, ollama, ollama_with_base, openrouter,
    qianfan, qwen, together, volcengine, zhipu,
};
pub use types::StreamChunk;
pub use types::*;

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn chat(&self, request: LlmRequest) -> Result<LlmResponse, ProviderError>;
    async fn stream(
        &self,
        _request: LlmRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>, ProviderError> {
        Err(ProviderError::Other(anyhow::anyhow!(
            "streaming not supported by this provider"
        )))
    }
    async fn health(&self) -> Result<(), ProviderError> {
        Ok(())
    }
    /// List available model IDs from the provider.
    /// Default: returns empty vec (provider doesn't support model listing).
    async fn list_models(&self) -> Result<Vec<String>, ProviderError> {
        Ok(vec![])
    }
}

// ============================================================
// Provider Configuration
// ============================================================

/// Provider type identifier.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProviderType {
    Anthropic,
    OpenAI,
    #[serde(rename = "azure-openai")]
    AzureOpenAI,
    Bedrock,
    Gemini,
    DeepSeek,
    Groq,
    Ollama,
    OpenRouter,
    Together,
    Fireworks,
    /// Custom OpenAI-compatible endpoint
    Custom,
    /// Qwen (通义千问) via DashScope - OpenAI compatible
    Qwen,
    /// Moonshot / Kimi - OpenAI compatible
    Moonshot,
    /// Zhipu GLM (智谱AI) - OpenAI compatible
    Zhipu,
    /// MiniMax - OpenAI compatible
    #[serde(rename = "minimax")]
    MiniMax,
    /// Volcengine / Doubao (火山引擎) - OpenAI compatible
    Volcengine,
    /// Baidu Qianfan (百度千帆) v2 - OpenAI compatible
    Qianfan,
}

/// Configuration for a single provider instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Unique provider ID (e.g., "openai", "deepseek", "my-ollama")
    pub id: String,
    /// Provider type
    #[serde(rename = "type")]
    pub provider_type: ProviderType,
    /// API key (optional for Ollama)
    #[serde(default)]
    pub api_key: Option<String>,
    /// Custom base URL (optional, uses default for each provider type)
    #[serde(default)]
    pub base_url: Option<String>,
    /// AWS access key ID (for Bedrock).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aws_access_key_id: Option<String>,
    /// AWS secret access key (for Bedrock).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aws_secret_access_key: Option<String>,
    /// AWS session token (for Bedrock; used with temporary credentials).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aws_session_token: Option<String>,
    /// AWS region (for Bedrock, e.g. "us-west-2").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
}

impl ProviderConfig {
    pub fn new(id: impl Into<String>, provider_type: ProviderType) -> Self {
        Self {
            id: id.into(),
            provider_type,
            api_key: None,
            base_url: None,
            aws_access_key_id: None,
            aws_secret_access_key: None,
            aws_session_token: None,
            region: None,
        }
    }

    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self
    }
}

/// Create a provider from configuration.
pub fn create_provider(config: &ProviderConfig) -> Result<Arc<dyn LlmProvider>, ProviderError> {
    let require_key = |provider: &str| -> Result<String, ProviderError> {
        config
            .api_key
            .clone()
            .ok_or_else(|| ProviderError::MissingConfig {
                provider: provider.to_string(),
                field: "api_key",
            })
    };

    let require_base_url = |provider: &str| -> Result<String, ProviderError> {
        config
            .base_url
            .clone()
            .ok_or_else(|| ProviderError::MissingConfig {
                provider: provider.to_string(),
                field: "base_url",
            })
    };

    let provider: Arc<dyn LlmProvider> =
        match config.provider_type {
            ProviderType::Anthropic => {
                let key = require_key("anthropic")?;
                let base_url = config
                    .base_url
                    .as_deref()
                    .unwrap_or("https://api.anthropic.com");
                Arc::new(AnthropicProvider::new(key, base_url))
            }
            ProviderType::OpenAI => {
                let key = require_key("openai")?;
                let base_url = config
                    .base_url
                    .as_deref()
                    .unwrap_or("https://api.openai.com/v1");
                Arc::new(OpenAiProvider::new(key, base_url))
            }
            ProviderType::AzureOpenAI => {
                let key = require_key("azure-openai")?;
                let base_url = require_base_url("azure-openai")?;
                Arc::new(AzureOpenAiProvider::new(key, base_url))
            }
            ProviderType::Bedrock => {
                let access_key_id = config.aws_access_key_id.clone().ok_or_else(|| {
                    ProviderError::MissingConfig {
                        provider: "bedrock".into(),
                        field: "aws_access_key_id",
                    }
                })?;
                let secret_access_key = config.aws_secret_access_key.clone().ok_or_else(|| {
                    ProviderError::MissingConfig {
                        provider: "bedrock".into(),
                        field: "aws_secret_access_key",
                    }
                })?;
                let region = config
                    .region
                    .clone()
                    .ok_or_else(|| ProviderError::MissingConfig {
                        provider: "bedrock".into(),
                        field: "region",
                    })?;
                Arc::new(BedrockProvider::new(
                    AwsCredentials {
                        access_key_id,
                        secret_access_key,
                        session_token: config.aws_session_token.clone(),
                    },
                    region,
                ))
            }
            ProviderType::Gemini => {
                let key = require_key("gemini")?;
                Arc::new(GeminiProvider::new(key))
            }
            ProviderType::DeepSeek => {
                let key = require_key("deepseek")?;
                Arc::new(deepseek(key))
            }
            ProviderType::Groq => {
                let key = require_key("groq")?;
                Arc::new(groq(key))
            }
            ProviderType::Ollama => {
                let base_url = config
                    .base_url
                    .as_deref()
                    .unwrap_or("http://localhost:11434/v1");
                Arc::new(ollama_with_base(base_url))
            }
            ProviderType::OpenRouter => {
                let key = require_key("openrouter")?;
                Arc::new(openrouter(key))
            }
            ProviderType::Together => {
                let key = require_key("together")?;
                Arc::new(together(key))
            }
            ProviderType::Fireworks => {
                let key = require_key("fireworks")?;
                Arc::new(fireworks(key))
            }
            ProviderType::Custom => {
                let key = require_key("custom")?;
                let base_url = require_base_url("custom")?;
                Arc::new(custom(key, base_url))
            }
            ProviderType::Qwen => {
                let key = require_key("qwen")?;
                Arc::new(qwen(key))
            }
            ProviderType::Moonshot => {
                let key = require_key("moonshot")?;
                Arc::new(moonshot(key))
            }
            ProviderType::Zhipu => {
                let key = require_key("zhipu")?;
                Arc::new(zhipu(key))
            }
            ProviderType::MiniMax => {
                let key = require_key("minimax")?;
                Arc::new(minimax(key))
            }
            ProviderType::Volcengine => {
                let key = require_key("volcengine")?;
                Arc::new(volcengine(key))
            }
            ProviderType::Qianfan => {
                let key = require_key("qianfan")?;
                Arc::new(qianfan(key))
            }
        };
    Ok(provider)
}

/// Register providers from a list of configurations.
pub fn register_from_configs(
    registry: &mut ProviderRegistry,
    configs: &[ProviderConfig],
) -> Result<(), ProviderError> {
    for config in configs {
        let provider = create_provider(config)?;
        registry.register(&config.id, provider);
        tracing::info!(
            "Registered provider: {} ({:?})",
            config.id,
            config.provider_type
        );
    }
    Ok(())
}

// ============================================================
// Provider Registry
// ============================================================

#[derive(Clone, Default)]
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

    pub fn get(&self, id: &str) -> Result<Arc<dyn LlmProvider>, ProviderError> {
        self.providers
            .get(id)
            .cloned()
            .ok_or_else(|| ProviderError::NotFound(id.to_string()))
    }

    pub fn list(&self) -> Vec<&str> {
        self.providers.keys().map(|s| s.as_str()).collect()
    }
}

pub struct StubProvider;

#[async_trait]
impl LlmProvider for StubProvider {
    async fn chat(&self, request: LlmRequest) -> Result<LlmResponse, ProviderError> {
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
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>, ProviderError> {
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
            thinking_level: None,
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
#[test]
fn provider_config_chinese_providers_serialize() {
    let types = vec![
        ("qwen", ProviderType::Qwen),
        ("moonshot", ProviderType::Moonshot),
        ("zhipu", ProviderType::Zhipu),
        ("minimax", ProviderType::MiniMax),
        ("volcengine", ProviderType::Volcengine),
        ("qianfan", ProviderType::Qianfan),
    ];
    for (expected_str, pt) in &types {
        let config = ProviderConfig::new("test", pt.clone()).with_api_key("sk-test");
        let json = serde_json::to_string(&config).unwrap();
        assert!(
            json.contains(expected_str),
            "Expected {} in {}",
            expected_str,
            json
        );
    }
}

#[test]
fn create_provider_chinese_providers() {
    let providers = vec![
        ProviderConfig::new("qwen", ProviderType::Qwen).with_api_key("sk-test"),
        ProviderConfig::new("moonshot", ProviderType::Moonshot).with_api_key("sk-test"),
        ProviderConfig::new("zhipu", ProviderType::Zhipu).with_api_key("sk-test"),
        ProviderConfig::new("minimax", ProviderType::MiniMax).with_api_key("sk-test"),
        ProviderConfig::new("volcengine", ProviderType::Volcengine).with_api_key("sk-test"),
        ProviderConfig::new("qianfan", ProviderType::Qianfan).with_api_key("sk-test"),
    ];
    for config in &providers {
        let result = create_provider(config);
        assert!(result.is_ok(), "Failed to create provider: {}", config.id);
    }
}

#[test]
fn provider_config_serialize_deserialize() {
    let config = ProviderConfig::new("my-openai", ProviderType::OpenAI)
        .with_api_key("sk-test")
        .with_base_url("https://custom.example.com/v1");

    let json = serde_json::to_string(&config).unwrap();
    let parsed: ProviderConfig = serde_json::from_str(&json).unwrap();

    assert_eq!(parsed.id, "my-openai");
    assert_eq!(parsed.provider_type, ProviderType::OpenAI);
    assert_eq!(parsed.api_key, Some("sk-test".to_string()));
    assert_eq!(
        parsed.base_url,
        Some("https://custom.example.com/v1".to_string())
    );
}

#[test]
fn provider_config_list_example() {
    let configs = vec![
        ProviderConfig::new("openai", ProviderType::OpenAI).with_api_key("sk-xxx"),
        ProviderConfig::new("deepseek", ProviderType::DeepSeek).with_api_key("sk-yyy"),
        ProviderConfig::new("local-ollama", ProviderType::Ollama),
    ];

    let json = serde_json::to_string_pretty(&configs).unwrap();
    assert!(json.contains("openai"));
    assert!(json.contains("deepseek"));
    assert!(json.contains("local-ollama"));
}

#[test]
fn provider_config_bedrock_fields_round_trip() {
    let config = ProviderConfig {
        id: "bedrock-prod".into(),
        provider_type: ProviderType::Bedrock,
        api_key: None,
        base_url: None,
        aws_access_key_id: Some("AKIA_TEST".into()),
        aws_secret_access_key: Some("secret".into()),
        aws_session_token: Some("token".into()),
        region: Some("us-west-2".into()),
    };
    let json_value = serde_json::to_value(&config).unwrap();
    assert_eq!(json_value["type"], "bedrock");
    let json = serde_json::to_string(&config).unwrap();
    let parsed: ProviderConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.provider_type, ProviderType::Bedrock);
    assert_eq!(parsed.aws_access_key_id.as_deref(), Some("AKIA_TEST"));
    assert_eq!(parsed.region.as_deref(), Some("us-west-2"));
}

#[test]
fn provider_config_bedrock_omits_optional_fields() {
    let config = ProviderConfig::new("openai", ProviderType::OpenAI).with_api_key("sk-x");
    let json = serde_json::to_value(&config).unwrap();
    assert!(json.get("aws_access_key_id").is_none());
    assert!(json.get("region").is_none());
}

#[test]
fn create_provider_bedrock_requires_aws_access_key_id() {
    let config = ProviderConfig::new("my-bedrock", ProviderType::Bedrock);
    let err = create_provider(&config).err().unwrap();
    let msg = err.to_string();
    assert!(
        msg.contains("aws_access_key_id"),
        "expected aws_access_key_id in: {msg}"
    );
}

#[test]
fn create_provider_bedrock_requires_secret_access_key() {
    let mut config = ProviderConfig::new("my-bedrock", ProviderType::Bedrock);
    config.aws_access_key_id = Some("AKIA".into());
    let err = create_provider(&config).err().unwrap();
    assert!(err.to_string().contains("aws_secret_access_key"));
}

#[test]
fn create_provider_bedrock_requires_region() {
    let mut config = ProviderConfig::new("my-bedrock", ProviderType::Bedrock);
    config.aws_access_key_id = Some("AKIA".into());
    config.aws_secret_access_key = Some("secret".into());
    let err = create_provider(&config).err().unwrap();
    assert!(err.to_string().contains("region"));
}

#[test]
fn create_provider_bedrock_success() {
    let config = ProviderConfig {
        id: "my-bedrock".into(),
        provider_type: ProviderType::Bedrock,
        api_key: None,
        base_url: None,
        aws_access_key_id: Some("AKIA".into()),
        aws_secret_access_key: Some("secret".into()),
        aws_session_token: Some("t".into()),
        region: Some("us-west-2".into()),
    };
    assert!(create_provider(&config).is_ok());
}
