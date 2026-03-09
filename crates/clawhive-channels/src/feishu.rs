use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;
use clawhive_bus::{EventBus, Topic};
use clawhive_gateway::Gateway;
use clawhive_schema::{BusMessage, InboundMessage};
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use reqwest;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use uuid::Uuid;

// ============================================================
// Protobuf Frame Codec (matches Feishu Go SDK pbbp2.proto)
// ============================================================

pub const FRAME_METHOD_CONTROL: i32 = 0; // Ping/Pong
pub const FRAME_METHOD_DATA: i32 = 1; // Event/Card

#[derive(Clone, PartialEq, Message)]
pub struct Frame {
    #[prost(uint64, tag = "1")]
    pub seq_id: u64,
    #[prost(uint64, tag = "2")]
    pub log_id: u64,
    #[prost(int32, tag = "3")]
    pub service: i32,
    #[prost(int32, tag = "4")]
    pub method: i32,
    #[prost(message, repeated, tag = "5")]
    pub headers: Vec<FrameHeader>,
    #[prost(string, tag = "6")]
    pub payload_encoding: String,
    #[prost(string, tag = "7")]
    pub payload_type: String,
    #[prost(bytes = "vec", tag = "8")]
    pub payload: Vec<u8>,
    #[prost(string, tag = "9")]
    pub log_id_new: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct FrameHeader {
    #[prost(string, tag = "1")]
    pub key: String,
    #[prost(string, tag = "2")]
    pub value: String,
}

impl Frame {
    pub fn get_header(&self, key: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|h| h.key == key)
            .map(|h| h.value.as_str())
    }

    pub fn set_header(&mut self, key: impl Into<String>, value: impl Into<String>) {
        let key = key.into();
        if let Some(h) = self.headers.iter_mut().find(|h| h.key == key) {
            h.value = value.into();
        } else {
            self.headers.push(FrameHeader {
                key,
                value: value.into(),
            });
        }
    }
}

pub fn new_ping_frame(service_id: i32) -> Frame {
    Frame {
        seq_id: 0,
        log_id: 0,
        service: service_id,
        method: FRAME_METHOD_CONTROL,
        headers: vec![FrameHeader {
            key: "type".to_string(),
            value: "ping".to_string(),
        }],
        payload_encoding: String::new(),
        payload_type: String::new(),
        payload: vec![],
        log_id_new: String::new(),
    }
}

// ============================================================
// Feishu Event Types (JSON inside Frame.payload)
// ============================================================

#[derive(Debug, Deserialize)]
pub struct FeishuEvent {
    pub schema: Option<String>,
    pub header: FeishuEventHeader,
    pub event: FeishuEventBody,
}

#[derive(Debug, Deserialize)]
pub struct FeishuEventHeader {
    pub event_id: String,
    pub event_type: String,
    pub create_time: String,
    pub token: String,
    pub app_id: String,
    pub tenant_key: String,
}

#[derive(Debug, Deserialize)]
pub struct FeishuEventBody {
    pub sender: FeishuSender,
    pub message: FeishuMessage,
}

#[derive(Debug, Deserialize)]
pub struct FeishuSender {
    pub sender_id: FeishuSenderId,
    pub sender_type: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct FeishuSenderId {
    pub open_id: String,
}

#[derive(Debug, Deserialize)]
pub struct FeishuMessage {
    pub message_id: String,
    pub chat_id: String,
    pub chat_type: String,
    pub message_type: String,
    pub content: String,
    #[serde(default)]
    pub mentions: Option<Vec<FeishuMention>>,
}

#[derive(Debug, Deserialize)]
pub struct FeishuMention {
    pub key: String,
    pub id: FeishuSenderId,
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct FeishuTextContent {
    pub text: String,
}

/// ACK response sent back via WebSocket
#[derive(Debug, Serialize)]
pub struct FeishuAckResponse {
    pub code: i32,
    pub headers: std::collections::HashMap<String, String>,
    pub data: Option<serde_json::Value>,
}

// ============================================================
// Adapter
// ============================================================

pub struct FeishuAdapter {
    connector_id: String,
}

impl FeishuAdapter {
    pub fn new(connector_id: impl Into<String>) -> Self {
        Self {
            connector_id: connector_id.into(),
        }
    }

    pub fn to_inbound(&self, event: &FeishuEvent) -> InboundMessage {
        let text = match event.event.message.message_type.as_str() {
            "text" => serde_json::from_str::<FeishuTextContent>(&event.event.message.content)
                .map(|c| c.text)
                .unwrap_or_default(),
            _ => event.event.message.content.clone(),
        };

        let is_mention = event
            .event
            .message
            .mentions
            .as_ref()
            .map(|m| !m.is_empty())
            .unwrap_or(false);

        InboundMessage {
            trace_id: Uuid::new_v4(),
            channel_type: "feishu".to_string(),
            connector_id: self.connector_id.clone(),
            conversation_scope: format!("chat:{}", event.event.message.chat_id),
            user_scope: format!("user:{}", event.event.sender.sender_id.open_id),
            text,
            at: Utc::now(),
            thread_id: None,
            is_mention,
            mention_target: None,
            message_id: Some(event.event.message.message_id.clone()),
            attachments: vec![],
            group_context: None,
        }
    }
}

const FEISHU_BASE_URL: &str = "https://open.feishu.cn/open-apis";
const FEISHU_WS_ENDPOINT: &str = "https://open.feishu.cn/callback/ws/endpoint";

#[derive(Debug, Deserialize)]
struct WsEndpointResponse {
    code: i32,
    #[serde(default)]
    msg: String,
    data: Option<WsEndpointData>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct WsEndpointData {
    #[serde(rename = "URL")]
    url: String,
    client_config: Option<FeishuClientConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct FeishuClientConfig {
    #[serde(default = "default_reconnect_count")]
    pub reconnect_count: i32,
    #[serde(default = "default_reconnect_interval")]
    pub reconnect_interval: u64,
    #[serde(default = "default_reconnect_nonce")]
    pub reconnect_nonce: u64,
    #[serde(default = "default_ping_interval")]
    pub ping_interval: u64,
}

fn default_reconnect_count() -> i32 {
    -1
}
fn default_reconnect_interval() -> u64 {
    120
}
fn default_reconnect_nonce() -> u64 {
    30
}
fn default_ping_interval() -> u64 {
    120
}

impl Default for FeishuClientConfig {
    fn default() -> Self {
        Self {
            reconnect_count: -1,
            reconnect_interval: 120,
            reconnect_nonce: 30,
            ping_interval: 120,
        }
    }
}

pub struct FeishuClient {
    app_id: String,
    app_secret: String,
    token: Arc<RwLock<String>>,
    http: reqwest::Client,
}

impl FeishuClient {
    pub fn new(app_id: impl Into<String>, app_secret: impl Into<String>) -> Self {
        Self {
            app_id: app_id.into(),
            app_secret: app_secret.into(),
            token: Arc::new(RwLock::new(String::new())),
            http: reqwest::Client::new(),
        }
    }

    pub async fn get_ws_endpoint(&self) -> Result<(String, FeishuClientConfig)> {
        let resp = self
            .http
            .post(FEISHU_WS_ENDPOINT)
            .header("locale", "zh")
            .json(&serde_json::json!({
                "AppID": self.app_id,
                "AppSecret": self.app_secret,
            }))
            .send()
            .await?
            .json::<WsEndpointResponse>()
            .await?;

        if resp.code != 0 {
            anyhow::bail!(
                "feishu: failed to get ws endpoint: code={}, msg={}",
                resp.code,
                resp.msg
            );
        }

        let data = resp
            .data
            .ok_or_else(|| anyhow::anyhow!("feishu: ws endpoint response missing data"))?;
        let config = data.client_config.unwrap_or_default();
        Ok((data.url, config))
    }

    pub async fn refresh_token(&self) -> Result<()> {
        let resp = self
            .http
            .post(format!(
                "{FEISHU_BASE_URL}/auth/v3/tenant_access_token/internal"
            ))
            .json(&serde_json::json!({
                "app_id": self.app_id,
                "app_secret": self.app_secret,
            }))
            .send()
            .await?
            .json::<serde_json::Value>()
            .await?;

        let token = resp["tenant_access_token"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("feishu: missing tenant_access_token"))?;

        let mut guard = self.token.write().await;
        *guard = token.to_string();

        tracing::info!(
            target: "clawhive::channel::feishu",
            "refreshed tenant_access_token"
        );
        Ok(())
    }

    pub async fn send_message(&self, chat_id: &str, msg_type: &str, content: &str) -> Result<()> {
        let token = self.token.read().await.clone();
        let resp = self
            .http
            .post(format!(
                "{FEISHU_BASE_URL}/im/v1/messages?receive_id_type=chat_id"
            ))
            .header("Authorization", format!("Bearer {token}"))
            .json(&serde_json::json!({
                "receive_id": chat_id,
                "msg_type": msg_type,
                "content": content,
            }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("feishu: send_message failed: {body}");
        }
        Ok(())
    }

    pub async fn reply_message(
        &self,
        message_id: &str,
        msg_type: &str,
        content: &str,
    ) -> Result<()> {
        let token = self.token.read().await.clone();
        let resp = self
            .http
            .post(format!(
                "{FEISHU_BASE_URL}/im/v1/messages/{message_id}/reply"
            ))
            .header("Authorization", format!("Bearer {token}"))
            .json(&serde_json::json!({
                "msg_type": msg_type,
                "content": content,
            }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("feishu: reply_message failed: {body}");
        }
        Ok(())
    }

    pub fn spawn_token_refresh(self: &Arc<Self>) {
        let client = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                if let Err(e) = client.refresh_token().await {
                    tracing::error!(
                        target: "clawhive::channel::feishu",
                        error = %e,
                        "failed to refresh tenant_access_token"
                    );
                }
                tokio::time::sleep(std::time::Duration::from_secs(100 * 60)).await;
            }
        });
    }
}

pub struct FeishuBot {
    app_id: String,
    app_secret: String,
    connector_id: String,
    gateway: Arc<Gateway>,
    bus: Arc<EventBus>,
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
        }
    }

    async fn run_impl(self) -> Result<()> {
        let client = Arc::new(FeishuClient::new(&self.app_id, &self.app_secret));

        client.refresh_token().await?;
        client.spawn_token_refresh();

        let adapter = Arc::new(FeishuAdapter::new(&self.connector_id));

        Self::spawn_delivery_listener(self.bus.clone(), client.clone(), self.connector_id.clone());

        tracing::info!(
            target: "clawhive::channel::feishu",
            connector_id = %self.connector_id,
            "feishu bot starting WebSocket connection"
        );

        loop {
            match self.connect_and_listen(&client, &adapter).await {
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

        tracing::info!(
            target: "clawhive::channel::feishu",
            "feishu WebSocket connected"
        );

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
                                let ack = self.handle_frame(
                                    &frame, adapter, client,
                                ).await;
                                if let Some(ack_bytes) = ack {
                                    if let Err(e) = write.send(WsMessage::Binary(ack_bytes.into())).await {
                                        tracing::error!(
                                            target: "clawhive::channel::feishu",
                                            error = %e,
                                            "failed to send ACK"
                                        );
                                        break;
                                    }
                                }
                            }
                        }
                        Some(Ok(WsMessage::Close(_))) | None => {
                            tracing::info!(
                                target: "clawhive::channel::feishu",
                                "WebSocket closed"
                            );
                            break;
                        }
                        Some(Err(e)) => {
                            tracing::error!(
                                target: "clawhive::channel::feishu",
                                error = %e,
                                "WebSocket read error"
                            );
                            break;
                        }
                        _ => {}
                    }
                }
                Some(ping_data) = ping_rx.recv() => {
                    if let Err(e) = write.send(WsMessage::Binary(ping_data.into())).await {
                        tracing::error!(
                            target: "clawhive::channel::feishu",
                            error = %e,
                            "failed to send ping"
                        );
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
        adapter: &FeishuAdapter,
        client: &Arc<FeishuClient>,
    ) -> Option<Vec<u8>> {
        let frame_type = frame.get_header("type").unwrap_or("");

        match (frame.method, frame_type) {
            (FRAME_METHOD_CONTROL, "pong") => {
                tracing::trace!(target: "clawhive::channel::feishu", "received pong");
                None
            }
            (FRAME_METHOD_DATA, "event") => {
                match serde_json::from_slice::<FeishuEvent>(&frame.payload) {
                    Ok(event) => {
                        if event.header.event_type == "im.message.receive_v1" {
                            let inbound = adapter.to_inbound(&event);
                            let gw = self.gateway.clone();
                            let client = client.clone();
                            let _chat_id = event.event.message.chat_id.clone();
                            let message_id = event.event.message.message_id.clone();
                            tokio::spawn(async move {
                                match gw.handle_inbound(inbound).await {
                                    Ok(outbound) => {
                                        if !outbound.text.trim().is_empty() {
                                            let content =
                                                serde_json::json!({"text": outbound.text})
                                                    .to_string();
                                            let result = if let Some(ref reply_to) =
                                                outbound.reply_to
                                            {
                                                client
                                                    .reply_message(reply_to, "text", &content)
                                                    .await
                                            } else {
                                                client
                                                    .reply_message(&message_id, "text", &content)
                                                    .await
                                            };
                                            if let Err(e) = result {
                                                tracing::error!(
                                                    target: "clawhive::channel::feishu",
                                                    error = %e,
                                                    "failed to send feishu reply"
                                                );
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        tracing::error!(
                                            target: "clawhive::channel::feishu",
                                            error = %e,
                                            "failed to handle inbound"
                                        );
                                    }
                                }
                            });
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "clawhive::channel::feishu",
                            error = %e,
                            "failed to parse event payload"
                        );
                    }
                }
                Some(self.build_ack(frame, 200))
            }
            _ => {
                tracing::debug!(
                    target: "clawhive::channel::feishu",
                    method = frame.method,
                    frame_type = frame_type,
                    "ignoring unknown frame"
                );
                None
            }
        }
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

    fn spawn_delivery_listener(
        bus: Arc<EventBus>,
        client: Arc<FeishuClient>,
        connector_id: String,
    ) {
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

                if channel_type != "feishu" || msg_connector_id != connector_id {
                    continue;
                }

                let chat_id = conversation_scope.trim_start_matches("chat:");
                let content = serde_json::json!({"text": text}).to_string();

                if let Err(e) = client.send_message(chat_id, "text", &content).await {
                    tracing::error!(
                        target: "clawhive::channel::feishu",
                        error = %e,
                        "failed to deliver announce message"
                    );
                }
            }
        });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip() {
        let frame = Frame {
            seq_id: 1,
            log_id: 0,
            service: 100,
            method: FRAME_METHOD_DATA,
            headers: vec![
                FrameHeader {
                    key: "type".to_string(),
                    value: "event".to_string(),
                },
                FrameHeader {
                    key: "message_id".to_string(),
                    value: "msg_001".to_string(),
                },
            ],
            payload_encoding: String::new(),
            payload_type: String::new(),
            payload: br#"{"test": true}"#.to_vec(),
            log_id_new: String::new(),
        };

        let mut buf = Vec::new();
        frame.encode(&mut buf).unwrap();
        assert!(!buf.is_empty());

        let decoded = Frame::decode(&buf[..]).unwrap();
        assert_eq!(decoded.seq_id, 1);
        assert_eq!(decoded.method, FRAME_METHOD_DATA);
        assert_eq!(decoded.headers.len(), 2);
        assert_eq!(decoded.headers[0].key, "type");
        assert_eq!(decoded.headers[0].value, "event");
        assert_eq!(decoded.payload, br#"{"test": true}"#);
    }

    #[test]
    fn ping_frame_has_correct_structure() {
        let frame = new_ping_frame(42);
        assert_eq!(frame.method, FRAME_METHOD_CONTROL);
        assert_eq!(frame.service, 42);
        let type_header = frame.headers.iter().find(|h| h.key == "type").unwrap();
        assert_eq!(type_header.value, "ping");
    }

    #[test]
    fn frame_get_header() {
        let frame = Frame {
            seq_id: 0,
            log_id: 0,
            service: 0,
            method: FRAME_METHOD_DATA,
            headers: vec![FrameHeader {
                key: "type".to_string(),
                value: "event".to_string(),
            }],
            payload_encoding: String::new(),
            payload_type: String::new(),
            payload: vec![],
            log_id_new: String::new(),
        };
        assert_eq!(frame.get_header("type"), Some("event"));
        assert_eq!(frame.get_header("missing"), None);
    }

    #[test]
    fn parse_message_event_payload() {
        let payload = r#"{
            "schema": "2.0",
            "header": {
                "event_id": "evt_001",
                "event_type": "im.message.receive_v1",
                "create_time": "1609296809",
                "token": "test_token",
                "app_id": "cli_xxx",
                "tenant_key": "tenant_xxx"
            },
            "event": {
                "sender": {
                    "sender_id": {"open_id": "ou_test"},
                    "sender_type": "user"
                },
                "message": {
                    "message_id": "om_test",
                    "chat_id": "oc_test",
                    "chat_type": "group",
                    "message_type": "text",
                    "content": "{\"text\":\"hello\"}"
                }
            }
        }"#;
        let event: FeishuEvent = serde_json::from_str(payload).unwrap();
        assert_eq!(event.header.event_type, "im.message.receive_v1");
        assert_eq!(event.event.message.chat_id, "oc_test");
        let content: FeishuTextContent =
            serde_json::from_str(&event.event.message.content).unwrap();
        assert_eq!(content.text, "hello");
    }

    #[test]
    fn adapter_to_inbound_converts_correctly() {
        let adapter = FeishuAdapter::new("feishu-main");
        let event = make_test_event("oc_chat1", "ou_user1", "om_msg1", "hello world");
        let inbound = adapter.to_inbound(&event);
        assert_eq!(inbound.channel_type, "feishu");
        assert_eq!(inbound.connector_id, "feishu-main");
        assert_eq!(inbound.conversation_scope, "chat:oc_chat1");
        assert_eq!(inbound.user_scope, "user:ou_user1");
        assert_eq!(inbound.text, "hello world");
        assert_eq!(inbound.message_id, Some("om_msg1".to_string()));
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
