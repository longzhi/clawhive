use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundMessage {
    pub trace_id: Uuid,
    pub channel_type: String,
    pub connector_id: String,
    pub conversation_scope: String,
    pub user_scope: String,
    pub text: String,
    pub at: DateTime<Utc>,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub is_mention: bool,
    #[serde(default)]
    pub mention_target: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboundMessage {
    pub trace_id: Uuid,
    pub channel_type: String,
    pub connector_id: String,
    pub conversation_scope: String,
    pub text: String,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    Inbound(InboundMessage),
    Outbound(OutboundMessage),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BusMessage {
    HandleIncomingMessage {
        inbound: InboundMessage,
        resolved_agent_id: String,
    },
    CancelTask {
        trace_id: Uuid,
    },
    RunScheduledConsolidation,
    MessageAccepted {
        trace_id: Uuid,
    },
    ReplyReady {
        outbound: OutboundMessage,
    },
    TaskFailed {
        trace_id: Uuid,
        error: String,
    },
    MemoryWriteRequested {
        session_key: String,
        speaker: String,
        text: String,
        importance: f32,
    },
}

#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionKey(pub String);

impl SessionKey {
    pub fn from_inbound(msg: &InboundMessage) -> Self {
        Self(format!(
            "{}:{}:{}:{}",
            msg.channel_type, msg.connector_id, msg.conversation_scope, msg.user_scope
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_key_from_inbound() {
        let inbound = InboundMessage {
            trace_id: Uuid::new_v4(),
            channel_type: "telegram".to_string(),
            connector_id: "tg_main".to_string(),
            conversation_scope: "chat:123".to_string(),
            user_scope: "user:456".to_string(),
            text: "hello".to_string(),
            at: Utc::now(),
            thread_id: None,
            is_mention: false,
            mention_target: None,
        };

        let key = SessionKey::from_inbound(&inbound);
        assert_eq!(key.0, "telegram:tg_main:chat:123:user:456");
    }

    #[test]
    fn bus_message_serde_roundtrip() {
        let trace_id = Uuid::new_v4();
        let outbound = OutboundMessage {
            trace_id,
            channel_type: "telegram".to_string(),
            connector_id: "tg_main".to_string(),
            conversation_scope: "chat:123".to_string(),
            text: "reply".to_string(),
            at: Utc::now(),
        };

        // Test HandleIncomingMessage variant
        let inbound = InboundMessage {
            trace_id,
            channel_type: "telegram".to_string(),
            connector_id: "tg_main".to_string(),
            conversation_scope: "chat:123".to_string(),
            user_scope: "user:456".to_string(),
            text: "hello".to_string(),
            at: Utc::now(),
            thread_id: None,
            is_mention: false,
            mention_target: None,
        };

        let msg1 = BusMessage::HandleIncomingMessage {
            inbound: inbound.clone(),
            resolved_agent_id: "agent1".to_string(),
        };
        let json1 = serde_json::to_string(&msg1).unwrap();
        let deserialized1: BusMessage = serde_json::from_str(&json1).unwrap();
        match deserialized1 {
            BusMessage::HandleIncomingMessage {
                resolved_agent_id, ..
            } => {
                assert_eq!(resolved_agent_id, "agent1");
            }
            _ => panic!("Expected HandleIncomingMessage variant"),
        }

        // Test ReplyReady variant
        let msg2 = BusMessage::ReplyReady {
            outbound: outbound.clone(),
        };
        let json2 = serde_json::to_string(&msg2).unwrap();
        let deserialized2: BusMessage = serde_json::from_str(&json2).unwrap();
        match deserialized2 {
            BusMessage::ReplyReady { outbound: out } => {
                assert_eq!(out.text, "reply");
            }
            _ => panic!("Expected ReplyReady variant"),
        }

        // Test TaskFailed variant
        let msg3 = BusMessage::TaskFailed {
            trace_id,
            error: "test error".to_string(),
        };
        let json3 = serde_json::to_string(&msg3).unwrap();
        let deserialized3: BusMessage = serde_json::from_str(&json3).unwrap();
        match deserialized3 {
            BusMessage::TaskFailed { error, .. } => {
                assert_eq!(error, "test error");
            }
            _ => panic!("Expected TaskFailed variant"),
        }
    }

    #[test]
    fn inbound_message_backward_compat() {
        // Test that new fields default correctly when deserializing old JSON
        let old_json = r#"{
            "trace_id": "550e8400-e29b-41d4-a716-446655440000",
            "channel_type": "telegram",
            "connector_id": "tg_main",
            "conversation_scope": "chat:123",
            "user_scope": "user:456",
            "text": "hello",
            "at": "2025-02-12T10:00:00Z"
        }"#;

        let msg: InboundMessage = serde_json::from_str(old_json).unwrap();
        assert_eq!(msg.thread_id, None);
        assert_eq!(msg.is_mention, false);
        assert_eq!(msg.mention_target, None);
        assert_eq!(msg.text, "hello");
    }
}
