use anyhow::{anyhow, Result};
use async_trait::async_trait;
use futures_core::Stream;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .unwrap_or_default(),
            access_token: access_token.into(),
            chatgpt_account_id,
            api_base: api_base.into().trim_end_matches('/').to_string(),
        }
    }

    pub(crate) fn to_responses_request(request: LlmRequest, stream: bool) -> ResponsesRequest {
        // OpenAI Responses API uses a flat tool structure (different from Chat Completions API):
        // { type: "function", name: "...", description: "...", input_schema: {...} }
        // NOT nested under "function" like Chat Completions API
        let tools = if request.tools.is_empty() {
            None
        } else {
            Some(
                request
                    .tools
                    .iter()
                    .map(|t| ResponsesTool {
                        tool_type: "function".to_string(),
                        name: t.name.clone(),
                        description: Some(t.description.clone()),
                        input_schema: Some(t.input_schema.clone()),
                    })
                    .collect(),
            )
        };

        ResponsesRequest {
            model: to_responses_model(&request.model),
            input: to_responses_input(request.messages),
            instructions: request.system,
            tools,
            tool_choice: if request.tools.is_empty() {
                None
            } else {
                Some("auto".to_string())
            },
            store: false,
            stream,
        }
    }
}

#[async_trait]
impl LlmProvider for OpenAiChatGptProvider {
    async fn chat(&self, request: LlmRequest) -> Result<LlmResponse> {
        let has_tools = !request.tools.is_empty();
        if has_tools {
            tracing::debug!(
                "ChatGPT Responses API: sending {} tool(s) via function_call format",
                request.tools.len()
            );
        }

        // ChatGPT Codex API requires stream=true, so we stream and collect
        let url = format!("{}/responses", self.api_base);
        let payload = Self::to_responses_request(request, true);

        // Debug: log the actual tools payload
        if let Some(ref tools) = payload.tools {
            tracing::debug!(
                "ChatGPT tools payload: {}",
                serde_json::to_string(tools).unwrap_or_else(|_| "failed to serialize".to_string())
            );
        }

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
        let mut content_blocks: Vec<ContentBlock> = Vec::new();
        let mut input_tokens = None;
        let mut output_tokens = None;
        let mut stop_reason = None;

        // Track function calls being built
        let mut function_calls: HashMap<String, FunctionCallBuilder> = HashMap::new();

        let mut stream = std::pin::pin!(parse_sse_stream(resp.bytes_stream()));
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(chunk) => {
                    full_text.push_str(&chunk.delta);
                    if chunk.input_tokens.is_some() {
                        input_tokens = chunk.input_tokens;
                    }
                    if chunk.output_tokens.is_some() {
                        output_tokens = chunk.output_tokens;
                    }
                    if chunk.stop_reason.is_some() {
                        stop_reason = chunk.stop_reason.clone();
                    }

                    // Collect function call blocks
                    for block in chunk.content_blocks {
                        match &block {
                            ContentBlock::ToolUse { id, .. } => {
                                // Merge with any partial function call we're building
                                if let Some(_builder) = function_calls.remove(id) {
                                    // Already have final, just add
                                    content_blocks.push(block);
                                } else {
                                    content_blocks.push(block);
                                }
                            }
                            _ => content_blocks.push(block),
                        }
                    }
                }
                Err(e) => tracing::warn!("SSE chunk error in chat(): {e}"),
            }
        }

        // Add any remaining partial function calls
        for (call_id, builder) in function_calls {
            if let (Some(name), Some(arguments)) = (builder.name, builder.arguments) {
                content_blocks.push(ContentBlock::ToolUse {
                    id: call_id,
                    name,
                    input: serde_json::from_str(&arguments).unwrap_or(serde_json::Value::Null),
                });
            }
        }

        // If we have text, ensure there's a text content block
        if !full_text.is_empty()
            && !content_blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { .. }))
        {
            content_blocks.insert(
                0,
                ContentBlock::Text {
                    text: full_text.clone(),
                },
            );
        }

        // Determine stop reason
        let final_stop_reason = if content_blocks
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolUse { .. }))
        {
            Some("tool_use".to_string())
        } else {
            stop_reason.or_else(|| Some("end_turn".to_string()))
        };

        Ok(LlmResponse {
            text: full_text,
            content: content_blocks,
            stop_reason: final_stop_reason,
            input_tokens,
            output_tokens,
        })
    }

    async fn stream(
        &self,
        request: LlmRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>> {
        let has_tools = !request.tools.is_empty();
        if has_tools {
            tracing::debug!(
                "ChatGPT Responses API: streaming with {} tool(s) via function_call format",
                request.tools.len()
            );
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

#[derive(Debug, Clone, Default)]
struct FunctionCallBuilder {
    name: Option<String>,
    arguments: Option<String>,
}

fn parse_sse_stream(
    byte_stream: impl Stream<Item = std::result::Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
) -> impl Stream<Item = Result<StreamChunk>> + Send {
    async_stream::stream! {
        tokio::pin!(byte_stream);
        let mut buffer = String::new();

        // Track function calls being built across events
        let mut function_calls: HashMap<String, FunctionCallBuilder> = HashMap::new();

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
                                    if let Some(chunk) = parse_sse_event(event, &mut function_calls)? {
                                        yield Ok(chunk);
                                    }
                                }
                                Err(e) => {
                                    tracing::debug!("Skipping unparseable SSE event: {e} - data: {data}");
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

        // Yield any remaining function calls
        for (call_id, builder) in function_calls {
            if let (Some(name), Some(arguments)) = (builder.name, builder.arguments) {
                yield Ok(StreamChunk {
                    delta: String::new(),
                    is_final: false,
                    input_tokens: None,
                    output_tokens: None,
                    stop_reason: None,
                    content_blocks: vec![ContentBlock::ToolUse {
                        id: call_id,
                        name,
                        input: serde_json::from_str(&arguments).unwrap_or(serde_json::Value::Null),
                    }],
                });
            }
        }
    }
}

fn parse_sse_event(
    event: ResponsesStreamEvent,
    function_calls: &mut HashMap<String, FunctionCallBuilder>,
) -> Result<Option<StreamChunk>> {
    match event.event_type.as_str() {
        // Text output delta
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

        // Function call output item added - initial notification
        "response.output_item.added" => {
            if let Some(item) = event.item {
                if item.item_type == Some("function_call".to_string()) {
                    let call_id = item.call_id.clone().unwrap_or_else(|| {
                        item.id.clone().unwrap_or_else(|| format!("call_{}", uuid::Uuid::new_v4()))
                    });
                    let builder = function_calls.entry(call_id).or_default();
                    if let Some(name) = item.name {
                        builder.name = Some(name);
                    }
                    if let Some(args) = item.arguments {
                        builder.arguments = Some(args);
                    }
                }
            }
            Ok(None)
        }

        // Function call arguments streaming
        "response.function_call_arguments.delta" => {
            if let Some(item_id) = event.item_id.or(event.call_id.clone()) {
                let delta = event.delta.unwrap_or_default();
                let builder = function_calls.entry(item_id).or_default();
                if let Some(ref mut args) = builder.arguments {
                    args.push_str(&delta);
                } else {
                    builder.arguments = Some(delta);
                }
            }
            Ok(None)
        }

        // Function call arguments complete
        "response.function_call_arguments.done" => {
            let call_id = event.item_id.or(event.call_id.clone());
            let arguments = event.arguments.or(event.delta);

            if let Some(call_id) = call_id {
                if let Some(mut builder) = function_calls.remove(&call_id) {
                    // If we got final arguments in this event, use them
                    if let Some(args) = arguments {
                        builder.arguments = Some(args);
                    }

                    if let (Some(name), Some(args)) = (builder.name, builder.arguments) {
                        return Ok(Some(StreamChunk {
                            delta: String::new(),
                            is_final: false,
                            input_tokens: None,
                            output_tokens: None,
                            stop_reason: None,
                            content_blocks: vec![ContentBlock::ToolUse {
                                id: call_id,
                                name,
                                input: serde_json::from_str(&args).unwrap_or(serde_json::Value::Null),
                            }],
                        }));
                    }
                }
            }
            Ok(None)
        }

        // Function call item done
        "response.output_item.done" => {
            if let Some(item) = event.item {
                if item.item_type == Some("function_call".to_string()) {
                    let call_id = item.call_id.or(item.id).unwrap_or_default();
                    let name = item.name.unwrap_or_default();
                    let arguments = item.arguments.unwrap_or_else(|| "{}".to_string());

                    // Remove from tracking if still there
                    function_calls.remove(&call_id);

                    return Ok(Some(StreamChunk {
                        delta: String::new(),
                        is_final: false,
                        input_tokens: None,
                        output_tokens: None,
                        stop_reason: None,
                        content_blocks: vec![ContentBlock::ToolUse {
                            id: call_id,
                            name,
                            input: serde_json::from_str(&arguments).unwrap_or(serde_json::Value::Null),
                        }],
                    }));
                }
            }
            Ok(None)
        }

        // Response completed
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

        // Error events
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

// ============ Request Types ============

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ResponsesRequest {
    pub model: String,
    pub input: Vec<ResponsesInputItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ResponsesTool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<String>,
    pub store: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stream: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum ResponsesInputItem {
    /// User or assistant message
    Message {
        role: String,
        content: Vec<ResponsesInputContent>,
    },
    /// Function call (tool use)
    FunctionCall {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        call_id: String,
        name: String,
        arguments: String,
    },
    /// Function call output (tool result)
    FunctionCallOutput {
        call_id: String,
        output: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ResponsesInputContent {
    #[serde(rename = "type")]
    pub content_type: String,
    pub text: String,
}

/// Tool definition for Responses API (flat structure, different from Chat Completions API)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ResponsesTool {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<serde_json::Value>,
}

// ============ Response Types ============

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
    #[serde(default)]
    pub item: Option<ResponsesOutputItem>,
    #[serde(default)]
    pub item_id: Option<String>,
    #[serde(default)]
    pub call_id: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ResponsesOutputItem {
    #[serde(rename = "type")]
    pub item_type: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub call_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
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

// ============ Conversion Helpers ============

fn to_responses_model(model: &str) -> String {
    model.strip_prefix("openai/").unwrap_or(model).to_string()
}

fn to_responses_input(messages: Vec<LlmMessage>) -> Vec<ResponsesInputItem> {
    let mut result = Vec::new();

    for message in messages {
        let content_type = match message.role.as_str() {
            "user" => "input_text",
            "assistant" => "output_text",
            _ => {
                // Check if this is a tool result message
                if message.role == "tool" {
                    // Tool results are handled separately
                    for block in message.content {
                        if let ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } = block
                        {
                            let output = if is_error {
                                format!("Error: {}", content)
                            } else {
                                content
                            };
                            result.push(ResponsesInputItem::FunctionCallOutput {
                                call_id: tool_use_id,
                                output,
                            });
                        }
                    }
                    continue;
                }
                tracing::warn!(
                    role = %message.role,
                    "unsupported role for ChatGPT Responses API, skipping message"
                );
                continue;
            }
        };

        let mut text_contents = Vec::new();

        for block in message.content {
            match block {
                ContentBlock::Text { text } => {
                    if !text.is_empty() {
                        text_contents.push(ResponsesInputContent {
                            content_type: content_type.to_string(),
                            text,
                        });
                    }
                }
                ContentBlock::ToolUse { id, name, input } => {
                    // First, flush any accumulated text content
                    if !text_contents.is_empty() {
                        result.push(ResponsesInputItem::Message {
                            role: message.role.clone(),
                            content: std::mem::take(&mut text_contents),
                        });
                    }

                    // Add the function call
                    let arguments = serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string());
                    result.push(ResponsesInputItem::FunctionCall {
                        id: Some(format!("fc_{}", uuid::Uuid::new_v4())),
                        call_id: id,
                        name,
                        arguments,
                    });
                }
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => {
                    // First, flush any accumulated text content
                    if !text_contents.is_empty() {
                        result.push(ResponsesInputItem::Message {
                            role: message.role.clone(),
                            content: std::mem::take(&mut text_contents),
                        });
                    }

                    // Add the function call output
                    let output = if is_error {
                        format!("Error: {}", content)
                    } else {
                        content
                    };
                    result.push(ResponsesInputItem::FunctionCallOutput {
                        call_id: tool_use_id,
                        output,
                    });
                }
            }
        }

        // Add any remaining text content
        if !text_contents.is_empty() {
            result.push(ResponsesInputItem::Message {
                role: message.role,
                content: text_contents,
            });
        }
    }

    result
}

fn format_api_error(
    status: StatusCode,
    raw_text: &str,
    parsed: Option<ResponsesApiErrorEnvelope>,
) -> anyhow::Error {
    let retryable = matches!(
        status.as_u16(),
        429 | 500 | 502 | 503 | 504 | 520 | 522 | 524
    );
    let tag = if retryable { " [retryable]" } else { "" };

    if let Some(api_error) = parsed {
        anyhow!(
            "chatgpt responses api error ({status}){tag}: {} ({})",
            api_error.error.message,
            api_error.error.r#type
        )
    } else {
        anyhow!("chatgpt responses api error ({status}){tag}: {raw_text}")
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
    use crate::ToolDef;

    #[test]
    fn to_responses_request_maps_system_and_messages() {
        let request = LlmRequest {
            model: "openai/gpt-5.3-codex".into(),
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

        assert_eq!(payload.model, "gpt-5.3-codex");
        assert_eq!(payload.instructions.as_deref(), Some("You are concise"));
        assert!(!payload.store);
        assert!(payload.stream);
        // Check that we have messages and function calls
        assert!(payload.input.len() >= 3);
    }

    #[test]
    fn to_responses_request_includes_tools() {
        let request = LlmRequest {
            model: "openai/gpt-5.3-codex".into(),
            system: None,
            messages: vec![LlmMessage::user("What's the weather?")],
            max_tokens: 128,
            tools: vec![ToolDef {
                name: "get_weather".into(),
                description: "Get weather for a location".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "location": {"type": "string"}
                    },
                    "required": ["location"]
                }),
            }],
        };

        let payload = OpenAiChatGptProvider::to_responses_request(request, true);

        assert!(payload.tools.is_some());
        let tools = payload.tools.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "get_weather");
        assert_eq!(payload.tool_choice, Some("auto".to_string()));
    }

    #[test]
    fn to_responses_input_converts_tool_use_and_result() {
        let messages = vec![
            LlmMessage::user("What's the weather in Tokyo?"),
            LlmMessage {
                role: "assistant".into(),
                content: vec![ContentBlock::ToolUse {
                    id: "call_abc123".into(),
                    name: "get_weather".into(),
                    input: serde_json::json!({"location": "Tokyo"}),
                }],
            },
            LlmMessage {
                role: "user".into(),
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_abc123".into(),
                    content: "Sunny, 25°C".into(),
                    is_error: false,
                }],
            },
        ];

        let input = to_responses_input(messages);

        // Should have: message, function_call, function_call_output
        assert!(input.len() >= 3);

        // Check function call
        let has_function_call = input.iter().any(|item| {
            matches!(item, ResponsesInputItem::FunctionCall { name, .. } if name == "get_weather")
        });
        assert!(has_function_call);

        // Check function call output
        let has_output = input.iter().any(|item| {
            matches!(item, ResponsesInputItem::FunctionCallOutput { output, .. } if output.contains("Sunny"))
        });
        assert!(has_output);
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
    async fn parse_sse_stream_parses_function_call() {
        let raw = concat!(
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_xyz\",\"name\":\"get_weather\",\"arguments\":\"\"}}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"call_xyz\",\"delta\":\"{\\\"loc\"}\n\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"call_xyz\",\"delta\":\"ation\\\": \\\"Tokyo\\\"}\"}\n\n",
            "data: {\"type\":\"response.function_call_arguments.done\",\"item_id\":\"call_xyz\",\"arguments\":\"{\\\"location\\\": \\\"Tokyo\\\"}\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\n",
            "data: [DONE]\n\n"
        );
        let stream = tokio_stream::iter(vec![Ok(bytes::Bytes::from(raw.as_bytes().to_vec()))]);
        let chunks: Vec<Result<StreamChunk>> = parse_sse_stream(stream).collect().await;

        // Should have a function call chunk and a final chunk
        let tool_use_chunk = chunks.iter().find(|c| {
            c.as_ref()
                .map(|chunk| !chunk.content_blocks.is_empty())
                .unwrap_or(false)
        });
        assert!(tool_use_chunk.is_some());

        let chunk = tool_use_chunk.unwrap().as_ref().unwrap();
        assert_eq!(chunk.content_blocks.len(), 1);
        match &chunk.content_blocks[0] {
            ContentBlock::ToolUse { id, name, input } => {
                assert_eq!(id, "call_xyz");
                assert_eq!(name, "get_weather");
                assert_eq!(input["location"], "Tokyo");
            }
            _ => panic!("Expected ToolUse block"),
        }
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
