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
    /// Platform-specific message ID for reactions/replies
    #[serde(default)]
    pub message_id: Option<String>,
    /// Attached media (images, files)
    #[serde(default)]
    pub attachments: Vec<Attachment>,
}

/// Media attachment
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attachment {
    /// Attachment type
    pub kind: AttachmentKind,
    /// URL or file path
    pub url: String,
    /// MIME type if known
    #[serde(default)]
    pub mime_type: Option<String>,
    /// File name if available
    #[serde(default)]
    pub file_name: Option<String>,
    /// File size in bytes
    #[serde(default)]
    pub size: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentKind {
    Image,
    Video,
    Audio,
    Document,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboundMessage {
    pub trace_id: Uuid,
    pub channel_type: String,
    pub connector_id: String,
    pub conversation_scope: String,
    pub text: String,
    pub at: DateTime<Utc>,
    /// Reply to a specific message
    #[serde(default)]
    pub reply_to: Option<String>,
    /// Attached media
    #[serde(default)]
    pub attachments: Vec<Attachment>,
}

/// Channel action (reaction, edit, delete)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelAction {
    pub trace_id: Uuid,
    pub channel_type: String,
    pub connector_id: String,
    pub conversation_scope: String,
    pub message_id: Option<String>,
    pub action: ActionKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum ActionKind {
    /// Add a reaction to a message
    React {
        emoji: String,
    },
    /// Remove a reaction from a message
    Unreact {
        emoji: Option<String>,
    },
    /// Edit a message
    Edit {
        new_text: String,
    },
    /// Delete a message
    Delete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ScheduledSessionMode {
    #[serde(rename = "isolated")]
    Isolated,
    #[serde(rename = "main")]
    Main,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ScheduledDeliveryMode {
    #[serde(rename = "none")]
    None,
    #[serde(rename = "announce")]
    Announce,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ScheduledRunStatus {
    #[serde(rename = "ok")]
    Ok,
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "skipped")]
    Skipped,
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
    ActionReady {
        action: ChannelAction,
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
    NeedHumanApproval {
        trace_id: Uuid,
        reason: String,
    },
    MemoryReadRequested {
        session_key: String,
        query: String,
    },
    ConsolidationCompleted {
        concepts_created: usize,
        concepts_updated: usize,
        episodes_processed: usize,
    },
    StreamDelta {
        trace_id: Uuid,
        delta: String,
        is_final: bool,
    },
    ScheduledTaskTriggered {
        schedule_id: String,
        agent_id: String,
        task: String,
        session_mode: ScheduledSessionMode,
        delivery_mode: ScheduledDeliveryMode,
        delivery_channel: Option<String>,
        delivery_connector_id: Option<String>,
        /// Source channel type for announce delivery (e.g., "discord", "telegram")
        source_channel_type: Option<String>,
        /// Source connector id for announce delivery (e.g., "dc_main", "tg_main")
        source_connector_id: Option<String>,
        /// Source conversation scope for announce delivery (e.g., "guild:123:channel:456")
        source_conversation_scope: Option<String>,
        triggered_at: DateTime<Utc>,
    },
    ScheduledTaskCompleted {
        schedule_id: String,
        status: ScheduledRunStatus,
        error: Option<String>,
        started_at: DateTime<Utc>,
        ended_at: DateTime<Utc>,
        response: Option<String>,
    },
    DeliverAnnounce {
        channel_type: String,
        connector_id: String,
        conversation_scope: String,
        text: String,
    },
    WaitTaskCompleted {
        task_id: String,
        session_key: String,
        status: String,
        message: String,
        output: Option<String>,
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
            message_id: None,
            attachments: vec![],
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
            reply_to: None,
            attachments: vec![],
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
            message_id: None,
            attachments: vec![],
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
        assert!(!msg.is_mention);
        assert_eq!(msg.mention_target, None);
        assert_eq!(msg.text, "hello");
    }

    #[test]
    fn event_inbound_serde_roundtrip() {
        let inbound = InboundMessage {
            trace_id: Uuid::new_v4(),
            channel_type: "telegram".into(),
            connector_id: "tg_main".into(),
            conversation_scope: "chat:1".into(),
            user_scope: "user:2".into(),
            text: "hello".into(),
            at: Utc::now(),
            thread_id: Some("thread-42".into()),
            is_mention: true,
            mention_target: Some("@bot".into()),
            message_id: None,
            attachments: vec![],
        };
        let event = Event::Inbound(inbound);
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: Event = serde_json::from_str(&json).unwrap();
        match deserialized {
            Event::Inbound(msg) => {
                assert_eq!(msg.text, "hello");
                assert_eq!(msg.thread_id, Some("thread-42".into()));
                assert!(msg.is_mention);
                assert_eq!(msg.mention_target, Some("@bot".into()));
            }
            _ => panic!("Expected Inbound variant"),
        }
    }

    #[test]
    fn event_outbound_serde_roundtrip() {
        let outbound = OutboundMessage {
            trace_id: Uuid::new_v4(),
            channel_type: "telegram".into(),
            connector_id: "tg_main".into(),
            conversation_scope: "chat:1".into(),
            text: "reply".into(),
            at: Utc::now(),
            reply_to: None,
            attachments: vec![],
        };
        let event = Event::Outbound(outbound);
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: Event = serde_json::from_str(&json).unwrap();
        match deserialized {
            Event::Outbound(msg) => assert_eq!(msg.text, "reply"),
            _ => panic!("Expected Outbound variant"),
        }
    }

    #[test]
    fn bus_message_remaining_variants_serde() {
        let trace_id = Uuid::new_v4();

        let msg = BusMessage::CancelTask { trace_id };
        let json = serde_json::to_string(&msg).unwrap();
        let de: BusMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(de, BusMessage::CancelTask { .. }));

        let msg = BusMessage::RunScheduledConsolidation;
        let json = serde_json::to_string(&msg).unwrap();
        let de: BusMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(de, BusMessage::RunScheduledConsolidation));

        let msg = BusMessage::MemoryWriteRequested {
            session_key: "s:1".into(),
            speaker: "user".into(),
            text: "hello".into(),
            importance: 0.8,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let de: BusMessage = serde_json::from_str(&json).unwrap();
        match de {
            BusMessage::MemoryWriteRequested { importance, .. } => {
                assert!((importance - 0.8).abs() < f32::EPSILON);
            }
            _ => panic!("Expected MemoryWriteRequested"),
        }

        let msg = BusMessage::NeedHumanApproval {
            trace_id,
            reason: "risky action".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let de: BusMessage = serde_json::from_str(&json).unwrap();
        match de {
            BusMessage::NeedHumanApproval { reason, .. } => {
                assert_eq!(reason, "risky action");
            }
            _ => panic!("Expected NeedHumanApproval"),
        }

        let msg = BusMessage::MemoryReadRequested {
            session_key: "s:1".into(),
            query: "find facts".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let de: BusMessage = serde_json::from_str(&json).unwrap();
        match de {
            BusMessage::MemoryReadRequested { query, .. } => {
                assert_eq!(query, "find facts");
            }
            _ => panic!("Expected MemoryReadRequested"),
        }

        let msg = BusMessage::ConsolidationCompleted {
            concepts_created: 3,
            concepts_updated: 1,
            episodes_processed: 10,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let de: BusMessage = serde_json::from_str(&json).unwrap();
        match de {
            BusMessage::ConsolidationCompleted {
                concepts_created,
                concepts_updated,
                episodes_processed,
            } => {
                assert_eq!(concepts_created, 3);
                assert_eq!(concepts_updated, 1);
                assert_eq!(episodes_processed, 10);
            }
            _ => panic!("Expected ConsolidationCompleted"),
        }

        let msg = BusMessage::MessageAccepted { trace_id };
        let json = serde_json::to_string(&msg).unwrap();
        let de: BusMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(de, BusMessage::MessageAccepted { .. }));
    }

    #[test]
    fn bus_message_stream_delta_serde_roundtrip() {
        let trace_id = Uuid::new_v4();
        let msg = BusMessage::StreamDelta {
            trace_id,
            delta: "hello".into(),
            is_final: false,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let de: BusMessage = serde_json::from_str(&json).unwrap();
        match de {
            BusMessage::StreamDelta {
                delta, is_final, ..
            } => {
                assert_eq!(delta, "hello");
                assert!(!is_final);
            }
            _ => panic!("Expected StreamDelta"),
        }
    }

    #[test]
    fn session_key_with_special_characters() {
        let inbound = InboundMessage {
            trace_id: Uuid::new_v4(),
            channel_type: "telegram".into(),
            connector_id: "tg:special/id".into(),
            conversation_scope: "group:chat:-100123".into(),
            user_scope: "user:0".into(),
            text: "".into(),
            at: Utc::now(),
            thread_id: None,
            is_mention: false,
            mention_target: None,
            message_id: None,
            attachments: vec![],
        };
        let key = SessionKey::from_inbound(&inbound);
        assert_eq!(key.0, "telegram:tg:special/id:group:chat:-100123:user:0");
    }
}
