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
    token: String,
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

            let token = connector_map
                .get(serde_yaml::Value::String("token".to_string()))
                .and_then(serde_yaml::Value::as_str)
                .unwrap_or_default();

            let status = if !enabled {
                "inactive"
            } else if token.is_empty() || token.starts_with("${") {
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
    let channel = channels
        .get_mut(serde_yaml::Value::String(kind.to_string()))
        .ok_or(axum::http::StatusCode::NOT_FOUND)?;
    let channel_map = channel
        .as_mapping_mut()
        .ok_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
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

    let exists = connectors.iter().any(|item| {
        item["connector_id"]
            .as_str()
            .map(|id| id == body.connector_id)
            .unwrap_or(false)
    });
    if exists {
        return Err(axum::http::StatusCode::CONFLICT);
    }

    let mut connector = serde_yaml::Mapping::new();
    connector.insert(
        serde_yaml::Value::String("connector_id".to_string()),
        serde_yaml::Value::String(body.connector_id.clone()),
    );
    connector.insert(
        serde_yaml::Value::String("token".to_string()),
        serde_yaml::Value::String(body.token.clone()),
    );
    connectors.push(serde_yaml::Value::Mapping(connector));

    write_main_config(&state, &main)?;
    Ok(Json(serde_json::json!({
        "kind": kind,
        "connector_id": body.connector_id,
        "token": body.token,
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
