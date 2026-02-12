use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};

use nanocrab_bus::EventBus;
use nanocrab_channels_telegram::TelegramBot;
use nanocrab_core::*;
use nanocrab_gateway::Gateway;
use nanocrab_memory::MemoryStore;
use nanocrab_provider::{register_builtin_providers, AnthropicProvider, ProviderRegistry};
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
    Start,
    #[command(about = "Local REPL for testing (no Telegram needed)")]
    Chat {
        #[arg(long, default_value = "nanocrab-main", help = "Agent ID to use")]
        agent: String,
    },
    #[command(about = "Validate config files")]
    Validate,
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
        Commands::Start => {
            start_bot(&cli.config_root).await?;
        }
        Commands::Chat { agent } => {
            run_repl(&cli.config_root, &agent).await?;
        }
    }

    Ok(())
}

fn bootstrap(root: &PathBuf) -> Result<(EventBus, Arc<MemoryStore>, Arc<Gateway>, NanocrabConfig)> {
    let config = load_config(&root.join("config"))?;

    let db_path = root.join("data/nanocrab.db");
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let memory = Arc::new(MemoryStore::open(db_path.to_str().unwrap_or("data/nanocrab.db"))?);

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

    let router = LlmRouter::new(registry, aliases, vec![]);

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

    let orchestrator = Arc::new(Orchestrator::new(
        router,
        config.agents.clone(),
        personas,
        session_mgr,
        memory.clone(),
        publisher.clone(),
    ));

    let gateway = Arc::new(Gateway::new(orchestrator, config.routing.clone(), publisher));

    Ok((bus, memory, gateway, config))
}

async fn start_bot(root: &PathBuf) -> Result<()> {
    let (_bus, _memory, gateway, config) = bootstrap(root)?;

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
