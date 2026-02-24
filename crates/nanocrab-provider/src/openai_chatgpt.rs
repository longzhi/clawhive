use anyhow::{anyhow, Result};
use async_trait::async_trait;
use futures_core::Stream;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use tokio_stream::StreamExt;

use crate::{ContentBlock, LlmMessage, LlmProvider, LlmRequest, LlmResponse, StreamChunk};

#[derive(Debug, Clone)]
pub struct OpenAiChatGptProvider {
    client: reqwest::Client,
    access_token: String,
    chatgpt_account_id: Option<String>,
    api_base: String,
}

impl OpenAiChatGptProvider {
    pub fn new(
        access_token: impl Into<String>,
        chatgpt_account_id: Option<String>,
        api_base: impl Into<String>,
    ) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .unwrap_or_default(),
            access_token: access_token.into(),
            chatgpt_account_id,
            api_base: api_base.into().trim_end_matches('/').to_string(),
        }
    }

    pub(crate) fn to_responses_request(request: LlmRequest, stream: bool) -> ResponsesRequest {
        ResponsesRequest {
            model: to_responses_model(&request.model),
            input: to_responses_input(request.messages),
            instructions: request.system,
            store: false,
            stream,
        }
    }
}

#[async_trait]
impl LlmProvider for OpenAiChatGptProvider {
    async fn chat(&self, request: LlmRequest) -> Result<LlmResponse> {
        if !request.tools.is_empty() {
            tracing::warn!("ChatGPT Responses API: stripping {} tool(s) from request (not supported via OAuth)", request.tools.len());
        }

        // ChatGPT Codex API requires stream=true, so we stream and collect
        let url = format!("{}/responses", self.api_base);
        let payload = Self::to_responses_request(request, true);

        let mut req = self
            .client
            .post(url)
            .header("authorization", format!("Bearer {}", self.access_token))
            .header("openai-beta", "responses=experimental")
            .header("originator", "nanocrab")
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .json(&payload);

        if let Some(account_id) = &self.chatgpt_account_id {
            req = req.header("chatgpt-account-id", account_id);
        }

        let resp = req.send().await?;
        if resp.status() != StatusCode::OK {
            let status = resp.status();
            let text = resp.text().await?;
            let parsed = serde_json::from_str::<ResponsesApiErrorEnvelope>(&text).ok();
            return Err(format_api_error(status, &text, parsed));
        }

        // Collect SSE stream into full response
        let mut full_text = String::new();
        let mut input_tokens = None;
        let mut output_tokens = None;
        let mut stream = std::pin::pin!(parse_sse_stream(resp.bytes_stream()));
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(chunk) => {
                    full_text.push_str(&chunk.delta);
                    if chunk.input_tokens.is_some() { input_tokens = chunk.input_tokens; }
                    if chunk.output_tokens.is_some() { output_tokens = chunk.output_tokens; }
                }
                Err(e) => tracing::warn!("SSE chunk error in chat(): {e}"),
            }
        }

        Ok(LlmResponse {
            text: full_text.clone(),
            content: vec![crate::ContentBlock::Text { text: full_text }],
            stop_reason: Some("end_turn".to_string()),
            input_tokens,
            output_tokens,
        })
    }

    async fn stream(
        &self,
        request: LlmRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>> {
        if !request.tools.is_empty() {
            tracing::warn!("ChatGPT Responses API: stripping {} tool(s) from request (not supported via OAuth)", request.tools.len());
        }

        let url = format!("{}/responses", self.api_base);
        let payload = Self::to_responses_request(request, true);

        let mut req = self
            .client
            .post(url)
            .header("authorization", format!("Bearer {}", self.access_token))
            .header("openai-beta", "responses=experimental")
            .header("originator", "nanocrab")
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .json(&payload);

        if let Some(account_id) = &self.chatgpt_account_id {
            req = req.header("chatgpt-account-id", account_id);
        }

        let resp = req.send().await?;
        if resp.status() != StatusCode::OK {
            let status = resp.status();
            let text = resp.text().await?;
            let parsed = serde_json::from_str::<ResponsesApiErrorEnvelope>(&text).ok();
            return Err(format_api_error(status, &text, parsed));
        }

        Ok(Box::pin(parse_sse_stream(resp.bytes_stream())))
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

                            match serde_json::from_str::<ResponsesStreamEvent>(data) {
                                Ok(event) => {
                                    if let Some(chunk) = parse_sse_event(event)? {
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

fn parse_sse_event(event: ResponsesStreamEvent) -> Result<Option<StreamChunk>> {
    match event.event_type.as_str() {
        "response.output_text.delta" => {
            let delta = event.delta.unwrap_or_default();
            if delta.is_empty() {
                return Ok(None);
            }

            Ok(Some(StreamChunk {
                delta,
                is_final: false,
                input_tokens: None,
                output_tokens: None,
                stop_reason: None,
                content_blocks: vec![],
            }))
        }
        "response.output_text.done" => Ok(None),
        "response.completed" | "response.done" => Ok(Some(StreamChunk {
            delta: String::new(),
            is_final: true,
            input_tokens: event
                .response
                .as_ref()
                .and_then(|resp| resp.usage.as_ref())
                .map(|usage| usage.input_tokens),
            output_tokens: event
                .response
                .as_ref()
                .and_then(|resp| resp.usage.as_ref())
                .map(|usage| usage.output_tokens),
            stop_reason: Some("end_turn".to_string()),
            content_blocks: vec![],
        })),
        "error" => {
            let message = event.message.unwrap_or_else(|| "unknown error".to_string());
            if let Some(code) = event.code {
                Err(anyhow!("chatgpt responses api error: {message} ({code})"))
            } else {
                Err(anyhow!("chatgpt responses api error: {message}"))
            }
        }
        "response.failed" => {
            let message = event
                .response
                .and_then(|resp| resp.error)
                .map(|err| err.message)
                .unwrap_or_else(|| "unknown error".to_string());
            Err(anyhow!("chatgpt responses api error: {message}"))
        }
        _ => Ok(None),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ResponsesRequest {
    pub model: String,
    pub input: Vec<ResponsesInputMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    pub store: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stream: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ResponsesInputMessage {
    pub role: String,
    pub content: Vec<ResponsesInputContent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ResponsesInputContent {
    #[serde(rename = "type")]
    pub content_type: String,
    pub text: String,
}


#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ResponsesUsage {
    #[serde(default)]
    pub input_tokens: u32,
    #[serde(default)]
    pub output_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ResponsesStreamEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(default)]
    pub delta: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub response: Option<ResponsesStreamEventResponse>,
    #[serde(default)]
    pub code: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ResponsesStreamEventResponse {
    #[serde(default)]
    pub usage: Option<ResponsesUsage>,
    #[serde(default)]
    pub error: Option<ResponsesError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ResponsesError {
    pub message: String,
}

fn to_responses_model(model: &str) -> String {
    model.strip_prefix("openai/").unwrap_or(model).to_string()
}

fn to_responses_input(messages: Vec<LlmMessage>) -> Vec<ResponsesInputMessage> {
    let mut result = Vec::new();
    for message in messages {
        let content_type = match message.role.as_str() {
            "user" => "input_text",
            "assistant" => "output_text",
            _ => {
                tracing::warn!(role = %message.role, "unsupported role for ChatGPT Responses API, skipping message");
                continue;
            }
        };

        let mut contents = Vec::new();
        for block in message.content {
            match block {
                ContentBlock::Text { text } => {
                    if !text.is_empty() {
                        contents.push(ResponsesInputContent {
                            content_type: content_type.to_string(),
                            text,
                        });
                    }
                }
                ContentBlock::ToolUse { .. } | ContentBlock::ToolResult { .. } => {
                    tracing::warn!(
                        role = %message.role,
                        "tool block is not supported by ChatGPT Responses API input, skipping block"
                    );
                }
            }
        }

        if contents.is_empty() {
            continue;
        }

        result.push(ResponsesInputMessage {
            role: message.role,
            content: contents,
        });
    }

    result
}

fn format_api_error(
    status: StatusCode,
    raw_text: &str,
    parsed: Option<ResponsesApiErrorEnvelope>,
) -> anyhow::Error {
    if let Some(api_error) = parsed {
        anyhow!(
            "chatgpt responses api error ({status}): {} ({})",
            api_error.error.message,
            api_error.error.r#type
        )
    } else {
        anyhow!("chatgpt responses api error ({status}): {raw_text}")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ResponsesApiErrorEnvelope {
    error: ResponsesApiErrorBody,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ResponsesApiErrorBody {
    #[serde(rename = "type")]
    r#type: String,
    message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_responses_request_maps_system_and_messages() {
        let request = LlmRequest {
            model: "openai/gpt-4o-mini".into(),
            system: Some("You are concise".into()),
            messages: vec![
                LlmMessage::user("Hello"),
                LlmMessage::assistant("Hi"),
                LlmMessage {
                    role: "assistant".into(),
                    content: vec![
                        ContentBlock::ToolUse {
                            id: "call_1".into(),
                            name: "weather".into(),
                            input: serde_json::json!({"city": "Shanghai"}),
                        },
                        ContentBlock::Text {
                            text: "Done".into(),
                        },
                    ],
                },
            ],
            max_tokens: 128,
            tools: vec![],
        };

        let payload = OpenAiChatGptProvider::to_responses_request(request, true);

        assert_eq!(payload.model, "gpt-4o-mini");
        assert_eq!(payload.instructions.as_deref(), Some("You are concise"));
        assert!(!payload.store);
        assert!(payload.stream);
        assert_eq!(payload.input.len(), 3);
        assert_eq!(payload.input[0].role, "user");
        assert_eq!(payload.input[0].content[0].content_type, "input_text");
        assert_eq!(payload.input[0].content[0].text, "Hello");
        assert_eq!(payload.input[1].role, "assistant");
        assert_eq!(payload.input[1].content[0].content_type, "output_text");
        assert_eq!(payload.input[2].content[0].text, "Done");
    }


    #[tokio::test]
    async fn parse_sse_stream_yields_delta_chunks() {
        let raw = concat!(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hel\"}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\n\n",
            "data: [DONE]\n\n"
        );
        let stream = tokio_stream::iter(vec![Ok(bytes::Bytes::from(raw.as_bytes().to_vec()))]);
        let chunks: Vec<Result<StreamChunk>> = parse_sse_stream(stream).collect().await;

        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].as_ref().unwrap().delta, "Hel");
        assert!(!chunks[0].as_ref().unwrap().is_final);
        assert_eq!(chunks[1].as_ref().unwrap().delta, "lo");
    }

    #[tokio::test]
    async fn parse_sse_stream_yields_final_usage_chunk() {
        let raw = concat!(
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\n",
            "data: [DONE]\n\n"
        );
        let stream = tokio_stream::iter(vec![Ok(bytes::Bytes::from(raw.as_bytes().to_vec()))]);
        let chunks: Vec<Result<StreamChunk>> = parse_sse_stream(stream).collect().await;

        assert_eq!(chunks.len(), 1);
        let chunk = chunks[0].as_ref().unwrap();
        assert!(chunk.is_final);
        assert_eq!(chunk.input_tokens, Some(10));
        assert_eq!(chunk.output_tokens, Some(5));
        assert_eq!(chunk.stop_reason.as_deref(), Some("end_turn"));
    }

    #[tokio::test]
    async fn parse_sse_stream_returns_error_events() {
        let raw = "data: {\"type\":\"error\",\"message\":\"bad\",\"code\":\"invalid_request\"}\n\n";
        let stream = tokio_stream::iter(vec![Ok(bytes::Bytes::from(raw.as_bytes().to_vec()))]);
        let chunks: Vec<Result<StreamChunk>> = parse_sse_stream(stream).collect().await;

        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].as_ref().err().unwrap().to_string().contains("bad"));
    }

    #[tokio::test]
    async fn parse_sse_stream_returns_response_failed_events() {
        let raw = concat!(
            "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"failed\"}}}\n\n",
            "data: [DONE]\n\n"
        );
        let stream = tokio_stream::iter(vec![Ok(bytes::Bytes::from(raw.as_bytes().to_vec()))]);
        let chunks: Vec<Result<StreamChunk>> = parse_sse_stream(stream).collect().await;

        assert_eq!(chunks.len(), 1);
        assert!(chunks[0]
            .as_ref()
            .err()
            .unwrap()
            .to_string()
            .contains("failed"));
    }

    }
