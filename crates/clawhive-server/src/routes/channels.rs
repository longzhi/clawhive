use axum::{
    extract::{Path, State},
    routing::{delete, get, post},
    Json, Router,
};
use serde::Deserialize;
use serde::Serialize;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(get_channels).put(update_channels))
        .route("/status", get(get_channels_status))
        .route("/{kind}/connectors", post(add_connector))
        .route("/{kind}/connectors/{id}", delete(remove_connector))
}

#[derive(Serialize)]
struct ConnectorStatus {
    kind: String,
    connector_id: String,
    status: String,
}

#[derive(Deserialize)]
struct AddConnectorRequest {
    connector_id: String,
    /// Token for Telegram/Discord connectors.
    #[serde(default)]
    token: Option<String>,
    /// Feishu app_id
    #[serde(default)]
    app_id: Option<String>,
    /// Feishu app_secret
    #[serde(default)]
    app_secret: Option<String>,
    /// DingTalk client_id
    #[serde(default)]
    client_id: Option<String>,
    /// DingTalk client_secret
    #[serde(default)]
    client_secret: Option<String>,
    /// WeCom bot_id
    #[serde(default)]
    bot_id: Option<String>,
    /// WeCom secret
    #[serde(default)]
    secret: Option<String>,
    #[serde(default)]
    groups: Option<Vec<String>>,
    #[serde(default)]
    require_mention: Option<bool>,
}

async fn get_channels(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, axum::http::StatusCode> {
    let path = state.root.join("config/main.yaml");
    let content = std::fs::read_to_string(&path).map_err(|_| axum::http::StatusCode::NOT_FOUND)?;
    let val: serde_yaml::Value = serde_yaml::from_str(&content)
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;

    let channels = &val["channels"];
    let json = serde_json::to_value(channels)
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(json))
}

async fn update_channels(
    State(state): State<AppState>,
    Json(channels): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, axum::http::StatusCode> {
    let path = state.root.join("config/main.yaml");
    let content = std::fs::read_to_string(&path).map_err(|_| axum::http::StatusCode::NOT_FOUND)?;
    let mut val: serde_yaml::Value = serde_yaml::from_str(&content)
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;

    let channels_yaml: serde_yaml::Value = serde_json::from_value(channels.clone())
        .map_err(|_| axum::http::StatusCode::BAD_REQUEST)?;
    val["channels"] = channels_yaml;

    let yaml =
        serde_yaml::to_string(&val).map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    std::fs::write(&path, yaml).map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(channels))
}

async fn get_channels_status(
    State(state): State<AppState>,
) -> Result<Json<Vec<ConnectorStatus>>, axum::http::StatusCode> {
    let path = state.root.join("config/main.yaml");
    let content = std::fs::read_to_string(&path).map_err(|_| axum::http::StatusCode::NOT_FOUND)?;
    let val: serde_yaml::Value = serde_yaml::from_str(&content)
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;

    let mut statuses = Vec::new();
    let channels = val["channels"]
        .as_mapping()
        .ok_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;

    for (kind, channel) in channels {
        let Some(kind_str) = kind.as_str() else {
            continue;
        };
        let Some(channel_map) = channel.as_mapping() else {
            continue;
        };

        let enabled = channel_map
            .get(serde_yaml::Value::String("enabled".to_string()))
            .and_then(serde_yaml::Value::as_bool)
            .unwrap_or(false);

        let Some(connectors) = channel_map
            .get(serde_yaml::Value::String("connectors".to_string()))
            .and_then(serde_yaml::Value::as_sequence)
        else {
            continue;
        };

        for connector in connectors {
            let Some(connector_map) = connector.as_mapping() else {
                continue;
            };
            let connector_id = connector_map
                .get(serde_yaml::Value::String("connector_id".to_string()))
                .and_then(serde_yaml::Value::as_str)
                .unwrap_or_default()
                .to_string();
            if connector_id.is_empty() {
                continue;
            }

            let has_credentials = match kind_str {
                "feishu" => {
                    let app_id = connector_map
                        .get(serde_yaml::Value::String("app_id".to_string()))
                        .and_then(serde_yaml::Value::as_str)
                        .unwrap_or_default();
                    let app_secret = connector_map
                        .get(serde_yaml::Value::String("app_secret".to_string()))
                        .and_then(serde_yaml::Value::as_str)
                        .unwrap_or_default();
                    !app_id.is_empty()
                        && !app_secret.is_empty()
                        && !app_id.starts_with("${")
                        && !app_secret.starts_with("${")
                }
                "dingtalk" => {
                    let client_id = connector_map
                        .get(serde_yaml::Value::String("client_id".to_string()))
                        .and_then(serde_yaml::Value::as_str)
                        .unwrap_or_default();
                    let client_secret = connector_map
                        .get(serde_yaml::Value::String("client_secret".to_string()))
                        .and_then(serde_yaml::Value::as_str)
                        .unwrap_or_default();
                    !client_id.is_empty()
                        && !client_secret.is_empty()
                        && !client_id.starts_with("${")
                        && !client_secret.starts_with("${")
                }
                "wecom" => {
                    let bot_id = connector_map
                        .get(serde_yaml::Value::String("bot_id".to_string()))
                        .and_then(serde_yaml::Value::as_str)
                        .unwrap_or_default();
                    let secret = connector_map
                        .get(serde_yaml::Value::String("secret".to_string()))
                        .and_then(serde_yaml::Value::as_str)
                        .unwrap_or_default();
                    !bot_id.is_empty()
                        && !secret.is_empty()
                        && !bot_id.starts_with("${")
                        && !secret.starts_with("${")
                }
                _ => {
                    // Token-based channels: telegram, discord, slack, whatsapp, imessage
                    let token = connector_map
                        .get(serde_yaml::Value::String("token".to_string()))
                        .and_then(serde_yaml::Value::as_str)
                        .unwrap_or_default();
                    !token.is_empty() && !token.starts_with("${")
                }
            };

            let status = if !enabled {
                "inactive"
            } else if !has_credentials {
                "error"
            } else {
                "connected"
            };

            statuses.push(ConnectorStatus {
                kind: kind_str.to_string(),
                connector_id,
                status: status.to_string(),
            });
        }
    }

    Ok(Json(statuses))
}

fn write_main_config(
    state: &AppState,
    val: &serde_yaml::Value,
) -> Result<(), axum::http::StatusCode> {
    let path = state.root.join("config/main.yaml");
    let yaml =
        serde_yaml::to_string(val).map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    std::fs::write(&path, yaml).map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)
}

fn load_main_config(state: &AppState) -> Result<serde_yaml::Value, axum::http::StatusCode> {
    let path = state.root.join("config/main.yaml");
    let content = std::fs::read_to_string(&path).map_err(|_| axum::http::StatusCode::NOT_FOUND)?;
    serde_yaml::from_str(&content).map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)
}

fn connectors_mut<'a>(
    root: &'a mut serde_yaml::Value,
    kind: &str,
) -> Result<&'a mut Vec<serde_yaml::Value>, axum::http::StatusCode> {
    let channels = root["channels"]
        .as_mapping_mut()
        .ok_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    let kind_key = serde_yaml::Value::String(kind.to_string());
    // Auto-create channel kind with enabled: true if missing
    if !channels.contains_key(&kind_key) {
        let mut new_channel = serde_yaml::Mapping::new();
        new_channel.insert(
            serde_yaml::Value::String("enabled".to_string()),
            serde_yaml::Value::Bool(true),
        );
        new_channel.insert(
            serde_yaml::Value::String("connectors".to_string()),
            serde_yaml::Value::Sequence(Vec::new()),
        );
        channels.insert(kind_key.clone(), serde_yaml::Value::Mapping(new_channel));
    }
    let channel_map = channels
        .get_mut(&kind_key)
        .and_then(|v| v.as_mapping_mut())
        .ok_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    // Ensure enabled is true when adding a connector
    channel_map.insert(
        serde_yaml::Value::String("enabled".to_string()),
        serde_yaml::Value::Bool(true),
    );
    let connectors = channel_map
        .entry(serde_yaml::Value::String("connectors".to_string()))
        .or_insert_with(|| serde_yaml::Value::Sequence(Vec::new()));
    connectors
        .as_sequence_mut()
        .ok_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR)
}
async fn add_connector(
    State(state): State<AppState>,
    Path(kind): Path<String>,
    Json(body): Json<AddConnectorRequest>,
) -> Result<Json<serde_json::Value>, axum::http::StatusCode> {
    if body.connector_id.trim().is_empty() {
        return Err(axum::http::StatusCode::BAD_REQUEST);
    }

    let mut main = load_main_config(&state)?;
    let connectors = connectors_mut(&mut main, &kind)?;

    // Remove existing connector with same ID (upsert behavior)
    connectors.retain(|item| {
        item["connector_id"]
            .as_str()
            .map(|id| id != body.connector_id)
            .unwrap_or(true)
    });

    let mut connector = serde_yaml::Mapping::new();
    connector.insert(
        serde_yaml::Value::String("connector_id".to_string()),
        serde_yaml::Value::String(body.connector_id.clone()),
    );

    // Write credential fields based on channel kind
    match kind.as_str() {
        "feishu" => {
            let app_id = body.app_id.as_deref().unwrap_or_default();
            let app_secret = body.app_secret.as_deref().unwrap_or_default();
            if app_id.is_empty() || app_secret.is_empty() {
                return Err(axum::http::StatusCode::BAD_REQUEST);
            }
            connector.insert(
                serde_yaml::Value::String("app_id".to_string()),
                serde_yaml::Value::String(app_id.to_string()),
            );
            connector.insert(
                serde_yaml::Value::String("app_secret".to_string()),
                serde_yaml::Value::String(app_secret.to_string()),
            );
        }
        "dingtalk" => {
            let client_id = body.client_id.as_deref().unwrap_or_default();
            let client_secret = body.client_secret.as_deref().unwrap_or_default();
            if client_id.is_empty() || client_secret.is_empty() {
                return Err(axum::http::StatusCode::BAD_REQUEST);
            }
            connector.insert(
                serde_yaml::Value::String("client_id".to_string()),
                serde_yaml::Value::String(client_id.to_string()),
            );
            connector.insert(
                serde_yaml::Value::String("client_secret".to_string()),
                serde_yaml::Value::String(client_secret.to_string()),
            );
        }
        "wecom" => {
            let bot_id = body.bot_id.as_deref().unwrap_or_default();
            let secret = body.secret.as_deref().unwrap_or_default();
            if bot_id.is_empty() || secret.is_empty() {
                return Err(axum::http::StatusCode::BAD_REQUEST);
            }
            connector.insert(
                serde_yaml::Value::String("bot_id".to_string()),
                serde_yaml::Value::String(bot_id.to_string()),
            );
            connector.insert(
                serde_yaml::Value::String("secret".to_string()),
                serde_yaml::Value::String(secret.to_string()),
            );
        }
        "imessage" | "whatsapp" => {
            // No credentials needed — iMessage uses AppleScript, WhatsApp pairs via QR
        }
        _ => {
            // Telegram, Discord, Slack, and other token-based channels
            let token = body.token.as_deref().unwrap_or_default();
            if token.is_empty() {
                return Err(axum::http::StatusCode::BAD_REQUEST);
            }
            connector.insert(
                serde_yaml::Value::String("token".to_string()),
                serde_yaml::Value::String(token.to_string()),
            );
        }
    }
    if let Some(groups) = &body.groups {
        if !groups.is_empty() {
            let groups_seq: Vec<serde_yaml::Value> = groups
                .iter()
                .map(|g| serde_yaml::Value::String(g.clone()))
                .collect();
            connector.insert(
                serde_yaml::Value::String("groups".to_string()),
                serde_yaml::Value::Sequence(groups_seq),
            );
        }
    }
    if let Some(require_mention) = body.require_mention {
        if !require_mention {
            connector.insert(
                serde_yaml::Value::String("require_mention".to_string()),
                serde_yaml::Value::Bool(false),
            );
        }
    }
    connectors.push(serde_yaml::Value::Mapping(connector));

    write_main_config(&state, &main)?;
    Ok(Json(serde_json::json!({
        "kind": kind,
        "connector_id": body.connector_id,
    })))
}

async fn remove_connector(
    State(state): State<AppState>,
    Path((kind, id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, axum::http::StatusCode> {
    let mut main = load_main_config(&state)?;
    let connectors = connectors_mut(&mut main, &kind)?;

    let before = connectors.len();
    connectors.retain(|item| {
        item["connector_id"]
            .as_str()
            .map(|connector_id| connector_id != id)
            .unwrap_or(true)
    });

    if connectors.len() == before {
        return Err(axum::http::StatusCode::NOT_FOUND);
    }

    write_main_config(&state, &main)?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "kind": kind,
        "connector_id": id,
    })))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{
        body::Body,
        http::{Request, StatusCode},
        Router,
    };
    use tower::util::ServiceExt;

    use crate::state::AppState;

    fn setup_test_root() -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "clawhive-server-channels-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(root.join("config")).expect("create config dir");
        std::fs::write(
            root.join("config/main.yaml"),
            r#"channels:
  telegram:
    enabled: true
    connectors:
      - connector_id: tg_main
        token: ${TELEGRAM_BOT_TOKEN}
  discord:
    enabled: false
    connectors: []
"#,
        )
        .expect("write main.yaml");
        root
    }

    fn setup_test_app() -> (Router, std::path::PathBuf) {
        let root = setup_test_root();
        let state = AppState {
            root: root.clone(),
            bus: Arc::new(clawhive_bus::EventBus::new(16)),
            gateway: None,
            web_password_hash: Arc::new(std::sync::RwLock::new(None)),
            session_store: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
            pending_openai_oauth: Arc::new(
                std::sync::RwLock::new(std::collections::HashMap::new()),
            ),
            openai_oauth_config: crate::state::default_openai_oauth_config(),
            enable_openai_oauth_callback_listener: true,
            daemon_mode: false,
            port: 3000,
        };
        (
            Router::new()
                .nest("/api/channels", super::router())
                .with_state(state),
            root,
        )
    }

    fn read_connectors_len(root: &std::path::Path, kind: &str) -> usize {
        let content = std::fs::read_to_string(root.join("config/main.yaml")).expect("read yaml");
        let val: serde_yaml::Value = serde_yaml::from_str(&content).expect("parse yaml");
        val["channels"][kind]["connectors"]
            .as_sequence()
            .map(std::vec::Vec::len)
            .unwrap_or(0)
    }

    #[tokio::test]
    async fn test_get_channels_status() {
        let (app, _) = setup_test_app();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/channels/status")
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("send request");

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_add_connector() {
        let (app, root) = setup_test_app();

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/channels/telegram/connectors")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"connector_id":"tg_extra","token":"123:abc"}"#,
                    ))
                    .expect("build request"),
            )
            .await
            .expect("send request");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(read_connectors_len(&root, "telegram"), 2);
    }

    #[tokio::test]
    async fn test_delete_connector() {
        let (app, root) = setup_test_app();

        let response = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/channels/telegram/connectors/tg_main")
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("send request");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(read_connectors_len(&root, "telegram"), 0);
    }
}
