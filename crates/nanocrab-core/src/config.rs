use std::{collections::HashSet, fs, path::Path};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};

use super::ModelPolicy;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub name: String,
    pub env: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeConfig {
    pub max_concurrent: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeaturesConfig {
    pub multi_agent: bool,
    pub sub_agent: bool,
    pub tui: bool,
    pub cli: bool,
}

fn default_embedding_model() -> String {
    "text-embedding-3-small".to_string()
}

fn default_embedding_dimensions() -> usize {
    1536
}

fn default_embedding_base_url() -> String {
    "https://api.openai.com/v1".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingConfig {
    pub enabled: bool,
    pub provider: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default = "default_embedding_model")]
    pub model: String,
    #[serde(default = "default_embedding_dimensions")]
    pub dimensions: usize,
    #[serde(default = "default_embedding_base_url")]
    pub base_url: String,
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: "stub".to_string(),
            api_key: String::new(),
            model: default_embedding_model(),
            dimensions: default_embedding_dimensions(),
            base_url: default_embedding_base_url(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramConnectorConfig {
    pub connector_id: String,
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramChannelConfig {
    pub enabled: bool,
    #[serde(default)]
    pub connectors: Vec<TelegramConnectorConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordConnectorConfig {
    pub connector_id: String,
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordChannelConfig {
    pub enabled: bool,
    #[serde(default)]
    pub connectors: Vec<DiscordConnectorConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelsConfig {
    pub telegram: Option<TelegramChannelConfig>,
    pub discord: Option<DiscordChannelConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WebSearchConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolsConfig {
    #[serde(default)]
    pub web_search: Option<WebSearchConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MainConfig {
    pub app: AppConfig,
    pub runtime: RuntimeConfig,
    pub features: FeaturesConfig,
    pub channels: ChannelsConfig,
    #[serde(default)]
    pub embedding: EmbeddingConfig,
    #[serde(default)]
    pub tools: ToolsConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchRule {
    pub kind: String,
    pub pattern: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingBinding {
    pub channel_type: String,
    pub connector_id: String,
    #[serde(rename = "match")]
    pub match_rule: MatchRule,
    pub agent_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutingConfig {
    pub default_agent_id: String,
    #[serde(default)]
    pub bindings: Vec<RoutingBinding>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub provider_id: String,
    pub enabled: bool,
    pub api_base: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub models: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IdentityConfig {
    pub name: String,
    pub emoji: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolPolicyConfig {
    #[serde(default)]
    pub allow: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryPolicyConfig {
    pub mode: String,
    pub write_scope: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentPolicyConfig {
    pub allow_spawn: bool,
}

fn default_heartbeat_interval_minutes() -> u64 {
    30
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HeartbeatPolicyConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_heartbeat_interval_minutes")]
    pub interval_minutes: u64,
    #[serde(default)]
    pub prompt: Option<String>,
}

impl Default for HeartbeatPolicyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_minutes: 30,
            prompt: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FullAgentConfig {
    pub agent_id: String,
    pub enabled: bool,
    #[serde(default)]
    pub workspace: Option<String>,
    pub identity: Option<IdentityConfig>,
    pub model_policy: ModelPolicy,
    pub tool_policy: Option<ToolPolicyConfig>,
    pub memory_policy: Option<MemoryPolicyConfig>,
    pub sub_agent: Option<SubAgentPolicyConfig>,
    #[serde(default)]
    pub heartbeat: Option<HeartbeatPolicyConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NanocrabConfig {
    pub main: MainConfig,
    pub routing: RoutingConfig,
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
    #[serde(default)]
    pub agents: Vec<FullAgentConfig>,
}

pub fn resolve_env_var(raw: &str) -> String {
    let mut output = String::new();
    let mut rest = raw;

    while let Some(start) = rest.find("${") {
        output.push_str(&rest[..start]);

        let candidate = &rest[start + 2..];
        let Some(end) = candidate.find('}') else {
            output.push_str(&rest[start..]);
            return output;
        };

        let key = &candidate[..end];
        output.push_str(&std::env::var(key).unwrap_or_default());
        rest = &candidate[end + 1..];
    }

    output.push_str(rest);
    output
}

pub fn load_config(root: &Path) -> Result<NanocrabConfig> {
    let mut main: MainConfig = read_yaml_file(&root.join("main.yaml"))?;
    let mut routing: RoutingConfig = read_yaml_file(&root.join("routing.yaml"))?;

    let mut providers = read_yaml_dir::<ProviderConfig>(&root.join("providers.d"))?;
    let mut agents = read_yaml_dir::<FullAgentConfig>(&root.join("agents.d"))?;

    resolve_main_env(&mut main);
    resolve_routing_env(&mut routing);
    resolve_providers_env(&mut providers);
    resolve_agents_env(&mut agents);

    let config = NanocrabConfig {
        main,
        routing,
        providers,
        agents,
    };

    validate_config(&config)?;
    Ok(config)
}

pub fn validate_config(config: &NanocrabConfig) -> Result<()> {
    let mut seen = HashSet::new();
    for agent in &config.agents {
        if !seen.insert(agent.agent_id.as_str()) {
            return Err(anyhow!("duplicate agent_id: {}", agent.agent_id));
        }
    }

    if !seen.contains(config.routing.default_agent_id.as_str()) {
        return Err(anyhow!(
            "default_agent_id does not exist in agents: {}",
            config.routing.default_agent_id
        ));
    }

    for binding in &config.routing.bindings {
        if !seen.contains(binding.agent_id.as_str()) {
            return Err(anyhow!("unknown agent_id in routing: {}", binding.agent_id));
        }
    }

    Ok(())
}

fn read_yaml_file<T>(path: &Path) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read config file: {}", path.display()))?;
    serde_yaml::from_str(&content)
        .with_context(|| format!("failed to parse yaml file: {}", path.display()))
}

fn read_yaml_dir<T>(dir: &Path) -> Result<Vec<T>>
where
    T: for<'de> Deserialize<'de>,
{
    let mut paths = Vec::new();
    for entry in fs::read_dir(dir)
        .with_context(|| format!("failed to read config dir: {}", dir.display()))?
    {
        let entry =
            entry.with_context(|| format!("failed to read dir entry: {}", dir.display()))?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("yaml") {
            paths.push(path);
        }
    }
    paths.sort();

    let mut items = Vec::with_capacity(paths.len());
    for path in paths {
        items.push(read_yaml_file::<T>(&path)?);
    }
    Ok(items)
}

fn resolve_main_env(main: &mut MainConfig) {
    main.app.name = resolve_env_var(&main.app.name);
    main.app.env = resolve_env_var(&main.app.env);

    if let Some(telegram) = &mut main.channels.telegram {
        for connector in &mut telegram.connectors {
            connector.connector_id = resolve_env_var(&connector.connector_id);
            connector.token = resolve_env_var(&connector.token);
        }
    }

    if let Some(discord) = &mut main.channels.discord {
        for connector in &mut discord.connectors {
            connector.connector_id = resolve_env_var(&connector.connector_id);
            connector.token = resolve_env_var(&connector.token);
        }
    }

    main.embedding.api_key = resolve_env_var(&main.embedding.api_key);
    main.embedding.base_url = resolve_env_var(&main.embedding.base_url);
    main.embedding.model = resolve_env_var(&main.embedding.model);
    main.embedding.provider = resolve_env_var(&main.embedding.provider);
}

fn resolve_routing_env(routing: &mut RoutingConfig) {
    routing.default_agent_id = resolve_env_var(&routing.default_agent_id);

    for binding in &mut routing.bindings {
        binding.channel_type = resolve_env_var(&binding.channel_type);
        binding.connector_id = resolve_env_var(&binding.connector_id);
        binding.match_rule.kind = resolve_env_var(&binding.match_rule.kind);
        if let Some(pattern) = &mut binding.match_rule.pattern {
            *pattern = resolve_env_var(pattern);
        }
        binding.agent_id = resolve_env_var(&binding.agent_id);
    }
}

fn resolve_providers_env(providers: &mut [ProviderConfig]) {
    for provider in providers {
        provider.provider_id = resolve_env_var(&provider.provider_id);
        provider.api_base = resolve_env_var(&provider.api_base);
        for model in &mut provider.models {
            *model = resolve_env_var(model);
        }
    }
}

fn resolve_agents_env(agents: &mut [FullAgentConfig]) {
    for agent in agents {
        agent.agent_id = resolve_env_var(&agent.agent_id);
        agent.model_policy.primary = resolve_env_var(&agent.model_policy.primary);
        for fallback in &mut agent.model_policy.fallbacks {
            *fallback = resolve_env_var(fallback);
        }

        if let Some(identity) = &mut agent.identity {
            identity.name = resolve_env_var(&identity.name);
            if let Some(emoji) = &mut identity.emoji {
                *emoji = resolve_env_var(emoji);
            }
        }

        if let Some(tool_policy) = &mut agent.tool_policy {
            for allow in &mut tool_policy.allow {
                *allow = resolve_env_var(allow);
            }
        }

        if let Some(memory_policy) = &mut agent.memory_policy {
            memory_policy.mode = resolve_env_var(&memory_policy.mode);
            memory_policy.write_scope = resolve_env_var(&memory_policy.write_scope);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn fixture_config_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../config")
    }

    #[test]
    fn load_config_from_workspace_fixtures() {
        let config = load_config(&fixture_config_root()).unwrap();
        assert_eq!(config.main.app.name, "nanocrab");
        assert_eq!(config.routing.default_agent_id, "nanocrab-main");
        assert_eq!(config.providers.len(), 2);
        assert_eq!(config.agents.len(), 2);
    }

    #[test]
    fn validate_config_detects_unknown_agent_id_in_routing() {
        let mut config = load_config(&fixture_config_root()).unwrap();
        config.routing.bindings[0].agent_id = "agent-does-not-exist".to_string();

        let err = validate_config(&config).unwrap_err();
        assert!(err.to_string().contains("unknown agent_id"));
    }

    #[test]
    fn validate_config_detects_duplicate_agent_id() {
        let mut config = load_config(&fixture_config_root()).unwrap();
        let duplicate = config.agents[0].clone();
        config.agents.push(duplicate);

        let err = validate_config(&config).unwrap_err();
        assert!(err.to_string().contains("duplicate agent_id"));
    }

    #[test]
    fn resolve_env_var_replaces_env_placeholder() {
        let expected = std::env::var("PATH").unwrap();
        assert_eq!(resolve_env_var("${PATH}"), expected);
    }

    #[test]
    fn resolve_env_var_returns_raw_when_not_placeholder() {
        assert_eq!(resolve_env_var("plain-value"), "plain-value");
    }

    #[test]
    fn resolve_env_var_multiple_placeholders() {
        let home = std::env::var("HOME").unwrap_or_default();
        let user = std::env::var("USER").unwrap_or_default();
        let result = resolve_env_var("home=${HOME},user=${USER}");
        assert_eq!(result, format!("home={home},user={user}"));
    }

    #[test]
    fn resolve_env_var_unclosed_bracket() {
        let result = resolve_env_var("prefix_${UNCLOSED");
        assert_eq!(result, "prefix_${UNCLOSED");
    }

    #[test]
    fn resolve_env_var_missing_env_returns_empty() {
        let result = resolve_env_var("val=${NANOCRAB_NONEXISTENT_VAR_XYZ}");
        assert_eq!(result, "val=");
    }

    #[test]
    fn resolve_env_var_empty_string() {
        assert_eq!(resolve_env_var(""), "");
    }

    #[test]
    fn validate_config_missing_default_agent() {
        let config = NanocrabConfig {
            main: MainConfig {
                app: AppConfig {
                    name: "test".into(),
                    env: "test".into(),
                },
                runtime: RuntimeConfig { max_concurrent: 4 },
                features: FeaturesConfig {
                    multi_agent: false,
                    sub_agent: false,
                    tui: false,
                    cli: true,
                },
                channels: ChannelsConfig {
                    telegram: None,
                    discord: None,
                },
                embedding: EmbeddingConfig::default(),
                tools: ToolsConfig::default(),
            },
            routing: RoutingConfig {
                default_agent_id: "nonexistent".into(),
                bindings: vec![],
            },
            providers: vec![],
            agents: vec![FullAgentConfig {
                agent_id: "agent-a".into(),
                enabled: true,
                identity: None,
                model_policy: super::super::ModelPolicy {
                    primary: "m".into(),
                    fallbacks: vec![],
                },
                tool_policy: None,
                memory_policy: None,
                sub_agent: None,
                workspace: None,
                heartbeat: None,
            }],
        };
        let err = validate_config(&config).unwrap_err();
        assert!(err.to_string().contains("default_agent_id does not exist"));
    }
}
