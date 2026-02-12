use anyhow::{anyhow, Result};
use async_trait::async_trait;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};

use crate::{LlmProvider, LlmRequest, LlmResponse};

#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    client: reqwest::Client,
    api_key: String,
    api_base: String,
}

impl AnthropicProvider {
    pub fn new(api_key: impl Into<String>, api_base: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.into(),
            api_base: api_base.into().trim_end_matches('/').to_string(),
        }
    }

    pub fn from_env(api_base: impl Into<String>) -> Result<Self> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .map_err(|_| anyhow!("ANTHROPIC_API_KEY is not set"))?;
        Ok(Self::new(api_key, api_base))
    }

    pub(crate) fn to_api_request(request: LlmRequest) -> ApiRequest {
        ApiRequest {
            model: request.model,
            system: request.system,
            max_tokens: request.max_tokens,
            messages: request
                .messages
                .into_iter()
                .map(|m| ApiMessage {
                    role: m.role,
                    content: m.content,
                })
                .collect(),
        }
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    async fn chat(&self, request: LlmRequest) -> Result<LlmResponse> {
        let url = format!("{}/v1/messages", self.api_base);
        let payload = Self::to_api_request(request);

        let resp = self
            .client
            .post(url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&payload)
            .send()
            .await?;

        let status = resp.status();
        if status != StatusCode::OK {
            let text = resp.text().await?;
            let parsed = serde_json::from_str::<ApiError>(&text).ok();
            return Err(format_api_error(status, parsed));
        }

        let body: ApiResponse = resp.json().await?;
        let text = body
            .content
            .iter()
            .filter_map(|block| block.text.as_deref())
            .collect::<Vec<_>>()
            .join("\n");

        Ok(LlmResponse {
            text,
            input_tokens: body.usage.as_ref().map(|u| u.input_tokens),
            output_tokens: body.usage.as_ref().map(|u| u.output_tokens),
            stop_reason: body.stop_reason,
        })
    }
}

fn format_api_error(status: StatusCode, parsed: Option<ApiError>) -> anyhow::Error {
    if let Some(api_error) = parsed {
        let detail = api_error.error;
        anyhow!(
            "anthropic api error ({status}): {} ({})",
            detail.message,
            detail.r#type
        )
    } else {
        anyhow!("anthropic api error ({status})")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ApiRequest {
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    pub max_tokens: u32,
    pub messages: Vec<ApiMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ApiMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ApiResponse {
    pub content: Vec<ContentBlock>,
    pub usage: Option<ApiUsage>,
    pub stop_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ContentBlock {
    #[serde(rename = "type")]
    pub block_type: String,
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ApiUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ApiError {
    pub error: ApiErrorDetail,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ApiErrorDetail {
    #[serde(rename = "type")]
    pub r#type: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LlmMessage;

    #[test]
    fn anthropic_new_constructs_correctly() {
        let provider = AnthropicProvider::new("test-key", "https://api.anthropic.com/");

        assert_eq!(provider.api_key, "test-key");
        assert_eq!(provider.api_base, "https://api.anthropic.com");
    }

    #[test]
    fn api_request_serialization_matches_expected_shape() {
        let req = LlmRequest {
            model: "claude-sonnet-4-5".to_string(),
            system: Some("system prompt".to_string()),
            messages: vec![LlmMessage {
                role: "user".to_string(),
                content: "hello".to_string(),
            }],
            max_tokens: 1024,
        };
        let api_req = AnthropicProvider::to_api_request(req);

        let value = serde_json::to_value(api_req).unwrap();
        let expected = serde_json::json!({
            "model": "claude-sonnet-4-5",
            "system": "system prompt",
            "max_tokens": 1024,
            "messages": [
                { "role": "user", "content": "hello" }
            ]
        });

        assert_eq!(value, expected);
    }

    #[test]
    fn api_response_deserialization_works() {
        let raw = serde_json::json!({
            "content": [
                {"type": "text", "text": "line 1"},
                {"type": "text", "text": "line 2"}
            ],
            "usage": {"input_tokens": 12, "output_tokens": 34},
            "stop_reason": "end_turn"
        });

        let parsed: ApiResponse = serde_json::from_value(raw).unwrap();
        assert_eq!(parsed.content.len(), 2);
        assert_eq!(parsed.content[0].text.as_deref(), Some("line 1"));
        assert_eq!(parsed.usage.as_ref().map(|u| u.input_tokens), Some(12));
        assert_eq!(parsed.usage.as_ref().map(|u| u.output_tokens), Some(34));
        assert_eq!(parsed.stop_reason.as_deref(), Some("end_turn"));
    }

    #[test]
    fn api_error_deserialization_works() {
        let raw = serde_json::json!({
            "type": "error",
            "error": {
                "type": "invalid_request_error",
                "message": "messages: field required"
            }
        });

        let parsed: ApiError = serde_json::from_value(raw).unwrap();
        assert_eq!(parsed.error.r#type, "invalid_request_error");
        assert_eq!(parsed.error.message, "messages: field required");
    }

    #[tokio::test]
    #[ignore]
    async fn integration_real_api_call() {
        let provider = match AnthropicProvider::from_env("https://api.anthropic.com") {
            Ok(provider) => provider,
            Err(_) => return,
        };

        let request = LlmRequest::simple(
            "claude-3-5-haiku-latest".to_string(),
            Some("Reply with exactly: pong".to_string()),
            "ping".to_string(),
        );

        let response = provider.chat(request).await.unwrap();
        assert!(!response.text.trim().is_empty());
    }
}
