use std::fs;
use std::path::Path;

use serde::Deserialize;

#[derive(Debug, Clone)]
pub enum AuthSummary {
    ApiKey,
    OAuth { profile_name: String },
}

#[derive(Debug, Clone)]
pub struct ProviderInfo {
    pub provider_id: String,
    pub auth_summary: AuthSummary,
}

#[derive(Debug, Clone)]
pub struct AgentInfo {
    pub agent_id: String,
    pub name: String,
    pub emoji: String,
    pub primary_model: String,
}

#[derive(Debug, Clone)]
pub struct ChannelInfo {
    pub channel_type: String,
    pub connector_id: String,
}

#[derive(Debug, Clone)]
pub struct ConfigState {
    pub providers: Vec<ProviderInfo>,
    pub agents: Vec<AgentInfo>,
    pub channels: Vec<ChannelInfo>,
    pub default_agent: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawProviderYaml {
    provider_id: String,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    auth_profile: Option<String>,
}

pub fn scan_config(root: &Path) -> ConfigState {
    let config_dir = root.join("config");
    let providers = scan_providers(&config_dir);
    let agents = scan_agents(&config_dir);
    let (channels, default_agent) = scan_main_and_routing(&config_dir);

    ConfigState {
        providers,
        agents,
        channels,
        default_agent,
    }
}

fn scan_providers(config_dir: &Path) -> Vec<ProviderInfo> {
    let providers_dir = config_dir.join("providers.d");
    let mut providers = Vec::new();

    if let Ok(entries) = fs::read_dir(providers_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
                continue;
            }

            let Ok(raw) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(provider) = serde_yaml::from_str::<RawProviderYaml>(&raw) else {
                continue;
            };

            let auth_summary = match provider.auth_profile {
                Some(profile_name) if !profile_name.trim().is_empty() => {
                    AuthSummary::OAuth { profile_name }
                }
                _ => AuthSummary::ApiKey,
            };

            let _ = provider.api_key;

            providers.push(ProviderInfo {
                provider_id: provider.provider_id,
                auth_summary,
            });
        }
    }

    providers.sort_by(|a, b| a.provider_id.cmp(&b.provider_id));
    providers
}

fn scan_agents(config_dir: &Path) -> Vec<AgentInfo> {
    let agents_dir = config_dir.join("agents.d");
    let mut agents = Vec::new();

    if let Ok(entries) = fs::read_dir(agents_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
                continue;
            }

            let Ok(raw) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(agent) = serde_yaml::from_str::<nanocrab_core::FullAgentConfig>(&raw) else {
                continue;
            };

            let (name, emoji) = match agent.identity {
                Some(identity) => (identity.name, identity.emoji.unwrap_or_default()),
                None => (agent.agent_id.clone(), String::new()),
            };

            agents.push(AgentInfo {
                agent_id: agent.agent_id,
                name,
                emoji,
                primary_model: agent.model_policy.primary,
            });
        }
    }

    agents.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
    agents
}

fn scan_main_and_routing(config_dir: &Path) -> (Vec<ChannelInfo>, Option<String>) {
    let mut channels = Vec::new();

    let main_path = config_dir.join("main.yaml");
    if let Ok(raw) = fs::read_to_string(main_path) {
        if let Ok(main) = serde_yaml::from_str::<nanocrab_core::MainConfig>(&raw) {
            if let Some(telegram) = main.channels.telegram {
                for connector in telegram.connectors {
                    channels.push(ChannelInfo {
                        channel_type: "telegram".to_string(),
                        connector_id: connector.connector_id,
                    });
                }
            }

            if let Some(discord) = main.channels.discord {
                for connector in discord.connectors {
                    channels.push(ChannelInfo {
                        channel_type: "discord".to_string(),
                        connector_id: connector.connector_id,
                    });
                }
            }
        }
    }

    channels.sort_by(|a, b| {
        a.channel_type
            .cmp(&b.channel_type)
            .then(a.connector_id.cmp(&b.connector_id))
    });

    let routing_path = config_dir.join("routing.yaml");
    let default_agent = fs::read_to_string(routing_path)
        .ok()
        .and_then(|raw| serde_yaml::from_str::<nanocrab_core::RoutingConfig>(&raw).ok())
        .map(|routing| routing.default_agent_id);

    (channels, default_agent)
}

#[cfg(test)]
mod tests {
    use super::{scan_config, AuthSummary};

    fn write(path: &std::path::Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }
        std::fs::write(path, content).expect("write file");
    }

    fn base_main_yaml(channels_yaml: &str) -> String {
        format!(
            "app:\n  name: nanocrab\n  env: dev\nruntime:\n  max_concurrent: 4\nfeatures:\n  multi_agent: true\n  sub_agent: true\n  tui: true\n  cli: true\nchannels:\n{channels_yaml}\nembedding:\n  enabled: false\n  provider: stub\n  api_key: \"\"\n  model: text-embedding-3-small\n  dimensions: 1536\n  base_url: https://api.openai.com/v1\ntools: {{}}\n"
        )
    }

    #[test]
    fn scan_empty_dir_returns_no_items() {
        let temp = tempfile::tempdir().expect("create tempdir");

        let state = scan_config(temp.path());

        assert!(state.providers.is_empty());
        assert!(state.agents.is_empty());
        assert!(state.channels.is_empty());
        assert!(state.default_agent.is_none());
    }

    #[test]
    fn scan_detects_provider_with_api_key() {
        let temp = tempfile::tempdir().expect("create tempdir");
        write(
            &temp.path().join("config/providers.d/openai.yaml"),
            "provider_id: openai\nenabled: true\napi_base: https://api.openai.com/v1\napi_key: \"sk-test\"\nmodels:\n  - gpt-4o-mini\n",
        );

        let state = scan_config(temp.path());

        assert_eq!(state.providers.len(), 1);
        assert_eq!(state.providers[0].provider_id, "openai");
        match &state.providers[0].auth_summary {
            AuthSummary::ApiKey => {}
            AuthSummary::OAuth { .. } => panic!("expected api key auth"),
        }
    }

    #[test]
    fn scan_detects_provider_with_oauth() {
        let temp = tempfile::tempdir().expect("create tempdir");
        write(
            &temp.path().join("config/providers.d/openai.yaml"),
            "provider_id: openai\nenabled: true\napi_base: https://api.openai.com/v1\napi_key: \"sk-test\"\nauth_profile: openai-123\nmodels:\n  - gpt-4o-mini\n",
        );

        let state = scan_config(temp.path());

        assert_eq!(state.providers.len(), 1);
        assert_eq!(state.providers[0].provider_id, "openai");
        match &state.providers[0].auth_summary {
            AuthSummary::OAuth { profile_name } => assert_eq!(profile_name, "openai-123"),
            AuthSummary::ApiKey => panic!("expected oauth auth"),
        }
    }

    #[test]
    fn scan_detects_agent() {
        let temp = tempfile::tempdir().expect("create tempdir");
        write(
            &temp.path().join("config/agents.d/nanocrab-main.yaml"),
            "agent_id: nanocrab-main\nenabled: true\nidentity:\n  name: Nanocrab\n  emoji: \"🦀\"\nmodel_policy:\n  primary: openai/gpt-4o-mini\n  fallbacks: []\n",
        );

        let state = scan_config(temp.path());

        assert_eq!(state.agents.len(), 1);
        let agent = &state.agents[0];
        assert_eq!(agent.agent_id, "nanocrab-main");
        assert_eq!(agent.name, "Nanocrab");
        assert_eq!(agent.emoji, "🦀");
        assert_eq!(agent.primary_model, "openai/gpt-4o-mini");
    }

    #[test]
    fn scan_detects_channels_from_main_yaml() {
        let temp = tempfile::tempdir().expect("create tempdir");
        let main_yaml = base_main_yaml(
            "  telegram:\n    enabled: true\n    connectors:\n      - connector_id: tg-main\n        token: \"token-a\"\n  discord:\n    enabled: true\n    connectors:\n      - connector_id: dc-main\n        token: \"token-b\"\n",
        );
        write(&temp.path().join("config/main.yaml"), &main_yaml);

        let state = scan_config(temp.path());

        assert_eq!(state.channels.len(), 2);
        assert_eq!(state.channels[0].channel_type, "discord");
        assert_eq!(state.channels[0].connector_id, "dc-main");
        assert_eq!(state.channels[1].channel_type, "telegram");
        assert_eq!(state.channels[1].connector_id, "tg-main");
    }

    #[test]
    fn scan_reads_default_agent_from_routing() {
        let temp = tempfile::tempdir().expect("create tempdir");
        write(
            &temp.path().join("config/routing.yaml"),
            "default_agent_id: nanocrab-main\nbindings: []\n",
        );

        let state = scan_config(temp.path());

        assert_eq!(state.default_agent.as_deref(), Some("nanocrab-main"));
    }
}
