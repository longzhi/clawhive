//! Agent tool for creating and managing wait tasks

use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use clawhive_provider::ToolDef;
use clawhive_scheduler::{WaitTask, WaitTaskManager};
use serde::Deserialize;

use crate::tool::{ToolContext, ToolExecutor, ToolOutput};

pub const WAIT_TOOL_NAME: &str = "wait_task";

pub struct WaitTool {
    manager: Arc<WaitTaskManager>,
}

impl WaitTool {
    pub fn new(manager: Arc<WaitTaskManager>) -> Self {
        Self { manager }
    }
}

#[derive(Debug, Deserialize)]
struct WaitInput {
    action: String,
    #[serde(default)]
    task: Option<WaitTaskInput>,
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    session_key: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WaitTaskInput {
    id: String,
    check_cmd: String,
    success_condition: String,
    #[serde(default)]
    failure_condition: Option<String>,
    #[serde(default)]
    poll_interval_ms: Option<u64>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    on_success: Option<String>,
    #[serde(default)]
    on_failure: Option<String>,
    #[serde(default)]
    on_timeout: Option<String>,
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
impl ToolExecutor for WaitTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: WAIT_TOOL_NAME.to_string(),
            description: "Create background polling tasks that wait for a condition without LLM involvement. Use for CI checks, process monitoring, file watching, etc.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["add", "list", "cancel", "status"],
                        "description": "Action to perform"
                    },
                    "task": {
                        "type": "object",
                        "description": "Task definition (required for 'add' action)",
                        "properties": {
                            "id": {
                                "type": "string",
                                "description": "Unique task identifier"
                            },
                            "check_cmd": {
                                "type": "string",
                                "description": "Shell command to execute for checking"
                            },
                            "success_condition": {
                                "type": "string",
                                "description": "Condition: 'contains:text', 'equals:text', 'regex:pattern', or 'exit:0'"
                            },
                            "failure_condition": {
                                "type": "string",
                                "description": "Optional: condition that triggers immediate failure"
                            },
                            "poll_interval_ms": {
                                "type": "number",
                                "description": "Polling interval in milliseconds (default: 30000)"
                            },
                            "timeout_ms": {
                                "type": "number",
                                "description": "Timeout in milliseconds (default: 600000 = 10 minutes)"
                            },
                            "on_success": {
                                "type": "string",
                                "description": "Message to send when condition is met"
                            },
                            "on_failure": {
                                "type": "string",
                                "description": "Message to send on failure"
                            },
                            "on_timeout": {
                                "type": "string",
                                "description": "Message to send on timeout"
                            }
                        },
                        "required": ["id", "check_cmd", "success_condition"]
                    },
                    "task_id": {
                        "type": "string",
                        "description": "Task ID (required for cancel/status actions)"
                    },
                    "session_key": {
                        "type": "string",
                        "description": "Filter tasks by session (for list action)"
                    }
                },
                "required": ["action"]
            }),
        }
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let parsed: WaitInput = serde_json::from_value(input)
            .map_err(|e| anyhow!("invalid wait_task input: {e}"))?;

        match parsed.action.as_str() {
            "add" => {
                let Some(task_input) = parsed.task else {
                    return Ok(tool_error("task is required for add action"));
                };

                let session_key = ctx.session_key().to_string();
                let mut task = WaitTask::new(
                    &task_input.id,
                    &session_key,
                    &task_input.check_cmd,
                    &task_input.success_condition,
                    task_input.poll_interval_ms.unwrap_or(30_000),
                    task_input.timeout_ms.unwrap_or(600_000),
                );

                task.failure_condition = task_input.failure_condition;
                task.on_success_message = task_input.on_success;
                task.on_failure_message = task_input.on_failure;
                task.on_timeout_message = task_input.on_timeout;

                self.manager.add(task).await?;

                Ok(tool_ok(format!(
                    "Created wait task '{}'. Polling every {}ms, timeout in {}ms.",
                    task_input.id,
                    task_input.poll_interval_ms.unwrap_or(30_000),
                    task_input.timeout_ms.unwrap_or(600_000),
                )))
            }
            "list" => {
                let session_key = parsed
                    .session_key
                    .unwrap_or_else(|| ctx.session_key().to_string());

                let tasks = self.manager.list_by_session(&session_key).await?;

                if tasks.is_empty() {
                    return Ok(tool_ok("No wait tasks found."));
                }

                let summary = tasks
                    .iter()
                    .map(|t| {
                        serde_json::json!({
                            "id": t.id,
                            "status": format!("{:?}", t.status).to_lowercase(),
                            "check_cmd": t.check_cmd,
                            "success_condition": t.success_condition,
                            "poll_interval_ms": t.poll_interval_ms,
                            "last_check_at_ms": t.last_check_at_ms,
                        })
                    })
                    .collect::<Vec<_>>();

                Ok(tool_ok(serde_json::to_string_pretty(&summary)?))
            }
            "cancel" => {
                let Some(task_id) = parsed.task_id else {
                    return Ok(tool_error("task_id is required for cancel action"));
                };

                if self.manager.cancel(&task_id).await? {
                    Ok(tool_ok(format!("Cancelled wait task '{task_id}'")))
                } else {
                    Ok(tool_error(format!(
                        "Wait task '{task_id}' not found or already completed"
                    )))
                }
            }
            "status" => {
                let Some(task_id) = parsed.task_id else {
                    return Ok(tool_error("task_id is required for status action"));
                };

                match self.manager.get(&task_id).await? {
                    Some(task) => {
                        let status = serde_json::json!({
                            "id": task.id,
                            "status": format!("{:?}", task.status).to_lowercase(),
                            "check_cmd": task.check_cmd,
                            "success_condition": task.success_condition,
                            "failure_condition": task.failure_condition,
                            "poll_interval_ms": task.poll_interval_ms,
                            "timeout_at_ms": task.timeout_at_ms,
                            "last_check_at_ms": task.last_check_at_ms,
                            "last_output": task.last_output,
                            "error": task.error,
                        });
                        Ok(tool_ok(serde_json::to_string_pretty(&status)?))
                    }
                    None => Ok(tool_error(format!("Wait task '{task_id}' not found"))),
                }
            }
            other => Ok(tool_error(format!("Unknown action: {other}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clawhive_bus::EventBus;
    use clawhive_scheduler::SqliteStore;
    use tempfile::TempDir;

    fn setup() -> (Arc<WaitTaskManager>, TempDir) {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.db");
        let store = Arc::new(SqliteStore::open(&db_path).unwrap());
        let bus = Arc::new(EventBus::new(16));
        let manager = Arc::new(WaitTaskManager::new(store, bus));
        (manager, tmp)
    }

    #[tokio::test]
    async fn add_action_creates_task() {
        let (manager, _tmp) = setup();
        let tool = WaitTool::new(manager.clone());
        let ctx = ToolContext::builtin().with_session_key("test:session:1");

        let result = tool
            .execute(
                serde_json::json!({
                    "action": "add",
                    "task": {
                        "id": "ci-check",
                        "check_cmd": "echo success",
                        "success_condition": "contains:success",
                        "poll_interval_ms": 5000,
                        "timeout_ms": 60000,
                        "on_success": "CI passed!"
                    }
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("ci-check"));

        let tasks = manager.list_by_session("test:session:1").await.unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "ci-check");
    }

    #[tokio::test]
    async fn cancel_action_cancels_task() {
        let (manager, _tmp) = setup();
        let tool = WaitTool::new(manager.clone());
        let ctx = ToolContext::builtin().with_session_key("test:session:2");

        // Add a task first
        let _ = tool
            .execute(
                serde_json::json!({
                    "action": "add",
                    "task": {
                        "id": "to-cancel",
                        "check_cmd": "sleep 100",
                        "success_condition": "exit:0"
                    }
                }),
                &ctx,
            )
            .await
            .unwrap();

        // Cancel it
        let result = tool
            .execute(
                serde_json::json!({
                    "action": "cancel",
                    "task_id": "to-cancel"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("Cancelled"));
    }

    #[tokio::test]
    async fn list_action_shows_tasks() {
        let (manager, _tmp) = setup();
        let tool = WaitTool::new(manager.clone());
        let ctx = ToolContext::builtin().with_session_key("test:session:3");

        // Add two tasks
        for i in 1..=2 {
            let _ = tool
                .execute(
                    serde_json::json!({
                        "action": "add",
                        "task": {
                            "id": format!("task-{i}"),
                            "check_cmd": "echo ok",
                            "success_condition": "exit:0"
                        }
                    }),
                    &ctx,
                )
                .await
                .unwrap();
        }

        let result = tool
            .execute(serde_json::json!({ "action": "list" }), &ctx)
            .await
            .unwrap();

        assert!(!result.is_error);
        let tasks: serde_json::Value = serde_json::from_str(&result.content).unwrap();
        assert_eq!(tasks.as_array().unwrap().len(), 2);
    }
}
