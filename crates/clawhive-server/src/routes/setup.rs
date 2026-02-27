use axum::{
    extract::State,
    routing::{get, post},
    Json, Router,
};
use serde::Serialize;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/status", get(setup_status))
        .route("/restart", post(restart))
}

#[derive(Serialize)]
pub struct SetupStatus {
    pub needs_setup: bool,
    pub has_providers: bool,
    pub has_active_agents: bool,
    pub has_channels: bool,
}

async fn setup_status(State(state): State<AppState>) -> Json<SetupStatus> {
    let providers_dir = state.root.join("config/providers.d");
    let has_providers = std::fs::read_dir(&providers_dir)
        .map(|entries| {
            entries
                .flatten()
                .any(|e| e.path().extension().and_then(|x| x.to_str()) == Some("yaml"))
        })
        .unwrap_or(false);

    let agents_dir = state.root.join("config/agents.d");
    let has_active_agents = std::fs::read_dir(&agents_dir)
        .map(|entries| {
            entries.flatten().any(|e| {
                let path = e.path();
                if path.extension().and_then(|x| x.to_str()) != Some("yaml") {
                    return false;
                }
                std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|content| serde_yaml::from_str::<serde_yaml::Value>(&content).ok())
                    .map(|val| val["enabled"].as_bool().unwrap_or(false))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);

    let main_yaml = state.root.join("config/main.yaml");
    let has_channels = std::fs::read_to_string(&main_yaml)
        .ok()
        .and_then(|content| serde_yaml::from_str::<serde_yaml::Value>(&content).ok())
        .map(|val| {
            let channels = &val["channels"];
            let tg_enabled = channels["telegram"]["enabled"].as_bool().unwrap_or(false);
            let dc_enabled = channels["discord"]["enabled"].as_bool().unwrap_or(false);
            let tg_has_connectors = channels["telegram"]["connectors"]
                .as_sequence()
                .map(|s| !s.is_empty())
                .unwrap_or(false);
            let dc_has_connectors = channels["discord"]["connectors"]
                .as_sequence()
                .map(|s| !s.is_empty())
                .unwrap_or(false);
            (tg_enabled && tg_has_connectors) || (dc_enabled && dc_has_connectors)
        })
        .unwrap_or(false);

    let needs_setup = !has_providers || !has_active_agents;

    Json(SetupStatus {
        needs_setup,
        has_providers,
        has_active_agents,
        has_channels,
    })
}

#[derive(Serialize)]
struct RestartResponse {
    ok: bool,
}

async fn restart(State(state): State<AppState>) -> Json<RestartResponse> {
    let root = state.root.clone();
    let port = state.port;

    // Spawn the restart in a background task so we can return 200 first
    tokio::spawn(async move {
        // Brief delay to allow the HTTP response to be sent
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        // Spawn a new clawhive start process, then exit the current one.
        // The new process will pick up the updated config files.
        let exe = std::env::current_exe().unwrap_or_else(|_| "clawhive".into());

        // Open log file for the new process
        let log_dir = root.join("logs");
        let _ = std::fs::create_dir_all(&log_dir);
        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_dir.join("clawhive.out"));

        let mut cmd = std::process::Command::new(&exe);
        cmd.arg("--config-root")
            .arg(&root)
            .arg("start")
            .arg("--port")
            .arg(port.to_string())
            .stdin(std::process::Stdio::null());

        if let Ok(log) = log_file {
            if let Ok(log_err) = log.try_clone() {
                cmd.stdout(std::process::Stdio::from(log));
                cmd.stderr(std::process::Stdio::from(log_err));
            }
        }

        match cmd.spawn() {
            Ok(child) => {
                tracing::info!(
                    "Spawned new clawhive process (pid: {}), exiting...",
                    child.id()
                );
            }
            Err(e) => {
                tracing::error!("Failed to spawn new clawhive process: {e}");
                return;
            }
        }

        // Exit current process
        std::process::exit(0);
    });

    Json(RestartResponse { ok: true })
}
