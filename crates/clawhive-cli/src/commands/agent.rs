use std::path::Path;

use anyhow::Result;
use clap::Subcommand;
use clawhive_core::load_config;

use crate::runtime::bootstrap::toggle_agent;
use crate::runtime::pid::{is_process_running, read_pid_file};

fn print_reload_hint(root: &Path) {
    let running = read_pid_file(root)
        .ok()
        .flatten()
        .is_some_and(is_process_running);
    if running {
        println!("Run `clawhive reload` to apply changes to the running service.");
    }
}

#[derive(Subcommand)]
pub(crate) enum AgentCommands {
    #[command(about = "List all configured agents")]
    List,
    #[command(about = "Show agent details")]
    Show {
        #[arg(help = "Agent ID")]
        agent_id: String,
    },
    #[command(about = "Enable an agent")]
    Enable {
        #[arg(help = "Agent ID")]
        agent_id: String,
    },
    #[command(about = "Disable an agent")]
    Disable {
        #[arg(help = "Agent ID")]
        agent_id: String,
    },
}

pub(crate) fn run(cmd: AgentCommands, root: &Path) -> Result<()> {
    let config = load_config(&root.join("config"))?;
    match cmd {
        AgentCommands::List => {
            println!(
                "{:<20} {:<10} {:<30} {:<20}",
                "AGENT ID", "ENABLED", "PRIMARY MODEL", "IDENTITY"
            );
            println!("{}", "-".repeat(80));
            for agent in &config.agents {
                let name = agent
                    .identity
                    .as_ref()
                    .map(|i| format!("{} {}", i.emoji.as_deref().unwrap_or(""), i.name))
                    .unwrap_or_else(|| "-".to_string());
                println!(
                    "{:<20} {:<10} {:<30} {:<20}",
                    agent.agent_id,
                    if agent.enabled { "yes" } else { "no" },
                    agent.model_policy.primary,
                    name.trim(),
                );
            }
        }
        AgentCommands::Show { agent_id } => {
            let agent = config
                .agents
                .iter()
                .find(|a| a.agent_id == agent_id)
                .ok_or_else(|| anyhow::anyhow!("agent not found: {agent_id}"))?;
            println!("Agent: {}", agent.agent_id);
            println!("Enabled: {}", agent.enabled);
            if let Some(identity) = &agent.identity {
                println!("Name: {}", identity.name);
                if let Some(emoji) = &identity.emoji {
                    println!("Emoji: {emoji}");
                }
            }
            println!("Primary model: {}", agent.model_policy.primary);
            if !agent.model_policy.fallbacks.is_empty() {
                println!("Fallbacks: {}", agent.model_policy.fallbacks.join(", "));
            }
            if let Some(tp) = &agent.tool_policy {
                println!("Tools: {}", tp.allow.join(", "));
            }
            if let Some(mp) = &agent.memory_policy {
                println!("Memory: mode={}, write_scope={}", mp.mode, mp.write_scope);
            }
            if let Some(sa) = &agent.sub_agent {
                println!("Sub-agent: allow_spawn={}", sa.allow_spawn);
            }
        }
        AgentCommands::Enable { agent_id } => {
            let config_dir = root.join("config/agents.d");
            toggle_agent(&config_dir, &agent_id, true)?;
            println!("Agent '{agent_id}' enabled.");
            print_reload_hint(root);
        }
        AgentCommands::Disable { agent_id } => {
            let config_dir = root.join("config/agents.d");
            toggle_agent(&config_dir, &agent_id, false)?;
            println!("Agent '{agent_id}' disabled.");
            print_reload_hint(root);
        }
    }
    Ok(())
}
