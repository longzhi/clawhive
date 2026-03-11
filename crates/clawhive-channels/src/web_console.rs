//! Web console channel adapter — protocol types, message mapping, and conversation helpers.
//!
//! This module contains the shared types and pure functions used by the web console chat
//! interface. The axum HTTP/WebSocket handlers live in `clawhive-server`; this module provides
//! the channel-level abstractions they depend on.

use std::path::{Path, PathBuf};

use clawhive_schema::{Attachment, AttachmentKind};
use serde::{Deserialize, Serialize};

// ── Protocol types ──────────────────────────────────────────────────────────

/// Attachment payload sent by the client (browser) via WebSocket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentPayload {
    pub kind: String,
    pub data: String,
    #[serde(default)]
    pub mime_type: Option<String>,
    #[serde(default)]
    pub file_name: Option<String>,
}

/// Messages sent by the client (browser) to the server via WebSocket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    SendMessage {
        text: String,
        agent_id: String,
        conversation_id: Option<String>,
        #[serde(default)]
        attachments: Vec<AttachmentPayload>,
    },
    Cancel,
    Ping,
}

/// Messages sent by the server to the client (browser) via WebSocket.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    StreamDelta {
        trace_id: String,
        delta: String,
        is_final: bool,
    },
    ToolCallStarted {
        trace_id: String,
        tool_name: String,
        arguments: String,
    },
    ToolCallCompleted {
        trace_id: String,
        tool_name: String,
        output: String,
        duration_ms: u64,
    },
    ReplyReady {
        trace_id: String,
        text: String,
    },
    Error {
        trace_id: Option<String>,
        message: String,
    },
    Pong,
}

// ── REST types ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct ConversationSummary {
    pub conversation_id: String,
    pub agent_id: String,
    pub last_message_at: Option<String>,
    pub message_count: usize,
    pub preview: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateConversationRequest {
    pub agent_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreateConversationResponse {
    pub conversation_id: String,
    pub agent_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolCallInfo {
    pub tool_name: String,
    pub arguments: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    pub is_running: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConversationMessage {
    pub role: String,
    pub text: String,
    pub timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallInfo>>,
}

// ── Constants ───────────────────────────────────────────────────────────────

pub const MAX_ATTACHMENTS_PER_MESSAGE: usize = 5;
pub const MAX_ATTACHMENT_DATA_BYTES: usize = 10 * 1024 * 1024;

// ── Message mapping ─────────────────────────────────────────────────────────

pub fn map_attachment(payload: &AttachmentPayload) -> Attachment {
    let kind = match payload.kind.as_str() {
        "image" => AttachmentKind::Image,
        "video" => AttachmentKind::Video,
        "audio" => AttachmentKind::Audio,
        "document" => AttachmentKind::Document,
        _ => AttachmentKind::Other,
    };
    Attachment {
        kind,
        url: payload.data.clone(),
        mime_type: payload.mime_type.clone(),
        file_name: payload.file_name.clone(),
        size: None,
    }
}

pub fn validate_attachments(attachments: &[AttachmentPayload]) -> Result<(), String> {
    if attachments.len() > MAX_ATTACHMENTS_PER_MESSAGE {
        return Err(format!(
            "Too many attachments: max {} per message",
            MAX_ATTACHMENTS_PER_MESSAGE
        ));
    }

    for (idx, attachment) in attachments.iter().enumerate() {
        if attachment.data.len() > MAX_ATTACHMENT_DATA_BYTES {
            return Err(format!(
                "Attachment {} exceeds {} bytes",
                idx + 1,
                MAX_ATTACHMENT_DATA_BYTES
            ));
        }
    }

    Ok(())
}

// ── Conversation helpers ────────────────────────────────────────────────────

pub fn token_prefix(token: &str) -> String {
    token.chars().take(8).collect()
}

pub fn workspace_sessions_dirs(root: &Path) -> Vec<(String, PathBuf)> {
    let mut dirs = Vec::new();
    let Ok(entries) = std::fs::read_dir(root.join("workspaces")) else {
        return dirs;
    };

    for entry in entries.flatten() {
        let agent_dir = entry.path();
        if !agent_dir.is_dir() {
            continue;
        }
        let Some(agent_id) = agent_dir.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        dirs.push((agent_id.to_string(), agent_dir.join("sessions")));
    }

    dirs
}

pub fn session_key_from_path(path: &Path) -> Option<String> {
    path.file_stem()
        .and_then(|s| s.to_str())
        .map(ToOwned::to_owned)
}

pub fn is_web_console_user_session(session_key: &str, user_token_prefix: &str) -> bool {
    session_key.contains("web_console")
        && session_key.contains(&format!("user:web_{user_token_prefix}"))
}

pub fn conversation_id_from_session_key(session_key: &str) -> Option<String> {
    let parts: Vec<&str> = session_key.split(':').collect();
    for idx in 0..parts.len() {
        if parts[idx] == "chat" {
            return parts.get(idx + 1).map(|value| (*value).to_string());
        }
    }
    None
}

pub fn summarize_session_content(content: &str) -> (usize, Option<String>, Option<String>) {
    let mut message_count = 0usize;
    let mut last_message_at: Option<String> = None;
    let mut preview: Option<String> = None;

    for line in content.lines() {
        let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if val["type"].as_str() != Some("message") {
            continue;
        }

        message_count += 1;
        last_message_at = val["timestamp"].as_str().map(ToOwned::to_owned);
        preview = extract_message_text(&val["message"]).or(preview);
    }

    (message_count, last_message_at, preview)
}

pub fn find_conversation_session_file(
    root: &Path,
    conversation_id: &str,
    user_prefix: &str,
) -> Option<PathBuf> {
    for (_, sessions_dir) in workspace_sessions_dirs(root) {
        let Ok(entries) = std::fs::read_dir(sessions_dir) else {
            continue;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }

            let Some(session_key) = session_key_from_path(&path) else {
                continue;
            };

            if !is_web_console_user_session(&session_key, user_prefix) {
                continue;
            }

            if conversation_id_from_session_key(&session_key).as_deref() == Some(conversation_id) {
                return Some(path);
            }
        }
    }

    None
}

pub fn parse_conversation_messages(content: &str) -> Vec<ConversationMessage> {
    let mut messages = Vec::new();

    for line in content.lines() {
        let Ok(val) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };

        match val["type"].as_str() {
            Some("message") => {
                let message_obj = &val["message"];
                let role = message_obj["role"]
                    .as_str()
                    .unwrap_or("assistant")
                    .to_string();
                let text = extract_message_text(message_obj).unwrap_or_default();
                let timestamp = val["timestamp"].as_str().unwrap_or_default().to_string();

                messages.push(ConversationMessage {
                    role,
                    text,
                    timestamp,
                    tool_calls: None,
                });
            }
            Some("tool_call") => {
                if let Some(last_assistant) =
                    messages.iter_mut().rev().find(|m| m.role == "assistant")
                {
                    let tool_name = val["tool"].as_str().unwrap_or_default().to_string();
                    let arguments =
                        serde_json::to_string(&val["input"]).unwrap_or_else(|_| "{}".to_string());
                    let tool_calls = last_assistant.tool_calls.get_or_insert_with(Vec::new);
                    tool_calls.push(ToolCallInfo {
                        tool_name,
                        arguments,
                        output: None,
                        duration_ms: None,
                        is_running: true,
                    });
                }
            }
            Some("tool_result") => {
                if let Some(last_assistant) =
                    messages.iter_mut().rev().find(|m| m.role == "assistant")
                {
                    let tool_name = val["tool"].as_str().unwrap_or_default();
                    if let Some(tool_calls) = &mut last_assistant.tool_calls {
                        for call in tool_calls.iter_mut().rev() {
                            if call.tool_name == tool_name && call.is_running {
                                call.output = Some(
                                    serde_json::to_string(&val["output"])
                                        .unwrap_or_else(|_| "{}".to_string()),
                                );
                                call.is_running = false;
                                break;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    messages
}

fn extract_message_text(message_obj: &serde_json::Value) -> Option<String> {
    match &message_obj["content"] {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Array(parts) => {
            let text = parts
                .iter()
                .filter_map(|part| part["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            if text.is_empty() {
                None
            } else {
                Some(text)
            }
        }
        _ => None,
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversation_id_from_session_key_extracts_id() {
        assert_eq!(
            conversation_id_from_session_key(
                "web_console:web_main:chat:abc-123-def:user:web_abcd1234"
            ),
            Some("abc-123-def".to_string())
        );
    }

    #[test]
    fn conversation_id_from_session_key_returns_none_for_non_chat() {
        assert_eq!(
            conversation_id_from_session_key("telegram:bot123:group:456:user:user789"),
            None
        );
    }

    #[test]
    fn is_web_console_user_session_matches_correctly() {
        let key = "web_console:web_main:chat:uuid123:user:web_abcd1234";
        assert!(is_web_console_user_session(key, "abcd1234"));
        assert!(!is_web_console_user_session(key, "other123"));
    }

    #[test]
    fn token_prefix_returns_first_8_chars() {
        assert_eq!(token_prefix("abcdefghijklmnop"), "abcdefgh");
        assert_eq!(token_prefix("short"), "short");
    }

    #[test]
    fn summarize_session_content_counts_messages() {
        let content = concat!(
            r#"{"type":"message","timestamp":"2024-01-01T00:00:00Z","message":{"role":"user","content":"hello"}}"#,
            "\n",
            r#"{"type":"message","timestamp":"2024-01-01T00:01:00Z","message":{"role":"assistant","content":"hi there"}}"#,
            "\n",
            r#"{"type":"tool_call","tool":"search","input":{}}"#,
        );
        let (count, last_at, preview) = summarize_session_content(content);
        assert_eq!(count, 2);
        assert_eq!(last_at.as_deref(), Some("2024-01-01T00:01:00Z"));
        assert_eq!(preview.as_deref(), Some("hi there"));
    }

    #[test]
    fn parse_conversation_messages_handles_tool_calls() {
        let content = concat!(
            r#"{"type":"message","timestamp":"2024-01-01T00:00:00Z","message":{"role":"user","content":"search"}}"#,
            "\n",
            r#"{"type":"message","timestamp":"2024-01-01T00:01:00Z","message":{"role":"assistant","content":"Searching..."}}"#,
            "\n",
            r#"{"type":"tool_call","tool":"web_search","input":{"q":"rust"}}"#,
            "\n",
            r#"{"type":"tool_result","tool":"web_search","output":{"results":[]}}"#,
        );
        let messages = parse_conversation_messages(content);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[1].role, "assistant");

        let tool_calls = messages[1].tool_calls.as_ref().unwrap();
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].tool_name, "web_search");
        assert!(!tool_calls[0].is_running);
        assert!(tool_calls[0].output.is_some());
    }

    #[test]
    fn map_attachment_maps_image_kind() {
        let payload = AttachmentPayload {
            kind: "image".to_string(),
            data: "base64-image".to_string(),
            mime_type: Some("image/png".to_string()),
            file_name: Some("pic.png".to_string()),
        };

        let attachment = map_attachment(&payload);
        assert!(matches!(attachment.kind, AttachmentKind::Image));
        assert_eq!(attachment.url, "base64-image");
        assert_eq!(attachment.mime_type.as_deref(), Some("image/png"));
        assert_eq!(attachment.file_name.as_deref(), Some("pic.png"));
    }

    #[test]
    fn map_attachment_unknown_kind_falls_back_to_other() {
        let payload = AttachmentPayload {
            kind: "unknown".to_string(),
            data: "base64-data".to_string(),
            mime_type: None,
            file_name: None,
        };

        let attachment = map_attachment(&payload);
        assert!(matches!(attachment.kind, AttachmentKind::Other));
    }

    #[test]
    fn send_message_serde_without_attachments_defaults_to_empty() {
        let msg = serde_json::from_str::<ClientMessage>(
            r#"{"type":"send_message","text":"hi","agent_id":"agent-1","conversation_id":null}"#,
        )
        .unwrap();

        match msg {
            ClientMessage::SendMessage { attachments, .. } => {
                assert!(attachments.is_empty());
            }
            _ => panic!("expected send_message variant"),
        }
    }

    #[test]
    fn send_message_serde_with_single_attachment() {
        let msg = serde_json::from_str::<ClientMessage>(
            r#"{"type":"send_message","text":"hi","agent_id":"agent-1","conversation_id":"conv-1","attachments":[{"kind":"image","data":"base64-image","mime_type":"image/png","file_name":"pic.png"}]}"#,
        )
        .unwrap();

        match msg {
            ClientMessage::SendMessage { attachments, .. } => {
                assert_eq!(attachments.len(), 1);
                assert_eq!(attachments[0].kind, "image");
                assert_eq!(attachments[0].data, "base64-image");
                assert_eq!(attachments[0].mime_type.as_deref(), Some("image/png"));
                assert_eq!(attachments[0].file_name.as_deref(), Some("pic.png"));
            }
            _ => panic!("expected send_message variant"),
        }
    }

    #[test]
    fn validate_attachments_rejects_too_many_or_too_large() {
        let too_many = vec![
            AttachmentPayload {
                kind: "image".to_string(),
                data: "x".to_string(),
                mime_type: None,
                file_name: None,
            };
            6
        ];
        let too_large = vec![AttachmentPayload {
            kind: "image".to_string(),
            data: "x".repeat(10 * 1024 * 1024 + 1),
            mime_type: None,
            file_name: None,
        }];
        let valid = vec![AttachmentPayload {
            kind: "image".to_string(),
            data: "x".repeat(10 * 1024 * 1024),
            mime_type: None,
            file_name: None,
        }];

        assert!(validate_attachments(&too_many).is_err());
        assert!(validate_attachments(&too_large).is_err());
        assert!(validate_attachments(&valid).is_ok());
    }
}
