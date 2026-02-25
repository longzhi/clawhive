use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use nanocrab_provider::ToolDef;
use nanocrab_scheduler::{DeliveryConfig, DeliveryMode, ScheduleConfig, ScheduleManager, ScheduleType, SessionMode};
use serde::Deserialize;

use crate::tool::{ToolContext, ToolExecutor, ToolOutput};

pub const SCHEDULE_TOOL_NAME: &str = "schedule";

const DEFAULT_AGENT_ID: &str = "nanocrab-main";

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
}

#[derive(Debug, Deserialize)]
struct ScheduleJobInput {
    #[serde(default)]
    schedule_id: Option<String>,
    name: String,
    #[serde(default)]
    description: Option<String>,
    schedule: ScheduleType,
    task: String,
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
}

impl ScheduleJobInput {
    fn into_config(self, default_agent_id: &str, ctx: &ToolContext) -> ScheduleConfig {
        let mut task = self.task;

        if let Some(limit) = self.context_messages {
            if limit > 0 {
                let context = ctx
                    .recent_messages(limit)
                    .into_iter()
                    .map(|message| format!("- {}: {}", message.role, message.content))
                    .collect::<Vec<_>>()
                    .join("\n");

                if !context.is_empty() {
                    task = format!("{task}\n\nRecent context:\n{context}");
                }
            }
        }

        let delivery = self.delivery.unwrap_or(DeliveryInput {
            mode: None,
            channel: None,
            connector_id: None,
        });

        // Default to Announce delivery if source info available and mode not specified
        let delivery_mode = delivery.mode.unwrap_or_else(|| {
            if ctx.source_conversation_scope().is_some() {
                DeliveryMode::Announce
            } else {
                DeliveryMode::None
            }
        });

        ScheduleConfig {
            schedule_id: self
                .schedule_id
                .filter(|id| !id.trim().is_empty())
                .unwrap_or_else(|| slug_from_name(&self.name)),
            enabled: true,
            name: self.name,
            description: self.description,
            schedule: self.schedule,
            agent_id: self
                .agent_id
                .filter(|id| !id.trim().is_empty())
                .unwrap_or_else(|| default_agent_id.to_string()),
            session_mode: self.session_mode.unwrap_or(SessionMode::Isolated),
            task,
            timeout_seconds: self.timeout_seconds.unwrap_or(300),
            delete_after_run: self.delete_after_run.unwrap_or(false),
            delivery: DeliveryConfig {
                mode: delivery_mode,
                channel: delivery.channel,
                connector_id: delivery.connector_id,
                source_channel_type: ctx.source_channel_type().map(String::from),
                source_connector_id: ctx.source_connector_id().map(String::from),
                source_conversation_scope: ctx.source_conversation_scope().map(String::from),
            },
        }
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

#[async_trait]
impl ToolExecutor for ScheduleTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: SCHEDULE_TOOL_NAME.to_string(),
            description: "Manage scheduled tasks: list/add/update/remove/run for reminders and recurring jobs."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": { 
                        "type": "string", 
                        "enum": ["list", "add", "update", "remove", "run"],
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
                            "task": { 
                                "type": "string",
                                "description": "The task/reminder text to deliver"
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
                            }
                        },
                        "required": ["name", "schedule", "task"]
                    },
                    "schedule_id": { 
                        "type": "string",
                        "description": "Schedule ID (required for update/remove/run actions)"
                    },
                    "patch": { 
                        "type": "object",
                        "description": "Partial update for 'update' action"
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
                let summary = entries
                    .iter()
                    .map(|entry| {
                        serde_json::json!({
                            "schedule_id": entry.config.schedule_id,
                            "name": entry.config.name,
                            "enabled": entry.config.enabled,
                            "next_run": entry.state.next_run_at_ms,
                            "last_status": entry.state.last_run_status,
                            "consecutive_errors": entry.state.consecutive_errors,
                        })
                    })
                    .collect::<Vec<_>>();
                Ok(tool_ok(serde_json::to_string_pretty(&summary)?))
            }
            "add" => {
                let Some(job) = parsed.job else {
                    return Ok(tool_error("job is required for add action"));
                };

                let config = job.into_config(&self.default_agent_id, ctx);
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
                Ok(tool_ok(format!("Triggered immediate run of '{schedule_id}'")))
            }
            other => Ok(tool_error(format!("Unknown action: {other}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use nanocrab_bus::{EventBus, Topic};
    use nanocrab_schema::BusMessage;
    use tempfile::TempDir;
    use tokio::time::{timeout, Duration};

    use super::ScheduleTool;
    use crate::tool::{ConversationMessage, ToolContext, ToolExecutor};

    fn setup() -> (Arc<nanocrab_scheduler::ScheduleManager>, Arc<EventBus>, TempDir) {
        let tmp = TempDir::new().unwrap();
        let config_dir = tmp.path().join("config/schedules.d");
        let data_dir = tmp.path().join("data/schedules");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::create_dir_all(&data_dir).unwrap();

        let bus = Arc::new(EventBus::new(16));
        let manager = Arc::new(
            nanocrab_scheduler::ScheduleManager::new(&config_dir, &data_dir, bus.clone()).unwrap(),
        );
        (manager, bus, tmp)
    }

    #[tokio::test]
    async fn add_action_supports_context_injection() {
        let (manager, _bus, _tmp) = setup();
        let tool = ScheduleTool::new(manager.clone());
        let ctx = ToolContext::default_policy(std::path::Path::new("/tmp")).with_recent_messages(vec![
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
                        "task": "Remind user to buy milk",
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
        assert!(entries[0].config.task.contains("Recent context"));
    }

    #[tokio::test]
    async fn list_action_returns_json_summary() {
        let (manager, _bus, _tmp) = setup();
        manager
            .add_schedule(nanocrab_scheduler::ScheduleConfig {
                schedule_id: "daily-report".to_string(),
                enabled: true,
                name: "Daily Report".to_string(),
                description: None,
                schedule: nanocrab_scheduler::ScheduleType::Cron {
                    expr: "0 9 * * *".to_string(),
                    tz: "UTC".to_string(),
                },
                agent_id: "nanocrab-main".to_string(),
                session_mode: nanocrab_scheduler::SessionMode::Isolated,
                task: "generate report".to_string(),
                timeout_seconds: 300,
                delete_after_run: false,
                delivery: nanocrab_scheduler::DeliveryConfig::default(),
            })
            .await
            .unwrap();

        let tool = ScheduleTool::new(manager);
        let ctx = ToolContext::default_policy(std::path::Path::new("/tmp"));
        let result = tool
            .execute(serde_json::json!({ "action": "list" }), &ctx)
            .await
            .unwrap();

        assert!(!result.is_error);
        let as_json: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert!(as_json.is_array());
        assert_eq!(as_json.as_array().unwrap().len(), 1);
        assert_eq!(as_json[0]["schedule_id"], "daily-report");
    }

    #[tokio::test]
    async fn run_action_publishes_trigger_event() {
        let (manager, bus, _tmp) = setup();
        let tool = ScheduleTool::new(manager.clone());
        let ctx = ToolContext::default_policy(std::path::Path::new("/tmp"));

        let _ = tool
            .execute(
                serde_json::json!({
                    "action": "add",
                    "job": {
                        "name": "Run-now test",
                        "schedule": { "kind": "at", "at": "5m" },
                        "task": "Do test"
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
        let ctx = ToolContext::default_policy(std::path::Path::new("/tmp"));

        let add_result = tool
            .execute(
                serde_json::json!({
                    "action": "add",
                    "job": {
                        "name": "Delete me",
                        "schedule": { "kind": "at", "at": "1h" },
                        "task": "cleanup"
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
}
