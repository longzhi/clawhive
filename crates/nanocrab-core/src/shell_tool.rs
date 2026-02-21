use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use corral_core::SandboxBuilder;
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

fn sandbox_for_workspace(workspace: &Path, timeout_secs: u64) -> Result<corral_core::Sandbox> {
    let mut builder = SandboxBuilder::new()
        .work_dir(workspace)
        .fs_read([workspace.to_string_lossy().to_string()])
        .fs_write([workspace.to_string_lossy().to_string()])
        .exec_allow(["sh"])
        .timeout(std::time::Duration::from_secs(timeout_secs.max(1)))
        .network_deny();

    if let Ok(path) = std::env::var("PATH") {
        builder = builder.env("PATH", &path);
    }

    if let Ok(home) = std::env::var("HOME") {
        builder = builder.env("HOME", &home);
    }

    if let Ok(tmpdir) = std::env::var("TMPDIR") {
        builder = builder.env("TMPDIR", &tmpdir);
    }

    builder.build()
}

#[async_trait]
impl ToolExecutor for ExecuteCommandTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "execute_command".into(),
            description: "Execute a shell command in a Corral sandbox scoped to the workspace directory. Returns stdout and stderr. Use for running builds, tests, scripts, or inspecting the system.".into(),
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
        let sandbox = sandbox_for_workspace(&workspace, timeout_secs)?;

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
