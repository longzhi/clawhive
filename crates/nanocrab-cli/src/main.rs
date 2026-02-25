use std::collections::HashMap;
use std::io::{Cursor, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};
use chrono::TimeZone;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

mod commands;
mod setup;
mod setup_ui;
mod setup_scan;

use commands::auth::{handle_auth_command, AuthCommands};
use setup::run_setup;
use nanocrab_auth::{AuthProfile, TokenManager};
use nanocrab_bus::EventBus;
use nanocrab_channels::discord::DiscordBot;
use nanocrab_channels::telegram::TelegramBot;
use nanocrab_channels::ChannelBot;
use nanocrab_core::*;
use nanocrab_gateway::{spawn_scheduled_task_listener, Gateway, RateLimitConfig, RateLimiter};
use nanocrab_memory::embedding::{
    EmbeddingProvider, OpenAiEmbeddingProvider, StubEmbeddingProvider,
};
use nanocrab_memory::search_index::SearchIndex;
use nanocrab_memory::MemoryStore;
use nanocrab_provider::{
    register_builtin_providers, AnthropicProvider, OpenAiChatGptProvider, OpenAiProvider,
    ProviderRegistry,
};
use nanocrab_runtime::NativeExecutor;
use nanocrab_scheduler::{ScheduleManager, ScheduleType};
use nanocrab_schema::InboundMessage;

#[derive(Parser)]
#[command(name = "nanocrab", version, about = "nanocrab AI agent framework")]
struct Cli {
    #[arg(
        long,
        default_value = "~/.nanocrab",
        help = "Config root directory (contains config/ and prompts/)"
    )]
    config_root: PathBuf,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    #[command(about = "Start all configured channel bots and HTTP API server")]
    Start {
        #[arg(long, short = 'd', help = "Run as a background daemon")]
        daemon: bool,
        #[arg(long, help = "Run TUI dashboard in the same process")]
        tui: bool,
        #[arg(long, default_value = "3001", help = "HTTP API server port")]
        port: u16,
    },
    #[command(about = "Stop a running nanocrab process")]
    Stop,
    #[command(about = "Restart nanocrab (stop + start)")]
    Restart {
        #[arg(long, short = 'd', help = "Run as a background daemon")]
        daemon: bool,
        #[arg(long, help = "Run TUI dashboard in the same process")]
        tui: bool,
        #[arg(long, default_value = "3001", help = "HTTP API server port")]
        port: u16,
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
    #[command(subcommand, about = "Manage scheduled tasks")]
    Schedule(ScheduleCommands),
    #[command(about = "Interactive configuration manager")]
    Setup {
        #[arg(long, help = "Skip confirmation prompts on reconfigure/remove")]
        force: bool,
    },
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
    #[command(about = "Analyze a skill directory before install")]
    Analyze {
        #[arg(help = "Path to skill directory, or http(s) URL to SKILL.md")]
        source: String,
    },
    #[command(about = "Install a skill with permission/risk confirmation")]
    Install {
        #[arg(help = "Path to skill directory, or http(s) URL to SKILL.md")]
        source: String,
        #[arg(long, help = "Skip confirmation prompts")]
        yes: bool,
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

#[derive(Subcommand)]
enum ScheduleCommands {
    #[command(about = "List all scheduled tasks with status")]
    List,
    #[command(about = "Trigger a scheduled task immediately")]
    Run {
        #[arg(help = "Schedule ID")]
        schedule_id: String,
    },
    #[command(about = "Enable a disabled schedule")]
    Enable {
        #[arg(help = "Schedule ID")]
        schedule_id: String,
    },
    #[command(about = "Disable a schedule")]
    Disable {
        #[arg(help = "Schedule ID")]
        schedule_id: String,
    },
    #[command(about = "Show recent run history for a schedule")]
    History {
        #[arg(help = "Schedule ID")]
        schedule_id: String,
        #[arg(long, default_value = "10")]
        limit: usize,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut cli = Cli::parse();

    // Expand ~ to home directory
    if cli.config_root.starts_with("~") {
        if let Some(home) = std::env::var_os("HOME") {
            cli.config_root = PathBuf::from(home).join(
                cli.config_root.strip_prefix("~").unwrap_or(&cli.config_root),
            );
        }
    }

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
        Commands::Start { daemon, tui, port } => {
            if daemon {
                daemonize(&cli.config_root, tui, port)?;
            } else {
                start_bot(&cli.config_root, tui, port).await?;
            }
        }
        Commands::Stop => {
            stop_process(&cli.config_root)?;
        }
        Commands::Restart { daemon, tui, port } => {
            let was_running = stop_process(&cli.config_root)?;
            if was_running {
                // Brief pause to let ports release
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            if daemon {
                daemonize(&cli.config_root, tui, port)?;
            } else {
                start_bot(&cli.config_root, tui, port).await?;
            }
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
                SkillCommands::Analyze { source } => {
                    let resolved = resolve_skill_source(&source).await?;
                    let report = analyze_skill_source(resolved.local_path())?;
                    print_skill_analysis(&report);
                }
                SkillCommands::Install { source, yes } => {
                    let resolved = resolve_skill_source(&source).await?;
                    let report = analyze_skill_source(resolved.local_path())?;
                    print_skill_analysis(&report);
                    install_skill_with_confirmation(
                        &cli.config_root,
                        &cli.config_root.join("skills"),
                        resolved.local_path(),
                        &report,
                        yes,
                    )?;
                }
            }
        }
        Commands::Session(cmd) => {
            let (_bus, memory, _gateway, _config, _schedule_manager) = bootstrap(&cli.config_root)?;
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
            let (_bus, _memory, gateway, _config, _schedule_manager) =
                bootstrap(&cli.config_root)?;
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
        Commands::Schedule(cmd) => {
            let (_bus, _memory, _gateway, _config, schedule_manager) =
                bootstrap(&cli.config_root)?;
            match cmd {
                ScheduleCommands::List => {
                    let entries = schedule_manager.list().await;
                    println!(
                        "{:<24} {:<8} {:<24} {:<26} {:<8}",
                        "ID", "ENABLED", "SCHEDULE", "NEXT RUN", "ERRORS"
                    );
                    println!("{}", "-".repeat(96));
                    for entry in entries {
                        let next_run = entry
                            .state
                            .next_run_at_ms
                            .and_then(|ms| chrono::Utc.timestamp_millis_opt(ms).single())
                            .map(|dt| dt.to_rfc3339())
                            .unwrap_or_else(|| "-".to_string());
                        println!(
                            "{:<24} {:<8} {:<24} {:<26} {:<8}",
                            entry.config.schedule_id,
                            if entry.config.enabled { "yes" } else { "no" },
                            format_schedule_type(&entry.config.schedule),
                            next_run,
                            entry.state.consecutive_errors,
                        );
                    }
                }
                ScheduleCommands::Run { schedule_id } => {
                    schedule_manager.trigger_now(&schedule_id).await?;
                    println!("Triggered schedule '{schedule_id}'.");
                }
                ScheduleCommands::Enable { schedule_id } => {
                    schedule_manager.set_enabled(&schedule_id, true).await?;
                    println!("Enabled schedule '{schedule_id}'.");
                }
                ScheduleCommands::Disable { schedule_id } => {
                    schedule_manager.set_enabled(&schedule_id, false).await?;
                    println!("Disabled schedule '{schedule_id}'.");
                }
                ScheduleCommands::History { schedule_id, limit } => {
                    let records = schedule_manager.recent_history(&schedule_id, limit).await?;
                    if records.is_empty() {
                        println!("No history for schedule '{schedule_id}'.");
                    } else {
                        for record in records {
                            println!(
                                "{} | {:>6}ms | {:?} | {}",
                                record.started_at.to_rfc3339(),
                                record.duration_ms,
                                record.status,
                                record.error.as_deref().unwrap_or("-"),
                            );
                        }
                    }
                }
            }
        }
        Commands::Setup { force } => {
            run_setup(&cli.config_root, force).await?;
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

fn format_schedule_type(schedule: &ScheduleType) -> String {
    match schedule {
        ScheduleType::Cron { expr, tz } => format!("cron({expr} @ {tz})"),
        ScheduleType::At { at } => format!("at({at})"),
        ScheduleType::Every {
            interval_ms,
            anchor_ms,
        } => match anchor_ms {
            Some(anchor) => format!("every({interval_ms}ms, anchor={anchor})"),
            None => format!("every({interval_ms}ms)"),
        },
    }
}

#[allow(clippy::type_complexity)]
fn bootstrap(
    root: &Path,
) -> Result<(
    Arc<EventBus>,
    Arc<MemoryStore>,
    Arc<Gateway>,
    NanocrabConfig,
    Arc<ScheduleManager>,
)> {
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

    let bus = Arc::new(EventBus::new(256));
    let publisher = bus.publisher();
    let schedule_manager = Arc::new(ScheduleManager::new(
        &root.join("config/schedules.d"),
        &root.join("data/schedules"),
        Arc::clone(&bus),
    )?);
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
        Arc::clone(&schedule_manager),
    ));

    let rate_limiter = RateLimiter::new(RateLimitConfig::default());
    let gateway = Arc::new(Gateway::new(
        orchestrator,
        config.routing.clone(),
        publisher,
        rate_limiter,
    ));

    Ok((bus, memory, gateway, config, schedule_manager))
}

fn build_router_from_config(config: &NanocrabConfig) -> LlmRouter {
    let active_profile = TokenManager::new()
        .ok()
        .and_then(|m| m.get_active_profile().ok().flatten());

    let openai_profile = active_profile.as_ref().and_then(|p| match p {
        AuthProfile::OpenAiOAuth { .. } => Some(p.clone()),
        AuthProfile::ApiKey { provider_id, .. } if provider_id == "openai" => Some(p.clone()),
        _ => None,
    });
    let anthropic_profile = active_profile.as_ref().and_then(|p| match p {
        AuthProfile::AnthropicSession { .. } => Some(p.clone()),
        AuthProfile::ApiKey { provider_id, .. } if provider_id == "anthropic" => Some(p.clone()),
        _ => None,
    });

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
                    .unwrap_or_default();
                if !api_key.is_empty() {
                    let provider = Arc::new(AnthropicProvider::new_with_auth(
                        api_key,
                        provider_config.api_base.clone(),
                        anthropic_profile.clone(),
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
                    .unwrap_or_default();
                if !api_key.is_empty() {
                    // Standard API key path — use chat/completions
                    let provider = Arc::new(OpenAiProvider::new_with_auth(
                        api_key,
                        provider_config.api_base.clone(),
                        openai_profile.clone(),
                    ));
                    registry.register("openai", provider);
                } else if let Some(AuthProfile::OpenAiOAuth {
                    access_token,
                    chatgpt_account_id,
                    ..
                }) = &openai_profile
                {
                    // OAuth path — use ChatGPT Responses API
                    let provider = Arc::new(OpenAiChatGptProvider::new(
                        access_token.clone(),
                        chatgpt_account_id.clone(),
                        provider_config.api_base.clone(),
                    ));
                    registry.register("openai", provider);
                    tracing::info!(
                        "OpenAI registered via ChatGPT OAuth (account: {:?})",
                        chatgpt_account_id
                    );
                } else {
                    tracing::warn!("OpenAI: no API key and no OAuth profile, skipping");
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
    // Use gpt-5.3-codex for ChatGPT OAuth compatibility (Codex Responses API)
    // gpt-4o-mini and other non-Codex models are not supported via ChatGPT OAuth
    aliases
        .entry("gpt".to_string())
        .or_insert_with(|| "openai/gpt-5.3-codex".to_string());

    let mut global_fallbacks = Vec::new();
    if registry.get("openai").is_ok() {
        global_fallbacks.push("gpt".to_string());
    }

    LlmRouter::new(registry, aliases, global_fallbacks)
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

    let api_key = embedding_config.api_key.clone();
    if api_key.is_empty() {
        tracing::warn!("OpenAI embedding API key not set, using stub provider");
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

// ---------------------------------------------------------------------------
// PID file management
// ---------------------------------------------------------------------------

fn pid_file_path(root: &Path) -> PathBuf {
    root.join("nanocrab.pid")
}

fn write_pid_file(root: &Path) -> Result<()> {
    let path = pid_file_path(root);
    std::fs::write(&path, std::process::id().to_string())?;
    Ok(())
}

fn read_pid_file(root: &Path) -> Result<Option<u32>> {
    let path = pid_file_path(root);
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let pid = content.trim().parse::<u32>()?;
            Ok(Some(pid))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn remove_pid_file(root: &Path) {
    let _ = std::fs::remove_file(pid_file_path(root));
}

fn is_process_running(pid: u32) -> bool {
    // kill(pid, 0) checks if process exists without sending a signal
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// Check for stale PID file. Returns error if another instance is running.
fn check_and_clean_pid(root: &Path) -> Result<()> {
    if let Some(pid) = read_pid_file(root)? {
        if is_process_running(pid) {
            anyhow::bail!("nanocrab is already running (pid: {pid}). Use 'nanocrab stop' first.");
        }
        tracing::info!("Removing stale PID file (pid: {pid}, process not running)");
        remove_pid_file(root);
    }
    Ok(())
}

/// Daemonize nanocrab by forking to background
fn daemonize(root: &Path, tui: bool, port: u16) -> Result<()> {
    use std::process::{Command, Stdio};

    if tui {
        anyhow::bail!("Cannot use --daemon with --tui (TUI requires a terminal)");
    }

    // Get the current executable path
    let exe = std::env::current_exe()?;

    // Prepare log file (append to nanocrab.out)
    let log_dir = root.join("logs");
    std::fs::create_dir_all(&log_dir)?;
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("nanocrab.out"))?;
    let log_file_err = log_file.try_clone()?;

    // Spawn the process in background
    let child = Command::new(&exe)
        .arg("--config-root")
        .arg(root)
        .arg("start")
        .arg("--port")
        .arg(port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_file_err))
        .spawn()?;

    println!("nanocrab started in background (pid: {})", child.id());

    Ok(())
}

/// Stop a running nanocrab process. Returns Ok(true) if stopped, Ok(false) if not running.
fn stop_process(root: &Path) -> Result<bool> {
    let pid = match read_pid_file(root)? {
        Some(pid) => pid,
        None => {
            println!("No PID file found. nanocrab is not running.");
            return Ok(false);
        }
    };

    if !is_process_running(pid) {
        println!("Process {pid} is not running. Cleaning up stale PID file.");
        remove_pid_file(root);
        return Ok(false);
    }

    println!("Stopping nanocrab (pid: {pid})...");
    unsafe { libc::kill(pid as i32, libc::SIGTERM); }

    // Wait up to 10s for graceful shutdown
    for _ in 0..20 {
        std::thread::sleep(Duration::from_millis(500));
        if !is_process_running(pid) {
            remove_pid_file(root);
            println!("Stopped.");
            return Ok(true);
        }
    }

    // Force kill
    eprintln!("Process did not exit after 10s, sending SIGKILL...");
    unsafe { libc::kill(pid as i32, libc::SIGKILL); }
    std::thread::sleep(Duration::from_millis(500));
    remove_pid_file(root);
    println!("Killed.");
    Ok(true)
}

async fn start_bot(root: &Path, with_tui: bool, port: u16) -> Result<()> {
    // PID file: check stale → write
    check_and_clean_pid(root)?;
    write_pid_file(root)?;
    tracing::info!("PID file written (pid: {})", std::process::id());

    let (bus, memory, gateway, config, schedule_manager) = bootstrap(root)?;

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

    let schedule_manager_for_loop = Arc::clone(&schedule_manager);
    let _schedule_handle = tokio::spawn(async move {
        schedule_manager_for_loop.run().await;
    });
    tracing::info!("Schedule manager started");

    let _schedule_listener_handle = spawn_scheduled_task_listener(gateway.clone(), Arc::clone(&bus));
    tracing::info!("Scheduled task gateway listener started");


    // Start embedded HTTP API server
    let http_state = nanocrab_server::state::AppState {
        root: root.to_path_buf(),
        bus: Arc::clone(&bus),
    };
    let http_addr = format!("0.0.0.0:{port}");
    tokio::spawn(async move {
        if let Err(err) = nanocrab_server::serve(http_state, &http_addr).await {
            tracing::error!("HTTP API server exited with error: {err}");
        }
    });
    let _tui_handle = if with_tui {
        let receivers = nanocrab_tui::subscribe_all(bus.as_ref()).await;
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
                    bus.clone(),
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
                bots.push(Box::new(
                    DiscordBot::new(
                        token,
                        connector.connector_id.clone(),
                        gateway.clone(),
                    )
                    .with_bus(bus.clone()),
                ));
            }
        }
    }

    if bots.is_empty() {
        anyhow::bail!("No channel bots configured or enabled. Check main.yaml channels section.");
    }

    tracing::info!("Starting {} channel bot(s)", bots.len());

    // Run bots with graceful shutdown on SIGTERM/SIGINT
    let root_for_cleanup = root.to_path_buf();
    let bot_future = async {
        if bots.len() == 1 {
            let bot = bots.into_iter().next().unwrap();
            tracing::info!("Starting {} bot: {}", bot.channel_type(), bot.connector_id());
            bot.run().await
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
            Ok(())
        }
    };

    let shutdown_signal = async {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        {
            let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("failed to install SIGTERM handler");
            tokio::select! {
                _ = ctrl_c => tracing::info!("Received SIGINT, shutting down..."),
                _ = sigterm.recv() => tracing::info!("Received SIGTERM, shutting down..."),
            }
        }
        #[cfg(not(unix))]
        {
            ctrl_c.await.ok();
            tracing::info!("Received SIGINT, shutting down...");
        }
    };

    tokio::select! {
        result = bot_future => {
            remove_pid_file(&root_for_cleanup);
            result?;
        }
        _ = shutdown_signal => {
            remove_pid_file(&root_for_cleanup);
            tracing::info!("PID file cleaned up. Goodbye.");
        }
    }

    Ok(())
}

async fn run_consolidate(root: &Path) -> Result<()> {
    let (_bus, memory, _gateway, config, _schedule_manager) = bootstrap(root)?;

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
    let (_bus, _memory, gateway, _config, _schedule_manager) = bootstrap(root)?;

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

#[derive(Debug)]
enum ResolvedSkillSource {
    Local(PathBuf),
    Remote {
        _temp_dir: tempfile::TempDir,
        path: PathBuf,
    },
}

impl ResolvedSkillSource {
    fn local_path(&self) -> &Path {
        match self {
            Self::Local(p) => p.as_path(),
            Self::Remote { path, .. } => path.as_path(),
        }
    }
}

#[derive(Debug, serde::Deserialize)]
struct InstallSkillFrontmatter {
    name: String,
    description: String,
    #[serde(default)]
    permissions: Option<SkillPermissions>,
}

#[derive(Debug)]
struct SkillRiskFinding {
    severity: &'static str,
    file: PathBuf,
    line: usize,
    pattern: &'static str,
    reason: &'static str,
}

#[derive(Debug)]
struct SkillAnalysisReport {
    source: PathBuf,
    skill_name: String,
    description: String,
    permissions: Option<SkillPermissions>,
    findings: Vec<SkillRiskFinding>,
}

async fn resolve_skill_source(source: &str) -> Result<ResolvedSkillSource> {
    if source.starts_with("http://") || source.starts_with("https://") {
        return download_remote_skill(source).await;
    }

    let local = PathBuf::from(source);
    if !local.exists() {
        anyhow::bail!("skill source does not exist: {}", local.display());
    }
    Ok(ResolvedSkillSource::Local(local))
}

async fn download_remote_skill(url: &str) -> Result<ResolvedSkillSource> {
    const MAX_DOWNLOAD_BYTES: usize = 20 * 1024 * 1024;

    let parsed = reqwest::Url::parse(url)
        .map_err(|e| anyhow::anyhow!("invalid URL '{url}': {e}"))?;
    match parsed.scheme() {
        "http" | "https" => {}
        s => anyhow::bail!("unsupported URL scheme: {s}"),
    }

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()?;

    let resp = client.get(parsed.clone()).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("download failed: HTTP {}", resp.status());
    }

    if let Some(len) = resp.content_length() {
        if len as usize > MAX_DOWNLOAD_BYTES {
            anyhow::bail!(
                "remote file too large: {} bytes (limit {})",
                len,
                MAX_DOWNLOAD_BYTES
            );
        }
    }

    let body = resp.bytes().await?;
    if body.len() > MAX_DOWNLOAD_BYTES {
        anyhow::bail!(
            "remote file too large: {} bytes (limit {})",
            body.len(),
            MAX_DOWNLOAD_BYTES
        );
    }

    let temp = tempfile::tempdir()?;
    let extract_root = temp.path().join("downloaded-skill");
    std::fs::create_dir_all(&extract_root)?;

    let path_lc = parsed.path().to_lowercase();
    if path_lc.ends_with(".zip") {
        extract_zip_bytes(&body, &extract_root)?;
    } else if path_lc.ends_with(".tar.gz") || path_lc.ends_with(".tgz") {
        extract_tar_gz_bytes(&body, &extract_root)?;
    } else if path_lc.ends_with(".tar") {
        extract_tar_bytes(&body, &extract_root)?;
    } else {
        std::fs::write(extract_root.join("SKILL.md"), &body)?;
    }

    let skill_root = find_skill_root(&extract_root)?;

    Ok(ResolvedSkillSource::Remote {
        _temp_dir: temp,
        path: skill_root,
    })
}

fn find_skill_root(root: &Path) -> Result<PathBuf> {
    if root.join("SKILL.md").exists() {
        return Ok(root.to_path_buf());
    }

    let mut hits = Vec::new();
    find_skill_md_recursive(root, &mut hits)?;
    if hits.is_empty() {
        anyhow::bail!("downloaded source does not contain SKILL.md");
    }

    if hits.len() > 1 {
        anyhow::bail!(
            "downloaded source contains multiple SKILL.md files; please provide a single-skill archive"
        );
    }

    let only = hits.into_iter().next().unwrap();
    let parent = only
        .parent()
        .ok_or_else(|| anyhow::anyhow!("invalid SKILL.md path in archive"))?;
    Ok(parent.to_path_buf())
}

fn find_skill_md_recursive(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let p = entry.path();
        if p.is_dir() {
            find_skill_md_recursive(&p, out)?;
        } else if p.file_name().and_then(|s| s.to_str()) == Some("SKILL.md") {
            out.push(p);
        }
    }
    Ok(())
}

fn is_safe_relative_path(path: &Path) -> bool {
    !path.is_absolute()
        && !path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::RootDir | Component::Prefix(_)))
}

fn extract_zip_bytes(bytes: &[u8], output_dir: &Path) -> Result<()> {
    let reader = Cursor::new(bytes);
    let mut archive = zip::ZipArchive::new(reader)?;

    for i in 0..archive.len() {
        let mut f = archive.by_index(i)?;
        let Some(raw_name) = f.enclosed_name().map(|p| p.to_path_buf()) else {
            continue;
        };
        if !is_safe_relative_path(&raw_name) {
            continue;
        }
        let outpath = output_dir.join(raw_name);
        if f.is_dir() {
            std::fs::create_dir_all(&outpath)?;
        } else {
            if let Some(parent) = outpath.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut out = std::fs::File::create(&outpath)?;
            std::io::copy(&mut f, &mut out)?;
        }
    }
    Ok(())
}

fn extract_tar_gz_bytes(bytes: &[u8], output_dir: &Path) -> Result<()> {
    let cursor = Cursor::new(bytes);
    let decoder = flate2::read::GzDecoder::new(cursor);
    let mut archive = tar::Archive::new(decoder);
    unpack_tar_archive(&mut archive, output_dir)
}

fn extract_tar_bytes(bytes: &[u8], output_dir: &Path) -> Result<()> {
    let cursor = Cursor::new(bytes);
    let mut archive = tar::Archive::new(cursor);
    unpack_tar_archive(&mut archive, output_dir)
}

fn unpack_tar_archive<R: std::io::Read>(archive: &mut tar::Archive<R>, output_dir: &Path) -> Result<()> {
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_path_buf();
        if !is_safe_relative_path(&path) {
            continue;
        }
        let outpath = output_dir.join(path);
        if let Some(parent) = outpath.parent() {
            std::fs::create_dir_all(parent)?;
        }
        entry.unpack(&outpath)?;
    }
    Ok(())
}

fn parse_skill_frontmatter(raw: &str) -> Result<InstallSkillFrontmatter> {
    let trimmed = raw.trim_start();
    if !trimmed.starts_with("---") {
        anyhow::bail!("SKILL.md must start with YAML frontmatter (---)");
    }
    let after_first = &trimmed[3..];
    let end = after_first
        .find("---")
        .ok_or_else(|| anyhow::anyhow!("no closing --- for frontmatter"))?;
    let yaml_str = &after_first[..end];
    let fm: InstallSkillFrontmatter =
        serde_yaml::from_str(yaml_str).map_err(|e| anyhow::anyhow!("invalid frontmatter: {e}"))?;
    Ok(fm)
}

fn analyze_skill_source(source: &Path) -> Result<SkillAnalysisReport> {
    let skill_md = source.join("SKILL.md");
    if !skill_md.exists() {
        anyhow::bail!("{} missing SKILL.md", source.display());
    }

    let raw = std::fs::read_to_string(&skill_md)?;
    let fm = parse_skill_frontmatter(&raw)?;

    let mut findings = Vec::new();
    scan_path_recursive(source, &mut findings)?;

    Ok(SkillAnalysisReport {
        source: source.to_path_buf(),
        skill_name: fm.name,
        description: fm.description,
        permissions: fm.permissions,
        findings,
    })
}

fn scan_path_recursive(dir: &Path, findings: &mut Vec<SkillRiskFinding>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.file_name().and_then(|s| s.to_str()) == Some(".git") {
            continue;
        }
        if path.is_dir() {
            scan_path_recursive(&path, findings)?;
            continue;
        }

        if let Ok(text) = std::fs::read_to_string(&path) {
            for (i, line) in text.lines().enumerate() {
                scan_line(&path, i + 1, line, findings);
            }
        }
    }
    Ok(())
}

fn scan_line(path: &Path, line_no: usize, line: &str, findings: &mut Vec<SkillRiskFinding>) {
    let checks: [(&str, &str, &str, &str); 9] = [
        ("critical", "rm -rf /", "dangerous delete", "Destructive filesystem wipe command"),
        ("critical", "mkfs", "disk format", "Potential disk formatting command"),
        ("high", "curl", "remote fetch", "Network fetch command found; verify intent"),
        ("high", "wget", "remote fetch", "Network fetch command found; verify intent"),
        ("high", "| sh", "pipe-to-shell", "Piping content to shell can execute untrusted code"),
        ("high", "base64 -d", "obfuscation", "Potential obfuscated payload decode"),
        ("high", "sudo ", "privilege escalation", "Privilege escalation command detected"),
        ("medium", "~/.ssh", "secret path", "Accessing SSH config/key paths"),
        ("medium", "~/.aws", "secret path", "Accessing cloud credential paths"),
    ];

    let normalized = line.to_lowercase();
    for (severity, pattern, reason, detail) in checks {
        if normalized.contains(&pattern.to_lowercase()) {
            findings.push(SkillRiskFinding {
                severity,
                file: path.to_path_buf(),
                line: line_no,
                pattern,
                reason: detail,
            });
            let _ = reason;
        }
    }
}

fn print_permissions_summary(permissions: &SkillPermissions) {
    println!("Requested permissions:");
    if !permissions.fs.read.is_empty() {
        println!("  fs.read: {}", permissions.fs.read.join(", "));
    }
    if !permissions.fs.write.is_empty() {
        println!("  fs.write: {}", permissions.fs.write.join(", "));
    }
    if !permissions.network.allow.is_empty() {
        println!("  network.allow: {}", permissions.network.allow.join(", "));
    }
    if !permissions.exec.is_empty() {
        println!("  exec: {}", permissions.exec.join(", "));
    }
    if !permissions.env.is_empty() {
        println!("  env: {}", permissions.env.join(", "));
    }
    if !permissions.services.is_empty() {
        println!("  services: {}", permissions.services.keys().cloned().collect::<Vec<_>>().join(", "));
    }
}

fn print_skill_analysis(report: &SkillAnalysisReport) {
    println!("Skill source: {}", report.source.display());
    println!("Skill name: {}", report.skill_name);
    println!("Description: {}", report.description);

    match &report.permissions {
        Some(perms) => {
            println!("Permissions declared in SKILL.md: yes");
            print_permissions_summary(perms);
        }
        None => {
            println!("Permissions declared in SKILL.md: no");
            println!("Effective behavior: default deny-first sandbox policy will be used.");
        }
    }

    if report.findings.is_empty() {
        println!("Risk scan: no obvious unsafe patterns found.");
    } else {
        println!("Risk scan findings ({}):", report.findings.len());
        for f in &report.findings {
            println!(
                "  [{}] {}:{} pattern='{}' {}",
                f.severity,
                f.file.display(),
                f.line,
                f.pattern,
                f.reason
            );
        }
    }
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let target = dst.join(entry.file_name());
        if path.is_dir() {
            copy_dir_recursive(&path, &target)?;
        } else {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(&path, &target)?;
        }
    }
    Ok(())
}

fn install_skill_with_confirmation(
    config_root: &Path,
    skills_root: &Path,
    source: &Path,
    report: &SkillAnalysisReport,
    yes: bool,
) -> Result<()> {
    let target = skills_root.join(&report.skill_name);

    if !yes {
        if !dialoguer::Confirm::new()
            .with_prompt("Install this skill with the above permissions/risk profile?")
            .default(false)
            .interact()?
        {
            println!("Installation cancelled.");
            return Ok(());
        }

        let high_risk = report
            .findings
            .iter()
            .any(|f| f.severity == "high" || f.severity == "critical");

        if high_risk {
            if !dialoguer::Confirm::new()
                .with_prompt("High-risk patterns detected. Confirm install anyway?")
                .default(false)
                .interact()?
            {
                println!("Installation cancelled due to risk findings.");
                return Ok(());
            }
        }
    }

    if target.exists() {
        std::fs::remove_dir_all(&target)?;
    }
    copy_dir_recursive(source, &target)?;
    println!("Installed skill '{}' to {}", report.skill_name, target.display());

    let audit_dir = config_root.join("logs");
    std::fs::create_dir_all(&audit_dir)?;
    let audit_path = audit_dir.join("skill-installs.jsonl");
    let event = serde_json::json!({
        "ts": chrono::Utc::now().to_rfc3339(),
        "skill": report.skill_name,
        "target": target,
        "findings": report.findings.len(),
        "high_risk": report.findings.iter().any(|f| f.severity == "high" || f.severity == "critical"),
        "declared_permissions": report.permissions.is_some(),
    });
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(audit_path)?;
    writeln!(f, "{}", serde_json::to_string(&event)?)?;

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
        assert!(matches!(cli.command, Commands::Start { tui: true, .. }));
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

    #[test]
    fn setup_ui_symbols_exist() {
        let _ = crate::setup_ui::CHECKMARK;
        let _ = crate::setup_ui::ARROW;
        let _ = crate::setup_ui::CRAB;
    }

    #[test]
    fn parses_setup_force_flag() {
        let cli = Cli::try_parse_from(["nanocrab", "setup", "--force"]).unwrap();
        assert!(matches!(cli.command, Commands::Setup { force: true }));
    }

    #[test]
    fn parses_stop_subcommand() {
        let cli = Cli::try_parse_from(["nanocrab", "stop"]).unwrap();
        assert!(matches!(cli.command, Commands::Stop));
    }

    #[test]
    fn parses_restart_subcommand() {
        let cli = Cli::try_parse_from(["nanocrab", "restart"]).unwrap();
        assert!(matches!(cli.command, Commands::Restart { tui: false, .. }));
    }

    #[test]
    fn parses_restart_with_flags() {
        let cli = Cli::try_parse_from(["nanocrab", "restart", "--tui", "--port", "8080"]).unwrap();
        assert!(matches!(cli.command, Commands::Restart { tui: true, port: 8080 }));
    }

    #[test]
    fn pid_file_write_read_remove() {
        let tmp = tempfile::tempdir().unwrap();
        write_pid_file(tmp.path()).unwrap();
        let pid = read_pid_file(tmp.path()).unwrap();
        assert_eq!(pid, Some(std::process::id()));
        remove_pid_file(tmp.path());
        let pid = read_pid_file(tmp.path()).unwrap();
        assert_eq!(pid, None);
    }

    #[test]
    fn read_pid_file_missing_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(read_pid_file(tmp.path()).unwrap(), None);
    }

    #[test]
    fn is_process_running_self() {
        assert!(is_process_running(std::process::id()));
    }

    #[test]
    fn is_process_running_nonexistent() {
        // PID 99999999 almost certainly does not exist
        assert!(!is_process_running(99_999_999));
    }

    #[test]
    fn check_and_clean_pid_stale() {
        let tmp = tempfile::tempdir().unwrap();
        // Write a fake PID that doesn't exist
        std::fs::write(tmp.path().join("nanocrab.pid"), "99999999").unwrap();
        // Should clean up the stale PID file
        check_and_clean_pid(tmp.path()).unwrap();
        assert_eq!(read_pid_file(tmp.path()).unwrap(), None);
    }

    #[test]
    fn check_and_clean_pid_active_fails() {
        let tmp = tempfile::tempdir().unwrap();
        // Write our own PID - it's running
        std::fs::write(tmp.path().join("nanocrab.pid"), std::process::id().to_string()).unwrap();
        let result = check_and_clean_pid(tmp.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already running"));
    }
}
