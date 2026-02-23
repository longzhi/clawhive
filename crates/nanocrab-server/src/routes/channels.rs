use axum::{extract::State, routing::get, Json, Router};
use serde::Serialize;

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(get_channels).put(update_channels))
        .route("/status", get(get_channels_status))
}

#[derive(Serialize)]
struct ConnectorStatus {
    kind: String,
    connector_id: String,
    status: String,
}

async fn get_channels(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, axum::http::StatusCode> {
    let path = state.root.join("config/main.yaml");
    let content =
        std::fs::read_to_string(&path).map_err(|_| axum::http::StatusCode::NOT_FOUND)?;
    let val: serde_yaml::Value =
        serde_yaml::from_str(&content).map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;

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
    let content =
        std::fs::read_to_string(&path).map_err(|_| axum::http::StatusCode::NOT_FOUND)?;
    let mut val: serde_yaml::Value =
        serde_yaml::from_str(&content).map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;

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
    let content =
        std::fs::read_to_string(&path).map_err(|_| axum::http::StatusCode::NOT_FOUND)?;
    let val: serde_yaml::Value =
        serde_yaml::from_str(&content).map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;

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
            "nanocrab-server-channels-test-{}",
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

    fn setup_test_app() -> Router {
        let root = setup_test_root();
        let state = AppState {
            root,
            bus: Arc::new(nanocrab_bus::EventBus::new(16)),
        };
        Router::new().nest("/api/channels", super::router()).with_state(state)
    }

    #[tokio::test]
    async fn test_get_channels_status() {
        let app = setup_test_app();
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
}
