use anyhow::Result;
use async_trait::async_trait;
use futures_core::Stream;
use reqwest::StatusCode;
use std::collections::HashMap;
use std::pin::Pin;
use tokio_stream::StreamExt;

use crate::openai_chatgpt::{
    format_api_error, parse_sse_stream, OpenAiChatGptProvider, ResponsesApiErrorEnvelope,
};
use crate::{ContentBlock, LlmProvider, LlmRequest, LlmResponse, StreamChunk};

#[derive(Debug, Clone)]
pub struct AzureOpenAiProvider {
    client: reqwest::Client,
    pub(crate) api_key: String,
    pub(crate) api_base: String,
}

impl AzureOpenAiProvider {
    pub fn new(api_key: impl Into<String>, api_base: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .unwrap_or_default(),
            api_key: api_key.into(),
            api_base: api_base.into().trim_end_matches('/').to_string(),
        }
    }
}

/// Helper duplicated from OpenAiChatGptProvider::chat() for collecting SSE into a full response.
#[derive(Debug, Clone, Default)]
struct FunctionCallBuilder {
    name: Option<String>,
    arguments: Option<String>,
}

#[async_trait]
impl LlmProvider for AzureOpenAiProvider {
    async fn chat(&self, request: LlmRequest) -> Result<LlmResponse> {
        let has_tools = !request.tools.is_empty();
        if has_tools {
            tracing::debug!(
                "Azure OpenAI Responses API: sending {} tool(s)",
                request.tools.len()
            );
        }

        // Azure OpenAI Responses API also requires stream=true
        let url = format!("{}/responses", self.api_base);
        let payload = OpenAiChatGptProvider::to_responses_request(request, true);

        let req = self
            .client
            .post(url)
            .header("api-key", &self.api_key)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .json(&payload);

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
                                if let Some(_builder) = function_calls.remove(id) {
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
                "Azure OpenAI Responses API: streaming with {} tool(s)",
                request.tools.len()
            );
        }

        let url = format!("{}/responses", self.api_base);
        let payload = OpenAiChatGptProvider::to_responses_request(request, true);

        let req = self
            .client
            .post(url)
            .header("api-key", &self.api_key)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .json(&payload);

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LlmMessage, ToolDef};

    #[test]
    fn azure_provider_constructs_correctly() {
        let provider =
            AzureOpenAiProvider::new("test-key", "https://myresource.openai.azure.com/openai/v1");
        assert_eq!(
            provider.api_base,
            "https://myresource.openai.azure.com/openai/v1"
        );
        assert_eq!(provider.api_key, "test-key");
    }

    #[test]
    fn azure_provider_trims_trailing_slash() {
        let provider =
            AzureOpenAiProvider::new("test-key", "https://myresource.openai.azure.com/openai/v1/");
        assert_eq!(
            provider.api_base,
            "https://myresource.openai.azure.com/openai/v1"
        );
    }

    #[test]
    fn azure_reuses_responses_request_format() {
        let request = LlmRequest {
            model: "gpt-4o".into(),
            system: Some("Be concise".into()),
            messages: vec![LlmMessage::user("Hello")],
            max_tokens: 128,
            tools: vec![ToolDef {
                name: "get_weather".into(),
                description: "Get weather".into(),
                input_schema: serde_json::json!({"type": "object", "properties": {"location": {"type": "string"}}}),
            }],
        };

        let payload = OpenAiChatGptProvider::to_responses_request(request, true);
        assert_eq!(payload.model, "gpt-4o");
        assert_eq!(payload.instructions.as_deref(), Some("Be concise"));
        assert!(payload.tools.is_some());
        assert!(payload.stream);
    }
}
