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
use dialoguer::{theme::ColorfulTheme, Confirm, Input, Select};

use clawhive_core::config::{
    ActionbookConfig, DiscordChannelConfig, DiscordConnectorConfig, MainConfig,
    TelegramChannelConfig, TelegramConnectorConfig, WebSearchConfig,
};

use crate::setup_scan::{scan_config, ConfigState};
use crate::setup_ui::{print_done, print_logo, render_dashboard, ARROW, CRAB};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetupAction {
    AddProvider,
    AddAgent,
    AddChannel,
    ConfigureTools,
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
        clawhive_schema::provider_presets::preset_by_id(self.as_str())
            .map(|p| p.name)
            .unwrap_or(self.as_str())
    }

    fn default_model(self) -> &'static str {
        clawhive_schema::provider_presets::preset_by_id(self.as_str())
            .map(|p| p.default_model)
            .unwrap_or("unknown")
    }

    fn api_base(self) -> &'static str {
        clawhive_schema::provider_presets::preset_by_id(self.as_str())
            .map(|p| p.api_base)
            .unwrap_or("")
    }

    fn supports_oauth(self) -> bool {
        // Anthropic subscription (setup-token) is no longer supported in the wizard.
        // The code path still exists in run_oauth_auth() for future use.
        matches!(self, Self::OpenAi)
    }

    fn needs_custom_base_url(self) -> bool {
        matches!(self, Self::AzureOpenAi)
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
    groups: Vec<String>,
    require_mention: bool,
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
            SetupAction::ConfigureTools => {
                handle_configure_tools(config_root, &theme)?;
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

                // Offer to set web console password (skippable)
                handle_set_web_password(config_root, &theme, &term)?;

                term.write_line(&format!(
                    "{} {}",
                    CRAB,
                    style("Setup finished.").green().bold()
                ))?;
                term.write_line("")?;
                term.write_line(&format!(
                    "  {} Run {} to apply changes.",
                    ARROW,
                    style("clawhive restart").cyan().bold()
                ))?;
                break;
            }
        }
    }

    Ok(())
}

fn handle_set_web_password(config_root: &Path, theme: &ColorfulTheme, term: &Term) -> Result<()> {
    // Check if password is already configured
    let main_yaml_path = config_root.join("config/main.yaml");
    let already_set = fs::read_to_string(&main_yaml_path)
        .ok()
        .and_then(|content| serde_yaml::from_str::<serde_yaml::Value>(&content).ok())
        .and_then(|val| val["web_password_hash"].as_str().map(|_| ()))
        .is_some();

    if already_set {
        return Ok(());
    }

    term.write_line("")?;
    term.write_line(&format!(
        "  {} {}",
        ARROW,
        style("Web Console Password").bold()
    ))?;
    term.write_line("    Set a password to protect your web console.")?;
    term.write_line("    Without a password, anyone with network access can control your agents.")?;
    term.write_line("")?;

    let set_now = Confirm::with_theme(theme)
        .with_prompt("Set a web console password now? (recommended)")
        .default(true)
        .interact()?;

    if !set_now {
        term.write_line(&format!(
            "    {} {}",
            ARROW,
            style("Skipped. You can set a password later via the web console.").dim()
        ))?;
        return Ok(());
    }

    loop {
        let password: String = Input::with_theme(theme)
            .with_prompt("  Password (min 6 chars)")
            .validate_with(|input: &String| {
                if input.trim().len() >= 6 {
                    Ok(())
                } else {
                    Err("Password must be at least 6 characters")
                }
            })
            .interact_text()?;

        let confirm: String = Input::with_theme(theme)
            .with_prompt("  Confirm password")
            .interact_text()?;

        if password != confirm {
            term.write_line(&format!(
                "    {} {}",
                ARROW,
                style("Passwords do not match. Try again.").red()
            ))?;
            continue;
        }

        let hash = bcrypt::hash(&password, bcrypt::DEFAULT_COST)
            .map_err(|e| anyhow!("Failed to hash password: {e}"))?;

        // Write hash to main.yaml
        let mut cfg = load_main_config(config_root)?;
        cfg.web_password_hash = Some(hash);
        save_main_config(config_root, &cfg)?;

        term.write_line(&format!(
            "    {} {}",
            ARROW,
            style("Password set successfully.").green()
        ))?;
        break;
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
            SetupAction::ConfigureTools,
            format!(
                "{} Configure Tools (web_search: {}, browser: {})",
                ARROW,
                if state.tools.web_search_enabled {
                    "on"
                } else {
                    "off"
                },
                if state.tools.actionbook_enabled {
                    "on"
                } else {
                    "off"
                }
            ),
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
    let provider = match prompt_provider(theme)? {
        Some(p) => p,
        None => return Ok(()),
    };

    // For OpenAI we allow separate API-key ("openai") and OAuth ("openai-chatgpt")
    // configs to coexist, so only block when both are already present.
    let fully_configured = if provider == ProviderId::OpenAi {
        let has_key = state.providers.iter().any(|i| i.provider_id == "openai");
        let has_oauth = state
            .providers
            .iter()
            .any(|i| i.provider_id == "openai-chatgpt");
        has_key && has_oauth
    } else {
        state
            .providers
            .iter()
            .any(|item| item.provider_id == provider.as_str())
    };
    if fully_configured && !force {
        let actions = ["Reconfigure", "Remove", "Cancel"];
        let selected = Select::with_theme(theme)
            .with_prompt(format!("{} already configured", provider.as_str()))
            .items(&actions)
            .default(2)
            .interact()?;
        match selected {
            0 => { /* continue to reconfigure below */ }
            1 => {
                if Confirm::with_theme(theme)
                    .with_prompt(format!(
                        "Are you sure you want to remove {}?",
                        provider.as_str()
                    ))
                    .default(false)
                    .interact()?
                {
                    let path =
                        config_root.join(format!("config/providers.d/{}.yaml", provider.as_str()));
                    if path.exists() {
                        fs::remove_file(&path)?;
                    }
                    print_done(term, &format!("Provider {} removed.", provider.as_str()));
                }
                return Ok(());
            }
            _ => {
                return Ok(());
            }
        }
    }

    let api_base_override = if provider.needs_custom_base_url() {
        let base = match input_or_back(
            theme,
            "Azure OpenAI endpoint URL (e.g. https://myresource.openai.azure.com/openai/v1)",
        )? {
            Some(b) => b,
            None => return Ok(()),
        };
        Some(base)
    } else if provider == ProviderId::Ollama {
        let base = match input_or_back_with_default(theme, "Ollama API URL", provider.api_base())? {
            Some(b) => b,
            None => return Ok(()),
        };
        if base == provider.api_base() {
            None
        } else {
            Some(base)
        }
    } else {
        None
    };

    let auth = match prompt_auth_choice(theme, provider).await? {
        Some(a) => a,
        None => return Ok(()),
    };
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
    let agent_id = match input_or_back_with_default(theme, "Agent ID", "clawhive-main")? {
        Some(id) if !id.is_empty() => id,
        Some(_) => anyhow::bail!("agent id cannot be empty"),
        None => return Ok(()),
    };

    let existing = state.agents.iter().any(|a| a.agent_id == agent_id);
    if existing && !force {
        let actions = ["Reconfigure", "Remove", "Cancel"];
        let selected = Select::with_theme(theme)
            .with_prompt(format!("{agent_id} already configured"))
            .items(&actions)
            .default(2)
            .interact()?;
        match selected {
            0 => { /* continue to reconfigure below */ }
            1 => {
                if Confirm::with_theme(theme)
                    .with_prompt(format!("Are you sure you want to remove {agent_id}?"))
                    .default(false)
                    .interact()?
                {
                    let path = config_root.join(format!("config/agents.d/{agent_id}.yaml"));
                    if path.exists() {
                        fs::remove_file(&path)?;
                    }
                    print_done(&Term::stdout(), &format!("Agent {agent_id} removed."));
                }
                return Ok(());
            }
            _ => {
                return Ok(());
            }
        }
    }

    let name = match input_or_back_with_default(theme, "Display name", "Clawhive")? {
        Some(n) => n,
        None => return Ok(()),
    };
    let emoji = match input_or_back_with_default(theme, "Emoji", "🦀")? {
        Some(e) => e,
        None => return Ok(()),
    };

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
        match input_or_back(theme, "Model ID (provider/model)")? {
            Some(m) => m,
            None => return Ok(()),
        }
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
    let channel_types = ["Telegram", "Discord", "← Back"];
    let selected = Select::with_theme(theme)
        .with_prompt("Channel type")
        .items(&channel_types)
        .default(0)
        .interact()?;
    let channel_type = match selected {
        0 => "telegram",
        1 => "discord",
        _ => return Ok(()),
    };
    let default_id = match channel_type {
        "telegram" => "my_telegram_bot",
        _ => "my_discord_bot",
    };

    let connector_id = match input_or_back_with_default(
        theme,
        "Bot name (a unique name to identify this bot)",
        default_id,
    )? {
        Some(id) => id,
        None => return Ok(()),
    };

    let token = match input_or_back(theme, "Bot token")? {
        Some(t) if !t.is_empty() => t,
        Some(_) => anyhow::bail!("Bot token cannot be empty"),
        None => return Ok(()),
    };
    let masked = mask_secret(&token);
    println!("  {ARROW} Token saved: {masked}");

    // Message routing kind selection
    let kind_options = ["DM only", "Group only", "DM + Group", "← Back"];
    let kind_idx = Select::with_theme(theme)
        .with_prompt("Message routing")
        .items(&kind_options)
        .default(0)
        .interact()?;
    let kinds: Vec<&str> = match kind_idx {
        0 => vec!["dm"],
        1 => vec!["group"],
        2 => vec!["dm", "group"],
        _ => return Ok(()),
    };
    let has_group = kinds.contains(&"group");

    // Groups + require_mention: only when group routing is selected
    let (groups, require_mention) = if has_group {
        let groups = if channel_type == "discord" {
            let groups_input = match input_or_back_with_default(
                theme,
                "Groups (comma-separated Discord channel IDs, leave empty for all)",
                "",
            )? {
                Some(g) => g,
                None => return Ok(()),
            };
            groups_input
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        } else {
            Vec::new()
        };
        let require_mention = Confirm::with_theme(theme)
            .with_prompt("Require @mention in groups?")
            .default(true)
            .interact()?;
        (groups, require_mention)
    } else {
        (Vec::new(), true)
    };

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
            &kinds,
        )?;
    } else {
        println!("  No agents configured yet. Routing will need to be set up later.");
    }

    let cfg = ChannelConfig {
        connector_id: connector_id.clone(),
        token,
        groups,
        require_mention,
    };
    add_channel_to_config(config_root, channel_type, &cfg)?;
    print_done(
        &Term::stdout(),
        &format!("Channel {connector_id} ({channel_type}) configured."),
    );
    Ok(())
}

fn handle_configure_tools(config_root: &Path, theme: &ColorfulTheme) -> Result<()> {
    let term = Term::stdout();

    let enable_ws = Confirm::with_theme(theme)
        .with_prompt("Enable web search?")
        .default(true)
        .interact()?;

    let (provider, api_key) = if enable_ws {
        let ws_providers = ["Brave", "Tavily", "Serper", "← Back"];
        let selected = Select::with_theme(theme)
            .with_prompt("Web search provider")
            .items(&ws_providers)
            .default(0)
            .interact()?;
        if selected == ws_providers.len() - 1 {
            return Ok(());
        }
        let provider = ws_providers[selected].to_lowercase();
        let key = match input_or_back(theme, &format!("{} API key", ws_providers[selected]))? {
            Some(k) if !k.is_empty() => k,
            Some(_) => anyhow::bail!("API key cannot be empty"),
            None => return Ok(()),
        };
        (Some(provider), Some(key))
    } else {
        (None, None)
    };

    let mut cfg = load_main_config(config_root)?;
    cfg.tools.web_search = Some(WebSearchConfig {
        enabled: enable_ws,
        provider: provider.clone(),
        api_key: api_key.clone(),
    });
    save_main_config(config_root, &cfg)?;

    let ab_installed = clawhive_core::bin_exists("actionbook");
    let ab_prompt = if ab_installed {
        "Enable browser automation? (actionbook is installed)"
    } else {
        "Enable browser automation? (actionbook NOT installed)"
    };

    let enable_ab = Confirm::with_theme(theme)
        .with_prompt(ab_prompt)
        .default(ab_installed)
        .interact()?;

    if enable_ab && !ab_installed {
        term.write_line("")?;
        term.write_line(&format!(
            "  {} Actionbook CLI is required for browser automation.",
            ARROW
        ))?;
        term.write_line("  Install with one of:")?;
        term.write_line("")?;
        term.write_line("    curl -fsSL https://actionbook.dev/install.sh | bash")?;
        term.write_line("    npm install -g @actionbookdev/cli")?;
        term.write_line("")?;

        let install_now = Confirm::with_theme(theme)
            .with_prompt("Install actionbook now? (uses curl)")
            .default(true)
            .interact()?;

        if install_now {
            term.write_line("  Installing actionbook...")?;
            let status = std::process::Command::new("sh")
                .arg("-c")
                .arg("curl -fsSL https://actionbook.dev/install.sh | bash")
                .status();
            match status {
                Ok(s) if s.success() => {
                    print_done(&term, "actionbook installed successfully.");
                }
                _ => {
                    term.write_line(&format!(
                        "  {} Installation failed. Install manually later.",
                        ARROW
                    ))?;
                }
            }
        }
    }

    let mut cfg = load_main_config(config_root)?;
    cfg.tools.actionbook = Some(ActionbookConfig { enabled: enable_ab });
    save_main_config(config_root, &cfg)?;
    print_done(
        &Term::stdout(),
        &format!(
            "Tools configured. web_search: {}",
            if enable_ws { "on" } else { "off" }
        ),
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
    let mut main_cfg = load_main_config(config_root)?;

    match channel_type {
        "telegram" => {
            let connector = TelegramConnectorConfig {
                connector_id: cfg.connector_id.clone(),
                token: cfg.token.clone(),
                require_mention: cfg.require_mention,
            };
            match main_cfg.channels.telegram.as_mut() {
                Some(tg) => {
                    tg.enabled = true;
                    tg.connectors.retain(|c| c.connector_id != cfg.connector_id);
                    tg.connectors.push(connector);
                }
                None => {
                    main_cfg.channels.telegram = Some(TelegramChannelConfig {
                        enabled: true,
                        connectors: vec![connector],
                    });
                }
            }
        }
        "discord" => {
            let connector = DiscordConnectorConfig {
                connector_id: cfg.connector_id.clone(),
                token: cfg.token.clone(),
                groups: cfg.groups.clone(),
                require_mention: cfg.require_mention,
            };
            match main_cfg.channels.discord.as_mut() {
                Some(dc) => {
                    dc.enabled = true;
                    dc.connectors.retain(|c| c.connector_id != cfg.connector_id);
                    dc.connectors.push(connector);
                }
                None => {
                    main_cfg.channels.discord = Some(DiscordChannelConfig {
                        enabled: true,
                        connectors: vec![connector],
                    });
                }
            }
        }
        _ => return Err(anyhow!("unsupported channel type: {channel_type}")),
    }

    save_main_config(config_root, &main_cfg)?;
    Ok(())
}

fn add_routing_binding(
    config_root: &Path,
    channel_type: &str,
    connector_id: &str,
    agent_id: &str,
    kinds: &[&str],
) -> Result<()> {
    let routing_path = config_root.join("config/routing.yaml");
    if !routing_path.exists() {
        let yaml = generate_routing_yaml(agent_id, channel_type, connector_id, kinds);
        fs::write(&routing_path, yaml)?;
        return Ok(());
    }

    let content = fs::read_to_string(&routing_path)?;
    let mut doc: serde_yaml::Value = serde_yaml::from_str(&content)?;

    // Build one binding per kind
    let new_bindings: Vec<serde_yaml::Value> = kinds
        .iter()
        .map(|kind| {
            let mut match_map = serde_yaml::Mapping::new();
            match_map.insert("kind".into(), serde_yaml::Value::String((*kind).into()));
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
            serde_yaml::Value::Mapping(binding_map)
        })
        .collect();

    if let Some(seq) = doc
        .get_mut("bindings")
        .and_then(|bindings| bindings.as_sequence_mut())
    {
        // Remove all old bindings for this connector_id
        seq.retain(|binding| {
            binding.get("connector_id").and_then(|v| v.as_str()) != Some(connector_id)
        });
        seq.extend(new_bindings);
    } else {
        doc["bindings"] = serde_yaml::Value::Sequence(new_bindings);
    }

    fs::write(&routing_path, serde_yaml::to_string(&doc)?)?;
    Ok(())
}

fn remove_channel_from_config(config_root: &Path, connector_id: &str) -> Result<()> {
    let main_path = config_root.join("config/main.yaml");
    if !main_path.exists() {
        return Ok(());
    }

    let mut cfg = load_main_config(config_root)?;
    if let Some(tg) = cfg.channels.telegram.as_mut() {
        tg.connectors.retain(|c| c.connector_id != connector_id);
    }
    if let Some(dc) = cfg.channels.discord.as_mut() {
        dc.connectors.retain(|c| c.connector_id != connector_id);
    }
    save_main_config(config_root, &cfg)?;
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

fn prompt_provider(theme: &ColorfulTheme) -> Result<Option<ProviderId>> {
    let mut options: Vec<&str> = ALL_PROVIDERS.iter().map(|p| p.display_name()).collect();
    options.push("← Back");
    let selected = Select::with_theme(theme)
        .with_prompt("Choose your LLM provider")
        .items(&options)
        .default(0)
        .interact()?;

    if selected >= ALL_PROVIDERS.len() {
        return Ok(None);
    }
    Ok(Some(ALL_PROVIDERS[selected]))
}

async fn prompt_auth_choice(
    theme: &ColorfulTheme,
    provider: ProviderId,
) -> Result<Option<AuthChoice>> {
    if provider.supports_oauth() {
        let methods: Vec<&str> = match provider {
            ProviderId::Anthropic => vec![
                "Setup Token (run `claude setup-token` in terminal)",
                "API Key (from console.anthropic.com/settings/keys)",
                "← Back",
            ],
            ProviderId::OpenAi => vec![
                "OAuth Login (use your ChatGPT subscription)",
                "API Key (from platform.openai.com/api-keys)",
                "← Back",
            ],
            _ => unreachable!(),
        };
        let method = Select::with_theme(theme)
            .with_prompt("Authentication method")
            .items(&methods)
            .default(0)
            .interact()?;

        match method {
            0 => run_oauth_auth(provider).await.map(Some),
            1 => prompt_api_key(theme, provider),
            _ => Ok(None),
        }
    } else if provider == ProviderId::Ollama {
        // Ollama runs locally, no auth needed
        Ok(Some(AuthChoice::ApiKey {
            api_key: String::new(),
        }))
    } else {
        prompt_api_key(theme, provider)
    }
}

fn prompt_api_key(theme: &ColorfulTheme, provider: ProviderId) -> Result<Option<AuthChoice>> {
    let api_key = match input_or_back(theme, &format!("Paste {} API key", provider.display_name()))?
    {
        Some(k) if !k.is_empty() => k,
        Some(_) => anyhow::bail!("API key cannot be empty"),
        None => return Ok(None),
    };
    let masked = mask_secret(&api_key);
    println!("  {ARROW} Key saved: {masked}");
    Ok(Some(AuthChoice::ApiKey { api_key }))
}

const BACK_SENTINEL: &str = "<";

fn input_or_back(theme: &ColorfulTheme, prompt: &str) -> Result<Option<String>> {
    let raw: String = Input::with_theme(theme)
        .with_prompt(format!("{prompt} (< to go back)"))
        .allow_empty(true)
        .interact_text()?;
    let trimmed = raw.trim();
    if trimmed == BACK_SENTINEL || trimmed.is_empty() {
        return Ok(None);
    }
    Ok(Some(trimmed.to_string()))
}

fn input_or_back_with_default(
    theme: &ColorfulTheme,
    prompt: &str,
    default: &str,
) -> Result<Option<String>> {
    let raw: String = Input::with_theme(theme)
        .with_prompt(format!("{prompt} (< to go back)"))
        .default(default.to_string())
        .interact_text()?;
    let trimmed = raw.trim();
    if trimmed == BACK_SENTINEL {
        return Ok(None);
    }
    Ok(Some(trimmed.to_string()))
}

/// Show first 8 and last 4 characters, mask the middle with asterisks.
fn mask_secret(s: &str) -> String {
    if s.len() <= 16 {
        return "*".repeat(s.len());
    }
    format!("{}****{}", &s[..8], &s[s.len() - 4..])
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

    let yaml = generate_provider_yaml(provider, auth, api_base_override);
    // Derive filename from the provider_id in the generated yaml so that
    // OpenAI OAuth ("openai-chatgpt") gets its own file alongside "openai".
    let pid = yaml
        .lines()
        .find_map(|l| l.strip_prefix("provider_id: "))
        .unwrap_or(provider.as_str());
    let target = providers_dir.join(format!("{pid}.yaml"));
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

    // Workspace prompt templates (AGENTS.md, SOUL.md, etc.) are created
    // automatically by workspace.init_with_defaults() during agent startup.
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
            // OpenAI OAuth uses the chatgpt codex endpoint and registers as
            // a separate "openai-chatgpt" provider so it can coexist with
            // the API-key-based "openai" provider.
            let (pid, base, model) = match provider {
                ProviderId::OpenAi => (
                    "openai-chatgpt",
                    "https://chatgpt.com/backend-api/codex",
                    "gpt-5.3-codex",
                ),
                _ => (provider.as_str(), base_url, provider.default_model()),
            };
            format!(
                "provider_id: {pid}\nenabled: true\napi_base: {base}\nauth_profile: \"{profile}\"\nmodels:\n  - {model}\n",
                pid = pid,
                base = base,
                profile = profile_name,
                model = model,
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

/// Load `main.yaml` as a typed `MainConfig`, falling back to defaults if the
/// file is missing or incomplete.
fn load_main_config(config_root: &Path) -> Result<MainConfig> {
    let main_path = config_root.join("config/main.yaml");
    if !main_path.exists() {
        return Ok(MainConfig::default());
    }
    let content = fs::read_to_string(&main_path)?;
    // If the file exists but can't parse (e.g. only has web_password_hash),
    // merge the raw value into a default config.
    match serde_yaml::from_str::<MainConfig>(&content) {
        Ok(cfg) => Ok(cfg),
        Err(_) => {
            // Parse as raw value and try to preserve web_password_hash
            let raw: serde_yaml::Value = serde_yaml::from_str(&content)
                .unwrap_or(serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
            let mut cfg = MainConfig::default();
            if let Some(hash) = raw.get("web_password_hash").and_then(|v| v.as_str()) {
                cfg.web_password_hash = Some(hash.to_string());
            }
            Ok(cfg)
        }
    }
}

/// Save `MainConfig` to `config/main.yaml`.
fn save_main_config(config_root: &Path, config: &MainConfig) -> Result<()> {
    let main_path = config_root.join("config/main.yaml");
    let yaml = serde_yaml::to_string(config)?;
    fs::write(&main_path, yaml)?;
    Ok(())
}

#[cfg(test)]
fn generate_main_yaml(
    _app_name: &str,
    telegram: Option<ChannelConfig>,
    discord: Option<ChannelConfig>,
) -> String {
    let mut cfg = MainConfig::default();
    if let Some(tg) = telegram {
        cfg.channels.telegram = Some(TelegramChannelConfig {
            enabled: true,
            connectors: vec![TelegramConnectorConfig {
                connector_id: tg.connector_id,
                token: tg.token,
                require_mention: tg.require_mention,
            }],
        });
    }
    if let Some(dc) = discord {
        cfg.channels.discord = Some(DiscordChannelConfig {
            enabled: true,
            connectors: vec![DiscordConnectorConfig {
                connector_id: dc.connector_id,
                token: dc.token,
                groups: dc.groups,
                require_mention: dc.require_mention,
            }],
        });
    }
    serde_yaml::to_string(&cfg).expect("failed to serialize MainConfig")
}

fn generate_routing_yaml(
    default_agent_id: &str,
    channel_type: &str,
    connector_id: &str,
    kinds: &[&str],
) -> String {
    let mut out = format!("default_agent_id: {default_agent_id}\n\nbindings:\n");

    for kind in kinds {
        out.push_str(&format!(
            "  - channel_type: {channel_type}\n    connector_id: {connector_id}\n    match:\n      kind: {kind}\n    agent_id: {default_agent_id}\n",
        ));
    }

    out
}

fn provider_models_for_id(provider_id: &str) -> Vec<String> {
    clawhive_schema::provider_presets::provider_models_for_id(provider_id)
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
        add_channel_to_config, add_routing_binding, build_action_labels, ensure_required_dirs,
        generate_agent_yaml, generate_main_yaml, generate_provider_yaml, generate_routing_yaml,
        provider_models_for_id, remove_channel_from_config, remove_routing_binding,
        validate_generated_config, write_agent_files_unchecked, write_provider_config_unchecked,
        AuthChoice, ChannelConfig, ProviderId, SetupAction, ALL_PROVIDERS,
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

        assert!(yaml.contains("provider_id: openai-chatgpt"));
        assert!(yaml.contains("auth_profile: \"openai-oauth\""));
        assert!(yaml.contains("api_base: https://chatgpt.com/backend-api/codex"));
        assert!(yaml.contains("gpt-5.3-codex"));
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
    fn provider_yaml_openai_oauth_uses_chatgpt_provider_id() {
        let yaml = generate_provider_yaml(
            ProviderId::OpenAi,
            &AuthChoice::OAuth {
                profile_name: "openai-oauth-123".to_string(),
            },
            None,
        );

        // OpenAI OAuth should produce provider_id "openai-chatgpt" so it
        // registers separately from the API-key-based "openai" provider.
        assert!(yaml.contains("provider_id: openai-chatgpt"));
        assert!(yaml.contains("auth_profile: \"openai-oauth-123\""));
        assert!(yaml.contains("api_base: https://chatgpt.com/backend-api/codex"));
        assert!(yaml.contains("gpt-5.3-codex"));
        assert!(!yaml.contains("api_key:"));
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
    fn provider_model_aliases_are_fully_qualified() {
        for provider in ALL_PROVIDERS {
            let models = provider_models_for_id(provider.as_str());
            let prefix = provider.as_str();
            assert!(
                models
                    .iter()
                    .all(|m: &String| m.starts_with(&format!("{prefix}/"))),
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
                groups: Vec::new(),
                require_mention: true,
            }),
            Some(ChannelConfig {
                connector_id: "dc-main".to_string(),
                token: "discord-token".to_string(),
                groups: Vec::new(),
                require_mention: true,
            }),
        );

        // Tokens must appear as plaintext (not env-var references)
        assert!(yaml.contains("123:telegram-token"));
        assert!(yaml.contains("discord-token"));
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
                groups: Vec::new(),
                require_mention: true,
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
                groups: Vec::new(),
                require_mention: true,
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
                groups: Vec::new(),
                require_mention: true,
            }),
            Some(ChannelConfig {
                connector_id: "dc-main".into(),
                token: "tok2".into(),
                groups: Vec::new(),
                require_mention: true,
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
        // Generate initial routing with tg-main, then add dc-main
        let initial = generate_routing_yaml("clawhive-main", "telegram", "tg-main", &["dm"]);
        std::fs::write(temp.path().join("config/routing.yaml"), &initial).unwrap();
        add_routing_binding(temp.path(), "discord", "dc-main", "clawhive-main", &["dm"]).unwrap();

        remove_routing_binding(temp.path(), "dc-main").unwrap();
        let content = std::fs::read_to_string(temp.path().join("config/routing.yaml")).unwrap();
        assert!(content.contains("tg-main"));
        assert!(!content.contains("dc-main"));
    }

    #[test]
    fn routing_yaml_contains_bindings_for_enabled_channels() {
        let yaml = generate_routing_yaml("clawhive-main", "telegram", "tg-main", &["dm", "group"]);

        assert!(yaml.contains("channel_type: telegram"));
        assert!(yaml.contains("connector_id: tg-main"));
        assert!(yaml.contains("kind: dm"));
        assert!(yaml.contains("kind: group"));
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
            "default_agent_id: clawhive-main\n\nbindings: []\n",
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
            tools: crate::setup_scan::ToolsState {
                web_search_enabled: false,
                web_search_provider: None,
                actionbook_enabled: false,
                actionbook_installed: false,
            },
        });

        assert_eq!(labels.len(), 7);
        assert!(matches!(labels[0].0, SetupAction::AddProvider));
        assert!(matches!(labels[1].0, SetupAction::AddAgent));
        assert!(matches!(labels[2].0, SetupAction::AddChannel));
        assert!(matches!(labels[3].0, SetupAction::ConfigureTools));
        assert!(matches!(labels[4].0, SetupAction::Modify));
        assert!(matches!(labels[5].0, SetupAction::Remove));
        assert!(matches!(labels[6].0, SetupAction::Done));
        assert!(labels[3].1.contains("browser:"));
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
    fn write_agent_files_unchecked_overwrites_yaml() {
        let temp = tempfile::tempdir().expect("create tempdir");
        ensure_required_dirs(temp.path()).expect("create required directories");

        let yaml_path = temp.path().join("config/agents.d/clawhive-main.yaml");
        std::fs::write(&yaml_path, "old: value\n").expect("write old agent yaml");

        write_agent_files_unchecked(
            temp.path(),
            "clawhive-main",
            "Clawhive",
            "🦀",
            "openai/gpt-4o-mini",
        )
        .expect("write agent files");

        let yaml = std::fs::read_to_string(&yaml_path).expect("read yaml");
        assert!(yaml.contains("agent_id: clawhive-main"));
        assert!(!yaml.contains("old: value"));
    }
}
