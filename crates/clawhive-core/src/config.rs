use std::{collections::HashSet, fs, path::Path};

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Deserializer, Serialize};

use super::ModelPolicy;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub name: String,
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
            enabled: true,
            provider: "auto".to_string(),
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
    #[serde(default = "default_true")]
    pub require_mention: bool,
    #[serde(default)]
    pub allow_from: Vec<String>,
    #[serde(default = "default_dm_policy")]
    pub dm_policy: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramChannelConfig {
    pub enabled: bool,
    #[serde(default)]
    pub connectors: Vec<TelegramConnectorConfig>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordConnectorConfig {
    pub connector_id: String,
    pub token: String,
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default = "default_true")]
    pub require_mention: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscordChannelConfig {
    pub enabled: bool,
    #[serde(default)]
    pub connectors: Vec<DiscordConnectorConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuConnectorConfig {
    pub connector_id: String,
    /// Feishu app ID (from Developer Console)
    pub app_id: String,
    /// Feishu app secret
    pub app_secret: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeishuChannelConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub connectors: Vec<FeishuConnectorConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DingTalkConnectorConfig {
    pub connector_id: String,
    /// DingTalk OAuth ClientID (AppKey)
    pub client_id: String,
    /// DingTalk OAuth ClientSecret (AppSecret)
    pub client_secret: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DingTalkChannelConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub connectors: Vec<DingTalkConnectorConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeComConnectorConfig {
    pub connector_id: String,
    /// WeCom AI Bot ID (from admin console)
    pub bot_id: String,
    /// WeCom AI Bot secret
    pub secret: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeComChannelConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub connectors: Vec<WeComConnectorConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeixinConnectorConfig {
    pub connector_id: String,
    #[serde(default)]
    pub bot_token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeixinChannelConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub connectors: Vec<WeixinConnectorConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlackConnectorConfig {
    pub connector_id: String,
    pub bot_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlackChannelConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub connectors: Vec<SlackConnectorConfig>,
}

fn default_whatsapp_db_path() -> String {
    "~/.clawhive/data/whatsapp.db".to_string()
}

fn default_dm_policy() -> String {
    "allowlist".to_string()
}

fn default_group_policy() -> String {
    "disabled".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhatsAppConnectorConfig {
    pub connector_id: String,
    #[serde(default = "default_whatsapp_db_path")]
    pub db_path: String,
    #[serde(default = "default_dm_policy")]
    pub dm_policy: String,
    #[serde(default)]
    pub allow_from: Vec<String>,
    #[serde(default = "default_group_policy")]
    pub group_policy: String,
    #[serde(default)]
    pub group_allow_from: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhatsAppChannelConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub connectors: Vec<WhatsAppConnectorConfig>,
}

fn default_poll_interval_secs() -> u64 {
    5
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IMessageConnectorConfig {
    pub connector_id: String,
    #[serde(default = "default_poll_interval_secs")]
    pub poll_interval_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IMessageChannelConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub connectors: Vec<IMessageConnectorConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookAuthConfig {
    pub method: String,
    #[serde(default)]
    pub key_hash: Option<String>,
    #[serde(default)]
    pub key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookSourceConfig {
    pub source_id: String,
    #[serde(default = "default_raw_format")]
    pub format: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Instruction prompt prepended to normalized payload before sending to agent.
    #[serde(default)]
    pub prompt: Option<String>,
    pub auth: WebhookAuthConfig,
    #[serde(default)]
    pub created_at: Option<String>,
}

fn default_raw_format() -> String {
    "raw".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebhookChannelConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub sources: Vec<WebhookSourceConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryRoutingConfig {
    pub mode: String,
    pub channel: String,
    pub connector_id: String,
    #[serde(default)]
    pub target: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelsConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telegram: Option<TelegramChannelConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discord: Option<DiscordChannelConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feishu: Option<FeishuChannelConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dingtalk: Option<DingTalkChannelConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wecom: Option<WeComChannelConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slack: Option<SlackChannelConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub whatsapp: Option<WhatsAppChannelConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imessage: Option<IMessageChannelConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook: Option<WebhookChannelConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weixin: Option<WeixinChannelConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebSearchProviderConfig {
    pub provider: String,
    pub api_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WebSearchConfig {
    #[serde(default)]
    pub enabled: bool,
    // Legacy single-provider format (backward compat)
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub api_key: Option<String>,
    // New multi-provider format
    #[serde(default)]
    pub providers: Vec<WebSearchProviderConfig>,
}

impl WebSearchConfig {
    /// Merge legacy and new format into a single provider list.
    /// If both exist and overlap on provider name, the new format wins.
    pub fn resolved_providers(&self) -> Vec<WebSearchProviderConfig> {
        let mut result = self.providers.clone();

        // Prepend legacy format if it's not already in the list
        if let (Some(provider), Some(api_key)) = (&self.provider, &self.api_key) {
            if !api_key.is_empty() && !result.iter().any(|p| p.provider == *provider) {
                result.insert(
                    0,
                    WebSearchProviderConfig {
                        provider: provider.clone(),
                        api_key: api_key.clone(),
                    },
                );
            }
        }

        result
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ActionbookConfig {
    #[serde(default)]
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolsConfig {
    #[serde(default)]
    pub web_search: Option<WebSearchConfig>,
    #[serde(default)]
    pub actionbook: Option<ActionbookConfig>,
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_consolidation_interval_hours() -> u64 {
    24
}

fn default_consolidation_schedule() -> String {
    "0 4 * * *".to_string()
}

fn default_archive_retention_days() -> u64 {
    30
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
    #[serde(default)]
    pub memory_search: MemorySearchConfig,
    #[serde(default = "default_consolidation_interval_hours")]
    pub consolidation_interval_hours: u64,
    #[serde(default = "default_consolidation_schedule")]
    pub consolidation_schedule: String,
    #[serde(default = "default_archive_retention_days")]
    pub archive_retention_days: u64,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_password_hash: Option<String>,
}

impl Default for MainConfig {
    fn default() -> Self {
        Self {
            app: AppConfig {
                name: "clawhive".to_string(),
            },
            runtime: RuntimeConfig { max_concurrent: 4 },
            features: FeaturesConfig {
                multi_agent: true,
                sub_agent: true,
                tui: true,
                cli: true,
            },
            channels: ChannelsConfig {
                telegram: None,
                discord: None,
                feishu: None,
                dingtalk: None,
                wecom: None,
                slack: None,
                whatsapp: None,
                imessage: None,
                webhook: None,
                weixin: None,
            },
            embedding: EmbeddingConfig::default(),
            tools: ToolsConfig::default(),
            memory_search: MemorySearchConfig::default(),
            consolidation_interval_hours: default_consolidation_interval_hours(),
            consolidation_schedule: default_consolidation_schedule(),
            archive_retention_days: default_archive_retention_days(),
            log_level: default_log_level(),
            web_password_hash: None,
        }
    }
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
    #[serde(default)]
    pub delivery: Option<DeliveryRoutingConfig>,
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
    pub auth_profile: Option<String>,
    #[serde(default)]
    pub provider_type: Option<String>,
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

/// Master security mode for an agent
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum SecurityMode {
    /// All security checks enabled (default)
    #[default]
    Standard,
    /// All security checks disabled - HardBaseline, approval, sandbox restrictions all off
    Off,
}

/// How exec command security is enforced
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ExecSecurityMode {
    /// Block all host exec requests
    Deny,
    /// Allow only allowlisted command patterns (default)
    #[default]
    Allowlist,
    /// Allow everything (use with caution)
    Full,
}

/// Exec approval behavior when command is not in allowlist
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ExecAskMode {
    /// Never prompt user
    Off,
    /// Prompt only when allowlist does not match (default)
    #[default]
    OnMiss,
    /// Prompt on every command
    Always,
}

/// Exec security configuration for an agent
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecSecurityConfig {
    #[serde(default)]
    pub security: ExecSecurityMode,
    #[serde(default)]
    pub ask: ExecAskMode,
    #[serde(default)]
    pub allowlist: Vec<String>,
    #[serde(default)]
    pub safe_bins: Vec<String>,
}

impl Default for ExecSecurityConfig {
    fn default() -> Self {
        Self {
            security: ExecSecurityMode::Allowlist,
            ask: ExecAskMode::OnMiss,
            allowlist: vec![
                "git *".into(),
                "cargo *".into(),
                "npm *".into(),
                "ls *".into(),
                "cat *".into(),
                "echo *".into(),
                "grep *".into(),
                "find *".into(),
                "which *".into(),
                "pwd".into(),
                "whoami".into(),
                "date".into(),
                "mkdir *".into(),
                "cp *".into(),
                "mv *".into(),
                "touch *".into(),
                "head *".into(),
                "tail *".into(),
                "wc *".into(),
                "sort *".into(),
                "uniq *".into(),
            ],
            safe_bins: vec![
                "jq".into(),
                "cut".into(),
                "uniq".into(),
                "head".into(),
                "tail".into(),
                "tr".into(),
                "wc".into(),
            ],
        }
    }
}

/// Sandbox environment configuration for an agent
/// Network access mode for sandbox
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SandboxNetworkMode {
    /// Block all network access from sandbox
    Deny,
    /// Default: allow whitelisted, prompt for unknown domains
    #[default]
    Ask,
    /// Allow all network access
    Allow,
}

impl<'de> Deserialize<'de> for SandboxNetworkMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        match value {
            serde_json::Value::Bool(true) => Ok(Self::Allow),
            serde_json::Value::Bool(false) => Ok(Self::Deny),
            serde_json::Value::String(mode) => match mode.as_str() {
                "deny" => Ok(Self::Deny),
                "ask" => Ok(Self::Ask),
                "allow" => Ok(Self::Allow),
                _ => Err(serde::de::Error::custom(format!(
                    "invalid sandbox network mode '{mode}', expected one of: deny, ask, allow"
                ))),
            },
            _ => Err(serde::de::Error::custom(
                "invalid sandbox network mode type, expected bool or string",
            )),
        }
    }
}

/// Sandbox environment configuration for an agent
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxPolicyConfig {
    /// Network access mode in sandbox
    #[serde(default)]
    pub network: SandboxNetworkMode,
    /// Allowlisted network targets used by sandbox brokers/tooling
    #[serde(default = "default_sandbox_network_allow")]
    pub network_allow: Vec<String>,
    /// Explicit private targets (dangerous) allowed by operator override
    #[serde(default)]
    pub dangerous_allow_private: Vec<String>,
    /// Command timeout in seconds (default: 30)
    #[serde(default = "default_sandbox_timeout")]
    pub timeout_secs: u64,
    /// Max memory in MB (default: 512)
    #[serde(default = "default_sandbox_memory")]
    pub max_memory_mb: u64,
    /// Environment variables to inherit into sandbox
    #[serde(default = "default_sandbox_env")]
    pub env_inherit: Vec<String>,
    /// Executables allowed in sandbox
    #[serde(default = "default_sandbox_exec")]
    pub exec_allow: Vec<String>,
}

fn default_sandbox_timeout() -> u64 {
    30
}

fn default_sandbox_memory() -> u64 {
    512
}

fn default_sandbox_env() -> Vec<String> {
    vec!["PATH".into(), "HOME".into(), "TMPDIR".into()]
}

fn default_sandbox_exec() -> Vec<String> {
    vec!["sh".into()]
}

fn default_sandbox_network_allow() -> Vec<String> {
    vec![
        "github.com".into(),
        "api.github.com".into(),
        "raw.githubusercontent.com".into(),
        "objects.githubusercontent.com".into(),
        "registry.npmjs.org".into(),
        "pypi.org".into(),
        "files.pythonhosted.org".into(),
        "crates.io".into(),
        "static.crates.io".into(),
        "cdn.jsdelivr.net".into(),
        "unpkg.com".into(),
        "docs.rs".into(),
        "doc.rust-lang.org".into(),
        "developer.mozilla.org".into(),
        "api.openai.com".into(),
        "api.anthropic.com".into(),
        "generativelanguage.googleapis.com".into(),
        "api.search.brave.com".into(),
        "duckduckgo.com".into(),
        "www.google.com".into(),
    ]
}

impl Default for SandboxPolicyConfig {
    fn default() -> Self {
        Self {
            network: SandboxNetworkMode::Ask,
            network_allow: default_sandbox_network_allow(),
            dangerous_allow_private: Vec::new(),
            timeout_secs: default_sandbox_timeout(),
            max_memory_mb: default_sandbox_memory(),
            env_inherit: default_sandbox_env(),
            exec_allow: default_sandbox_exec(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryPolicyConfig {
    pub mode: String,
    pub write_scope: String,
    #[serde(default = "default_idle_reset_minutes")]
    pub idle_minutes: Option<u64>,
    #[serde(default = "default_daily_reset_hour")]
    pub daily_at_hour: Option<u8>,
    #[serde(default)]
    pub limit_history_turns: Option<u32>,
    #[serde(default = "default_max_injected_chars")]
    pub max_injected_chars: usize,
    #[serde(default = "default_daily_summary_interval")]
    pub daily_summary_interval: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemorySearchConfig {
    #[serde(default = "default_vector_weight")]
    pub vector_weight: f64,
    #[serde(default = "default_bm25_weight")]
    pub bm25_weight: f64,
    #[serde(default = "default_decay_half_life_days")]
    pub decay_half_life_days: u64,
    #[serde(default = "default_mmr_lambda")]
    pub mmr_lambda: f64,
    #[serde(default = "default_access_boost_factor")]
    pub access_boost_factor: f64,
    #[serde(default = "default_max_results")]
    pub max_results: usize,
    #[serde(default = "default_min_score")]
    pub min_score: f64,
    #[serde(default = "default_embedding_cache_ttl_days")]
    pub embedding_cache_ttl_days: u64,
    #[serde(default)]
    pub temperature: TemperatureConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TemperatureConfig {
    #[serde(default = "default_hot_days")]
    pub hot_days: u64,
    #[serde(default = "default_warm_days")]
    pub warm_days: u64,
    #[serde(default = "default_cold_filter")]
    pub cold_filter: bool,
    #[serde(default = "default_access_protect_count")]
    pub access_protect_count: u64,
}

fn default_vector_weight() -> f64 {
    0.7
}

fn default_bm25_weight() -> f64 {
    0.3
}

fn default_decay_half_life_days() -> u64 {
    30
}

fn default_mmr_lambda() -> f64 {
    0.7
}

fn default_access_boost_factor() -> f64 {
    0.2
}

fn default_max_results() -> usize {
    6
}

fn default_min_score() -> f64 {
    0.35
}

fn default_embedding_cache_ttl_days() -> u64 {
    90
}

fn default_hot_days() -> u64 {
    7
}

fn default_warm_days() -> u64 {
    30
}

fn default_cold_filter() -> bool {
    true
}

fn default_access_protect_count() -> u64 {
    5
}

impl Default for TemperatureConfig {
    fn default() -> Self {
        Self {
            hot_days: default_hot_days(),
            warm_days: default_warm_days(),
            cold_filter: default_cold_filter(),
            access_protect_count: default_access_protect_count(),
        }
    }
}

impl Default for MemorySearchConfig {
    fn default() -> Self {
        Self {
            vector_weight: default_vector_weight(),
            bm25_weight: default_bm25_weight(),
            decay_half_life_days: default_decay_half_life_days(),
            mmr_lambda: default_mmr_lambda(),
            access_boost_factor: default_access_boost_factor(),
            max_results: default_max_results(),
            min_score: default_min_score(),
            embedding_cache_ttl_days: default_embedding_cache_ttl_days(),
            temperature: TemperatureConfig::default(),
        }
    }
}

fn default_max_injected_chars() -> usize {
    6000
}

fn default_idle_reset_minutes() -> Option<u64> {
    None
}

fn default_daily_reset_hour() -> Option<u8> {
    None
}

fn default_daily_summary_interval() -> u64 {
    0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentPolicyConfig {
    pub allow_spawn: bool,
    #[serde(default = "default_max_concurrent_subagents")]
    pub max_concurrent: usize,
}

fn default_max_concurrent_subagents() -> usize {
    5
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
    pub security: SecurityMode,
    #[serde(default)]
    pub workspace: Option<String>,
    pub identity: Option<IdentityConfig>,
    pub model_policy: ModelPolicy,
    pub tool_policy: Option<ToolPolicyConfig>,
    pub memory_policy: Option<MemoryPolicyConfig>,
    pub sub_agent: Option<SubAgentPolicyConfig>,
    #[serde(default)]
    pub heartbeat: Option<HeartbeatPolicyConfig>,
    #[serde(default)]
    pub exec_security: Option<ExecSecurityConfig>,
    #[serde(default)]
    pub sandbox: Option<SandboxPolicyConfig>,
    /// Maximum output tokens per LLM response. Defaults to 4096 (conversation) / 8192 (scheduled).
    #[serde(default)]
    pub max_response_tokens: Option<u32>,
    /// Maximum tool-use loop iterations. Defaults to 50.
    #[serde(default)]
    pub max_iterations: Option<u32>,
    /// Maximum seconds an agent turn may run before cooperative abort. Default 1800 (30 min), range 60-7200.
    #[serde(default)]
    pub turn_timeout_secs: Option<u64>,
    /// How long the typing indicator stays active (seconds). Default 120. Clamped to turn_timeout.
    #[serde(default)]
    pub typing_ttl_secs: Option<u64>,
    /// Seconds of silence before sending a progress message. Default 60. Set to 0 to disable.
    #[serde(default)]
    pub progress_delay_secs: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
pub struct TurnLifecycleConfig {
    pub turn_timeout_secs: u64,
    pub typing_ttl_secs: u64,
    pub progress_delay_secs: u64,
}

impl FullAgentConfig {
    pub fn turn_lifecycle(&self) -> TurnLifecycleConfig {
        let turn_timeout = self.turn_timeout_secs.unwrap_or(1800).clamp(60, 7200);
        let typing_ttl = self.typing_ttl_secs.unwrap_or(120);
        let progress_delay = self.progress_delay_secs.unwrap_or(60);
        TurnLifecycleConfig {
            turn_timeout_secs: turn_timeout,
            typing_ttl_secs: typing_ttl.min(turn_timeout),
            progress_delay_secs: progress_delay,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClawhiveConfig {
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
        output.push_str(&crate::dotenv::resolve_env(key).unwrap_or_default());
        rest = &candidate[end + 1..];
    }

    output.push_str(rest);
    output
}

pub fn load_config(root: &Path) -> Result<ClawhiveConfig> {
    let mut main: MainConfig = read_yaml_file(&root.join("main.yaml"))?;
    let mut routing: RoutingConfig = read_yaml_file(&root.join("routing.yaml"))?;

    let mut providers = read_yaml_dir::<ProviderConfig>(&root.join("providers.d"))?;
    let mut agents = read_yaml_dir::<FullAgentConfig>(&root.join("agents.d"))?;

    resolve_main_env(&mut main);
    resolve_routing_env(&mut routing);
    resolve_providers_env(&mut providers);
    resolve_agents_env(&mut agents);

    let config = ClawhiveConfig {
        main,
        routing,
        providers,
        agents,
    };

    validate_config(&config)?;
    Ok(config)
}

pub fn validate_config(config: &ClawhiveConfig) -> Result<()> {
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

    if let Some(telegram) = &mut main.channels.telegram {
        for connector in &mut telegram.connectors {
            connector.connector_id = resolve_env_var(&connector.connector_id);
            connector.token = resolve_env_var(&connector.token);
            connector.dm_policy = resolve_env_var(&connector.dm_policy);
            for allow_from in &mut connector.allow_from {
                *allow_from = resolve_env_var(allow_from);
            }
        }
    }

    if let Some(discord) = &mut main.channels.discord {
        for connector in &mut discord.connectors {
            connector.connector_id = resolve_env_var(&connector.connector_id);
            connector.token = resolve_env_var(&connector.token);
        }
    }

    if let Some(feishu) = &mut main.channels.feishu {
        for connector in &mut feishu.connectors {
            connector.connector_id = resolve_env_var(&connector.connector_id);
            connector.app_id = resolve_env_var(&connector.app_id);
            connector.app_secret = resolve_env_var(&connector.app_secret);
        }
    }

    if let Some(dingtalk) = &mut main.channels.dingtalk {
        for connector in &mut dingtalk.connectors {
            connector.connector_id = resolve_env_var(&connector.connector_id);
            connector.client_id = resolve_env_var(&connector.client_id);
            connector.client_secret = resolve_env_var(&connector.client_secret);
        }
    }

    if let Some(wecom) = &mut main.channels.wecom {
        for connector in &mut wecom.connectors {
            connector.connector_id = resolve_env_var(&connector.connector_id);
            connector.bot_id = resolve_env_var(&connector.bot_id);
            connector.secret = resolve_env_var(&connector.secret);
        }
    }

    if let Some(slack) = &mut main.channels.slack {
        for connector in &mut slack.connectors {
            connector.connector_id = resolve_env_var(&connector.connector_id);
            connector.bot_token = resolve_env_var(&connector.bot_token);
        }
    }

    if let Some(whatsapp) = &mut main.channels.whatsapp {
        for connector in &mut whatsapp.connectors {
            connector.connector_id = resolve_env_var(&connector.connector_id);
            connector.db_path = resolve_env_var(&connector.db_path);
            connector.dm_policy = resolve_env_var(&connector.dm_policy);
            for allow_from in &mut connector.allow_from {
                *allow_from = resolve_env_var(allow_from);
            }
            connector.group_policy = resolve_env_var(&connector.group_policy);
            for group_allow_from in &mut connector.group_allow_from {
                *group_allow_from = resolve_env_var(group_allow_from);
            }
        }
    }

    if let Some(imessage) = &mut main.channels.imessage {
        for connector in &mut imessage.connectors {
            connector.connector_id = resolve_env_var(&connector.connector_id);
        }
    }

    if let Some(webhook) = &mut main.channels.webhook {
        for source in &mut webhook.sources {
            source.source_id = resolve_env_var(&source.source_id);
            if let Some(key) = &mut source.auth.key {
                *key = resolve_env_var(key);
            }
            if let Some(key_hash) = &mut source.auth.key_hash {
                *key_hash = resolve_env_var(key_hash);
            }
        }
    }

    if let Some(weixin) = &mut main.channels.weixin {
        for connector in &mut weixin.connectors {
            connector.connector_id = resolve_env_var(&connector.connector_id);
            if let Some(token) = &mut connector.bot_token {
                *token = resolve_env_var(token);
            }
        }
    }

    main.embedding.api_key = resolve_env_var(&main.embedding.api_key);
    main.embedding.base_url = resolve_env_var(&main.embedding.base_url);
    main.embedding.model = resolve_env_var(&main.embedding.model);
    main.embedding.provider = resolve_env_var(&main.embedding.provider);
    main.log_level = resolve_env_var(&main.log_level);
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
        if let Some(delivery) = &mut binding.delivery {
            delivery.mode = resolve_env_var(&delivery.mode);
            delivery.channel = resolve_env_var(&delivery.channel);
            delivery.connector_id = resolve_env_var(&delivery.connector_id);
            if let Some(target) = &mut delivery.target {
                *target = resolve_env_var(target);
            }
        }
    }
}

fn resolve_providers_env(providers: &mut [ProviderConfig]) {
    for provider in providers {
        provider.provider_id = resolve_env_var(&provider.provider_id);
        provider.api_base = resolve_env_var(&provider.api_base);
        if let Some(profile) = &mut provider.auth_profile {
            *profile = resolve_env_var(profile);
        }
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

        if let Some(exec_security) = &mut agent.exec_security {
            for allow in &mut exec_security.allowlist {
                *allow = resolve_env_var(allow);
            }
            for bin in &mut exec_security.safe_bins {
                *bin = resolve_env_var(bin);
            }
        }

        if let Some(sandbox) = &mut agent.sandbox {
            for host in &mut sandbox.network_allow {
                *host = resolve_env_var(host);
            }
            for host in &mut sandbox.dangerous_allow_private {
                *host = resolve_env_var(host);
            }
            for key in &mut sandbox.env_inherit {
                *key = resolve_env_var(key);
            }
            for cmd in &mut sandbox.exec_allow {
                *cmd = resolve_env_var(cmd);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    /// Create a temporary config directory with minimal valid files for testing.
    fn make_temp_config() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let root = tmp.path().to_path_buf();
        std::fs::create_dir_all(root.join("agents.d")).unwrap();
        std::fs::create_dir_all(root.join("providers.d")).unwrap();
        std::fs::write(
            root.join("main.yaml"),
            "app:\n  name: clawhive\n  env: test\nruntime:\n  max_concurrent: 4\nfeatures:\n  multi_agent: true\n  sub_agent: false\n  tui: false\n  cli: true\nchannels:\n  telegram: null\n  discord: null\n",
        ).unwrap();
        std::fs::write(
            root.join("routing.yaml"),
            "default_agent_id: main-agent\nbindings:\n  - channel_type: telegram\n    connector_id: tg\n    match:\n      kind: dm\n    agent_id: main-agent\n",
        ).unwrap();
        std::fs::write(
            root.join("providers.d/openai.yaml"),
            "provider_id: openai\nenabled: true\napi_base: https://api.openai.com/v1\napi_key: sk-test\nmodels:\n  - gpt-4o\n",
        ).unwrap();
        std::fs::write(
            root.join("agents.d/main-agent.yaml"),
            "agent_id: main-agent\nenabled: true\nmodel_policy:\n  primary: gpt-4o\n  fallbacks: []\n",
        ).unwrap();
        (tmp, root)
    }

    #[test]
    fn load_config_from_temp_fixtures() {
        let (_tmp, root) = make_temp_config();
        let config = load_config(&root).unwrap();
        assert_eq!(config.main.app.name, "clawhive");
        assert_eq!(config.main.consolidation_interval_hours, 24);
        assert_eq!(config.routing.default_agent_id, "main-agent");
        assert_eq!(config.providers.len(), 1);
        assert_eq!(config.agents.len(), 1);
    }

    #[test]
    fn main_config_without_log_level_defaults_to_info() {
        let yaml = r#"
app:
  name: test
runtime:
  max_concurrent: 2
features:
  multi_agent: false
  sub_agent: false
  tui: false
  cli: false
channels: {}
"#;
        let config: MainConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.consolidation_interval_hours, 24);
        assert_eq!(config.log_level, "info");
    }

    #[test]
    fn memory_search_config_deserializes_with_all_defaults() {
        let yaml = r#"
app:
  name: test
runtime:
  max_concurrent: 2
features:
  multi_agent: false
  sub_agent: false
  tui: false
  cli: false
channels: {}
"#;
        let config: MainConfig = serde_yaml::from_str(yaml).unwrap();

        assert_eq!(config.memory_search.vector_weight, 0.7);
        assert_eq!(config.memory_search.bm25_weight, 0.3);
        assert_eq!(config.memory_search.decay_half_life_days, 30);
        assert_eq!(config.memory_search.mmr_lambda, 0.7);
        assert_eq!(config.memory_search.access_boost_factor, 0.2);
        assert_eq!(config.memory_search.max_results, 6);
        assert_eq!(config.memory_search.min_score, 0.35);
        assert_eq!(config.memory_search.embedding_cache_ttl_days, 90);
        assert_eq!(config.memory_search.temperature.hot_days, 7);
        assert_eq!(config.memory_search.temperature.warm_days, 30);
        assert!(config.memory_search.temperature.cold_filter);
        assert_eq!(config.memory_search.temperature.access_protect_count, 5);
    }

    #[test]
    fn memory_policy_config_defaults_max_injected_chars() {
        let yaml = r#"
mode: session
write_scope: session
"#;
        let config: MemoryPolicyConfig = serde_yaml::from_str(yaml).unwrap();

        assert_eq!(config.idle_minutes, None);
        assert_eq!(config.daily_at_hour, None);
        assert_eq!(config.max_injected_chars, 6000);
        assert_eq!(config.daily_summary_interval, 0);
    }

    #[test]
    fn validate_config_detects_unknown_agent_id_in_routing() {
        let (_tmp, root) = make_temp_config();
        let mut config = load_config(&root).unwrap();
        config.routing.bindings[0].agent_id = "agent-does-not-exist".to_string();

        let err = validate_config(&config).unwrap_err();
        assert!(err.to_string().contains("unknown agent_id"));
    }

    #[test]
    fn validate_config_detects_duplicate_agent_id() {
        let (_tmp, root) = make_temp_config();
        let mut config = load_config(&root).unwrap();
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
        let result = resolve_env_var("val=${CLAWHIVE_NONEXISTENT_VAR_XYZ}");
        assert_eq!(result, "val=");
    }

    #[test]
    fn resolve_env_var_empty_string() {
        assert_eq!(resolve_env_var(""), "");
    }

    #[test]
    fn webhook_channel_config_serde_roundtrip() {
        let yaml = r#"
enabled: true
sources:
  - source_id: alertmanager
    format: alertmanager
    description: "Alertmanager production alerts"
    auth:
      method: api_key
      key_hash: "sha256:abcdef1234567890"
  - source_id: my-script
    format: generic
    description: "Custom deploy notifications"
    auth:
      method: api_key
      key: "${WEBHOOK_MY_SCRIPT_KEY}"
"#;
        let config: WebhookChannelConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.enabled);
        assert_eq!(config.sources.len(), 2);
        assert_eq!(config.sources[0].source_id, "alertmanager");
        assert_eq!(config.sources[0].format, "alertmanager");
        assert_eq!(
            config.sources[0].auth.key_hash,
            Some("sha256:abcdef1234567890".to_string())
        );
        assert_eq!(config.sources[1].format, "generic");
        assert!(config.sources[1].auth.key.is_some());
    }

    #[test]
    fn routing_delivery_config_serde_roundtrip() {
        let yaml = r#"
default_agent_id: main-agent
bindings:
  - channel_type: webhook
    connector_id: alertmanager
    match:
      kind: group
    agent_id: devops-bot
    delivery:
      mode: announce
      channel: discord
      connector_id: discord_main
      target: "guild:xxx:channel:ops-alerts"
"#;
        let config: RoutingConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.bindings.len(), 1);
        let binding = &config.bindings[0];
        assert_eq!(binding.channel_type, "webhook");
        let delivery = binding.delivery.as_ref().unwrap();
        assert_eq!(delivery.mode, "announce");
        assert_eq!(delivery.channel, "discord");
        assert_eq!(
            delivery.target,
            Some("guild:xxx:channel:ops-alerts".to_string())
        );
    }

    #[test]
    fn routing_without_delivery_still_works() {
        let yaml = r#"
default_agent_id: main-agent
bindings:
  - channel_type: telegram
    connector_id: tg_main
    match:
      kind: dm
    agent_id: main-agent
"#;
        let config: RoutingConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(config.bindings[0].delivery.is_none());
    }

    #[test]
    fn webhook_source_default_format_is_raw() {
        let yaml = r#"
source_id: test
auth:
  method: api_key
  key: "test-key"
"#;
        let config: WebhookSourceConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(config.format, "raw");
    }

    #[test]
    fn validate_config_missing_default_agent() {
        let config = ClawhiveConfig {
            main: MainConfig {
                app: AppConfig {
                    name: "test".into(),
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
                    feishu: None,
                    dingtalk: None,
                    wecom: None,
                    slack: None,
                    whatsapp: None,
                    imessage: None,
                    webhook: None,
                    weixin: None,
                },
                embedding: EmbeddingConfig::default(),
                tools: ToolsConfig::default(),
                memory_search: MemorySearchConfig::default(),
                consolidation_interval_hours: 24,
                consolidation_schedule: default_consolidation_schedule(),
                archive_retention_days: 30,
                log_level: default_log_level(),
                web_password_hash: None,
            },
            routing: RoutingConfig {
                default_agent_id: "nonexistent".into(),
                bindings: vec![],
            },
            providers: vec![],
            agents: vec![FullAgentConfig {
                agent_id: "agent-a".into(),
                enabled: true,
                security: SecurityMode::default(),
                identity: None,
                model_policy: super::super::ModelPolicy {
                    primary: "m".into(),
                    fallbacks: vec![],
                    thinking_level: None,
                    context_window: None,
                    compaction_model: None,
                },
                tool_policy: None,
                memory_policy: None,
                sub_agent: None,
                workspace: None,
                heartbeat: None,
                exec_security: None,
                sandbox: None,
                max_response_tokens: None,
                max_iterations: None,
                turn_timeout_secs: None,
                typing_ttl_secs: None,
                progress_delay_secs: None,
            }],
        };
        let err = validate_config(&config).unwrap_err();
        assert!(err.to_string().contains("default_agent_id does not exist"));
    }

    #[test]
    fn exec_security_default_values() {
        let cfg = ExecSecurityConfig::default();
        assert_eq!(cfg.security, ExecSecurityMode::Allowlist);
        assert_eq!(cfg.ask, ExecAskMode::OnMiss);
        assert!(cfg.allowlist.iter().any(|p| p == "git *"));
        assert!(cfg.safe_bins.iter().any(|b| b == "jq"));
    }

    #[test]
    fn sandbox_policy_default_values() {
        let cfg = SandboxPolicyConfig::default();
        assert_eq!(cfg.network, SandboxNetworkMode::Ask);
        assert!(!cfg.network_allow.is_empty());
        assert!(cfg.network_allow.iter().any(|h| h.contains("github.com")));
        assert!(cfg.dangerous_allow_private.is_empty());
        assert_eq!(cfg.timeout_secs, 30);
        assert_eq!(cfg.max_memory_mb, 512);
        assert_eq!(cfg.env_inherit, vec!["PATH", "HOME", "TMPDIR"]);
        assert_eq!(cfg.exec_allow, vec!["sh"]);
    }

    #[test]
    fn sandbox_network_mode_from_string() {
        let ask: SandboxNetworkMode = serde_json::from_str("\"ask\"").unwrap();
        assert_eq!(ask, SandboxNetworkMode::Ask);
        let allow: SandboxNetworkMode = serde_json::from_str("\"allow\"").unwrap();
        assert_eq!(allow, SandboxNetworkMode::Allow);
        let deny: SandboxNetworkMode = serde_json::from_str("\"deny\"").unwrap();
        assert_eq!(deny, SandboxNetworkMode::Deny);
    }

    #[test]
    fn sandbox_network_mode_from_bool_compat() {
        let allow: SandboxNetworkMode = serde_json::from_str("true").unwrap();
        assert_eq!(allow, SandboxNetworkMode::Allow);
        let deny: SandboxNetworkMode = serde_json::from_str("false").unwrap();
        assert_eq!(deny, SandboxNetworkMode::Deny);
    }

    #[test]
    fn sandbox_network_mode_default_is_ask() {
        assert_eq!(SandboxNetworkMode::default(), SandboxNetworkMode::Ask);
    }

    #[test]
    fn sandbox_policy_default_network_allow_not_empty() {
        let cfg = SandboxPolicyConfig::default();
        assert!(!cfg.network_allow.is_empty());
        assert!(cfg.network_allow.iter().any(|h| h.contains("github.com")));
    }

    #[test]
    fn security_mode_defaults_to_standard() {
        let cfg: SecurityMode = serde_json::from_str("\"standard\"").unwrap();
        assert_eq!(cfg, SecurityMode::Standard);
    }

    #[test]
    fn security_mode_off() {
        let cfg: SecurityMode = serde_json::from_str("\"off\"").unwrap();
        assert_eq!(cfg, SecurityMode::Off);
    }

    #[test]
    fn security_mode_default_is_standard() {
        assert_eq!(SecurityMode::default(), SecurityMode::Standard);
    }

    #[test]
    fn resolve_main_env_resolves_whatsapp_access_policy_fields() {
        let original_allow = std::env::var("CLAWHIVE_TEST_ALLOW_FROM").ok();
        let original_group = std::env::var("CLAWHIVE_TEST_GROUP_ALLOW_FROM").ok();

        unsafe {
            std::env::set_var("CLAWHIVE_TEST_ALLOW_FROM", "+1234567890");
            std::env::set_var("CLAWHIVE_TEST_GROUP_ALLOW_FROM", "+9876543210");
        }

        let mut main = MainConfig::default();
        main.channels.whatsapp = Some(WhatsAppChannelConfig {
            enabled: true,
            connectors: vec![WhatsAppConnectorConfig {
                connector_id: "${CLAWHIVE_TEST_ALLOW_FROM}".to_string(),
                db_path: "~/.clawhive/data/whatsapp.db".to_string(),
                dm_policy: "${CLAWHIVE_TEST_ALLOW_FROM}".to_string(),
                allow_from: vec!["${CLAWHIVE_TEST_ALLOW_FROM}".to_string()],
                group_policy: "${CLAWHIVE_TEST_GROUP_ALLOW_FROM}".to_string(),
                group_allow_from: vec!["${CLAWHIVE_TEST_GROUP_ALLOW_FROM}".to_string()],
            }],
        });

        resolve_main_env(&mut main);

        let connector = &main.channels.whatsapp.as_ref().unwrap().connectors[0];
        assert_eq!(connector.connector_id, "+1234567890");
        assert_eq!(connector.dm_policy, "+1234567890");
        assert_eq!(connector.allow_from, vec!["+1234567890"]);
        assert_eq!(connector.group_policy, "+9876543210");
        assert_eq!(connector.group_allow_from, vec!["+9876543210"]);

        unsafe {
            match original_allow {
                Some(value) => std::env::set_var("CLAWHIVE_TEST_ALLOW_FROM", value),
                None => std::env::remove_var("CLAWHIVE_TEST_ALLOW_FROM"),
            }
            match original_group {
                Some(value) => std::env::set_var("CLAWHIVE_TEST_GROUP_ALLOW_FROM", value),
                None => std::env::remove_var("CLAWHIVE_TEST_GROUP_ALLOW_FROM"),
            }
        }
    }

    #[test]
    fn turn_lifecycle_defaults() {
        let yaml = "agent_id: a\nenabled: true\nmodel_policy:\n  primary: m\n  fallbacks: []\n";
        let agent: FullAgentConfig = serde_yaml::from_str(yaml).unwrap();
        let lc = agent.turn_lifecycle();
        assert_eq!(lc.turn_timeout_secs, 1800);
        assert_eq!(lc.typing_ttl_secs, 120);
        assert_eq!(lc.progress_delay_secs, 60);
    }

    #[test]
    fn turn_lifecycle_explicit_values() {
        let yaml = "agent_id: a\nenabled: true\nmodel_policy:\n  primary: m\n  fallbacks: []\nturn_timeout_secs: 900\ntyping_ttl_secs: 60\nprogress_delay_secs: 30\n";
        let agent: FullAgentConfig = serde_yaml::from_str(yaml).unwrap();
        let lc = agent.turn_lifecycle();
        assert_eq!(lc.turn_timeout_secs, 900);
        assert_eq!(lc.typing_ttl_secs, 60);
        assert_eq!(lc.progress_delay_secs, 30);
    }

    #[test]
    fn turn_lifecycle_clamps_timeout_low() {
        let yaml = "agent_id: a\nenabled: true\nmodel_policy:\n  primary: m\n  fallbacks: []\nturn_timeout_secs: 10\n";
        let agent: FullAgentConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(agent.turn_lifecycle().turn_timeout_secs, 60);
    }

    #[test]
    fn turn_lifecycle_clamps_timeout_high() {
        let yaml = "agent_id: a\nenabled: true\nmodel_policy:\n  primary: m\n  fallbacks: []\nturn_timeout_secs: 99999\n";
        let agent: FullAgentConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(agent.turn_lifecycle().turn_timeout_secs, 7200);
    }

    #[test]
    fn turn_lifecycle_typing_ttl_clamped_by_timeout() {
        let yaml = "agent_id: a\nenabled: true\nmodel_policy:\n  primary: m\n  fallbacks: []\nturn_timeout_secs: 90\ntyping_ttl_secs: 200\n";
        let agent: FullAgentConfig = serde_yaml::from_str(yaml).unwrap();
        let lc = agent.turn_lifecycle();
        assert_eq!(lc.turn_timeout_secs, 90);
        assert_eq!(lc.typing_ttl_secs, 90);
    }

    #[test]
    fn resolved_providers_old_format_only() {
        let cfg = WebSearchConfig {
            enabled: true,
            provider: Some("brave".into()),
            api_key: Some("key1".into()),
            providers: vec![],
        };
        let providers = cfg.resolved_providers();
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].provider, "brave");
        assert_eq!(providers[0].api_key, "key1");
    }

    #[test]
    fn resolved_providers_new_format_only() {
        let cfg = WebSearchConfig {
            enabled: true,
            provider: None,
            api_key: None,
            providers: vec![
                WebSearchProviderConfig {
                    provider: "brave".into(),
                    api_key: "k1".into(),
                },
                WebSearchProviderConfig {
                    provider: "tavily".into(),
                    api_key: "k2".into(),
                },
            ],
        };
        let providers = cfg.resolved_providers();
        assert_eq!(providers.len(), 2);
        assert_eq!(providers[0].provider, "brave");
        assert_eq!(providers[1].provider, "tavily");
    }

    #[test]
    fn resolved_providers_both_formats_dedup() {
        let cfg = WebSearchConfig {
            enabled: true,
            provider: Some("brave".into()),
            api_key: Some("key-old".into()),
            providers: vec![
                WebSearchProviderConfig {
                    provider: "brave".into(),
                    api_key: "key-new".into(),
                },
                WebSearchProviderConfig {
                    provider: "tavily".into(),
                    api_key: "k2".into(),
                },
            ],
        };
        let providers = cfg.resolved_providers();
        assert_eq!(providers.len(), 2);
        assert_eq!(providers[0].provider, "brave");
        assert_eq!(providers[0].api_key, "key-new");
    }

    #[test]
    fn resolved_providers_both_formats_no_overlap() {
        let cfg = WebSearchConfig {
            enabled: true,
            provider: Some("brave".into()),
            api_key: Some("key-old".into()),
            providers: vec![WebSearchProviderConfig {
                provider: "tavily".into(),
                api_key: "k2".into(),
            }],
        };
        let providers = cfg.resolved_providers();
        assert_eq!(providers.len(), 2);
        assert_eq!(providers[0].provider, "brave");
        assert_eq!(providers[1].provider, "tavily");
    }

    #[test]
    fn resolved_providers_empty() {
        let cfg = WebSearchConfig::default();
        assert!(cfg.resolved_providers().is_empty());
    }
}
