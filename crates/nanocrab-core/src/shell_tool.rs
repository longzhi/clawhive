use std::path::PathBuf;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
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

#[async_trait]
impl ToolExecutor for ExecuteCommandTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "execute_command".into(),
            description: "Execute a shell command in the workspace directory. Returns stdout and stderr. Use for running builds, tests, scripts, or inspecting the system.".into(),
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

        let workspace = self.workspace.clone();
        let command = command.to_string();

        let result = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            tokio::process::Command::new("sh")
                .arg("-c")
                .arg(&command)
                .current_dir(&workspace)
                .output(),
        )
        .await;

        match result {
            Ok(Ok(output)) => {
                let mut combined = String::new();

                let stdout = String::from_utf8_lossy(&output.stdout);
                let stderr = String::from_utf8_lossy(&output.stderr);

                if !stdout.is_empty() {
                    combined.push_str(&stdout);
                }
                if !stderr.is_empty() {
                    if !combined.is_empty() {
                        combined.push('\n');
                    }
                    combined.push_str("[stderr]\n");
                    combined.push_str(&stderr);
                }

                if combined.len() > MAX_OUTPUT_BYTES {
                    combined.truncate(MAX_OUTPUT_BYTES);
                    combined.push_str("\n...(output truncated)");
                }

                let exit_code = output.status.code().unwrap_or(-1);
                if !output.status.success() {
                    combined.push_str(&format!("\n[exit code: {exit_code}]"));
                }

                Ok(ToolOutput {
                    content: if combined.is_empty() {
                        format!("[exit code: {exit_code}]")
                    } else {
                        combined
                    },
                    is_error: !output.status.success(),
                })
            }
            Ok(Err(e)) => Ok(ToolOutput {
                content: format!("Failed to execute command: {e}"),
                is_error: true,
            }),
            Err(_) => Ok(ToolOutput {
                content: format!("Command timed out after {timeout_secs}s"),
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
        assert!(result.content.contains("timed out"));
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
}
