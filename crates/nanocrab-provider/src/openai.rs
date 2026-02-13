use anyhow::{anyhow, Result};
use async_trait::async_trait;
use futures_core::Stream;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use tokio_stream::StreamExt;

use crate::{ContentBlock, LlmMessage, LlmProvider, LlmRequest, LlmResponse, StreamChunk};

#[derive(Debug, Clone)]
pub struct OpenAiProvider {
    client: reqwest::Client,
    api_key: String,
    api_base: String,
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

impl OpenAiProvider {
    pub fn new(api_key: impl Into<String>, api_base: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .unwrap_or_default(),
            api_key: api_key.into(),
            api_base: api_base.into().trim_end_matches('/').to_string(),
        }
    }

    pub fn from_env(api_key_env: &str, api_base: impl Into<String>) -> Result<Self> {
        let api_key =
            std::env::var(api_key_env).map_err(|_| anyhow!("{api_key_env} is not set"))?;
        Ok(Self::new(api_key, api_base))
    }

    pub(crate) fn to_api_request(request: LlmRequest, stream: bool) -> ApiRequest {
        let tools = if request.tools.is_empty() {
            None
        } else {
            Some(
                request
                    .tools
                    .into_iter()
                    .map(|tool| ApiTool {
                        tool_type: "function".to_string(),
                        function: ApiFunctionDef {
                            name: tool.name,
                            description: tool.description,
                            parameters: tool.input_schema,
                        },
                    })
                    .collect(),
            )
        };

        ApiRequest {
            model: request.model,
            messages: to_api_messages(request.system, request.messages),
            max_tokens: Some(request.max_tokens),
            tools,
            stream,
            stream_options: if stream {
                Some(ApiStreamOptions {
                    include_usage: true,
                })
            } else {
                None
            },
        }
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    async fn chat(&self, request: LlmRequest) -> Result<LlmResponse> {
        let url = format!("{}/chat/completions", self.api_base);
        let payload = Self::to_api_request(request, false);

        let resp = match self
            .client
            .post(url)
            .header("authorization", format!("Bearer {}", self.api_key))
            .header("content-type", "application/json")
            .json(&payload)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) if e.is_timeout() => {
                return Err(anyhow!(
                    "openai api error (timeout) [retryable]: request timed out after 60s"
                ));
            }
            Err(e) if e.is_connect() => {
                return Err(anyhow!("openai api error (connect) [retryable]: {e}"));
            }
            Err(e) => return Err(e.into()),
        };

        let status = resp.status();
        if status != StatusCode::OK {
            let text = resp.text().await?;
            let parsed = serde_json::from_str::<ApiErrorEnvelope>(&text).ok();
            return Err(format_api_error(status, parsed));
        }

        let body: ApiResponse = resp.json().await?;
        to_llm_response(body)
    }

    async fn stream(
        &self,
        request: LlmRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>> {
        let url = format!("{}/chat/completions", self.api_base);
        let payload = Self::to_api_request(request, true);

        let resp = match self
            .client
            .post(url)
            .header("authorization", format!("Bearer {}", self.api_key))
            .header("content-type", "application/json")
            .json(&payload)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) if e.is_timeout() => {
                return Err(anyhow!(
                    "openai api error (timeout) [retryable]: request timed out after 60s"
                ));
            }
            Err(e) if e.is_connect() => {
                return Err(anyhow!("openai api error (connect) [retryable]: {e}"));
            }
            Err(e) => return Err(e.into()),
        };

        let status = resp.status();
        if status != StatusCode::OK {
            let text = resp.text().await?;
            let parsed = serde_json::from_str::<ApiErrorEnvelope>(&text).ok();
            return Err(format_api_error(status, parsed));
        }

        let sse_stream = parse_sse_stream(resp.bytes_stream());
        Ok(Box::pin(sse_stream))
    }
}

fn to_api_messages(system: Option<String>, messages: Vec<LlmMessage>) -> Vec<ApiMessage> {
    let mut result = Vec::new();

    if let Some(system_text) = system {
        result.push(ApiMessage {
            role: "system".to_string(),
            content: Some(system_text),
            tool_calls: None,
            tool_call_id: None,
        });
    }

    for message in messages {
        let text = message.text();
        let tool_uses: Vec<ApiToolCall> = message
            .content
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse { id, name, input } => Some(ApiToolCall {
                    id: id.clone(),
                    call_type: "function".to_string(),
                    function: ApiToolFunctionCall {
                        name: name.clone(),
                        arguments: input.to_string(),
                    },
                }),
                _ => None,
            })
            .collect();

        if !text.is_empty() || !tool_uses.is_empty() {
            result.push(ApiMessage {
                role: message.role.clone(),
                content: if text.is_empty() { None } else { Some(text) },
                tool_calls: if tool_uses.is_empty() {
                    None
                } else {
                    Some(tool_uses)
                },
                tool_call_id: None,
            });
        }

        for block in message.content {
            if let ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } = block
            {
                result.push(ApiMessage {
                    role: "tool".to_string(),
                    content: Some(content),
                    tool_calls: None,
                    tool_call_id: Some(tool_use_id),
                });
            }
        }
    }

    result
}

fn to_llm_response(body: ApiResponse) -> Result<LlmResponse> {
    let choice = body
        .choices
        .first()
        .ok_or_else(|| anyhow!("openai api error: empty choices"))?;
    let message = &choice.message;

    let mut content = Vec::new();

    if let Some(text) = &message.content {
        if !text.is_empty() {
            content.push(ContentBlock::Text { text: text.clone() });
        }
    }

    if let Some(tool_calls) = &message.tool_calls {
        for call in tool_calls {
            let input = serde_json::from_str::<serde_json::Value>(&call.function.arguments)
                .unwrap_or(serde_json::Value::Object(Default::default()));
            content.push(ContentBlock::ToolUse {
                id: call.id.clone(),
                name: call.function.name.clone(),
                input,
            });
        }
    }

    let text = message.content.clone().unwrap_or_default();

    Ok(LlmResponse {
        text,
        content,
        input_tokens: body.usage.as_ref().map(|u| u.prompt_tokens),
        output_tokens: body.usage.as_ref().map(|u| u.completion_tokens),
        stop_reason: normalize_finish_reason(choice.finish_reason.clone()),
    })
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

                            match serde_json::from_str::<ApiStreamChunk>(data) {
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

fn parse_sse_event(event: &ApiStreamChunk) -> Option<StreamChunk> {
    let choice = event.choices.first()?;

    if let Some(text) = &choice.delta.content {
        if !text.is_empty() {
            return Some(StreamChunk {
                delta: text.clone(),
                is_final: false,
                input_tokens: None,
                output_tokens: None,
                stop_reason: None,
            });
        }
    }

    if choice.finish_reason.is_some() {
        return Some(StreamChunk {
            delta: String::new(),
            is_final: true,
            input_tokens: event.usage.as_ref().map(|u| u.prompt_tokens),
            output_tokens: event.usage.as_ref().map(|u| u.completion_tokens),
            stop_reason: normalize_finish_reason(choice.finish_reason.clone()),
        });
    }

    None
}

fn normalize_finish_reason(reason: Option<String>) -> Option<String> {
    match reason.as_deref() {
        Some("tool_calls") => Some("tool_use".to_string()),
        Some("stop") => Some("end_turn".to_string()),
        _ => reason,
    }
}

fn format_api_error(status: StatusCode, parsed: Option<ApiErrorEnvelope>) -> anyhow::Error {
    let kind = ProviderErrorKind::from_status(status);
    let retryable = if kind.is_retryable() {
        " [retryable]"
    } else {
        ""
    };
    if let Some(api_error) = parsed {
        anyhow!(
            "openai api error ({status}){retryable}: {} ({})",
            api_error.error.message,
            api_error.error.r#type
        )
    } else {
        anyhow!("openai api error ({status}){retryable}")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ApiRequest {
    pub model: String,
    pub messages: Vec<ApiMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ApiTool>>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<ApiStreamOptions>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ApiMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ApiToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ApiTool {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: ApiFunctionDef,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ApiFunctionDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ApiResponse {
    pub choices: Vec<ApiChoice>,
    #[serde(default)]
    pub usage: Option<ApiUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ApiChoice {
    pub message: ApiAssistantMessage,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ApiAssistantMessage {
    #[serde(default)]
    pub content: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ApiToolCall>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ApiToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: ApiToolFunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ApiToolFunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ApiStreamChunk {
    #[serde(default)]
    pub choices: Vec<ApiStreamChoice>,
    #[serde(default)]
    pub usage: Option<ApiUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ApiStreamChoice {
    #[serde(default)]
    pub delta: ApiStreamDelta,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct ApiStreamDelta {
    #[serde(default)]
    pub content: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ApiStreamOptions {
    pub include_usage: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ApiUsage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ApiErrorEnvelope {
    pub error: ApiErrorBody,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ApiErrorBody {
    #[serde(rename = "type")]
    pub r#type: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolDef;

    #[test]
    fn to_api_request_maps_tools_and_messages() {
        let req = LlmRequest {
            model: "gpt-4o-mini".into(),
            system: None,
            messages: vec![
                LlmMessage::user("hello"),
                LlmMessage {
                    role: "assistant".into(),
                    content: vec![
                        ContentBlock::Text {
                            text: "calling".into(),
                        },
                        ContentBlock::ToolUse {
                            id: "call_1".into(),
                            name: "weather".into(),
                            input: serde_json::json!({"city": "shanghai"}),
                        },
                    ],
                },
                LlmMessage {
                    role: "user".into(),
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "call_1".into(),
                        content: "sunny".into(),
                        is_error: false,
                    }],
                },
            ],
            max_tokens: 128,
            tools: vec![ToolDef {
                name: "weather".into(),
                description: "Get weather".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {"city": {"type": "string"}}
                }),
            }],
        };

        let api = OpenAiProvider::to_api_request(req, false);
        let json = serde_json::to_value(api).unwrap();
        assert!(json["tools"].is_array());
        assert_eq!(json["messages"][1]["role"], "assistant");
        assert!(json["messages"][1]["tool_calls"].is_array());
        assert_eq!(json["messages"][2]["role"], "tool");
    }

    #[test]
    fn to_api_request_includes_system_as_first_message() {
        let req = LlmRequest::simple("gpt-4o-mini".into(), Some("be concise".into()), "hi".into());
        let api = OpenAiProvider::to_api_request(req, false);
        assert_eq!(api.messages[0].role, "system");
        assert_eq!(api.messages[0].content.as_deref(), Some("be concise"));
    }

    #[test]
    fn api_response_deserialization_with_tool_calls() {
        let raw = serde_json::json!({
            "choices": [{
                "message": {
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "weather", "arguments": "{\"city\":\"shanghai\"}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 11, "completion_tokens": 7}
        });
        let parsed: ApiResponse = serde_json::from_value(raw).unwrap();
        let resp = to_llm_response(parsed).unwrap();
        assert!(matches!(resp.content[0], ContentBlock::ToolUse { .. }));
        assert_eq!(resp.stop_reason.as_deref(), Some("tool_use"));
    }

    #[test]
    fn api_response_deserialization_text_only() {
        let raw = serde_json::json!({
            "choices": [{
                "message": {"content": "hello"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 12, "completion_tokens": 3}
        });
        let parsed: ApiResponse = serde_json::from_value(raw).unwrap();
        let resp = to_llm_response(parsed).unwrap();
        assert_eq!(resp.text, "hello");
        assert_eq!(resp.stop_reason.as_deref(), Some("end_turn"));
    }

    #[test]
    fn format_api_error_retryable_for_429() {
        let err = format_api_error(
            StatusCode::TOO_MANY_REQUESTS,
            Some(ApiErrorEnvelope {
                error: ApiErrorBody {
                    r#type: "rate_limit_error".into(),
                    message: "too many requests".into(),
                },
            }),
        );
        assert!(err.to_string().contains("[retryable]"));
    }

    #[test]
    fn format_api_error_not_retryable_for_401() {
        let err = format_api_error(
            StatusCode::UNAUTHORIZED,
            Some(ApiErrorEnvelope {
                error: ApiErrorBody {
                    r#type: "invalid_api_key".into(),
                    message: "bad key".into(),
                },
            }),
        );
        assert!(!err.to_string().contains("[retryable]"));
    }

    #[test]
    fn parse_sse_event_content_delta() {
        let event: ApiStreamChunk = serde_json::from_value(serde_json::json!({
            "choices": [{"delta": {"content": "Hel"}, "finish_reason": null}]
        }))
        .unwrap();
        let chunk = parse_sse_event(&event).unwrap();
        assert_eq!(chunk.delta, "Hel");
        assert!(!chunk.is_final);
    }

    #[test]
    fn parse_sse_event_finish_with_usage() {
        let event: ApiStreamChunk = serde_json::from_value(serde_json::json!({
            "choices": [{"delta": {}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 8, "completion_tokens": 4}
        }))
        .unwrap();
        let chunk = parse_sse_event(&event).unwrap();
        assert!(chunk.is_final);
        assert_eq!(chunk.input_tokens, Some(8));
        assert_eq!(chunk.output_tokens, Some(4));
        assert_eq!(chunk.stop_reason.as_deref(), Some("end_turn"));
    }

    #[test]
    fn to_api_messages_handles_tool_result() {
        let req = LlmRequest {
            model: "gpt-4o-mini".into(),
            system: None,
            messages: vec![LlmMessage {
                role: "user".into(),
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: "ok".into(),
                    is_error: false,
                }],
            }],
            max_tokens: 100,
            tools: vec![],
        };
        let api = OpenAiProvider::to_api_request(req, false);
        assert_eq!(api.messages[0].role, "tool");
        assert_eq!(api.messages[0].tool_call_id.as_deref(), Some("call_1"));
    }

    #[test]
    fn from_env_missing_key_returns_error() {
        std::env::remove_var("OPENAI_KEY_FOR_TEST");
        let result = OpenAiProvider::from_env("OPENAI_KEY_FOR_TEST", "https://api.openai.com/v1");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("OPENAI_KEY_FOR_TEST"));
    }

    #[test]
    fn finish_reason_normalization() {
        assert_eq!(
            normalize_finish_reason(Some("tool_calls".into())).as_deref(),
            Some("tool_use")
        );
        assert_eq!(
            normalize_finish_reason(Some("stop".into())).as_deref(),
            Some("end_turn")
        );
    }
}
