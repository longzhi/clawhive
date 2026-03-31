use std::collections::HashMap;

use crate::config::ClawhiveConfig;

pub struct ConfigDiff {
    pub agents_added: Vec<String>,
    pub agents_removed: Vec<String>,
    pub agents_changed: Vec<String>,
    pub routing_changed: bool,
    pub providers_changed: bool,
    pub embedding_changed: bool,
    pub channels_changed: bool,
    pub requires_restart: Vec<String>,
}

impl ConfigDiff {
    pub fn between(old: &ClawhiveConfig, new: &ClawhiveConfig) -> Self {
        let mut diff = Self {
            agents_added: Vec::new(),
            agents_removed: Vec::new(),
            agents_changed: Vec::new(),
            routing_changed: false,
            providers_changed: false,
            embedding_changed: false,
            channels_changed: false,
            requires_restart: Vec::new(),
        };

        let old_agents: HashMap<&str, _> = old
            .agents
            .iter()
            .map(|a| (a.agent_id.as_str(), a))
            .collect();
        let new_agents: HashMap<&str, _> = new
            .agents
            .iter()
            .map(|a| (a.agent_id.as_str(), a))
            .collect();

        for id in new_agents.keys() {
            if !old_agents.contains_key(id) {
                diff.agents_added.push((*id).to_string());
            }
        }
        for id in old_agents.keys() {
            if !new_agents.contains_key(id) {
                diff.agents_removed.push((*id).to_string());
            }
        }
        for (id, new_agent) in &new_agents {
            if let Some(old_agent) = old_agents.get(id) {
                if serde_json::to_string(old_agent).ok() != serde_json::to_string(new_agent).ok() {
                    diff.agents_changed.push((*id).to_string());
                }
            }
        }

        diff.agents_added.sort();
        diff.agents_removed.sort();
        diff.agents_changed.sort();
        diff.routing_changed =
            serde_json::to_string(&old.routing).ok() != serde_json::to_string(&new.routing).ok();
        diff.providers_changed = serde_json::to_string(&old.providers).ok()
            != serde_json::to_string(&new.providers).ok();
        diff.embedding_changed = serde_json::to_string(&old.main.embedding).ok()
            != serde_json::to_string(&new.main.embedding).ok();
        diff.channels_changed = serde_json::to_string(&old.main.channels).ok()
            != serde_json::to_string(&new.main.channels).ok();

        if old.main.log_level != new.main.log_level {
            diff.requires_restart.push("log_level".to_string());
        }

        diff
    }

    pub fn is_empty(&self) -> bool {
        self.agents_added.is_empty()
            && self.agents_removed.is_empty()
            && self.agents_changed.is_empty()
            && !self.routing_changed
            && !self.providers_changed
            && !self.embedding_changed
            && !self.channels_changed
            && self.requires_restart.is_empty()
    }

    pub fn has_config_changes(&self) -> bool {
        !self.agents_added.is_empty()
            || !self.agents_removed.is_empty()
            || !self.agents_changed.is_empty()
            || self.routing_changed
            || self.providers_changed
            || self.embedding_changed
            || self.channels_changed
    }
}

#[cfg(test)]
mod tests {
    use crate::config::{
        ClawhiveConfig, FullAgentConfig, MainConfig, ProviderConfig, RoutingConfig,
    };
    use crate::{ConfigDiff, ModelPolicy, SecurityMode};

    fn base_config() -> ClawhiveConfig {
        ClawhiveConfig {
            main: MainConfig::default(),
            routing: RoutingConfig {
                default_agent_id: "agent-a".to_string(),
                bindings: Vec::new(),
            },
            providers: vec![ProviderConfig {
                provider_id: "openai".to_string(),
                enabled: true,
                api_base: "https://api.openai.com/v1".to_string(),
                api_key: Some("sk-test".to_string()),
                auth_profile: None,
                provider_type: None,
                models: vec!["gpt-4o".to_string()],
            }],
            agents: vec![FullAgentConfig {
                agent_id: "agent-a".to_string(),
                enabled: true,
                security: SecurityMode::default(),
                workspace: None,
                identity: None,
                model_policy: ModelPolicy {
                    primary: "gpt-4o".to_string(),
                    fallbacks: Vec::new(),
                    thinking_level: None,
                    context_window: None,
                },
                tool_policy: None,
                memory_policy: None,
                sub_agent: None,
                heartbeat: None,
                exec_security: None,
                sandbox: None,
                max_response_tokens: None,
                max_iterations: None,
            }],
        }
    }

    #[test]
    fn between_reports_no_changes_for_identical_configs() {
        let old = base_config();
        let new = old.clone();

        let diff = ConfigDiff::between(&old, &new);

        assert!(diff.is_empty());
        assert!(!diff.has_config_changes());
    }

    #[test]
    fn between_detects_added_removed_and_changed_agents() {
        let mut old = base_config();
        old.agents.push(FullAgentConfig {
            agent_id: "agent-removed".to_string(),
            ..old.agents[0].clone()
        });

        let mut new = base_config();
        new.agents[0].model_policy.primary = "gpt-4.1".to_string();
        new.agents.push(FullAgentConfig {
            agent_id: "agent-added".to_string(),
            ..new.agents[0].clone()
        });

        let diff = ConfigDiff::between(&old, &new);

        assert_eq!(diff.agents_added, vec!["agent-added".to_string()]);
        assert_eq!(diff.agents_removed, vec!["agent-removed".to_string()]);
        assert_eq!(diff.agents_changed, vec!["agent-a".to_string()]);
        assert!(diff.has_config_changes());
    }

    #[test]
    fn between_detects_routing_provider_and_embedding_changes() {
        let old = base_config();
        let mut new = base_config();
        new.routing.default_agent_id = "agent-b".to_string();
        new.providers[0].api_base = "https://example.com/v1".to_string();
        new.main.embedding.provider = "gemini".to_string();

        let diff = ConfigDiff::between(&old, &new);

        assert!(diff.routing_changed);
        assert!(diff.providers_changed);
        assert!(diff.embedding_changed);
    }

    #[test]
    fn between_tracks_restart_required_changes() {
        let old = base_config();
        let mut new = base_config();
        new.main.log_level = "debug".to_string();

        let diff = ConfigDiff::between(&old, &new);

        assert_eq!(diff.requires_restart, vec!["log_level".to_string()]);
        assert!(!diff.has_config_changes());
        assert!(!diff.is_empty());
    }
}
