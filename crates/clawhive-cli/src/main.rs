use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

mod commands;
mod runtime;
mod setup;
mod setup_scan;
mod setup_ui;

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
};
use clawhive_schema::InboundMessage;
use commands::auth::{handle_auth_command, AuthCommands};
use runtime::bootstrap::{
    bootstrap, build_embedding_provider, build_router_from_config, resolve_security_override,
};
use runtime::pid::{
    check_and_clean_pid, is_process_running, read_pid_file, remove_pid_file, write_pid_file,
};
use runtime::skeleton::ensure_skeleton_config;
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
    Agent(commands::agent::AgentCommands),
    #[command(subcommand, about = "Skill management")]
    Skill(commands::skill::SkillCommands),
    #[command(subcommand, about = "Session management")]
    Session(SessionCommands),
    #[command(subcommand, about = "Task management")]
    Task(TaskCommands),
    #[command(subcommand, about = "Auth management")]
    Auth(AuthCommands),
    #[command(subcommand, about = "Manage scheduled tasks")]
    Schedule(commands::schedule::ScheduleCommands),
    #[command(subcommand, about = "Manage wait tasks (background polling)")]
    Wait(commands::wait::WaitCommands),
    #[command(subcommand, about = "Manage runtime allowlist")]
    Allowlist(commands::allowlist::AllowlistCommands),
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
            commands::validate::run(&cli.config_root)?;
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
            commands::agent::run(cmd, &cli.config_root)?;
        }
        Commands::Skill(cmd) => {
            commands::skill::run(cmd, &cli.config_root).await?;
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
            commands::schedule::run(cmd, &cli.config_root).await?;
        }
        Commands::Wait(cmd) => {
            commands::wait::run(cmd, &cli.config_root).await?;
        }
        Commands::Allowlist(cmd) => {
            commands::allowlist::run(cmd, &cli.config_root)?;
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
            commands::logs::run(&cli.config_root, lines)?;
        }
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
            Commands::Agent(commands::agent::AgentCommands::List)
        ));
    }

    #[test]
    fn parses_skill_list_subcommand() {
        let cli = Cli::try_parse_from(["clawhive", "skill", "list"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Commands::Skill(commands::skill::SkillCommands::List)
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
            Commands::Agent(commands::agent::AgentCommands::Enable { .. })
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
}
