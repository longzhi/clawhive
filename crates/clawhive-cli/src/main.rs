use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::TimeZone;
use clap::{CommandFactory, Parser, Subcommand, ValueEnum};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

mod commands;
mod setup;
mod setup_scan;
mod setup_ui;

use clawhive_auth::{AuthProfile, TokenManager};
use clawhive_bus::EventBus;
use clawhive_channels::dingtalk::DingTalkBot;
use clawhive_channels::discord::DiscordBot;
use clawhive_channels::feishu::FeishuBot;
use clawhive_channels::telegram::TelegramBot;
use clawhive_channels::wecom::WeComBot;
use clawhive_channels::ChannelBot;
use clawhive_core::heartbeat::{is_heartbeat_ack, should_skip_heartbeat, DEFAULT_HEARTBEAT_PROMPT};
use clawhive_core::*;
use clawhive_gateway::{
    spawn_approval_delivery_listener, spawn_scheduled_task_listener, spawn_wait_task_listener,
    Gateway, RateLimitConfig, RateLimiter,
};
use clawhive_memory::embedding::{
    EmbeddingProvider, GeminiEmbeddingProvider, OllamaEmbeddingProvider, OpenAiEmbeddingProvider,
    StubEmbeddingProvider,
};
use clawhive_memory::search_index::SearchIndex;
use clawhive_memory::MemoryStore;
use clawhive_provider::{
    minimax, moonshot, qianfan, qwen, register_builtin_providers, volcengine, zhipu,
    AnthropicProvider, AzureOpenAiProvider, OpenAiChatGptProvider, OpenAiProvider,
    ProviderRegistry,
};
use clawhive_runtime::NativeExecutor;
use clawhive_scheduler::{ScheduleManager, ScheduleType, SqliteStore, WaitTask, WaitTaskManager};
use clawhive_schema::InboundMessage;
use commands::auth::{handle_auth_command, AuthCommands};
use setup::run_setup;
use tokio::time::sleep;

/// Default HTTP API server port.
const DEFAULT_PORT: u16 = 8848;

#[derive(Parser)]
#[command(name = "clawhive", version, about = "clawhive AI agent framework")]
struct Cli {
    #[arg(
        long,
        default_value = "~/.clawhive",
        help = "Config root directory (contains config/ and prompts/)"
    )]
    config_root: PathBuf,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    #[command(about = "Start all configured channel bots and HTTP API server")]
    Start {
        #[arg(long, short = 'd', help = "Run as a background daemon")]
        daemon: bool,
        #[arg(long, help = "Run TUI dashboard in the same process")]
        tui: bool,
        #[arg(long, default_value_t = DEFAULT_PORT, help = "HTTP API server port")]
        port: u16,
        /// Override security mode (overrides agent config)
        #[arg(long, value_name = "MODE")]
        security: Option<SecurityMode>,
        /// Shorthand for --security off
        #[arg(long)]
        no_security: bool,
    },
    #[command(about = "Start clawhive as a background daemon (alias for `start -d`)")]
    Up {
        #[arg(long, default_value_t = DEFAULT_PORT, help = "HTTP API server port")]
        port: u16,
        /// Override security mode (overrides agent config)
        #[arg(long, value_name = "MODE")]
        security: Option<SecurityMode>,
        /// Shorthand for --security off
        #[arg(long)]
        no_security: bool,
    },
    #[command(about = "Show clawhive status")]
    Status,
    #[command(about = "Stop a running clawhive process")]
    Stop,
    #[command(about = "Restart clawhive (stop + start as daemon)")]
    Restart {
        #[arg(long, default_value_t = DEFAULT_PORT, help = "HTTP API server port")]
        port: u16,
        /// Override security mode (overrides agent config)
        #[arg(long, value_name = "MODE")]
        security: Option<SecurityMode>,
        /// Shorthand for --security off
        #[arg(long)]
        no_security: bool,
    },
    #[command(about = "Code mode: open developer TUI")]
    Code {
        #[arg(long, default_value_t = DEFAULT_PORT, help = "HTTP API server port")]
        port: u16,
        /// Override security mode (overrides agent config)
        #[arg(long, value_name = "MODE")]
        security: Option<SecurityMode>,
        /// Shorthand for --security off
        #[arg(long)]
        no_security: bool,
    },
    #[command(about = "Dashboard mode: attach TUI observability panel to running gateway")]
    Dashboard {
        #[arg(long, default_value_t = DEFAULT_PORT, help = "HTTP API server port")]
        port: u16,
    },
    #[command(about = "Local REPL for testing (no Telegram needed)")]
    Chat {
        #[arg(long, default_value = "clawhive-main", help = "Agent ID to use")]
        agent: String,
        /// Override security mode (overrides agent config)
        #[arg(long, value_name = "MODE")]
        security: Option<SecurityMode>,
        /// Shorthand for --security off
        #[arg(long)]
        no_security: bool,
    },
    #[command(about = "Show current configuration (tokens masked)")]
    Config,
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
    #[command(subcommand, about = "Manage wait tasks (background polling)")]
    Wait(WaitCommands),
    #[command(subcommand, about = "Manage runtime allowlist")]
    Allowlist(AllowlistCommands),
    #[command(about = "Interactive configuration manager")]
    Setup {
        #[arg(long, help = "Skip confirmation prompts on reconfigure/remove")]
        force: bool,
    },
    #[command(about = "Update clawhive to the latest version", alias = "upgrade")]
    Update {
        #[arg(long, help = "Check for updates without installing")]
        check: bool,
        #[arg(long, help = "Update channel (alpha, beta, rc, stable)")]
        channel: Option<String>,
        #[arg(long, help = "Install a specific version")]
        version: Option<String>,
        #[arg(long, short = 'y', help = "Skip confirmation prompt")]
        yes: bool,
    },
    #[command(about = "Tail the latest clawhive log file")]
    Logs {
        #[arg(
            long,
            short = 'n',
            default_value = "50",
            help = "Number of lines to show before following"
        )]
        lines: usize,
    },
}

#[derive(serde::Deserialize, serde::Serialize, Default)]
struct AllowlistFile {
    #[serde(default)]
    agents: HashMap<String, AllowlistAgent>,
}

#[derive(serde::Deserialize, serde::Serialize, Default)]
struct AllowlistAgent {
    #[serde(default)]
    exec: Vec<String>,
    #[serde(default)]
    network: Vec<String>,
}

#[derive(Clone, Copy, ValueEnum)]
enum AllowlistType {
    Exec,
    Network,
}

#[derive(Subcommand)]
enum AllowlistCommands {
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

#[derive(Subcommand)]
enum WaitCommands {
    #[command(about = "List all wait tasks")]
    List {
        #[arg(long, help = "Filter by session key")]
        session: Option<String>,
    },
    #[command(about = "Add a new wait task")]
    Add {
        #[arg(help = "Unique task ID")]
        id: String,
        #[arg(long, help = "Session key to notify")]
        session: String,
        #[arg(long, help = "Shell command to check")]
        cmd: String,
        #[arg(long, help = "Success condition (contains:, equals:, regex:, exit:)")]
        condition: String,
        #[arg(long, default_value = "30000", help = "Poll interval in ms")]
        interval: u64,
        #[arg(long, default_value = "600000", help = "Timeout in ms")]
        timeout: u64,
        #[arg(long, help = "Message on success")]
        on_success: Option<String>,
        #[arg(long, help = "Message on failure")]
        on_failure: Option<String>,
        #[arg(long, help = "Message on timeout")]
        on_timeout: Option<String>,
    },
    #[command(about = "Cancel a wait task")]
    Cancel {
        #[arg(help = "Task ID")]
        task_id: String,
    },
    #[command(about = "Show wait task details")]
    Show {
        #[arg(help = "Task ID")]
        task_id: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut cli = Cli::parse();

    // Expand ~ to home directory
    if cli.config_root.starts_with("~") {
        if let Some(home) = std::env::var_os("HOME") {
            cli.config_root = PathBuf::from(home).join(
                cli.config_root
                    .strip_prefix("~")
                    .unwrap_or(&cli.config_root),
            );
        }
    }

    let log_dir = cli.config_root.join("logs");
    std::fs::create_dir_all(&log_dir)?;
    let file_appender = tracing_appender::rolling::daily(&log_dir, "clawhive.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    // Suppress stderr logs when running TUI modes or Logs to avoid corrupting the terminal.
    let is_tui_mode = matches!(
        cli.command,
        Some(Commands::Code { .. })
            | Some(Commands::Dashboard { .. })
            | Some(Commands::Logs { .. })
    );

    if is_tui_mode {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(non_blocking),
            )
            .init();
    } else {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_writer(non_blocking),
            )
            .init();
    }

    let Some(command) = cli.command else {
        Cli::command().print_help()?;
        println!();
        return Ok(());
    };

    match command {
        Commands::Config => {
            commands::config::print_config(&cli.config_root)?;
        }
        Commands::Validate => {
            let config = load_config(&cli.config_root.join("config"))?;
            println!(
                "Config valid. {} agents, {} providers, {} routing bindings.",
                config.agents.len(),
                config.providers.len(),
                config.routing.bindings.len()
            );
        }
        Commands::Start {
            daemon,
            tui,
            port,
            security,
            no_security,
        } => {
            ensure_skeleton_config(&cli.config_root, port)?;
            let security_override = resolve_security_override(security, no_security);
            if daemon {
                daemonize(&cli.config_root, tui, port, security_override)?;
            } else {
                start_bot(&cli.config_root, tui, port, security_override).await?;
            }
        }
        Commands::Up {
            port,
            security,
            no_security,
        } => {
            if let Some(pid) = read_pid_file(&cli.config_root)? {
                if is_process_running(pid) {
                    commands::status::print_status(&cli.config_root);
                    return Ok(());
                }
            }
            ensure_skeleton_config(&cli.config_root, port)?;
            let security_override = resolve_security_override(security, no_security);
            daemonize(&cli.config_root, false, port, security_override)?;
            // Brief pause to let the daemon start and write its PID file
            tokio::time::sleep(Duration::from_millis(800)).await;
            commands::status::print_status_after_start(&cli.config_root);
        }
        Commands::Status => {
            commands::status::print_status(&cli.config_root);
        }
        Commands::Stop => {
            stop_process(&cli.config_root)?;
        }
        Commands::Restart {
            port,
            security,
            no_security,
        } => {
            let was_running = stop_process(&cli.config_root)?;
            if was_running {
                // Brief pause to let ports release
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            ensure_skeleton_config(&cli.config_root, port)?;
            let security_override = resolve_security_override(security, no_security);
            daemonize(&cli.config_root, false, port, security_override)?;
            tokio::time::sleep(Duration::from_millis(800)).await;
            commands::status::print_status_after_start(&cli.config_root);
        }
        Commands::Code {
            port,
            security,
            no_security,
        } => {
            let security_override = resolve_security_override(security, no_security);
            run_code_tui(&cli.config_root, port, security_override).await?;
        }
        Commands::Dashboard { port } => {
            run_dashboard_tui(port).await?;
        }
        Commands::Chat {
            agent,
            security,
            no_security,
        } => {
            let security_override = resolve_security_override(security, no_security);
            run_repl(&cli.config_root, &agent, security_override).await?;
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
                    let resolved =
                        clawhive_core::skill_install::resolve_skill_source(&source).await?;
                    let report =
                        clawhive_core::skill_install::analyze_skill_source(resolved.local_path())?;
                    println!(
                        "{}",
                        clawhive_core::skill_install::render_skill_analysis(&report)
                    );
                }
                SkillCommands::Install { source, yes } => {
                    let resolved =
                        clawhive_core::skill_install::resolve_skill_source(&source).await?;
                    let report =
                        clawhive_core::skill_install::analyze_skill_source(resolved.local_path())?;
                    println!(
                        "{}",
                        clawhive_core::skill_install::render_skill_analysis(&report)
                    );

                    let high_risk = clawhive_core::skill_install::has_high_risk_findings(&report);
                    let mut proceed = yes;
                    if !yes {
                        proceed = dialoguer::Confirm::new()
                            .with_prompt(
                                "Install this skill with the above permissions/risk profile?",
                            )
                            .default(false)
                            .interact()?;
                        if !proceed {
                            println!("Installation cancelled.");
                        }

                        if proceed
                            && high_risk
                            && !dialoguer::Confirm::new()
                                .with_prompt("High-risk patterns detected. Confirm install anyway?")
                                .default(false)
                                .interact()?
                        {
                            println!("Installation cancelled due to risk findings.");
                            proceed = false;
                        }
                    }

                    if proceed {
                        let installed = clawhive_core::skill_install::install_skill_from_analysis(
                            &cli.config_root,
                            &cli.config_root.join("skills"),
                            resolved.local_path(),
                            &report,
                            yes || high_risk,
                        )?;
                        println!(
                            "Installed skill '{}' to {}",
                            report.skill_name,
                            installed.target.display()
                        );
                    }
                }
            }
        }
        Commands::Session(cmd) => {
            let (
                _bus,
                memory,
                _gateway,
                _config,
                _schedule_manager,
                _wait_manager,
                _approval_registry,
            ) = bootstrap(&cli.config_root, None).await?;
            let session_mgr = SessionManager::new(memory, 1800);
            match cmd {
                SessionCommands::Reset { session_key } => {
                    let key = clawhive_schema::SessionKey(session_key.clone());
                    match session_mgr.reset(&key).await? {
                        true => println!("Session '{session_key}' reset successfully."),
                        false => println!("Session '{session_key}' not found."),
                    }
                }
            }
        }
        Commands::Task(cmd) => {
            let (
                _bus,
                _memory,
                gateway,
                _config,
                _schedule_manager,
                _wait_manager,
                _approval_registry,
            ) = bootstrap(&cli.config_root, None).await?;
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
                        message_id: None,
                        attachments: vec![],
                        group_context: None,
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
            let (
                _bus,
                _memory,
                _gateway,
                _config,
                schedule_manager,
                _wait_manager,
                _approval_registry,
            ) = bootstrap(&cli.config_root, None).await?;
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
        Commands::Wait(cmd) => {
            let db_path = cli.config_root.join("data/scheduler.db");
            let store = Arc::new(SqliteStore::open(&db_path)?);
            let bus = Arc::new(EventBus::new(256));
            let wait_manager = WaitTaskManager::new(store.clone(), bus);

            match cmd {
                WaitCommands::List { session } => {
                    let tasks = if let Some(session_key) = session {
                        wait_manager.list_by_session(&session_key).await?
                    } else {
                        // Load all pending tasks
                        store.load_pending_wait_tasks().await?
                    };

                    if tasks.is_empty() {
                        println!("No wait tasks found.");
                    } else {
                        println!(
                            "{:<20} {:<12} {:<30} {:<20}",
                            "ID", "STATUS", "CONDITION", "SESSION"
                        );
                        println!("{}", "-".repeat(82));
                        for task in tasks {
                            println!(
                                "{:<20} {:<12} {:<30} {:<20}",
                                if task.id.len() > 18 {
                                    format!("{}...", &task.id[..15])
                                } else {
                                    task.id
                                },
                                format!("{:?}", task.status).to_lowercase(),
                                if task.success_condition.len() > 28 {
                                    format!("{}...", &task.success_condition[..25])
                                } else {
                                    task.success_condition
                                },
                                if task.session_key.len() > 18 {
                                    format!("{}...", &task.session_key[..15])
                                } else {
                                    task.session_key
                                },
                            );
                        }
                    }
                }
                WaitCommands::Add {
                    id,
                    session,
                    cmd,
                    condition,
                    interval,
                    timeout,
                    on_success,
                    on_failure,
                    on_timeout,
                } => {
                    let mut task =
                        WaitTask::new(&id, &session, &cmd, &condition, interval, timeout);
                    task.on_success_message = on_success;
                    task.on_failure_message = on_failure;
                    task.on_timeout_message = on_timeout;
                    wait_manager.add(task).await?;
                    println!("Wait task '{id}' created.");
                }
                WaitCommands::Cancel { task_id } => {
                    if wait_manager.cancel(&task_id).await? {
                        println!("Wait task '{task_id}' cancelled.");
                    } else {
                        println!("Wait task '{task_id}' not found or already completed.");
                    }
                }
                WaitCommands::Show { task_id } => match wait_manager.get(&task_id).await? {
                    Some(task) => {
                        println!("ID: {}", task.id);
                        println!("Session: {}", task.session_key);
                        println!("Status: {:?}", task.status);
                        println!("Command: {}", task.check_cmd);
                        println!("Success condition: {}", task.success_condition);
                        if let Some(fc) = &task.failure_condition {
                            println!("Failure condition: {fc}");
                        }
                        println!("Poll interval: {}ms", task.poll_interval_ms);
                        println!(
                            "Timeout at: {}",
                            chrono::Utc
                                .timestamp_millis_opt(task.timeout_at_ms)
                                .single()
                                .map(|dt| dt.to_rfc3339())
                                .unwrap_or_else(|| "-".to_string())
                        );
                        if let Some(last) = task.last_check_at_ms {
                            println!(
                                "Last check: {}",
                                chrono::Utc
                                    .timestamp_millis_opt(last)
                                    .single()
                                    .map(|dt| dt.to_rfc3339())
                                    .unwrap_or_else(|| "-".to_string())
                            );
                        }
                        if let Some(output) = &task.last_output {
                            let preview: String = output.chars().take(200).collect();
                            println!("Last output: {preview}");
                        }
                        if let Some(err) = &task.error {
                            println!("Error: {err}");
                        }
                    }
                    None => {
                        println!("Wait task '{task_id}' not found.");
                    }
                },
            }
        }
        Commands::Allowlist(cmd) => {
            let allowlist_path = cli.config_root.join("data/runtime_allowlist.json");

            match cmd {
                AllowlistCommands::List { agent } => {
                    if !allowlist_path.exists() {
                        println!("No allowlist entries.");
                        return Ok(());
                    }

                    let content = std::fs::read_to_string(&allowlist_path)?;
                    let allowlist: AllowlistFile = serde_json::from_str(&content)
                        .context("Failed to parse runtime_allowlist.json")?;
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
                    let mut allowlist: AllowlistFile = serde_json::from_str(&content)
                        .context("Failed to parse runtime_allowlist.json")?;
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
                                let network_count =
                                    network_before.saturating_sub(entries.network.len());
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
                    allowlist.agents.retain(|_, entries| {
                        !entries.exec.is_empty() || !entries.network.is_empty()
                    });
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
                    let mut allowlist: AllowlistFile = serde_json::from_str(&content)
                        .context("Failed to parse runtime_allowlist.json")?;
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
                    allowlist.agents.retain(|_, entries| {
                        !entries.exec.is_empty() || !entries.network.is_empty()
                    });
                    std::fs::write(&allowlist_path, serde_json::to_string_pretty(&allowlist)?)?;

                    for (agent_id, count) in cleared {
                        println!(
                            "Cleared {count} entr{suffix} for agent '{agent_id}'.",
                            suffix = if count == 1 { "y" } else { "ies" }
                        );
                    }
                }
            }
        }
        Commands::Setup { force } => {
            run_setup(&cli.config_root, force).await?;
        }
        Commands::Update {
            check,
            channel,
            version,
            yes,
        } => {
            commands::update::handle_update(check, channel, version, yes).await?;
        }
        Commands::Logs { lines } => {
            let log_dir = cli.config_root.join("logs");
            if !log_dir.exists() {
                anyhow::bail!(
                    "Log directory not found: {}. Has clawhive been started yet?",
                    log_dir.display()
                );
            }

            // Collect all files that start with "clawhive.log"
            let mut log_files: Vec<PathBuf> = std::fs::read_dir(&log_dir)?
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.path())
                .filter(|path| {
                    path.file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n.starts_with("clawhive.log"))
                        .unwrap_or(false)
                })
                .collect();

            if log_files.is_empty() {
                anyhow::bail!("No log files found in {}", log_dir.display());
            }

            // Daily rolling files are named "clawhive.log.YYYY-MM-DD",
            // so lexicographic sort gives us the latest file last.
            log_files.sort();
            let latest = log_files.last().unwrap();

            eprintln!("Following: {}", latest.display());

            let status = std::process::Command::new("tail")
                .arg(format!("-n{lines}"))
                .arg("-f")
                .arg(latest)
                .status()
                .context("Failed to run `tail`. Make sure it is installed.")?;

            if !status.success() {
                anyhow::bail!("`tail` exited with status: {status}");
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

fn resolve_security_override(
    security: Option<SecurityMode>,
    no_security: bool,
) -> Option<SecurityMode> {
    if no_security {
        Some(SecurityMode::Off)
    } else {
        security
    }
}

#[allow(clippy::type_complexity)]
async fn bootstrap(
    root: &Path,
    security_override: Option<SecurityMode>,
) -> Result<(
    Arc<EventBus>,
    Arc<MemoryStore>,
    Arc<Gateway>,
    ClawhiveConfig,
    Arc<ScheduleManager>,
    Arc<WaitTaskManager>,
    Arc<ApprovalRegistry>,
)> {
    let mut config = load_config(&root.join("config"))?;

    if let Some(mode) = security_override {
        for agent in &mut config.agents {
            agent.security = mode.clone();
        }
        if mode == SecurityMode::Off {
            tracing::warn!(
                "⚠️  Security disabled via --no-security flag. All security checks are OFF."
            );
            eprintln!(
                "⚠️  WARNING: Security disabled. All security checks (HardBaseline, approval, sandbox restrictions) are OFF."
            );
        }
    }

    let db_path = root.join("data/clawhive.db");
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let memory = Arc::new(MemoryStore::open(
        db_path.to_str().unwrap_or("data/clawhive.db"),
    )?);

    let router = build_router_from_config(&config);

    // Initialize peer registry by scanning workspaces
    let workspaces_root = root.join("workspaces");
    let peer_registry = match PeerRegistry::scan_workspaces(&workspaces_root) {
        Ok(registry) => {
            tracing::info!("Discovered {} peer agents", registry.len());
            registry
        }
        Err(e) => {
            tracing::warn!("Failed to scan workspaces for peers: {e}");
            PeerRegistry::new()
        }
    };

    // Load personas from workspace directories (OpenClaw-style)
    let mut personas = HashMap::new();
    for agent_config in &config.agents {
        let identity = agent_config.identity.as_ref();
        let name = identity
            .map(|i| i.name.as_str())
            .unwrap_or(&agent_config.agent_id);
        let emoji = identity.and_then(|i| i.emoji.as_deref());

        // Resolve workspace path and ensure prompt templates exist
        let workspace = Workspace::resolve(
            root,
            &agent_config.agent_id,
            agent_config.workspace.as_deref(),
        );
        if let Err(e) = workspace.init_with_defaults().await {
            tracing::warn!(
                "Failed to init workspace for {}: {e}",
                agent_config.agent_id
            );
        }

        match load_persona_from_workspace(workspace.root(), &agent_config.agent_id, name, emoji) {
            Ok(mut persona) => {
                // Inject peers context for multi-agent collaboration
                let peers_md = peer_registry.format_peers_md(&agent_config.agent_id);
                if !peers_md.is_empty() {
                    persona.peers_context = peers_md;
                }
                personas.insert(agent_config.agent_id.clone(), persona);
            }
            Err(e) => {
                tracing::warn!("Failed to load persona for {}: {e}", agent_config.agent_id);
            }
        }
    }

    let bus = Arc::new(EventBus::new(256));
    let publisher = bus.publisher();
    let new_path = root.join("data/runtime_allowlist.json");
    let old_path = root.join("data/exec_allowlist.json");
    if !new_path.exists() && old_path.exists() {
        if let Err(e) = std::fs::rename(&old_path, &new_path) {
            tracing::warn!("Failed to migrate exec_allowlist.json to runtime_allowlist.json: {e}");
        } else {
            tracing::info!("Migrated exec_allowlist.json -> runtime_allowlist.json");
        }
    }
    let approval_registry = Arc::new(ApprovalRegistry::with_persistence(new_path));
    let schedule_manager = Arc::new(ScheduleManager::new(
        &root.join("config/schedules.d"),
        &root.join("data/schedules"),
        Arc::clone(&bus),
    )?);

    // Initialize SQLite store for wait tasks
    let scheduler_db_path = root.join("data/scheduler.db");
    let sqlite_store = Arc::new(SqliteStore::open(&scheduler_db_path)?);
    let wait_task_manager = Arc::new(WaitTaskManager::new(
        Arc::clone(&sqlite_store),
        Arc::clone(&bus),
    ));
    let session_mgr = SessionManager::new(memory.clone(), 1800);
    let skill_registry = SkillRegistry::load_from_dir(&root.join("skills")).unwrap_or_else(|e| {
        tracing::warn!("Failed to load skills: {e}");
        SkillRegistry::new()
    });
    let workspace_dir = root.to_path_buf();
    let file_store = clawhive_memory::file_store::MemoryFileStore::new(&workspace_dir);
    let session_writer = clawhive_memory::SessionWriter::new(&workspace_dir);
    let session_reader = clawhive_memory::SessionReader::new(&workspace_dir);
    let search_index = SearchIndex::new(memory.db());
    let embedding_provider = build_embedding_provider(&config).await;

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
        Some(approval_registry.clone()),
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
        Some(approval_registry.clone()),
    ));

    Ok((
        bus,
        memory,
        gateway,
        config,
        schedule_manager,
        wait_task_manager,
        approval_registry,
    ))
}

fn build_router_from_config(config: &ClawhiveConfig) -> LlmRouter {
    let token_manager = TokenManager::new().ok();
    let active_profile = token_manager
        .as_ref()
        .and_then(|m| m.get_active_profile().ok().flatten());

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

        // Resolve OAuth profile: named auth_profile takes priority, then fallback to active_profile
        let named_profile = provider_config.auth_profile.as_ref().and_then(|name| {
            token_manager
                .as_ref()
                .and_then(|m| m.get_profile(name).ok().flatten())
        });

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

                // Resolve the effective OAuth profile for this provider
                let oauth_profile = named_profile.clone().or_else(|| {
                    active_profile.as_ref().and_then(|p| match p {
                        AuthProfile::OpenAiOAuth { .. } => Some(p.clone()),
                        _ => None,
                    })
                });

                if !api_key.is_empty() {
                    // Standard API key path — use chat/completions
                    let provider = Arc::new(OpenAiProvider::new_with_auth(
                        api_key,
                        provider_config.api_base.clone(),
                        oauth_profile,
                    ));
                    registry.register("openai", provider);
                } else if let Some(AuthProfile::OpenAiOAuth {
                    access_token,
                    chatgpt_account_id,
                    ..
                }) = &oauth_profile
                {
                    // Backward compat: openai config with no api_key but has OAuth → ChatGPT provider
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
            "openai-chatgpt" => {
                // Dedicated ChatGPT OAuth provider
                let oauth_profile = named_profile.clone().or_else(|| {
                    active_profile.as_ref().and_then(|p| match p {
                        AuthProfile::OpenAiOAuth { .. } => Some(p.clone()),
                        _ => None,
                    })
                });

                if let Some(AuthProfile::OpenAiOAuth {
                    access_token,
                    chatgpt_account_id,
                    ..
                }) = &oauth_profile
                {
                    let provider = Arc::new(OpenAiChatGptProvider::new(
                        access_token.clone(),
                        chatgpt_account_id.clone(),
                        provider_config.api_base.clone(),
                    ));
                    registry.register("openai-chatgpt", provider);
                    tracing::info!(
                        "openai-chatgpt registered via OAuth (account: {:?})",
                        chatgpt_account_id
                    );
                } else {
                    tracing::warn!("openai-chatgpt: no OAuth profile found, skipping");
                }
            }
            "azure-openai" => {
                let api_key = provider_config.api_key.clone().filter(|k| !k.is_empty());
                if let Some(api_key) = api_key {
                    let provider = Arc::new(AzureOpenAiProvider::new(
                        api_key,
                        provider_config.api_base.clone(),
                    ));
                    registry.register("azure-openai", provider);
                } else {
                    tracing::warn!("Azure OpenAI: no API key set, skipping");
                }
            }
            "qwen" => {
                let api_key = provider_config.api_key.clone().filter(|k| !k.is_empty());
                if let Some(api_key) = api_key {
                    let provider = Arc::new(qwen(api_key));
                    registry.register("qwen", provider);
                } else {
                    tracing::warn!("Qwen: no API key set, skipping");
                }
            }
            "moonshot" => {
                let api_key = provider_config.api_key.clone().filter(|k| !k.is_empty());
                if let Some(api_key) = api_key {
                    let provider = Arc::new(moonshot(api_key));
                    registry.register("moonshot", provider);
                } else {
                    tracing::warn!("Moonshot: no API key set, skipping");
                }
            }
            "zhipu" => {
                let api_key = provider_config.api_key.clone().filter(|k| !k.is_empty());
                if let Some(api_key) = api_key {
                    let provider = Arc::new(zhipu(api_key));
                    registry.register("zhipu", provider);
                } else {
                    tracing::warn!("Zhipu: no API key set, skipping");
                }
            }
            "minimax" => {
                let api_key = provider_config.api_key.clone().filter(|k| !k.is_empty());
                if let Some(api_key) = api_key {
                    let provider = Arc::new(minimax(api_key));
                    registry.register("minimax", provider);
                } else {
                    tracing::warn!("MiniMax: no API key set, skipping");
                }
            }
            "volcengine" => {
                let api_key = provider_config.api_key.clone().filter(|k| !k.is_empty());
                if let Some(api_key) = api_key {
                    let provider = Arc::new(volcengine(api_key));
                    registry.register("volcengine", provider);
                } else {
                    tracing::warn!("Volcengine: no API key set, skipping");
                }
            }
            "qianfan" => {
                let api_key = provider_config.api_key.clone().filter(|k| !k.is_empty());
                if let Some(api_key) = api_key {
                    let provider = Arc::new(qianfan(api_key));
                    registry.register("qianfan", provider);
                } else {
                    tracing::warn!("Qianfan: no API key set, skipping");
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
    // Anthropic model aliases: short names → latest models
    aliases
        .entry("sonnet".to_string())
        .or_insert_with(|| "anthropic/claude-sonnet-4-6".to_string());
    aliases
        .entry("haiku".to_string())
        .or_insert_with(|| "anthropic/claude-haiku-4-5".to_string());
    aliases
        .entry("opus".to_string())
        .or_insert_with(|| "anthropic/claude-opus-4-6".to_string());

    // Anthropic model aliases: bare model IDs (without provider prefix) → fully qualified
    for model_id in &[
        "claude-opus-4-6",
        "claude-sonnet-4-6",
        "claude-haiku-4-5",
        "claude-haiku-4-5-20251001",
        "claude-sonnet-4-5",
        "claude-sonnet-4-5-20250929",
        "claude-opus-4-5",
        "claude-opus-4-5-20251101",
        "claude-opus-4-1",
        "claude-opus-4-1-20250805",
        "claude-sonnet-4-0",
        "claude-sonnet-4-20250514",
        "claude-opus-4-0",
        "claude-opus-4-20250514",
        "claude-3-haiku-20240307",
    ] {
        aliases
            .entry(model_id.to_string())
            .or_insert_with(|| format!("anthropic/{model_id}"));
    }
    // Use gpt-5.3-codex for ChatGPT OAuth compatibility (Codex Responses API)
    // gpt-4o-mini and other non-Codex models are not supported via ChatGPT OAuth
    aliases
        .entry("gpt".to_string())
        .or_insert_with(|| "openai/gpt-5.3-codex".to_string());
    aliases
        .entry("chatgpt".to_string())
        .or_insert_with(|| "openai-chatgpt/gpt-5.3-codex".to_string());

    let mut global_fallbacks = Vec::new();
    if registry.get("openai").is_ok() {
        global_fallbacks.push("gpt".to_string());
    }
    if registry.get("openai-chatgpt").is_ok() {
        global_fallbacks.push("chatgpt".to_string());
    }

    LlmRouter::new(registry, aliases, global_fallbacks)
}

async fn build_embedding_provider(config: &ClawhiveConfig) -> Arc<dyn EmbeddingProvider> {
    let embedding_config = &config.main.embedding;

    // If explicitly disabled, use stub
    if !embedding_config.enabled {
        tracing::info!("Embedding disabled, using stub provider");
        return Arc::new(StubEmbeddingProvider::new(8));
    }

    // Priority: ollama > openai (explicit key) > openai (reuse provider key) > stub
    match embedding_config.provider.as_str() {
        "ollama" => {
            let provider = OllamaEmbeddingProvider::with_model(
                embedding_config.model.clone(),
                embedding_config.dimensions,
            )
            .with_base_url(embedding_config.base_url.clone());

            if provider.is_available().await {
                tracing::info!(
                    "Ollama embedding provider initialized (model: {}, dimensions: {})",
                    embedding_config.model,
                    embedding_config.dimensions
                );
                return Arc::new(provider);
            }
            tracing::warn!("Ollama not available, falling back");
        }
        "auto" | "" => {
            // Try Ollama first (free, local)
            let ollama = OllamaEmbeddingProvider::new();
            if ollama.is_available().await {
                tracing::info!(
                    "Auto-detected Ollama, using embedding model: {}",
                    ollama.model_id()
                );
                return Arc::new(ollama);
            }
            tracing::debug!("Ollama not available for auto-detection");
        }
        "openai" => {} // Fall through to OpenAI logic below
        "gemini" | "google" => {
            let api_key = embedding_config.api_key.clone();
            if !api_key.is_empty() {
                let provider = GeminiEmbeddingProvider::with_model(
                    api_key,
                    embedding_config.model.clone(),
                    embedding_config.dimensions,
                )
                .with_base_url(embedding_config.base_url.clone());

                tracing::info!(
                    "Gemini embedding provider initialized (model: {}, dimensions: {})",
                    embedding_config.model,
                    embedding_config.dimensions
                );
                return Arc::new(provider);
            }
            tracing::warn!("Gemini embedding API key not set, falling back");
        }
        other => {
            tracing::warn!("Unknown embedding provider '{}', falling back", other);
        }
    }

    // Try explicit embedding API key first
    let api_key = embedding_config.api_key.clone();
    if !api_key.is_empty() {
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
        return Arc::new(provider);
    }

    // Try to reuse API key from configured LLM providers
    // Priority: OpenAI > Gemini (both support embeddings)
    let mut gemini_key: Option<String> = None;

    for p in &config.providers {
        if !p.enabled {
            continue;
        }
        if let Some(ref key) = p.api_key {
            if key.is_empty() {
                continue;
            }

            // OpenAI (direct API only)
            if p.api_base.contains("openai.com") {
                let provider = OpenAiEmbeddingProvider::with_model(
                    key.clone(),
                    "text-embedding-3-small".to_string(),
                    1536,
                )
                .with_base_url(p.api_base.clone());

                tracing::info!("Reusing OpenAI API key for embeddings (text-embedding-3-small)");
                return Arc::new(provider);
            }

            // Gemini / Google
            if p.provider_id == "gemini"
                || p.provider_id == "google"
                || p.api_base.contains("generativelanguage.googleapis.com")
                || p.api_base.contains("google")
            {
                gemini_key = Some(key.clone());
            }
        }
    }

    // Also check env var for Gemini
    if gemini_key.is_none() {
        if let Ok(key) = std::env::var("GEMINI_API_KEY") {
            if !key.is_empty() {
                gemini_key = Some(key);
            }
        }
    }

    if let Some(key) = gemini_key {
        tracing::info!("Using Gemini API key for embeddings (gemini-embedding-001)");
        return Arc::new(GeminiEmbeddingProvider::new(key));
    }

    // No embedding provider available — stub will be used
    // BM25 keyword search will handle memory_search as fallback
    tracing::warn!("No embedding provider available, memory_search will use keyword matching only");
    Arc::new(StubEmbeddingProvider::new(8))
}

// ---------------------------------------------------------------------------
// Skeleton config — first-run bootstrap
// ---------------------------------------------------------------------------

/// If `config/main.yaml` does not exist, create a minimal skeleton so the
/// server can start and present the Web Setup Wizard.
fn ensure_skeleton_config(root: &Path, port: u16) -> Result<()> {
    let config_dir = root.join("config");
    let main_yaml = config_dir.join("main.yaml");

    if main_yaml.exists() {
        return Ok(());
    }

    // Create directory structure
    std::fs::create_dir_all(config_dir.join("agents.d"))?;
    std::fs::create_dir_all(config_dir.join("providers.d"))?;
    std::fs::create_dir_all(config_dir.join("schedules.d"))?;

    // config/main.yaml — channels disabled
    std::fs::write(
        &main_yaml,
        "app:\n  name: clawhive\n\nruntime:\n  max_concurrent: 4\n\nfeatures:\n  multi_agent: true\n  sub_agent: true\n  tui: true\n  cli: true\n\nchannels:\n  telegram:\n    enabled: false\n    connectors: []\n  discord:\n    enabled: false\n    connectors: []\n  feishu:\n    enabled: false\n    connectors: []\n  dingtalk:\n    enabled: false\n    connectors: []\n  wecom:\n    enabled: false\n    connectors: []\n\nembedding:\n  enabled: true\n  provider: auto\n  api_key: \"\"\n  model: text-embedding-3-small\n  dimensions: 1536\n  base_url: https://api.openai.com/v1\n\ntools: {}\n",
    )?;

    // config/routing.yaml
    std::fs::write(
        config_dir.join("routing.yaml"),
        "default_agent_id: clawhive-main\nbindings: []\n",
    )?;

    // config/agents.d/clawhive-main.yaml — placeholder, disabled
    std::fs::write(
        config_dir.join("agents.d/clawhive-main.yaml"),
        "agent_id: clawhive-main\nenabled: false\nidentity:\n  name: \"Clawhive\"\n  emoji: \"\\U0001F41D\"\nmodel_policy:\n  primary: \"\"\n  fallbacks: []\nmemory_policy:\n  mode: \"standard\"\n  write_scope: \"all\"\n",
    )?;

    // Workspace prompt templates (AGENTS.md, SOUL.md, etc.) are created
    // automatically by workspace.init_with_defaults() during agent startup.

    eprintln!();
    eprintln!("  \u{1F41D} First run detected — setup required.");
    eprintln!();
    eprintln!("     Open the Web Setup Wizard to get started:");
    eprintln!();
    eprintln!("       → http://localhost:{port}/setup");
    eprintln!();
    eprintln!("     Or use the CLI wizard: clawhive setup");
    eprintln!();

    Ok(())
}

// ---------------------------------------------------------------------------
// PID file management
// ---------------------------------------------------------------------------

fn pid_file_path(root: &Path) -> PathBuf {
    root.join("clawhive.pid")
}

fn write_pid_file(root: &Path) -> Result<()> {
    let path = pid_file_path(root);
    std::fs::write(&path, std::process::id().to_string())?;
    Ok(())
}

pub(crate) fn read_pid_file(root: &Path) -> Result<Option<u32>> {
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

pub(crate) fn is_process_running(pid: u32) -> bool {
    // kill(pid, 0) checks if process exists without sending a signal
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// Check for stale PID file. Returns error if another instance is running.
fn check_and_clean_pid(root: &Path) -> Result<()> {
    if let Some(pid) = read_pid_file(root)? {
        if is_process_running(pid) {
            anyhow::bail!("clawhive is already running (pid: {pid}). Use 'clawhive stop' first.");
        }
        tracing::info!("Removing stale PID file (pid: {pid}, process not running)");
        remove_pid_file(root);
    }
    Ok(())
}

/// Daemonize clawhive by forking to background
fn daemonize(
    root: &Path,
    tui: bool,
    port: u16,
    security_override: Option<SecurityMode>,
) -> Result<()> {
    use std::process::{Command, Stdio};

    if tui {
        anyhow::bail!("Cannot use --daemon with --tui (TUI requires a terminal)");
    }

    // Get the current executable path
    let exe = std::env::current_exe()?;

    // Prepare log file (append to clawhive.out)
    let log_dir = root.join("logs");
    std::fs::create_dir_all(&log_dir)?;
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("clawhive.out"))?;
    let log_file_err = log_file.try_clone()?;

    // Spawn the process in background
    let mut command = Command::new(&exe);
    command
        .arg("--config-root")
        .arg(root)
        .arg("start")
        .arg("--port")
        .arg(port.to_string());

    match security_override {
        Some(SecurityMode::Off) => {
            command.arg("--no-security");
        }
        Some(SecurityMode::Standard) => {
            command.arg("--security").arg("standard");
        }
        None => {}
    }

    let _child = command
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_file_err))
        .spawn()?;

    Ok(())
}

/// Stop a running clawhive process. Returns Ok(true) if stopped, Ok(false) if not running.
fn stop_process(root: &Path) -> Result<bool> {
    let pid = match read_pid_file(root)? {
        Some(pid) => pid,
        None => {
            println!("No PID file found. clawhive is not running.");
            return Ok(false);
        }
    };

    if !is_process_running(pid) {
        println!("Process {pid} is not running. Cleaning up stale PID file.");
        remove_pid_file(root);
        return Ok(false);
    }

    println!("Stopping clawhive (pid: {pid})...");
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }

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
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
    std::thread::sleep(Duration::from_millis(500));
    remove_pid_file(root);
    println!("Killed.");
    Ok(true)
}

async fn start_bot(
    root: &Path,
    with_tui: bool,
    port: u16,
    security_override: Option<SecurityMode>,
) -> Result<()> {
    // PID file: check stale → write
    check_and_clean_pid(root)?;
    write_pid_file(root)?;
    tracing::info!("PID file written (pid: {})", std::process::id());

    let (bus, memory, gateway, config, schedule_manager, wait_task_manager, approval_registry) =
        bootstrap(root, security_override).await?;

    let workspace_dir = root.to_path_buf();
    let file_store_for_consolidation =
        clawhive_memory::file_store::MemoryFileStore::new(&workspace_dir);
    let consolidation_search_index = clawhive_memory::search_index::SearchIndex::new(memory.db());
    let consolidation_embedding_provider = build_embedding_provider(&config).await;

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

    let wait_manager_for_loop = Arc::clone(&wait_task_manager);
    let _wait_handle = tokio::spawn(async move {
        wait_manager_for_loop.run().await;
    });
    tracing::info!("Wait task manager started");

    let _schedule_listener_handle =
        spawn_scheduled_task_listener(gateway.clone(), Arc::clone(&bus));
    tracing::info!("Scheduled task gateway listener started");

    let _wait_task_listener_handle = spawn_wait_task_listener(gateway.clone(), Arc::clone(&bus));
    tracing::info!("Wait task gateway listener started");

    let _approval_listener_handle = spawn_approval_delivery_listener(Arc::clone(&bus));
    tracing::info!("Approval delivery listener started");

    // Spawn heartbeat tasks for agents with heartbeat enabled
    for agent_config in &config.agents {
        if !agent_config.enabled {
            continue;
        }

        let heartbeat_config = match &agent_config.heartbeat {
            Some(hb) if hb.enabled => hb.clone(),
            _ => continue,
        };

        let agent_id = agent_config.agent_id.clone();
        let agent_id_for_log = agent_id.clone();
        let gateway_clone = gateway.clone();
        let interval_minutes = heartbeat_config.interval_minutes;
        let prompt = heartbeat_config
            .prompt
            .clone()
            .unwrap_or_else(|| DEFAULT_HEARTBEAT_PROMPT.to_string());

        // Get workspace to check HEARTBEAT.md content
        let workspace = Workspace::resolve(root, &agent_id, agent_config.workspace.as_deref());

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(interval_minutes * 60));
            interval.tick().await; // Skip first immediate tick

            loop {
                interval.tick().await;

                // Check if HEARTBEAT.md has meaningful content
                let heartbeat_content = tokio::fs::read_to_string(workspace.heartbeat_md())
                    .await
                    .unwrap_or_default();

                if should_skip_heartbeat(&heartbeat_content) {
                    tracing::debug!(
                        "Skipping heartbeat for {} - no tasks in HEARTBEAT.md",
                        agent_id
                    );
                    continue;
                }

                // Create heartbeat inbound message
                let inbound = clawhive_schema::InboundMessage {
                    trace_id: uuid::Uuid::new_v4(),
                    channel_type: "heartbeat".to_string(),
                    connector_id: "system".to_string(),
                    conversation_scope: format!("heartbeat:{}", agent_id),
                    user_scope: "system".to_string(),
                    text: prompt.clone(),
                    at: chrono::Utc::now(),
                    thread_id: None,
                    is_mention: false,
                    mention_target: None,
                    message_id: None,
                    attachments: vec![],
                    group_context: None,
                };

                tracing::debug!("Sending heartbeat to agent {}", agent_id);

                match gateway_clone.handle_inbound(inbound).await {
                    Ok(outbound) => {
                        if is_heartbeat_ack(&outbound.text, 50) {
                            tracing::debug!("Heartbeat ack from {}", agent_id);
                        } else {
                            tracing::info!(
                                "Heartbeat response from {}: {}",
                                agent_id,
                                outbound.text
                            );

                            // Deliver to agent's last active channel
                            if let Some(target) = gateway_clone.last_active_channel(&agent_id).await
                            {
                                if let Err(e) = gateway_clone
                                    .publish_announce(
                                        &target.channel_type,
                                        &target.connector_id,
                                        &target.conversation_scope,
                                        &outbound.text,
                                    )
                                    .await
                                {
                                    tracing::error!("Failed to deliver heartbeat response: {e}");
                                }
                            } else {
                                tracing::warn!(
                                    "No active channel for {} - heartbeat response not delivered",
                                    agent_id
                                );
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Heartbeat failed for {}: {}", agent_id, e);
                    }
                }
            }
        });

        tracing::info!(
            "Heartbeat started for {} (every {}m)",
            agent_id_for_log,
            interval_minutes
        );
    }

    // Start embedded HTTP API server
    let web_password_hash = std::fs::read_to_string(root.join("config/main.yaml"))
        .ok()
        .and_then(|content| serde_yaml::from_str::<serde_yaml::Value>(&content).ok())
        .and_then(|val| val["web_password_hash"].as_str().map(ToOwned::to_owned));
    let http_state = clawhive_server::state::AppState {
        root: root.to_path_buf(),
        bus: Arc::clone(&bus),
        gateway: Some(gateway.clone()),
        web_password_hash: Arc::new(RwLock::new(web_password_hash)),
        session_store: Arc::new(RwLock::new(HashMap::<String, Instant>::new())),
        daemon_mode: false,
        port,
    };
    let http_addr = format!("0.0.0.0:{port}");
    tokio::spawn(async move {
        if let Err(err) = clawhive_server::serve(http_state, &http_addr).await {
            tracing::error!("HTTP API server exited with error: {err}");
        }
    });
    let _tui_handle = if with_tui {
        let receivers = clawhive_tui::subscribe_all(bus.as_ref()).await;
        Some(tokio::spawn(async move {
            if let Err(err) =
                clawhive_tui::run_tui_from_receivers(receivers, Some(approval_registry)).await
            {
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
                tracing::info!(
                    "Registering Telegram bot: {} (require_mention: {})",
                    connector.connector_id,
                    connector.require_mention
                );
                bots.push(Box::new(
                    TelegramBot::new(
                        token,
                        connector.connector_id.clone(),
                        gateway.clone(),
                        bus.clone(),
                    )
                    .with_require_mention(connector.require_mention),
                ));
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
                tracing::info!(
                    "Registering Discord bot: {} (groups: {}, require_mention: {})",
                    connector.connector_id,
                    if connector.groups.is_empty() {
                        "all".to_string()
                    } else {
                        connector.groups.len().to_string()
                    },
                    connector.require_mention
                );
                bots.push(Box::new(
                    DiscordBot::new(token, connector.connector_id.clone(), gateway.clone())
                        .with_bus(bus.clone())
                        .with_groups(connector.groups.clone())
                        .with_require_mention(connector.require_mention),
                ));
            }
        }
    }
    // Feishu
    if let Some(feishu_config) = &config.main.channels.feishu {
        if feishu_config.enabled {
            for connector in &feishu_config.connectors {
                tracing::info!("Registering Feishu bot: {}", connector.connector_id);
                bots.push(Box::new(FeishuBot::new(
                    connector.app_id.clone(),
                    connector.app_secret.clone(),
                    connector.connector_id.clone(),
                    gateway.clone(),
                    bus.clone(),
                )));
            }
        }
    }

    // DingTalk
    if let Some(dingtalk_config) = &config.main.channels.dingtalk {
        if dingtalk_config.enabled {
            for connector in &dingtalk_config.connectors {
                tracing::info!("Registering DingTalk bot: {}", connector.connector_id);
                bots.push(Box::new(DingTalkBot::new(
                    connector.client_id.clone(),
                    connector.client_secret.clone(),
                    connector.connector_id.clone(),
                    gateway.clone(),
                    bus.clone(),
                )));
            }
        }
    }

    // WeCom
    if let Some(wecom_config) = &config.main.channels.wecom {
        if wecom_config.enabled {
            for connector in &wecom_config.connectors {
                tracing::info!("Registering WeCom bot: {}", connector.connector_id);
                bots.push(Box::new(WeComBot::new(
                    connector.bot_id.clone(),
                    connector.secret.clone(),
                    connector.connector_id.clone(),
                    gateway.clone(),
                    bus.clone(),
                )));
            }
        }
    }

    if bots.is_empty() {
        tracing::warn!("No channel bots configured or enabled. HTTP server is running for setup.");
        eprintln!("  No channel bots configured yet.");
        eprintln!();
        eprintln!("     Complete setup at → http://localhost:{port}/setup");
        // Keep process alive for the HTTP setup wizard — wait for shutdown signal
        let shutdown_signal = async {
            let ctrl_c = tokio::signal::ctrl_c();
            #[cfg(unix)]
            {
                let mut sigterm =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
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
        shutdown_signal.await;
        remove_pid_file(root);
        return Ok(());
    }

    tracing::info!("Starting {} channel bot(s)", bots.len());

    // Run bots with graceful shutdown on SIGTERM/SIGINT
    let root_for_cleanup = root.to_path_buf();
    let bot_future = async {
        if bots.len() == 1 {
            let bot = bots.into_iter().next().unwrap();
            tracing::info!(
                "Starting {} bot: {}",
                bot.channel_type(),
                bot.connector_id()
            );
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
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
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
    let (_bus, memory, _gateway, config, _schedule_manager, _wait_manager, _approval_registry) =
        bootstrap(root, None).await?;

    let workspace_dir = root.to_path_buf();
    let file_store = clawhive_memory::file_store::MemoryFileStore::new(&workspace_dir);
    let consolidation_search_index = clawhive_memory::search_index::SearchIndex::new(memory.db());
    let consolidation_embedding_provider = build_embedding_provider(&config).await;
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

async fn run_dashboard_tui(port: u16) -> Result<()> {
    let base_url = format!("http://127.0.0.1:{port}");
    let metrics_url = format!("{base_url}/api/events/metrics");
    let stream_url = format!("{base_url}/api/events/stream");

    let client = reqwest::Client::new();
    let probe = client
        .get(&metrics_url)
        .timeout(Duration::from_secs(2))
        .send()
        .await;

    match probe {
        Ok(resp) if resp.status().is_success() => {}
        Ok(resp) => {
            anyhow::bail!(
                "Gateway not ready at {base_url} (HTTP {}). Start it first with `clawhive start`.",
                resp.status()
            );
        }
        Err(err) => {
            anyhow::bail!(
                "Cannot connect to Gateway at {base_url}: {err}. Start it first with `clawhive start`."
            );
        }
    }

    let bus = EventBus::new(1024);
    let publisher = bus.publisher();
    let stream_url_bg = stream_url.clone();
    tokio::spawn(async move {
        if let Err(err) = forward_sse_to_bus(stream_url_bg, publisher).await {
            tracing::error!("dev stream relay stopped: {err}");
        }
    });

    clawhive_tui::run_tui(&bus, None).await
}

async fn run_code_tui(
    root: &Path,
    port: u16,
    security_override: Option<SecurityMode>,
) -> Result<()> {
    let _ = port;
    let (bus, _memory, gateway, _config, _schedule_manager, _wait_manager, approval_registry) =
        bootstrap(root, security_override).await?;
    clawhive_tui::run_code_tui(bus.as_ref(), gateway, Some(approval_registry)).await
}

async fn forward_sse_to_bus(
    stream_url: String,
    publisher: clawhive_bus::BusPublisher,
) -> Result<()> {
    let client = reqwest::Client::new();

    loop {
        let response = client
            .get(&stream_url)
            .header("accept", "text/event-stream")
            .send()
            .await;

        let mut response = match response {
            Ok(resp) if resp.status().is_success() => resp,
            Ok(resp) => {
                tracing::warn!("dev stream connect failed: HTTP {}", resp.status());
                sleep(Duration::from_millis(800)).await;
                continue;
            }
            Err(err) => {
                tracing::warn!("dev stream connect error: {err}");
                sleep(Duration::from_millis(800)).await;
                continue;
            }
        };

        let mut buffer = String::new();
        let mut event_data: Vec<String> = Vec::new();

        loop {
            let chunk = response.chunk().await;
            let Some(chunk) = (match chunk {
                Ok(v) => v,
                Err(err) => {
                    tracing::warn!("dev stream read error: {err}");
                    None
                }
            }) else {
                break;
            };

            let text = String::from_utf8_lossy(&chunk);
            buffer.push_str(&text);

            while let Some(pos) = buffer.find('\n') {
                let mut line = buffer[..pos].to_string();
                buffer.drain(..=pos);

                if line.ends_with('\r') {
                    line.pop();
                }

                if line.is_empty() {
                    if !event_data.is_empty() {
                        let payload = event_data.join("\n");
                        event_data.clear();
                        match serde_json::from_str::<clawhive_schema::BusMessage>(&payload) {
                            Ok(msg) => {
                                let _ = publisher.publish(msg).await;
                            }
                            Err(err) => {
                                tracing::warn!("dev stream invalid bus payload: {err}");
                            }
                        }
                    }
                    continue;
                }

                if let Some(rest) = line.strip_prefix("data:") {
                    event_data.push(rest.trim_start().to_string());
                }
            }
        }

        sleep(Duration::from_millis(300)).await;
    }
}

async fn run_repl(
    root: &Path,
    _agent_id: &str,
    security_override: Option<SecurityMode>,
) -> Result<()> {
    let (_bus, _memory, gateway, _config, _schedule_manager, _wait_manager, _approval_registry) =
        bootstrap(root, security_override).await?;

    println!("clawhive REPL. Type 'quit' to exit.");
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
            message_id: None,
            attachments: vec![],
            group_context: None,
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
        let cli = Cli::parse_from(["clawhive", "consolidate"]);
        assert!(matches!(cli.command.unwrap(), Commands::Consolidate));
    }

    #[test]
    fn parses_start_tui_flag() {
        let cli = Cli::try_parse_from(["clawhive", "start", "--tui"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Commands::Start { tui: true, .. }
        ));
    }

    #[test]
    fn parses_start_no_security_flag() {
        let cli = Cli::try_parse_from(["clawhive", "start", "--no-security"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Commands::Start {
                no_security: true,
                ..
            }
        ));
    }

    #[test]
    fn parses_start_security_off() {
        let cli = Cli::try_parse_from(["clawhive", "start", "--security", "off"]).unwrap();
        if let Commands::Start { security, .. } = cli.command.unwrap() {
            assert_eq!(security, Some(SecurityMode::Off));
        } else {
            panic!("expected Start command");
        }
    }

    #[test]
    fn parses_chat_no_security_flag() {
        let cli = Cli::try_parse_from(["clawhive", "chat", "--no-security"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Commands::Chat {
                no_security: true,
                ..
            }
        ));
    }

    #[test]
    fn parses_agent_list_subcommand() {
        let cli = Cli::try_parse_from(["clawhive", "agent", "list"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Commands::Agent(AgentCommands::List)
        ));
    }

    #[test]
    fn parses_skill_list_subcommand() {
        let cli = Cli::try_parse_from(["clawhive", "skill", "list"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Commands::Skill(SkillCommands::List)
        ));
    }

    #[test]
    fn parses_session_reset_subcommand() {
        let cli = Cli::try_parse_from(["clawhive", "session", "reset", "my-session"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Commands::Session(SessionCommands::Reset { .. })
        ));
    }

    #[test]
    fn parses_task_trigger_subcommand() {
        let cli = Cli::try_parse_from(["clawhive", "task", "trigger", "main", "do stuff"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Commands::Task(TaskCommands::Trigger { .. })
        ));
    }

    #[test]
    fn parses_agent_enable_subcommand() {
        let cli = Cli::try_parse_from(["clawhive", "agent", "enable", "my-agent"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Commands::Agent(AgentCommands::Enable { .. })
        ));
    }

    #[test]
    fn parses_auth_status_subcommand() {
        let cli = Cli::try_parse_from(["clawhive", "auth", "status"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Commands::Auth(AuthCommands::Status)
        ));
    }

    #[test]
    fn parses_auth_login_openai_subcommand() {
        let cli = Cli::try_parse_from(["clawhive", "auth", "login", "openai"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
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
        let cli = Cli::try_parse_from(["clawhive", "setup", "--force"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Commands::Setup { force: true }
        ));
    }

    #[test]
    fn parses_stop_subcommand() {
        let cli = Cli::try_parse_from(["clawhive", "stop"]).unwrap();
        assert!(matches!(cli.command.unwrap(), Commands::Stop));
    }

    #[test]
    fn parses_restart_subcommand() {
        let cli = Cli::try_parse_from(["clawhive", "restart"]).unwrap();
        assert!(matches!(cli.command.unwrap(), Commands::Restart { .. }));
    }

    #[test]
    fn parses_restart_with_port() {
        let cli = Cli::try_parse_from(["clawhive", "restart", "--port", "8080"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Commands::Restart { port: 8080, .. }
        ));
    }

    #[test]
    fn parses_dashboard_with_port() {
        let cli = Cli::try_parse_from(["clawhive", "dashboard", "--port", "8081"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Commands::Dashboard { port: 8081 }
        ));
    }

    #[test]
    fn parses_code_with_port() {
        let cli = Cli::try_parse_from(["clawhive", "code", "--port", "8082"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Commands::Code { port: 8082, .. }
        ));
    }

    #[test]
    fn no_args_defaults_to_chat() {
        let cli = Cli::try_parse_from(["clawhive"]).unwrap();
        assert!(cli.command.is_none());
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
        std::fs::write(tmp.path().join("clawhive.pid"), "99999999").unwrap();
        // Should clean up the stale PID file
        check_and_clean_pid(tmp.path()).unwrap();
        assert_eq!(read_pid_file(tmp.path()).unwrap(), None);
    }

    #[test]
    fn check_and_clean_pid_active_fails() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("clawhive.pid"),
            std::process::id().to_string(),
        )
        .unwrap();
        let result = check_and_clean_pid(tmp.path());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already running"));
    }
}
