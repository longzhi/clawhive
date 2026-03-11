use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use chrono::Utc;
use clawhive_provider::ToolDef;
use clawhive_scheduler::{
    resolve_payload, DeliveryConfig, DeliveryMode, ScheduleConfig, ScheduleManager, ScheduleType,
    SessionMode, TaskPayload,
};
use serde::Deserialize;

use crate::tool::{ToolContext, ToolExecutor, ToolOutput};

pub const SCHEDULE_TOOL_NAME: &str = "schedule";

const DEFAULT_AGENT_ID: &str = "clawhive-main";

pub struct ScheduleTool {
    manager: Arc<ScheduleManager>,
    default_agent_id: String,
}

impl ScheduleTool {
    pub fn new(manager: Arc<ScheduleManager>) -> Self {
        Self {
            manager,
            default_agent_id: DEFAULT_AGENT_ID.to_string(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct ScheduleInput {
    action: String,
    #[serde(default)]
    job: Option<ScheduleJobInput>,
    #[serde(default)]
    schedule_id: Option<String>,
    #[serde(default)]
    patch: Option<serde_json::Value>,
    #[serde(default)]
    include_history: Option<bool>,
    #[serde(default)]
    history_limit: Option<usize>,
    #[serde(default)]
    include_payload_summary: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ScheduleJobInput {
    #[serde(default)]
    schedule_id: Option<String>,
    name: String,
    #[serde(default)]
    description: Option<String>,
    schedule: ScheduleType,
    #[serde(default)]
    payload: Option<TaskPayload>,
    #[serde(default)]
    session_mode: Option<SessionMode>,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    delete_after_run: Option<bool>,
    #[serde(default)]
    timeout_seconds: Option<u64>,
    #[serde(default)]
    context_messages: Option<usize>,
    #[serde(default)]
    delivery: Option<DeliveryInput>,
}

#[derive(Debug, Deserialize)]
struct DeliveryInput {
    #[serde(default)]
    mode: Option<DeliveryMode>,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    connector_id: Option<String>,
    #[serde(default)]
    webhook_url: Option<String>,
}

impl ScheduleJobInput {
    fn into_config(
        self,
        default_agent_id: &str,
        ctx: &ToolContext,
    ) -> Result<ScheduleConfig, anyhow::Error> {
        let session_mode = self.session_mode.clone().unwrap_or(SessionMode::Isolated);
        let resolved = resolve_payload(self.payload)?;

        let mut task_text = match &resolved {
            TaskPayload::AgentTurn { message, .. } => message.clone(),
            TaskPayload::SystemEvent { text } => text.clone(),
            TaskPayload::DirectDeliver { text } => text.clone(),
        };

        if let Some(limit) = self.context_messages {
            if limit > 0 {
                let context = ctx
                    .recent_messages(limit)
                    .into_iter()
                    .map(|message| format!("- {}: {}", message.role, message.content))
                    .collect::<Vec<_>>()
                    .join("\n");

                if !context.is_empty() {
                    task_text = format!("{task_text}\n\nRecent context:\n{context}");
                }
            }
        }

        let payload = match resolved {
            TaskPayload::AgentTurn {
                model,
                thinking,
                timeout_seconds,
                light_context,
                ..
            } => TaskPayload::AgentTurn {
                message: task_text.clone(),
                model,
                thinking,
                timeout_seconds,
                light_context,
            },
            TaskPayload::SystemEvent { .. } => TaskPayload::SystemEvent {
                text: task_text.clone(),
            },
            TaskPayload::DirectDeliver { .. } => TaskPayload::DirectDeliver {
                text: task_text.clone(),
            },
        };

        let delivery = self.delivery.unwrap_or(DeliveryInput {
            mode: None,
            channel: None,
            connector_id: None,
            webhook_url: None,
        });

        // Default to Announce delivery if source info available and mode not specified
        let delivery_mode = delivery.mode.unwrap_or_else(|| {
            if ctx.source_conversation_scope().is_some() {
                DeliveryMode::Announce
            } else {
                DeliveryMode::None
            }
        });

        // Convert relative time to absolute timestamp for "at" schedules
        // This prevents the bug where "1m" keeps being re-interpreted as "1 minute from now"
        let schedule = normalize_schedule_type(self.schedule);

        // "at" schedules should default to delete_after_run=true since they're one-shot
        #[allow(clippy::unnecessary_lazy_evaluations)]
        let delete_after_run = self
            .delete_after_run
            .unwrap_or_else(|| matches!(schedule, ScheduleType::At { .. }));

        Ok(ScheduleConfig {
            schedule_id: self
                .schedule_id
                .filter(|id| !id.trim().is_empty())
                .unwrap_or_else(|| slug_from_name(&self.name)),
            enabled: true,
            name: self.name,
            description: self.description,
            schedule,
            agent_id: self
                .agent_id
                .filter(|id| !id.trim().is_empty())
                .unwrap_or_else(|| default_agent_id.to_string()),
            session_mode,
            payload: Some(payload),
            timeout_seconds: self.timeout_seconds.unwrap_or(300),
            delete_after_run,
            delivery: DeliveryConfig {
                mode: delivery_mode,
                channel: delivery.channel,
                connector_id: delivery.connector_id,
                source_channel_type: ctx.source_channel_type().map(String::from),
                source_connector_id: ctx.source_connector_id().map(String::from),
                source_conversation_scope: ctx.source_conversation_scope().map(String::from),
                source_user_scope: ctx.source_user_scope().map(String::from),
                webhook_url: delivery.webhook_url,
                failure_destination: None,
                best_effort: false,
            },
        })
    }
}

/// Convert relative time in "at" schedules to absolute ISO timestamp.
/// This prevents the bug where "1m" keeps being re-interpreted on every check.
fn normalize_schedule_type(schedule: ScheduleType) -> ScheduleType {
    match schedule {
        ScheduleType::At { at } => {
            // Try to parse as relative time (e.g., "1m", "2h", "30s")
            if let Some(ms) = try_parse_relative_ms(&at) {
                let now_ms = Utc::now().timestamp_millis();
                let target_ms = now_ms + ms;
                let target_dt =
                    chrono::DateTime::from_timestamp_millis(target_ms).unwrap_or_else(Utc::now);
                ScheduleType::At {
                    at: target_dt.to_rfc3339(),
                }
            } else {
                // Already absolute or unparseable, keep as-is
                ScheduleType::At { at }
            }
        }
        other => other,
    }
}

/// Try to parse a relative time string like "1m", "2h", "30s", "1d"
fn try_parse_relative_ms(input: &str) -> Option<i64> {
    let input = input.trim();
    if input.len() < 2 {
        return None;
    }

    let (num_str, unit) = input.split_at(input.len() - 1);
    let num: i64 = num_str.parse().ok()?;

    match unit {
        "s" => Some(num * 1_000),
        "m" => Some(num * 60_000),
        "h" => Some(num * 3_600_000),
        "d" => Some(num * 86_400_000),
        _ => None,
    }
}

fn slug_from_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_dash = false;

    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }

    let slug = out.trim_matches('-').to_string();
    if slug.is_empty() {
        "schedule".to_string()
    } else {
        slug
    }
}

fn tool_error(message: impl Into<String>) -> ToolOutput {
    ToolOutput {
        content: message.into(),
        is_error: true,
    }
}

fn tool_ok(message: impl Into<String>) -> ToolOutput {
    ToolOutput {
        content: message.into(),
        is_error: false,
    }
}

fn format_timestamp_ms(ms: Option<i64>) -> Option<String> {
    ms.and_then(|v| chrono::DateTime::<Utc>::from_timestamp_millis(v).map(|dt| dt.to_rfc3339()))
}

fn payload_kind(payload: Option<&TaskPayload>) -> Option<&'static str> {
    match payload {
        Some(TaskPayload::SystemEvent { .. }) => Some("system_event"),
        Some(TaskPayload::AgentTurn { .. }) => Some("agent_turn"),
        Some(TaskPayload::DirectDeliver { .. }) => Some("direct_deliver"),
        None => None,
    }
}

fn payload_text(payload: Option<&TaskPayload>) -> Option<&str> {
    match payload {
        Some(TaskPayload::SystemEvent { text }) => Some(text.as_str()),
        Some(TaskPayload::AgentTurn { message, .. }) => Some(message.as_str()),
        Some(TaskPayload::DirectDeliver { text }) => Some(text.as_str()),
        None => None,
    }
}

fn payload_preview(payload: Option<&TaskPayload>, max_chars: usize) -> Option<String> {
    let raw = payload_text(payload)?;
    let compact = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.is_empty() {
        return None;
    }
    Some(truncate_for_preview(&compact, max_chars))
}

fn truncate_for_preview(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let mut out = text.chars().take(max_chars).collect::<String>();
    out.push_str("...");
    out
}

#[async_trait]
impl ToolExecutor for ScheduleTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: SCHEDULE_TOOL_NAME.to_string(),
            description: "Manage scheduled tasks: list/get/add/update/remove/run for reminders and recurring jobs. 'list' returns summary metadata. For full payload details, call action='get' with schedule_id."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": { 
                        "type": "string", 
                        "enum": ["list", "get", "add", "update", "remove", "run"],
                        "description": "Action to perform"
                    },
                    "job": { 
                        "type": "object",
                        "description": "Job definition (required for 'add' action)",
                        "properties": {
                            "name": { 
                                "type": "string",
                                "description": "Human-readable name for the schedule"
                            },
                            "schedule": {
                                "type": "object",
                                "description": "When to run. Use {kind:'at', at:'2m'} for relative time, {kind:'at', at:'2026-02-25T10:00:00Z'} for absolute, {kind:'cron', expr:'0 9 * * *', tz:'UTC'} for recurring",
                                "properties": {
                                    "kind": { "type": "string", "enum": ["at", "cron", "every"] },
                                    "at": { "type": "string", "description": "For kind='at': relative (2m, 1h) or ISO timestamp" },
                                    "expr": { "type": "string", "description": "For kind='cron': cron expression" },
                                    "tz": { "type": "string", "description": "For kind='cron': timezone (default UTC)" },
                                    "interval_ms": { "type": "number", "description": "For kind='every': interval in milliseconds" }
                                },
                                "required": ["kind"]
                            },
                            "payload": {
                                "type": "object",
                                "description": "Typed task payload. Use instead of 'task' for typed control. Kinds: system_event (inject into source session), agent_turn (isolated agent execution), direct_deliver (simple text delivery).",
                                "properties": {
                                    "kind": {
                                        "type": "string",
                                        "enum": ["system_event", "agent_turn", "direct_deliver"],
                                        "description": "Payload type"
                                    },
                                    "text": { "type": "string", "description": "For system_event/direct_deliver: the text content" },
                                    "message": { "type": "string", "description": "For agent_turn: the task message" },
                                    "model": { "type": "string", "description": "For agent_turn: model override" },
                                    "thinking": { "type": "string", "description": "For agent_turn: thinking level" },
                                    "timeout_seconds": { "type": "number", "description": "For agent_turn: timeout in seconds (default 300)" }
                                },
                                "required": ["kind"]
                            },
                            "session_mode": { 
                                "type": "string", 
                                "enum": ["isolated", "main"],
                                "description": "Session mode (default: isolated)"
                            },
                            "delete_after_run": { 
                                "type": "boolean",
                                "description": "Delete schedule after first run (default: false)"
                            },
                            "context_messages": {
                                "type": "number",
                                "description": "Number of recent messages to include as context (0-10)"
                            },
                            "delivery": {
                                "type": "object",
                                "description": "Delivery behavior for schedule completion notifications",
                                "properties": {
                                    "mode": {
                                        "type": "string",
                                        "enum": ["none", "announce", "webhook"],
                                        "description": "Delivery mode"
                                    },
                                    "channel": {
                                        "type": "string",
                                        "description": "Announce channel override (defaults to source channel)"
                                    },
                                    "connector_id": {
                                        "type": "string",
                                        "description": "Announce connector override (defaults to source connector)"
                                    },
                                    "webhook_url": {
                                        "type": "string",
                                        "description": "Webhook URL when mode=webhook"
                                    }
                                }
                            }
                        },
                        "required": ["name", "schedule"]
                    },
                    "schedule_id": { 
                        "type": "string",
                        "description": "Schedule ID (required for update/remove/run actions)"
                    },
                    "patch": { 
                        "type": "object",
                        "description": "Partial update for 'update' action"
                    },
                    "include_payload_summary": {
                        "type": "boolean",
                        "description": "For action='list': include compact payload_preview text (default false). Use action='get' for full payload."
                    }
                },
                "required": ["action"]
            }),
        }
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let parsed: ScheduleInput = serde_json::from_value(input)
            .map_err(|e| anyhow!("invalid schedule tool input: {e}"))?;

        match parsed.action.as_str() {
            "list" => {
                let entries = self.manager.list().await;
                let include_payload_summary = parsed.include_payload_summary.unwrap_or(false);
                let summary = entries
                    .iter()
                    .map(|entry| {
                        let payload = entry.config.payload.as_ref();
                        serde_json::json!({
                            "schedule_id": entry.config.schedule_id,
                            "name": entry.config.name,
                            "enabled": entry.config.enabled,
                            "next_run": format_timestamp_ms(entry.state.next_run_at_ms),
                            "next_run_ms": entry.state.next_run_at_ms,
                            "last_run_at": format_timestamp_ms(entry.state.last_run_at_ms),
                            "last_status": entry.state.last_run_status,
                            "consecutive_errors": entry.state.consecutive_errors,
                            "has_payload": payload.is_some(),
                            "payload_kind": payload_kind(payload),
                            "payload_preview": if include_payload_summary {
                                payload_preview(payload, 120)
                            } else {
                                None
                            },
                        })
                    })
                    .collect::<Vec<_>>();
                Ok(tool_ok(serde_json::to_string_pretty(&summary)?))
            }
            "get" => {
                let Some(schedule_id) = parsed.schedule_id else {
                    return Ok(tool_error("schedule_id is required for get action"));
                };

                let entries = self.manager.list().await;
                let Some(entry) = entries
                    .into_iter()
                    .find(|e| e.config.schedule_id == schedule_id)
                else {
                    return Ok(tool_error(format!("schedule not found: {schedule_id}")));
                };

                let include_history = parsed.include_history.unwrap_or(true);
                let history_limit = parsed.history_limit.unwrap_or(5);
                let history = if include_history {
                    self.manager
                        .recent_history(&schedule_id, history_limit)
                        .await?
                } else {
                    Vec::new()
                };

                let detail = serde_json::json!({
                    "config": entry.config,
                    "state": entry.state,
                    "state_view": {
                        "next_run": format_timestamp_ms(entry.state.next_run_at_ms),
                        "next_run_ms": entry.state.next_run_at_ms,
                        "last_run_at": format_timestamp_ms(entry.state.last_run_at_ms),
                        "last_status": entry.state.last_run_status,
                        "consecutive_errors": entry.state.consecutive_errors,
                    },
                    "recent_history": history,
                });
                Ok(tool_ok(serde_json::to_string_pretty(&detail)?))
            }
            "add" => {
                let Some(job) = parsed.job else {
                    return Ok(tool_error("job is required for add action"));
                };

                let config = match job.into_config(&self.default_agent_id, ctx) {
                    Ok(c) => c,
                    Err(e) => return Ok(tool_error(format!("Invalid job: {e}"))),
                };
                self.manager.add_schedule(config.clone()).await?;
                let next = self.manager.get_next_run(&config.schedule_id).await;

                Ok(tool_ok(format!(
                    "Created schedule '{}' (id: {}). Next run: {:?}",
                    config.name, config.schedule_id, next
                )))
            }
            "update" => {
                let Some(schedule_id) = parsed.schedule_id else {
                    return Ok(tool_error("schedule_id is required for update action"));
                };
                let Some(patch) = parsed.patch else {
                    return Ok(tool_error("patch is required for update action"));
                };

                self.manager.update_schedule(&schedule_id, &patch).await?;
                Ok(tool_ok(format!("Updated schedule '{schedule_id}'")))
            }
            "remove" => {
                let Some(schedule_id) = parsed.schedule_id else {
                    return Ok(tool_error("schedule_id is required for remove action"));
                };

                self.manager.remove_schedule(&schedule_id).await?;
                Ok(tool_ok(format!("Removed schedule '{schedule_id}'")))
            }
            "run" => {
                let Some(schedule_id) = parsed.schedule_id else {
                    return Ok(tool_error("schedule_id is required for run action"));
                };

                self.manager.trigger_now(&schedule_id).await?;
                Ok(tool_ok(format!(
                    "Triggered immediate run of '{schedule_id}'"
                )))
            }
            other => Ok(tool_error(format!("Unknown action: {other}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use clawhive_bus::{EventBus, Topic};
    use clawhive_schema::BusMessage;
    use tempfile::TempDir;
    use tokio::time::{timeout, Duration};

    use super::ScheduleTool;
    use crate::tool::{ConversationMessage, ToolContext, ToolExecutor};

    fn setup() -> (
        Arc<clawhive_scheduler::ScheduleManager>,
        Arc<EventBus>,
        TempDir,
    ) {
        let tmp = TempDir::new().unwrap();
        let config_dir = tmp.path().join("config/schedules.d");
        let data_dir = tmp.path().join("data/schedules");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::create_dir_all(&data_dir).unwrap();

        let bus = Arc::new(EventBus::new(16));
        let manager = Arc::new(
            clawhive_scheduler::ScheduleManager::new(&config_dir, &data_dir, bus.clone()).unwrap(),
        );
        (manager, bus, tmp)
    }

    #[tokio::test]
    async fn add_action_supports_context_injection() {
        let (manager, _bus, _tmp) = setup();
        let tool = ScheduleTool::new(manager.clone());
        let ctx = ToolContext::builtin().with_recent_messages(vec![
            ConversationMessage {
                role: "user".to_string(),
                content: "remember milk".to_string(),
            },
            ConversationMessage {
                role: "assistant".to_string(),
                content: "I will remind you".to_string(),
            },
        ]);

        let result = tool
            .execute(
                serde_json::json!({
                    "action": "add",
                    "job": {
                        "name": "Milk reminder",
                        "schedule": { "kind": "at", "at": "20m" },
                        "payload": {
                            "kind": "agent_turn",
                            "message": "Remind user to buy milk"
                        },
                        "context_messages": 2,
                        "delete_after_run": true,
                        "session_mode": "main"
                    }
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        let entries = manager.list().await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].config.schedule_id, "milk-reminder");
        let payload_text = match entries[0].config.payload.as_ref().unwrap() {
            clawhive_scheduler::TaskPayload::AgentTurn { message, .. } => message.clone(),
            _ => panic!("expected AgentTurn"),
        };
        assert!(payload_text.contains("Recent context"));
    }

    #[tokio::test]
    async fn get_action_returns_payload_and_delivery_details() {
        let (manager, _bus, _tmp) = setup();
        let tool = ScheduleTool::new(manager.clone());
        let ctx = ToolContext::builtin();

        let add_result = tool
            .execute(
                serde_json::json!({
                    "action": "add",
                    "job": {
                        "name": "Payload detail",
                        "schedule": { "kind": "every", "interval_ms": 60000 },
                        "payload": {
                            "kind": "agent_turn",
                            "message": "check budget status",
                            "timeout_seconds": 600
                        },
                        "delivery": {
                            "mode": "announce",
                            "channel": "guild:1:channel:2",
                            "connector_id": "dc_main"
                        }
                    }
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!add_result.is_error);

        let schedule_id = manager.list().await[0].config.schedule_id.clone();
        let detail_result = tool
            .execute(
                serde_json::json!({
                    "action": "get",
                    "schedule_id": schedule_id
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!detail_result.is_error);
        let detail: serde_json::Value = serde_json::from_str(&detail_result.content).unwrap();
        assert_eq!(detail["config"]["payload"]["kind"], "agent_turn");
        assert_eq!(
            detail["config"]["payload"]["message"],
            "check budget status"
        );
        assert_eq!(detail["config"]["delivery"]["mode"], "announce");
        assert_eq!(detail["config"]["delivery"]["connector_id"], "dc_main");
    }

    #[tokio::test]
    async fn add_action_with_payload_direct_deliver() {
        let (manager, _bus, _tmp) = setup();
        let tool = ScheduleTool::new(manager.clone());
        let ctx = ToolContext::builtin();

        let result = tool
            .execute(
                serde_json::json!({
                    "action": "add",
                    "job": {
                        "name": "Direct reminder",
                        "schedule": { "kind": "at", "at": "5m" },
                        "payload": {
                            "kind": "direct_deliver",
                            "text": "Time to eat!"
                        }
                    }
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        let entries = manager.list().await;
        assert_eq!(entries.len(), 1);
    }

    #[tokio::test]
    async fn add_action_with_payload_agent_turn() {
        let (manager, _bus, _tmp) = setup();
        let tool = ScheduleTool::new(manager.clone());
        let ctx = ToolContext::builtin();

        let result = tool
            .execute(
                serde_json::json!({
                    "action": "add",
                    "job": {
                        "name": "Agent task",
                        "schedule": { "kind": "cron", "expr": "0 9 * * *" },
                        "payload": {
                            "kind": "agent_turn",
                            "message": "Generate daily report",
                            "timeout_seconds": 600
                        }
                    }
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        let entries = manager.list().await;
        assert_eq!(entries.len(), 1);
    }

    #[tokio::test]
    async fn add_action_requires_payload() {
        let (manager, _bus, _tmp) = setup();
        let tool = ScheduleTool::new(manager.clone());
        let ctx = ToolContext::builtin();

        let result = tool
            .execute(
                serde_json::json!({
                    "action": "add",
                    "job": {
                        "name": "No payload task",
                        "schedule": { "kind": "at", "at": "5m" }
                    }
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("payload"));
    }

    #[tokio::test]
    async fn list_action_returns_json_summary() {
        let (manager, _bus, _tmp) = setup();
        manager
            .add_schedule(clawhive_scheduler::ScheduleConfig {
                schedule_id: "daily-report".to_string(),
                enabled: true,
                name: "Daily Report".to_string(),
                description: None,
                schedule: clawhive_scheduler::ScheduleType::Cron {
                    expr: "0 9 * * *".to_string(),
                    tz: "UTC".to_string(),
                },
                agent_id: "clawhive-main".to_string(),
                session_mode: clawhive_scheduler::SessionMode::Isolated,
                payload: Some(clawhive_scheduler::TaskPayload::AgentTurn {
                    message: "generate report".to_string(),
                    model: None,
                    thinking: None,
                    timeout_seconds: 300,
                    light_context: false,
                }),
                timeout_seconds: 300,
                delete_after_run: false,
                delivery: clawhive_scheduler::DeliveryConfig::default(),
            })
            .await
            .unwrap();

        let tool = ScheduleTool::new(manager);
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(serde_json::json!({ "action": "list" }), &ctx)
            .await
            .unwrap();

        assert!(!result.is_error);
        let as_json: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert!(as_json.is_array());
        assert_eq!(as_json.as_array().unwrap().len(), 1);
        assert_eq!(as_json[0]["schedule_id"], "daily-report");
        assert!(as_json[0]["next_run"].is_string() || as_json[0]["next_run"].is_null());
        assert!(as_json[0]["next_run_ms"].is_number() || as_json[0]["next_run_ms"].is_null());
    }

    #[tokio::test]
    async fn list_action_exposes_payload_hints_without_full_payload_by_default() {
        let (manager, _bus, _tmp) = setup();
        manager
            .add_schedule(clawhive_scheduler::ScheduleConfig {
                schedule_id: "daily-digest".to_string(),
                enabled: true,
                name: "Daily Digest".to_string(),
                description: None,
                schedule: clawhive_scheduler::ScheduleType::Cron {
                    expr: "0 9 * * *".to_string(),
                    tz: "UTC".to_string(),
                },
                agent_id: "clawhive-main".to_string(),
                session_mode: clawhive_scheduler::SessionMode::Isolated,
                payload: Some(clawhive_scheduler::TaskPayload::AgentTurn {
                    message: "Generate daily digest from news headlines".to_string(),
                    model: None,
                    thinking: None,
                    timeout_seconds: 300,
                    light_context: false,
                }),
                timeout_seconds: 300,
                delete_after_run: false,
                delivery: clawhive_scheduler::DeliveryConfig::default(),
            })
            .await
            .unwrap();

        let tool = ScheduleTool::new(manager);
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(serde_json::json!({ "action": "list" }), &ctx)
            .await
            .unwrap();

        assert!(!result.is_error);
        let as_json: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(as_json[0]["has_payload"], true);
        assert_eq!(as_json[0]["payload_kind"], "agent_turn");
        assert!(as_json[0]["payload_preview"].is_null());
    }

    #[tokio::test]
    async fn list_action_can_include_payload_preview_when_requested() {
        let (manager, _bus, _tmp) = setup();
        manager
            .add_schedule(clawhive_scheduler::ScheduleConfig {
                schedule_id: "payload-preview".to_string(),
                enabled: true,
                name: "Payload Preview".to_string(),
                description: None,
                schedule: clawhive_scheduler::ScheduleType::Cron {
                    expr: "0 9 * * *".to_string(),
                    tz: "UTC".to_string(),
                },
                agent_id: "clawhive-main".to_string(),
                session_mode: clawhive_scheduler::SessionMode::Isolated,
                payload: Some(clawhive_scheduler::TaskPayload::AgentTurn {
                    message: "Generate and summarize long daily payload text".to_string(),
                    model: None,
                    thinking: None,
                    timeout_seconds: 300,
                    light_context: false,
                }),
                timeout_seconds: 300,
                delete_after_run: false,
                delivery: clawhive_scheduler::DeliveryConfig::default(),
            })
            .await
            .unwrap();

        let tool = ScheduleTool::new(manager);
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(
                serde_json::json!({ "action": "list", "include_payload_summary": true }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        let as_json: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert!(as_json[0]["payload_preview"].is_string());
    }

    #[tokio::test]
    async fn run_action_publishes_trigger_event() {
        let (manager, bus, _tmp) = setup();
        let tool = ScheduleTool::new(manager.clone());
        let ctx = ToolContext::builtin();

        let _ = tool
            .execute(
                serde_json::json!({
                    "action": "add",
                    "job": {
                        "name": "Run-now test",
                        "schedule": { "kind": "at", "at": "5m" },
                        "payload": {
                            "kind": "agent_turn",
                            "message": "Do test"
                        }
                    }
                }),
                &ctx,
            )
            .await
            .unwrap();

        let id = manager.list().await[0].config.schedule_id.clone();
        let mut rx = bus.subscribe(Topic::ScheduledTaskTriggered).await;
        let result = tool
            .execute(
                serde_json::json!({
                    "action": "run",
                    "schedule_id": id,
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        let msg = timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(msg, BusMessage::ScheduledTaskTriggered { .. }));
    }

    #[tokio::test]
    async fn remove_action_deletes_schedule() {
        let (manager, _bus, _tmp) = setup();
        let tool = ScheduleTool::new(manager.clone());
        let ctx = ToolContext::builtin();

        let add_result = tool
            .execute(
                serde_json::json!({
                    "action": "add",
                    "job": {
                        "name": "Delete me",
                        "schedule": { "kind": "at", "at": "1h" },
                        "payload": {
                            "kind": "agent_turn",
                            "message": "cleanup"
                        }
                    }
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(!add_result.is_error);

        let schedule_id = manager.list().await[0].config.schedule_id.clone();
        let remove_result = tool
            .execute(
                serde_json::json!({
                    "action": "remove",
                    "schedule_id": schedule_id,
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!remove_result.is_error);
        assert!(manager.list().await.is_empty());
    }

    #[tokio::test]
    async fn at_schedule_converts_relative_to_absolute_and_defaults_delete_after_run() {
        let (manager, _bus, _tmp) = setup();
        let tool = ScheduleTool::new(manager.clone());
        let ctx = ToolContext::builtin();

        let result = tool
            .execute(
                serde_json::json!({
                    "action": "add",
                    "job": {
                        "name": "One-shot reminder",
                        "schedule": { "kind": "at", "at": "5m" },
                        "payload": {
                            "kind": "agent_turn",
                            "message": "Do something"
                        }
                    }
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        let entries = manager.list().await;
        assert_eq!(entries.len(), 1);

        let config = &entries[0].config;

        // Should default to delete_after_run=true for "at" schedules
        assert!(
            config.delete_after_run,
            "at schedules should default to delete_after_run=true"
        );

        // The "at" time should be converted to an absolute ISO timestamp, not "5m"
        if let clawhive_scheduler::ScheduleType::At { at } = &config.schedule {
            assert!(
                at.contains("T") && at.contains(":"),
                "relative time '5m' should be converted to absolute ISO timestamp, got: {}",
                at
            );
            assert!(
                !at.ends_with("m") && !at.ends_with("h") && !at.ends_with("s"),
                "should not be a relative time string, got: {}",
                at
            );
        } else {
            panic!("expected At schedule type");
        }
    }

    #[tokio::test]
    async fn add_action_captures_source_user_scope() {
        let (manager, _bus, _tmp) = setup();
        let tool = ScheduleTool::new(manager.clone());
        let ctx = ToolContext::builtin()
            .with_source(
                "discord".into(),
                "dc_main".into(),
                "guild:1:channel:2".into(),
            )
            .with_source_user_scope("user:789".into());

        let result = tool
            .execute(
                serde_json::json!({
                    "action": "add",
                    "job": {
                        "name": "User scope test",
                        "schedule": { "kind": "at", "at": "5m" },
                        "payload": {
                            "kind": "agent_turn",
                            "message": "test task"
                        }
                    }
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        let entries = manager.list().await;
        assert_eq!(
            entries[0].config.delivery.source_user_scope.as_deref(),
            Some("user:789")
        );
    }

    #[test]
    fn try_parse_relative_ms_works() {
        use super::try_parse_relative_ms;

        assert_eq!(try_parse_relative_ms("1m"), Some(60_000));
        assert_eq!(try_parse_relative_ms("5m"), Some(300_000));
        assert_eq!(try_parse_relative_ms("2h"), Some(7_200_000));
        assert_eq!(try_parse_relative_ms("30s"), Some(30_000));
        assert_eq!(try_parse_relative_ms("1d"), Some(86_400_000));

        // Not relative times
        assert_eq!(try_parse_relative_ms("2026-02-25T10:00:00Z"), None);
        assert_eq!(try_parse_relative_ms("invalid"), None);
    }
}
