use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::Result;

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

use crate::runtime::bootstrap::{bootstrap, build_embedding_provider, build_router_from_config};
use crate::runtime::pid::{
    check_and_clean_pid, is_process_running, read_pid_file, remove_pid_file, write_pid_file,
};
use crate::runtime::skeleton::ensure_skeleton_config;

pub(crate) async fn run_start(
    root: &Path,
    daemon: bool,
    tui: bool,
    port: u16,
    security_override: Option<SecurityMode>,
) -> Result<()> {
    ensure_skeleton_config(root, port)?;
    if daemon {
        daemonize(root, tui, port, security_override)
    } else {
        start_bot(root, tui, port, security_override).await
    }
}

pub(crate) async fn run_up(
    root: &Path,
    port: u16,
    security_override: Option<SecurityMode>,
) -> Result<()> {
    if let Some(pid) = read_pid_file(root)? {
        if is_process_running(pid) {
            crate::commands::status::print_status(root);
            return Ok(());
        }
    }
    ensure_skeleton_config(root, port)?;
    daemonize(root, false, port, security_override)?;
    // Brief pause to let the daemon start and write its PID file
    tokio::time::sleep(Duration::from_millis(800)).await;
    crate::commands::status::print_status_after_start(root);
    Ok(())
}

pub(crate) fn run_stop(root: &Path) -> Result<()> {
    stop_process(root)?;
    Ok(())
}

pub(crate) async fn run_restart(
    root: &Path,
    port: u16,
    security_override: Option<SecurityMode>,
) -> Result<()> {
    let was_running = stop_process(root)?;
    if was_running {
        // Brief pause to let ports release
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    ensure_skeleton_config(root, port)?;
    daemonize(root, false, port, security_override)?;
    tokio::time::sleep(Duration::from_millis(800)).await;
    crate::commands::status::print_status_after_start(root);
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
    // SAFETY: pid is a valid process ID obtained from the PID file and confirmed running above.
    // SIGTERM is a standard signal; sending it to another process is safe on Unix.
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
    // SAFETY: pid is confirmed still running above and was obtained from the PID file.
    // SIGKILL is used as a last resort after graceful SIGTERM timed out.
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
            Arc::new(build_router_from_config(&config).await),
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
                    message_source: None,
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
        pending_openai_oauth: Arc::new(RwLock::new(HashMap::new())),
        openai_oauth_config: clawhive_server::state::default_openai_oauth_config(),
        enable_openai_oauth_callback_listener: true,
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
