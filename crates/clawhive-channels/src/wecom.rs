use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::Utc;
use clawhive_bus::{EventBus, Topic};
use clawhive_gateway::Gateway;
use clawhive_schema::{BusMessage, InboundMessage};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use uuid::Uuid;

const WECOM_WSS_URL: &str = "wss://openws.work.weixin.qq.com";

#[derive(Debug, Deserialize)]
pub struct WeComMessage {
    #[serde(default)]
    pub cmd: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub body: Option<serde_json::Value>,
    #[serde(default)]
    pub errcode: Option<i32>,
    #[serde(default)]
    pub errmsg: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WeComMsgBody {
    pub msgid: String,
    pub aibotid: String,
    pub chatid: Option<String>,
    pub chattype: Option<String>,
    pub from: WeComFrom,
    pub msgtype: String,
    pub text: Option<WeComText>,
}

#[derive(Debug, Deserialize)]
pub struct WeComFrom {
    pub userid: String,
}

#[derive(Debug, Deserialize)]
pub struct WeComText {
    pub content: String,
}

#[derive(Debug, Serialize)]
pub struct WeComSubscribe {
    pub cmd: &'static str,
    pub headers: HashMap<String, String>,
    pub body: WeComSubscribeBody,
}

#[derive(Debug, Serialize)]
pub struct WeComSubscribeBody {
    pub bot_id: String,
    pub secret: String,
}

#[derive(Debug, Serialize)]
pub struct WeComReplyMessage {
    pub cmd: &'static str,
    pub headers: HashMap<String, String>,
    pub body: WeComReplyBody,
}

#[derive(Debug, Serialize)]
pub struct WeComReplyBody {
    pub msgid: String,
    pub msgtype: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<WeComReplyText>,
    pub finish: bool,
}

#[derive(Debug, Serialize)]
pub struct WeComReplyText {
    pub content: String,
}

impl WeComReplyMessage {
    pub fn text(req_id: &str, msgid: &str, content: &str, finish: bool) -> Self {
        let mut headers = HashMap::new();
        headers.insert("req_id".to_string(), req_id.to_string());
        Self {
            cmd: "aibot_respond_msg",
            headers,
            body: WeComReplyBody {
                msgid: msgid.to_string(),
                msgtype: "text".to_string(),
                text: Some(WeComReplyText {
                    content: content.to_string(),
                }),
                finish,
            },
        }
    }
}

#[derive(Debug, Serialize)]
pub struct WeComPing {
    pub cmd: &'static str,
    pub headers: HashMap<String, String>,
}

pub struct WeComAdapter {
    connector_id: String,
}

impl WeComAdapter {
    pub fn new(connector_id: impl Into<String>) -> Self {
        Self {
            connector_id: connector_id.into(),
        }
    }

    pub fn to_inbound(&self, body: &WeComMsgBody, _req_id: &str) -> InboundMessage {
        let text = body
            .text
            .as_ref()
            .map(|t| t.content.clone())
            .unwrap_or_default();

        let conversation_scope = match body.chattype.as_deref() {
            Some("group") => format!("chat:{}", body.chatid.as_deref().unwrap_or("")),
            _ => format!("dm:{}", body.from.userid),
        };

        InboundMessage {
            trace_id: Uuid::new_v4(),
            channel_type: "wecom".to_string(),
            connector_id: self.connector_id.clone(),
            conversation_scope,
            user_scope: format!("user:{}", body.from.userid),
            text,
            at: Utc::now(),
            thread_id: None,
            is_mention: body.chattype.as_deref() == Some("group"),
            mention_target: None,
            message_id: Some(body.msgid.clone()),
            attachments: vec![],
            group_context: None,
        }
    }
}

type WsSink = futures_util::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    WsMessage,
>;

pub struct WeComBot {
    bot_id: String,
    secret: String,
    connector_id: String,
    gateway: Arc<Gateway>,
    bus: Arc<EventBus>,
}

impl WeComBot {
    pub fn new(
        bot_id: impl Into<String>,
        secret: impl Into<String>,
        connector_id: impl Into<String>,
        gateway: Arc<Gateway>,
        bus: Arc<EventBus>,
    ) -> Self {
        Self {
            bot_id: bot_id.into(),
            secret: secret.into(),
            connector_id: connector_id.into(),
            gateway,
            bus,
        }
    }

    async fn run_impl(self) -> anyhow::Result<()> {
        let adapter = Arc::new(WeComAdapter::new(&self.connector_id));

        tracing::info!(
            target: "clawhive::channel::wecom",
            connector_id = %self.connector_id,
            "wecom AI Bot starting WebSocket connection"
        );

        loop {
            match self.connect_and_listen(&adapter).await {
                Ok(()) => {
                    tracing::info!(
                        target: "clawhive::channel::wecom",
                        "wecom WebSocket disconnected, reconnecting in 3s..."
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                }
                Err(e) => {
                    tracing::error!(
                        target: "clawhive::channel::wecom",
                        error = %e,
                        "wecom WebSocket error, reconnecting in 5s..."
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                }
            }
        }
    }

    async fn connect_and_listen(&self, adapter: &Arc<WeComAdapter>) -> anyhow::Result<()> {
        let (ws_stream, _) = tokio_tungstenite::connect_async(WECOM_WSS_URL).await?;
        let (mut write, mut read) = ws_stream.split();

        let mut sub_headers = HashMap::new();
        sub_headers.insert("req_id".to_string(), Uuid::new_v4().to_string());
        let subscribe = WeComSubscribe {
            cmd: "aibot_subscribe",
            headers: sub_headers,
            body: WeComSubscribeBody {
                bot_id: self.bot_id.clone(),
                secret: self.secret.clone(),
            },
        };
        let sub_json = serde_json::to_string(&subscribe)?;
        write.send(WsMessage::Text(sub_json.into())).await?;

        if let Some(Ok(WsMessage::Text(text))) = read.next().await {
            let resp: WeComMessage = serde_json::from_str(&text)?;
            if resp.errcode != Some(0) {
                anyhow::bail!(
                    "wecom: subscribe failed: errcode={:?}, errmsg={:?}",
                    resp.errcode,
                    resp.errmsg
                );
            }
        } else {
            anyhow::bail!("wecom: no subscribe response received");
        }

        tracing::info!(
            target: "clawhive::channel::wecom",
            "wecom AI Bot subscribed successfully"
        );

        // TODO: Consider replacing Arc<Mutex<WsSink>> with a bounded MPSC channel
        // and a dedicated writer task to avoid potential lock contention under load.
        let write = Arc::new(tokio::sync::Mutex::new(write));

        Self::spawn_delivery_listener(self.bus.clone(), write.clone(), self.connector_id.clone());

        let write_ping = write.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                let mut ping_headers = HashMap::new();
                ping_headers.insert("req_id".to_string(), Uuid::new_v4().to_string());
                let ping = WeComPing {
                    cmd: "ping",
                    headers: ping_headers,
                };
                if let Ok(json) = serde_json::to_string(&ping) {
                    let mut w = write_ping.lock().await;
                    if w.send(WsMessage::Text(json.into())).await.is_err() {
                        break;
                    }
                }
            }
        });

        // TODO: Improve dedup — currently clears entire HashSet at 10k entries.
        // Consider LRU cache or time-based expiry for more predictable behavior.
        let mut seen_msgs: HashSet<String> = HashSet::new();

        while let Some(msg) = read.next().await {
            match msg {
                Ok(WsMessage::Text(text)) => match serde_json::from_str::<WeComMessage>(&text) {
                    Ok(wc_msg) => {
                        if wc_msg.cmd == "aibot_msg_callback" {
                            if let Some(body_val) = wc_msg.body {
                                match serde_json::from_value::<WeComMsgBody>(body_val) {
                                    Ok(body) => {
                                        if seen_msgs.contains(&body.msgid) {
                                            continue;
                                        }
                                        seen_msgs.insert(body.msgid.clone());
                                        if seen_msgs.len() > 10_000 {
                                            seen_msgs.clear();
                                        }

                                        let req_id = wc_msg
                                            .headers
                                            .get("req_id")
                                            .cloned()
                                            .unwrap_or_default();
                                        let msgid = body.msgid.clone();
                                        let inbound = adapter.to_inbound(&body, &req_id);
                                        let gw = self.gateway.clone();
                                        let write_reply = write.clone();

                                        tokio::spawn(async move {
                                            match gw.handle_inbound(inbound).await {
                                                Ok(outbound) => {
                                                    if !outbound.text.trim().is_empty() {
                                                        let reply = WeComReplyMessage::text(
                                                            &req_id,
                                                            &msgid,
                                                            &outbound.text,
                                                            true,
                                                        );
                                                        match serde_json::to_string(&reply) {
                                                            Ok(json) => {
                                                                let mut w =
                                                                    write_reply.lock().await;
                                                                if let Err(e) = w
                                                                    .send(WsMessage::Text(
                                                                        json.into(),
                                                                    ))
                                                                    .await
                                                                {
                                                                    tracing::error!(
                                                                        target: "clawhive::channel::wecom",
                                                                        error = %e,
                                                                        "failed to send wecom reply"
                                                                    );
                                                                }
                                                            }
                                                            Err(e) => {
                                                                tracing::error!(
                                                                    target: "clawhive::channel::wecom",
                                                                    error = %e,
                                                                    "failed to serialize wecom reply"
                                                                );
                                                            }
                                                        }
                                                    }
                                                }
                                                Err(e) => {
                                                    tracing::error!(
                                                        target: "clawhive::channel::wecom",
                                                        error = %e,
                                                        "failed to handle inbound"
                                                    );
                                                }
                                            }
                                        });
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            target: "clawhive::channel::wecom",
                                            error = %e,
                                            "failed to parse msg body"
                                        );
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "clawhive::channel::wecom",
                            error = %e,
                            "failed to parse wecom message"
                        );
                    }
                },
                Ok(WsMessage::Close(_)) => break,
                Err(e) => {
                    tracing::warn!(
                        target: "clawhive::channel::wecom",
                        error = %e,
                        "wecom WebSocket read error"
                    );
                    break;
                }
                _ => {}
            }
        }

        Ok(())
    }

    fn spawn_delivery_listener(
        bus: Arc<EventBus>,
        _write: Arc<tokio::sync::Mutex<WsSink>>,
        connector_id: String,
    ) {
        tokio::spawn(async move {
            let mut rx = bus.subscribe(Topic::DeliverAnnounce).await;
            while let Some(msg) = rx.recv().await {
                let BusMessage::DeliverAnnounce {
                    channel_type,
                    connector_id: msg_connector_id,
                    conversation_scope: _,
                    text: _,
                } = msg
                else {
                    continue;
                };

                if channel_type != "wecom" || msg_connector_id != connector_id {
                    continue;
                }

                // WeCom AI Bot WebSocket mode only supports request-response.
                // The `aibot_respond_msg` command requires a real incoming msgid,
                // so proactive/scheduled message delivery is not possible via
                // this protocol. Log and skip.
                tracing::warn!(
                    target: "clawhive::channel::wecom",
                    "WeCom AI Bot mode does not support proactive message delivery; \
                     scheduled task result dropped"
                );
            }
        });
    }
}

#[async_trait::async_trait]
impl crate::ChannelBot for WeComBot {
    fn channel_type(&self) -> &str {
        "wecom"
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
    fn parse_message_callback() {
        let json = r#"{
            "cmd": "aibot_msg_callback",
            "headers": {"req_id": "req_001"},
            "body": {
                "msgid": "msg_001",
                "aibotid": "bot_001",
                "chatid": "chat_001",
                "chattype": "group",
                "from": {"userid": "user_001"},
                "msgtype": "text",
                "text": {"content": "@RobotA hello"}
            }
        }"#;
        let msg: WeComMessage = serde_json::from_str(json).unwrap();
        assert_eq!(msg.cmd, "aibot_msg_callback");
        assert_eq!(msg.headers["req_id"], "req_001");

        let body: WeComMsgBody = serde_json::from_value(msg.body.unwrap()).unwrap();
        assert_eq!(body.msgid, "msg_001");
        assert_eq!(body.chattype, Some("group".to_string()));
        assert_eq!(body.from.userid, "user_001");
        assert_eq!(body.text.as_ref().unwrap().content, "@RobotA hello");
    }

    #[test]
    fn parse_subscribe_response() {
        let json = r#"{
            "headers": {"req_id": "req_001"},
            "errcode": 0,
            "errmsg": "ok"
        }"#;
        let resp: WeComMessage = serde_json::from_str(json).unwrap();
        assert_eq!(resp.errcode, Some(0));
    }

    #[test]
    fn adapter_to_inbound_group() {
        let adapter = WeComAdapter::new("wecom-main");
        let body = make_test_body("msg_1", "chat_1", Some("group"), "user_1", "hello");
        let inbound = adapter.to_inbound(&body, "req_001");
        assert_eq!(inbound.channel_type, "wecom");
        assert_eq!(inbound.connector_id, "wecom-main");
        assert_eq!(inbound.conversation_scope, "chat:chat_1");
        assert_eq!(inbound.user_scope, "user:user_1");
        assert_eq!(inbound.text, "hello");
    }

    #[test]
    fn adapter_to_inbound_single() {
        let adapter = WeComAdapter::new("wecom-main");
        let body = make_test_body("msg_2", "", Some("single"), "user_2", "hi");
        let inbound = adapter.to_inbound(&body, "req_002");
        assert_eq!(inbound.conversation_scope, "dm:user_2");
    }

    #[test]
    fn reply_message_serializes_correctly() {
        let reply = WeComReplyMessage::text("req_001", "msg_001", "hello back", true);
        let json = serde_json::to_string(&reply).unwrap();
        assert!(json.contains("aibot_respond_msg"));
        assert!(json.contains("req_001"));
        assert!(json.contains("hello back"));
        assert!(json.contains("\"finish\":true"));
    }

    fn make_test_body(
        msgid: &str,
        chatid: &str,
        chattype: Option<&str>,
        userid: &str,
        text: &str,
    ) -> WeComMsgBody {
        WeComMsgBody {
            msgid: msgid.to_string(),
            aibotid: "bot_test".to_string(),
            chatid: if chatid.is_empty() {
                None
            } else {
                Some(chatid.to_string())
            },
            chattype: chattype.map(|s| s.to_string()),
            from: WeComFrom {
                userid: userid.to_string(),
            },
            msgtype: "text".to_string(),
            text: Some(WeComText {
                content: text.to_string(),
            }),
        }
    }
}
