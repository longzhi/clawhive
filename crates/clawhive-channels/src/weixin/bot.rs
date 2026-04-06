use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use chrono::Utc;
use clawhive_bus::{EventBus, Topic};
use clawhive_gateway::Gateway;
use clawhive_schema::{
    ApprovalDisplay, Attachment, AttachmentKind, BusMessage, InboundMessage, OutboundMessage,
};
use uuid::Uuid;

use super::api::ILinkClient;
use super::types::WeixinSession;
use crate::common::AbortOnDrop;

const MAX_TEXT_LEN: usize = 4000;
const PROGRESS_MESSAGE: &str = "⏳ Still working on it... (send /stop to cancel)";

pub struct WeixinBot {
    connector_id: String,
    gateway: Arc<Gateway>,
    bus: Arc<EventBus>,
    session: WeixinSession,
    data_dir: PathBuf,
}

impl WeixinBot {
    pub fn new(
        connector_id: String,
        gateway: Arc<Gateway>,
        bus: Arc<EventBus>,
        session: WeixinSession,
        data_dir: PathBuf,
    ) -> Self {
        Self {
            connector_id,
            gateway,
            bus,
            session,
            data_dir,
        }
    }

    async fn run_impl(self) -> Result<()> {
        let client = Arc::new(ILinkClient::new(self.session, &self.data_dir));
        let mut cursor = client.load_cursor();

        tracing::info!(
            target: "clawhive::channel::weixin",
            connector_id = %self.connector_id,
            "weixin iLink bot starting long-poll loop"
        );

        // Spawn outbound delivery listener
        spawn_delivery_listener(self.bus.clone(), client.clone(), self.connector_id.clone());
        spawn_approval_listener(self.bus.clone(), client.clone(), self.connector_id.clone());

        let mut consecutive_errors: u32 = 0;

        loop {
            match client.get_updates(&cursor).await {
                Ok(resp) => {
                    consecutive_errors = 0;

                    // errcode=-14 means "session paused / rate limited"
                    if resp.errcode == -14 {
                        tracing::warn!(
                            target: "clawhive::channel::weixin",
                            "errcode=-14, pausing for 1 hour"
                        );
                        tokio::time::sleep(Duration::from_secs(3600)).await;
                        continue;
                    }

                    if resp.ret != 0 {
                        tracing::warn!(
                            target: "clawhive::channel::weixin",
                            ret = resp.ret,
                            errcode = resp.errcode,
                            errmsg = %resp.errmsg,
                            "getupdates non-zero ret"
                        );
                    }

                    // Update cursor
                    if !resp.get_updates_buf.is_empty() {
                        cursor = resp.get_updates_buf.clone();
                        client.save_cursor(&cursor);
                    }

                    for msg in &resp.msgs {
                        // Skip bot messages (message_type == 2)
                        if msg.message_type == 2 {
                            continue;
                        }
                        // Skip non-finished messages (message_state != 2)
                        if msg.message_state != 2 {
                            continue;
                        }

                        // Cache context_token for this user
                        if !msg.context_token.is_empty() {
                            client
                                .set_context_token(&msg.from_user_id, &msg.context_token)
                                .await;
                        }

                        // Extract text from items
                        let text = extract_text(&msg.item_list);

                        // Download images and build attachments
                        let mut attachments = Vec::new();
                        for item in &msg.item_list {
                            if item.item_type == 2 {
                                if let Some(image_item) = &item.image_item {
                                    match client.download_image(image_item).await {
                                        Ok(bytes) => {
                                            let b64 = base64::engine::general_purpose::STANDARD
                                                .encode(&bytes);
                                            let mime = detect_image_mime(&bytes);
                                            attachments.push(Attachment {
                                                kind: AttachmentKind::Image,
                                                url: b64,
                                                mime_type: Some(mime.to_string()),
                                                file_name: None,
                                                size: Some(bytes.len() as u64),
                                            });
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                target: "clawhive::channel::weixin",
                                                error = %e,
                                                "failed to download inbound image"
                                            );
                                        }
                                    }
                                }
                            }
                        }

                        let inbound = InboundMessage {
                            trace_id: Uuid::new_v4(),
                            channel_type: "weixin".to_string(),
                            connector_id: self.connector_id.clone(),
                            conversation_scope: format!("dm:{}", msg.from_user_id),
                            user_scope: format!("user:{}", msg.from_user_id),
                            text,
                            at: Utc::now(),
                            thread_id: None,
                            is_mention: true, // DM always counts as mention
                            mention_target: None,
                            message_id: Some(msg.message_id.to_string()),
                            attachments,
                            message_source: None,
                        };

                        let gw = self.gateway.clone();
                        let client_clone = client.clone();
                        let from_user = msg.from_user_id.clone();
                        let ctx_token = msg.context_token.clone();

                        tokio::spawn(async move {
                            // Send typing indicator
                            if !ctx_token.is_empty() {
                                let _ = client_clone.send_typing(&from_user, &ctx_token).await;
                            }

                            let turn_complete = Arc::new(tokio::sync::Notify::new());
                            let progress_complete = turn_complete.clone();
                            let progress_client = client_clone.clone();
                            let progress_user = from_user.clone();
                            let progress_ctx = ctx_token.clone();
                            let _progress_guard = AbortOnDrop(tokio::spawn(async move {
                                tokio::select! {
                                    _ = tokio::time::sleep(Duration::from_secs(60)) => {
                                        if !progress_ctx.is_empty() {
                                            if let Err(e) = progress_client
                                                .send_text(&progress_user, PROGRESS_MESSAGE, &progress_ctx)
                                                .await
                                            {
                                                tracing::warn!(
                                                    target: "clawhive::channel::weixin",
                                                    error = %e,
                                                    "failed to send weixin progress message"
                                                );
                                            }
                                        }
                                    }
                                    _ = progress_complete.notified() => {}
                                }
                            }));

                            let result = gw.handle_inbound(inbound).await;
                            turn_complete.notify_waiters();

                            match result {
                                Ok(Some(outbound)) => {
                                    if let Err(e) = send_outbound_reply(
                                        &client_clone,
                                        &from_user,
                                        &ctx_token,
                                        &outbound,
                                    )
                                    .await
                                    {
                                        tracing::error!(
                                            target: "clawhive::channel::weixin",
                                            error = %e,
                                            "failed to send outbound reply"
                                        );
                                    }
                                }
                                Ok(None) => {}
                                Err(e) => {
                                    tracing::error!(
                                        target: "clawhive::channel::weixin",
                                        error = %e,
                                        "failed to handle inbound message"
                                    );
                                }
                            }
                        });
                    }
                }
                Err(e) => {
                    consecutive_errors += 1;
                    let backoff = Duration::from_secs(
                        (2u64.saturating_pow(consecutive_errors.min(6))).min(120),
                    );
                    tracing::error!(
                        target: "clawhive::channel::weixin",
                        error = %e,
                        consecutive = consecutive_errors,
                        backoff_secs = backoff.as_secs(),
                        "getupdates failed, backing off"
                    );
                    tokio::time::sleep(backoff).await;
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl crate::ChannelBot for WeixinBot {
    fn channel_type(&self) -> &str {
        "weixin"
    }

    fn connector_id(&self) -> &str {
        &self.connector_id
    }

    async fn run(self: Box<Self>) -> Result<()> {
        (*self).run_impl().await
    }
}

// ---------------------------------------------------------------------------
// Outbound
// ---------------------------------------------------------------------------

/// Send outbound reply: text (split if needed) + image attachments.
async fn send_outbound_reply(
    client: &ILinkClient,
    to_user_id: &str,
    context_token: &str,
    outbound: &OutboundMessage,
) -> Result<()> {
    // Send text, splitting at MAX_TEXT_LEN if needed
    let text = outbound.text.trim();
    if !text.is_empty() {
        for chunk in split_text(text, MAX_TEXT_LEN) {
            client
                .send_text(to_user_id, chunk, context_token)
                .await
                .context("send_text failed")?;
        }
    }

    // Send image attachments
    for att in &outbound.attachments {
        if att.kind == AttachmentKind::Image {
            match resolve_attachment_bytes(att).await {
                Ok(bytes) => {
                    if let Err(e) = client.send_image(to_user_id, &bytes, context_token).await {
                        tracing::warn!(
                            target: "clawhive::channel::weixin",
                            error = %e,
                            "failed to send image attachment"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        target: "clawhive::channel::weixin",
                        error = %e,
                        "failed to resolve attachment bytes"
                    );
                }
            }
        }
    }

    Ok(())
}

/// Spawn a listener for `DeliverAnnounce` bus messages.
fn spawn_delivery_listener(bus: Arc<EventBus>, client: Arc<ILinkClient>, connector_id: String) {
    tokio::spawn(async move {
        let mut rx = bus.subscribe(Topic::DeliverAnnounce).await;
        while let Some(msg) = rx.recv().await {
            let BusMessage::DeliverAnnounce {
                channel_type,
                connector_id: msg_connector_id,
                conversation_scope,
                text,
            } = msg
            else {
                continue;
            };

            if channel_type != "weixin" || msg_connector_id != connector_id {
                continue;
            }

            // Parse user_id from "dm:{user_id}"
            let user_id = match conversation_scope.strip_prefix("dm:") {
                Some(id) => id.to_string(),
                None => {
                    tracing::warn!(
                        target: "clawhive::channel::weixin",
                        conversation_scope = %conversation_scope,
                        "cannot parse user_id from conversation_scope"
                    );
                    continue;
                }
            };

            let ctx_token = client.get_context_token(&user_id).await.unwrap_or_default();
            if ctx_token.is_empty() {
                tracing::warn!(
                    target: "clawhive::channel::weixin",
                    user_id = %user_id,
                    "no context_token cached, cannot deliver announce"
                );
                continue;
            }

            for chunk in split_text(text.trim(), MAX_TEXT_LEN) {
                if let Err(e) = client.send_text(&user_id, chunk, &ctx_token).await {
                    tracing::error!(
                        target: "clawhive::channel::weixin",
                        error = %e,
                        "failed to deliver announce to weixin"
                    );
                }
            }
        }
    });
}

fn spawn_approval_listener(bus: Arc<EventBus>, client: Arc<ILinkClient>, connector_id: String) {
    tokio::spawn(async move {
        let mut rx = bus.subscribe(Topic::DeliverApprovalRequest).await;
        while let Some(msg) = rx.recv().await {
            let BusMessage::DeliverApprovalRequest {
                channel_type,
                connector_id: msg_connector_id,
                conversation_scope,
                short_id,
                agent_id,
                command,
                network_target,
                summary,
            } = msg
            else {
                continue;
            };

            if channel_type != "weixin" || msg_connector_id != connector_id {
                continue;
            }

            let user_id = match conversation_scope.strip_prefix("dm:") {
                Some(id) => id.to_string(),
                None => {
                    tracing::warn!(
                        target: "clawhive::channel::weixin",
                        conversation_scope = %conversation_scope,
                        "cannot parse user_id for approval delivery"
                    );
                    continue;
                }
            };

            let ctx_token = client.get_context_token(&user_id).await.unwrap_or_default();
            if ctx_token.is_empty() {
                tracing::warn!(
                    target: "clawhive::channel::weixin",
                    user_id = %user_id,
                    "no context_token cached, cannot deliver approval"
                );
                continue;
            }

            let display =
                ApprovalDisplay::new(&agent_id, &command, network_target.as_deref(), summary);
            let text = format!(
                "{}\n\nReply:\n✅ yes {short_id}\n🔓 always {short_id}\n❌ no {short_id}",
                display.to_markdown()
            );

            for chunk in split_text(text.trim(), MAX_TEXT_LEN) {
                if let Err(e) = client.send_text(&user_id, chunk, &ctx_token).await {
                    tracing::error!(
                        target: "clawhive::channel::weixin",
                        error = %e,
                        "failed to deliver approval to weixin"
                    );
                }
            }

            tracing::info!(
                target: "clawhive::channel::weixin",
                short_id,
                connector_id,
                "weixin approval message sent"
            );
        }
    });
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Extract text from message item list.
fn extract_text(items: &[super::types::MessageItem]) -> String {
    let mut parts = Vec::new();
    for item in items {
        if item.item_type == 1 {
            if let Some(text_item) = &item.text_item {
                parts.push(text_item.text.clone());
            }
        }
    }
    parts.join("\n")
}

/// Split text into chunks of at most `max_len` bytes.
fn split_text(text: &str, max_len: usize) -> Vec<&str> {
    if text.len() <= max_len {
        return vec![text];
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let end = if start + max_len >= text.len() {
            text.len()
        } else {
            // Find a safe char boundary
            let mut boundary = start + max_len;
            while boundary > start && !text.is_char_boundary(boundary) {
                boundary -= 1;
            }
            if boundary == start {
                // Fallback: advance to next char boundary
                boundary = start + max_len;
                while boundary < text.len() && !text.is_char_boundary(boundary) {
                    boundary += 1;
                }
            }
            boundary
        };
        chunks.push(&text[start..end]);
        start = end;
    }
    chunks
}

/// Detect image MIME type from magic bytes.
fn detect_image_mime(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47]) {
        "image/png"
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "image/jpeg"
    } else if bytes.starts_with(b"GIF8") {
        "image/gif"
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        "image/webp"
    } else {
        "image/jpeg"
    }
}

/// Resolve attachment bytes from various sources.
async fn resolve_attachment_bytes(att: &Attachment) -> Result<Vec<u8>> {
    let url = &att.url;
    if url.starts_with('/') || url.starts_with("./") {
        return tokio::fs::read(url)
            .await
            .map_err(|e| anyhow!("read file {url}: {e}"));
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        let resp = reqwest::get(url).await?;
        return Ok(resp.bytes().await?.to_vec());
    }
    base64::engine::general_purpose::STANDARD
        .decode(url)
        .map_err(|e| anyhow!("base64 decode: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_text_short() {
        let chunks = split_text("hello", 100);
        assert_eq!(chunks, vec!["hello"]);
    }

    #[test]
    fn split_text_exact() {
        let text = "abcde";
        let chunks = split_text(text, 5);
        assert_eq!(chunks, vec!["abcde"]);
    }

    #[test]
    fn split_text_long() {
        let text = "abcdefghij";
        let chunks = split_text(text, 4);
        assert_eq!(chunks, vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn split_text_multibyte() {
        // Chinese chars are 3 bytes each in UTF-8
        let text = "你好世界"; // 12 bytes
        let chunks = split_text(text, 6);
        assert_eq!(chunks, vec!["你好", "世界"]);
    }

    #[test]
    fn detect_png() {
        let data = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        assert_eq!(detect_image_mime(&data), "image/png");
    }

    #[test]
    fn detect_jpeg() {
        let data = [0xFF, 0xD8, 0xFF, 0xE0];
        assert_eq!(detect_image_mime(&data), "image/jpeg");
    }

    #[test]
    fn extract_text_from_items() {
        use super::super::types::{MessageItem, TextItem};
        let items = vec![
            MessageItem {
                item_type: 1,
                text_item: Some(TextItem {
                    text: "hello".to_string(),
                }),
                image_item: None,
            },
            MessageItem {
                item_type: 2,
                text_item: None,
                image_item: None,
            },
            MessageItem {
                item_type: 1,
                text_item: Some(TextItem {
                    text: "world".to_string(),
                }),
                image_item: None,
            },
        ];
        assert_eq!(extract_text(&items), "hello\nworld");
    }
}
