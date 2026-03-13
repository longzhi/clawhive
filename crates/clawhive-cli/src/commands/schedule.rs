use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use chrono::TimeZone;
use clap::Subcommand;
use clawhive_scheduler::{RunRecord, SessionMode};
use uuid::Uuid;

use crate::runtime::bootstrap::{bootstrap, format_schedule_type};
use crate::runtime::pid::{is_process_running, read_pid_file, read_port_file};

const INTERNAL_CLI_TOKEN_HEADER: &str = "x-clawhive-cli-token";
const INTERNAL_CLI_TOKEN_FILE: &str = "data/cli_internal_token";
const DEFAULT_PORT: u16 = 8848;

#[derive(Subcommand)]
pub(crate) enum ScheduleCommands {
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

pub(crate) async fn run(cmd: ScheduleCommands, root: &Path) -> Result<()> {
    match cmd {
        ScheduleCommands::Run { schedule_id } => {
            run_schedule_via_daemon(root, &schedule_id).await?;
            println!("Triggered schedule '{schedule_id}'.");
        }
        ScheduleCommands::List => {
            let (
                _bus,
                _memory,
                _gateway,
                _config,
                schedule_manager,
                _wait_manager,
                _approval_registry,
            ) = bootstrap(root, None).await?;
            let entries = schedule_manager.list().await;
            println!(
                "{:<24} {:<8} {:<10} {:<24} {:<26} {:<8}",
                "ID", "ENABLED", "SESSION", "SCHEDULE", "NEXT RUN", "ERRORS"
            );
            println!("{}", "-".repeat(108));
            for entry in entries {
                let next_run = entry
                    .state
                    .next_run_at_ms
                    .and_then(|ms| chrono::Utc.timestamp_millis_opt(ms).single())
                    .map(|dt| dt.to_rfc3339())
                    .unwrap_or_else(|| "-".to_string());
                println!(
                    "{:<24} {:<8} {:<10} {:<24} {:<26} {:<8}",
                    entry.config.schedule_id,
                    if entry.config.enabled { "yes" } else { "no" },
                    format_session_mode(&entry.config.session_mode),
                    format_schedule_type(&entry.config.schedule),
                    next_run,
                    entry.state.consecutive_errors,
                );
            }
        }
        ScheduleCommands::Enable { schedule_id } => {
            let (
                _bus,
                _memory,
                _gateway,
                _config,
                schedule_manager,
                _wait_manager,
                _approval_registry,
            ) = bootstrap(root, None).await?;
            schedule_manager.set_enabled(&schedule_id, true).await?;
            println!("Enabled schedule '{schedule_id}'.");
        }
        ScheduleCommands::Disable { schedule_id } => {
            let (
                _bus,
                _memory,
                _gateway,
                _config,
                schedule_manager,
                _wait_manager,
                _approval_registry,
            ) = bootstrap(root, None).await?;
            schedule_manager.set_enabled(&schedule_id, false).await?;
            println!("Disabled schedule '{schedule_id}'.");
        }
        ScheduleCommands::History { schedule_id, limit } => {
            let (
                _bus,
                _memory,
                _gateway,
                _config,
                schedule_manager,
                _wait_manager,
                _approval_registry,
            ) = bootstrap(root, None).await?;
            let records = schedule_manager.recent_history(&schedule_id, limit).await?;
            if records.is_empty() {
                println!("No history for schedule '{schedule_id}'.");
            } else {
                for record in records {
                    println!("{}", format_history_record(&record));
                }
            }
        }
    }
    Ok(())
}

fn format_session_mode(mode: &SessionMode) -> &'static str {
    match mode {
        SessionMode::Isolated => "isolated",
        SessionMode::Main => "main",
    }
}

fn format_history_record(record: &RunRecord) -> String {
    let mut out = format!(
        "{} | {:>6}ms | {:?} | {}",
        record.started_at.to_rfc3339(),
        record.duration_ms,
        record.status,
        record.error.as_deref().unwrap_or("-"),
    );

    if let Some(response) = record.response.as_deref().filter(|v| !v.trim().is_empty()) {
        out.push_str(&format!("\nresponse: {}", response.trim()));
    }

    if let Some(session_key) = record
        .session_key
        .as_deref()
        .filter(|v| !v.trim().is_empty())
    {
        out.push_str(&format!("\nsession_key: {}", session_key.trim()));
    }

    out
}

fn build_schedule_run_url(port: u16, schedule_id: &str) -> Result<String> {
    let mut url = reqwest::Url::parse(&format!("http://127.0.0.1:{port}"))?;
    url.path_segments_mut()
        .map_err(|_| anyhow!("failed to construct schedule run URL"))?
        .extend(["api", "schedules"])
        .push(schedule_id)
        .push("run");
    Ok(url.to_string())
}

async fn run_schedule_via_daemon(root: &Path, schedule_id: &str) -> Result<()> {
    let pid = read_pid_file(root)?.ok_or_else(|| {
        anyhow!("clawhive daemon is not running. Start it with `clawhive up` first.")
    })?;
    if !is_process_running(pid) {
        return Err(anyhow!(
            "clawhive daemon is not running (stale pid: {pid}). Start it with `clawhive up`."
        ));
    }

    let port = read_port_file(root)?.unwrap_or(DEFAULT_PORT);
    let token = ensure_internal_cli_token(root)?;
    let url = build_schedule_run_url(port, schedule_id)?;

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(8))
        .build()
        .context("failed to initialize HTTP client")?;

    let response = client
        .post(&url)
        .header(INTERNAL_CLI_TOKEN_HEADER, token)
        .send()
        .await
        .with_context(|| format!("failed to call daemon API at {url}"))?;

    match response.status() {
        reqwest::StatusCode::NO_CONTENT => Ok(()),
        reqwest::StatusCode::NOT_FOUND => {
            Err(anyhow!("schedule not found: {schedule_id}"))
        }
        reqwest::StatusCode::UNAUTHORIZED => Err(anyhow!(
            "daemon rejected internal schedule trigger token. Restart daemon with `clawhive restart`."
        )),
        status => {
            let body = response.text().await.unwrap_or_default();
            Err(anyhow!(
                "daemon returned {status} when triggering schedule '{schedule_id}': {body}"
            ))
        }
    }
}

fn ensure_internal_cli_token(root: &Path) -> Result<String> {
    let path = root.join(INTERNAL_CLI_TOKEN_FILE);
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let existing = existing.trim();
        if !existing.is_empty() {
            return Ok(existing.to_string());
        }
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let token = Uuid::new_v4().to_string();
    std::fs::write(&path, &token)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&path, perms)?;
    }

    Ok(token)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use clawhive_scheduler::{RunRecord, RunStatus, SessionMode};

    use super::{build_schedule_run_url, format_history_record, format_session_mode};

    #[test]
    fn history_format_includes_response_and_session_key() {
        let record = RunRecord {
            schedule_id: "daily-digest".to_string(),
            started_at: chrono::Utc.with_ymd_and_hms(2026, 3, 13, 2, 8, 13).unwrap(),
            ended_at: chrono::Utc
                .with_ymd_and_hms(2026, 3, 13, 2, 11, 15)
                .unwrap(),
            status: RunStatus::Ok,
            error: None,
            duration_ms: 181_766,
            response: Some("final notify body".to_string()),
            session_key: Some("discord:bot:schedule:daily:uuid:user:1".to_string()),
        };

        let text = format_history_record(&record);
        assert!(text.contains("final notify body"));
        assert!(text.contains("discord:bot:schedule:daily:uuid:user:1"));
    }

    #[test]
    fn session_mode_format_is_stable() {
        assert_eq!(format_session_mode(&SessionMode::Isolated), "isolated");
        assert_eq!(format_session_mode(&SessionMode::Main), "main");
    }

    #[test]
    fn schedule_run_url_encodes_schedule_id() {
        let url = build_schedule_run_url(8848, "daily digest/#1").unwrap();
        assert_eq!(
            url,
            "http://127.0.0.1:8848/api/schedules/daily%20digest%2F%231/run"
        );
    }
}
