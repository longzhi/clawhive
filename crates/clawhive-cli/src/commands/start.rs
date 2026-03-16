use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::Result;

use clawhive_channels::dingtalk::DingTalkBot;
use clawhive_channels::discord::DiscordBot;
use clawhive_channels::feishu::FeishuBot;
use clawhive_channels::imessage::IMessageBot;
use clawhive_channels::slack::{SlackBot, SlackBotConfig};
use clawhive_channels::telegram::TelegramBot;
use clawhive_channels::wecom::WeComBot;
use clawhive_channels::ChannelBot;
use clawhive_core::heartbeat::{is_heartbeat_ack, should_skip_heartbeat, DEFAULT_HEARTBEAT_PROMPT};
use clawhive_core::*;
use clawhive_gateway::supervisor::{BotFactory, ChannelSupervisor};
use clawhive_gateway::{
    spawn_approval_delivery_listener, spawn_scheduled_task_listener, spawn_wait_task_listener,
    ReloadCoordinator,
};

use crate::runtime::bootstrap::{bootstrap, build_embedding_provider, build_router_from_config};
use crate::runtime::pid::{
    check_and_clean_pid, is_process_running, read_pid_file, remove_pid_file, remove_port_file,
    write_pid_file, write_port_file,
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
        remove_port_file(root);
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
            remove_port_file(root);
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
    remove_port_file(root);
    println!("Killed.");
    Ok(true)
}

fn build_bot_factory() -> BotFactory {
    Arc::new(|channel_type, config, gateway, bus| match channel_type {
        "telegram" => {
            let token = config["token"].as_str().unwrap_or_default().to_string();
            let connector_id = config["connector_id"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let require_mention = config["require_mention"].as_bool().unwrap_or(false);
            let bot = TelegramBot::new(token, connector_id, gateway, bus)
                .with_require_mention(require_mention);
            Ok(Box::pin(async move { Box::new(bot).run().await })
                as std::pin::Pin<
                    Box<dyn std::future::Future<Output = Result<()>> + Send + 'static>,
                >)
        }
        "discord" => {
            let token = config["token"].as_str().unwrap_or_default().to_string();
            let connector_id = config["connector_id"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let groups = config["groups"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(ToOwned::to_owned))
                        .collect::<Vec<String>>()
                })
                .unwrap_or_default();
            let require_mention = config["require_mention"].as_bool().unwrap_or(false);
            let bot = DiscordBot::new(token, connector_id, gateway)
                .with_bus(bus)
                .with_groups(groups)
                .with_require_mention(require_mention);
            Ok(Box::pin(async move { Box::new(bot).run().await })
                as std::pin::Pin<
                    Box<dyn std::future::Future<Output = Result<()>> + Send + 'static>,
                >)
        }
        "feishu" => {
            let app_id = config["app_id"].as_str().unwrap_or_default().to_string();
            let app_secret = config["app_secret"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let connector_id = config["connector_id"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let bot = FeishuBot::new(app_id, app_secret, connector_id, gateway, bus);
            Ok(Box::pin(async move { Box::new(bot).run().await })
                as std::pin::Pin<
                    Box<dyn std::future::Future<Output = Result<()>> + Send + 'static>,
                >)
        }
        "dingtalk" => {
            let client_id = config["client_id"].as_str().unwrap_or_default().to_string();
            let client_secret = config["client_secret"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let connector_id = config["connector_id"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let bot = DingTalkBot::new(client_id, client_secret, connector_id, gateway, bus);
            Ok(Box::pin(async move { Box::new(bot).run().await })
                as std::pin::Pin<
                    Box<dyn std::future::Future<Output = Result<()>> + Send + 'static>,
                >)
        }
        "wecom" => {
            let bot_id = config["bot_id"].as_str().unwrap_or_default().to_string();
            let secret = config["secret"].as_str().unwrap_or_default().to_string();
            let connector_id = config["connector_id"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let bot = WeComBot::new(bot_id, secret, connector_id, gateway, bus);
            Ok(Box::pin(async move { Box::new(bot).run().await })
                as std::pin::Pin<
                    Box<dyn std::future::Future<Output = Result<()>> + Send + 'static>,
                >)
        }
        "slack" => {
            let bot_token = config["bot_token"].as_str().unwrap_or_default().to_string();
            let connector_id = config["connector_id"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let slack_config = SlackBotConfig::new(bot_token, connector_id);
            let bot = SlackBot::new(slack_config, gateway);
            Ok(Box::pin(async move { Box::new(bot).run().await })
                as std::pin::Pin<
                    Box<dyn std::future::Future<Output = Result<()>> + Send + 'static>,
                >)
        }
        "whatsapp" => {
            let connector_id = config["connector_id"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let default_db = format!("~/.clawhive/data/whatsapp-{connector_id}.db");
            let raw_db_path = config["db_path"]
                .as_str()
                .unwrap_or(&default_db)
                .to_string();
            let db_path = expand_tilde(&raw_db_path);
            if let Some(parent) = db_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let dm_policy = config["dm_policy"]
                .as_str()
                .unwrap_or("allowlist")
                .to_string();
            let allow_from = config["allow_from"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(ToOwned::to_owned))
                        .collect::<Vec<String>>()
                })
                .unwrap_or_default();
            let group_policy = config["group_policy"]
                .as_str()
                .unwrap_or("disabled")
                .to_string();
            let group_allow_from = config["group_allow_from"]
                .as_array()
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(ToOwned::to_owned))
                        .collect::<Vec<String>>()
                })
                .unwrap_or_default();
            let access_policy = clawhive_channels::whatsapp::AccessPolicy::from_config(
                &dm_policy,
                &allow_from,
                &group_policy,
                &group_allow_from,
            );
            Ok(Box::pin(async move {
                clawhive_channels::whatsapp::start_whatsapp(
                    connector_id,
                    db_path,
                    access_policy,
                    gateway,
                    bus,
                )
                .await
            })
                as std::pin::Pin<
                    Box<dyn std::future::Future<Output = Result<()>> + Send + 'static>,
                >)
        }
        "imessage" => {
            let connector_id = config["connector_id"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let poll_interval = config["poll_interval_secs"].as_u64().unwrap_or(5);
            let bot = IMessageBot::new(connector_id, gateway).with_poll_interval(poll_interval);
            Ok(Box::pin(async move { Box::new(bot).run().await })
                as std::pin::Pin<
                    Box<dyn std::future::Future<Output = Result<()>> + Send + 'static>,
                >)
        }
        _ => Err(anyhow::anyhow!("unknown channel type: {channel_type}")),
    })
}

fn start_configured_bots(
    supervisor: &mut ChannelSupervisor,
    config: &ClawhiveConfig,
) -> Result<usize> {
    let mut started = 0usize;

    if let Some(tg) = &config.main.channels.telegram {
        if tg.enabled {
            for connector in &tg.connectors {
                let config_json = serde_json::to_value(connector)?;
                if let Err(error) =
                    supervisor.start(connector.connector_id.clone(), "telegram", config_json)
                {
                    tracing::error!(connector = %connector.connector_id, "failed to start telegram bot: {error}");
                } else {
                    started += 1;
                }
            }
        }
    }

    if let Some(dc) = &config.main.channels.discord {
        if dc.enabled {
            for connector in &dc.connectors {
                let config_json = serde_json::to_value(connector)?;
                if let Err(error) =
                    supervisor.start(connector.connector_id.clone(), "discord", config_json)
                {
                    tracing::error!(connector = %connector.connector_id, "failed to start discord bot: {error}");
                } else {
                    started += 1;
                }
            }
        }
    }

    if let Some(feishu) = &config.main.channels.feishu {
        if feishu.enabled {
            for connector in &feishu.connectors {
                let config_json = serde_json::to_value(connector)?;
                if let Err(error) =
                    supervisor.start(connector.connector_id.clone(), "feishu", config_json)
                {
                    tracing::error!(connector = %connector.connector_id, "failed to start feishu bot: {error}");
                } else {
                    started += 1;
                }
            }
        }
    }

    if let Some(dingtalk) = &config.main.channels.dingtalk {
        if dingtalk.enabled {
            for connector in &dingtalk.connectors {
                let config_json = serde_json::to_value(connector)?;
                if let Err(error) =
                    supervisor.start(connector.connector_id.clone(), "dingtalk", config_json)
                {
                    tracing::error!(connector = %connector.connector_id, "failed to start dingtalk bot: {error}");
                } else {
                    started += 1;
                }
            }
        }
    }

    if let Some(wecom) = &config.main.channels.wecom {
        if wecom.enabled {
            for connector in &wecom.connectors {
                let config_json = serde_json::to_value(connector)?;
                if let Err(error) =
                    supervisor.start(connector.connector_id.clone(), "wecom", config_json)
                {
                    tracing::error!(connector = %connector.connector_id, "failed to start wecom bot: {error}");
                } else {
                    started += 1;
                }
            }
        }
    }

    if let Some(slack) = &config.main.channels.slack {
        if slack.enabled {
            for connector in &slack.connectors {
                let config_json = serde_json::to_value(connector)?;
                if let Err(error) =
                    supervisor.start(connector.connector_id.clone(), "slack", config_json)
                {
                    tracing::error!(connector = %connector.connector_id, "failed to start slack bot: {error}");
                } else {
                    started += 1;
                }
            }
        }
    }

    if let Some(whatsapp) = &config.main.channels.whatsapp {
        if whatsapp.enabled {
            for connector in &whatsapp.connectors {
                let config_json = serde_json::to_value(connector)?;
                if let Err(error) =
                    supervisor.start(connector.connector_id.clone(), "whatsapp", config_json)
                {
                    tracing::error!(connector = %connector.connector_id, "failed to start whatsapp bot: {error}");
                } else {
                    started += 1;
                }
            }
        }
    }

    if let Some(imessage) = &config.main.channels.imessage {
        if imessage.enabled {
            for connector in &imessage.connectors {
                let config_json = serde_json::to_value(connector)?;
                if let Err(error) =
                    supervisor.start(connector.connector_id.clone(), "imessage", config_json)
                {
                    tracing::error!(connector = %connector.connector_id, "failed to start imessage bot: {error}");
                } else {
                    started += 1;
                }
            }
        }
    }

    Ok(started)
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
    write_port_file(root, port)?;
    tracing::info!("PID file written (pid: {})", std::process::id());

    let (bus, memory, gateway, config, schedule_manager, wait_task_manager, approval_registry) =
        bootstrap(root, security_override).await?;

    let workspace_dir = root.to_path_buf();
    let file_store_for_consolidation =
        clawhive_memory::file_store::MemoryFileStore::new(&workspace_dir);
    let session_reader_for_consolidation =
        clawhive_memory::session::SessionReader::new(&workspace_dir);
    let consolidation_search_index = clawhive_memory::search_index::SearchIndex::new(memory.db());
    let consolidation_embedding_provider = build_embedding_provider(&config).await;

    {
        let startup_index = consolidation_search_index.clone();
        let startup_fs = file_store_for_consolidation.clone();
        let startup_reader = clawhive_memory::session::SessionReader::new(&workspace_dir);
        let startup_ep = consolidation_embedding_provider.clone();
        tokio::task::spawn(async move {
            if let Err(e) = startup_index.ensure_vec_table(startup_ep.dimensions()) {
                tracing::warn!("Failed to ensure vec table at startup: {e}");
                return;
            }
            match startup_index
                .index_all(&startup_fs, &startup_reader, startup_ep.as_ref())
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
        .with_file_store_for_reindex(file_store_for_consolidation)
        .with_session_reader_for_reindex(session_reader_for_consolidation),
    );
    let consolidation_interval_hours = config.main.consolidation_interval_hours;
    let scheduler = ConsolidationScheduler::new(consolidator, consolidation_interval_hours);
    let _consolidation_handle = scheduler.start();
    tracing::info!(
        interval_hours = consolidation_interval_hours,
        "Hippocampus consolidation scheduler started"
    );

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

    let bot_factory = build_bot_factory();
    let mut supervisor =
        ChannelSupervisor::new(gateway.clone(), bus.clone()).with_bot_factory(bot_factory);
    let started_bots = start_configured_bots(&mut supervisor, &config)?;
    let supervisor = Arc::new(tokio::sync::Mutex::new(supervisor));
    let reload_coordinator = Arc::new(ReloadCoordinator::new(
        config.clone(),
        Arc::clone(gateway.orchestrator()),
        Arc::clone(&supervisor),
        root.to_path_buf(),
        Arc::clone(&memory),
        bus.publisher(),
        Arc::clone(&schedule_manager),
        Some(Arc::clone(&approval_registry)),
    ));

    #[cfg(unix)]
    {
        let rc = Arc::clone(&reload_coordinator);
        let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
        tokio::spawn(async move {
            loop {
                sighup.recv().await;
                tracing::info!("SIGHUP received, triggering config reload");
                match rc.reload().await {
                    Ok(outcome) => {
                        tracing::info!(generation = outcome.generation, "reload complete")
                    }
                    Err(error) => tracing::error!("reload failed: {error}"),
                }
            }
        });
    }

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
        schedule_manager: Some(Arc::clone(&schedule_manager)),
        reload_coordinator: Some(Arc::clone(&reload_coordinator)),
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

    // Webhook sources (no ChannelBot needed — uses HTTP endpoint in clawhive-server)
    if let Some(webhook_config) = &config.main.channels.webhook {
        if webhook_config.enabled {
            for source in &webhook_config.sources {
                tracing::info!(
                    source_id = %source.source_id,
                    format = %source.format,
                    "Registered webhook source"
                );
            }
        }
    }

    if started_bots == 0 {
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

    let root_for_cleanup = root.to_path_buf();
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
        _ = shutdown_signal => {
            supervisor.lock().await.shutdown_all().await;
            remove_pid_file(&root_for_cleanup);
            remove_port_file(&root_for_cleanup);
            tracing::info!("PID file cleaned up. Goodbye.");
        }
    }

    Ok(())
}

fn expand_tilde(path: &str) -> std::path::PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return std::path::PathBuf::from(home).join(rest);
        }
    }
    std::path::PathBuf::from(path)
}
