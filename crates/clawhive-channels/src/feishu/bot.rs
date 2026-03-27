use std::sync::Arc;

use anyhow::Result;
use base64::Engine;
use chrono::Utc;
use clawhive_bus::EventBus;
use clawhive_gateway::Gateway;
use clawhive_schema::{Attachment, AttachmentKind, InboundMessage};
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use uuid::Uuid;

use super::client::FeishuClient;
use super::codec::*;
use super::listeners;
use super::message::*;
use super::types::*;

pub struct FeishuAdapter {
    connector_id: String,
}

impl FeishuAdapter {
    pub fn new(connector_id: impl Into<String>) -> Self {
        Self {
            connector_id: connector_id.into(),
        }
    }

    pub fn to_inbound(&self, event: &FeishuEvent, attachments: Vec<Attachment>) -> InboundMessage {
        let text = match event.event.message.message_type.as_str() {
            "text" => serde_json::from_str::<FeishuTextContent>(&event.event.message.content)
                .map(|c| c.text)
                .unwrap_or_default(),
            "post" => serde_json::from_str::<FeishuPostContent>(&event.event.message.content)
                .map(|c| c.extract_text())
                .unwrap_or_default(),
            "image" | "file" => String::new(),
            _ => event.event.message.content.clone(),
        };

        let is_mention = event
            .event
            .message
            .mentions
            .as_ref()
            .map(|m| !m.is_empty())
            .unwrap_or(false);

        let conversation_scope =
            feishu_scope(&event.event.message.chat_id, &event.event.message.chat_type);

        InboundMessage {
            trace_id: Uuid::new_v4(),
            channel_type: "feishu".to_string(),
            connector_id: self.connector_id.clone(),
            conversation_scope,
            user_scope: format!("user:{}", event.event.sender.sender_id.open_id),
            text,
            at: Utc::now(),
            thread_id: None,
            is_mention,
            mention_target: None,
            message_id: Some(event.event.message.message_id.clone()),
            attachments,
            message_source: None,
        }
    }
}

pub struct FeishuBot {
    app_id: String,
    app_secret: String,
    pub(crate) connector_id: String,
    gateway: Arc<Gateway>,
    bus: Arc<EventBus>,
    require_mention: bool,
}

impl FeishuBot {
    pub fn new(
        app_id: impl Into<String>,
        app_secret: impl Into<String>,
        connector_id: impl Into<String>,
        gateway: Arc<Gateway>,
        bus: Arc<EventBus>,
    ) -> Self {
        Self {
            app_id: app_id.into(),
            app_secret: app_secret.into(),
            connector_id: connector_id.into(),
            gateway,
            bus,
            require_mention: true,
        }
    }

    pub fn with_require_mention(mut self, require: bool) -> Self {
        self.require_mention = require;
        self
    }

    pub async fn run_impl(self) -> Result<()> {
        let client = Arc::new(FeishuClient::new(&self.app_id, &self.app_secret));

        client.refresh_token().await?;
        client.spawn_token_refresh();

        let bot_open_id = match client.get_bot_open_id().await {
            Ok(id) => {
                tracing::info!(
                    target: "clawhive::channel::feishu",
                    bot_open_id = %id,
                    "fetched bot identity"
                );
                Some(id)
            }
            Err(e) => {
                tracing::warn!(
                    target: "clawhive::channel::feishu",
                    error = %e,
                    "failed to fetch bot open_id, mention filtering may be inaccurate"
                );
                None
            }
        };

        let adapter = Arc::new(FeishuAdapter::new(&self.connector_id));

        listeners::spawn_delivery_listener(
            self.bus.clone(),
            client.clone(),
            self.connector_id.clone(),
        );
        listeners::spawn_approval_listener(
            self.bus.clone(),
            client.clone(),
            self.connector_id.clone(),
        );
        listeners::spawn_skill_confirm_listener(
            self.bus.clone(),
            client.clone(),
            self.connector_id.clone(),
        );
        listeners::spawn_action_listener(
            self.bus.clone(),
            client.clone(),
            self.connector_id.clone(),
        );

        tracing::info!(
            target: "clawhive::channel::feishu",
            connector_id = %self.connector_id,
            "feishu bot starting WebSocket connection"
        );

        let bot_open_id = Arc::new(bot_open_id);

        loop {
            match self
                .connect_and_listen(&client, &adapter, &bot_open_id)
                .await
            {
                Ok(()) => {
                    tracing::info!(
                        target: "clawhive::channel::feishu",
                        "feishu WebSocket disconnected, reconnecting..."
                    );
                }
                Err(e) => {
                    tracing::error!(
                        target: "clawhive::channel::feishu",
                        error = %e,
                        "feishu WebSocket error, reconnecting in 5s..."
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        }
    }

    async fn connect_and_listen(
        &self,
        client: &Arc<FeishuClient>,
        adapter: &Arc<FeishuAdapter>,
        bot_open_id: &Arc<Option<String>>,
    ) -> Result<()> {
        let (wss_url, config) = client.get_ws_endpoint().await?;

        let parsed_url = url::Url::parse(&wss_url)?;
        let service_id: i32 = parsed_url
            .query_pairs()
            .find(|(k, _)| k == "service_id")
            .and_then(|(_, v)| v.parse().ok())
            .unwrap_or(0);

        let (ws_stream, _) = tokio_tungstenite::connect_async(&wss_url).await?;
        let (mut write, mut read) = ws_stream.split();

        tracing::info!(target: "clawhive::channel::feishu", "feishu WebSocket connected");

        let ping_interval = std::time::Duration::from_secs(config.ping_interval);
        let (ping_tx, mut ping_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(ping_interval).await;
                let frame = new_ping_frame(service_id);
                let mut buf = Vec::new();
                if frame.encode(&mut buf).is_ok() && ping_tx.send(buf).await.is_err() {
                    break;
                }
            }
        });

        loop {
            tokio::select! {
                msg = read.next() => {
                    match msg {
                        Some(Ok(WsMessage::Binary(data))) => {
                            if let Ok(frame) = Frame::decode(&data[..]) {
                                let ack = self.handle_frame(&frame, adapter, client, bot_open_id).await;
                                if let Some(ack_bytes) = ack {
                                    if let Err(e) = write.send(WsMessage::Binary(ack_bytes.into())).await {
                                        tracing::error!(target: "clawhive::channel::feishu", error = %e, "failed to send ACK");
                                        break;
                                    }
                                }
                            }
                        }
                        Some(Ok(WsMessage::Text(data))) => {
                            if let Ok(frame) = Frame::decode(data.as_bytes()) {
                                let ack = self.handle_frame(&frame, adapter, client, bot_open_id).await;
                                if let Some(ack_bytes) = ack {
                                    if let Err(e) = write.send(WsMessage::Binary(ack_bytes.into())).await {
                                        tracing::error!(target: "clawhive::channel::feishu", error = %e, "failed to send ACK");
                                        break;
                                    }
                                }
                            }
                        }
                        Some(Ok(WsMessage::Close(_))) | None => {
                            tracing::info!(target: "clawhive::channel::feishu", "WebSocket closed");
                            break;
                        }
                        Some(Err(e)) => {
                            tracing::error!(target: "clawhive::channel::feishu", error = %e, "WebSocket read error");
                            break;
                        }
                        _ => {}
                    }
                }
                Some(ping_data) = ping_rx.recv() => {
                    if let Err(e) = write.send(WsMessage::Binary(ping_data.into())).await {
                        tracing::error!(target: "clawhive::channel::feishu", error = %e, "failed to send ping");
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    async fn handle_frame(
        &self,
        frame: &Frame,
        adapter: &Arc<FeishuAdapter>,
        client: &Arc<FeishuClient>,
        bot_open_id: &Arc<Option<String>>,
    ) -> Option<Vec<u8>> {
        let frame_type = frame.get_header("type").unwrap_or("");

        match (frame.method, frame_type) {
            (FRAME_METHOD_CONTROL, "pong") => {
                tracing::trace!(target: "clawhive::channel::feishu", "received pong");
                None
            }
            (FRAME_METHOD_DATA, "event") => {
                let raw: serde_json::Value = match serde_json::from_slice(&frame.payload) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(target: "clawhive::channel::feishu", error = %e, "failed to parse event payload");
                        return Some(self.build_ack(frame, 200));
                    }
                };

                let event_type = raw
                    .pointer("/header/event_type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");

                match event_type {
                    "im.message.receive_v1" => {
                        if let Ok(event) = serde_json::from_value::<FeishuEvent>(raw) {
                            self.handle_message_event(&event, adapter, client, bot_open_id)
                                .await;
                        }
                    }
                    "card.action.trigger" => {
                        if let Ok(event) = serde_json::from_value::<FeishuCardActionEvent>(raw) {
                            self.handle_card_action(&event, client).await;
                        }
                        return Some(self.build_card_action_ack(frame));
                    }
                    other => {
                        tracing::debug!(target: "clawhive::channel::feishu", event_type = other, "ignoring unhandled event type");
                    }
                }
                Some(self.build_ack(frame, 200))
            }
            _ => {
                tracing::debug!(target: "clawhive::channel::feishu", method = frame.method, frame_type = frame_type, "ignoring unknown frame");
                None
            }
        }
    }

    async fn handle_message_event(
        &self,
        event: &FeishuEvent,
        adapter: &Arc<FeishuAdapter>,
        client: &Arc<FeishuClient>,
        bot_open_id: &Option<String>,
    ) {
        let chat_id = event.event.message.chat_id.clone();
        let chat_type = event.event.message.chat_type.clone();
        let message_id = event.event.message.message_id.clone();
        let is_bot_mentioned = event
            .event
            .message
            .mentions
            .as_ref()
            .map(|mentions| {
                mentions.iter().any(|m| {
                    bot_open_id
                        .as_ref()
                        .map(|bid| m.id.open_id == *bid)
                        .unwrap_or(true) // fallback: treat any mention as bot mention
                })
            })
            .unwrap_or(false);

        if chat_type == "group" && self.require_mention && !is_bot_mentioned {
            tracing::debug!(
                target: "clawhive::channel::feishu",
                %chat_id,
                %message_id,
                "feishu inbound skipped: group message without mention"
            );
            return;
        }

        let attachments = Self::download_inbound_attachments(event, client).await;
        let inbound = adapter.to_inbound(event, attachments);
        let gw = self.gateway.clone();
        let client = client.clone();

        tokio::spawn(async move {
            let placeholder_id: Option<String> = None;

            match gw.handle_inbound(inbound).await {
                Ok(Some(outbound)) => {
                    let text = outbound.text.trim();

                    if text.is_empty() && outbound.attachments.is_empty() {
                        if let Some(ref ph_id) = placeholder_id {
                            let _ = client.delete_message(ph_id).await;
                        }
                        if let Some(fallback) = empty_outbound_fallback_text(&chat_type) {
                            let content = serde_json::json!({"text": fallback}).to_string();
                            let _ = client.reply_message(&message_id, "text", &content).await;
                        }
                    } else if let Some(ref ph_id) = placeholder_id {
                        if !text.is_empty() {
                            let use_card = has_formatting(text);
                            let max_bytes = if use_card {
                                FEISHU_CARD_MAX_BYTES
                            } else {
                                FEISHU_TEXT_MAX_BYTES
                            };
                            let chunks = split_message(text, max_bytes);

                            let first = chunks[0];
                            if use_card {
                                let _ = client.delete_message(ph_id).await;
                                let card = md_to_feishu_card(first);
                                if let Err(e) = client.reply_card(&message_id, &card).await {
                                    tracing::error!(target: "clawhive::channel::feishu", error = %e, "failed to reply with card");
                                }
                            } else {
                                let content = serde_json::json!({"text": first}).to_string();
                                if let Err(e) = client.edit_message(ph_id, "text", &content).await {
                                    tracing::error!(target: "clawhive::channel::feishu", error = %e, "failed to edit message with text");
                                }
                            }

                            for chunk in chunks.iter().skip(1) {
                                if use_card {
                                    let card = md_to_feishu_card(chunk);
                                    if let Err(e) = client.send_card(&chat_id, &card).await {
                                        tracing::error!(target: "clawhive::channel::feishu", error = %e, "failed to send card chunk");
                                    }
                                } else {
                                    let content = serde_json::json!({"text": *chunk}).to_string();
                                    if let Err(e) =
                                        client.send_message(&chat_id, "text", &content).await
                                    {
                                        tracing::error!(target: "clawhive::channel::feishu", error = %e, "failed to send text chunk");
                                    }
                                }
                            }
                        }

                        if !outbound.attachments.is_empty() {
                            send_outbound_attachments(&client, &chat_id, &outbound.attachments)
                                .await;
                        }
                    } else {
                        if !text.is_empty() {
                            let use_card = has_formatting(text);
                            let reply_to = outbound.reply_to.as_deref().unwrap_or(&message_id);

                            if use_card {
                                let chunks = split_message(text, FEISHU_CARD_MAX_BYTES);
                                for (i, chunk) in chunks.iter().enumerate() {
                                    let card = md_to_feishu_card(chunk);
                                    if i == 0 {
                                        if let Err(e) = client.reply_card(reply_to, &card).await {
                                            tracing::error!(target: "clawhive::channel::feishu", error = %e, "failed to reply with card");
                                        }
                                    } else if let Err(e) = client.send_card(&chat_id, &card).await {
                                        tracing::error!(target: "clawhive::channel::feishu", error = %e, "failed to send card chunk");
                                    }
                                }
                            } else {
                                let chunks = split_message(text, FEISHU_TEXT_MAX_BYTES);
                                for (i, chunk) in chunks.iter().enumerate() {
                                    let content = serde_json::json!({"text": *chunk}).to_string();
                                    if i == 0 {
                                        if let Err(e) =
                                            client.reply_message(reply_to, "text", &content).await
                                        {
                                            tracing::error!(target: "clawhive::channel::feishu", error = %e, "failed to reply with text");
                                        }
                                    } else if let Err(e) =
                                        client.send_message(&chat_id, "text", &content).await
                                    {
                                        tracing::error!(target: "clawhive::channel::feishu", error = %e, "failed to send text chunk");
                                    }
                                }
                            }
                        }

                        if !outbound.attachments.is_empty() {
                            send_outbound_attachments(&client, &chat_id, &outbound.attachments)
                                .await;
                        }
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::error!(target: "clawhive::channel::feishu", error = %e, "failed to handle inbound");
                    if let Some(ref ph_id) = placeholder_id {
                        let _ = client.delete_message(ph_id).await;
                    }
                }
            }
        });
    }

    async fn download_inbound_attachments(
        event: &FeishuEvent,
        client: &Arc<FeishuClient>,
    ) -> Vec<Attachment> {
        let mut attachments = Vec::new();
        let msg = &event.event.message;

        match msg.message_type.as_str() {
            "image" => {
                if let Ok(img) = serde_json::from_str::<FeishuImageContent>(&msg.content) {
                    Self::download_image(client, &msg.message_id, &img.image_key, &mut attachments)
                        .await;
                }
            }
            "post" => {
                if let Ok(post) = serde_json::from_str::<FeishuPostContent>(&msg.content) {
                    for key in post.image_keys() {
                        Self::download_image(client, &msg.message_id, &key, &mut attachments).await;
                    }
                }
            }
            "file" => {
                if let Ok(file) = serde_json::from_str::<FeishuFileContent>(&msg.content) {
                    match client
                        .download_resource(&msg.message_id, &file.file_key, "file")
                        .await
                    {
                        Ok(bytes) => {
                            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
                            attachments.push(Attachment {
                                kind: AttachmentKind::Document,
                                url: b64,
                                mime_type: None,
                                file_name: file.file_name,
                                size: Some(bytes.len() as u64),
                            });
                        }
                        Err(e) => {
                            tracing::warn!(target: "clawhive::channel::feishu", error = %e, "failed to download inbound file")
                        }
                    }
                }
            }
            _ => {}
        }

        attachments
    }

    async fn download_image(
        client: &Arc<FeishuClient>,
        message_id: &str,
        image_key: &str,
        attachments: &mut Vec<Attachment>,
    ) {
        match client
            .download_resource(message_id, image_key, "image")
            .await
        {
            Ok(bytes) => {
                let mime = detect_image_mime(&bytes);
                let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
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
                    target: "clawhive::channel::feishu",
                    error = %e,
                    image_key,
                    "failed to download inbound image"
                );
            }
        }
    }

    async fn handle_card_action(&self, event: &FeishuCardActionEvent, client: &Arc<FeishuClient>) {
        let action_type = event
            .event
            .action
            .value
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        match action_type {
            "approve_allow" | "approve_always" | "approve_deny" => {
                self.handle_approval_callback(event, client).await;
            }
            "skill_confirm" | "skill_cancel" => {
                self.handle_skill_confirm_callback(event, client).await;
            }
            _ => {
                tracing::debug!(target: "clawhive::channel::feishu", action = action_type, "ignoring unknown card action");
            }
        }
    }

    async fn handle_approval_callback(
        &self,
        event: &FeishuCardActionEvent,
        client: &Arc<FeishuClient>,
    ) {
        let action_value = &event.event.action.value;
        let action_type = action_value
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let short_id = action_value
            .get("short_id")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let decision = match action_type {
            "approve_allow" => "allow",
            "approve_always" => "always",
            "approve_deny" => "deny",
            _ => return,
        };

        let chat_id = event.event.context.open_chat_id.clone();
        let user_id = event.event.operator.open_id.clone();
        let msg_id = event.event.context.open_message_id.clone();
        let conversation_scope = action_value
            .get("scope")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| format!("chat:{chat_id}"));

        let inbound = InboundMessage {
            trace_id: Uuid::new_v4(),
            channel_type: "feishu".to_string(),
            connector_id: self.connector_id.clone(),
            conversation_scope,
            user_scope: format!("user:{user_id}"),
            text: format!("/approve {short_id} {decision}"),
            at: Utc::now(),
            thread_id: None,
            is_mention: false,
            mention_target: None,
            message_id: Some(msg_id.clone()),
            attachments: vec![],
            message_source: None,
        };

        let gw = self.gateway.clone();
        let client = client.clone();
        tokio::spawn(async move {
            match gw.handle_inbound(inbound).await {
                Ok(Some(outbound)) => {
                    let result_text = if outbound.text.trim().is_empty() {
                        format!("Decision recorded: {decision}")
                    } else {
                        outbound.text.clone()
                    };
                    let updated_card = serde_json::json!({
                        "schema": "2.0",
                        "header": { "title": { "tag": "plain_text", "content": "⚠️ Command Approval" }, "template": "green" },
                        "body": { "elements": [{ "tag": "markdown", "content": result_text }] }
                    });
                    let content = serde_json::to_string(&updated_card).unwrap_or_default();
                    let _ = client.edit_message(&msg_id, "interactive", &content).await;
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::error!(target: "clawhive::channel::feishu", error = %e, "failed to handle approval callback")
                }
            }
        });
    }

    async fn handle_skill_confirm_callback(
        &self,
        event: &FeishuCardActionEvent,
        client: &Arc<FeishuClient>,
    ) {
        let action_value = &event.event.action.value;
        let action_type = action_value
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let token = action_value
            .get("token")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        let chat_id = event.event.context.open_chat_id.clone();
        let user_id = event.event.operator.open_id.clone();
        let msg_id = event.event.context.open_message_id.clone();
        let conversation_scope = action_value
            .get("scope")
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| format!("chat:{chat_id}"));

        if action_type == "skill_cancel" {
            let card = serde_json::json!({
                "schema": "2.0",
                "header": { "title": { "tag": "plain_text", "content": "📦 Skill Installation" }, "template": "grey" },
                "body": { "elements": [{ "tag": "markdown", "content": "Installation cancelled." }] }
            });
            let content = serde_json::to_string(&card).unwrap_or_default();
            let _ = client.edit_message(&msg_id, "interactive", &content).await;
            return;
        }

        let inbound = InboundMessage {
            trace_id: Uuid::new_v4(),
            channel_type: "feishu".to_string(),
            connector_id: self.connector_id.clone(),
            conversation_scope,
            user_scope: format!("user:{user_id}"),
            text: format!("/skill confirm {token}"),
            at: Utc::now(),
            thread_id: None,
            is_mention: false,
            mention_target: None,
            message_id: Some(msg_id.clone()),
            attachments: vec![],
            message_source: None,
        };

        let gw = self.gateway.clone();
        let client = client.clone();
        tokio::spawn(async move {
            match gw.handle_inbound(inbound).await {
                Ok(Some(outbound)) => {
                    let result_text = if outbound.text.trim().is_empty() {
                        "Skill installed successfully.".to_string()
                    } else {
                        outbound.text.clone()
                    };
                    let card = serde_json::json!({
                        "schema": "2.0",
                        "header": { "title": { "tag": "plain_text", "content": "📦 Skill Installation" }, "template": "green" },
                        "body": { "elements": [{ "tag": "markdown", "content": result_text }] }
                    });
                    let content = serde_json::to_string(&card).unwrap_or_default();
                    let _ = client.edit_message(&msg_id, "interactive", &content).await;
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::error!(target: "clawhive::channel::feishu", error = %e, "failed to handle skill confirm callback")
                }
            }
        });
    }

    /// Build ACK for card action callbacks.
    /// Feishu card actions expect code 0 and a toast in data to avoid error 200340.
    fn build_card_action_ack(&self, original: &Frame) -> Vec<u8> {
        let ack = FeishuAckResponse {
            code: 0,
            headers: std::collections::HashMap::new(),
            data: Some(serde_json::json!({
                "toast": {
                    "type": "info",
                    "content": "Processing..."
                }
            })),
        };
        let payload = serde_json::to_vec(&ack).unwrap_or_default();
        let mut ack_frame = original.clone();
        ack_frame.payload = payload;
        let mut buf = Vec::new();
        let _ = ack_frame.encode(&mut buf);
        buf
    }

    fn build_ack(&self, original: &Frame, status_code: i32) -> Vec<u8> {
        let ack = FeishuAckResponse {
            code: status_code,
            headers: std::collections::HashMap::new(),
            data: None,
        };
        let payload = serde_json::to_vec(&ack).unwrap_or_default();
        let mut ack_frame = original.clone();
        ack_frame.payload = payload;
        let mut buf = Vec::new();
        let _ = ack_frame.encode(&mut buf);
        buf
    }
}

#[async_trait::async_trait]
impl crate::ChannelBot for FeishuBot {
    fn channel_type(&self) -> &str {
        "feishu"
    }

    fn connector_id(&self) -> &str {
        &self.connector_id
    }

    async fn run(self: Box<Self>) -> anyhow::Result<()> {
        (*self).run_impl().await
    }
}

/// Build conversation_scope for feishu based on chat_type.
/// - `"p2p"` → `dm:{chat_id}`
/// - `"group"` (and others) → `group:chat:{chat_id}`
pub(crate) fn feishu_scope(chat_id: &str, chat_type: &str) -> String {
    match chat_type {
        "p2p" => format!("dm:{chat_id}"),
        _ => format!("group:chat:{chat_id}"),
    }
}

/// Extract the raw feishu chat_id from any conversation_scope format.
/// Handles: `group:chat:{id}`, `dm:{id}`, `chat:{id}` (legacy).
pub(crate) fn parse_feishu_chat_id(scope: &str) -> &str {
    scope
        .strip_prefix("group:chat:")
        .or_else(|| scope.strip_prefix("dm:"))
        .or_else(|| scope.strip_prefix("chat:"))
        .unwrap_or(scope)
}

fn detect_image_mime(bytes: &[u8]) -> &'static str {
    match bytes {
        [0x89, 0x50, 0x4E, 0x47, ..] => "image/png",
        [0xFF, 0xD8, 0xFF, ..] => "image/jpeg",
        [0x47, 0x49, 0x46, 0x38, ..] => "image/gif",
        [0x52, 0x49, 0x46, 0x46, _, _, _, _, 0x57, 0x45, 0x42, 0x50, ..] => "image/webp",
        _ => "image/png",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_to_inbound_converts_correctly() {
        let adapter = FeishuAdapter::new("feishu-main");
        let event = make_test_event("oc_chat1", "ou_user1", "om_msg1", "hello world");
        let inbound = adapter.to_inbound(&event, vec![]);
        assert_eq!(inbound.channel_type, "feishu");
        assert_eq!(inbound.connector_id, "feishu-main");
        assert_eq!(inbound.conversation_scope, "group:chat:oc_chat1");
        assert_eq!(inbound.text, "hello world");
    }

    #[test]
    fn adapter_to_inbound_post_extracts_text() {
        let adapter = FeishuAdapter::new("feishu-main");
        let mut event = make_test_event("oc_chat1", "ou_user1", "om_msg1", "");
        event.event.message.message_type = "post".to_string();
        event.event.message.content =
            r#"{"title":"","content":[[{"tag":"text","text":"这是什么"}]]}"#.to_string();
        let inbound = adapter.to_inbound(&event, vec![]);
        assert_eq!(inbound.text, "这是什么");
    }

    #[test]
    fn detect_jpeg_from_magic_bytes() {
        assert_eq!(
            detect_image_mime(&[0xFF, 0xD8, 0xFF, 0xE0, 0x00]),
            "image/jpeg"
        );
    }

    #[test]
    fn detect_png_from_magic_bytes() {
        assert_eq!(
            detect_image_mime(&[0x89, 0x50, 0x4E, 0x47, 0x0D]),
            "image/png"
        );
    }

    #[test]
    fn detect_webp_from_magic_bytes() {
        let webp = [
            0x52, 0x49, 0x46, 0x46, 0x00, 0x00, 0x00, 0x00, 0x57, 0x45, 0x42, 0x50,
        ];
        assert_eq!(detect_image_mime(&webp), "image/webp");
    }

    #[test]
    fn feishu_scope_group() {
        assert_eq!(feishu_scope("oc_123", "group"), "group:chat:oc_123");
    }

    #[test]
    fn feishu_scope_dm() {
        assert_eq!(feishu_scope("oc_456", "p2p"), "dm:oc_456");
    }

    #[test]
    fn parse_feishu_chat_id_group() {
        assert_eq!(parse_feishu_chat_id("group:chat:oc_123"), "oc_123");
    }

    #[test]
    fn parse_feishu_chat_id_dm() {
        assert_eq!(parse_feishu_chat_id("dm:oc_456"), "oc_456");
    }

    #[test]
    fn parse_feishu_chat_id_legacy() {
        assert_eq!(parse_feishu_chat_id("chat:oc_789"), "oc_789");
    }

    #[test]
    fn adapter_to_inbound_dm_scope() {
        let adapter = FeishuAdapter::new("feishu-main");
        let mut event = make_test_event("oc_chat1", "ou_user1", "om_msg1", "hi");
        event.event.message.chat_type = "p2p".to_string();
        let inbound = adapter.to_inbound(&event, vec![]);
        assert_eq!(inbound.conversation_scope, "dm:oc_chat1");
    }

    fn make_test_event(chat_id: &str, user_id: &str, msg_id: &str, text: &str) -> FeishuEvent {
        FeishuEvent {
            schema: Some("2.0".to_string()),
            header: FeishuEventHeader {
                event_id: "evt_test".to_string(),
                event_type: "im.message.receive_v1".to_string(),
                create_time: "1234567890".to_string(),
                token: "test_token".to_string(),
                app_id: "cli_test".to_string(),
                tenant_key: "tenant_test".to_string(),
            },
            event: FeishuEventBody {
                sender: FeishuSender {
                    sender_id: FeishuSenderId {
                        open_id: user_id.to_string(),
                    },
                    sender_type: Some("user".to_string()),
                },
                message: FeishuMessage {
                    message_id: msg_id.to_string(),
                    chat_id: chat_id.to_string(),
                    chat_type: "group".to_string(),
                    message_type: "text".to_string(),
                    content: format!("{{\"text\":\"{text}\"}}"),
                    mentions: None,
                },
            },
        }
    }
}
