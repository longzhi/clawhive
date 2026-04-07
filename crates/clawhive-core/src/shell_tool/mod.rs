mod exec_approval;
mod execution;
mod network;
mod sandbox;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clawhive_bus::BusPublisher;
use clawhive_provider::LlmMessage;

use super::access_gate::AccessGate;
use super::approval::ApprovalRegistry;
use super::config::{ExecSecurityConfig, SandboxPolicyConfig};
use super::router::LlmRouter;

pub use sandbox::{augment_path_like_host, default_path_candidates};

const MAX_OUTPUT_BYTES: usize = 20_000;

/// Lightweight handle for generating approval summaries via LLM.
#[derive(Clone)]
pub struct ApprovalSummarizer {
    router: LlmRouter,
    model: String,
    fallbacks: Vec<String>,
}

impl ApprovalSummarizer {
    pub fn new(router: LlmRouter, model: String, fallbacks: Vec<String>) -> Self {
        Self {
            router,
            model,
            fallbacks,
        }
    }

    pub async fn summarize(&self, command: &str, network_target: Option<&str>) -> Option<String> {
        let mut prompt = format!(
            "Describe what the following command does in one short sentence, in the same language as the command context. \
             Use plain language a non-technical user can understand. Do not include the command itself. \
             Output only the summary sentence, nothing else.\n\nCommand: {command}"
        );
        if let Some(target) = network_target {
            prompt.push_str(&format!("\nNetwork target: {target}"));
        }

        let result = tokio::time::timeout(
            Duration::from_secs(5),
            self.router.chat(
                &self.model,
                &self.fallbacks,
                Some(
                    "You are a concise command summarizer. Output only one summary sentence, nothing else."
                        .to_string(),
                ),
                vec![LlmMessage::user(&prompt)],
                128,
            ),
        )
        .await;

        match result {
            Ok(Ok(resp)) => {
                let text = resp.text.trim().to_string();
                if text.is_empty() {
                    None
                } else {
                    Some(text)
                }
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "approval summary LLM call failed");
                None
            }
            Err(_) => {
                tracing::warn!("approval summary LLM call timed out");
                None
            }
        }
    }
}

pub struct ExecuteCommandTool {
    workspace: PathBuf,
    default_timeout: u64,
    gate: Arc<AccessGate>,
    exec_security: ExecSecurityConfig,
    sandbox_config: SandboxPolicyConfig,
    approval_registry: Option<Arc<ApprovalRegistry>>,
    bus: Option<BusPublisher>,
    agent_id: String,
    approval_summarizer: Option<ApprovalSummarizer>,
}

impl ExecuteCommandTool {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        workspace: PathBuf,
        default_timeout: u64,
        gate: Arc<AccessGate>,
        exec_security: ExecSecurityConfig,
        sandbox_config: SandboxPolicyConfig,
        approval_registry: Option<Arc<ApprovalRegistry>>,
        bus: Option<BusPublisher>,
        agent_id: String,
        approval_summarizer: Option<ApprovalSummarizer>,
    ) -> Self {
        Self {
            workspace,
            default_timeout,
            gate,
            exec_security,
            sandbox_config,
            approval_registry,
            bus,
            agent_id,
            approval_summarizer,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::access_gate::AccessGate;
    use crate::approval::ApprovalRegistry;
    use crate::config::{
        ExecAskMode, ExecSecurityConfig, ExecSecurityMode, SandboxNetworkMode, SandboxPolicyConfig,
    };
    use crate::tool::{ToolContext, ToolExecutor};
    use clawhive_schema::ApprovalDecision;
    use tempfile::TempDir;

    fn make_gate(workspace: &Path) -> Arc<AccessGate> {
        Arc::new(AccessGate::in_memory(workspace.to_path_buf()))
    }

    fn make_tool(tmp: &TempDir) -> ExecuteCommandTool {
        let gate = make_gate(tmp.path());
        ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            10,
            gate,
            ExecSecurityConfig::default(),
            SandboxPolicyConfig::default(),
            None,
            None,
            "test-agent".to_string(),
            None,
        )
    }

    fn make_full_mode_tool(tmp: &TempDir, timeout: u64) -> ExecuteCommandTool {
        let gate = make_gate(tmp.path());
        ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            timeout,
            gate,
            ExecSecurityConfig {
                security: ExecSecurityMode::Full,
                ..ExecSecurityConfig::default()
            },
            SandboxPolicyConfig::default(),
            None,
            None,
            "test-agent".to_string(),
            None,
        )
    }

    #[tokio::test]
    async fn echo_command() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(serde_json::json!({"command": "echo hello"}), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("hello"));
    }

    #[tokio::test]
    async fn failing_command() {
        let tmp = TempDir::new().unwrap();
        let tool = make_full_mode_tool(&tmp, 10);
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(serde_json::json!({"command": "exit 1"}), &ctx)
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("exit code: 1"));
    }

    #[tokio::test]
    async fn timeout_command() {
        let tmp = TempDir::new().unwrap();
        let tool = make_full_mode_tool(&tmp, 1);
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(
                serde_json::json!({"command": "sleep 10", "timeout_seconds": 1}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("killed") || result.content.contains("Timeout"));
    }

    #[tokio::test]
    async fn runs_in_workspace_dir() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("marker.txt"), "found").unwrap();
        let tool = make_tool(&tmp);
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(serde_json::json!({"command": "cat marker.txt"}), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("found"));
    }

    #[tokio::test]
    async fn external_context_requires_exec_permission() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("data.txt"), "hello").unwrap();

        let tool = make_tool(&tmp);

        // External context with cat allowed
        let perms = corral_core::Permissions {
            fs: corral_core::FsPermissions {
                read: vec![format!("{}/**", tmp.path().display())],
                write: vec![],
            },
            network: corral_core::NetworkPermissions { allow: vec![] },
            exec: vec!["cat".into()],
            env: vec![],
            services: Default::default(),
        };
        let ctx = ToolContext::external(perms);

        let result = tool
            .execute(serde_json::json!({"command": "cat data.txt"}), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("hello"));
    }

    #[tokio::test]
    async fn external_context_denies_unlisted_command() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);

        // External context with only echo allowed
        let perms = corral_core::Permissions {
            fs: corral_core::FsPermissions::default(),
            network: corral_core::NetworkPermissions { allow: vec![] },
            exec: vec!["echo".into()],
            env: vec![],
            services: Default::default(),
        };
        let ctx = ToolContext::external(perms);

        // Try to run ls (not in exec list)
        let result = tool
            .execute(serde_json::json!({"command": "ls"}), &ctx)
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("denied"));
    }

    #[tokio::test]
    async fn hard_baseline_blocks_dangerous_command() {
        let tmp = TempDir::new().unwrap();
        let tool = make_full_mode_tool(&tmp, 10);

        // Even builtin context should block dangerous commands
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(serde_json::json!({"command": "rm -rf /"}), &ctx)
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("denied"));
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn denies_network_by_default_on_linux() {
        let tmp = TempDir::new().unwrap();
        let tool = make_tool(&tmp);
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(
                serde_json::json!({"command": "curl -sS https://example.com", "timeout_seconds": 5}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn exec_security_deny_blocks_all_commands() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let tool = ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            10,
            gate,
            ExecSecurityConfig {
                security: ExecSecurityMode::Deny,
                ..ExecSecurityConfig::default()
            },
            SandboxPolicyConfig::default(),
            None,
            None,
            "test-agent".to_string(),
            None,
        );
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(serde_json::json!({"command": "echo denied"}), &ctx)
            .await
            .unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("exec is disabled"));
    }

    #[tokio::test]
    async fn exec_security_allowlist_blocks_unlisted_commands() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let tool = ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            10,
            gate,
            ExecSecurityConfig {
                security: ExecSecurityMode::Allowlist,
                allowlist: vec!["git *".into()],
                safe_bins: vec![],
                ..ExecSecurityConfig::default()
            },
            SandboxPolicyConfig::default(),
            None,
            None,
            "test-agent".to_string(),
            None,
        );
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(serde_json::json!({"command": "python --version"}), &ctx)
            .await
            .unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("not in allowlist"));
    }

    #[tokio::test]
    async fn exec_security_full_allows_non_baseline_command() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let tool = ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            10,
            gate,
            ExecSecurityConfig {
                security: ExecSecurityMode::Full,
                allowlist: vec![],
                safe_bins: vec![],
                ..ExecSecurityConfig::default()
            },
            SandboxPolicyConfig::default(),
            None,
            None,
            "test-agent".to_string(),
            None,
        );
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(serde_json::json!({"command": "echo allowed"}), &ctx)
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("allowed"));
    }

    #[test]
    fn is_command_allowed_matches_allowlist_patterns() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let tool = ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            10,
            gate,
            ExecSecurityConfig {
                security: ExecSecurityMode::Allowlist,
                allowlist: vec!["git *".into(), "pwd".into()],
                safe_bins: vec![],
                ..ExecSecurityConfig::default()
            },
            SandboxPolicyConfig::default(),
            None,
            None,
            "test-agent".to_string(),
            None,
        );

        assert!(tool.is_command_allowed("git status"));
        assert!(tool.is_command_allowed("git"));
        assert!(tool.is_command_allowed("pwd"));
        assert!(!tool.is_command_allowed("ls -la"));
    }

    #[test]
    fn is_command_allowed_accepts_safe_bins() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let tool = ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            10,
            gate,
            ExecSecurityConfig {
                security: ExecSecurityMode::Allowlist,
                allowlist: vec![],
                safe_bins: vec!["jq".into()],
                ..ExecSecurityConfig::default()
            },
            SandboxPolicyConfig::default(),
            None,
            None,
            "test-agent".to_string(),
            None,
        );

        assert!(tool.is_command_allowed("jq --version"));
        assert!(tool.is_command_allowed("/usr/bin/jq .foo data.json"));
        assert!(!tool.is_command_allowed("cat data.json"));
    }

    #[tokio::test]
    async fn allowlist_onmiss_waits_for_allow_once_and_executes() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let approval_registry = Arc::new(ApprovalRegistry::new());
        let tool = ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            10,
            gate,
            ExecSecurityConfig {
                security: ExecSecurityMode::Allowlist,
                ask: ExecAskMode::OnMiss,
                allowlist: vec![],
                safe_bins: vec![],
            },
            SandboxPolicyConfig::default(),
            Some(approval_registry.clone()),
            None,
            "agent-test".to_string(),
            None,
        );
        let ctx = ToolContext::builtin();

        let tool_task = tokio::spawn(async move {
            tool.execute(serde_json::json!({"command": "printf approved"}), &ctx)
                .await
                .unwrap()
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(approval_registry.has_pending().await);

        let pending = approval_registry.pending_list().await;
        let (trace_id, _, _) = pending.first().unwrap();
        approval_registry
            .resolve(*trace_id, ApprovalDecision::AllowOnce)
            .await
            .unwrap();

        let output = tool_task.await.unwrap();
        assert!(!output.is_error);
        assert!(output.content.contains("approved"));
    }

    #[tokio::test]
    async fn allowlist_onmiss_deny_blocks_execution() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let approval_registry = Arc::new(ApprovalRegistry::new());
        let tool = ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            10,
            gate,
            ExecSecurityConfig {
                security: ExecSecurityMode::Allowlist,
                ask: ExecAskMode::OnMiss,
                allowlist: vec![],
                safe_bins: vec![],
            },
            SandboxPolicyConfig::default(),
            Some(approval_registry.clone()),
            None,
            "agent-test".to_string(),
            None,
        );
        let ctx = ToolContext::builtin();

        let tool_task = tokio::spawn(async move {
            tool.execute(serde_json::json!({"command": "printf denied"}), &ctx)
                .await
                .unwrap()
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        let pending = approval_registry.pending_list().await;
        let (trace_id, _, _) = pending.first().unwrap();
        approval_registry
            .resolve(*trace_id, ApprovalDecision::Deny)
            .await
            .unwrap();

        let output = tool_task.await.unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("denied"));
    }

    #[tokio::test]
    async fn always_allow_persists_for_same_agent_via_registry() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let approval_registry = Arc::new(ApprovalRegistry::new());

        let tool = ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            10,
            gate.clone(),
            ExecSecurityConfig {
                security: ExecSecurityMode::Allowlist,
                ask: ExecAskMode::OnMiss,
                allowlist: vec![],
                safe_bins: vec![],
            },
            SandboxPolicyConfig::default(),
            Some(approval_registry.clone()),
            None,
            "agent-test".to_string(),
            None,
        );
        let ctx = ToolContext::builtin();

        let first = tokio::spawn(async move {
            tool.execute(serde_json::json!({"command": "printf persist"}), &ctx)
                .await
                .unwrap()
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let pending = approval_registry.pending_list().await;
        let (trace_id, _, _) = pending.first().unwrap();
        approval_registry
            .resolve(*trace_id, ApprovalDecision::AlwaysAllow)
            .await
            .unwrap();
        let first_output = first.await.unwrap();
        assert!(!first_output.is_error);

        let tool_again = ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            10,
            gate,
            ExecSecurityConfig {
                security: ExecSecurityMode::Allowlist,
                ask: ExecAskMode::OnMiss,
                allowlist: vec![],
                safe_bins: vec![],
            },
            SandboxPolicyConfig::default(),
            Some(approval_registry.clone()),
            None,
            "agent-test".to_string(),
            None,
        );
        let ctx2 = ToolContext::builtin();
        let second = tokio::spawn(async move {
            tool_again
                .execute(serde_json::json!({"command": "printf persist"}), &ctx2)
                .await
                .unwrap()
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !approval_registry.has_pending().await,
            "second execution should not require approval"
        );

        let second_output = second.await.unwrap();
        assert!(!second_output.is_error);
        assert!(second_output.content.contains("persist"));
    }

    #[tokio::test]
    async fn always_allow_normalizes_env_prefixed_command() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let approval_registry = Arc::new(ApprovalRegistry::new());

        let tool = ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            10,
            gate.clone(),
            ExecSecurityConfig {
                security: ExecSecurityMode::Allowlist,
                ask: ExecAskMode::OnMiss,
                allowlist: vec![],
                safe_bins: vec![],
            },
            SandboxPolicyConfig::default(),
            Some(approval_registry.clone()),
            None,
            "agent-test".to_string(),
            None,
        );
        let ctx = ToolContext::builtin();

        let first = tokio::spawn(async move {
            tool.execute(
                serde_json::json!({"command": "FOO=bar printf normalized"}),
                &ctx,
            )
            .await
            .unwrap()
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let pending = approval_registry.pending_list().await;
        let (trace_id, _, _) = pending.first().unwrap();
        approval_registry
            .resolve(*trace_id, ApprovalDecision::AlwaysAllow)
            .await
            .unwrap();
        let first_output = first.await.unwrap();
        assert!(!first_output.is_error);

        let tool_again = ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            10,
            gate,
            ExecSecurityConfig {
                security: ExecSecurityMode::Allowlist,
                ask: ExecAskMode::OnMiss,
                allowlist: vec![],
                safe_bins: vec![],
            },
            SandboxPolicyConfig::default(),
            Some(approval_registry.clone()),
            None,
            "agent-test".to_string(),
            None,
        );
        let ctx2 = ToolContext::builtin();
        let second = tokio::spawn(async move {
            tool_again
                .execute(serde_json::json!({"command": "printf normalized"}), &ctx2)
                .await
                .unwrap()
        });

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !approval_registry.has_pending().await,
            "normalized command should not require approval"
        );

        let second_output = second.await.unwrap();
        assert!(!second_output.is_error);
        assert!(second_output.content.contains("normalized"));
    }

    #[tokio::test]
    async fn ask_always_skips_repeat_prompt_when_runtime_allowed() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let approval_registry = Arc::new(ApprovalRegistry::new());
        approval_registry
            .add_runtime_allow_pattern("agent-test", "printf *".to_string())
            .await;

        let tool = ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            10,
            gate,
            ExecSecurityConfig {
                security: ExecSecurityMode::Allowlist,
                ask: ExecAskMode::Always,
                allowlist: vec![],
                safe_bins: vec![],
            },
            SandboxPolicyConfig::default(),
            Some(approval_registry.clone()),
            None,
            "agent-test".to_string(),
            None,
        );
        let ctx = ToolContext::builtin();

        let output = tool
            .execute(serde_json::json!({"command": "printf no-repeat"}), &ctx)
            .await
            .unwrap();

        assert!(!output.is_error);
        assert!(output.content.contains("no-repeat"));
        assert!(
            !approval_registry.has_pending().await,
            "runtime-allowed command should bypass ask=Always"
        );
    }

    #[tokio::test]
    async fn allowlist_onmiss_without_registry_denies() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let tool = ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            10,
            gate,
            ExecSecurityConfig {
                security: ExecSecurityMode::Allowlist,
                ask: ExecAskMode::OnMiss,
                allowlist: vec![],
                safe_bins: vec![],
            },
            SandboxPolicyConfig::default(),
            None,
            None,
            "agent-test".to_string(),
            None,
        );
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(serde_json::json!({"command": "printf denied"}), &ctx)
            .await
            .unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("no approval UI available"));
    }

    #[tokio::test]
    async fn hard_baseline_blocks_localhost_in_network_ask_mode() {
        let tmp = TempDir::new().unwrap();
        let gate = make_gate(tmp.path());
        let sandbox = SandboxPolicyConfig {
            network: SandboxNetworkMode::Ask,
            ..Default::default()
        };
        let tool = ExecuteCommandTool::new(
            tmp.path().to_path_buf(),
            10,
            gate,
            ExecSecurityConfig {
                security: ExecSecurityMode::Full,
                ask: ExecAskMode::Off,
                allowlist: vec![],
                safe_bins: vec![],
            },
            sandbox,
            None,
            None,
            "agent-test".to_string(),
            None,
        );
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(
                serde_json::json!({"command": "curl -sS http://localhost:8001/health"}),
                &ctx,
            )
            .await
            .unwrap();

        assert!(
            result.is_error,
            "localhost should be blocked by hard baseline"
        );
        assert!(
            result.content.contains("hard baseline") || result.content.contains("denied"),
            "error should mention hard baseline, got: {}",
            result.content
        );
    }
}
