use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

mod commands;

use commands::auth::{handle_auth_command, AuthCommands};
use nanocrab_bus::EventBus;
use nanocrab_channels::discord::DiscordBot;
use nanocrab_channels::telegram::TelegramBot;
use nanocrab_channels::ChannelBot;
use nanocrab_core::*;
use nanocrab_gateway::{Gateway, RateLimitConfig, RateLimiter};
use nanocrab_memory::embedding::{
    EmbeddingProvider, OpenAiEmbeddingProvider, StubEmbeddingProvider,
};
use nanocrab_memory::search_index::SearchIndex;
use nanocrab_memory::MemoryStore;
use nanocrab_provider::{
    register_builtin_providers, AnthropicProvider, OpenAiProvider, ProviderRegistry,
};
use nanocrab_runtime::NativeExecutor;
use nanocrab_schema::InboundMessage;

#[derive(Parser)]
#[command(name = "nanocrab", version, about = "nanocrab AI agent framework")]
struct Cli {
    #[arg(
        long,
        default_value = ".",
        help = "Config root directory (contains config/ and prompts/)"
    )]
    config_root: PathBuf,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    #[command(about = "Start all configured channel bots")]
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
    #[command(subcommand, about = "Auth management")]
    Auth(AuthCommands),
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
    let cli = Cli::parse();

    let log_dir = cli.config_root.join("logs");
    std::fs::create_dir_all(&log_dir)?;
    let file_appender = tracing_appender::rolling::daily(&log_dir, "nanocrab.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(tracing_subscriber::fmt::layer().with_ansi(false).with_writer(non_blocking))
        .init();

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
                                if skill.requirements_met() {
                                    "yes"
                                } else {
                                    "no"
                                },
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
                            if skill.requirements_met() {
                                "yes"
                            } else {
                                "no"
                            }
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
                TaskCommands::Trigger {
                    agent: _agent,
                    task,
                } => {
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
        Commands::Auth(cmd) => {
            handle_auth_command(cmd).await?;
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

fn bootstrap(root: &Path) -> Result<(EventBus, Arc<MemoryStore>, Arc<Gateway>, NanocrabConfig)> {
    let config = load_config(&root.join("config"))?;

    let db_path = root.join("data/nanocrab.db");
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let memory = Arc::new(MemoryStore::open(
        db_path.to_str().unwrap_or("data/nanocrab.db"),
    )?);

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
    let workspace_dir = root.to_path_buf();
    let file_store = nanocrab_memory::file_store::MemoryFileStore::new(&workspace_dir);
    let session_writer = nanocrab_memory::SessionWriter::new(&workspace_dir);
    let session_reader = nanocrab_memory::SessionReader::new(&workspace_dir);
    let search_index = SearchIndex::new(memory.db());
    let embedding_provider = build_embedding_provider(&config);

    let brave_api_key = config
        .main
        .tools
        .web_search
        .as_ref()
        .filter(|ws| ws.enabled)
        .and_then(|ws| ws.api_key.clone())
        .filter(|k| !k.is_empty());

    let orchestrator = Arc::new(Orchestrator::new(
        router,
        config.agents.clone(),
        personas,
        session_mgr,
        skill_registry,
        memory.clone(),
        publisher.clone(),
        Arc::new(NativeExecutor),
        file_store,
        session_writer,
        session_reader,
        search_index,
        embedding_provider,
        workspace_dir.clone(),
        brave_api_key,
        Some(root.to_path_buf()),
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
                let api_key = provider_config
                    .api_key
                    .clone()
                    .filter(|k| !k.is_empty())
                    .unwrap_or_else(|| std::env::var(&provider_config.api_key_env).unwrap_or_default());
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
            "openai" => {
                let api_key = provider_config
                    .api_key
                    .clone()
                    .filter(|k| !k.is_empty())
                    .unwrap_or_else(|| std::env::var(&provider_config.api_key_env).unwrap_or_default());
                if !api_key.is_empty() {
                    let provider = Arc::new(OpenAiProvider::new(
                        api_key,
                        provider_config.api_base.clone(),
                    ));
                    registry.register("openai", provider);
                } else {
                    tracing::warn!(
                        "OpenAI API key not set (env: {}), skipping",
                        provider_config.api_key_env
                    );
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
    for provider_config in &config.providers {
        if !provider_config.enabled {
            continue;
        }
        for model in &provider_config.models {
            aliases.insert(
                model.clone(),
                format!("{}/{}", provider_config.provider_id, model),
            );
        }
    }
    aliases
        .entry("sonnet".to_string())
        .or_insert_with(|| "anthropic/claude-sonnet-4-5".to_string());
    aliases
        .entry("haiku".to_string())
        .or_insert_with(|| "anthropic/claude-3-haiku-20240307".to_string());
    aliases
        .entry("opus".to_string())
        .or_insert_with(|| "anthropic/claude-opus-4-6".to_string());

    LlmRouter::new(registry, aliases, vec![])
}

fn build_embedding_provider(config: &NanocrabConfig) -> Arc<dyn EmbeddingProvider> {
    let embedding_config = &config.main.embedding;

    // Check if embedding is disabled or provider is not openai
    if !embedding_config.enabled || embedding_config.provider != "openai" {
        tracing::info!(
            "Embedding provider disabled or not openai (provider: {}), using stub",
            embedding_config.provider
        );
        return Arc::new(StubEmbeddingProvider::new(8));
    }

    // Read API key from environment
    let api_key = std::env::var(&embedding_config.api_key_env).unwrap_or_default();
    if api_key.is_empty() {
        tracing::warn!(
            "OpenAI embedding API key not set (env: {}), using stub provider",
            embedding_config.api_key_env
        );
        return Arc::new(StubEmbeddingProvider::new(8));
    }

    // Create OpenAI embedding provider with configured model and dimensions
    let provider = OpenAiEmbeddingProvider::with_model(
        api_key,
        embedding_config.model.clone(),
        embedding_config.dimensions,
    )
    .with_base_url(embedding_config.base_url.clone());

    tracing::info!(
        "OpenAI embedding provider initialized (model: {}, dimensions: {})",
        embedding_config.model,
        embedding_config.dimensions
    );

    Arc::new(provider)
}

async fn start_bot(root: &Path, with_tui: bool) -> Result<()> {
    let (bus, memory, gateway, config) = bootstrap(root)?;

    let workspace_dir = root.to_path_buf();
    let file_store_for_consolidation =
        nanocrab_memory::file_store::MemoryFileStore::new(&workspace_dir);
    let consolidation_search_index = nanocrab_memory::search_index::SearchIndex::new(memory.db());
    let consolidation_embedding_provider = build_embedding_provider(&config);

    {
        let startup_index = consolidation_search_index.clone();
        let startup_fs = file_store_for_consolidation.clone();
        let startup_ep = consolidation_embedding_provider.clone();
        tokio::task::spawn(async move {
            if let Err(e) = startup_index.ensure_vec_table(startup_ep.dimensions()) {
                tracing::warn!("Failed to ensure vec table at startup: {e}");
                return;
            }
            match startup_index
                .index_all(&startup_fs, startup_ep.as_ref())
                .await
            {
                Ok(count) => {
                    if count > 0 {
                        tracing::info!("Startup indexing: {count} chunks indexed");
                    }
                }
                Err(e) => tracing::warn!("Startup indexing failed: {e}"),
            }
        });
    }

    let consolidator = Arc::new(
        HippocampusConsolidator::new(
            file_store_for_consolidation.clone(),
            Arc::new(build_router_from_config(&config)),
            "sonnet".to_string(),
            vec!["haiku".to_string()],
        )
        .with_search_index(consolidation_search_index)
        .with_embedding_provider(consolidation_embedding_provider)
        .with_file_store_for_reindex(file_store_for_consolidation),
    );
    let scheduler = ConsolidationScheduler::new(consolidator, 24);
    let _consolidation_handle = scheduler.start();
    tracing::info!("Hippocampus consolidation scheduler started (every 24h)");

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

    let mut bots: Vec<Box<dyn ChannelBot>> = Vec::new();

    if let Some(tg_config) = &config.main.channels.telegram {
        if tg_config.enabled {
            for connector in &tg_config.connectors {
                let token = resolve_env_var(&connector.token);
                if token.is_empty() {
                    tracing::warn!(
                        "Telegram token is empty for connector {}, skipping",
                        connector.connector_id
                    );
                    continue;
                }
                tracing::info!("Registering Telegram bot: {}", connector.connector_id);
                bots.push(Box::new(TelegramBot::new(
                    token,
                    connector.connector_id.clone(),
                    gateway.clone(),
                )));
            }
        }
    }

    if let Some(dc_config) = &config.main.channels.discord {
        if dc_config.enabled {
            for connector in &dc_config.connectors {
                let token = resolve_env_var(&connector.token);
                if token.is_empty() {
                    tracing::warn!(
                        "Discord token is empty for connector {}, skipping",
                        connector.connector_id
                    );
                    continue;
                }
                tracing::info!("Registering Discord bot: {}", connector.connector_id);
                bots.push(Box::new(DiscordBot::new(
                    token,
                    connector.connector_id.clone(),
                    gateway.clone(),
                )));
            }
        }
    }

    if bots.is_empty() {
        anyhow::bail!("No channel bots configured or enabled. Check main.yaml channels section.");
    }

    tracing::info!("Starting {} channel bot(s)", bots.len());

    if bots.len() == 1 {
        let bot = bots.into_iter().next().unwrap();
        tracing::info!(
            "Starting {} bot: {}",
            bot.channel_type(),
            bot.connector_id()
        );
        bot.run().await?;
    } else {
        let mut handles = Vec::new();
        for bot in bots {
            let channel = bot.channel_type().to_string();
            let connector = bot.connector_id().to_string();
            handles.push(tokio::spawn(async move {
                tracing::info!("Starting {channel} bot: {connector}");
                if let Err(err) = bot.run().await {
                    tracing::error!("{channel} bot ({connector}) exited with error: {err}");
                }
            }));
        }
        for handle in handles {
            let _ = handle.await;
        }
    }

    Ok(())
}

async fn run_consolidate(root: &Path) -> Result<()> {
    let (_bus, memory, _gateway, config) = bootstrap(root)?;

    let workspace_dir = root.to_path_buf();
    let file_store = nanocrab_memory::file_store::MemoryFileStore::new(&workspace_dir);
    let consolidation_search_index = nanocrab_memory::search_index::SearchIndex::new(memory.db());
    let consolidation_embedding_provider = build_embedding_provider(&config);
    let consolidator = Arc::new(
        HippocampusConsolidator::new(
            file_store.clone(),
            Arc::new(build_router_from_config(&config)),
            "sonnet".to_string(),
            vec!["haiku".to_string()],
        )
        .with_search_index(consolidation_search_index)
        .with_embedding_provider(consolidation_embedding_provider)
        .with_file_store_for_reindex(file_store),
    );

    let scheduler = ConsolidationScheduler::new(consolidator, 24);
    println!("Running hippocampus consolidation...");
    let report = scheduler.run_once().await?;
    println!("Consolidation complete:");
    println!("  Daily files read: {}", report.daily_files_read);
    println!("  Memory updated: {}", report.memory_updated);
    println!("  Reindexed: {}", report.reindexed);
    println!("  Summary: {}", report.summary);
    Ok(())
}

async fn run_repl(root: &Path, _agent_id: &str) -> Result<()> {
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

    #[test]
    fn parses_auth_status_subcommand() {
        let cli = Cli::try_parse_from(["nanocrab", "auth", "status"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Auth(AuthCommands::Status)
        ));
    }

    #[test]
    fn parses_auth_login_openai_subcommand() {
        let cli = Cli::try_parse_from(["nanocrab", "auth", "login", "openai"]).unwrap();
        assert!(matches!(
            cli.command,
            Commands::Auth(AuthCommands::Login { .. })
        ));
    }
}
