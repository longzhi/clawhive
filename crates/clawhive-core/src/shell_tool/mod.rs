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
mod tests;
