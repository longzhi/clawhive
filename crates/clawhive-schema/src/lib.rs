use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub mod command_registry;
pub mod provider_presets;
pub mod text;

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
    /// Message origin: "interactive" (default), "scheduled_task", "system_event"
    #[serde(default)]
    pub message_source: Option<String>,
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
    React { emoji: String },
    /// Remove a reaction from a message
    Unreact { emoji: Option<String> },
    /// Edit a message
    Edit { new_text: String },
    /// Delete a message
    Delete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum ScheduledTaskPayload {
    #[serde(rename = "system_event")]
    SystemEvent { text: String },
    #[serde(rename = "agent_turn")]
    AgentTurn {
        message: String,
        model: Option<String>,
        thinking: Option<String>,
        timeout_seconds: u64,
        light_context: bool,
    },
    #[serde(rename = "direct_deliver")]
    DirectDeliver { text: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ScheduledDeliveryMode {
    #[serde(rename = "none")]
    None,
    #[serde(rename = "announce")]
    Announce,
    #[serde(rename = "webhook")]
    Webhook,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledFailureDestination {
    pub channel: Option<String>,
    pub connector_id: Option<String>,
    pub conversation_scope: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledDeliveryInfo {
    pub mode: ScheduledDeliveryMode,
    pub channel: Option<String>,
    pub connector_id: Option<String>,
    pub source_channel_type: Option<String>,
    pub source_connector_id: Option<String>,
    pub source_conversation_scope: Option<String>,
    pub source_user_scope: Option<String>,
    pub webhook_url: Option<String>,
    pub failure_destination: Option<ScheduledFailureDestination>,
    #[serde(default)]
    pub best_effort: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ScheduledDeliveryStatus {
    #[serde(rename = "delivered")]
    Delivered,
    #[serde(rename = "not_delivered")]
    NotDelivered,
    #[serde(rename = "not_requested")]
    NotRequested,
}

fn default_scheduled_delivery_status() -> ScheduledDeliveryStatus {
    ScheduledDeliveryStatus::NotRequested
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum ScheduledSessionMode {
    #[serde(rename = "isolated")]
    Isolated,
    #[serde(rename = "main")]
    Main,
}

/// Decision from human approval UI
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ApprovalDecision {
    /// Allow this one request only
    AllowOnce,
    /// Add to allowlist and allow
    AlwaysAllow,
    /// Block this request
    Deny,
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
        agent_id: String,
        command: String,
        /// Network target requiring approval (None = exec-only approval)
        network_target: Option<String>,
        #[serde(default)]
        summary: Option<String>,
        source_channel_type: Option<String>,
        source_connector_id: Option<String>,
        source_conversation_scope: Option<String>,
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
    ToolCallStarted {
        trace_id: Uuid,
        tool_name: String,
        arguments: String,
    },
    ToolCallCompleted {
        trace_id: Uuid,
        tool_name: String,
        output: String,
        duration_ms: u64,
    },
    ScheduledTaskTriggered {
        schedule_id: String,
        agent_id: String,
        payload: ScheduledTaskPayload,
        delivery: ScheduledDeliveryInfo,
        session_mode: ScheduledSessionMode,
        triggered_at: DateTime<Utc>,
    },
    ScheduledTaskCompleted {
        schedule_id: String,
        status: ScheduledRunStatus,
        error: Option<String>,
        started_at: DateTime<Utc>,
        ended_at: DateTime<Utc>,
        #[serde(default = "default_scheduled_delivery_status")]
        delivery_status: ScheduledDeliveryStatus,
        #[serde(default)]
        delivery_error: Option<String>,
        response: Option<String>,
        #[serde(default)]
        session_key: Option<String>,
    },
    DeliverAnnounce {
        channel_type: String,
        connector_id: String,
        conversation_scope: String,
        text: String,
    },
    DeliverApprovalRequest {
        channel_type: String,
        connector_id: String,
        conversation_scope: String,
        short_id: String,
        agent_id: String,
        command: String,
        #[serde(default)]
        network_target: Option<String>,
        #[serde(default)]
        summary: Option<String>,
    },
    DeliverSkillConfirm {
        channel_type: String,
        connector_id: String,
        conversation_scope: String,
        token: String,
        skill_name: String,
        analysis_text: String,
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

    pub fn is_scheduled_session(&self) -> bool {
        self.0
            .split(':')
            .nth(2)
            .is_some_and(|scope| scope.starts_with("schedule"))
    }
}

// ---------------------------------------------------------------------------
// Approval display helpers (shared across channel adapters)
// ---------------------------------------------------------------------------

/// Maximum characters for command preview in approval messages.
const APPROVAL_CMD_MAX_CHARS: usize = 200;

/// Structured approval info ready for channel-specific rendering.
#[derive(Debug, Clone)]
pub struct ApprovalDisplay {
    /// e.g. "Command Approval Required" or "Network Access Required"
    pub title: String,
    pub agent_id: String,
    /// First token of the command (e.g. `curl`, `python3`)
    pub program: String,
    /// Truncated command string for display
    pub command_preview: String,
    /// Network target if this is a network approval
    pub network_target: Option<String>,
    /// Human-friendly summary (hides raw command when present)
    pub summary: Option<String>,
}

/// Extract the main program name from a shell command for approval decisions.
pub fn approval_program(command: &str) -> String {
    // Track parenthesis depth to skip subshell contents like $(python3 -c "...")
    let mut depth = 0i32;
    for token in command.split_whitespace() {
        let was_top = depth == 0;
        depth += token.matches('(').count() as i32;
        depth -= token.matches(')').count() as i32;
        if depth < 0 {
            depth = 0;
        }

        // Skip tokens inside subshells or that started inside one
        if !was_top || depth > 0 {
            continue;
        }

        // Skip variable assignments (FOO=bar, FOO=$(cmd))
        if token.contains('=') {
            continue;
        }

        // Skip flags and quoted strings
        if token.starts_with('-') || token.starts_with('"') || token.starts_with('\'') {
            continue;
        }

        // Found the actual program — strip path prefix
        return token.rsplit('/').next().unwrap_or(token).to_string();
    }

    // Fallback: first token, cleaned up
    let first = command.split_whitespace().next().unwrap_or("unknown");
    if let Some(pos) = first.find("$(") {
        first[pos + 2..].to_string()
    } else {
        first.to_string()
    }
}

impl ApprovalDisplay {
    pub fn new(
        agent_id: &str,
        command: &str,
        network_target: Option<&str>,
        summary: Option<String>,
    ) -> Self {
        let program = approval_program(command);

        let command_preview = if command.len() > APPROVAL_CMD_MAX_CHARS {
            let end = command.floor_char_boundary(APPROVAL_CMD_MAX_CHARS);
            format!("{}…", &command[..end])
        } else {
            command.to_string()
        };

        let title = if network_target.is_some() {
            "Network Access Required".to_string()
        } else {
            "Command Approval Required".to_string()
        };

        Self {
            title,
            agent_id: agent_id.to_string(),
            program,
            command_preview,
            network_target: network_target.map(|s| s.to_string()),
            summary,
        }
    }

    /// Format as Discord/generic Markdown.
    pub fn to_markdown(&self) -> String {
        if let Some(ref summary) = self.summary {
            return format!(
                "⚠️ **{}**\n🤖 `{}`\n📝 {}",
                self.title, self.agent_id, summary
            );
        }
        let mut s = format!(
            "⚠️ **{}**\n**Agent:** `{}`\n**Program:** `{}`",
            self.title, self.agent_id, self.program
        );
        if let Some(ref target) = self.network_target {
            s.push_str(&format!("\n**Target:** `{target}`"));
        }
        s.push_str(&format!("\n**Command:** `{}`", self.command_preview));
        s
    }

    /// Format as Telegram HTML.
    pub fn to_html(&self) -> String {
        fn esc(s: &str) -> String {
            s.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
        }
        if let Some(ref summary) = self.summary {
            return format!(
                "⚠️ <b>{}</b>\n🤖 <code>{}</code>\n📝 {}",
                esc(&self.title),
                esc(&self.agent_id),
                esc(summary),
            );
        }
        let mut s = format!(
            "⚠️ <b>{}</b>\n<b>Agent:</b> <code>{}</code>\n<b>Program:</b> <code>{}</code>",
            esc(&self.title),
            esc(&self.agent_id),
            esc(&self.program),
        );
        if let Some(ref target) = self.network_target {
            s.push_str(&format!("\n<b>Target:</b> <code>{}</code>", esc(target)));
        }
        s.push_str(&format!(
            "\n<b>Command:</b> <code>{}</code>",
            esc(&self.command_preview)
        ));
        s
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
            message_source: None,
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
            message_source: None,
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
            message_source: None,
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
            agent_id: "agent-1".into(),
            command: "rm -rf /tmp/test".into(),
            network_target: None,
            summary: None,
            source_channel_type: Some("telegram".into()),
            source_connector_id: Some("tg_main".into()),
            source_conversation_scope: Some("chat:123".into()),
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
            message_source: None,
        };
        let key = SessionKey::from_inbound(&inbound);
        assert_eq!(key.0, "telegram:tg:special/id:group:chat:-100123:user:0");
    }

    #[test]
    fn scheduled_task_payload_serde_roundtrip() {
        let payload = ScheduledTaskPayload::AgentTurn {
            message: "do task".into(),
            model: None,
            thinking: None,
            timeout_seconds: 300,
            light_context: false,
        };
        let json = serde_json::to_string(&payload).unwrap();
        let back: ScheduledTaskPayload = serde_json::from_str(&json).unwrap();
        assert!(
            matches!(back, ScheduledTaskPayload::AgentTurn { message, .. } if message == "do task")
        );
    }

    #[test]
    fn approval_display_exec_command() {
        let d = ApprovalDisplay::new("agent-1", "rm -rf /tmp", None, None);
        assert_eq!(d.title, "Command Approval Required");
        assert_eq!(d.program, "rm");
        assert_eq!(d.command_preview, "rm -rf /tmp");
        assert!(d.network_target.is_none());
    }

    #[test]
    fn approval_display_network() {
        let d = ApprovalDisplay::new(
            "agent-1",
            "curl https://example.com",
            Some("example.com:443"),
            None,
        );
        assert_eq!(d.title, "Network Access Required");
        assert_eq!(d.program, "curl");
        assert!(d.network_target.as_deref() == Some("example.com:443"));
    }

    #[test]
    fn approval_display_truncates_long_command() {
        let long_cmd = format!("python3 {}", "x".repeat(300));
        let d = ApprovalDisplay::new("agent-1", &long_cmd, None, None);
        assert!(d.command_preview.len() < 210);
        assert!(d.command_preview.ends_with('…'));
        assert_eq!(d.program, "python3");
    }

    #[test]
    fn approval_display_markdown_format() {
        let d = ApprovalDisplay::new(
            "agent-1",
            "curl https://api.com/data",
            Some("api.com:443"),
            None,
        );
        let md = d.to_markdown();
        assert!(md.contains("**Network Access Required**"));
        assert!(md.contains("**Program:** `curl`"));
        assert!(md.contains("**Target:** `api.com:443`"));
        assert!(md.contains("**Command:**"));
    }

    #[test]
    fn approval_display_skips_variable_assignments() {
        let cmd = r#"NOW_MS=$(python3 -c "import time; print(int(time.time()*1000))") FROM_MS=$(python3 -c "import time; print(int((time.time()-86400)*1000))") curl -s -H "Authorization: Bearer token" https://api.example.com"#;
        let d = ApprovalDisplay::new("agent-1", cmd, None, None);
        assert_eq!(d.program, "curl");
    }

    #[test]
    fn approval_display_simple_var_assignment() {
        let d = ApprovalDisplay::new("agent-1", "FOO=bar python3 script.py", None, None);
        assert_eq!(d.program, "python3");
    }

    #[test]
    fn approval_display_html_escapes() {
        let d = ApprovalDisplay::new("agent<x>", "echo '<tag>'", None, None);
        let html = d.to_html();
        assert!(html.contains("agent&lt;x&gt;"));
        assert!(html.contains("&lt;tag&gt;"));
        assert!(!html.contains("<tag>"));
    }

    #[test]
    fn approval_display_with_summary_hides_command() {
        let d = ApprovalDisplay::new(
            "agent-1",
            "curl -s -H 'Auth: Bearer xxx' https://api.example.com/data",
            Some("api.example.com:443"),
            Some("访问 api.example.com 获取数据".to_string()),
        );
        let md = d.to_markdown();
        assert!(md.contains("访问 api.example.com 获取数据"));
        assert!(!md.contains("curl"));
        assert!(!md.contains("Command:"));
    }

    #[test]
    fn approval_display_without_summary_shows_command() {
        let d = ApprovalDisplay::new("agent-1", "rm -rf /tmp", None, None);
        let md = d.to_markdown();
        assert!(md.contains("**Program:** `rm`"));
        assert!(md.contains("**Command:**"));
    }
}
