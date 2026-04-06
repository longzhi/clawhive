use std::sync::Arc;

use anyhow::Result;
use clawhive_schema::*;

use crate::access_gate::AccessGate;
use crate::config_view::ConfigView;
use crate::skill::SkillRegistry;

use super::Orchestrator;

pub(super) const SKILL_INSTALL_USAGE_HINT: &str =
    "请提供 skill 来源路径或 URL。用法: /skill install <source>";

impl Orchestrator {
    pub(super) async fn handle_skill_analyze_or_install_command(
        &self,
        inbound: InboundMessage,
        source: String,
        install_requested: bool,
    ) -> Result<OutboundMessage> {
        let resolved = crate::skill_install::resolve_skill_source(&source).await?;
        let report = crate::skill_install::analyze_skill_source(resolved.local_path())?;
        let token = self
            .skill_install_state
            .create_pending(
                source,
                report.clone(),
                inbound.user_scope.clone(),
                inbound.conversation_scope.clone(),
            )
            .await;

        let mode_text = if install_requested {
            "Install request analyzed."
        } else {
            "Analyze complete."
        };
        let analysis = crate::skill_install::render_skill_analysis(&report);
        let text = format!("{mode_text}\n\n{analysis}\n\nTo continue, run: /skill confirm {token}");

        // Publish bus message so Discord/Telegram can render confirm buttons
        let _ = self
            .bus
            .publish(clawhive_schema::BusMessage::DeliverSkillConfirm {
                channel_type: inbound.channel_type.clone(),
                connector_id: inbound.connector_id.clone(),
                conversation_scope: inbound.conversation_scope.clone(),
                token: token.clone(),
                skill_name: report.skill_name.clone(),
                analysis_text: analysis,
            })
            .await;

        Ok(OutboundMessage {
            trace_id: inbound.trace_id,
            channel_type: inbound.channel_type,
            connector_id: inbound.connector_id,
            conversation_scope: inbound.conversation_scope,
            text,
            at: chrono::Utc::now(),
            reply_to: None,
            attachments: vec![],
        })
    }
    pub(super) async fn handle_skill_confirm_command(
        &self,
        inbound: InboundMessage,
        agent_id: &str,
        token: String,
    ) -> Result<OutboundMessage> {
        if !self
            .skill_install_state
            .is_scope_allowed(&inbound.user_scope)
        {
            return Ok(OutboundMessage {
                trace_id: inbound.trace_id,
                channel_type: inbound.channel_type,
                connector_id: inbound.connector_id,
                conversation_scope: inbound.conversation_scope,
                text: "You are not authorized to install skills in this environment.".to_string(),
                at: chrono::Utc::now(),
                reply_to: None,
                attachments: vec![],
            });
        }

        let Some(pending) = self.skill_install_state.take_if_valid(&token).await else {
            return Ok(OutboundMessage {
                trace_id: inbound.trace_id,
                channel_type: inbound.channel_type,
                connector_id: inbound.connector_id,
                conversation_scope: inbound.conversation_scope,
                text: "Invalid or expired skill install confirmation token.".to_string(),
                at: chrono::Utc::now(),
                reply_to: None,
                attachments: vec![],
            });
        };

        if pending.user_scope != inbound.user_scope
            || pending.conversation_scope != inbound.conversation_scope
        {
            return Ok(OutboundMessage {
                trace_id: inbound.trace_id,
                channel_type: inbound.channel_type,
                connector_id: inbound.connector_id,
                conversation_scope: inbound.conversation_scope,
                text: "This token belongs to a different user or conversation.".to_string(),
                at: chrono::Utc::now(),
                reply_to: None,
                attachments: vec![],
            });
        }

        let crate::skill_install_state::PendingSkillInstall {
            source,
            report,
            user_scope: _,
            conversation_scope: _,
            created_at: _,
        } = pending;

        if crate::skill_install::has_high_risk_findings(&report) {
            let Some(registry) = self.approval_registry.as_ref() else {
                return Ok(OutboundMessage {
                    trace_id: inbound.trace_id,
                    channel_type: inbound.channel_type,
                    connector_id: inbound.connector_id,
                    conversation_scope: inbound.conversation_scope,
                    text:
                        "High-risk skill install requires approval but no approval UI is available."
                            .to_string(),
                    at: chrono::Utc::now(),
                    reply_to: None,
                    attachments: vec![],
                });
            };

            let command = format!("skill install {}", report.skill_name);
            let trace_id = uuid::Uuid::new_v4();
            let rx = registry
                .request(trace_id, command.clone(), agent_id.to_string())
                .await;

            let _ = self
                .bus
                .publish(BusMessage::NeedHumanApproval {
                    trace_id,
                    reason: format!(
                        "High-risk skill install requires approval: {}",
                        report.skill_name
                    ),
                    agent_id: agent_id.to_string(),
                    command,
                    network_target: None,
                    summary: None,
                    source_channel_type: Some(inbound.channel_type.clone()),
                    source_connector_id: Some(inbound.connector_id.clone()),
                    source_conversation_scope: Some(inbound.conversation_scope.clone()),
                })
                .await;

            match rx.await {
                Ok(ApprovalDecision::AllowOnce) | Ok(ApprovalDecision::AlwaysAllow) => {}
                Ok(ApprovalDecision::Deny) | Err(_) => {
                    return Ok(OutboundMessage {
                        trace_id: inbound.trace_id,
                        channel_type: inbound.channel_type,
                        connector_id: inbound.connector_id,
                        conversation_scope: inbound.conversation_scope,
                        text: "Skill install denied by user.".to_string(),
                        at: chrono::Utc::now(),
                        reply_to: None,
                        attachments: vec![],
                    });
                }
            }
        }

        let resolved = crate::skill_install::resolve_skill_source(&source).await?;
        let installed = crate::skill_install::install_skill_from_analysis(
            self.workspaces.default_root(),
            &self.skills_root,
            resolved.local_path(),
            &report,
            true,
        )?;
        self.reload_skills();

        let mut text = format!(
            "Installed skill '{}' to {} (findings: {}, high-risk: {}).",
            report.skill_name,
            installed.target.display(),
            report.findings.len(),
            installed.high_risk
        );

        let env_vars = report.all_required_env_vars();
        let missing = crate::dotenv::missing_env_vars(&env_vars);
        if !missing.is_empty() {
            text.push_str(&format!(
                "\n\n⚠️ Missing environment variables: {}\nAdd them to ~/.clawhive/.env (KEY=value format) for this skill to work.",
                missing.join(", ")
            ));
        }

        Ok(OutboundMessage {
            trace_id: inbound.trace_id,
            channel_type: inbound.channel_type,
            connector_id: inbound.connector_id,
            conversation_scope: inbound.conversation_scope,
            text,
            at: chrono::Utc::now(),
            reply_to: None,
            attachments: vec![],
        })
    }

    pub(super) fn workspace_root_for(&self, agent_id: &str) -> std::path::PathBuf {
        self.workspaces.workspace_root(agent_id)
    }

    pub(super) fn access_gate_for(&self, agent_id: &str) -> Arc<AccessGate> {
        self.workspaces.access_gate(agent_id)
    }

    pub(super) fn active_skill_registry(&self) -> Arc<SkillRegistry> {
        self.skill_registry.load_full()
    }

    pub fn config_view(&self) -> Arc<ConfigView> {
        self.config_view.load_full()
    }

    pub fn apply_config_view(&self, view: ConfigView) {
        self.config_view.store(Arc::new(view));
    }

    pub fn reload_skills(&self) {
        match SkillRegistry::load_from_dir(&self.skills_root) {
            Ok(registry) => {
                self.skill_registry.store(Arc::new(registry));
                tracing::info!(
                    skills_root = %self.skills_root.display(),
                    "skill registry reloaded"
                );
            }
            Err(e) => {
                tracing::warn!(
                    skills_root = %self.skills_root.display(),
                    error = %e,
                    "failed to reload skill registry, keeping cached version"
                );
            }
        }
    }

    pub(super) fn forced_skill_names(input: &str) -> Option<Vec<String>> {
        let trimmed = input.trim();
        let rest = trimmed.strip_prefix("/skill ")?;
        let names_part = rest.split_whitespace().next()?.trim();
        if names_part.is_empty() {
            return None;
        }

        let names: Vec<String> = names_part
            .split(',')
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();

        if names.is_empty() {
            None
        } else {
            Some(names)
        }
    }

    pub(super) fn merge_permissions(
        perms: impl IntoIterator<Item = corral_core::Permissions>,
    ) -> Option<corral_core::Permissions> {
        let mut list: Vec<corral_core::Permissions> = perms.into_iter().collect();
        if list.is_empty() {
            return None;
        }

        let mut merged = corral_core::Permissions::default();
        for p in list.drain(..) {
            merged.fs.read.extend(p.fs.read);
            merged.fs.write.extend(p.fs.write);
            merged.network.allow.extend(p.network.allow);
            merged.exec.extend(p.exec);
            merged.env.extend(p.env);
            merged.services.extend(p.services);
        }

        merged.fs.read.sort();
        merged.fs.read.dedup();
        merged.fs.write.sort();
        merged.fs.write.dedup();
        merged.network.allow.sort();
        merged.network.allow.dedup();
        merged.exec.sort();
        merged.exec.dedup();
        merged.env.sort();
        merged.env.dedup();

        Some(merged)
    }

    #[cfg(test)]
    pub(super) fn compute_merged_permissions(
        active_skills: &SkillRegistry,
        forced_skills: Option<&[String]>,
    ) -> Option<corral_core::Permissions> {
        if let Some(forced_names) = forced_skills {
            let selected_perms = forced_names
                .iter()
                .filter_map(|forced| {
                    active_skills
                        .get(forced)
                        .and_then(|skill| skill.permissions.as_ref())
                        .map(|p| p.to_corral_permissions())
                })
                .collect::<Vec<_>>();
            Self::merge_permissions(selected_perms)
        } else {
            active_skills.merged_permissions()
        }
    }

    pub(super) fn forced_allowed_tools(
        forced_skills: Option<&[String]>,
        agent_allowed: Option<Vec<String>>,
    ) -> Option<Vec<String>> {
        // In forced skill mode, require shell execution so skill permissions
        // are enforced by sandbox preflight/policy.
        let forced_base = if forced_skills.is_some() {
            Some(vec!["execute_command".to_string()])
        } else {
            None
        };

        match (forced_base, agent_allowed) {
            (Some(base), Some(agent)) => {
                let filtered: Vec<String> = base
                    .into_iter()
                    .filter(|t| agent.iter().any(|a| a == t))
                    .collect();
                Some(filtered)
            }
            (Some(base), None) => Some(base),
            (None, Some(agent)) => Some(agent),
            (None, None) => None,
        }
    }
}
