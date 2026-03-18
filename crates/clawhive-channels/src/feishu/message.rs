use anyhow::Result;
use clawhive_schema::{Attachment, AttachmentKind};

pub fn build_approval_card(agent_id: &str, command: &str, short_id: &str) -> serde_json::Value {
    let (cmd_display, network_display) =
        if let Some((cmd, target)) = command.split_once("\nNetwork: ") {
            (cmd.to_string(), Some(target.to_string()))
        } else {
            (command.to_string(), None)
        };

    let md_content = format!(
        "**Agent:** `{agent_id}`\n**Command:** `{cmd_display}`{}",
        network_display
            .as_ref()
            .map(|t| format!("\n**Network:** `{t}`"))
            .unwrap_or_default()
    );

    serde_json::json!({
        "schema": "2.0",
        "header": {
            "title": { "tag": "plain_text", "content": "⚠️ Command Approval Required" },
            "template": "orange"
        },
        "body": {
            "elements": [
                { "tag": "markdown", "content": md_content },
                {
                    "tag": "action",
                    "actions": [
                        {
                            "tag": "button",
                            "text": { "tag": "plain_text", "content": "✅ Allow Once" },
                            "type": "primary",
                            "value": { "action": "approve_allow", "short_id": short_id }
                        },
                        {
                            "tag": "button",
                            "text": { "tag": "plain_text", "content": "🔓 Always Allow" },
                            "type": "default",
                            "value": { "action": "approve_always", "short_id": short_id }
                        },
                        {
                            "tag": "button",
                            "text": { "tag": "plain_text", "content": "❌ Deny" },
                            "type": "danger",
                            "value": { "action": "approve_deny", "short_id": short_id }
                        }
                    ]
                }
            ]
        }
    })
}

pub fn build_skill_confirm_card(skill_name: &str, token: &str) -> serde_json::Value {
    serde_json::json!({
        "schema": "2.0",
        "header": {
            "title": { "tag": "plain_text", "content": "📦 Confirm Skill Installation" },
            "template": "blue"
        },
        "body": {
            "elements": [
                { "tag": "markdown", "content": format!("Install skill **{skill_name}**?") },
                {
                    "tag": "action",
                    "actions": [
                        {
                            "tag": "button",
                            "text": { "tag": "plain_text", "content": format!("✅ Install {skill_name}") },
                            "type": "primary",
                            "value": { "action": "skill_confirm", "token": token }
                        },
                        {
                            "tag": "button",
                            "text": { "tag": "plain_text", "content": "❌ Cancel" },
                            "type": "danger",
                            "value": { "action": "skill_cancel", "token": token }
                        }
                    ]
                }
            ]
        }
    })
}

pub fn md_to_feishu_card(text: &str) -> serde_json::Value {
    serde_json::json!({
        "schema": "2.0",
        "body": {
            "elements": [{ "tag": "markdown", "content": text }]
        }
    })
}

pub fn has_formatting(text: &str) -> bool {
    text.contains("```")
        || text.contains("**")
        || text.contains("~~")
        || text.contains("](")
        || text.contains("- ")
        || text.contains("1. ")
}

/// 28KB conservative limit for card wrapper overhead.
pub const FEISHU_CARD_MAX_BYTES: usize = 28_000;
pub const FEISHU_TEXT_MAX_BYTES: usize = 140_000;

pub fn split_message(text: &str, max_bytes: usize) -> Vec<&str> {
    if text.len() <= max_bytes {
        return vec![text];
    }

    let mut chunks = Vec::new();
    let mut remaining = text;

    while !remaining.is_empty() {
        if remaining.len() <= max_bytes {
            chunks.push(remaining);
            break;
        }

        let boundary = &remaining[..max_bytes];
        let split_at = boundary
            .rfind('\n')
            .or_else(|| boundary.rfind(' '))
            .unwrap_or_else(|| {
                let mut i = max_bytes;
                while i > 0 && !remaining.is_char_boundary(i) {
                    i -= 1;
                }
                i
            });

        if split_at == 0 {
            let safe = {
                let mut i = max_bytes;
                while i > 0 && !remaining.is_char_boundary(i) {
                    i -= 1;
                }
                i
            };
            let (chunk, rest) = remaining.split_at(safe);
            chunks.push(chunk);
            remaining = rest;
        } else {
            let (chunk, rest) = remaining.split_at(split_at);
            let rest = rest.strip_prefix('\n').unwrap_or(rest);
            chunks.push(chunk);
            remaining = rest;
        }
    }

    chunks
}

pub fn empty_outbound_fallback_text(chat_type: &str) -> Option<&'static str> {
    if chat_type == "p2p" {
        Some("Sorry, I got an empty response. Please try again.")
    } else {
        None
    }
}

pub async fn resolve_attachment_bytes(att: &Attachment) -> Result<Vec<u8>> {
    use base64::Engine;
    let url = &att.url;
    if url.starts_with('/') || url.starts_with("./") {
        return tokio::fs::read(url)
            .await
            .map_err(|e| anyhow::anyhow!("read file {url}: {e}"));
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        let resp = reqwest::get(url).await?;
        return Ok(resp.bytes().await?.to_vec());
    }
    base64::engine::general_purpose::STANDARD
        .decode(url)
        .map_err(|e| anyhow::anyhow!("base64 decode: {e}"))
}

pub fn default_file_name(kind: &AttachmentKind, mime_type: &Option<String>) -> String {
    let ext = mime_type
        .as_deref()
        .and_then(|m| m.split('/').nth(1))
        .unwrap_or("bin");
    match kind {
        AttachmentKind::Image => format!("image.{ext}"),
        AttachmentKind::Video => format!("video.{ext}"),
        AttachmentKind::Audio => format!("audio.{ext}"),
        AttachmentKind::Document | AttachmentKind::Other => format!("file.{ext}"),
    }
}

pub async fn send_outbound_attachments(
    client: &super::client::FeishuClient,
    chat_id: &str,
    attachments: &[Attachment],
) {
    for att in attachments {
        let bytes = match resolve_attachment_bytes(att).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    target: "clawhive::channel::feishu",
                    error = %e,
                    "failed to resolve attachment data"
                );
                continue;
            }
        };

        let result = match att.kind {
            AttachmentKind::Image => {
                let file_name = att
                    .file_name
                    .clone()
                    .unwrap_or_else(|| default_file_name(&att.kind, &att.mime_type));
                match client.upload_image(bytes, &file_name).await {
                    Ok(image_key) => {
                        let content = serde_json::json!({"image_key": image_key}).to_string();
                        client.send_message(chat_id, "image", &content).await
                    }
                    Err(e) => Err(e),
                }
            }
            _ => {
                let file_name = att
                    .file_name
                    .clone()
                    .unwrap_or_else(|| default_file_name(&att.kind, &att.mime_type));
                let file_type = match att.kind {
                    AttachmentKind::Video => "mp4",
                    AttachmentKind::Audio => "opus",
                    AttachmentKind::Document => "stream",
                    _ => "stream",
                };
                match client.upload_file(bytes, &file_name, file_type).await {
                    Ok(file_key) => {
                        let content = serde_json::json!({"file_key": file_key}).to_string();
                        client.send_message(chat_id, "file", &content).await
                    }
                    Err(e) => Err(e),
                }
            }
        };

        if let Err(e) = result {
            tracing::error!(
                target: "clawhive::channel::feishu",
                error = %e,
                kind = ?att.kind,
                "failed to send feishu attachment"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_message_short_text_single_chunk() {
        assert_eq!(split_message("hello world", 100), vec!["hello world"]);
    }

    #[test]
    fn split_message_respects_max_bytes() {
        let chunks = split_message("line1\nline2\nline3\nline4", 12);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(chunk.len() <= 12);
        }
    }

    #[test]
    fn split_message_prefers_newline_split() {
        assert_eq!(
            split_message("first line\nsecond line\nthird line", 15)[0],
            "first line"
        );
    }

    #[test]
    fn has_formatting_detects_markdown() {
        assert!(has_formatting("```rust\ncode\n```"));
        assert!(has_formatting("**bold**"));
        assert!(!has_formatting("plain text"));
    }

    #[test]
    fn empty_outbound_fallback_dm_returns_text() {
        assert!(empty_outbound_fallback_text("p2p").is_some());
    }

    #[test]
    fn empty_outbound_fallback_group_returns_none() {
        assert!(empty_outbound_fallback_text("group").is_none());
    }

    #[test]
    fn build_approval_card_structure() {
        let card = build_approval_card("test-agent", "rm -rf /tmp", "abc123");
        let elements = card.pointer("/body/elements").unwrap().as_array().unwrap();
        assert_eq!(elements.len(), 2);
        assert_eq!(
            elements[1]
                .pointer("/actions")
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            3
        );
    }

    #[test]
    fn build_approval_card_with_network() {
        let card = build_approval_card(
            "test-agent",
            "curl api.example.com\nNetwork: api.example.com:443",
            "abc123",
        );
        assert!(card
            .pointer("/body/elements/0/content")
            .unwrap()
            .as_str()
            .unwrap()
            .contains("Network:"));
    }

    #[test]
    fn build_skill_confirm_card_structure() {
        let card = build_skill_confirm_card("weather", "tok_123");
        let elements = card.pointer("/body/elements").unwrap().as_array().unwrap();
        assert_eq!(elements.len(), 2);
        assert_eq!(
            elements[1]
                .pointer("/actions")
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn md_to_feishu_card_structure() {
        let card = md_to_feishu_card("**hello** world");
        assert_eq!(
            card.pointer("/body/elements/0/tag")
                .unwrap()
                .as_str()
                .unwrap(),
            "markdown"
        );
    }
}
