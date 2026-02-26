//! WaitTask - Lightweight background task runner for polling conditions
//!
//! Unlike full LLM-driven agents, WaitTask runs simple command-based checks
//! without LLM involvement, notifying the session only when complete.

use std::process::Stdio;
use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::time::{interval, Duration};

use clawhive_bus::EventBus;
use clawhive_schema::BusMessage;

use crate::SqliteStore;

/// A lightweight wait task that polls a command until a condition is met
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaitTask {
    /// Unique task identifier
    pub id: String,
    /// Session key to notify on completion
    pub session_key: String,
    /// Command to execute for checking (shell command)
    pub check_cmd: String,
    /// Success condition: "contains:<text>", "equals:<text>", "regex:<pattern>", or "exit:0"
    pub success_condition: String,
    /// Failure condition (optional): triggers immediate failure
    #[serde(default)]
    pub failure_condition: Option<String>,
    /// Polling interval in milliseconds
    pub poll_interval_ms: u64,
    /// Absolute timeout timestamp (Unix ms)
    pub timeout_at_ms: i64,
    /// Creation timestamp
    pub created_at_ms: i64,
    /// Last check timestamp
    #[serde(default)]
    pub last_check_at_ms: Option<i64>,
    /// Current status
    #[serde(default)]
    pub status: WaitTaskStatus,
    /// Message to send on success
    #[serde(default)]
    pub on_success_message: Option<String>,
    /// Message to send on failure
    #[serde(default)]
    pub on_failure_message: Option<String>,
    /// Message to send on timeout
    #[serde(default)]
    pub on_timeout_message: Option<String>,
    /// Last check output (for debugging)
    #[serde(default)]
    pub last_output: Option<String>,
    /// Error message if failed
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WaitTaskStatus {
    #[default]
    Pending,
    Running,
    Success,
    Failed,
    Timeout,
    Cancelled,
}

impl WaitTask {
    /// Create a new wait task
    pub fn new(
        id: impl Into<String>,
        session_key: impl Into<String>,
        check_cmd: impl Into<String>,
        success_condition: impl Into<String>,
        poll_interval_ms: u64,
        timeout_ms: u64,
    ) -> Self {
        let now = Utc::now().timestamp_millis();
        Self {
            id: id.into(),
            session_key: session_key.into(),
            check_cmd: check_cmd.into(),
            success_condition: success_condition.into(),
            failure_condition: None,
            poll_interval_ms,
            timeout_at_ms: now + timeout_ms as i64,
            created_at_ms: now,
            last_check_at_ms: None,
            status: WaitTaskStatus::Pending,
            on_success_message: None,
            on_failure_message: None,
            on_timeout_message: None,
            last_output: None,
            error: None,
        }
    }

    /// Execute the check command and return (exit_code, stdout)
    async fn execute_check(&self) -> Result<(i32, String)> {
        let output = Command::new("sh")
            .arg("-c")
            .arg(&self.check_cmd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let code = output.status.code().unwrap_or(-1);
        Ok((code, stdout))
    }

    /// Evaluate the condition against command output
    fn evaluate_condition(condition: &str, exit_code: i32, output: &str) -> bool {
        if let Some(text) = condition.strip_prefix("contains:") {
            return output.contains(text);
        }
        if let Some(text) = condition.strip_prefix("equals:") {
            return output.trim() == text;
        }
        if let Some(pattern) = condition.strip_prefix("regex:") {
            if let Ok(re) = regex::Regex::new(pattern) {
                return re.is_match(output);
            }
            return false;
        }
        if let Some(code_str) = condition.strip_prefix("exit:") {
            if let Ok(expected) = code_str.parse::<i32>() {
                return exit_code == expected;
            }
        }
        // Default: check if output contains the condition text
        output.contains(condition)
    }

    /// Check if success condition is met
    fn is_success(&self, exit_code: i32, output: &str) -> bool {
        Self::evaluate_condition(&self.success_condition, exit_code, output)
    }

    /// Check if failure condition is met
    fn is_failure(&self, exit_code: i32, output: &str) -> bool {
        self.failure_condition
            .as_ref()
            .map(|c| Self::evaluate_condition(c, exit_code, output))
            .unwrap_or(false)
    }
}

/// Manages wait tasks with SQLite persistence and background execution
pub struct WaitTaskManager {
    store: Arc<SqliteStore>,
    bus: Arc<EventBus>,
}

impl WaitTaskManager {
    /// Create a new WaitTaskManager with SQLite storage
    pub fn new(store: Arc<SqliteStore>, bus: Arc<EventBus>) -> Self {
        Self { store, bus }
    }

    /// Add a new wait task
    pub async fn add(&self, task: WaitTask) -> Result<()> {
        self.store.save_wait_task(&task).await
    }

    /// Cancel a wait task
    pub async fn cancel(&self, task_id: &str) -> Result<bool> {
        if let Some(mut task) = self.store.get_wait_task(task_id).await? {
            if task.status == WaitTaskStatus::Pending || task.status == WaitTaskStatus::Running {
                task.status = WaitTaskStatus::Cancelled;
                self.store.save_wait_task(&task).await?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Get task by ID
    pub async fn get(&self, task_id: &str) -> Result<Option<WaitTask>> {
        self.store.get_wait_task(task_id).await
    }

    /// List all tasks for a session
    pub async fn list_by_session(&self, session_key: &str) -> Result<Vec<WaitTask>> {
        self.store.list_wait_tasks_by_session(session_key).await
    }

    /// Run the task manager loop
    pub async fn run(&self) {
        let mut ticker = interval(Duration::from_secs(1));

        loop {
            ticker.tick().await;
            let now = Utc::now().timestamp_millis();

            // Load pending tasks from SQLite
            let tasks = match self.store.load_pending_wait_tasks().await {
                Ok(t) => t,
                Err(e) => {
                    tracing::error!(error = %e, "Failed to load pending wait tasks");
                    continue;
                }
            };

            // Filter tasks that need checking
            let tasks_to_check: Vec<WaitTask> = tasks
                .into_iter()
                .filter(|t| {
                    t.last_check_at_ms
                        .map(|last| now - last >= t.poll_interval_ms as i64)
                        .unwrap_or(true)
                })
                .collect();

            for task in tasks_to_check {
                self.process_task(&task, now).await;
            }

            // Cleanup old completed tasks (older than 24 hours)
            let cutoff = now - 24 * 60 * 60 * 1000;
            if let Err(e) = self.store.cleanup_old_wait_tasks(cutoff).await {
                tracing::warn!(error = %e, "Failed to cleanup old wait tasks");
            }
        }
    }

    async fn process_task(&self, task: &WaitTask, now: i64) {
        let task_id = task.id.clone();

        // Check timeout first
        if now >= task.timeout_at_ms {
            self.complete_task(&task_id, WaitTaskStatus::Timeout, None, None)
                .await;
            return;
        }

        // Execute check
        match task.execute_check().await {
            Ok((exit_code, output)) => {
                // Update last check in database
                let mut updated_task = task.clone();
                updated_task.last_check_at_ms = Some(now);
                updated_task.last_output = Some(output.clone());
                updated_task.status = WaitTaskStatus::Running;

                if let Err(e) = self.store.save_wait_task(&updated_task).await {
                    tracing::warn!(task_id = %task_id, error = %e, "Failed to update task");
                }

                // Evaluate conditions
                if task.is_failure(exit_code, &output) {
                    self.complete_task(
                        &task_id,
                        WaitTaskStatus::Failed,
                        Some(output),
                        Some("Failure condition matched".to_string()),
                    )
                    .await;
                } else if task.is_success(exit_code, &output) {
                    self.complete_task(&task_id, WaitTaskStatus::Success, Some(output), None)
                        .await;
                }
                // Otherwise, continue polling
            }
            Err(e) => {
                tracing::warn!(task_id = %task_id, error = %e, "Wait task check failed");
                // Update last check time but don't fail - transient errors are ok
                let mut updated_task = task.clone();
                updated_task.last_check_at_ms = Some(now);
                updated_task.error = Some(e.to_string());
                let _ = self.store.save_wait_task(&updated_task).await;
            }
        }
    }

    async fn complete_task(
        &self,
        task_id: &str,
        status: WaitTaskStatus,
        output: Option<String>,
        error: Option<String>,
    ) {
        // Load and update task
        let task = match self.store.get_wait_task(task_id).await {
            Ok(Some(mut t)) => {
                t.status = status.clone();
                t.last_output = output;
                t.error = error;
                if let Err(e) = self.store.save_wait_task(&t).await {
                    tracing::error!(task_id = %task_id, error = %e, "Failed to save completed task");
                }
                t
            }
            _ => return,
        };

        // Build notification message
        let message = match status {
            WaitTaskStatus::Success => task
                .on_success_message
                .unwrap_or_else(|| format!("✅ Task '{}' completed successfully", task.id)),
            WaitTaskStatus::Failed => task
                .on_failure_message
                .unwrap_or_else(|| format!("❌ Task '{}' failed", task.id)),
            WaitTaskStatus::Timeout => task
                .on_timeout_message
                .unwrap_or_else(|| format!("⏱️ Task '{}' timed out", task.id)),
            _ => return,
        };

        // Notify via event bus
        let event = BusMessage::WaitTaskCompleted {
            task_id: task.id,
            session_key: task.session_key,
            status: format!("{:?}", status).to_lowercase(),
            message,
            output: task.last_output,
        };

        if let Err(e) = self.bus.publish(event).await {
            tracing::error!(error = %e, "Failed to publish wait task completion event");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_condition_contains() {
        assert!(WaitTask::evaluate_condition(
            "contains:success",
            0,
            "Build success!"
        ));
        assert!(!WaitTask::evaluate_condition(
            "contains:success",
            0,
            "Build failed"
        ));
    }

    #[test]
    fn test_condition_equals() {
        assert!(WaitTask::evaluate_condition("equals:done", 0, "done\n"));
        assert!(!WaitTask::evaluate_condition("equals:done", 0, "not done"));
    }

    #[test]
    fn test_condition_exit_code() {
        assert!(WaitTask::evaluate_condition("exit:0", 0, ""));
        assert!(!WaitTask::evaluate_condition("exit:0", 1, ""));
    }

    #[test]
    fn test_condition_regex() {
        assert!(WaitTask::evaluate_condition(
            r"regex:status:\s*complete",
            0,
            "status: complete"
        ));
    }
}
