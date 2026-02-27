use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use clawhive_auth::oauth::{
    extract_chatgpt_account_id, profile_from_setup_token, run_openai_pkce_flow,
    validate_setup_token, OpenAiOAuthConfig,
};
use clawhive_auth::{AuthProfile, TokenManager};
use console::{style, Term};
use dialoguer::{theme::ColorfulTheme, Confirm, Input, Password, Select};

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
    AzureOpenAi,
    Gemini,
    DeepSeek,
    Groq,
    Ollama,
    OpenRouter,
    Together,
    Fireworks,
}

const ALL_PROVIDERS: &[ProviderId] = &[
    ProviderId::Anthropic,
    ProviderId::OpenAi,
    ProviderId::AzureOpenAi,
    ProviderId::Gemini,
    ProviderId::DeepSeek,
    ProviderId::Groq,
    ProviderId::Ollama,
    ProviderId::OpenRouter,
    ProviderId::Together,
    ProviderId::Fireworks,
];

impl ProviderId {
    fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
            Self::AzureOpenAi => "azure-openai",
            Self::Gemini => "gemini",
            Self::DeepSeek => "deepseek",
            Self::Groq => "groq",
            Self::Ollama => "ollama",
            Self::OpenRouter => "openrouter",
            Self::Together => "together",
            Self::Fireworks => "fireworks",
        }
    }

    fn display_name(self) -> &'static str {
        match self {
            Self::Anthropic => "Anthropic",
            Self::OpenAi => "OpenAI",
            Self::AzureOpenAi => "Azure OpenAI",
            Self::Gemini => "Google Gemini",
            Self::DeepSeek => "DeepSeek",
            Self::Groq => "Groq",
            Self::Ollama => "Ollama (local)",
            Self::OpenRouter => "OpenRouter",
            Self::Together => "Together AI",
            Self::Fireworks => "Fireworks AI",
        }
    }

    fn default_model(self) -> &'static str {
        match self {
            Self::Anthropic => "claude-sonnet-4-5",
            Self::OpenAi => "gpt-4o-mini",
            Self::AzureOpenAi => "gpt-4o-mini",
            Self::Gemini => "gemini-2.0-flash",
            Self::DeepSeek => "deepseek-chat",
            Self::Groq => "llama-3.3-70b-versatile",
            Self::Ollama => "llama3.2",
            Self::OpenRouter => "anthropic/claude-sonnet-4-5",
            Self::Together => "meta-llama/Llama-3.3-70B-Instruct-Turbo",
            Self::Fireworks => "accounts/fireworks/models/llama-v3p3-70b-instruct",
        }
    }

    fn api_base(self) -> &'static str {
        match self {
            Self::Anthropic => "https://api.anthropic.com/v1",
            Self::OpenAi => "https://api.openai.com/v1",
            Self::AzureOpenAi => "https://<your-resource>.openai.azure.com/openai",
            Self::Gemini => "https://generativelanguage.googleapis.com/v1beta",
            Self::DeepSeek => "https://api.deepseek.com/v1",
            Self::Groq => "https://api.groq.com/openai/v1",
            Self::Ollama => "http://localhost:11434/v1",
            Self::OpenRouter => "https://openrouter.ai/api/v1",
            Self::Together => "https://api.together.xyz/v1",
            Self::Fireworks => "https://api.fireworks.ai/inference/v1",
        }
    }

    fn supports_oauth(self) -> bool {
        matches!(self, Self::Anthropic | Self::OpenAi)
    }

    fn needs_custom_base_url(self) -> bool {
        matches!(self, Self::AzureOpenAi)
    }

    fn from_str(s: &str) -> Option<Self> {
        ALL_PROVIDERS.iter().find(|p| p.as_str() == s).copied()
    }
}

#[derive(Debug, Clone)]
enum AuthChoice {
    OAuth { profile_name: String },
    ApiKey { api_key: String },
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
                if let Err(err) = validate_generated_config(config_root) {
                    term.write_line(&format!(
                        "{} {}",
                        ARROW,
                        style(format!("Config validation warning: {err}")).yellow()
                    ))?;
                }
                term.write_line(&format!(
                    "{} {}",
                    CRAB,
                    style("Setup finished.").green().bold()
                ))?;
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
        (
            SetupAction::Modify,
            format!("{} Modify existing item", ARROW),
        ),
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
            .with_prompt(format!(
                "{} already configured. Reconfigure?",
                provider.as_str()
            ))
            .default(false)
            .interact()?;
        if !should_reconfigure {
            term.write_line("Provider unchanged.")?;
            return Ok(());
        }
    }

    let api_base_override = if provider.needs_custom_base_url() {
        let base: String = Input::with_theme(theme)
            .with_prompt(
                "Azure OpenAI endpoint URL (e.g. https://myresource.openai.azure.com/openai)",
            )
            .interact_text()?;
        Some(base)
    } else {
        None
    };

    let auth = prompt_auth_choice(theme, provider).await?;
    let path = write_provider_config_unchecked(
        config_root,
        provider,
        &auth,
        api_base_override.as_deref(),
    )?;
    print_done(
        term,
        &format!(
            "Provider configuration saved: {}",
            display_rel(config_root, &path)
        ),
    );
    Ok(())
}

fn handle_add_agent(
    config_root: &Path,
    theme: &ColorfulTheme,
    state: &ConfigState,
    force: bool,
) -> Result<()> {
    let agent_id: String = Input::with_theme(theme)
        .with_prompt("Agent ID")
        .default("clawhive-main".to_string())
        .interact_text()?;
    let agent_id = agent_id.trim().to_string();
    if agent_id.is_empty() {
        anyhow::bail!("agent id cannot be empty");
    }

    let existing = state.agents.iter().any(|a| a.agent_id == agent_id);
    if existing && !force {
        let reconfigure = Confirm::with_theme(theme)
            .with_prompt(format!("{agent_id} already configured. Reconfigure?"))
            .default(false)
            .interact()?;
        if !reconfigure {
            return Ok(());
        }
    }

    let name: String = Input::with_theme(theme)
        .with_prompt("Display name")
        .default("Clawhive".to_string())
        .interact_text()?;
    let emoji: String = Input::with_theme(theme)
        .with_prompt("Emoji")
        .default("🦀".to_string())
        .interact_text()?;

    let mut models = Vec::new();
    for p in &state.providers {
        for m in provider_models_for_id(&p.provider_id) {
            models.push(m);
        }
    }
    if models.is_empty() {
        models.push("sonnet".to_string());
    }
    models.push("Custom…".to_string());

    let model_labels: Vec<&str> = models.iter().map(String::as_str).collect();
    let selected = Select::with_theme(theme)
        .with_prompt("Primary model")
        .items(&model_labels)
        .default(0)
        .interact()?;

    let primary_model = if models[selected] == "Custom…" {
        Input::with_theme(theme)
            .with_prompt("Model ID (provider/model)")
            .interact_text()?
    } else {
        models[selected].clone()
    };

    write_agent_files_unchecked(config_root, &agent_id, &name, &emoji, &primary_model)?;
    print_done(&Term::stdout(), &format!("Agent {agent_id} configured."));
    Ok(())
}

fn handle_add_channel(
    config_root: &Path,
    theme: &ColorfulTheme,
    state: &ConfigState,
    _force: bool,
) -> Result<()> {
    let channel_types = ["Telegram", "Discord"];
    let selected = Select::with_theme(theme)
        .with_prompt("Channel type")
        .items(&channel_types)
        .default(0)
        .interact()?;
    let channel_type = match selected {
        0 => "telegram",
        _ => "discord",
    };
    let default_id = match channel_type {
        "telegram" => "tg-main",
        _ => "dc-main",
    };

    let connector_id: String = Input::with_theme(theme)
        .with_prompt("Connector ID")
        .default(default_id.to_string())
        .interact_text()?;

    let token = Password::with_theme(theme)
        .with_prompt("Bot token")
        .allow_empty_password(false)
        .interact()?;

    if !state.agents.is_empty() {
        let agent_labels: Vec<&str> = state.agents.iter().map(|a| a.agent_id.as_str()).collect();
        let agent_idx = Select::with_theme(theme)
            .with_prompt("Route messages to which agent?")
            .items(&agent_labels)
            .default(0)
            .interact()?;
        add_routing_binding(
            config_root,
            channel_type,
            &connector_id,
            &state.agents[agent_idx].agent_id,
        )?;
    } else {
        println!("  No agents configured yet. Routing will need to be set up later.");
    }

    let cfg = ChannelConfig {
        connector_id: connector_id.clone(),
        token,
    };
    add_channel_to_config(config_root, channel_type, &cfg)?;
    print_done(
        &Term::stdout(),
        &format!("Channel {connector_id} ({channel_type}) configured."),
    );
    Ok(())
}

async fn handle_modify(
    config_root: &Path,
    theme: &ColorfulTheme,
    state: &ConfigState,
    force: bool,
) -> Result<()> {
    let mut items: Vec<(String, &str)> = Vec::new();
    for provider in &state.providers {
        items.push((format!("{} (provider)", provider.provider_id), "provider"));
    }
    for agent in &state.agents {
        items.push((format!("{} (agent)", agent.agent_id), "agent"));
    }
    for channel in &state.channels {
        items.push((format!("{} (channel)", channel.connector_id), "channel"));
    }
    items.push(("← Back".to_string(), "back"));

    let labels: Vec<&str> = items.iter().map(|(label, _)| label.as_str()).collect();
    let selected = Select::with_theme(theme)
        .with_prompt("Which item to modify?")
        .items(&labels)
        .default(0)
        .interact()?;

    match items[selected].1 {
        "provider" => handle_add_provider(config_root, &Term::stdout(), theme, state, true).await?,
        "agent" => handle_add_agent(config_root, theme, state, true)?,
        "channel" => handle_add_channel(config_root, theme, state, force)?,
        _ => {}
    }
    Ok(())
}

fn handle_remove(
    config_root: &Path,
    theme: &ColorfulTheme,
    state: &ConfigState,
    force: bool,
) -> Result<()> {
    let mut items: Vec<(String, &str, String)> = Vec::new();
    for provider in &state.providers {
        items.push((
            format!("{} (provider)", provider.provider_id),
            "provider",
            provider.provider_id.clone(),
        ));
    }
    for agent in &state.agents {
        items.push((
            format!("{} (agent)", agent.agent_id),
            "agent",
            agent.agent_id.clone(),
        ));
    }
    for channel in &state.channels {
        items.push((
            format!("{} (channel)", channel.connector_id),
            "channel",
            channel.connector_id.clone(),
        ));
    }
    items.push(("← Back".to_string(), "back", String::new()));

    let labels: Vec<&str> = items.iter().map(|(label, _, _)| label.as_str()).collect();
    let selected = Select::with_theme(theme)
        .with_prompt("Which item to remove?")
        .items(&labels)
        .default(0)
        .interact()?;

    let (_, item_type, item_id) = &items[selected];
    match *item_type {
        "provider" => {
            if state.providers.len() <= 1 {
                println!("  Cannot remove last provider.");
                return Ok(());
            }
            if !force
                && !Confirm::with_theme(theme)
                    .with_prompt(format!("Remove provider {item_id}?"))
                    .default(false)
                    .interact()?
            {
                return Ok(());
            }

            let path = config_root.join(format!("config/providers.d/{item_id}.yaml"));
            if path.exists() {
                fs::remove_file(&path)?;
            }
            print_done(&Term::stdout(), &format!("Provider {item_id} removed."));
        }
        "agent" => {
            if state.agents.len() <= 1 {
                println!("  Cannot remove last agent.");
                return Ok(());
            }
            if !force
                && !Confirm::with_theme(theme)
                    .with_prompt(format!("Remove agent {item_id}?"))
                    .default(false)
                    .interact()?
            {
                return Ok(());
            }

            let path = config_root.join(format!("config/agents.d/{item_id}.yaml"));
            if path.exists() {
                fs::remove_file(&path)?;
            }
            print_done(&Term::stdout(), &format!("Agent {item_id} removed."));
        }
        "channel" => {
            if !force
                && !Confirm::with_theme(theme)
                    .with_prompt(format!("Remove channel {item_id}?"))
                    .default(false)
                    .interact()?
            {
                return Ok(());
            }

            remove_channel_from_config(config_root, item_id)?;
            remove_routing_binding(config_root, item_id)?;
            print_done(&Term::stdout(), &format!("Channel {item_id} removed."));
        }
        _ => {}
    }
    Ok(())
}

fn add_channel_to_config(
    config_root: &Path,
    channel_type: &str,
    cfg: &ChannelConfig,
) -> Result<()> {
    let main_path = config_root.join("config/main.yaml");
    if !main_path.exists() {
        let tg = if channel_type == "telegram" {
            Some(cfg.clone())
        } else {
            None
        };
        let dc = if channel_type == "discord" {
            Some(cfg.clone())
        } else {
            None
        };
        let yaml = generate_main_yaml("clawhive", tg, dc);
        fs::write(&main_path, yaml)?;
        return Ok(());
    }

    let content = fs::read_to_string(&main_path)?;
    let mut doc: serde_yaml::Value = serde_yaml::from_str(&content)?;
    let channels = doc
        .get_mut("channels")
        .and_then(|c| c.as_mapping_mut())
        .ok_or_else(|| anyhow!("main.yaml missing channels section"))?;

    let mut connector_map = serde_yaml::Mapping::new();
    connector_map.insert(
        serde_yaml::Value::String("connector_id".into()),
        serde_yaml::Value::String(cfg.connector_id.clone()),
    );
    connector_map.insert(
        serde_yaml::Value::String("token".into()),
        serde_yaml::Value::String(cfg.token.clone()),
    );
    let connector_value = serde_yaml::Value::Mapping(connector_map);

    let channel_key = serde_yaml::Value::String(channel_type.to_string());
    match channels.get_mut(&channel_key) {
        Some(section) => {
            section["enabled"] = serde_yaml::Value::Bool(true);
            if let Some(seq) = section
                .get_mut("connectors")
                .and_then(|connectors| connectors.as_sequence_mut())
            {
                seq.retain(|connector| {
                    connector.get("connector_id").and_then(|v| v.as_str())
                        != Some(&cfg.connector_id)
                });
                seq.push(connector_value);
            } else {
                section["connectors"] = serde_yaml::Value::Sequence(vec![connector_value]);
            }
        }
        None => {
            let mut section = serde_yaml::Mapping::new();
            section.insert("enabled".into(), serde_yaml::Value::Bool(true));
            section.insert(
                "connectors".into(),
                serde_yaml::Value::Sequence(vec![connector_value]),
            );
            channels.insert(channel_key, serde_yaml::Value::Mapping(section));
        }
    }

    fs::write(&main_path, serde_yaml::to_string(&doc)?)?;
    Ok(())
}

fn add_routing_binding(
    config_root: &Path,
    channel_type: &str,
    connector_id: &str,
    agent_id: &str,
) -> Result<()> {
    let routing_path = config_root.join("config/routing.yaml");
    if !routing_path.exists() {
        let yaml = generate_routing_yaml(agent_id, None, None);
        fs::write(&routing_path, yaml)?;
        return Ok(());
    }

    let content = fs::read_to_string(&routing_path)?;
    let mut doc: serde_yaml::Value = serde_yaml::from_str(&content)?;

    let mut match_map = serde_yaml::Mapping::new();
    match_map.insert("kind".into(), serde_yaml::Value::String("dm".into()));
    let mut binding_map = serde_yaml::Mapping::new();
    binding_map.insert(
        "channel_type".into(),
        serde_yaml::Value::String(channel_type.into()),
    );
    binding_map.insert(
        "connector_id".into(),
        serde_yaml::Value::String(connector_id.into()),
    );
    binding_map.insert("match".into(), serde_yaml::Value::Mapping(match_map));
    binding_map.insert(
        "agent_id".into(),
        serde_yaml::Value::String(agent_id.into()),
    );
    let new_binding = serde_yaml::Value::Mapping(binding_map);

    if let Some(seq) = doc
        .get_mut("bindings")
        .and_then(|bindings| bindings.as_sequence_mut())
    {
        seq.retain(|binding| {
            binding.get("connector_id").and_then(|v| v.as_str()) != Some(connector_id)
        });
        seq.push(new_binding);
    } else {
        doc["bindings"] = serde_yaml::Value::Sequence(vec![new_binding]);
    }

    fs::write(&routing_path, serde_yaml::to_string(&doc)?)?;
    Ok(())
}

fn remove_channel_from_config(config_root: &Path, connector_id: &str) -> Result<()> {
    let main_path = config_root.join("config/main.yaml");
    if !main_path.exists() {
        return Ok(());
    }

    let content = fs::read_to_string(&main_path)?;
    let mut doc: serde_yaml::Value = serde_yaml::from_str(&content)?;
    if let Some(channels) = doc
        .get_mut("channels")
        .and_then(|channels| channels.as_mapping_mut())
    {
        for (_channel, section) in channels.iter_mut() {
            if let Some(connectors) = section
                .get_mut("connectors")
                .and_then(|connectors| connectors.as_sequence_mut())
            {
                connectors.retain(|connector| {
                    connector
                        .get("connector_id")
                        .and_then(|value| value.as_str())
                        != Some(connector_id)
                });
            }
        }
    }

    fs::write(&main_path, serde_yaml::to_string(&doc)?)?;
    Ok(())
}

fn remove_routing_binding(config_root: &Path, connector_id: &str) -> Result<()> {
    let routing_path = config_root.join("config/routing.yaml");
    if !routing_path.exists() {
        return Ok(());
    }

    let content = fs::read_to_string(&routing_path)?;
    let mut doc: serde_yaml::Value = serde_yaml::from_str(&content)?;
    if let Some(bindings) = doc
        .get_mut("bindings")
        .and_then(|bindings| bindings.as_sequence_mut())
    {
        bindings.retain(|binding| {
            binding.get("connector_id").and_then(|value| value.as_str()) != Some(connector_id)
        });
    }

    fs::write(&routing_path, serde_yaml::to_string(&doc)?)?;
    Ok(())
}

fn display_rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

fn ensure_required_dirs(config_root: &Path) -> Result<()> {
    for rel in ["config/agents.d", "config/providers.d"] {
        let dir = config_root.join(rel);
        fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    }
    Ok(())
}

fn validate_generated_config(config_root: &Path) -> Result<()> {
    let config_path = config_root.join("config");
    clawhive_core::load_config(&config_path)
        .with_context(|| format!("config validation failed in {}", config_path.display()))?;
    Ok(())
}

fn prompt_provider(theme: &ColorfulTheme) -> Result<ProviderId> {
    let options: Vec<&str> = ALL_PROVIDERS.iter().map(|p| p.display_name()).collect();
    let selected = Select::with_theme(theme)
        .with_prompt("Choose your LLM provider")
        .items(&options)
        .default(0)
        .interact()?;

    ALL_PROVIDERS
        .get(selected)
        .copied()
        .ok_or_else(|| anyhow!("invalid provider selection index: {selected}"))
}

async fn prompt_auth_choice(theme: &ColorfulTheme, provider: ProviderId) -> Result<AuthChoice> {
    if provider.supports_oauth() {
        let methods: Vec<&str> = match provider {
            ProviderId::Anthropic => vec![
                "Setup Token (run `claude setup-token` in terminal)",
                "API Key (from console.anthropic.com/settings/keys)",
            ],
            ProviderId::OpenAi => vec![
                "OAuth Login (use your ChatGPT subscription)",
                "API Key (from platform.openai.com/api-keys)",
            ],
            _ => unreachable!(),
        };
        let method = Select::with_theme(theme)
            .with_prompt("Authentication method")
            .items(&methods)
            .default(0)
            .interact()?;

        match method {
            0 => run_oauth_auth(provider).await,
            1 => prompt_api_key(theme, provider),
            _ => Err(anyhow!("invalid auth method index: {method}")),
        }
    } else if provider == ProviderId::Ollama {
        // Ollama runs locally, no auth needed
        Ok(AuthChoice::ApiKey {
            api_key: String::new(),
        })
    } else {
        prompt_api_key(theme, provider)
    }
}

fn prompt_api_key(theme: &ColorfulTheme, provider: ProviderId) -> Result<AuthChoice> {
    let api_key = Password::with_theme(theme)
        .with_prompt(format!("Paste {} API key", provider.display_name()))
        .allow_empty_password(false)
        .interact()?;
    if api_key.trim().is_empty() {
        anyhow::bail!("API key cannot be empty");
    }
    Ok(AuthChoice::ApiKey {
        api_key: api_key.trim().to_string(),
    })
}

async fn run_oauth_auth(provider: ProviderId) -> Result<AuthChoice> {
    let manager = TokenManager::new()?;
    let profile_name = format!("{}-{}", provider.as_str(), unix_timestamp()?);

    match provider {
        ProviderId::OpenAi => {
            let term = Term::stdout();
            let _ = term.write_line("");
            let _ = term.write_line("  Opening browser for OpenAI OAuth login...");
            let _ = term.write_line("  Complete the login in your browser.");
            let _ = term.write_line("  Waiting for callback (timeout: 5 minutes)...");
            let _ = term.write_line("");
            let client_id = "app_EMoamEEZ73f0CkXaXp7hrann";
            let config = OpenAiOAuthConfig::default_with_client(client_id);
            let http = reqwest::Client::new();
            let token = run_openai_pkce_flow(&http, &config).await?;
            let account_id = extract_chatgpt_account_id(&token.access_token);
            if let Some(ref id) = account_id {
                eprintln!("  ✓ ChatGPT account: {id}");
            } else {
                eprintln!("  ⚠ Could not extract chatgpt_account_id from token");
            }
            manager.save_profile(
                &profile_name,
                AuthProfile::OpenAiOAuth {
                    access_token: token.access_token,
                    refresh_token: token.refresh_token,
                    expires_at: unix_timestamp()? + token.expires_in,
                    chatgpt_account_id: account_id,
                },
            )?;
        }
        ProviderId::Anthropic => {
            let term = Term::stdout();
            let _ = term.write_line("");
            let _ = term
                .write_line("  To use Anthropic with your subscription, you need a setup-token.");
            let _ = term.write_line("  If you have Claude Code CLI installed, run:");
            let _ = term.write_line("");
            let _ = term.write_line("    claude setup-token");
            let _ = term.write_line("");
            let _ = term.write_line("  Then paste the token below.");
            let _ = term.write_line("");
            let token: String = Input::new()
                .with_prompt("Paste your Anthropic setup-token")
                .interact_text()
                .context("failed to read Anthropic setup-token")?;
            let http = reqwest::Client::new();
            let ok = validate_setup_token(&http, &token, "https://api.anthropic.com").await?;
            if !ok {
                anyhow::bail!(
                    "Anthropic setup-token validation failed. Check the log above for details."
                );
            }
            manager.save_profile(&profile_name, profile_from_setup_token(token))?;
        }
        _ => {
            anyhow::bail!("OAuth is not supported for {}", provider.display_name());
        }
    }

    Ok(AuthChoice::OAuth { profile_name })
}

fn write_provider_config_unchecked(
    config_root: &Path,
    provider: ProviderId,
    auth: &AuthChoice,
    api_base_override: Option<&str>,
) -> Result<PathBuf> {
    let providers_dir = config_root.join("config/providers.d");
    fs::create_dir_all(&providers_dir)
        .with_context(|| format!("failed to create {}", providers_dir.display()))?;

    let target = providers_dir.join(format!("{}.yaml", provider.as_str()));
    let yaml = generate_provider_yaml(provider, auth, api_base_override);
    fs::write(&target, yaml).with_context(|| format!("failed to write {}", target.display()))?;
    Ok(target)
}

fn write_agent_files_unchecked(
    config_root: &Path,
    agent_id: &str,
    name: &str,
    emoji: &str,
    primary_model: &str,
) -> Result<()> {
    let agents_dir = config_root.join("config/agents.d");
    fs::create_dir_all(&agents_dir)?;
    let yaml = generate_agent_yaml(agent_id, name, emoji, primary_model);
    fs::write(agents_dir.join(format!("{agent_id}.yaml")), yaml)?;

    let prompt_dir = config_root.join("prompts").join(agent_id);
    fs::create_dir_all(&prompt_dir)?;
    let prompt_path = prompt_dir.join("system.md");
    if !prompt_path.exists() {
        fs::write(&prompt_path, default_system_prompt(name))?;
    }
    Ok(())
}

fn generate_provider_yaml(
    provider: ProviderId,
    auth: &AuthChoice,
    api_base_override: Option<&str>,
) -> String {
    let base_url = api_base_override.unwrap_or(provider.api_base());
    match auth {
        AuthChoice::OAuth { profile_name } => {
            let base = match provider {
                ProviderId::OpenAi => "https://chatgpt.com/backend-api/codex",
                _ => base_url,
            };
            format!(
                "provider_id: {provider}\nenabled: true\napi_base: {base}\nauth_profile: \"{profile}\"\nmodels:\n  - {model}\n",
                provider = provider.as_str(),
                base = base,
                profile = profile_name,
                model = provider.default_model(),
            )
        }
        AuthChoice::ApiKey { api_key } => {
            if api_key.is_empty() {
                // Ollama or other local providers without auth
                format!(
                    "provider_id: {provider}\nenabled: true\napi_base: {base}\nmodels:\n  - {model}\n",
                    provider = provider.as_str(),
                    base = base_url,
                    model = provider.default_model(),
                )
            } else {
                format!(
                    "provider_id: {provider}\nenabled: true\napi_base: {base}\napi_key: \"{key}\"\nmodels:\n  - {model}\n",
                    provider = provider.as_str(),
                    base = base_url,
                    key = api_key,
                    model = provider.default_model(),
                )
            }
        }
    }
}

fn generate_agent_yaml(agent_id: &str, name: &str, emoji: &str, primary_model: &str) -> String {
    format!(
        "agent_id: {agent_id}\nenabled: true\nidentity:\n  name: \"{name}\"\n  emoji: \"{emoji}\"\nmodel_policy:\n  primary: \"{primary_model}\"\n  fallbacks: []\nmemory_policy:\n  mode: \"standard\"\n  write_scope: \"all\"\n"
    )
}

fn default_system_prompt(agent_name: &str) -> String {
    format!(
        "You are {agent_name}, a helpful AI assistant powered by clawhive.\n\nYou are knowledgeable, concise, and friendly. When you don't know something, you say so honestly.\n"
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
        "\nembedding:\n  enabled: false\n  provider: stub\n  api_key: \"\"\n  model: text-embedding-3-small\n  dimensions: 1536\n  base_url: https://api.openai.com/v1\n\ntools: {}\n",
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
    let prefix = provider.as_str();
    match provider {
        ProviderId::Anthropic => vec![
            format!("{prefix}/claude-sonnet-4-5"),
            format!("{prefix}/claude-3-haiku-20240307"),
        ],
        ProviderId::OpenAi => vec![format!("{prefix}/gpt-4o-mini"), format!("{prefix}/gpt-4o")],
        ProviderId::Gemini => vec![
            format!("{prefix}/gemini-2.0-flash"),
            format!("{prefix}/gemini-2.0-pro"),
        ],
        ProviderId::DeepSeek => vec![
            format!("{prefix}/deepseek-chat"),
            format!("{prefix}/deepseek-reasoner"),
        ],
        ProviderId::Groq => vec![
            format!("{prefix}/llama-3.3-70b-versatile"),
            format!("{prefix}/mixtral-8x7b-32768"),
        ],
        ProviderId::Ollama => vec![format!("{prefix}/llama3.2"), format!("{prefix}/mistral")],
        ProviderId::OpenRouter => vec![
            format!("{prefix}/anthropic/claude-sonnet-4-5"),
            format!("{prefix}/openai/gpt-4o"),
        ],
        ProviderId::Together => vec![
            format!("{prefix}/meta-llama/Llama-3.3-70B-Instruct-Turbo"),
            format!("{prefix}/mistralai/Mixtral-8x7B-Instruct-v0.1"),
        ],
        ProviderId::Fireworks => vec![
            format!("{prefix}/accounts/fireworks/models/llama-v3p3-70b-instruct"),
            format!("{prefix}/accounts/fireworks/models/mixtral-8x7b-instruct"),
        ],
        ProviderId::AzureOpenAi => {
            vec![format!("{prefix}/gpt-4o-mini"), format!("{prefix}/gpt-4o")]
        }
    }
}

fn provider_models_for_id(provider_id: &str) -> Vec<String> {
    match ProviderId::from_str(provider_id) {
        Some(p) => provider_models(p),
        None => vec![],
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
        add_channel_to_config, build_action_labels, default_system_prompt, ensure_required_dirs,
        generate_agent_yaml, generate_main_yaml, generate_provider_yaml, generate_routing_yaml,
        provider_models, provider_models_for_id, remove_channel_from_config,
        remove_routing_binding, validate_generated_config, write_agent_files_unchecked,
        write_provider_config_unchecked, AuthChoice, ChannelConfig, ProviderId, SetupAction,
        ALL_PROVIDERS,
    };
    use crate::setup_scan::ConfigState;

    #[test]
    fn provider_yaml_uses_auth_profile_for_oauth() {
        let yaml = generate_provider_yaml(
            ProviderId::OpenAi,
            &AuthChoice::OAuth {
                profile_name: "openai-oauth".to_string(),
            },
            None,
        );

        assert!(yaml.contains("provider_id: openai"));
        assert!(yaml.contains("auth_profile: \"openai-oauth\""));
        assert!(yaml.contains("api_base: https://chatgpt.com/backend-api/codex"));
        assert!(!yaml.contains("api_key:"));
    }

    #[test]
    fn provider_yaml_uses_api_key_for_api_key_auth() {
        let yaml = generate_provider_yaml(
            ProviderId::Anthropic,
            &AuthChoice::ApiKey {
                api_key: "sk-test-key".to_string(),
            },
            None,
        );

        assert!(yaml.contains("provider_id: anthropic"));
        assert!(yaml.contains("api_key: \"sk-test-key\""));
        assert!(!yaml.contains("auth_profile:"));
    }

    #[test]
    fn agent_yaml_contains_identity_and_model_policy() {
        let yaml = generate_agent_yaml("clawhive-main", "Clawhive", "🦀", "openai/gpt-4o-mini");

        assert!(yaml.contains("agent_id: clawhive-main"));
        assert!(yaml.contains("name: \"Clawhive\""));
        assert!(yaml.contains("emoji: \"🦀\""));
        assert!(yaml.contains("primary: \"openai/gpt-4o-mini\""));
    }

    #[test]
    fn default_system_prompt_contains_agent_name() {
        let prompt = default_system_prompt("Clawhive");
        assert!(prompt.contains("You are Clawhive"));
        assert!(prompt.contains("helpful AI assistant"));
    }

    #[test]
    fn provider_model_aliases_are_fully_qualified() {
        for provider in ALL_PROVIDERS {
            let models = provider_models(*provider);
            let prefix = provider.as_str();
            assert!(
                models.iter().all(|m| m.starts_with(&format!("{prefix}/"))),
                "all models for {} should start with {prefix}/",
                provider.display_name()
            );
        }
    }

    #[test]
    fn provider_models_for_id_returns_known_provider_models() {
        for provider in ALL_PROVIDERS {
            let models = provider_models_for_id(provider.as_str());
            assert!(
                !models.is_empty(),
                "provider_models_for_id({}) should return models",
                provider.as_str()
            );
        }
        let unknown = provider_models_for_id("nonexistent");
        assert!(unknown.is_empty());
    }

    #[test]
    fn main_yaml_writes_plaintext_channel_tokens() {
        let yaml = generate_main_yaml(
            "clawhive",
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
    fn add_channel_to_existing_main_yaml_preserves_other_channels() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("config")).unwrap();
        let initial = generate_main_yaml(
            "clawhive",
            Some(ChannelConfig {
                connector_id: "tg-main".into(),
                token: "tok1".into(),
            }),
            None,
        );
        std::fs::write(temp.path().join("config/main.yaml"), &initial).unwrap();
        add_channel_to_config(
            temp.path(),
            "discord",
            &ChannelConfig {
                connector_id: "dc-main".into(),
                token: "tok2".into(),
            },
        )
        .unwrap();
        let content = std::fs::read_to_string(temp.path().join("config/main.yaml")).unwrap();
        assert!(content.contains("tg-main"));
        assert!(content.contains("dc-main"));
    }

    #[test]
    fn remove_channel_from_main_yaml_preserves_other_connectors() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("config")).unwrap();
        let initial = generate_main_yaml(
            "clawhive",
            Some(ChannelConfig {
                connector_id: "tg-main".into(),
                token: "tok1".into(),
            }),
            Some(ChannelConfig {
                connector_id: "dc-main".into(),
                token: "tok2".into(),
            }),
        );
        std::fs::write(temp.path().join("config/main.yaml"), &initial).unwrap();

        remove_channel_from_config(temp.path(), "dc-main").unwrap();
        let content = std::fs::read_to_string(temp.path().join("config/main.yaml")).unwrap();
        assert!(content.contains("tg-main"));
        assert!(!content.contains("dc-main"));
    }

    #[test]
    fn remove_routing_binding_preserves_other_bindings() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("config")).unwrap();
        let initial = generate_routing_yaml(
            "clawhive-main",
            Some(ChannelConfig {
                connector_id: "tg-main".into(),
                token: "tok1".into(),
            }),
            Some(ChannelConfig {
                connector_id: "dc-main".into(),
                token: "tok2".into(),
            }),
        );
        std::fs::write(temp.path().join("config/routing.yaml"), &initial).unwrap();

        remove_routing_binding(temp.path(), "dc-main").unwrap();
        let content = std::fs::read_to_string(temp.path().join("config/routing.yaml")).unwrap();
        assert!(content.contains("tg-main"));
        assert!(!content.contains("dc-main"));
    }

    #[test]
    fn routing_yaml_contains_bindings_for_enabled_channels() {
        let yaml = generate_routing_yaml(
            "clawhive-main",
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
        assert!(yaml.contains("agent_id: clawhive-main"));
    }

    #[test]
    fn ensure_required_dirs_creates_expected_paths() {
        let temp = tempfile::tempdir().expect("create tempdir");
        ensure_required_dirs(temp.path()).expect("create required directories");

        for rel in ["config/agents.d", "config/providers.d"] {
            assert!(temp.path().join(rel).exists(), "missing {rel}");
        }
    }

    #[test]
    fn validate_generated_config_accepts_minimal_valid_files() {
        let temp = tempfile::tempdir().expect("create tempdir");
        ensure_required_dirs(temp.path()).expect("create required directories");

        std::fs::write(
            temp.path().join("config/main.yaml"),
            generate_main_yaml("clawhive", None, None),
        )
        .expect("write main.yaml");
        std::fs::write(
            temp.path().join("config/routing.yaml"),
            generate_routing_yaml("clawhive-main", None, None),
        )
        .expect("write routing.yaml");
        std::fs::write(
            temp.path().join("config/providers.d/openai.yaml"),
            generate_provider_yaml(
                ProviderId::OpenAi,
                &AuthChoice::ApiKey {
                    api_key: "sk-test".to_string(),
                },
                None,
            ),
        )
        .expect("write provider yaml");
        std::fs::write(
            temp.path().join("config/agents.d/clawhive-main.yaml"),
            generate_agent_yaml("clawhive-main", "Clawhive", "🦀", "openai/gpt-4o-mini"),
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
                api_key: "sk-test".to_string(),
            },
            None,
        )
        .expect("write provider config");

        let updated = std::fs::read_to_string(&target).expect("read updated provider file");
        assert!(updated.contains("provider_id: openai"));
        assert!(!updated.contains("old: value"));
    }

    #[test]
    fn write_agent_files_unchecked_overwrites_yaml_and_keeps_existing_prompt() {
        let temp = tempfile::tempdir().expect("create tempdir");
        ensure_required_dirs(temp.path()).expect("create required directories");

        let yaml_path = temp.path().join("config/agents.d/clawhive-main.yaml");
        std::fs::write(&yaml_path, "old: value\n").expect("write old agent yaml");

        let prompt_dir = temp.path().join("prompts/clawhive-main");
        std::fs::create_dir_all(&prompt_dir).expect("create prompt dir");
        let prompt_path = prompt_dir.join("system.md");
        std::fs::write(&prompt_path, "keep this prompt").expect("write existing prompt");

        write_agent_files_unchecked(
            temp.path(),
            "clawhive-main",
            "Clawhive",
            "🦀",
            "openai/gpt-4o-mini",
        )
        .expect("write agent files");

        let yaml = std::fs::read_to_string(&yaml_path).expect("read yaml");
        let prompt = std::fs::read_to_string(&prompt_path).expect("read prompt");

        assert!(yaml.contains("agent_id: clawhive-main"));
        assert!(!yaml.contains("old: value"));
        assert_eq!(prompt, "keep this prompt");
    }
}
