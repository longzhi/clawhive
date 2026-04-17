//! Converse API request/response schema + mapping to/from LlmRequest/LlmResponse.

use anyhow::{anyhow, Result};
use serde::Serialize;
use serde_json::Value as JsonValue;

use crate::types::{ContentBlock, LlmMessage, LlmRequest, LlmResponse};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConverseRequest {
    pub messages: Vec<ConverseMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<Vec<ConverseSystemBlock>>,
    pub inference_config: InferenceConfig,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_config: Option<ToolConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub additional_model_request_fields: Option<JsonValue>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConverseMessage {
    pub role: String,
    pub content: Vec<ConverseContent>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ConverseContent {
    Text {
        text: String,
    },
    Image {
        image: ConverseImage,
    },
    ToolUse {
        #[serde(rename = "toolUse")]
        tool_use: ConverseToolUse,
    },
    ToolResult {
        #[serde(rename = "toolResult")]
        tool_result: ConverseToolResult,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct ConverseImage {
    pub format: String,
    pub source: ConverseImageSource,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConverseImageSource {
    pub bytes: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConverseToolUse {
    pub tool_use_id: String,
    pub name: String,
    pub input: JsonValue,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConverseToolResult {
    pub tool_use_id: String,
    pub content: Vec<ConverseContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConverseSystemBlock {
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InferenceConfig {
    pub max_tokens: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolConfig {
    pub tools: Vec<ToolSpecWrapper>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolSpecWrapper {
    #[serde(rename = "toolSpec")]
    pub tool_spec: ToolSpec,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: ToolInputSchema,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolInputSchema {
    pub json: JsonValue,
}

pub fn to_converse_request(req: &LlmRequest) -> ConverseRequest {
    let messages: Vec<ConverseMessage> = req.messages.iter().map(message_to_converse).collect();
    let system = req
        .system
        .as_ref()
        .map(|s| vec![ConverseSystemBlock { text: s.clone() }]);
    let tool_config = if req.tools.is_empty() {
        None
    } else {
        Some(ToolConfig {
            tools: req
                .tools
                .iter()
                .map(|t| ToolSpecWrapper {
                    tool_spec: ToolSpec {
                        name: t.name.clone(),
                        description: t.description.clone(),
                        input_schema: ToolInputSchema {
                            json: t.input_schema.clone(),
                        },
                    },
                })
                .collect(),
        })
    };
    ConverseRequest {
        messages,
        system,
        inference_config: InferenceConfig {
            max_tokens: req.max_tokens,
        },
        tool_config,
        additional_model_request_fields: None,
    }
}

fn message_to_converse(msg: &LlmMessage) -> ConverseMessage {
    ConverseMessage {
        role: msg.role.clone(),
        content: msg.content.iter().map(content_to_converse).collect(),
    }
}

fn content_to_converse(block: &ContentBlock) -> ConverseContent {
    match block {
        ContentBlock::Text { text } => ConverseContent::Text { text: text.clone() },
        ContentBlock::Image { data, media_type } => ConverseContent::Image {
            image: ConverseImage {
                format: media_type_to_format(media_type),
                source: ConverseImageSource {
                    bytes: data.clone(),
                },
            },
        },
        ContentBlock::ToolUse { id, name, input } => ConverseContent::ToolUse {
            tool_use: ConverseToolUse {
                tool_use_id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            },
        },
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => ConverseContent::ToolResult {
            tool_result: ConverseToolResult {
                tool_use_id: tool_use_id.clone(),
                content: vec![ConverseContent::Text {
                    text: content.clone(),
                }],
                status: Some(if *is_error {
                    "error".into()
                } else {
                    "success".into()
                }),
            },
        },
    }
}

fn media_type_to_format(mt: &str) -> String {
    match mt {
        "image/png" => "png".into(),
        "image/jpeg" | "image/jpg" => "jpeg".into(),
        "image/gif" => "gif".into(),
        "image/webp" => "webp".into(),
        other => other.trim_start_matches("image/").to_string(),
    }
}

pub fn from_converse_response(raw: JsonValue) -> Result<LlmResponse> {
    let message = raw
        .get("output")
        .and_then(|o| o.get("message"))
        .ok_or_else(|| anyhow!("converse response missing output.message"))?;
    let content_array = message
        .get("content")
        .and_then(|c| c.as_array())
        .ok_or_else(|| anyhow!("converse response missing output.message.content array"))?;

    let mut blocks: Vec<ContentBlock> = Vec::with_capacity(content_array.len());
    let mut text_parts: Vec<String> = Vec::new();

    for item in content_array {
        if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
            blocks.push(ContentBlock::Text {
                text: text.to_string(),
            });
            text_parts.push(text.to_string());
        } else if let Some(tool_use) = item.get("toolUse") {
            let id = tool_use
                .get("toolUseId")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("toolUse missing toolUseId"))?
                .to_string();
            let name = tool_use
                .get("name")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("toolUse missing name"))?
                .to_string();
            let input = tool_use.get("input").cloned().unwrap_or(JsonValue::Null);
            blocks.push(ContentBlock::ToolUse { id, name, input });
        }
    }

    let stop_reason = raw
        .get("stopReason")
        .and_then(|s| s.as_str())
        .map(String::from);
    let input_tokens = raw
        .get("usage")
        .and_then(|u| u.get("inputTokens"))
        .and_then(|v| v.as_u64())
        .map(|n| n as u32);
    let output_tokens = raw
        .get("usage")
        .and_then(|u| u.get("outputTokens"))
        .and_then(|v| v.as_u64())
        .map(|n| n as u32);

    Ok(LlmResponse {
        text: text_parts.join(""),
        content: blocks,
        input_tokens,
        output_tokens,
        stop_reason,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{LlmMessage, LlmRequest};

    #[test]
    fn to_converse_request_plain_text() {
        let req = LlmRequest::simple(
            "anthropic.claude-3-5-sonnet-20241022-v2:0".into(),
            Some("You are a helpful assistant.".into()),
            "hello".into(),
        );
        let cv = to_converse_request(&req);
        let json = serde_json::to_value(&cv).unwrap();

        assert_eq!(json["system"][0]["text"], "You are a helpful assistant.");
        assert_eq!(json["messages"][0]["role"], "user");
        assert_eq!(json["messages"][0]["content"][0]["text"], "hello");
        assert_eq!(json["inferenceConfig"]["maxTokens"], 2048);
        assert!(json.get("toolConfig").is_none(), "no tools → no toolConfig");
    }

    #[test]
    fn to_converse_request_no_system() {
        let req = LlmRequest {
            model: "meta.llama3-1-70b-instruct-v1:0".into(),
            system: None,
            messages: vec![LlmMessage::user("ping")],
            max_tokens: 512,
            tools: vec![],
            thinking_level: None,
        };
        let cv = to_converse_request(&req);
        let json = serde_json::to_value(&cv).unwrap();
        assert!(json.get("system").is_none());
        assert_eq!(json["inferenceConfig"]["maxTokens"], 512);
    }

    #[test]
    fn to_converse_request_with_image() {
        use crate::types::ContentBlock;
        let req = LlmRequest {
            model: "anthropic.claude-3-5-sonnet-20241022-v2:0".into(),
            system: None,
            messages: vec![LlmMessage {
                role: "user".into(),
                content: vec![
                    ContentBlock::Text {
                        text: "what's this?".into(),
                    },
                    ContentBlock::Image {
                        data: "iVBORw0KGgo=".into(),
                        media_type: "image/png".into(),
                    },
                ],
            }],
            max_tokens: 1024,
            tools: vec![],
            thinking_level: None,
        };
        let cv = to_converse_request(&req);
        let json = serde_json::to_value(&cv).unwrap();
        assert_eq!(json["messages"][0]["content"][0]["text"], "what's this?");
        assert_eq!(json["messages"][0]["content"][1]["image"]["format"], "png");
        assert_eq!(
            json["messages"][0]["content"][1]["image"]["source"]["bytes"],
            "iVBORw0KGgo="
        );
    }

    #[test]
    fn to_converse_request_with_tool_use_and_result() {
        use crate::types::{ContentBlock, ToolDef};
        let req = LlmRequest {
            model: "anthropic.claude-3-5-sonnet-20241022-v2:0".into(),
            system: None,
            messages: vec![
                LlmMessage::user("search rust"),
                LlmMessage {
                    role: "assistant".into(),
                    content: vec![ContentBlock::ToolUse {
                        id: "toolu_1".into(),
                        name: "search".into(),
                        input: serde_json::json!({"q": "rust"}),
                    }],
                },
                LlmMessage {
                    role: "user".into(),
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "toolu_1".into(),
                        content: "no results".into(),
                        is_error: false,
                    }],
                },
            ],
            max_tokens: 1024,
            tools: vec![ToolDef {
                name: "search".into(),
                description: "search the web".into(),
                input_schema: serde_json::json!({"type":"object","properties":{"q":{"type":"string"}}}),
            }],
            thinking_level: None,
        };
        let cv = to_converse_request(&req);
        let json = serde_json::to_value(&cv).unwrap();
        assert_eq!(json["toolConfig"]["tools"][0]["toolSpec"]["name"], "search");
        assert_eq!(
            json["toolConfig"]["tools"][0]["toolSpec"]["inputSchema"]["json"]["type"],
            "object"
        );
        assert_eq!(
            json["messages"][1]["content"][0]["toolUse"]["toolUseId"],
            "toolu_1"
        );
        assert_eq!(
            json["messages"][1]["content"][0]["toolUse"]["name"],
            "search"
        );
        assert_eq!(
            json["messages"][1]["content"][0]["toolUse"]["input"]["q"],
            "rust"
        );
        assert_eq!(
            json["messages"][2]["content"][0]["toolResult"]["toolUseId"],
            "toolu_1"
        );
        assert_eq!(
            json["messages"][2]["content"][0]["toolResult"]["status"],
            "success"
        );
        assert_eq!(
            json["messages"][2]["content"][0]["toolResult"]["content"][0]["text"],
            "no results"
        );
    }

    #[test]
    fn to_converse_request_tool_result_error_status() {
        use crate::types::ContentBlock;
        let req = LlmRequest {
            model: "x".into(),
            system: None,
            messages: vec![LlmMessage {
                role: "user".into(),
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "boom".into(),
                    is_error: true,
                }],
            }],
            max_tokens: 100,
            tools: vec![],
            thinking_level: None,
        };
        let cv = to_converse_request(&req);
        let json = serde_json::to_value(&cv).unwrap();
        assert_eq!(
            json["messages"][0]["content"][0]["toolResult"]["status"],
            "error"
        );
    }

    #[test]
    fn from_converse_response_text_only() {
        use crate::types::ContentBlock;
        let raw = serde_json::json!({
            "output": { "message": { "role": "assistant", "content": [ {"text": "hi there"} ] } },
            "stopReason": "end_turn",
            "usage": { "inputTokens": 10, "outputTokens": 5 }
        });
        let resp = from_converse_response(raw).unwrap();
        assert_eq!(resp.text, "hi there");
        assert_eq!(resp.stop_reason.as_deref(), Some("end_turn"));
        assert_eq!(resp.input_tokens, Some(10));
        assert_eq!(resp.output_tokens, Some(5));
        assert_eq!(resp.content.len(), 1);
        assert!(matches!(&resp.content[0], ContentBlock::Text { text } if text == "hi there"));
    }

    #[test]
    fn from_converse_response_tool_use() {
        use crate::types::ContentBlock;
        let raw = serde_json::json!({
            "output": { "message": { "role": "assistant", "content": [
                {"text": "let me search"},
                {"toolUse": { "toolUseId": "tool_123", "name": "search", "input": {"q": "rust"} }}
            ]}},
            "stopReason": "tool_use",
            "usage": { "inputTokens": 50, "outputTokens": 20 }
        });
        let resp = from_converse_response(raw).unwrap();
        assert_eq!(resp.stop_reason.as_deref(), Some("tool_use"));
        assert_eq!(resp.content.len(), 2);
        assert!(matches!(&resp.content[1],
            ContentBlock::ToolUse { id, name, .. } if id == "tool_123" && name == "search"));
    }

    #[test]
    fn from_converse_response_missing_usage_ok() {
        let raw = serde_json::json!({
            "output": { "message": { "role": "assistant", "content": [{"text": "ok"}] } },
            "stopReason": "end_turn"
        });
        let resp = from_converse_response(raw).unwrap();
        assert_eq!(resp.input_tokens, None);
        assert_eq!(resp.output_tokens, None);
    }

    #[test]
    fn from_converse_response_malformed_errors() {
        let raw = serde_json::json!({ "output": {} });
        assert!(from_converse_response(raw).is_err());
    }
}
