//! Audit logging for tool executions.
//!
//! Provides structured logging of all tool calls for security review and debugging.

use chrono::{DateTime, Utc};
use serde::Serialize;

use super::policy::ToolOrigin;

/// Audit log entry for a tool execution.
#[derive(Debug, Clone, Serialize)]
pub struct ToolAuditEntry {
    /// When the tool was called
    pub timestamp: DateTime<Utc>,
    /// Name of the tool
    pub tool_name: String,
    /// Tool origin (builtin/external)
    pub origin: ToolOrigin,
    /// Truncated/sanitized input summary
    pub input_summary: String,
    /// Execution result
    pub result: ToolResult,
    /// Execution duration in milliseconds
    pub duration_ms: u64,
    /// Session identifier (if available)
    pub session_id: Option<String>,
    /// Agent that invoked the tool
    pub agent_id: Option<String>,
}

/// Tool execution result for audit purposes.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ToolResult {
    /// Tool executed successfully
    Ok {
        /// Truncated output preview
        output_preview: String,
    },
    /// Tool execution denied by policy
    Denied {
        /// Reason for denial
        reason: String,
    },
    /// Tool execution failed
    Error {
        /// Error message
        message: String,
    },
}

impl ToolAuditEntry {
    /// Create a new audit entry for a successful execution.
    pub fn success(
        tool_name: impl Into<String>,
        origin: ToolOrigin,
        input: &serde_json::Value,
        output: &str,
        duration_ms: u64,
    ) -> Self {
        Self {
            timestamp: Utc::now(),
            tool_name: tool_name.into(),
            origin,
            input_summary: summarize_input(input, 200),
            result: ToolResult::Ok {
                output_preview: truncate_string(output, 100),
            },
            duration_ms,
            session_id: None,
            agent_id: None,
        }
    }

    /// Create a new audit entry for a denied execution.
    pub fn denied(
        tool_name: impl Into<String>,
        origin: ToolOrigin,
        input: &serde_json::Value,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            timestamp: Utc::now(),
            tool_name: tool_name.into(),
            origin,
            input_summary: summarize_input(input, 200),
            result: ToolResult::Denied {
                reason: reason.into(),
            },
            duration_ms: 0,
            session_id: None,
            agent_id: None,
        }
    }

    /// Create a new audit entry for a failed execution.
    pub fn error(
        tool_name: impl Into<String>,
        origin: ToolOrigin,
        input: &serde_json::Value,
        error: impl Into<String>,
        duration_ms: u64,
    ) -> Self {
        Self {
            timestamp: Utc::now(),
            tool_name: tool_name.into(),
            origin,
            input_summary: summarize_input(input, 200),
            result: ToolResult::Error {
                message: error.into(),
            },
            duration_ms,
            session_id: None,
            agent_id: None,
        }
    }

    /// Set the session ID for this audit entry.
    pub fn with_session(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Set the agent ID for this audit entry.
    pub fn with_agent(mut self, agent_id: impl Into<String>) -> Self {
        self.agent_id = Some(agent_id.into());
        self
    }

    /// Emit this entry to the tracing log.
    pub fn emit(&self) {
        let result_status = match &self.result {
            ToolResult::Ok { .. } => "ok",
            ToolResult::Denied { .. } => "denied",
            ToolResult::Error { .. } => "error",
        };

        tracing::info!(
            target: "nanocrab::audit",
            tool = %self.tool_name,
            origin = %self.origin,
            result = result_status,
            duration_ms = self.duration_ms,
            session_id = ?self.session_id,
            agent_id = ?self.agent_id,
            input = %self.input_summary,
            "tool_execution"
        );

        // Also log denied/error at warn level for visibility
        match &self.result {
            ToolResult::Denied { reason } => {
                tracing::warn!(
                    target: "nanocrab::audit",
                    tool = %self.tool_name,
                    reason = %reason,
                    "tool execution denied"
                );
            }
            ToolResult::Error { message } => {
                tracing::warn!(
                    target: "nanocrab::audit",
                    tool = %self.tool_name,
                    error = %message,
                    "tool execution failed"
                );
            }
            ToolResult::Ok { .. } => {}
        }
    }
}

/// Generate a truncated/sanitized summary of tool input.
///
/// Removes potentially sensitive values and truncates to max length.
pub fn summarize_input(input: &serde_json::Value, max_len: usize) -> String {
    // For objects, show keys but potentially redact sensitive values
    let summary = match input {
        serde_json::Value::Object(map) => {
            let parts: Vec<String> = map
                .iter()
                .map(|(k, v)| {
                    let v_str = match v {
                        serde_json::Value::String(_) if is_sensitive_key(k) => {
                            "[REDACTED]".to_string()
                        }
                        serde_json::Value::String(s) if s.len() > 50 => {
                            format!("\"{}...\"", &s[..47])
                        }
                        _ => v.to_string(),
                    };
                    format!("{}:{}", k, v_str)
                })
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        _ => input.to_string(),
    };

    truncate_string(&summary, max_len)
}

/// Check if a key might contain sensitive data.
fn is_sensitive_key(key: &str) -> bool {
    let sensitive = [
        "password",
        "secret",
        "token",
        "key",
        "credential",
        "auth",
        "api_key",
        "apikey",
    ];
    let key_lower = key.to_lowercase();
    sensitive.iter().any(|s| key_lower.contains(s))
}

/// Truncate a string to max length, adding ellipsis if truncated.
fn truncate_string(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len.saturating_sub(3)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summarize_redacts_sensitive_keys() {
        let input = serde_json::json!({
            "username": "alice",
            "password": "secret123",
            "api_key": "sk-xxx",
            "data": "normal"
        });
        let summary = summarize_input(&input, 500);

        assert!(summary.contains("username"));
        assert!(summary.contains("alice"));
        assert!(summary.contains("[REDACTED]"));
        assert!(!summary.contains("secret123"));
        assert!(!summary.contains("sk-xxx"));
    }

    #[test]
    fn summarize_truncates_long_strings() {
        let input = serde_json::json!({
            "content": "a".repeat(100)
        });
        let summary = summarize_input(&input, 500);

        assert!(summary.len() < 100);
        assert!(summary.contains("..."));
    }

    #[test]
    fn truncate_works() {
        assert_eq!(truncate_string("short", 10), "short");
        assert_eq!(truncate_string("hello world", 8), "hello...");
    }

    #[test]
    fn audit_entry_serializes() {
        let entry = ToolAuditEntry::success(
            "read_file",
            ToolOrigin::Builtin,
            &serde_json::json!({"path": "/test.txt"}),
            "file contents",
            50,
        );

        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("read_file"));
        assert!(json.contains("builtin"));
        assert!(json.contains("ok"));
    }
}
