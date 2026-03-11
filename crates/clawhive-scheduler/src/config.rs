use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct ScheduleConfig {
    pub schedule_id: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub schedule: ScheduleType,
    pub agent_id: String,
    #[serde(default)]
    pub session_mode: SessionMode,
    /// Typed task payload.
    #[serde(default)]
    pub payload: Option<TaskPayload>,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    #[serde(default)]
    pub delete_after_run: bool,
    #[serde(default)]
    pub delivery: DeliveryConfig,
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self {
            schedule_id: String::new(),
            enabled: default_true(),
            name: String::new(),
            description: None,
            schedule: ScheduleType::At {
                at: "10m".to_string(),
            },
            agent_id: "clawhive-main".to_string(),
            session_mode: SessionMode::default(),
            payload: None,
            timeout_seconds: default_timeout(),
            delete_after_run: false,
            delivery: DeliveryConfig::default(),
        }
    }
}
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(tag = "kind")]
pub enum ScheduleType {
    #[serde(rename = "cron")]
    Cron {
        expr: String,
        #[serde(default = "default_tz")]
        tz: String,
    },
    #[serde(rename = "at")]
    At { at: String },
    #[serde(rename = "every")]
    Every {
        interval_ms: u64,
        #[serde(default)]
        anchor_ms: Option<u64>,
    },
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq)]
pub enum SessionMode {
    #[default]
    #[serde(rename = "isolated")]
    Isolated,
    #[serde(rename = "main")]
    Main,
}

fn default_payload_timeout() -> u64 {
    300
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(tag = "kind")]
pub enum TaskPayload {
    /// Inject into the source channel's session, reusing the original conversation context.
    /// Agent processes it on next heartbeat or wake.
    #[serde(rename = "system_event")]
    SystemEvent { text: String },
    /// Create an isolated session and run a full agent turn.
    #[serde(rename = "agent_turn")]
    AgentTurn {
        message: String,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        thinking: Option<String>,
        #[serde(default = "default_payload_timeout")]
        timeout_seconds: u64,
        #[serde(default)]
        light_context: bool,
    },
    /// Deliver text directly without going through the agent. For simple reminders.
    #[serde(rename = "direct_deliver")]
    DirectDeliver { text: String },
}

/// Resolve payload — payload is required, no legacy fallback.
pub fn resolve_payload(payload: Option<TaskPayload>) -> Result<TaskPayload, anyhow::Error> {
    payload.ok_or_else(|| anyhow::anyhow!("payload must be provided"))
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct FailureDestination {
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub connector_id: Option<String>,
    #[serde(default)]
    pub conversation_scope: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct DeliveryConfig {
    #[serde(default)]
    pub mode: DeliveryMode,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub connector_id: Option<String>,
    /// Source channel type (e.g., "discord", "telegram") for announce delivery
    #[serde(default)]
    pub source_channel_type: Option<String>,
    /// Source connector id for announce delivery
    #[serde(default)]
    pub source_connector_id: Option<String>,
    /// Source conversation scope (e.g., "guild:123:channel:456") for announce delivery
    #[serde(default)]
    pub source_conversation_scope: Option<String>,
    /// Source user scope for preserving session key identity in SystemEvent execution
    #[serde(default)]
    pub source_user_scope: Option<String>,
    /// Webhook URL for webhook delivery mode
    #[serde(default)]
    pub webhook_url: Option<String>,
    /// Where to deliver failure notifications
    #[serde(default)]
    pub failure_destination: Option<FailureDestination>,
    /// Best-effort delivery: don't report delivery failure as error
    #[serde(default)]
    pub best_effort: bool,
}

impl Default for DeliveryConfig {
    fn default() -> Self {
        Self {
            mode: DeliveryMode::None,
            channel: None,
            connector_id: None,
            source_channel_type: None,
            source_connector_id: None,
            source_conversation_scope: None,
            source_user_scope: None,
            webhook_url: None,
            failure_destination: None,
            best_effort: false,
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq)]
pub enum DeliveryMode {
    #[default]
    #[serde(rename = "none")]
    None,
    #[serde(rename = "announce")]
    Announce,
    #[serde(rename = "webhook")]
    Webhook,
}

fn default_true() -> bool {
    true
}

fn default_timeout() -> u64 {
    300
}

fn default_tz() -> String {
    "UTC".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_payload_serde_system_event() {
        let payload = TaskPayload::SystemEvent {
            text: "hello".into(),
        };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("system_event"));
        let back: TaskPayload = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, TaskPayload::SystemEvent { text } if text == "hello"));
    }

    #[test]
    fn task_payload_serde_agent_turn() {
        let payload = TaskPayload::AgentTurn {
            message: "do task".into(),
            model: Some("anthropic/claude-opus-4".into()),
            thinking: None,
            timeout_seconds: 120,
            light_context: false,
        };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("agent_turn"));
        let back: TaskPayload = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, TaskPayload::AgentTurn { message, .. } if message == "do task"));
    }

    #[test]
    fn task_payload_serde_direct_deliver() {
        let payload = TaskPayload::DirectDeliver {
            text: "reminder".into(),
        };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("direct_deliver"));
        let back: TaskPayload = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, TaskPayload::DirectDeliver { text } if text == "reminder"));
    }

    #[test]
    fn resolve_payload_requires_payload() {
        let payload = TaskPayload::DirectDeliver { text: "hi".into() };
        let result = resolve_payload(Some(payload)).unwrap();
        assert!(matches!(result, TaskPayload::DirectDeliver { .. }));
    }

    #[test]
    fn resolve_payload_errors_when_none() {
        let result = resolve_payload(None);
        assert!(result.is_err());
    }
    #[test]
    fn delivery_config_serde_with_user_scope() {
        let config = DeliveryConfig {
            source_user_scope: Some("user:456".into()),
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("user:456"));
        let back: DeliveryConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.source_user_scope.as_deref(), Some("user:456"));
    }

    #[test]
    fn delivery_config_serde_with_webhook() {
        let config = DeliveryConfig {
            mode: DeliveryMode::Webhook,
            webhook_url: Some("https://example.com/hook".into()),
            best_effort: true,
            failure_destination: Some(FailureDestination {
                channel: Some("discord".into()),
                connector_id: Some("dc_main".into()),
                conversation_scope: Some("guild:1:channel:2".into()),
            }),
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("webhook"));
        assert!(json.contains("https://example.com/hook"));
        assert!(json.contains("best_effort"));
        let back: DeliveryConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.mode, DeliveryMode::Webhook);
        assert_eq!(
            back.webhook_url.as_deref(),
            Some("https://example.com/hook")
        );
        assert!(back.best_effort);
        assert!(back.failure_destination.is_some());
    }

    #[test]
    fn delivery_config_defaults_backward_compatible() {
        let json = r#"{"mode":"none"}"#;
        let config: DeliveryConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.mode, DeliveryMode::None);
        assert!(config.webhook_url.is_none());
        assert!(!config.best_effort);
        assert!(config.failure_destination.is_none());
    }
}
