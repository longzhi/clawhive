use axum::{
    extract::State,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/status", get(setup_status))
        .route("/restart", post(restart))
        .route("/tools/web-search", get(get_web_search).put(put_web_search))
        .route("/provider-presets", get(get_provider_presets))
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

// ---------------------------------------------------------------------------
// Provider presets (single source of truth for CLI + Web UI)
// ---------------------------------------------------------------------------
async fn get_provider_presets() -> Json<Vec<serde_json::Value>> {
    let presets: Vec<serde_json::Value> = clawhive_schema::provider_presets::PROVIDER_PRESETS
        .iter()
        .map(|p| {
            serde_json::json!({
                "id": p.id,
                "name": p.name,
                "api_base": p.api_base,
                "needs_key": p.needs_key,
                "default_model": p.default_model,
                "models": p.models,
            })
        })
        .collect();
    Json(presets)
}

// ---------------------------------------------------------------------------
// Web Search tools config
// ---------------------------------------------------------------------------
#[derive(Serialize, Deserialize)]
pub struct WebSearchConfig {
    pub enabled: bool,
    pub provider: Option<String>,
    pub api_key: Option<String>,
}

async fn get_web_search(
    State(state): State<AppState>,
) -> Result<Json<WebSearchConfig>, axum::http::StatusCode> {
    let path = state.root.join("config/main.yaml");
    let val = std::fs::read_to_string(&path)
        .ok()
        .and_then(|c| serde_yaml::from_str::<serde_yaml::Value>(&c).ok())
        .unwrap_or(serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));

    let ws = &val["tools"]["web_search"];
    Ok(Json(WebSearchConfig {
        enabled: ws["enabled"].as_bool().unwrap_or(false),
        provider: ws["provider"].as_str().map(|s| s.to_string()),
        api_key: ws["api_key"].as_str().map(|s| s.to_string()),
    }))
}

async fn put_web_search(
    State(state): State<AppState>,
    Json(config): Json<WebSearchConfig>,
) -> Result<Json<WebSearchConfig>, axum::http::StatusCode> {
    let path = state.root.join("config/main.yaml");
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc: serde_yaml::Value = serde_yaml::from_str(&content)
        .unwrap_or(serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));

    // Ensure tools mapping exists
    if !doc["tools"].is_mapping() {
        doc["tools"] = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
    }

    let mut ws_map = serde_yaml::Mapping::new();
    ws_map.insert("enabled".into(), serde_yaml::Value::Bool(config.enabled));
    if let Some(ref p) = config.provider {
        ws_map.insert("provider".into(), serde_yaml::Value::String(p.clone()));
    }
    if let Some(ref k) = config.api_key {
        ws_map.insert("api_key".into(), serde_yaml::Value::String(k.clone()));
    }
    doc["tools"]["web_search"] = serde_yaml::Value::Mapping(ws_map);

    let yaml =
        serde_yaml::to_string(&doc).map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    std::fs::write(&path, yaml).map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(config))
}

// ---------------------------------------------------------------------------
// Restart
// ---------------------------------------------------------------------------
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
