use std::path::Path;

use clawhive_core::load_config;
use console::style;

use crate::{is_process_running, read_pid_file};

pub fn print_status(root: &Path) {
    let pid_info = match read_pid_file(root) {
        Ok(Some(pid)) if is_process_running(pid) => Some(pid),
        Ok(Some(_)) => None,
        _ => None,
    };

    let running = pid_info.is_some();

    // Header
    if running {
        println!(
            "  clawhive is {} (pid: {})",
            style("running").green(),
            pid_info.unwrap()
        );
    } else {
        println!("  clawhive is {}", style("stopped").red());
    }
    println!();

    // Config info
    let config_path = root.join("config");
    match load_config(&config_path) {
        Ok(config) => {
            // Agents
            let enabled_agents: Vec<_> = config.agents.iter().filter(|a| a.enabled).collect();
            println!(
                "  Agents:      {} configured, {} enabled",
                config.agents.len(),
                enabled_agents.len()
            );
            for agent in &enabled_agents {
                println!(
                    "               {} (model: {})",
                    style(&agent.agent_id).cyan(),
                    agent.model_policy.primary
                );
            }

            // Providers
            let enabled_providers: Vec<_> = config.providers.iter().filter(|p| p.enabled).collect();
            println!(
                "  Providers:   {} configured, {} enabled",
                config.providers.len(),
                enabled_providers.len()
            );
            for p in &enabled_providers {
                let key_status = if p.api_key.as_ref().is_some_and(|k| !k.is_empty()) {
                    style("✓ key set").green().to_string()
                } else if p.auth_profile.is_some() {
                    style("✓ oauth").green().to_string()
                } else {
                    style("✗ no key").yellow().to_string()
                };
                println!(
                    "               {} ({})",
                    style(&p.provider_id).cyan(),
                    key_status
                );
            }

            // Channels
            let mut channels: Vec<&str> = Vec::new();
            if config.main.channels.telegram.is_some() {
                channels.push("telegram");
            }
            if config.main.channels.discord.is_some() {
                channels.push("discord");
            }
            if channels.is_empty() {
                println!("  Channels:    {}", style("none configured").yellow());
            } else {
                println!(
                    "  Channels:    {}",
                    channels
                        .iter()
                        .map(|c| style(c).cyan().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }

            // Routing
            println!(
                "  Routing:     {} bindings (default: {})",
                config.routing.bindings.len(),
                style(&config.routing.default_agent_id).cyan()
            );
        }
        Err(_) => {
            println!(
                "  Config:      {}",
                style("not found — run `clawhive setup`").yellow()
            );
        }
    }

    // Paths
    println!();
    println!("  Config dir:  {}", root.join("config").display());
    println!("  Data dir:    {}", root.join("data").display());
    println!("  Log dir:     {}", root.join("logs").display());
}
