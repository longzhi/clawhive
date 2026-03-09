use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use chrono::TimeZone;
use clap::Subcommand;

use clawhive_bus::EventBus;
use clawhive_scheduler::{SqliteStore, WaitTask, WaitTaskManager};

#[derive(Subcommand)]
pub(crate) enum WaitCommands {
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

pub(crate) async fn run(cmd: WaitCommands, root: &Path) -> Result<()> {
    let db_path = root.join("data/scheduler.db");
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
            let mut task = WaitTask::new(&id, &session, &cmd, &condition, interval, timeout);
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

    Ok(())
}
