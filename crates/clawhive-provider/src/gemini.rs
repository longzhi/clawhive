//! Google Gemini API provider
//!
//! https://ai.google.dev/api/generate-content

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use futures_core::Stream;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use tokio_stream::StreamExt;

use crate::{ContentBlock, LlmProvider, LlmRequest, LlmResponse, StreamChunk};

const GEMINI_API_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";

#[derive(Debug, Clone)]
pub struct GeminiProvider {
    client: reqwest::Client,
    api_key: String,
}

impl GeminiProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .unwrap_or_default(),
            api_key: api_key.into(),
        }
    }

    fn build_request(&self, request: &LlmRequest) -> GeminiRequest {
        let mut contents = Vec::new();

        for msg in &request.messages {
            let role = match msg.role.as_str() {
                "user" => "user",
                "assistant" => "model",
                _ => "user",
            };

            let mut parts = Vec::new();

            for block in &msg.content {
                match block {
                    ContentBlock::Text { text } => {
                        parts.push(GeminiPart::Text { text: text.clone() });
                    }
                    ContentBlock::ToolUse { id, name, input } => {
                        parts.push(GeminiPart::FunctionCall {
                            function_call: GeminiFunctionCall {
                                name: name.clone(),
                                args: input.clone(),
                            },
                        });
                        // Store ID for later matching (Gemini doesn't use IDs)
                        let _ = id;
                    }
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } => {
                        parts.push(GeminiPart::FunctionResponse {
                            function_response: GeminiFunctionResponse {
                                name: tool_use_id.clone(), // Gemini uses name, we pass ID
                                response: serde_json::json!({ "result": content }),
                            },
                        });
                    }
                }
            }

            if !parts.is_empty() {
                contents.push(GeminiContent {
                    role: role.to_string(),
                    parts,
                });
            }
        }

        let tools = if request.tools.is_empty() {
            None
        } else {
            let function_declarations: Vec<GeminiFunctionDeclaration> = request
                .tools
                .iter()
                .map(|tool| GeminiFunctionDeclaration {
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    parameters: tool.input_schema.clone(),
                })
                .collect();
            Some(vec![GeminiTool {
                function_declarations,
            }])
        };

        GeminiRequest {
            contents,
            system_instruction: request.system.as_ref().map(|s| GeminiContent {
                role: "user".to_string(),
                parts: vec![GeminiPart::Text { text: s.clone() }],
            }),
            generation_config: Some(GeminiGenerationConfig {
                max_output_tokens: Some(request.max_tokens),
                temperature: None,
                top_p: None,
                top_k: None,
            }),
            tools,
        }
    }
}

#[async_trait]
impl LlmProvider for GeminiProvider {
    async fn chat(&self, request: LlmRequest) -> Result<LlmResponse> {
        let model = &request.model;
        let url = format!(
            "{}/models/{}:generateContent?key={}",
            GEMINI_API_BASE, model, self.api_key
        );

        let payload = self.build_request(&request);

        let resp = match self
            .client
            .post(&url)
            .header("content-type", "application/json")
            .json(&payload)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) if e.is_timeout() => {
                return Err(anyhow!(
                    "gemini api error (timeout) [retryable]: request timed out"
                ));
            }
            Err(e) if e.is_connect() => {
                return Err(anyhow!("gemini api error (connect) [retryable]: {e}"));
            }
            Err(e) => return Err(e.into()),
        };

        let status = resp.status();
        if status != StatusCode::OK {
            let text = resp.text().await?;
            return Err(format_api_error(status, &text));
        }

        let body: GeminiResponse = resp.json().await?;
        to_llm_response(body)
    }

    async fn stream(
        &self,
        request: LlmRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>> {
        let model = &request.model;
        let url = format!(
            "{}/models/{}:streamGenerateContent?key={}&alt=sse",
            GEMINI_API_BASE, model, self.api_key
        );

        let payload = self.build_request(&request);

        let resp = match self
            .client
            .post(&url)
            .header("content-type", "application/json")
            .json(&payload)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) if e.is_timeout() => {
                return Err(anyhow!(
                    "gemini api error (timeout) [retryable]: request timed out"
                ));
            }
            Err(e) if e.is_connect() => {
                return Err(anyhow!("gemini api error (connect) [retryable]: {e}"));
            }
            Err(e) => return Err(e.into()),
        };

        let status = resp.status();
        if status != StatusCode::OK {
            let text = resp.text().await?;
            return Err(format_api_error(status, &text));
        }

        let sse_stream = parse_sse_stream(resp.bytes_stream());
        Ok(Box::pin(sse_stream))
    }
}

fn to_llm_response(body: GeminiResponse) -> Result<LlmResponse> {
    let candidate = body
        .candidates
        .first()
        .ok_or_else(|| anyhow!("gemini api error: empty candidates"))?;

    let mut content = Vec::new();
    let mut text = String::new();

    for part in &candidate.content.parts {
        match part {
            GeminiPart::Text { text: t } => {
                if !t.is_empty() {
                    text.push_str(t);
                    content.push(ContentBlock::Text { text: t.clone() });
                }
            }
            GeminiPart::FunctionCall { function_call } => {
                content.push(ContentBlock::ToolUse {
                    id: format!("gemini_{}", function_call.name),
                    name: function_call.name.clone(),
                    input: function_call.args.clone(),
                });
            }
            _ => {}
        }
    }

    let stop_reason = match candidate.finish_reason.as_deref() {
        Some("STOP") => Some("end_turn".to_string()),
        Some("MAX_TOKENS") => Some("max_tokens".to_string()),
        Some("SAFETY") => Some("safety".to_string()),
        Some(r) => Some(r.to_lowercase()),
        None => None,
    };

    Ok(LlmResponse {
        text,
        content,
        input_tokens: body.usage_metadata.as_ref().map(|u| u.prompt_token_count),
        output_tokens: body
            .usage_metadata
            .as_ref()
            .map(|u| u.candidates_token_count),
        stop_reason,
    })
}

fn parse_sse_stream(
    byte_stream: impl Stream<Item = std::result::Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
) -> impl Stream<Item = Result<StreamChunk>> + Send {
    async_stream::stream! {
        tokio::pin!(byte_stream);
        let mut buffer = String::new();
        let mut accumulated_text = String::new();
        let mut tool_calls: Vec<ContentBlock> = Vec::new();

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

                            match serde_json::from_str::<GeminiResponse>(data) {
                                Ok(response) => {
                                    if let Some(candidate) = response.candidates.first() {
                                        for part in &candidate.content.parts {
                                            match part {
                                                GeminiPart::Text { text } => {
                                                    if !text.is_empty() {
                                                        accumulated_text.push_str(text);
                                                        yield Ok(StreamChunk {
                                                            delta: text.clone(),
                                                            is_final: false,
                                                            input_tokens: None,
                                                            output_tokens: None,
                                                            stop_reason: None,
                                                            content_blocks: vec![],
                                                        });
                                                    }
                                                }
                                                GeminiPart::FunctionCall { function_call } => {
                                                    tool_calls.push(ContentBlock::ToolUse {
                                                        id: format!("gemini_{}", function_call.name),
                                                        name: function_call.name.clone(),
                                                        input: function_call.args.clone(),
                                                    });
                                                }
                                                _ => {}
                                            }
                                        }

                                        if candidate.finish_reason.is_some() {
                                            let stop_reason = match candidate.finish_reason.as_deref() {
                                                Some("STOP") => Some("end_turn".to_string()),
                                                Some("MAX_TOKENS") => Some("max_tokens".to_string()),
                                                Some(r) => Some(r.to_lowercase()),
                                                None => None,
                                            };

                                            yield Ok(StreamChunk {
                                                delta: String::new(),
                                                is_final: true,
                                                input_tokens: response.usage_metadata.as_ref().map(|u| u.prompt_token_count),
                                                output_tokens: response.usage_metadata.as_ref().map(|u| u.candidates_token_count),
                                                stop_reason,
                                                content_blocks: std::mem::take(&mut tool_calls),
                                            });
                                        }
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

fn format_api_error(status: StatusCode, text: &str) -> anyhow::Error {
    let retryable = match status.as_u16() {
        429 | 500..=599 => " [retryable]",
        _ => "",
    };
    anyhow!("gemini api error ({status}){retryable}: {text}")
}

// ============================================================
// Gemini API Types
// ============================================================

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiRequest {
    contents: Vec<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation_config: Option<GeminiGenerationConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<GeminiTool>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GeminiContent {
    role: String,
    parts: Vec<GeminiPart>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
enum GeminiPart {
    Text {
        text: String,
    },
    FunctionCall {
        #[serde(rename = "functionCall")]
        function_call: GeminiFunctionCall,
    },
    FunctionResponse {
        #[serde(rename = "functionResponse")]
        function_response: GeminiFunctionResponse,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GeminiFunctionCall {
    name: String,
    args: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GeminiFunctionResponse {
    name: String,
    response: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiGenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_k: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiTool {
    function_declarations: Vec<GeminiFunctionDeclaration>,
}

#[derive(Debug, Clone, Serialize)]
struct GeminiFunctionDeclaration {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiResponse {
    candidates: Vec<GeminiCandidate>,
    #[serde(default)]
    usage_metadata: Option<GeminiUsageMetadata>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiCandidate {
    content: GeminiContent,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiUsageMetadata {
    #[serde(default)]
    prompt_token_count: u32,
    #[serde(default)]
    candidates_token_count: u32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LlmMessage, ToolDef};

    #[test]
    fn build_request_basic() {
        let provider = GeminiProvider::new("test-key");
        let req = LlmRequest::simple("gemini-pro".into(), Some("Be helpful".into()), "Hi".into());
        let api_req = provider.build_request(&req);

        assert!(api_req.system_instruction.is_some());
        assert_eq!(api_req.contents.len(), 1);
        assert_eq!(api_req.contents[0].role, "user");
    }

    #[test]
    fn build_request_with_tools() {
        let provider = GeminiProvider::new("test-key");
        let req = LlmRequest {
            model: "gemini-pro".into(),
            system: None,
            messages: vec![LlmMessage::user("What's the weather?")],
            max_tokens: 1000,
            tools: vec![ToolDef {
                name: "get_weather".into(),
                description: "Get weather info".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": { "city": { "type": "string" } }
                }),
            }],
        };
        let api_req = provider.build_request(&req);

        assert!(api_req.tools.is_some());
        assert_eq!(
            api_req.tools.as_ref().unwrap()[0]
                .function_declarations
                .len(),
            1
        );
    }

    #[test]
    fn to_llm_response_text_only() {
        let raw = serde_json::json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"text": "Hello!"}]
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 5,
                "candidatesTokenCount": 2
            }
        });
        let parsed: GeminiResponse = serde_json::from_value(raw).unwrap();
        let resp = to_llm_response(parsed).unwrap();

        assert_eq!(resp.text, "Hello!");
        assert_eq!(resp.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(resp.input_tokens, Some(5));
        assert_eq!(resp.output_tokens, Some(2));
    }

    #[test]
    fn to_llm_response_with_function_call() {
        let raw = serde_json::json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{
                        "functionCall": {
                            "name": "get_weather",
                            "args": {"city": "Tokyo"}
                        }
                    }]
                },
                "finishReason": "STOP"
            }]
        });
        let parsed: GeminiResponse = serde_json::from_value(raw).unwrap();
        let resp = to_llm_response(parsed).unwrap();

        assert_eq!(resp.content.len(), 1);
        assert!(
            matches!(&resp.content[0], ContentBlock::ToolUse { name, .. } if name == "get_weather")
        );
    }
}
