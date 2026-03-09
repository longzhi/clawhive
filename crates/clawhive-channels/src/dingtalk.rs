use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use clawhive_bus::{EventBus, Topic};
use clawhive_gateway::Gateway;
use clawhive_schema::{BusMessage, InboundMessage};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use uuid::Uuid;

const DINGTALK_GATEWAY_URL: &str = "https://api.dingtalk.com/v1.0/gateway/connections/open";
const BOT_MESSAGES_TOPIC: &str = "/v1.0/im/bot/messages/get";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DingTalkDataFrame {
    pub spec_version: Option<String>,
    #[serde(rename = "type")]
    pub frame_type: String,
    #[serde(default)]
    pub time: Option<i64>,
    pub headers: HashMap<String, String>,
    pub data: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DingTalkAckResponse {
    pub code: i32,
    pub headers: HashMap<String, String>,
    pub message: String,
    pub data: String,
}

impl DingTalkAckResponse {
    pub fn success(message_id: &str) -> Self {
        let mut headers = HashMap::new();
        headers.insert("messageId".to_string(), message_id.to_string());
        headers.insert("contentType".to_string(), "application/json".to_string());
        Self {
            code: 200,
            headers,
            message: "OK".to_string(),
            data: r#"{"status":"SUCCESS","message":"success"}"#.to_string(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DingTalkBotCallback {
    pub conversation_id: String,
    pub msg_id: String,
    pub sender_nick: Option<String>,
    pub sender_staff_id: Option<String>,
    pub conversation_type: String,
    pub sender_id: String,
    pub text: DingTalkText,
    pub msgtype: String,
    #[serde(default)]
    pub is_in_at_list: Option<bool>,
    pub session_webhook: Option<String>,
    pub session_webhook_expired_time: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct DingTalkText {
    pub content: String,
}

pub struct DingTalkAdapter {
    connector_id: String,
}

impl DingTalkAdapter {
    pub fn new(connector_id: impl Into<String>) -> Self {
        Self {
            connector_id: connector_id.into(),
        }
    }

    pub fn to_inbound(&self, msg: &DingTalkBotCallback) -> InboundMessage {
        let is_mention = msg.is_in_at_list.unwrap_or(false);

        InboundMessage {
            trace_id: Uuid::new_v4(),
            channel_type: "dingtalk".to_string(),
            connector_id: self.connector_id.clone(),
            conversation_scope: format!("conversation:{}", msg.conversation_id),
            user_scope: format!(
                "user:{}",
                msg.sender_staff_id.as_deref().unwrap_or(&msg.sender_id)
            ),
            text: msg.text.content.trim().to_string(),
            at: Utc::now(),
            thread_id: None,
            is_mention,
            mention_target: None,
            message_id: Some(msg.msg_id.clone()),
            attachments: vec![],
            group_context: None,
        }
    }
}

pub struct DingTalkClient {
    client_id: String,
    client_secret: String,
    http: reqwest::Client,
    session_webhooks: Arc<RwLock<HashMap<String, (String, i64)>>>,
}

#[derive(Debug, Deserialize)]
pub struct ConnectionResponse {
    pub endpoint: String,
    pub ticket: String,
}

impl DingTalkClient {
    pub fn new(client_id: impl Into<String>, client_secret: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            http: reqwest::Client::new(),
            session_webhooks: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn open_connection(&self) -> anyhow::Result<ConnectionResponse> {
        let resp = self
            .http
            .post(DINGTALK_GATEWAY_URL)
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({
                "clientId": self.client_id,
                "clientSecret": self.client_secret,
                "subscriptions": [
                    {"type": "EVENT", "topic": "*"},
                    {"type": "CALLBACK", "topic": BOT_MESSAGES_TOPIC}
                ],
                "ua": "clawhive-dingtalk/0.1.0"
            }))
            .send()
            .await?
            .json::<ConnectionResponse>()
            .await?;
        Ok(resp)
    }

    pub async fn cache_session_webhook(
        &self,
        conversation_id: &str,
        webhook: &str,
        expired_time: Option<i64>,
    ) {
        // Default to 90 minutes from now if no expiry provided
        let expiry = expired_time.unwrap_or_else(|| Utc::now().timestamp_millis() + 90 * 60 * 1000);
        let mut map = self.session_webhooks.write().await;
        map.insert(conversation_id.to_string(), (webhook.to_string(), expiry));
    }

    pub async fn reply_via_session_webhook(
        &self,
        conversation_id: &str,
        text: &str,
    ) -> anyhow::Result<()> {
        let entry = {
            let map = self.session_webhooks.read().await;
            map.get(conversation_id).cloned()
        };

        let (webhook, expired_time) = entry
            .ok_or_else(|| anyhow::anyhow!("dingtalk: no session webhook for {conversation_id}"))?;

        if Utc::now().timestamp_millis() > expired_time {
            // Remove stale entry
            self.session_webhooks.write().await.remove(conversation_id);
            anyhow::bail!(
                "dingtalk: session webhook expired for {conversation_id}, \
                 will use fresh webhook from next message"
            );
        }

        self.http
            .post(&webhook)
            .json(&serde_json::json!({
                "msgtype": "text",
                "text": { "content": text }
            }))
            .send()
            .await?;

        Ok(())
    }
}

pub struct DingTalkBot {
    client_id: String,
    client_secret: String,
    connector_id: String,
    gateway: Arc<Gateway>,
    bus: Arc<EventBus>,
}

impl DingTalkBot {
    pub fn new(
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
        connector_id: impl Into<String>,
        gateway: Arc<Gateway>,
        bus: Arc<EventBus>,
    ) -> Self {
        Self {
            client_id: client_id.into(),
            client_secret: client_secret.into(),
            connector_id: connector_id.into(),
            gateway,
            bus,
        }
    }

    async fn run_impl(self) -> anyhow::Result<()> {
        let client = Arc::new(DingTalkClient::new(&self.client_id, &self.client_secret));
        let adapter = Arc::new(DingTalkAdapter::new(&self.connector_id));

        Self::spawn_delivery_listener(self.bus.clone(), client.clone(), self.connector_id.clone());

        tracing::info!(
            target: "clawhive::channel::dingtalk",
            connector_id = %self.connector_id,
            "dingtalk bot starting Stream mode connection"
        );

        loop {
            match self.connect_and_listen(&client, &adapter).await {
                Ok(()) => {
                    tracing::info!(
                        target: "clawhive::channel::dingtalk",
                        "dingtalk Stream disconnected, reconnecting in 3s..."
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                }
                Err(e) => {
                    tracing::error!(
                        target: "clawhive::channel::dingtalk",
                        error = %e,
                        "dingtalk Stream error, reconnecting in 5s..."
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        }
    }

    async fn connect_and_listen(
        &self,
        client: &Arc<DingTalkClient>,
        adapter: &Arc<DingTalkAdapter>,
    ) -> anyhow::Result<()> {
        let conn = client.open_connection().await?;
        let wss_url = format!("{}?ticket={}", conn.endpoint, conn.ticket);

        let (ws_stream, _) = tokio_tungstenite::connect_async(&wss_url).await?;
        let (mut write, mut read) = ws_stream.split();

        tracing::info!(
            target: "clawhive::channel::dingtalk",
            "dingtalk Stream WebSocket connected"
        );

        let (ping_tx, mut ping_rx) = tokio::sync::mpsc::channel::<()>(1);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(120)).await;
                if ping_tx.send(()).await.is_err() {
                    break;
                }
            }
        });

        loop {
            tokio::select! {
                msg = read.next() => {
                    match msg {
                        Some(Ok(WsMessage::Text(text))) => {
                            match serde_json::from_str::<DingTalkDataFrame>(&text) {
                                Ok(frame) => {
                                    let ack = self.handle_frame(&frame, client, adapter).await;
                                    if let Some(ack_json) = ack {
                                        if let Err(e) = write.send(WsMessage::Text(ack_json.into())).await {
                                            tracing::error!(
                                                target: "clawhive::channel::dingtalk",
                                                error = %e,
                                                "failed to send ACK"
                                            );
                                            break;
                                        }
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(
                                        target: "clawhive::channel::dingtalk",
                                        error = %e,
                                        "failed to parse Stream frame"
                                    );
                                }
                            }
                        }
                        Some(Ok(WsMessage::Close(_))) | None => {
                            tracing::info!(
                                target: "clawhive::channel::dingtalk",
                                "WebSocket closed"
                            );
                            break;
                        }
                        Some(Err(e)) => {
                            tracing::error!(
                                target: "clawhive::channel::dingtalk",
                                error = %e,
                                "WebSocket read error"
                            );
                            break;
                        }
                        _ => {}
                    }
                }
                Some(()) = ping_rx.recv() => {
                    if let Err(e) = write.send(WsMessage::Ping(vec![].into())).await {
                        tracing::error!(
                            target: "clawhive::channel::dingtalk",
                            error = %e,
                            "failed to send WebSocket ping"
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
        frame: &DingTalkDataFrame,
        client: &Arc<DingTalkClient>,
        adapter: &DingTalkAdapter,
    ) -> Option<String> {
        let topic = frame.headers.get("topic").map(|s| s.as_str()).unwrap_or("");
        let message_id = frame.headers.get("messageId").cloned().unwrap_or_default();

        match (frame.frame_type.as_str(), topic) {
            ("SYSTEM", "ping") => {
                let mut ack = DingTalkAckResponse::success(&message_id);
                ack.data = frame.data.clone();
                serde_json::to_string(&ack).ok()
            }
            ("SYSTEM", "disconnect") => {
                tracing::info!(
                    target: "clawhive::channel::dingtalk",
                    "received disconnect signal"
                );
                None
            }
            ("CALLBACK", t) if t == BOT_MESSAGES_TOPIC => {
                match serde_json::from_str::<DingTalkBotCallback>(&frame.data) {
                    Ok(callback) => {
                        if let Some(ref webhook) = callback.session_webhook {
                            client
                                .cache_session_webhook(
                                    &callback.conversation_id,
                                    webhook,
                                    callback.session_webhook_expired_time,
                                )
                                .await;
                        }

                        let inbound = adapter.to_inbound(&callback);
                        let gw = self.gateway.clone();
                        let client = client.clone();
                        let conv_id = callback.conversation_id.clone();
                        tokio::spawn(async move {
                            match gw.handle_inbound(inbound).await {
                                Ok(outbound) => {
                                    if !outbound.text.trim().is_empty() {
                                        if let Err(e) = client
                                            .reply_via_session_webhook(&conv_id, &outbound.text)
                                            .await
                                        {
                                            tracing::error!(
                                                target: "clawhive::channel::dingtalk",
                                                error = %e,
                                                "failed to send dingtalk reply"
                                            );
                                        }
                                    }
                                }
                                Err(e) => {
                                    tracing::error!(
                                        target: "clawhive::channel::dingtalk",
                                        error = %e,
                                        "failed to handle inbound"
                                    );
                                }
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "clawhive::channel::dingtalk",
                            error = %e,
                            "failed to parse bot callback data"
                        );
                    }
                }
                let ack = DingTalkAckResponse::success(&message_id);
                serde_json::to_string(&ack).ok()
            }
            _ => {
                tracing::debug!(
                    target: "clawhive::channel::dingtalk",
                    frame_type = %frame.frame_type,
                    topic = topic,
                    "ignoring unknown frame"
                );
                let ack = DingTalkAckResponse::success(&message_id);
                serde_json::to_string(&ack).ok()
            }
        }
    }

    fn spawn_delivery_listener(
        bus: Arc<EventBus>,
        client: Arc<DingTalkClient>,
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

                if channel_type != "dingtalk" || msg_connector_id != connector_id {
                    continue;
                }

                let conv_id = conversation_scope.trim_start_matches("conversation:");
                if let Err(e) = client.reply_via_session_webhook(conv_id, &text).await {
                    tracing::error!(
                        target: "clawhive::channel::dingtalk",
                        error = %e,
                        "failed to deliver announce message"
                    );
                }
            }
        });
    }
}

#[async_trait::async_trait]
impl crate::ChannelBot for DingTalkBot {
    fn channel_type(&self) -> &str {
        "dingtalk"
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
    fn parse_stream_data_frame() {
        let json = r#"{
            "specVersion": "1.0",
            "type": "CALLBACK",
            "time": 1690362102194,
            "headers": {
                "appId": "app_test",
                "contentType": "application/json",
                "messageId": "msg_001",
                "time": "1690362102194",
                "topic": "/v1.0/im/bot/messages/get"
            },
            "data": "{\"conversationId\":\"cid_test\",\"msgId\":\"msg_test\",\"senderNick\":\"张三\",\"senderStaffId\":\"user123\",\"conversationType\":\"2\",\"senderId\":\"sender_test\",\"text\":{\"content\":\"hello\"},\"msgtype\":\"text\",\"sessionWebhook\":\"https://oapi.dingtalk.com/robot/sendBySession?session=xxx\",\"sessionWebhookExpiredTime\":1690367502152}"
        }"#;
        let frame: DingTalkDataFrame = serde_json::from_str(json).unwrap();
        assert_eq!(frame.frame_type, "CALLBACK");
        assert_eq!(frame.headers["topic"], "/v1.0/im/bot/messages/get");

        let callback: DingTalkBotCallback = serde_json::from_str(&frame.data).unwrap();
        assert_eq!(callback.conversation_id, "cid_test");
        assert_eq!(callback.text.content, "hello");
        assert_eq!(callback.conversation_type, "2");
        assert!(callback.session_webhook.is_some());
    }

    #[test]
    fn parse_system_ping() {
        let json = r#"{
            "specVersion": "1.0",
            "type": "SYSTEM",
            "headers": {
                "topic": "ping",
                "messageId": "sys_001",
                "contentType": "application/json"
            },
            "data": "{\"opaque\": \"123-dsfs\"}"
        }"#;
        let frame: DingTalkDataFrame = serde_json::from_str(json).unwrap();
        assert_eq!(frame.frame_type, "SYSTEM");
        assert_eq!(frame.headers["topic"], "ping");
    }

    #[test]
    fn adapter_to_inbound_converts_correctly() {
        let adapter = DingTalkAdapter::new("dingtalk-main");
        let callback = make_test_callback("cid_1", "user1", "msg_1", "hello world");
        let inbound = adapter.to_inbound(&callback);
        assert_eq!(inbound.channel_type, "dingtalk");
        assert_eq!(inbound.connector_id, "dingtalk-main");
        assert_eq!(inbound.conversation_scope, "conversation:cid_1");
        assert_eq!(inbound.user_scope, "user:user1");
        assert_eq!(inbound.text, "hello world");
    }

    #[test]
    fn ack_response_serializes_correctly() {
        let ack = DingTalkAckResponse::success("msg_001");
        let json = serde_json::to_string(&ack).unwrap();
        assert!(json.contains("\"code\":200"));
        assert!(json.contains("msg_001"));
    }

    fn make_test_callback(
        conv_id: &str,
        user_id: &str,
        msg_id: &str,
        text: &str,
    ) -> DingTalkBotCallback {
        DingTalkBotCallback {
            conversation_id: conv_id.to_string(),
            msg_id: msg_id.to_string(),
            sender_nick: Some("Test User".to_string()),
            sender_staff_id: Some(user_id.to_string()),
            conversation_type: "2".to_string(),
            sender_id: user_id.to_string(),
            text: DingTalkText {
                content: text.to_string(),
            },
            msgtype: "text".to_string(),
            is_in_at_list: Some(true),
            session_webhook: Some(
                "https://oapi.dingtalk.com/robot/sendBySession?session=xxx".to_string(),
            ),
            session_webhook_expired_time: Some(1690367502152),
        }
    }
}
