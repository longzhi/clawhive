use anyhow::{anyhow, Result};
use async_trait::async_trait;
use clawhive_auth::AuthProfile;
use futures_core::Stream;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use tokio_stream::StreamExt;

use crate::{LlmProvider, LlmRequest, LlmResponse, StreamChunk};

#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    client: reqwest::Client,
    api_key: String,
    api_base: String,
    auth_profile: Option<AuthProfile>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProviderErrorKind {
    RateLimit,
    ServerError,
    Timeout,
    AuthError,
    InvalidRequest,
    Unknown,
}

impl ProviderErrorKind {
    pub fn from_status(status: reqwest::StatusCode) -> Self {
        match status.as_u16() {
            429 => Self::RateLimit,
            401 | 403 => Self::AuthError,
            400 | 422 => Self::InvalidRequest,
            500..=599 => Self::ServerError,
            _ => Self::Unknown,
        }
    }

    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::RateLimit | Self::ServerError | Self::Timeout)
    }
}

impl AnthropicProvider {
    pub fn new(api_key: impl Into<String>, api_base: impl Into<String>) -> Self {
        Self::new_with_auth(api_key, api_base, None)
    }

    pub fn new_with_auth(
        api_key: impl Into<String>,
        api_base: impl Into<String>,
        auth_profile: Option<AuthProfile>,
    ) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .unwrap_or_default(),
            api_key: api_key.into(),
            api_base: api_base.into().trim_end_matches('/').to_string(),
            auth_profile,
        }
    }

    fn use_session_auth(&self) -> Option<&str> {
        match &self.auth_profile {
            Some(AuthProfile::AnthropicSession { session_token }) => Some(session_token.as_str()),
            Some(AuthProfile::ApiKey { api_key, .. }) => Some(api_key.as_str()),
            _ => None,
        }
    }

    pub(crate) fn to_api_request(request: LlmRequest) -> ApiRequest {
        let tools: Vec<ApiToolDef> = request
            .tools
            .into_iter()
            .map(|t| ApiToolDef {
                name: t.name,
                description: t.description,
                input_schema: t.input_schema,
            })
            .collect();

        ApiRequest {
            model: request.model,
            system: request.system,
            max_tokens: request.max_tokens,
            messages: request
                .messages
                .into_iter()
                .map(|m| {
                    let has_non_text = m
                        .content
                        .iter()
                        .any(|b| !matches!(b, crate::ContentBlock::Text { .. }));
                    if has_non_text {
                        // Send as array for tool_use/tool_result messages
                        let blocks: Vec<serde_json::Value> = m
                            .content
                            .iter()
                            .map(|b| match b {
                                crate::ContentBlock::Text { text } => {
                                    serde_json::json!({"type": "text", "text": text})
                                }
                                crate::ContentBlock::ToolUse { id, name, input } => {
                                    serde_json::json!({"type": "tool_use", "id": id, "name": name, "input": input})
                                }
                                crate::ContentBlock::ToolResult {
                                    tool_use_id,
                                    content,
                                    is_error,
                                } => {
                                    serde_json::json!({"type": "tool_result", "tool_use_id": tool_use_id, "content": content, "is_error": is_error})
                                }
                            })
                            .collect();
                        ApiMessage {
                            role: m.role,
                            content: serde_json::Value::Array(blocks),
                        }
                    } else {
                        let text = m.text();
                        ApiMessage {
                            role: m.role,
                            content: serde_json::Value::String(text),
                        }
                    }
                })
                .collect(),
            tools: if tools.is_empty() { None } else { Some(tools) },
            stream: false,
        }
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    async fn chat(&self, request: LlmRequest) -> Result<LlmResponse> {
        let url = format!("{}/v1/messages", self.api_base);
        let payload = Self::to_api_request(request);

        let mut req = self
            .client
            .post(url)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&payload);
        req = match self.use_session_auth() {
            Some(session) => req
                .header("authorization", format!("Bearer {session}"))
                .header(
                    "anthropic-beta",
                    clawhive_auth::oauth::ANTHROPIC_OAUTH_BETAS,
                ),
            None => req.header("x-api-key", &self.api_key),
        };

        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) if e.is_timeout() => {
                return Err(anyhow::anyhow!(
                    "anthropic api error (timeout) [retryable]: request timed out after 60s"
                ));
            }
            Err(e) if e.is_connect() => {
                return Err(anyhow::anyhow!(
                    "anthropic api error (connect) [retryable]: {e}"
                ));
            }
            Err(e) => return Err(e.into()),
        };

        let status = resp.status();
        if status != StatusCode::OK {
            let text = resp.text().await?;
            let parsed = serde_json::from_str::<ApiError>(&text).ok();
            return Err(format_api_error(status, parsed));
        }

        let body: ApiResponse = resp.json().await?;
        let content_blocks: Vec<crate::ContentBlock> = body
            .content
            .iter()
            .filter_map(|block| match block.block_type.as_str() {
                "text" => block
                    .text
                    .as_ref()
                    .map(|t| crate::ContentBlock::Text { text: t.clone() }),
                "tool_use" => {
                    let id = block.id.as_ref()?.clone();
                    let name = block.name.as_ref()?.clone();
                    let input = block
                        .input
                        .clone()
                        .unwrap_or(serde_json::Value::Object(Default::default()));
                    Some(crate::ContentBlock::ToolUse { id, name, input })
                }
                _ => None,
            })
            .collect();
        let text = body
            .content
            .iter()
            .filter_map(|block| block.text.as_deref())
            .collect::<Vec<_>>()
            .join("\n");

        Ok(LlmResponse {
            text,
            content: content_blocks,
            input_tokens: body.usage.as_ref().map(|u| u.input_tokens),
            output_tokens: body.usage.as_ref().map(|u| u.output_tokens),
            stop_reason: body.stop_reason,
        })
    }

    async fn stream(
        &self,
        request: LlmRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>> {
        let url = format!("{}/v1/messages", self.api_base);
        let mut payload = Self::to_api_request(request);
        payload.stream = true;

        let mut req = self
            .client
            .post(url)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&payload);
        req = match self.use_session_auth() {
            Some(session) => req
                .header("authorization", format!("Bearer {session}"))
                .header(
                    "anthropic-beta",
                    clawhive_auth::oauth::ANTHROPIC_OAUTH_BETAS,
                ),
            None => req.header("x-api-key", &self.api_key),
        };

        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) if e.is_timeout() => {
                return Err(anyhow::anyhow!(
                    "anthropic api error (timeout) [retryable]: request timed out after 60s"
                ));
            }
            Err(e) if e.is_connect() => {
                return Err(anyhow::anyhow!(
                    "anthropic api error (connect) [retryable]: {e}"
                ));
            }
            Err(e) => return Err(e.into()),
        };

        let status = resp.status();
        if status != StatusCode::OK {
            let text = resp.text().await?;
            let parsed = serde_json::from_str::<ApiError>(&text).ok();
            return Err(format_api_error(status, parsed));
        }

        let sse_stream = parse_sse_stream(resp.bytes_stream());
        Ok(Box::pin(sse_stream))
    }
}

fn parse_sse_stream(
    byte_stream: impl Stream<Item = std::result::Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
) -> impl Stream<Item = Result<StreamChunk>> + Send {
    async_stream::stream! {
        tokio::pin!(byte_stream);
        let mut buffer = String::new();

        while let Some(chunk_result) = byte_stream.next().await {
            match chunk_result {
                Ok(bytes) => {
                    buffer.push_str(&String::from_utf8_lossy(&bytes));

                    while let Some(pos) = buffer.find("\n\n") {
                        let event_text = buffer[..pos].to_string();
                        buffer = buffer[pos + 2..].to_string();

                        for line in event_text.lines() {
                            let Some(data) = line.strip_prefix("data: ") else {
                                continue;
                            };

                            if data == "[DONE]" {
                                continue;
                            }

                            match serde_json::from_str::<serde_json::Value>(data) {
                                Ok(event) => {
                                    if let Some(chunk) = parse_sse_event(&event) {
                                        yield Ok(chunk);
                                    }
                                }
                                Err(e) => {
                                    yield Err(anyhow!("invalid sse event payload: {e}"));
                                    return;
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    yield Err(anyhow!("stream error: {e}"));
                    return;
                }
            }
        }
    }
}

fn parse_sse_event(event: &serde_json::Value) -> Option<StreamChunk> {
    let event_type = event.get("type")?.as_str()?;

    match event_type {
        "content_block_delta" => {
            let delta = event.get("delta")?;
            let text = delta.get("text")?.as_str()?.to_string();
            Some(StreamChunk {
                delta: text,
                is_final: false,
                input_tokens: None,
                output_tokens: None,
                stop_reason: None,
                content_blocks: vec![],
            })
        }
        "message_delta" => {
            let delta = event.get("delta")?;
            let stop_reason = delta
                .get("stop_reason")
                .and_then(|value| value.as_str())
                .map(std::string::ToString::to_string);
            let usage = event.get("usage");
            let output_tokens = usage
                .and_then(|u| u.get("output_tokens"))
                .and_then(|value| value.as_u64())
                .and_then(|value| u32::try_from(value).ok());

            Some(StreamChunk {
                delta: String::new(),
                is_final: true,
                input_tokens: None,
                output_tokens,
                stop_reason,
                content_blocks: vec![],
            })
        }
        "message_start" => {
            let message = event.get("message")?;
            let usage = message.get("usage")?;
            let input_tokens = usage
                .get("input_tokens")
                .and_then(|value| value.as_u64())
                .and_then(|value| u32::try_from(value).ok());

            Some(StreamChunk {
                delta: String::new(),
                is_final: false,
                input_tokens,
                output_tokens: None,
                stop_reason: None,
                content_blocks: vec![],
            })
        }
        _ => None,
    }
}

fn format_api_error(status: StatusCode, parsed: Option<ApiError>) -> anyhow::Error {
    let kind = ProviderErrorKind::from_status(status);
    let retryable = if kind.is_retryable() {
        " [retryable]"
    } else {
        ""
    };
    if let Some(api_error) = parsed {
        let detail = api_error.error;
        anyhow!(
            "anthropic api error ({status}){retryable}: {} ({})",
            detail.message,
            detail.r#type
        )
    } else {
        anyhow!("anthropic api error ({status}){retryable}")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ApiRequest {
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    pub max_tokens: u32,
    pub messages: Vec<ApiMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ApiToolDef>>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stream: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ApiMessage {
    pub role: String,
    pub content: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ApiToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ApiResponse {
    pub content: Vec<ApiContentBlock>,
    pub usage: Option<ApiUsage>,
    pub stop_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ApiContentBlock {
    #[serde(rename = "type")]
    pub block_type: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub input: Option<serde_json::Value>,
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
    fn anthropic_session_profile_uses_bearer_session_token() {
        let provider = AnthropicProvider::new_with_auth(
            "test-key",
            "https://api.anthropic.com",
            Some(AuthProfile::AnthropicSession {
                session_token: "session-xyz".to_string(),
            }),
        );

        assert_eq!(provider.use_session_auth(), Some("session-xyz"));
    }

    #[test]
    fn api_request_serialization_matches_expected_shape() {
        let req = LlmRequest {
            model: "claude-sonnet-4-5".to_string(),
            system: Some("system prompt".to_string()),
            messages: vec![LlmMessage::user("hello")],
            max_tokens: 1024,
            tools: vec![],
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

    #[test]
    fn parse_sse_event_content_block_delta() {
        let event = serde_json::json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": "Hello"}
        });
        let chunk = parse_sse_event(&event).unwrap();
        assert_eq!(chunk.delta, "Hello");
        assert!(!chunk.is_final);
    }

    #[test]
    fn parse_sse_event_message_delta() {
        let event = serde_json::json!({
            "type": "message_delta",
            "delta": {"stop_reason": "end_turn"},
            "usage": {"output_tokens": 42}
        });
        let chunk = parse_sse_event(&event).unwrap();
        assert!(chunk.is_final);
        assert_eq!(chunk.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(chunk.output_tokens, Some(42));
    }

    #[test]
    fn parse_sse_event_message_start() {
        let event = serde_json::json!({
            "type": "message_start",
            "message": {
                "usage": {"input_tokens": 15}
            }
        });
        let chunk = parse_sse_event(&event).unwrap();
        assert_eq!(chunk.input_tokens, Some(15));
        assert!(!chunk.is_final);
    }

    #[test]
    fn api_request_stream_field_serialization() {
        let req = ApiRequest {
            model: "claude-sonnet-4-5".into(),
            system: None,
            max_tokens: 1024,
            messages: vec![],
            tools: None,
            stream: false,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("stream").is_none());

        let req_stream = ApiRequest {
            model: "claude-sonnet-4-5".into(),
            system: None,
            max_tokens: 1024,
            messages: vec![],
            tools: None,
            stream: true,
        };
        let json_stream = serde_json::to_value(&req_stream).unwrap();
        assert_eq!(json_stream.get("stream").unwrap(), true);
    }

    #[test]
    fn provider_error_kind_classification() {
        assert_eq!(
            ProviderErrorKind::from_status(reqwest::StatusCode::TOO_MANY_REQUESTS),
            ProviderErrorKind::RateLimit
        );
        assert_eq!(
            ProviderErrorKind::from_status(reqwest::StatusCode::INTERNAL_SERVER_ERROR),
            ProviderErrorKind::ServerError
        );
        assert_eq!(
            ProviderErrorKind::from_status(reqwest::StatusCode::UNAUTHORIZED),
            ProviderErrorKind::AuthError
        );
        assert_eq!(
            ProviderErrorKind::from_status(reqwest::StatusCode::BAD_REQUEST),
            ProviderErrorKind::InvalidRequest
        );
        assert!(ProviderErrorKind::RateLimit.is_retryable());
        assert!(ProviderErrorKind::ServerError.is_retryable());
        assert!(ProviderErrorKind::Timeout.is_retryable());
        assert!(!ProviderErrorKind::AuthError.is_retryable());
        assert!(!ProviderErrorKind::InvalidRequest.is_retryable());
    }

    #[test]
    fn format_api_error_with_parsed_body() {
        let parsed = Some(ApiError {
            error: ApiErrorDetail {
                r#type: "invalid_request_error".into(),
                message: "messages: required".into(),
            },
        });
        let err = format_api_error(StatusCode::BAD_REQUEST, parsed);
        let text = err.to_string();
        assert!(text.contains("400"));
        assert!(text.contains("messages: required"));
        assert!(!text.contains("[retryable]"));
    }

    #[test]
    fn format_api_error_without_parsed_body() {
        let err = format_api_error(StatusCode::INTERNAL_SERVER_ERROR, None);
        let text = err.to_string();
        assert!(text.contains("500"));
        assert!(text.contains("[retryable]"));
    }

    #[test]
    fn format_api_error_rate_limit_is_retryable() {
        let parsed = Some(ApiError {
            error: ApiErrorDetail {
                r#type: "rate_limit_error".into(),
                message: "too many requests".into(),
            },
        });
        let err = format_api_error(StatusCode::TOO_MANY_REQUESTS, parsed);
        let text = err.to_string();
        assert!(text.contains("[retryable]"));
        assert!(text.contains("429"));
    }

    #[test]
    fn parse_sse_event_unknown_type_returns_none() {
        let event = serde_json::json!({
            "type": "ping",
            "data": {}
        });
        assert!(parse_sse_event(&event).is_none());
    }

    #[test]
    fn api_request_without_system_omits_field() {
        let req = ApiRequest {
            model: "m".into(),
            system: None,
            max_tokens: 100,
            messages: vec![],
            tools: None,
            stream: false,
        };
        let json = serde_json::to_value(&req).unwrap();
        assert!(json.get("system").is_none());
    }

    #[tokio::test]
    #[ignore]
    async fn integration_real_api_call() {
        let api_key = match std::env::var("ANTHROPIC_API_KEY") {
            Ok(api_key) if !api_key.is_empty() => api_key,
            _ => return,
        };
        let provider = AnthropicProvider::new(api_key, "https://api.anthropic.com");

        let request = LlmRequest::simple(
            "claude-3-5-haiku-latest".to_string(),
            Some("Reply with exactly: pong".to_string()),
            "ping".to_string(),
        );

        let response = provider.chat(request).await.unwrap();
        assert!(!response.text.trim().is_empty());
    }
}
