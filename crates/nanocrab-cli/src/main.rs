use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};

use nanocrab_bus::EventBus;
use nanocrab_channels_telegram::TelegramBot;
use nanocrab_core::*;
use nanocrab_gateway::{Gateway, RateLimitConfig, RateLimiter};
use nanocrab_memory::MemoryStore;
use nanocrab_provider::{register_builtin_providers, AnthropicProvider, ProviderRegistry};
use nanocrab_runtime::NativeExecutor;
use nanocrab_schema::InboundMessage;

#[derive(Parser)]
#[command(name = "nanocrab", version, about = "nanocrab AI agent framework")]
struct Cli {
    #[arg(long, default_value = ".", help = "Config root directory (contains config/ and prompts/)")]
    config_root: PathBuf,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    #[command(about = "Start the Telegram bot")]
    Start {
        #[arg(long, help = "Run TUI dashboard in the same process")]
        tui: bool,
    },
    #[command(about = "Local REPL for testing (no Telegram needed)")]
    Chat {
        #[arg(long, default_value = "nanocrab-main", help = "Agent ID to use")]
        agent: String,
    },
    #[command(about = "Validate config files")]
    Validate,
    #[command(about = "Run memory consolidation manually")]
    Consolidate,
    #[command(subcommand, about = "Agent management")]
    Agent(AgentCommands),
    #[command(subcommand, about = "Skill management")]
    Skill(SkillCommands),
    #[command(subcommand, about = "Session management")]
    Session(SessionCommands),
    #[command(subcommand, about = "Task management")]
    Task(TaskCommands),
}

#[derive(Subcommand)]
enum AgentCommands {
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

#[derive(Subcommand)]
enum SkillCommands {
    #[command(about = "List available skills")]
    List,
    #[command(about = "Show skill details")]
    Show {
        #[arg(help = "Skill name")]
        skill_name: String,
    },
}

#[derive(Subcommand)]
enum SessionCommands {
    #[command(about = "Reset a session by key")]
    Reset {
        #[arg(help = "Session key")]
        session_key: String,
    },
}

#[derive(Subcommand)]
enum TaskCommands {
    #[command(about = "Trigger a one-off task")]
    Trigger {
        #[arg(help = "Agent ID")]
        agent: String,
        #[arg(help = "Task description")]
        task: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    match cli.command {
        Commands::Validate => {
            let config = load_config(&cli.config_root.join("config"))?;
            println!(
                "Config valid. {} agents, {} providers, {} routing bindings.",
                config.agents.len(),
                config.providers.len(),
                config.routing.bindings.len()
            );
        }
        Commands::Start { tui } => {
            start_bot(&cli.config_root, tui).await?;
        }
        Commands::Chat { agent } => {
            run_repl(&cli.config_root, &agent).await?;
        }
        Commands::Consolidate => {
            run_consolidate(&cli.config_root).await?;
        }
        Commands::Agent(cmd) => {
            let config = load_config(&cli.config_root.join("config"))?;
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
                    let config_dir = cli.config_root.join("config/agents.d");
                    toggle_agent(&config_dir, &agent_id, true)?;
                    println!("Agent '{agent_id}' enabled.");
                }
                AgentCommands::Disable { agent_id } => {
                    let config_dir = cli.config_root.join("config/agents.d");
                    toggle_agent(&config_dir, &agent_id, false)?;
                    println!("Agent '{agent_id}' disabled.");
                }
            }
        }
        Commands::Skill(cmd) => {
            let skill_registry = SkillRegistry::load_from_dir(&cli.config_root.join("skills"))
                .unwrap_or_else(|_| SkillRegistry::new());
            match cmd {
                SkillCommands::List => {
                    let skills = skill_registry.list();
                    if skills.is_empty() {
                        println!("No skills found in skills/ directory.");
                    } else {
                        println!("{:<20} {:<50} {:<10}", "NAME", "DESCRIPTION", "AVAILABLE");
                        println!("{}", "-".repeat(80));
                        for skill in &skills {
                            println!(
                                "{:<20} {:<50} {:<10}",
                                skill.name,
                                if skill.description.len() > 48 {
                                    format!("{}...", &skill.description[..45])
                                } else {
                                    skill.description.clone()
                                },
                                if skill.requirements_met() { "yes" } else { "no" },
                            );
                        }
                    }
                }
                SkillCommands::Show { skill_name } => match skill_registry.get(&skill_name) {
                    Some(skill) => {
                        println!("Skill: {}", skill.name);
                        println!("Description: {}", skill.description);
                        println!(
                            "Available: {}",
                            if skill.requirements_met() { "yes" } else { "no" }
                        );
                        if !skill.requires.bins.is_empty() {
                            println!("Required bins: {}", skill.requires.bins.join(", "));
                        }
                        if !skill.requires.env.is_empty() {
                            println!("Required env: {}", skill.requires.env.join(", "));
                        }
                        println!("\n--- Content ---\n{}", skill.content);
                    }
                    None => {
                        anyhow::bail!("skill not found: {skill_name}");
                    }
                },
            }
        }
        Commands::Session(cmd) => {
            let (_bus, memory, _gateway, _config) = bootstrap(&cli.config_root)?;
            let session_mgr = SessionManager::new(memory, 1800);
            match cmd {
                SessionCommands::Reset { session_key } => {
                    let key = nanocrab_schema::SessionKey(session_key.clone());
                    match session_mgr.reset(&key).await? {
                        true => println!("Session '{session_key}' reset successfully."),
                        false => println!("Session '{session_key}' not found."),
                    }
                }
            }
        }
        Commands::Task(cmd) => {
            let (_bus, _memory, gateway, _config) = bootstrap(&cli.config_root)?;
            match cmd {
                TaskCommands::Trigger { agent: _agent, task } => {
                    let inbound = InboundMessage {
                        trace_id: uuid::Uuid::new_v4(),
                        channel_type: "cli".into(),
                        connector_id: "cli".into(),
                        conversation_scope: "task:cli".into(),
                        user_scope: "user:cli".into(),
                        text: task,
                        at: chrono::Utc::now(),
                        thread_id: None,
                        is_mention: false,
                        mention_target: None,
                    };
                    match gateway.handle_inbound(inbound).await {
                        Ok(out) => println!("{}", out.text),
                        Err(err) => eprintln!("Task failed: {err}"),
                    }
                }
            }
        }
    }

    Ok(())
}

fn toggle_agent(agents_dir: &std::path::Path, agent_id: &str, enabled: bool) -> Result<()> {
    let path = agents_dir.join(format!("{agent_id}.yaml"));
    if !path.exists() {
        anyhow::bail!("agent config not found: {}", path.display());
    }
    let content = std::fs::read_to_string(&path)?;
    let mut doc: serde_yaml::Value = serde_yaml::from_str(&content)?;
    if let serde_yaml::Value::Mapping(ref mut map) = doc {
        map.insert(
            serde_yaml::Value::String("enabled".into()),
            serde_yaml::Value::Bool(enabled),
        );
    }
    let output = serde_yaml::to_string(&doc)?;
    std::fs::write(&path, output)?;
    Ok(())
}

fn bootstrap(root: &PathBuf) -> Result<(EventBus, Arc<MemoryStore>, Arc<Gateway>, NanocrabConfig)> {
    let config = load_config(&root.join("config"))?;

    let db_path = root.join("data/nanocrab.db");
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let memory = Arc::new(MemoryStore::open(db_path.to_str().unwrap_or("data/nanocrab.db"))?);

    let router = build_router_from_config(&config);

    let prompts_root = root.join("prompts");
    let mut personas = HashMap::new();
    for agent_config in &config.agents {
        let identity = agent_config.identity.as_ref();
        let name = identity
            .map(|i| i.name.as_str())
            .unwrap_or(&agent_config.agent_id);
        let emoji = identity.and_then(|i| i.emoji.as_deref());
        match load_persona(&prompts_root, &agent_config.agent_id, name, emoji) {
            Ok(persona) => {
                personas.insert(agent_config.agent_id.clone(), persona);
            }
            Err(e) => {
                tracing::warn!("Failed to load persona for {}: {e}", agent_config.agent_id);
            }
        }
    }

    let bus = EventBus::new(256);
    let publisher = bus.publisher();
    let session_mgr = SessionManager::new(memory.clone(), 1800);
    let skill_registry = SkillRegistry::load_from_dir(&root.join("skills")).unwrap_or_else(|e| {
        tracing::warn!("Failed to load skills: {e}");
        SkillRegistry::new()
    });

    let orchestrator = Arc::new(Orchestrator::new(
        router,
        config.agents.clone(),
        personas,
        session_mgr,
        skill_registry,
        memory.clone(),
        publisher.clone(),
        Arc::new(NativeExecutor),
    ));

    let rate_limiter = RateLimiter::new(RateLimitConfig::default());
    let gateway = Arc::new(Gateway::new(
        orchestrator,
        config.routing.clone(),
        publisher,
        rate_limiter,
    ));

    Ok((bus, memory, gateway, config))
}

fn build_router_from_config(config: &NanocrabConfig) -> LlmRouter {
    let mut registry = ProviderRegistry::new();
    for provider_config in &config.providers {
        if !provider_config.enabled {
            continue;
        }
        match provider_config.provider_id.as_str() {
            "anthropic" => {
                let api_key = resolve_env_var(&provider_config.api_key_env);
                if !api_key.is_empty() {
                    let provider = Arc::new(AnthropicProvider::new(
                        api_key,
                        provider_config.api_base.clone(),
                    ));
                    registry.register("anthropic", provider);
                } else {
                    tracing::warn!("Anthropic API key not set, using stub provider");
                    register_builtin_providers(&mut registry);
                }
            }
            _ => {
                tracing::warn!("Unknown provider: {}", provider_config.provider_id);
            }
        }
    }

    if registry.get("anthropic").is_err() {
        register_builtin_providers(&mut registry);
    }

    let mut aliases = HashMap::new();
    aliases.insert(
        "sonnet".to_string(),
        "anthropic/claude-sonnet-4-5".to_string(),
    );
    aliases.insert(
        "haiku".to_string(),
        "anthropic/claude-3-5-haiku-latest".to_string(),
    );

    LlmRouter::new(registry, aliases, vec![])
}

async fn start_bot(root: &PathBuf, with_tui: bool) -> Result<()> {
    let (bus, memory, gateway, config) = bootstrap(root)?;

    let consolidator = Arc::new(Consolidator::new(
        memory,
        Arc::new(build_router_from_config(&config)),
        "sonnet".to_string(),
        vec!["haiku".to_string()],
    ));
    let scheduler = ConsolidationScheduler::new(consolidator, 24);
    let _consolidation_handle = scheduler.start();
    tracing::info!("Consolidation scheduler started (every 24h)");

    let _tui_handle = if with_tui {
        let receivers = nanocrab_tui::subscribe_all(&bus).await;
        Some(tokio::spawn(async move {
            if let Err(err) = nanocrab_tui::run_tui_from_receivers(receivers).await {
                tracing::error!("TUI exited with error: {err}");
            }
        }))
    } else {
        None
    };

    let tg_config = config
        .main
        .channels
        .telegram
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("telegram not configured in main.yaml"))?;

    if !tg_config.enabled {
        anyhow::bail!("telegram channel is disabled");
    }

    for connector in &tg_config.connectors {
        let token = resolve_env_var(&connector.token);
        if token.is_empty() {
            anyhow::bail!(
                "Telegram token is empty for connector {}",
                connector.connector_id
            );
        }

        tracing::info!("Starting Telegram bot: {}", connector.connector_id);
        let bot = TelegramBot::new(
            token,
            connector.connector_id.clone(),
            gateway.clone(),
        );
        bot.run().await?;
        break;
    }

    Ok(())
}

async fn run_consolidate(root: &PathBuf) -> Result<()> {
    let (_bus, memory, _gateway, config) = bootstrap(root)?;

    let consolidator = Arc::new(Consolidator::new(
        memory,
        Arc::new(build_router_from_config(&config)),
        "sonnet".to_string(),
        vec!["haiku".to_string()],
    ));

    let scheduler = ConsolidationScheduler::new(consolidator, 24);
    println!("Running consolidation...");
    let report = scheduler.run_once().await?;
    println!("Consolidation complete:");
    println!("  Concepts created: {}", report.concepts_created);
    println!("  Concepts updated: {}", report.concepts_updated);
    println!("  Episodes processed: {}", report.episodes_processed);
    println!("  Concepts staled: {}", report.concepts_staled);
    println!("  Episodes purged: {}", report.episodes_purged);
    Ok(())
}

async fn run_repl(root: &PathBuf, _agent_id: &str) -> Result<()> {
    let (_bus, _memory, gateway, _config) = bootstrap(root)?;

    println!("nanocrab REPL. Type 'quit' to exit.");
    println!("---");

    let stdin = std::io::stdin();
    loop {
        print!("> ");
        std::io::stdout().flush()?;
        let mut input = String::new();
        stdin.read_line(&mut input)?;
        let input = input.trim();
        if input == "quit" || input == "exit" {
            break;
        }
        if input.is_empty() {
            continue;
        }

        let inbound = InboundMessage {
            trace_id: uuid::Uuid::new_v4(),
            channel_type: "repl".into(),
            connector_id: "repl".into(),
            conversation_scope: "repl:0".into(),
            user_scope: "user:local".into(),
            text: input.to_string(),
            at: chrono::Utc::now(),
            thread_id: None,
            is_mention: false,
            mention_target: None,
        };

        match gateway.handle_inbound(inbound).await {
            Ok(out) => println!("{}", out.text),
            Err(err) => eprintln!("Error: {err}"),
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn parses_consolidate_subcommand() {
        let cli = Cli::parse_from(["nanocrab", "consolidate"]);
        assert!(matches!(cli.command, Commands::Consolidate));
    }

    #[test]
    fn parses_start_tui_flag() {
        let cli = Cli::try_parse_from(["nanocrab", "start", "--tui"]).unwrap();
        assert!(matches!(cli.command, Commands::Start { tui: true }));
    }

    #[test]
    fn parses_agent_list_subcommand() {
        let cli = Cli::try_parse_from(["nanocrab", "agent", "list"]).unwrap();
        assert!(matches!(cli.command, Commands::Agent(AgentCommands::List)));
    }

    #[test]
    fn parses_skill_list_subcommand() {
        let cli = Cli::try_parse_from(["nanocrab", "skill", "list"]).unwrap();
        assert!(matches!(cli.command, Commands::Skill(SkillCommands::List)));
    }

    #[test]
    fn parses_session_reset_subcommand() {
        let cli = Cli::try_parse_from(["nanocrab", "session", "reset", "my-session"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Session(SessionCommands::Reset { .. })
        ));
    }

    #[test]
    fn parses_task_trigger_subcommand() {
        let cli = Cli::try_parse_from(["nanocrab", "task", "trigger", "main", "do stuff"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Task(TaskCommands::Trigger { .. })
        ));
    }

    #[test]
    fn parses_agent_enable_subcommand() {
        let cli = Cli::try_parse_from(["nanocrab", "agent", "enable", "my-agent"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Agent(AgentCommands::Enable { .. })
        ));
    }
}
