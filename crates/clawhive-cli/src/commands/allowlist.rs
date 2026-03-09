use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use clap::{Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct AllowlistFile {
    #[serde(default)]
    pub(crate) agents: HashMap<String, AllowlistAgent>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub(crate) struct AllowlistAgent {
    #[serde(default)]
    pub(crate) exec: Vec<String>,
    #[serde(default)]
    pub(crate) network: Vec<String>,
}

#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum AllowlistType {
    Exec,
    Network,
}

#[derive(Subcommand)]
pub(crate) enum AllowlistCommands {
    #[command(about = "List runtime allowlist entries")]
    List {
        #[arg(long, help = "Filter by agent ID")]
        agent: Option<String>,
    },
    #[command(about = "Remove allowlist entries by exact pattern")]
    Remove {
        #[arg(help = "Pattern to remove")]
        pattern: String,
        #[arg(long, help = "Filter by agent ID")]
        agent: Option<String>,
        #[arg(long, value_enum, help = "Filter by entry type")]
        r#type: Option<AllowlistType>,
    },
    #[command(about = "Clear allowlist entries")]
    Clear {
        #[arg(long, help = "Filter by agent ID")]
        agent: Option<String>,
    },
}

pub(crate) fn run(cmd: AllowlistCommands, root: &Path) -> Result<()> {
    let allowlist_path = root.join("data/runtime_allowlist.json");

    match cmd {
        AllowlistCommands::List { agent } => {
            if !allowlist_path.exists() {
                println!("No allowlist entries.");
                return Ok(());
            }

            let content = std::fs::read_to_string(&allowlist_path)?;
            let allowlist: AllowlistFile =
                serde_json::from_str(&content).context("Failed to parse runtime_allowlist.json")?;
            let mut printed = false;

            for (agent_id, entries) in &allowlist.agents {
                if let Some(filter) = &agent {
                    if filter != agent_id {
                        continue;
                    }
                }

                if printed {
                    println!();
                }
                printed = true;

                println!("Agent: {agent_id}");
                println!("  exec:");
                for pattern in &entries.exec {
                    println!("    - {pattern}");
                }
                println!("  network:");
                for pattern in &entries.network {
                    println!("    - {pattern}");
                }
            }

            if !printed {
                println!("No allowlist entries.");
            }
        }
        AllowlistCommands::Remove {
            pattern,
            agent,
            r#type,
        } => {
            if !allowlist_path.exists() {
                println!("No allowlist entries.");
                return Ok(());
            }

            let content = std::fs::read_to_string(&allowlist_path)?;
            let mut allowlist: AllowlistFile =
                serde_json::from_str(&content).context("Failed to parse runtime_allowlist.json")?;
            let mut removed = Vec::new();

            for (agent_id, entries) in &mut allowlist.agents {
                if let Some(filter) = &agent {
                    if filter != agent_id {
                        continue;
                    }
                }
                match r#type {
                    Some(AllowlistType::Exec) => {
                        let before = entries.exec.len();
                        entries.exec.retain(|item| item != &pattern);
                        let count = before.saturating_sub(entries.exec.len());
                        if count > 0 {
                            removed.push((agent_id.clone(), "exec", count));
                        }
                    }
                    Some(AllowlistType::Network) => {
                        let before = entries.network.len();
                        entries.network.retain(|item| item != &pattern);
                        let count = before.saturating_sub(entries.network.len());
                        if count > 0 {
                            removed.push((agent_id.clone(), "network", count));
                        }
                    }
                    None => {
                        let exec_before = entries.exec.len();
                        entries.exec.retain(|item| item != &pattern);
                        let exec_count = exec_before.saturating_sub(entries.exec.len());
                        if exec_count > 0 {
                            removed.push((agent_id.clone(), "exec", exec_count));
                        }

                        let network_before = entries.network.len();
                        entries.network.retain(|item| item != &pattern);
                        let network_count = network_before.saturating_sub(entries.network.len());
                        if network_count > 0 {
                            removed.push((agent_id.clone(), "network", network_count));
                        }
                    }
                }
            }

            if removed.is_empty() {
                println!("No matching allowlist entries removed.");
                return Ok(());
            }

            if let Some(parent) = allowlist_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            allowlist
                .agents
                .retain(|_, entries| !entries.exec.is_empty() || !entries.network.is_empty());
            std::fs::write(&allowlist_path, serde_json::to_string_pretty(&allowlist)?)?;

            for (agent_id, category, count) in removed {
                println!(
                    "Removed {count} {category} entr{suffix} from agent '{agent_id}'.",
                    suffix = if count == 1 { "y" } else { "ies" }
                );
            }
        }
        AllowlistCommands::Clear { agent } => {
            if !allowlist_path.exists() {
                println!("No allowlist entries.");
                return Ok(());
            }

            let content = std::fs::read_to_string(&allowlist_path)?;
            let mut allowlist: AllowlistFile =
                serde_json::from_str(&content).context("Failed to parse runtime_allowlist.json")?;
            let mut cleared = Vec::new();

            for (agent_id, entries) in &mut allowlist.agents {
                if let Some(filter) = &agent {
                    if filter != agent_id {
                        continue;
                    }
                }

                let removed_count = entries.exec.len() + entries.network.len();
                if removed_count > 0 {
                    entries.exec.clear();
                    entries.network.clear();
                    cleared.push((agent_id.clone(), removed_count));
                }
            }

            if cleared.is_empty() {
                println!("No allowlist entries to clear.");
                return Ok(());
            }

            if let Some(parent) = allowlist_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            allowlist
                .agents
                .retain(|_, entries| !entries.exec.is_empty() || !entries.network.is_empty());
            std::fs::write(&allowlist_path, serde_json::to_string_pretty(&allowlist)?)?;

            for (agent_id, count) in cleared {
                println!(
                    "Cleared {count} entr{suffix} for agent '{agent_id}'.",
                    suffix = if count == 1 { "y" } else { "ies" }
                );
            }
        }
    }

    Ok(())
}
