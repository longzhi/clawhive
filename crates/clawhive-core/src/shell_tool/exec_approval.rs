use anyhow::Result;
use clawhive_schema::{approval_program, ApprovalDecision, BusMessage};

use super::ExecuteCommandTool;

impl ExecuteCommandTool {
    pub(super) async fn wait_for_approval(
        &self,
        command: &str,
        source_info: Option<(&str, &str, &str)>,
    ) -> Result<Option<String>> {
        let Some(registry) = self.approval_registry.as_ref() else {
            return Ok(Some(
                "Command not in allowlist and no approval UI available".to_string(),
            ));
        };

        let trace_id = uuid::Uuid::new_v4();
        let has_bus = self.bus.is_some();
        let has_source = source_info.is_some();
        tracing::info!(
            command, %trace_id, has_bus, has_source,
            "requesting exec approval"
        );

        let rx = registry
            .request(trace_id, command.to_string(), self.agent_id.clone())
            .await;

        if let (Some(bus), Some((ch_type, conn_id, conv_scope))) = (self.bus.as_ref(), source_info)
        {
            let summary = match &self.approval_summarizer {
                Some(summarizer) => summarizer.summarize(command, None).await,
                None => None,
            };

            let _ = bus
                .publish(BusMessage::NeedHumanApproval {
                    trace_id,
                    reason: format!("Command requires approval: {command}"),
                    agent_id: self.agent_id.clone(),
                    command: command.to_string(),
                    network_target: None,
                    summary,
                    source_channel_type: Some(ch_type.to_string()),
                    source_connector_id: Some(conn_id.to_string()),
                    source_conversation_scope: Some(conv_scope.to_string()),
                })
                .await;
        }

        match tokio::time::timeout(std::time::Duration::from_secs(600), rx).await {
            Ok(Ok(ApprovalDecision::AllowOnce)) => Ok(None),
            Ok(Ok(ApprovalDecision::AlwaysAllow)) => {
                let pattern = format!("{} *", approval_program(command));
                registry
                    .add_runtime_allow_pattern(&self.agent_id, pattern.clone())
                    .await;
                tracing::info!(pattern, "adding to exec allowlist");
                Ok(None)
            }
            Ok(Ok(ApprovalDecision::Deny)) | Ok(Err(_)) => {
                Ok(Some("Command denied by user".to_string()))
            }
            Err(_) => {
                tracing::warn!(command, "approval request timed out after 10 minutes");
                Ok(Some("Approval timed out".to_string()))
            }
        }
    }

    pub(super) async fn wait_for_network_approval(
        &self,
        command: &str,
        host: &str,
        port: u16,
        source_info: Option<(&str, &str, &str)>,
    ) -> Result<Option<String>> {
        let Some(registry) = self.approval_registry.as_ref() else {
            return Ok(Some(
                "Network access requires approval but no approval UI available".to_string(),
            ));
        };

        let target = format!("{host}:{port}");
        let trace_id = uuid::Uuid::new_v4();
        tracing::info!(command, %trace_id, target, "requesting network approval");

        let rx = registry
            .request(trace_id, command.to_string(), self.agent_id.clone())
            .await;

        if let (Some(bus), Some((ch_type, conn_id, conv_scope))) = (self.bus.as_ref(), source_info)
        {
            let summary = match &self.approval_summarizer {
                Some(summarizer) => summarizer.summarize(command, Some(&target)).await,
                None => None,
            };

            let _ = bus
                .publish(BusMessage::NeedHumanApproval {
                    trace_id,
                    reason: format!("Network access: {target}"),
                    agent_id: self.agent_id.clone(),
                    command: command.to_string(),
                    network_target: Some(target.clone()),
                    summary,
                    source_channel_type: Some(ch_type.to_string()),
                    source_connector_id: Some(conn_id.to_string()),
                    source_conversation_scope: Some(conv_scope.to_string()),
                })
                .await;
        }

        match tokio::time::timeout(std::time::Duration::from_secs(600), rx).await {
            Ok(Ok(ApprovalDecision::AllowOnce)) => Ok(None),
            Ok(Ok(ApprovalDecision::AlwaysAllow)) => {
                registry
                    .add_network_allow_pattern(&self.agent_id, target)
                    .await;
                tracing::info!(host, port, "adding to network allowlist");
                Ok(None)
            }
            Ok(Ok(ApprovalDecision::Deny)) | Ok(Err(_)) => Ok(Some(format!(
                "Network access to {host}:{port} denied by user"
            ))),
            Err(_) => {
                tracing::warn!(
                    host,
                    port,
                    "network approval request timed out after 10 minutes"
                );
                Ok(Some(format!(
                    "Network access to {host}:{port} approval timed out"
                )))
            }
        }
    }

    pub(super) fn is_command_allowed(&self, command: &str) -> bool {
        let cmd_lower = command.to_lowercase();
        let first_token = command.split_whitespace().next().unwrap_or("");
        let basename = std::path::Path::new(first_token)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(first_token);

        if self.exec_security.safe_bins.iter().any(|b| b == basename) {
            return true;
        }

        self.exec_security.allowlist.iter().any(|pattern| {
            if pattern.ends_with(" *") {
                let prefix = &pattern[..pattern.len() - 2];
                basename == prefix || first_token == prefix
            } else {
                cmd_lower == pattern.to_lowercase() || basename == pattern
            }
        })
    }
}
