use std::fs;
use std::path::Path;

use anyhow::{anyhow, Result};
use console::Term;
use dialoguer::{theme::ColorfulTheme, Confirm, Select};

use clawhive_core::config::{
    DingTalkChannelConfig, DingTalkConnectorConfig, DiscordChannelConfig, DiscordConnectorConfig,
    FeishuChannelConfig, FeishuConnectorConfig, TelegramChannelConfig, TelegramConnectorConfig,
    WeComChannelConfig, WeComConnectorConfig,
};

use super::config_io::{
    input_or_back, input_or_back_with_default, load_main_config, mask_secret, save_main_config,
};
use super::scan::ConfigState;
use super::ui::print_done;
use super::ui::ARROW;

#[derive(Debug, Clone, Default)]
pub(super) struct ChannelConfig {
    pub(super) connector_id: String,
    pub(super) token: String,
    pub(super) groups: Vec<String>,
    pub(super) require_mention: bool,
    pub(super) app_id: Option<String>,
    pub(super) app_secret: Option<String>,
    pub(super) client_id: Option<String>,
    pub(super) client_secret: Option<String>,
    pub(super) bot_id: Option<String>,
    pub(super) secret: Option<String>,
}

pub(super) fn handle_add_channel(
    config_root: &Path,
    theme: &ColorfulTheme,
    state: &ConfigState,
    _force: bool,
) -> Result<()> {
    let channel_types = [
        "Telegram", "Discord", "Feishu", "DingTalk", "WeCom", "← Back",
    ];
    let selected = Select::with_theme(theme)
        .with_prompt("Channel type")
        .items(&channel_types)
        .default(0)
        .interact()?;
    let channel_type = match selected {
        0 => "telegram",
        1 => "discord",
        2 => "feishu",
        3 => "dingtalk",
        4 => "wecom",
        _ => return Ok(()),
    };
    let default_id = match channel_type {
        "telegram" => "my_telegram_bot",
        "discord" => "my_discord_bot",
        "feishu" => "my_feishu_bot",
        "dingtalk" => "my_dingtalk_bot",
        "wecom" => "my_wecom_bot",
        _ => "my_bot",
    };

    let connector_id = match input_or_back_with_default(
        theme,
        "Bot name (a unique name to identify this bot)",
        default_id,
    )? {
        Some(id) => id,
        None => return Ok(()),
    };

    let token;
    let mut app_id = None;
    let mut app_secret = None;
    let mut client_id = None;
    let mut client_secret = None;
    let mut bot_id_str = None;
    let mut secret = None;

    match channel_type {
        "feishu" => {
            let id = match input_or_back(theme, "App ID (from Feishu Developer Console)")? {
                Some(t) if !t.is_empty() => t,
                Some(_) => anyhow::bail!("App ID cannot be empty"),
                None => return Ok(()),
            };
            let sec = match input_or_back(theme, "App Secret")? {
                Some(t) if !t.is_empty() => t,
                Some(_) => anyhow::bail!("App Secret cannot be empty"),
                None => return Ok(()),
            };
            println!(
                "  {ARROW} Credentials saved: {}:{}",
                mask_secret(&id),
                mask_secret(&sec)
            );
            app_id = Some(id);
            app_secret = Some(sec);
            token = String::new();
        }
        "dingtalk" => {
            let id = match input_or_back(theme, "Client ID (AppKey from DingTalk Developer)")? {
                Some(t) if !t.is_empty() => t,
                Some(_) => anyhow::bail!("Client ID cannot be empty"),
                None => return Ok(()),
            };
            let sec = match input_or_back(theme, "Client Secret (AppSecret)")? {
                Some(t) if !t.is_empty() => t,
                Some(_) => anyhow::bail!("Client Secret cannot be empty"),
                None => return Ok(()),
            };
            println!(
                "  {ARROW} Credentials saved: {}:{}",
                mask_secret(&id),
                mask_secret(&sec)
            );
            client_id = Some(id);
            client_secret = Some(sec);
            token = String::new();
        }
        "wecom" => {
            let id = match input_or_back(theme, "Bot ID (from WeCom Admin Console)")? {
                Some(t) if !t.is_empty() => t,
                Some(_) => anyhow::bail!("Bot ID cannot be empty"),
                None => return Ok(()),
            };
            let sec = match input_or_back(theme, "Secret")? {
                Some(t) if !t.is_empty() => t,
                Some(_) => anyhow::bail!("Secret cannot be empty"),
                None => return Ok(()),
            };
            println!(
                "  {ARROW} Credentials saved: {}:{}",
                mask_secret(&id),
                mask_secret(&sec)
            );
            bot_id_str = Some(id);
            secret = Some(sec);
            token = String::new();
        }
        _ => {
            token = match input_or_back(theme, "Bot token")? {
                Some(t) if !t.is_empty() => t,
                Some(_) => anyhow::bail!("Bot token cannot be empty"),
                None => return Ok(()),
            };
            let masked = mask_secret(&token);
            println!("  {ARROW} Token saved: {masked}");
        }
    }

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
        app_id,
        app_secret,
        client_id,
        client_secret,
        bot_id: bot_id_str,
        secret,
    };
    add_channel_to_config(config_root, channel_type, &cfg)?;
    print_done(
        &Term::stdout(),
        &format!("Channel {connector_id} ({channel_type}) configured."),
    );
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
        "feishu" => {
            let connector = FeishuConnectorConfig {
                connector_id: cfg.connector_id.clone(),
                app_id: cfg.app_id.clone().unwrap_or_default(),
                app_secret: cfg.app_secret.clone().unwrap_or_default(),
            };
            match main_cfg.channels.feishu.as_mut() {
                Some(fs) => {
                    fs.enabled = true;
                    fs.connectors.retain(|c| c.connector_id != cfg.connector_id);
                    fs.connectors.push(connector);
                }
                None => {
                    main_cfg.channels.feishu = Some(FeishuChannelConfig {
                        enabled: true,
                        connectors: vec![connector],
                    });
                }
            }
        }
        "dingtalk" => {
            let connector = DingTalkConnectorConfig {
                connector_id: cfg.connector_id.clone(),
                client_id: cfg.client_id.clone().unwrap_or_default(),
                client_secret: cfg.client_secret.clone().unwrap_or_default(),
            };
            match main_cfg.channels.dingtalk.as_mut() {
                Some(dt) => {
                    dt.enabled = true;
                    dt.connectors.retain(|c| c.connector_id != cfg.connector_id);
                    dt.connectors.push(connector);
                }
                None => {
                    main_cfg.channels.dingtalk = Some(DingTalkChannelConfig {
                        enabled: true,
                        connectors: vec![connector],
                    });
                }
            }
        }
        "wecom" => {
            let connector = WeComConnectorConfig {
                connector_id: cfg.connector_id.clone(),
                bot_id: cfg.bot_id.clone().unwrap_or_default(),
                secret: cfg.secret.clone().unwrap_or_default(),
            };
            match main_cfg.channels.wecom.as_mut() {
                Some(wc) => {
                    wc.enabled = true;
                    wc.connectors.retain(|c| c.connector_id != cfg.connector_id);
                    wc.connectors.push(connector);
                }
                None => {
                    main_cfg.channels.wecom = Some(WeComChannelConfig {
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

pub(super) fn remove_channel_from_config(config_root: &Path, connector_id: &str) -> Result<()> {
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

pub(super) fn remove_routing_binding(config_root: &Path, connector_id: &str) -> Result<()> {
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

#[cfg(test)]
fn generate_main_yaml(
    _app_name: &str,
    telegram: Option<ChannelConfig>,
    discord: Option<ChannelConfig>,
) -> String {
    use clawhive_core::config::MainConfig;

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

#[cfg(test)]
mod tests {
    use super::*;

    fn generate_provider_yaml_for_test() -> String {
        "provider_id: openai\nenabled: true\napi_base: https://api.openai.com/v1\napi_key: \"sk-test\"\nmodels:\n  - gpt-4o-mini\n".to_string()
    }

    fn generate_agent_yaml_for_test() -> String {
        "agent_id: clawhive-main\nenabled: true\nidentity:\n  name: \"Clawhive\"\n  emoji: \"🦀\"\nmodel_policy:\n  primary: \"openai/gpt-4o-mini\"\n  fallbacks: []\nmemory_policy:\n  mode: \"standard\"\n  write_scope: \"all\"\n".to_string()
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
                ..Default::default()
            }),
            Some(ChannelConfig {
                connector_id: "dc-main".to_string(),
                token: "discord-token".to_string(),
                groups: Vec::new(),
                require_mention: true,
                ..Default::default()
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
                ..Default::default()
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
                ..Default::default()
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
                ..Default::default()
            }),
            Some(ChannelConfig {
                connector_id: "dc-main".into(),
                token: "tok2".into(),
                groups: Vec::new(),
                require_mention: true,
                ..Default::default()
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
    fn validate_generated_config_accepts_minimal_valid_files() {
        use super::super::config_io::{ensure_required_dirs, validate_generated_config};

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
            generate_provider_yaml_for_test(),
        )
        .expect("write provider yaml");
        std::fs::write(
            temp.path().join("config/agents.d/clawhive-main.yaml"),
            generate_agent_yaml_for_test(),
        )
        .expect("write agent yaml");

        validate_generated_config(temp.path()).expect("generated config should be valid");
    }
}
