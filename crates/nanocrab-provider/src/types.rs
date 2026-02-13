use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmMessage {
    pub role: String,
    pub content: Vec<ContentBlock>,
}

impl LlmMessage {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn tool_uses(&self) -> Vec<(&str, &str, &serde_json::Value)> {
        self.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, name, input } => {
                    Some((id.as_str(), name.as_str(), input))
                }
                _ => None,
            })
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmRequest {
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<LlmMessage>,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDef>,
}

fn default_max_tokens() -> u32 {
    2048
}

impl LlmRequest {
    pub fn simple(model: String, system: Option<String>, user: String) -> Self {
        Self {
            model,
            system,
            messages: vec![LlmMessage::user(user)],
            max_tokens: default_max_tokens(),
            tools: vec![],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmResponse {
    pub text: String,
    pub content: Vec<ContentBlock>,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub stop_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamChunk {
    pub delta: String,
    pub is_final: bool,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub stop_reason: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_block_text_serde() {
        let block = ContentBlock::Text {
            text: "hello".into(),
        };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "text");
        assert_eq!(json["text"], "hello");
        let roundtrip: ContentBlock = serde_json::from_value(json).unwrap();
        assert!(matches!(roundtrip, ContentBlock::Text { text } if text == "hello"));
    }

    #[test]
    fn content_block_tool_use_serde() {
        let block = ContentBlock::ToolUse {
            id: "toolu_123".into(),
            name: "memory_search".into(),
            input: serde_json::json!({"query": "rust"}),
        };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "tool_use");
        assert_eq!(json["id"], "toolu_123");
        assert_eq!(json["name"], "memory_search");
        let roundtrip: ContentBlock = serde_json::from_value(json).unwrap();
        assert!(matches!(roundtrip, ContentBlock::ToolUse { name, .. } if name == "memory_search"));
    }

    #[test]
    fn content_block_tool_result_serde() {
        let block = ContentBlock::ToolResult {
            tool_use_id: "toolu_123".into(),
            content: "search results here".into(),
            is_error: false,
        };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "tool_result");
        assert_eq!(json["tool_use_id"], "toolu_123");
        let roundtrip: ContentBlock = serde_json::from_value(json).unwrap();
        assert!(
            matches!(roundtrip, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "toolu_123")
        );
    }

    #[test]
    fn llm_message_text_helper() {
        let msg = LlmMessage::user("hello");
        assert_eq!(msg.role, "user");
        assert_eq!(msg.text(), "hello");
    }

    #[test]
    fn llm_message_with_tool_use() {
        let msg = LlmMessage {
            role: "assistant".into(),
            content: vec![
                ContentBlock::Text {
                    text: "Let me search...".into(),
                },
                ContentBlock::ToolUse {
                    id: "toolu_1".into(),
                    name: "memory_search".into(),
                    input: serde_json::json!({"query": "test"}),
                },
            ],
        };
        assert_eq!(msg.text(), "Let me search...");
        assert!(msg.tool_uses().len() == 1);
    }

    #[test]
    fn tool_def_serde() {
        let tool = ToolDef {
            name: "memory_search".into(),
            description: "Search memory".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Search query"}
                },
                "required": ["query"]
            }),
        };
        let json = serde_json::to_value(&tool).unwrap();
        assert_eq!(json["name"], "memory_search");
        assert!(json["input_schema"]["properties"]["query"].is_object());
    }

    #[test]
    fn llm_message_simple_constructor() {
        let msg = LlmMessage::user("test");
        assert_eq!(msg.content.len(), 1);
        assert!(matches!(&msg.content[0], ContentBlock::Text { text } if text == "test"));
    }

    #[test]
    fn llm_message_assistant_constructor() {
        let msg = LlmMessage::assistant("reply");
        assert_eq!(msg.role, "assistant");
        assert_eq!(msg.text(), "reply");
    }

    #[test]
    fn llm_request_simple_still_works() {
        let req = LlmRequest::simple("model".into(), None, "hello".into());
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].text(), "hello");
    }
}
