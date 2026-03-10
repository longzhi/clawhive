use std::fs;
use std::path::Path;

use anyhow::{anyhow, Result};
use console::{style, Term};
use dialoguer::{theme::ColorfulTheme, Confirm, Input, Select};

mod agent;
mod channel;
mod config_io;
mod provider;
pub(crate) mod scan;
mod tools;
pub(crate) mod ui;

use agent::handle_add_agent;
use channel::{handle_add_channel, remove_channel_from_config, remove_routing_binding};
use config_io::{
    ensure_required_dirs, load_main_config, save_main_config, validate_generated_config,
};
use provider::handle_add_provider;
use scan::scan_config;
use tools::handle_configure_tools;
use ui::{print_done, print_logo, render_dashboard, ARROW, CRAB};

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

fn build_action_labels(state: &scan::ConfigState) -> Vec<(SetupAction, String)> {
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

async fn handle_modify(
    config_root: &Path,
    theme: &ColorfulTheme,
    state: &scan::ConfigState,
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
    state: &scan::ConfigState,
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

#[cfg(test)]
mod tests {
    use super::*;
    use scan::ToolsState;

    #[test]
    fn ensure_required_dirs_creates_expected_paths() {
        let temp = tempfile::tempdir().expect("create tempdir");
        ensure_required_dirs(temp.path()).expect("create required directories");

        for rel in ["config/agents.d", "config/providers.d"] {
            assert!(temp.path().join(rel).exists(), "missing {rel}");
        }
    }

    #[test]
    fn build_action_labels_includes_all_actions() {
        let labels = build_action_labels(&scan::ConfigState {
            providers: vec![],
            agents: vec![],
            channels: vec![],
            default_agent: None,
            tools: ToolsState {
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
}
