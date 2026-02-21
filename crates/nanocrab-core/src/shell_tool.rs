use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use corral_core::{
    start_broker, BrokerConfig, Permissions, PolicyEngine, Sandbox, SandboxConfig, ServiceHandler,
    ServicePermission,
};
use nanocrab_provider::ToolDef;

use super::tool::{ToolExecutor, ToolOutput};

const MAX_OUTPUT_BYTES: usize = 50_000;

pub struct ExecuteCommandTool {
    workspace: PathBuf,
    default_timeout: u64,
}

impl ExecuteCommandTool {
    pub fn new(workspace: PathBuf, default_timeout: u64) -> Self {
        Self {
            workspace,
            default_timeout,
        }
    }
}

struct RemindersHandler;

#[async_trait]
impl ServiceHandler for RemindersHandler {
    async fn handle(
        &self,
        method: &str,
        params: &serde_json::Value,
        policy: &PolicyEngine,
    ) -> Result<serde_json::Value> {
        match method {
            "list" => {
                let list = params.get("list").and_then(|v| v.as_str());
                if let Some(list_name) = list {
                    policy.check_reminders_scope_result(list_name)?;
                }
                policy.check_service_result("reminders", "list", params)?;

                let mut cmd = tokio::process::Command::new("remindctl");
                cmd.arg("list");
                if let Some(list_name) = list {
                    cmd.arg(list_name);
                }
                cmd.arg("--json").arg("--no-input");

                let output = cmd.output().await?;
                if !output.status.success() {
                    return Err(anyhow!(
                        "remindctl list failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                    ));
                }
                let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
                Ok(value)
            }
            "add" => {
                let list = params
                    .get("list")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("reminders.add requires 'list'"))?;
                let title = params
                    .get("title")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("reminders.add requires 'title'"))?;

                policy.check_service_result("reminders", "add", params)?;
                policy.check_reminders_scope_result(list)?;

                let mut cmd = tokio::process::Command::new("remindctl");
                cmd.arg("add")
                    .arg("--title")
                    .arg(title)
                    .arg("--list")
                    .arg(list)
                    .arg("--json")
                    .arg("--no-input");

                if let Some(due) = params.get("dueDate").and_then(|v| v.as_str()) {
                    cmd.arg("--due").arg(due);
                }
                if let Some(notes) = params.get("notes").and_then(|v| v.as_str()) {
                    cmd.arg("--notes").arg(notes);
                }
                if let Some(priority) = params.get("priority").and_then(|v| v.as_str()) {
                    cmd.arg("--priority").arg(priority);
                }

                let output = cmd.output().await?;
                if !output.status.success() {
                    return Err(anyhow!(
                        "remindctl add failed: {}",
                        String::from_utf8_lossy(&output.stderr)
                    ));
                }
                let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
                Ok(value)
            }
            _ => Err(anyhow!("Unknown reminders method: {method}")),
        }
    }

    fn namespace(&self) -> &str {
        "reminders"
    }
}

async fn sandbox_for_workspace(
    workspace: &Path,
    timeout_secs: u64,
    enable_reminders_service: bool,
    reminders_lists: &[String],
) -> Result<Sandbox> {
    let workspace_pattern = format!("{}/**", workspace.display());

    let mut permissions_builder = Permissions::builder()
        .fs_read([workspace_pattern.clone()])
        .fs_write([workspace_pattern])
        .exec_allow(["sh"])
        .network_deny()
        .env_allow(["PATH", "HOME", "TMPDIR"]);

    if enable_reminders_service {
        let mut scope = HashMap::new();
        if !reminders_lists.is_empty() {
            scope.insert("lists".to_string(), serde_json::json!(reminders_lists));
        }
        permissions_builder = permissions_builder.service(
            "reminders",
            ServicePermission {
                access: "readwrite".to_string(),
                scope,
            },
        );
    }

    let permissions = permissions_builder.build();

    let mut env_vars = HashMap::new();
    if let Ok(path) = std::env::var("PATH") {
        env_vars.insert("PATH".to_string(), path);
    }
    if let Ok(home) = std::env::var("HOME") {
        env_vars.insert("HOME".to_string(), home);
    }
    if let Ok(tmpdir) = std::env::var("TMPDIR") {
        env_vars.insert("TMPDIR".to_string(), tmpdir);
    }

    let mut broker_config = BrokerConfig::new(PolicyEngine::new(permissions.clone()));
    if enable_reminders_service {
        broker_config.register_handler(Arc::new(RemindersHandler));
    }
    let broker_handle = start_broker(broker_config).await?;

    let config = SandboxConfig {
        permissions,
        work_dir: workspace.to_path_buf(),
        data_dir: None,
        timeout: std::time::Duration::from_secs(timeout_secs.max(1)),
        max_memory_mb: Some(512),
        env_vars,
        broker_socket: Some(broker_handle.socket_path.clone()),
    };

    Sandbox::new(config)
}

#[async_trait]
impl ToolExecutor for ExecuteCommandTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "execute_command".into(),
            description: "Execute a shell command in a Corral sandbox scoped to the workspace directory. Returns stdout and stderr. Optional broker-backed reminders service can be enabled with explicit permission.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute (passed to sh -c)"
                    },
                    "timeout_seconds": {
                        "type": "integer",
                        "description": "Timeout in seconds (default: 30)"
                    },
                    "enable_reminders_service": {
                        "type": "boolean",
                        "description": "Enable broker-backed reminders service for this execution (default: false)"
                    },
                    "reminders_lists": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "Optional allowed reminder lists when reminders service is enabled"
                    }
                },
                "required": ["command"]
            }),
        }
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolOutput> {
        let command = input["command"]
            .as_str()
            .ok_or_else(|| anyhow!("missing 'command' field"))?;
        let timeout_secs = input["timeout_seconds"]
            .as_u64()
            .unwrap_or(self.default_timeout);
        let enable_reminders_service = input["enable_reminders_service"].as_bool().unwrap_or(false);
        let reminders_lists = input["reminders_lists"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        let workspace = self.workspace.clone();
        let command = command.to_string();
        let sandbox = sandbox_for_workspace(
            &workspace,
            timeout_secs,
            enable_reminders_service,
            &reminders_lists,
        )
        .await?;

        let result = sandbox.execute(&command).await;

        match result {
            Ok(output) => {
                let mut combined = String::new();

                if !output.stdout.is_empty() {
                    combined.push_str(&output.stdout);
                }
                if !output.stderr.is_empty() {
                    if !combined.is_empty() {
                        combined.push('\n');
                    }
                    combined.push_str("[stderr]\n");
                    combined.push_str(&output.stderr);
                }

                if combined.len() > MAX_OUTPUT_BYTES {
                    combined.truncate(MAX_OUTPUT_BYTES);
                    combined.push_str("\n...(output truncated)");
                }

                let exit_code = output.exit_code;
                let mut is_error = !output.exit_code.eq(&0);

                if output.was_killed {
                    is_error = true;
                    if !combined.is_empty() {
                        combined.push('\n');
                    }
                    combined.push_str("[killed: timeout exceeded]");
                }

                if exit_code != 0 {
                    combined.push_str(&format!("\n[exit code: {exit_code}]"));
                }

                Ok(ToolOutput {
                    content: if combined.is_empty() {
                        format!("[exit code: {exit_code}]")
                    } else {
                        combined
                    },
                    is_error,
                })
            }
            Err(e) => Ok(ToolOutput {
                content: format!("Failed to execute command: {e}"),
                is_error: true,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn echo_command() {
        let tmp = TempDir::new().unwrap();
        let tool = ExecuteCommandTool::new(tmp.path().to_path_buf(), 10);
        let result = tool
            .execute(serde_json::json!({"command": "echo hello"}))
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("hello"));
    }

    #[tokio::test]
    async fn failing_command() {
        let tmp = TempDir::new().unwrap();
        let tool = ExecuteCommandTool::new(tmp.path().to_path_buf(), 10);
        let result = tool
            .execute(serde_json::json!({"command": "exit 1"}))
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("exit code: 1"));
    }

    #[tokio::test]
    async fn timeout_command() {
        let tmp = TempDir::new().unwrap();
        let tool = ExecuteCommandTool::new(tmp.path().to_path_buf(), 1);
        let result = tool
            .execute(serde_json::json!({"command": "sleep 10", "timeout_seconds": 1}))
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("killed") || result.content.contains("Timeout"));
    }

    #[tokio::test]
    async fn runs_in_workspace_dir() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("marker.txt"), "found").unwrap();
        let tool = ExecuteCommandTool::new(tmp.path().to_path_buf(), 10);
        let result = tool
            .execute(serde_json::json!({"command": "cat marker.txt"}))
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("found"));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn denies_network_by_default_on_linux() {
        let tmp = TempDir::new().unwrap();
        let tool = ExecuteCommandTool::new(tmp.path().to_path_buf(), 10);
        let result = tool
            .execute(serde_json::json!({"command": "curl -sS https://example.com", "timeout_seconds": 5}))
            .await
            .unwrap();
        assert!(result.is_error);
    }
}
