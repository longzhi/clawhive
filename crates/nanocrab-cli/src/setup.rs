use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use console::{style, Term};
use dialoguer::{theme::ColorfulTheme, Confirm, Input, Password, Select};
use nanocrab_auth::oauth::{profile_from_setup_token, run_openai_pkce_flow, validate_setup_token, OpenAiOAuthConfig};
use nanocrab_auth::{AuthProfile, TokenManager};

use crate::setup_scan::{scan_config, ConfigState};
use crate::setup_ui::{print_done, print_logo, render_dashboard, ARROW, CRAB};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetupAction {
    AddProvider,
    AddAgent,
    AddChannel,
    Modify,
    Remove,
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderId {
    Anthropic,
    OpenAi,
}

impl ProviderId {
    fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
        }
    }

    fn default_model(self) -> &'static str {
        match self {
            Self::Anthropic => "claude-sonnet-4-5",
            Self::OpenAi => "gpt-4o-mini",
        }
    }

    fn api_base(self) -> &'static str {
        match self {
            Self::Anthropic => "https://api.anthropic.com/v1",
            Self::OpenAi => "https://api.openai.com/v1",
        }
    }

    fn default_api_key_env(self) -> &'static str {
        match self {
            Self::Anthropic => "ANTHROPIC_API_KEY",
            Self::OpenAi => "OPENAI_API_KEY",
        }
    }
}

#[derive(Debug, Clone)]
enum AuthChoice {
    OAuth { profile_name: String },
    ApiKey { env_var: String },
}

#[derive(Debug, Clone)]
struct AgentSetup {
    agent_id: String,
    name: String,
    emoji: String,
    primary_model: String,
}

#[derive(Debug, Clone)]
struct ChannelConfig {
    connector_id: String,
    token: String,
}

pub async fn run_setup(config_root: &Path, force: bool) -> Result<()> {
    let term = Term::stdout();
    let theme = ColorfulTheme::default();

    print_logo(&term);
    ensure_required_dirs(config_root)?;

    loop {
        let state = scan_config(config_root);
        render_dashboard(&term, &state);

        let actions = build_action_labels(&state);
        let labels: Vec<&str> = actions.iter().map(|(_, label)| label.as_str()).collect();
        let selected = Select::with_theme(&theme)
            .with_prompt("Choose setup action")
            .items(&labels)
            .default(0)
            .interact()?;

        match actions[selected].0 {
            SetupAction::AddProvider => {
                handle_add_provider(config_root, &term, &theme, &state, force).await?;
            }
            SetupAction::AddAgent => {
                handle_add_agent(config_root, &theme, &state, force)?;
            }
            SetupAction::AddChannel => {
                handle_add_channel(config_root, &theme, &state, force)?;
            }
            SetupAction::Modify => {
                handle_modify(config_root, &theme, &state, force).await?;
            }
            SetupAction::Remove => {
                handle_remove(config_root, &theme, &state, force)?;
            }
            SetupAction::Done => {
                term.write_line(&format!("{} {}", CRAB, style("Setup finished.").green().bold()))?;
                break;
            }
        }
    }

    Ok(())
}

fn build_action_labels(state: &ConfigState) -> Vec<(SetupAction, String)> {
    vec![
        (
            SetupAction::AddProvider,
            format!("{} Add Provider ({})", ARROW, state.providers.len()),
        ),
        (
            SetupAction::AddAgent,
            format!("{} Add Agent ({})", ARROW, state.agents.len()),
        ),
        (
            SetupAction::AddChannel,
            format!("{} Add Channel ({})", ARROW, state.channels.len()),
        ),
        (SetupAction::Modify, format!("{} Modify existing item", ARROW)),
        (SetupAction::Remove, format!("{} Remove item", ARROW)),
        (SetupAction::Done, "Done".to_string()),
    ]
}

async fn handle_add_provider(
    config_root: &Path,
    term: &Term,
    theme: &ColorfulTheme,
    state: &ConfigState,
    force: bool,
) -> Result<()> {
    let provider = prompt_provider(theme)?;

    let already_configured = state
        .providers
        .iter()
        .any(|item| item.provider_id == provider.as_str());
    if already_configured && !force {
        let should_reconfigure = Confirm::with_theme(theme)
            .with_prompt(format!("{} already configured. Reconfigure?", provider.as_str()))
            .default(false)
            .interact()?;
        if !should_reconfigure {
            term.write_line("Provider unchanged.")?;
            return Ok(());
        }
    }

    let auth = prompt_auth_choice(theme, provider).await?;
    let path = write_provider_config_unchecked(config_root, provider, &auth)?;
    print_done(
        term,
        &format!("Provider configuration saved: {}", display_rel(config_root, &path)),
    );
    Ok(())
}

fn handle_add_agent(
    _config_root: &Path,
    _theme: &ColorfulTheme,
    _state: &ConfigState,
    _force: bool,
) -> Result<()> {
    Ok(())
}

fn handle_add_channel(
    _config_root: &Path,
    _theme: &ColorfulTheme,
    _state: &ConfigState,
    _force: bool,
) -> Result<()> {
    Ok(())
}

async fn handle_modify(
    _config_root: &Path,
    _theme: &ColorfulTheme,
    _state: &ConfigState,
    _force: bool,
) -> Result<()> {
    Ok(())
}

fn handle_remove(
    _config_root: &Path,
    _theme: &ColorfulTheme,
    _state: &ConfigState,
    _force: bool,
) -> Result<()> {
    Ok(())
}

fn display_rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

fn ensure_required_dirs(config_root: &Path) -> Result<()> {
    for rel in [
        "config/agents.d",
        "config/providers.d",
        "prompts",
        "skills",
        "data",
        "logs",
    ] {
        let dir = config_root.join(rel);
        fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    }
    Ok(())
}

fn validate_generated_config(config_root: &Path) -> Result<()> {
    let config_path = config_root.join("config");
    nanocrab_core::load_config(&config_path)
        .with_context(|| format!("config validation failed in {}", config_path.display()))?;
    Ok(())
}

fn prompt_channel_config(
    theme: &ColorfulTheme,
    channel_name: &str,
    default_connector_id: &str,
) -> Result<Option<ChannelConfig>> {
    let enabled = Confirm::with_theme(theme)
        .with_prompt(format!("Enable {channel_name}?"))
        .default(channel_name == "Telegram")
        .interact()?;
    if !enabled {
        return Ok(None);
    }

    let connector_id: String = Input::with_theme(theme)
        .with_prompt(format!("{channel_name} connector_id"))
        .default(default_connector_id.to_string())
        .interact_text()?;
    let token = Password::with_theme(theme)
        .with_prompt(format!("{channel_name} bot token"))
        .allow_empty_password(false)
        .interact()?;
    if token.trim().is_empty() {
        anyhow::bail!("{channel_name} token cannot be empty");
    }

    Ok(Some(ChannelConfig {
        connector_id,
        token,
    }))
}

fn prompt_agent_setup(theme: &ColorfulTheme, provider: ProviderId) -> Result<AgentSetup> {
    let agent_id: String = dialoguer::Input::with_theme(theme)
        .with_prompt("Agent ID")
        .default("nanocrab-main".to_string())
        .interact_text()?;
    let name: String = dialoguer::Input::with_theme(theme)
        .with_prompt("Agent display name")
        .default("Nanocrab".to_string())
        .interact_text()?;
    let emoji: String = dialoguer::Input::with_theme(theme)
        .with_prompt("Agent emoji")
        .default("🦀".to_string())
        .interact_text()?;

    let models = provider_models(provider);
    let model_labels: Vec<&str> = models.iter().map(String::as_str).collect();
    let selected = Select::with_theme(theme)
        .with_prompt("Primary model")
        .items(&model_labels)
        .default(0)
        .interact()?;

    let agent_id = agent_id.trim().to_string();
    let name = name.trim().to_string();
    let emoji = emoji.trim().to_string();
    if agent_id.is_empty() {
        anyhow::bail!("agent id cannot be empty");
    }
    if name.is_empty() {
        anyhow::bail!("agent display name cannot be empty");
    }

    Ok(AgentSetup {
        agent_id,
        name,
        emoji,
        primary_model: models[selected].clone(),
    })
}

fn prompt_provider(theme: &ColorfulTheme) -> Result<ProviderId> {
    let options = ["Anthropic", "OpenAI"];
    let selected = Select::with_theme(theme)
        .with_prompt("Choose your LLM provider")
        .items(&options)
        .default(0)
        .interact()?;

    match selected {
        0 => Ok(ProviderId::Anthropic),
        1 => Ok(ProviderId::OpenAi),
        _ => Err(anyhow!("invalid provider selection index: {selected}")),
    }
}

async fn prompt_auth_choice(theme: &ColorfulTheme, provider: ProviderId) -> Result<AuthChoice> {
    let methods = ["OAuth Login (use your subscription)", "API Key"];
    let method = Select::with_theme(theme)
        .with_prompt("Authentication method")
        .items(&methods)
        .default(0)
        .interact()?;

    match method {
        0 => run_oauth_auth(provider).await,
        1 => {
            let api_key = Password::with_theme(theme)
                .with_prompt(format!("Paste {} API key", provider.as_str()))
                .allow_empty_password(false)
                .interact()?;
            if api_key.trim().is_empty() {
                anyhow::bail!("API key cannot be empty");
            }
            Ok(AuthChoice::ApiKey {
                env_var: provider.default_api_key_env().to_string(),
            })
        }
        _ => Err(anyhow!("invalid auth method index: {method}")),
    }
}

async fn run_oauth_auth(provider: ProviderId) -> Result<AuthChoice> {
    let manager = TokenManager::new()?;
    let profile_name = format!("{}-{}", provider.as_str(), unix_timestamp()?);

    match provider {
        ProviderId::OpenAi => {
            let client_id = std::env::var("OPENAI_OAUTH_CLIENT_ID")
                .map_err(|_| anyhow!("OPENAI_OAUTH_CLIENT_ID is not set"))?;
            let config = OpenAiOAuthConfig::default_with_client(client_id);
            let http = reqwest::Client::new();
            let token = run_openai_pkce_flow(&http, &config).await?;
            manager.save_profile(
                &profile_name,
                AuthProfile::OpenAiOAuth {
                    access_token: token.access_token,
                    refresh_token: token.refresh_token,
                    expires_at: unix_timestamp()? + token.expires_in,
                },
            )?;
        }
        ProviderId::Anthropic => {
            let token = Password::new()
                .with_prompt("Paste your Anthropic setup-token")
                .allow_empty_password(false)
                .interact()
                .context("failed to read Anthropic setup-token")?;
            let http = reqwest::Client::new();
            let ok = validate_setup_token(&http, &token, "https://api.anthropic.com/v1/models").await?;
            if !ok {
                anyhow::bail!("Anthropic setup-token validation failed");
            }
            manager.save_profile(&profile_name, profile_from_setup_token(token))?;
        }
    }

    Ok(AuthChoice::OAuth { profile_name })
}

fn write_provider_config(config_root: &Path, provider: ProviderId, auth: &AuthChoice, force: bool) -> Result<PathBuf> {
    let providers_dir = config_root.join("config/providers.d");
    fs::create_dir_all(&providers_dir)
        .with_context(|| format!("failed to create {}", providers_dir.display()))?;

    let target = providers_dir.join(format!("{}.yaml", provider.as_str()));
    if target.exists() && !force {
        anyhow::bail!(
            "provider config already exists: {} (use --force to overwrite)",
            target.display()
        );
    }

    let yaml = generate_provider_yaml(provider, auth);
    fs::write(&target, yaml).with_context(|| format!("failed to write {}", target.display()))?;
    Ok(target)
}

fn write_provider_config_unchecked(
    config_root: &Path,
    provider: ProviderId,
    auth: &AuthChoice,
) -> Result<PathBuf> {
    let providers_dir = config_root.join("config/providers.d");
    fs::create_dir_all(&providers_dir)
        .with_context(|| format!("failed to create {}", providers_dir.display()))?;

    let target = providers_dir.join(format!("{}.yaml", provider.as_str()));
    let yaml = generate_provider_yaml(provider, auth);
    fs::write(&target, yaml).with_context(|| format!("failed to write {}", target.display()))?;
    Ok(target)
}

fn write_agent_files(config_root: &Path, agent: &AgentSetup, force: bool) -> Result<(PathBuf, PathBuf)> {
    let agents_dir = config_root.join("config/agents.d");
    fs::create_dir_all(&agents_dir)
        .with_context(|| format!("failed to create {}", agents_dir.display()))?;

    let agent_yaml_path = agents_dir.join(format!("{}.yaml", agent.agent_id));
    if agent_yaml_path.exists() && !force {
        anyhow::bail!(
            "agent config already exists: {} (use --force to overwrite)",
            agent_yaml_path.display()
        );
    }

    let yaml = generate_agent_yaml(&agent.agent_id, &agent.name, &agent.emoji, &agent.primary_model);
    fs::write(&agent_yaml_path, yaml)
        .with_context(|| format!("failed to write {}", agent_yaml_path.display()))?;

    let prompt_dir = config_root.join("prompts").join(&agent.agent_id);
    fs::create_dir_all(&prompt_dir)
        .with_context(|| format!("failed to create {}", prompt_dir.display()))?;
    let prompt_path = prompt_dir.join("system.md");
    if !prompt_path.exists() || force {
        fs::write(&prompt_path, default_system_prompt(&agent.name))
            .with_context(|| format!("failed to write {}", prompt_path.display()))?;
    }

    Ok((agent_yaml_path, prompt_path))
}

fn write_main_and_routing(
    config_root: &Path,
    default_agent_id: &str,
    telegram: Option<ChannelConfig>,
    discord: Option<ChannelConfig>,
    force: bool,
) -> Result<(PathBuf, PathBuf)> {
    let config_dir = config_root.join("config");
    fs::create_dir_all(&config_dir)
        .with_context(|| format!("failed to create {}", config_dir.display()))?;

    let main_path = config_dir.join("main.yaml");
    let routing_path = config_dir.join("routing.yaml");
    if (!force) && (main_path.exists() || routing_path.exists()) {
        anyhow::bail!(
            "config files already exist: {} or {} (use --force to overwrite)",
            main_path.display(),
            routing_path.display()
        );
    }

    let main_yaml = generate_main_yaml("nanocrab", telegram.clone(), discord.clone());
    let routing_yaml = generate_routing_yaml(default_agent_id, telegram, discord);
    fs::write(&main_path, main_yaml)
        .with_context(|| format!("failed to write {}", main_path.display()))?;
    fs::write(&routing_path, routing_yaml)
        .with_context(|| format!("failed to write {}", routing_path.display()))?;

    Ok((main_path, routing_path))
}

fn generate_provider_yaml(provider: ProviderId, auth: &AuthChoice) -> String {
    match auth {
        AuthChoice::OAuth { profile_name } => format!(
            "provider_id: {provider}\nenabled: true\napi_base: {base}\napi_key_env: {env}\nauth_profile: \"{profile}\"\nmodels:\n  - {model}\n",
            provider = provider.as_str(),
            base = provider.api_base(),
            env = provider.default_api_key_env(),
            profile = profile_name,
            model = provider.default_model(),
        ),
        AuthChoice::ApiKey { env_var } => format!(
            "provider_id: {provider}\nenabled: true\napi_base: {base}\napi_key_env: {env}\nmodels:\n  - {model}\n",
            provider = provider.as_str(),
            base = provider.api_base(),
            env = env_var,
            model = provider.default_model(),
        ),
    }
}

fn generate_agent_yaml(agent_id: &str, name: &str, emoji: &str, primary_model: &str) -> String {
    format!(
        "agent_id: {agent_id}\nenabled: true\nidentity:\n  name: \"{name}\"\n  emoji: \"{emoji}\"\nmodel_policy:\n  primary: \"{primary_model}\"\n  fallbacks: []\nmemory_policy:\n  mode: \"standard\"\n  write_scope: \"all\"\n"
    )
}

fn default_system_prompt(agent_name: &str) -> String {
    format!(
        "You are {agent_name}, a helpful AI assistant powered by nanocrab.\n\nYou are knowledgeable, concise, and friendly. When you don't know something, you say so honestly.\n"
    )
}

fn generate_main_yaml(
    app_name: &str,
    telegram: Option<ChannelConfig>,
    discord: Option<ChannelConfig>,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "app:\n  name: {app_name}\n  env: dev\n\nruntime:\n  max_concurrent: 4\n\nfeatures:\n  multi_agent: true\n  sub_agent: true\n  tui: true\n  cli: true\n\nchannels:\n"
    ));

    match telegram {
        Some(cfg) => {
            out.push_str("  telegram:\n    enabled: true\n    connectors:\n");
            out.push_str(&format!(
                "      - connector_id: {}\n        token: \"{}\"\n",
                cfg.connector_id, cfg.token
            ));
        }
        None => {
            out.push_str("  telegram:\n    enabled: false\n    connectors: []\n");
        }
    }

    match discord {
        Some(cfg) => {
            out.push_str("  discord:\n    enabled: true\n    connectors:\n");
            out.push_str(&format!(
                "      - connector_id: {}\n        token: \"{}\"\n",
                cfg.connector_id, cfg.token
            ));
        }
        None => {
            out.push_str("  discord:\n    enabled: false\n    connectors: []\n");
        }
    }

    out.push_str(
        "\nembedding:\n  enabled: false\n  provider: stub\n  api_key_env: \"\"\n  model: text-embedding-3-small\n  dimensions: 1536\n  base_url: https://api.openai.com/v1\n\ntools: {}\n",
    );

    out
}

fn generate_routing_yaml(
    default_agent_id: &str,
    telegram: Option<ChannelConfig>,
    discord: Option<ChannelConfig>,
) -> String {
    let mut out = format!("default_agent_id: {default_agent_id}\n\nbindings:\n");

    if let Some(cfg) = telegram {
        out.push_str(&format!(
            "  - channel_type: telegram\n    connector_id: {}\n    match:\n      kind: dm\n    agent_id: {}\n",
            cfg.connector_id, default_agent_id
        ));
    }
    if let Some(cfg) = discord {
        out.push_str(&format!(
            "  - channel_type: discord\n    connector_id: {}\n    match:\n      kind: dm\n    agent_id: {}\n",
            cfg.connector_id, default_agent_id
        ));
    }

    out
}

fn provider_models(provider: ProviderId) -> Vec<String> {
    match provider {
        ProviderId::Anthropic => vec![
            "anthropic/claude-sonnet-4-5".to_string(),
            "anthropic/claude-3-haiku-20240307".to_string(),
        ],
        ProviderId::OpenAi => vec!["openai/gpt-4o-mini".to_string(), "openai/gpt-4o".to_string()],
    }
}

fn unix_timestamp() -> Result<i64> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| anyhow!("system clock before unix epoch: {e}"))?;
    Ok(now.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::{
        build_action_labels, default_system_prompt, ensure_required_dirs, generate_agent_yaml,
        generate_main_yaml, generate_provider_yaml, generate_routing_yaml, provider_models,
        validate_generated_config, write_provider_config_unchecked, AuthChoice, ChannelConfig,
        ProviderId, SetupAction,
    };
    use crate::setup_scan::ConfigState;

    #[test]
    fn provider_yaml_uses_auth_profile_for_oauth() {
        let yaml = generate_provider_yaml(
            ProviderId::OpenAi,
            &AuthChoice::OAuth {
                profile_name: "openai-oauth".to_string(),
            },
        );

        assert!(yaml.contains("provider_id: openai"));
        assert!(yaml.contains("auth_profile: \"openai-oauth\""));
        assert!(yaml.contains("api_key_env: OPENAI_API_KEY"));
        assert!(!yaml.contains("api_key:"));
    }

    #[test]
    fn provider_yaml_uses_api_key_env_for_api_key_auth() {
        let yaml = generate_provider_yaml(
            ProviderId::Anthropic,
            &AuthChoice::ApiKey {
                env_var: "ANTHROPIC_API_KEY".to_string(),
            },
        );

        assert!(yaml.contains("provider_id: anthropic"));
        assert!(yaml.contains("api_key_env: ANTHROPIC_API_KEY"));
        assert!(!yaml.contains("auth_profile:"));
    }

    #[test]
    fn agent_yaml_contains_identity_and_model_policy() {
        let yaml = generate_agent_yaml("nanocrab-main", "Nanocrab", "🦀", "openai/gpt-4o-mini");

        assert!(yaml.contains("agent_id: nanocrab-main"));
        assert!(yaml.contains("name: \"Nanocrab\""));
        assert!(yaml.contains("emoji: \"🦀\""));
        assert!(yaml.contains("primary: \"openai/gpt-4o-mini\""));
    }

    #[test]
    fn default_system_prompt_contains_agent_name() {
        let prompt = default_system_prompt("Nanocrab");
        assert!(prompt.contains("You are Nanocrab"));
        assert!(prompt.contains("helpful AI assistant"));
    }

    #[test]
    fn provider_model_aliases_are_fully_qualified() {
        let anthropic_models = provider_models(ProviderId::Anthropic);
        let openai_models = provider_models(ProviderId::OpenAi);

        assert!(anthropic_models.iter().all(|m| m.starts_with("anthropic/")));
        assert!(openai_models.iter().all(|m| m.starts_with("openai/")));
    }

    #[test]
    fn main_yaml_writes_plaintext_channel_tokens() {
        let yaml = generate_main_yaml(
            "nanocrab",
            Some(ChannelConfig {
                connector_id: "tg-main".to_string(),
                token: "123:telegram-token".to_string(),
            }),
            Some(ChannelConfig {
                connector_id: "dc-main".to_string(),
                token: "discord-token".to_string(),
            }),
        );

        assert!(yaml.contains("token: \"123:telegram-token\""));
        assert!(yaml.contains("token: \"discord-token\""));
        assert!(!yaml.contains("${"));
    }

    #[test]
    fn routing_yaml_contains_bindings_for_enabled_channels() {
        let yaml = generate_routing_yaml(
            "nanocrab-main",
            Some(ChannelConfig {
                connector_id: "tg-main".to_string(),
                token: "ignored".to_string(),
            }),
            Some(ChannelConfig {
                connector_id: "dc-main".to_string(),
                token: "ignored".to_string(),
            }),
        );

        assert!(yaml.contains("channel_type: telegram"));
        assert!(yaml.contains("channel_type: discord"));
        assert!(yaml.contains("connector_id: tg-main"));
        assert!(yaml.contains("connector_id: dc-main"));
        assert!(yaml.contains("agent_id: nanocrab-main"));
    }

    #[test]
    fn ensure_required_dirs_creates_expected_paths() {
        let temp = tempfile::tempdir().expect("create tempdir");
        ensure_required_dirs(temp.path()).expect("create required directories");

        for rel in [
            "config/agents.d",
            "config/providers.d",
            "prompts",
            "skills",
            "data",
            "logs",
        ] {
            assert!(temp.path().join(rel).exists(), "missing {rel}");
        }
    }

    #[test]
    fn validate_generated_config_accepts_minimal_valid_files() {
        let temp = tempfile::tempdir().expect("create tempdir");
        ensure_required_dirs(temp.path()).expect("create required directories");

        std::fs::write(
            temp.path().join("config/main.yaml"),
            generate_main_yaml("nanocrab", None, None),
        )
        .expect("write main.yaml");
        std::fs::write(
            temp.path().join("config/routing.yaml"),
            generate_routing_yaml("nanocrab-main", None, None),
        )
        .expect("write routing.yaml");
        std::fs::write(
            temp.path().join("config/providers.d/openai.yaml"),
            generate_provider_yaml(
                ProviderId::OpenAi,
                &AuthChoice::ApiKey {
                    env_var: "OPENAI_API_KEY".to_string(),
                },
            ),
        )
        .expect("write provider yaml");
        std::fs::write(
            temp.path().join("config/agents.d/nanocrab-main.yaml"),
            generate_agent_yaml(
                "nanocrab-main",
                "Nanocrab",
                "🦀",
                "openai/gpt-4o-mini",
            ),
        )
        .expect("write agent yaml");

        validate_generated_config(temp.path()).expect("generated config should be valid");
    }

    #[test]
    fn build_action_labels_includes_all_actions() {
        let labels = build_action_labels(&ConfigState {
            providers: vec![],
            agents: vec![],
            channels: vec![],
            default_agent: None,
        });

        assert_eq!(labels.len(), 6);
        assert!(matches!(labels[0].0, SetupAction::AddProvider));
        assert!(matches!(labels[1].0, SetupAction::AddAgent));
        assert!(matches!(labels[2].0, SetupAction::AddChannel));
        assert!(matches!(labels[3].0, SetupAction::Modify));
        assert!(matches!(labels[4].0, SetupAction::Remove));
        assert!(matches!(labels[5].0, SetupAction::Done));
    }

    #[test]
    fn write_provider_config_unchecked_overwrites_existing_file() {
        let temp = tempfile::tempdir().expect("create tempdir");
        ensure_required_dirs(temp.path()).expect("create required directories");

        let target = temp.path().join("config/providers.d/openai.yaml");
        std::fs::write(&target, "old: value\n").expect("write old provider file");

        write_provider_config_unchecked(
            temp.path(),
            ProviderId::OpenAi,
            &AuthChoice::ApiKey {
                env_var: "OPENAI_API_KEY".to_string(),
            },
        )
        .expect("write provider config");

        let updated = std::fs::read_to_string(&target).expect("read updated provider file");
        assert!(updated.contains("provider_id: openai"));
        assert!(!updated.contains("old: value"));
    }
}
