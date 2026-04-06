use std::path::PathBuf;
use std::sync::Arc;

use base64::prelude::*;
use chrono::Utc;
use clawhive_bus::{EventBus, Topic};
use clawhive_gateway::Gateway;
use clawhive_schema::{
    ActionKind, ApprovalDisplay, Attachment, AttachmentKind, BusMessage, InboundMessage,
    OutboundMessage,
};
use uuid::Uuid;
use wacore::download::MediaType;
use wacore::types::events::Event;
use waproto::whatsapp as wa;
use waproto::whatsapp::message::{DocumentMessage, ImageMessage};
use whatsapp_rust::bot::Bot;
use whatsapp_rust::client::Client;
use whatsapp_rust::upload::{UploadOptions, UploadResponse};
use whatsapp_rust::{Jid, TokioRuntime};
use whatsapp_rust_sqlite_storage::SqliteStore;
use whatsapp_rust_tokio_transport::TokioWebSocketTransportFactory;
use whatsapp_rust_ureq_http_client::UreqHttpClient;

use crate::common::AbortOnDrop;

const PROGRESS_MESSAGE: &str = "⏳ Still working on it... (send /stop to cancel)";

pub struct WhatsAppAdapter {
    connector_id: String,
}

impl WhatsAppAdapter {
    pub fn new(connector_id: impl Into<String>) -> Self {
        Self {
            connector_id: connector_id.into(),
        }
    }

    pub fn to_inbound(
        &self,
        chat_jid: &str,
        sender_jid: &str,
        text: &str,
        message_id: Option<String>,
    ) -> InboundMessage {
        InboundMessage {
            trace_id: Uuid::new_v4(),
            channel_type: "whatsapp".to_string(),
            connector_id: self.connector_id.clone(),
            conversation_scope: format!("chat:{chat_jid}"),
            user_scope: format!("user:{sender_jid}"),
            text: text.to_string(),
            at: Utc::now(),
            thread_id: None,
            is_mention: false,
            mention_target: None,
            message_id,
            attachments: vec![],
            message_source: None,
        }
    }

    pub fn render_outbound(&self, outbound: &OutboundMessage) -> String {
        format!(
            "[whatsapp:{}] {}",
            outbound.conversation_scope, outbound.text
        )
    }
}

pub enum PairStatus {
    QrCode(String, std::time::Duration),
    AlreadyPaired,
    Paired,
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct AccessPolicy {
    pub dm_policy: String,
    pub allow_from: Vec<String>,
    pub group_policy: String,
    pub group_allow_from: Vec<String>,
}

impl AccessPolicy {
    pub fn from_config(
        dm_policy: &str,
        allow_from: &[String],
        group_policy: &str,
        group_allow_from: &[String],
    ) -> Self {
        Self {
            dm_policy: dm_policy.to_string(),
            allow_from: allow_from
                .iter()
                .map(|phone| normalize_phone(phone))
                .collect(),
            group_policy: group_policy.to_string(),
            group_allow_from: group_allow_from
                .iter()
                .map(|phone| normalize_phone(phone))
                .collect(),
        }
    }

    fn is_allowed_dm(&self, sender_jid: &str) -> bool {
        match self.dm_policy.as_str() {
            "open" => true,
            "disabled" => false,
            _ => {
                let sender_number = extract_number_from_jid(sender_jid);
                self.allow_from
                    .iter()
                    .any(|number| number == &sender_number)
            }
        }
    }

    fn is_allowed_group(&self, sender_jid: &str) -> bool {
        match self.group_policy.as_str() {
            "open" => true,
            "disabled" => false,
            _ => {
                let sender_number = extract_number_from_jid(sender_jid);
                let allowlist = if self.group_allow_from.is_empty() {
                    &self.allow_from
                } else {
                    &self.group_allow_from
                };
                allowlist.iter().any(|number| number == &sender_number)
            }
        }
    }
}

pub async fn run_pairing(
    db_path: PathBuf,
    tx: tokio::sync::mpsc::Sender<PairStatus>,
) -> anyhow::Result<()> {
    let db_str = db_path.to_string_lossy().to_string();
    let backend = Arc::new(SqliteStore::new(&db_str).await?);

    let tx_event = tx.clone();
    let mut bot = Bot::builder()
        .with_backend(backend)
        .with_transport_factory(TokioWebSocketTransportFactory::new())
        .with_http_client(UreqHttpClient::new())
        .with_runtime(TokioRuntime)
        .skip_history_sync()
        .on_event(move |event, _client| {
            let tx = tx_event.clone();
            async move {
                match event {
                    Event::PairingQrCode { code, timeout } => {
                        let _ = tx.send(PairStatus::QrCode(code, timeout)).await;
                    }
                    Event::PairSuccess { .. } => {
                        let _ = tx.send(PairStatus::Paired).await;
                    }
                    Event::Connected(_) => {
                        let _ = tx.send(PairStatus::AlreadyPaired).await;
                    }
                    _ => {}
                }
            }
        })
        .build()
        .await?;

    let _handle = bot.run().await?;
    tokio::signal::ctrl_c().await.ok();
    Ok(())
}

/// Start the WhatsApp channel.
///
/// `db_path` is the path to the SQLite file used for WhatsApp session persistence.
pub async fn start_whatsapp(
    connector_id: String,
    db_path: PathBuf,
    access_policy: AccessPolicy,
    gateway: Arc<Gateway>,
    bus: Arc<EventBus>,
) -> anyhow::Result<()> {
    let adapter = Arc::new(WhatsAppAdapter::new(&connector_id));

    let db_str = db_path.to_string_lossy().to_string();
    let backend = Arc::new(SqliteStore::new(&db_str).await?);

    let gateway_for_bot = gateway.clone();
    let adapter_for_bot = adapter.clone();
    let policy_for_bot = access_policy.clone();

    let mut bot = Bot::builder()
        .with_backend(backend)
        .with_transport_factory(TokioWebSocketTransportFactory::new())
        .with_http_client(UreqHttpClient::new())
        .with_runtime(TokioRuntime)
        .skip_history_sync()
        .on_event(move |event, client| {
            let gateway = gateway_for_bot.clone();
            let adapter = adapter_for_bot.clone();
            let policy = policy_for_bot.clone();

            async move {
                match event {
                    Event::PairingQrCode { code, .. } => {
                        tracing::info!("WhatsApp QR code for pairing:\n{}", code);
                    }
                    Event::PairSuccess { .. } => {
                        tracing::info!("WhatsApp pairing successful!");
                    }
                    Event::Message(msg, info) => {
                        let is_self_chat;
                        let effective_msg: &wa::Message =
                            if let Some(ref dsm) = msg.device_sent_message {
                                let sender_number =
                                    extract_number_from_jid(&info.source.sender.to_string());
                                let dest_number = dsm
                                    .destination_jid
                                    .as_deref()
                                    .map(extract_number_from_jid)
                                    .unwrap_or_default();
                                if sender_number != dest_number {
                                    return;
                                }
                                is_self_chat = true;
                                match dsm.message.as_deref() {
                                    Some(inner) => inner,
                                    None => return,
                                }
                            } else {
                                is_self_chat = false;
                                &msg
                            };

                        let chat_jid = info.source.chat.to_string();
                        let sender_jid = info.source.sender.to_string();
                        let is_group = chat_jid.ends_with("@g.us");

                        if !is_group && !is_self_chat {
                            let sender_user = extract_number_from_jid(&sender_jid);
                            let chat_user = extract_number_from_jid(&chat_jid);
                            if sender_user != chat_user {
                                return;
                            }
                        }

                        let policy_jid = if sender_jid.ends_with("@lid") {
                            let lid_user = extract_number_from_jid(&sender_jid);
                            if let Some(phone) = client.get_phone_number_from_lid(&lid_user).await {
                                tracing::info!(
                                    lid = %lid_user,
                                    resolved_phone = %phone,
                                    "LID resolved to phone number"
                                );
                                format!("{phone}@s.whatsapp.net")
                            } else {
                                // LID cache may not be populated yet — retry once after a short delay
                                tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
                                if let Some(phone) =
                                    client.get_phone_number_from_lid(&lid_user).await
                                {
                                    tracing::info!(
                                        lid = %lid_user,
                                        resolved_phone = %phone,
                                        "LID resolved to phone number (retry)"
                                    );
                                    format!("{phone}@s.whatsapp.net")
                                } else {
                                    tracing::warn!(
                                        lid = %lid_user,
                                        "LID not found in cache after retry, using raw sender_jid"
                                    );
                                    sender_jid.clone()
                                }
                            }
                        } else {
                            sender_jid.clone()
                        };

                        if !is_self_chat {
                            let allowed = if is_group {
                                policy.is_allowed_group(&policy_jid)
                            } else {
                                policy.is_allowed_dm(&policy_jid)
                            };

                            tracing::info!(
                                sender = %sender_jid,
                                policy_jid = %policy_jid,
                                allowed,
                                dm_policy = %policy.dm_policy,
                                allow_from = ?policy.allow_from,
                                "access policy check"
                            );

                            if !allowed {
                                tracing::info!(
                                    sender = %sender_jid,
                                    chat = %chat_jid,
                                    is_group,
                                    "WhatsApp message blocked by access policy"
                                );
                                return;
                            }
                        }

                        let has_image = effective_msg.image_message.is_some();
                        let text = extract_message_text(effective_msg);
                        if text.is_empty() && !has_image {
                            tracing::debug!(
                                sender = %sender_jid,
                                chat = %chat_jid,
                                is_group,
                                "WhatsApp message ignored: no text and no image"
                            );
                            return;
                        }

                        tracing::info!(
                            sender = %sender_jid,
                            chat = %chat_jid,
                            is_group,
                            text_len = text.len(),
                            "WhatsApp message received"
                        );

                        let msg_id = Some(info.id.clone());

                        let mut inbound = adapter.to_inbound(&chat_jid, &sender_jid, &text, msg_id);

                        if let Some(ref image) = effective_msg.image_message {
                            match client.download(image.as_ref()).await {
                                Ok(data) => {
                                    let base64_data = BASE64_STANDARD.encode(&data);
                                    let mime = image
                                        .mimetype
                                        .clone()
                                        .unwrap_or_else(|| "image/jpeg".to_string());
                                    inbound.attachments.push(Attachment {
                                        kind: AttachmentKind::Image,
                                        url: base64_data,
                                        mime_type: Some(mime),
                                        file_name: None,
                                        size: image.file_length,
                                    });
                                }
                                Err(e) => {
                                    tracing::warn!("Failed to download WhatsApp image: {e}");
                                }
                            }
                        }

                        let _ = client.chatstate().send_composing(&info.source.chat).await;

                        let Some(agent_id) = gateway.resolve_agent(&inbound) else {
                            tracing::debug!(
                                target: "clawhive::channel::whatsapp",
                                conversation_scope = %inbound.conversation_scope,
                                "no routing binding matched, ignoring message"
                            );
                            return;
                        };
                        let agent_emoji = gateway
                            .orchestrator()
                            .config_view()
                            .agents
                            .get(&agent_id)
                            .and_then(|a| a.identity.as_ref())
                            .and_then(|i| i.emoji.clone());
                        let prefix = build_bot_prefix(agent_emoji.as_deref());

                        let gateway = gateway.clone();
                        let client = client.clone();
                        let reply_chat = info.source.chat.clone();
                        tokio::spawn(async move {
                            let turn_complete = Arc::new(tokio::sync::Notify::new());
                            let progress_complete = turn_complete.clone();
                            let progress_client = client.clone();
                            let progress_chat = reply_chat.clone();
                            let _progress_guard = AbortOnDrop(tokio::spawn(async move {
                                tokio::select! {
                                    _ = tokio::time::sleep(tokio::time::Duration::from_secs(60)) => {
                                        let progress = wa::Message {
                                            conversation: Some(PROGRESS_MESSAGE.to_string()),
                                            ..Default::default()
                                        };
                                        if let Err(e) = progress_client.send_message(progress_chat, progress).await {
                                            tracing::warn!(error = %e, "failed to send WhatsApp progress message");
                                        }
                                    }
                                    _ = progress_complete.notified() => {}
                                }
                            }));

                            let result = gateway.handle_inbound(inbound).await;
                            turn_complete.notify_waiters();

                            match result {
                                Ok(Some(outbound)) => {
                                    let _ = client.chatstate().send_paused(&reply_chat).await;

                                    let has_text = !outbound.text.trim().is_empty();
                                    let has_attachments = !outbound.attachments.is_empty();

                                    if !has_text && !has_attachments {
                                        return;
                                    }

                                    let prefixed_text = format!("{prefix}{}", outbound.text);

                                    if has_attachments {
                                        for (i, att) in outbound.attachments.iter().enumerate() {
                                            let caption = if i == 0 && has_text {
                                                Some(prefixed_text.as_str())
                                            } else {
                                                None
                                            };
                                            if let Err(e) = send_attachment(
                                                &client,
                                                &reply_chat,
                                                att,
                                                caption,
                                            )
                                            .await
                                            {
                                                tracing::error!(
                                                    error = %e,
                                                    "failed to send WhatsApp attachment"
                                                );
                                            }
                                        }

                                        if has_text {
                                            tracing::info!(
                                                sender = %sender_jid,
                                                chat = %chat_jid,
                                                attachments = outbound.attachments.len(),
                                                "WhatsApp reply with attachments sent"
                                            );
                                            return;
                                        }
                                    }

                                    if has_text && !has_attachments {
                                        let reply = wa::Message {
                                            conversation: Some(prefixed_text),
                                            ..Default::default()
                                        };
                                        if let Err(e) = client.send_message(reply_chat.clone(), reply).await {
                                            tracing::error!("Failed to send WhatsApp reply: {e}");
                                        } else {
                                            tracing::info!(
                                                sender = %sender_jid,
                                                chat = %chat_jid,
                                                "WhatsApp reply sent"
                                            );
                                        }
                                    }
                                }
                                Ok(None) => {
                                    let _ = client.chatstate().send_paused(&reply_chat).await;
                                }
                                Err(err) => {
                                    let _ = client.chatstate().send_paused(&reply_chat).await;
                                    tracing::error!("Gateway error for WhatsApp message: {err}");
                                }
                            }
                        });
                    }
                    Event::Connected(_) => {
                        tracing::info!("WhatsApp connected");
                    }
                    Event::Disconnected(_) => {
                        tracing::warn!("WhatsApp disconnected");
                    }
                    _ => {}
                }
            }
        })
        .build()
        .await?;

    let wa_client = bot.client();

    tokio::spawn(spawn_action_listener(
        bus.clone(),
        connector_id.clone(),
        wa_client.clone(),
    ));
    tokio::spawn(spawn_delivery_listener(
        bus.clone(),
        connector_id.clone(),
        wa_client.clone(),
    ));
    tokio::spawn(spawn_approval_listener(
        bus.clone(),
        connector_id.clone(),
        wa_client.clone(),
    ));

    tracing::info!("Starting WhatsApp channel (connector: {})", connector_id);
    bot.run().await?.await?;

    Ok(())
}

fn extract_message_text(msg: &wa::Message) -> String {
    if let Some(ref conv) = msg.conversation {
        return conv.to_string();
    }
    if let Some(ref ext) = msg.extended_text_message {
        if let Some(ref text) = ext.text {
            return text.to_string();
        }
    }
    if let Some(ref image) = msg.image_message {
        if let Some(ref caption) = image.caption {
            return caption.to_string();
        }
    }
    if let Some(ref video) = msg.video_message {
        if let Some(ref caption) = video.caption {
            return caption.to_string();
        }
    }
    if let Some(ref document) = msg.document_message {
        if let Some(ref caption) = document.caption {
            return caption.to_string();
        }
    }
    String::new()
}

fn normalize_phone(phone: &str) -> String {
    phone.trim_start_matches('+').to_string()
}

fn extract_number_from_jid(jid: &str) -> String {
    jid.split('@')
        .next()
        .unwrap_or_default()
        .split(':')
        .next()
        .unwrap_or_default()
        .to_string()
}

fn parse_chat_jid(conversation_scope: &str) -> Option<&str> {
    conversation_scope.strip_prefix("chat:")
}

fn build_bot_prefix(emoji: Option<&str>) -> String {
    match emoji {
        Some(e) => format!("{e} "),
        None => "[Bot] ".to_string(),
    }
}

async fn spawn_action_listener(bus: Arc<EventBus>, connector_id: String, client: Arc<Client>) {
    let mut rx = bus.subscribe(Topic::ActionReady).await;
    while let Some(msg) = rx.recv().await {
        let BusMessage::ActionReady { action } = msg else {
            continue;
        };

        if action.channel_type != "whatsapp" || action.connector_id != connector_id {
            continue;
        }

        let Some(chat_jid_str) = parse_chat_jid(&action.conversation_scope) else {
            tracing::warn!(
                "Could not parse WhatsApp chat JID: {}",
                action.conversation_scope
            );
            continue;
        };

        let Ok(chat_jid) = chat_jid_str.parse() else {
            tracing::warn!("Invalid WhatsApp JID: {chat_jid_str}");
            continue;
        };

        match action.action {
            ActionKind::React { ref emoji } => {
                tracing::debug!("WhatsApp reaction: {emoji} (not supported by protocol)");
            }
            ActionKind::Edit { ref new_text } => {
                if let Some(ref original_id) = action.message_id {
                    let fallback_prefix = build_bot_prefix(None);
                    let edit_msg = wa::Message {
                        conversation: Some(format!("{fallback_prefix}{new_text}")),
                        ..Default::default()
                    };
                    if let Err(e) = client
                        .edit_message(chat_jid, original_id.clone(), edit_msg)
                        .await
                    {
                        tracing::error!("Failed to edit WhatsApp message: {e}");
                    }
                } else {
                    tracing::warn!("WhatsApp edit requires original message_id");
                }
            }
            ActionKind::Delete => {
                if let Some(ref original_id) = action.message_id {
                    if let Err(e) = client
                        .revoke_message(
                            chat_jid,
                            original_id.clone(),
                            whatsapp_rust::send::RevokeType::Sender,
                        )
                        .await
                    {
                        tracing::error!("Failed to delete WhatsApp message: {e}");
                    }
                } else {
                    tracing::warn!("WhatsApp delete requires original message_id");
                }
            }
            ActionKind::Unreact { .. } => {}
        }
    }
}

async fn spawn_delivery_listener(bus: Arc<EventBus>, connector_id: String, client: Arc<Client>) {
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

        if channel_type != "whatsapp" || msg_connector_id != connector_id {
            continue;
        }

        let Some(chat_jid_str) = parse_chat_jid(&conversation_scope) else {
            tracing::warn!("Could not parse WhatsApp chat JID: {}", conversation_scope);
            continue;
        };

        let Ok(chat_jid) = chat_jid_str.parse() else {
            tracing::warn!("Invalid WhatsApp JID: {chat_jid_str}");
            continue;
        };

        let fallback_prefix = build_bot_prefix(None);
        let message = wa::Message {
            conversation: Some(format!("{fallback_prefix}{text}")),
            ..Default::default()
        };
        if let Err(e) = client.send_message(chat_jid, message).await {
            tracing::error!("Failed to deliver WhatsApp announce: {e}");
        }
    }
}

async fn spawn_approval_listener(bus: Arc<EventBus>, connector_id: String, client: Arc<Client>) {
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

        if channel_type != "whatsapp" || msg_connector_id != connector_id {
            continue;
        }

        let Some(chat_jid_str) = parse_chat_jid(&conversation_scope) else {
            tracing::warn!("Could not parse WhatsApp chat JID for approval: {conversation_scope}");
            continue;
        };

        let Ok(chat_jid) = chat_jid_str.parse() else {
            tracing::warn!("Invalid WhatsApp JID for approval: {chat_jid_str}");
            continue;
        };

        let display = ApprovalDisplay::new(&agent_id, &command, network_target.as_deref(), summary);
        let text = format!(
            "{}\n\nReply:\n✅ yes {short_id}\n🔓 always {short_id}\n❌ no {short_id}",
            display.to_markdown()
        );

        let prefix = build_bot_prefix(None);
        let message = wa::Message {
            conversation: Some(format!("{prefix}{text}")),
            ..Default::default()
        };
        if let Err(e) = client.send_message(chat_jid, message).await {
            tracing::error!("Failed to send WhatsApp approval message: {e}");
        } else {
            tracing::info!(short_id, connector_id, "WhatsApp approval message sent");
        }
    }
}

async fn resolve_attachment_bytes(att: &Attachment) -> anyhow::Result<Vec<u8>> {
    let url = &att.url;

    if url.starts_with('/')
        || url.starts_with("./")
        || url.starts_with("../")
        || std::path::Path::new(url).exists()
    {
        return tokio::fs::read(url)
            .await
            .map_err(|e| anyhow::anyhow!("read file {url}: {e}"));
    }

    if url.starts_with("http://") || url.starts_with("https://") {
        let resp = reqwest::get(url).await?.error_for_status()?;
        return Ok(resp.bytes().await?.to_vec());
    }

    BASE64_STANDARD
        .decode(url)
        .map_err(|e| anyhow::anyhow!("base64 decode: {e}"))
}

fn attachment_media_type(kind: AttachmentKind) -> MediaType {
    match kind {
        AttachmentKind::Image => MediaType::Image,
        _ => MediaType::Document,
    }
}

fn build_attachment_message(
    att: &Attachment,
    upload: UploadResponse,
    caption: Option<&str>,
) -> wa::Message {
    match att.kind {
        AttachmentKind::Image => wa::Message {
            image_message: Some(Box::new(ImageMessage {
                url: Some(upload.url),
                direct_path: Some(upload.direct_path),
                media_key: Some(upload.media_key.to_vec()),
                file_enc_sha256: Some(upload.file_enc_sha256.to_vec()),
                file_sha256: Some(upload.file_sha256.to_vec()),
                file_length: Some(upload.file_length),
                mimetype: att.mime_type.clone(),
                caption: caption.map(ToString::to_string),
                ..Default::default()
            })),
            ..Default::default()
        },
        _ => {
            let file_name = att.file_name.clone().unwrap_or_else(|| {
                let ext = att
                    .mime_type
                    .as_deref()
                    .and_then(|m| m.split('/').nth(1))
                    .unwrap_or("bin");
                format!("file.{ext}")
            });

            wa::Message {
                document_message: Some(Box::new(DocumentMessage {
                    url: Some(upload.url),
                    direct_path: Some(upload.direct_path),
                    media_key: Some(upload.media_key.to_vec()),
                    file_enc_sha256: Some(upload.file_enc_sha256.to_vec()),
                    file_sha256: Some(upload.file_sha256.to_vec()),
                    file_length: Some(upload.file_length),
                    mimetype: att.mime_type.clone(),
                    file_name: Some(file_name),
                    caption: caption.map(ToString::to_string),
                    ..Default::default()
                })),
                ..Default::default()
            }
        }
    }
}

async fn send_attachment(
    client: &Client,
    chat_jid: &Jid,
    att: &Attachment,
    caption: Option<&str>,
) -> anyhow::Result<()> {
    let bytes = resolve_attachment_bytes(att).await?;
    let media_type = attachment_media_type(att.kind.clone());

    let upload = client
        .upload(bytes, media_type, UploadOptions::default())
        .await?;
    let message = build_attachment_message(att, upload, caption);

    client.send_message(chat_jid.clone(), message).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::env;
    use std::fs;

    use super::*;
    use wacore::download::MediaType;
    use waproto::whatsapp::message::{DocumentMessage, ImageMessage, VideoMessage};

    fn test_upload_response() -> UploadResponse {
        UploadResponse {
            url: "https://example.com/media".to_string(),
            direct_path: "/v/t62/example".to_string(),
            media_key: [1u8; 32],
            file_enc_sha256: [4u8; 32],
            file_sha256: [7u8; 32],
            file_length: 42,
            media_key_timestamp: 0,
        }
    }

    #[test]
    fn adapter_to_inbound_sets_fields() {
        let adapter = WhatsAppAdapter::new("wa_main");
        let msg = adapter.to_inbound("123@s.whatsapp.net", "456@s.whatsapp.net", "hello", None);
        assert_eq!(msg.channel_type, "whatsapp");
        assert_eq!(msg.connector_id, "wa_main");
        assert_eq!(msg.conversation_scope, "chat:123@s.whatsapp.net");
        assert_eq!(msg.user_scope, "user:456@s.whatsapp.net");
        assert_eq!(msg.text, "hello");
    }

    #[test]
    fn render_outbound_formats_correctly() {
        let adapter = WhatsAppAdapter::new("wa_main");
        let outbound = OutboundMessage {
            trace_id: Uuid::new_v4(),
            channel_type: "whatsapp".into(),
            connector_id: "wa_main".into(),
            conversation_scope: "chat:123@s.whatsapp.net".into(),
            text: "hi there".into(),
            at: Utc::now(),
            reply_to: None,
            attachments: vec![],
        };
        let rendered = adapter.render_outbound(&outbound);
        assert_eq!(rendered, "[whatsapp:chat:123@s.whatsapp.net] hi there");
    }

    #[test]
    fn parse_chat_jid_works() {
        assert_eq!(
            parse_chat_jid("chat:123@s.whatsapp.net"),
            Some("123@s.whatsapp.net")
        );
        assert_eq!(parse_chat_jid("invalid"), None);
    }

    #[test]
    fn extract_message_text_prefers_supported_caption_fields() {
        let image = wa::Message {
            image_message: Some(Box::new(ImageMessage {
                caption: Some("image caption".to_string()),
                ..Default::default()
            })),
            ..Default::default()
        };
        assert_eq!(extract_message_text(&image), "image caption");

        let video = wa::Message {
            video_message: Some(Box::new(VideoMessage {
                caption: Some("video caption".to_string()),
                ..Default::default()
            })),
            ..Default::default()
        };
        assert_eq!(extract_message_text(&video), "video caption");

        let document = wa::Message {
            document_message: Some(Box::new(DocumentMessage {
                caption: Some("doc caption".to_string()),
                ..Default::default()
            })),
            ..Default::default()
        };
        assert_eq!(extract_message_text(&document), "doc caption");
    }

    #[test]
    fn access_policy_normalizes_allowlist_and_matches_sender_jid() {
        let policy =
            AccessPolicy::from_config("allowlist", &["+1234567890".to_string()], "disabled", &[]);

        assert!(policy.is_allowed_dm("1234567890@s.whatsapp.net"));
        assert!(policy.is_allowed_dm("1234567890:0@s.whatsapp.net"));
        assert!(!policy.is_allowed_dm("9876543210@s.whatsapp.net"));
    }

    #[test]
    fn access_policy_group_allowlist_falls_back_to_dm_allowlist() {
        let policy =
            AccessPolicy::from_config("allowlist", &["+1234567890".to_string()], "allowlist", &[]);

        assert!(policy.is_allowed_group("1234567890@s.whatsapp.net"));
        assert!(!policy.is_allowed_group("9876543210@s.whatsapp.net"));
    }

    #[test]
    fn build_attachment_message_uses_caption_for_first_image() {
        let attachment = Attachment {
            kind: AttachmentKind::Image,
            url: "aGVsbG8=".to_string(),
            mime_type: Some("image/png".to_string()),
            file_name: None,
            size: Some(42),
        };

        let message =
            build_attachment_message(&attachment, test_upload_response(), Some("caption"));

        assert_eq!(message.conversation, None);
        let image = message.image_message.expect("expected image message");
        assert_eq!(image.caption.as_deref(), Some("caption"));
        assert_eq!(image.mimetype.as_deref(), Some("image/png"));
        assert_eq!(image.file_length, Some(42));
    }

    #[test]
    fn build_attachment_message_generates_document_filename_from_mime() {
        let attachment = Attachment {
            kind: AttachmentKind::Document,
            url: "aGVsbG8=".to_string(),
            mime_type: Some("application/pdf".to_string()),
            file_name: None,
            size: Some(42),
        };

        let message =
            build_attachment_message(&attachment, test_upload_response(), Some("doc caption"));

        let document = message.document_message.expect("expected document message");
        assert_eq!(document.file_name.as_deref(), Some("file.pdf"));
        assert_eq!(document.caption.as_deref(), Some("doc caption"));
        assert_eq!(document.mimetype.as_deref(), Some("application/pdf"));
    }

    #[test]
    fn attachment_media_type_uses_document_for_non_images() {
        assert!(matches!(
            attachment_media_type(AttachmentKind::Image),
            MediaType::Image
        ));
        assert!(matches!(
            attachment_media_type(AttachmentKind::Video),
            MediaType::Document
        ));
        assert!(matches!(
            attachment_media_type(AttachmentKind::Audio),
            MediaType::Document
        ));
        assert!(matches!(
            attachment_media_type(AttachmentKind::Document),
            MediaType::Document
        ));
        assert!(matches!(
            attachment_media_type(AttachmentKind::Other),
            MediaType::Document
        ));
    }

    #[tokio::test]
    async fn resolve_attachment_bytes_reads_plain_relative_path() {
        let old_dir = env::current_dir().expect("cwd");
        let temp_dir = env::temp_dir().join(format!("clawhive-whatsapp-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&temp_dir).expect("create temp dir");
        let file_path = temp_dir.join("note.txt");
        fs::write(&file_path, b"hello").expect("write file");
        env::set_current_dir(&temp_dir).expect("set cwd");

        let attachment = Attachment {
            kind: AttachmentKind::Document,
            url: "note.txt".to_string(),
            mime_type: Some("text/plain".to_string()),
            file_name: Some("note.txt".to_string()),
            size: Some(5),
        };

        let result = resolve_attachment_bytes(&attachment).await;

        env::set_current_dir(old_dir).expect("restore cwd");
        fs::remove_dir_all(&temp_dir).expect("cleanup temp dir");
        assert_eq!(result.expect("read bytes"), b"hello");
    }

    #[test]
    fn build_bot_prefix_with_emoji() {
        assert_eq!(build_bot_prefix(Some("🐝")), "🐝 ");
        assert_eq!(build_bot_prefix(Some("🤖")), "🤖 ");
    }

    #[test]
    fn build_bot_prefix_without_emoji() {
        assert_eq!(build_bot_prefix(None), "[Bot] ");
    }
}
