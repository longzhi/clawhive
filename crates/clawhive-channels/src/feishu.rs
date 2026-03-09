use chrono::Utc;
use clawhive_schema::InboundMessage;
use prost::Message;
use serde::{Deserialize, Serialize};
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
