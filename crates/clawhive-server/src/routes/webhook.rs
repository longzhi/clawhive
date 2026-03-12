use axum::{
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, StatusCode},
    routing::post,
    Json, Router,
};
use chrono::Utc;
use clawhive_channels::webhook::get_normalizer;
use clawhive_core::config::{DeliveryRoutingConfig, WebhookChannelConfig};
use clawhive_schema::{BusMessage, InboundMessage};
use uuid::Uuid;

use crate::state::AppState;
use crate::webhook_auth;

/// Maximum webhook request body size: 1 MB.
const MAX_BODY_SIZE: usize = 1024 * 1024;

pub fn webhook_router() -> Router<AppState> {
    Router::new()
        .route("/{source_id}", post(handle_webhook))
        .layer(DefaultBodyLimit::max(MAX_BODY_SIZE))
}

async fn handle_webhook(
    State(state): State<AppState>,
    Path(source_id): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Result<(StatusCode, Json<serde_json::Value>), StatusCode> {
    let webhook_cfg = load_webhook_config(&state)?;
    let source = webhook_cfg
        .sources
        .iter()
        .find(|source| source.source_id == source_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    // Validate auth method
    if source.auth.method != "api_key" {
        tracing::warn!(
            source_id = %source_id,
            method = %source.auth.method,
            "unsupported auth method for webhook source"
        );
        return Err(StatusCode::UNAUTHORIZED);
    }

    let provided_key = webhook_auth::extract_api_key(&headers).ok_or(StatusCode::UNAUTHORIZED)?;
    let stored = source
        .auth
        .key_hash
        .as_deref()
        .or(source.auth.key.as_deref())
        .ok_or(StatusCode::UNAUTHORIZED)?;
    if !webhook_auth::verify_api_key(&provided_key, stored) {
        return Err(StatusCode::UNAUTHORIZED);
    }

    let payload: serde_json::Value =
        serde_json::from_str(&body).map_err(|_| StatusCode::UNPROCESSABLE_ENTITY)?;

    let Some(gateway) = state.gateway.clone() else {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    };

    let normalizer = get_normalizer(&source.format);
    let normalized = normalizer.normalize(&payload);
    let conversation_scope = normalizer.derive_scope(&payload, &source_id);
    let trace_id = Uuid::new_v4();

    let text = match &source.prompt {
        Some(prompt) => format!("{prompt}\n\n---\n\n{}", normalized.text),
        None => normalized.text,
    };

    let inbound = InboundMessage {
        trace_id,
        channel_type: "webhook".to_string(),
        connector_id: source_id.clone(),
        conversation_scope,
        user_scope: format!("webhook:{source_id}"),
        text,
        at: Utc::now(),
        thread_id: None,
        is_mention: false,
        mention_target: None,
        message_id: None,
        attachments: vec![],
        group_context: None,
        message_source: Some("webhook_event".to_string()),
    };

    let agent_id = gateway.resolve_agent(&inbound);
    let delivery = find_delivery_for_webhook(&state, &source_id);

    let bus = state.bus.clone();
    tokio::spawn(async move {
        match gateway.handle_inbound(inbound).await {
            Ok(outbound) => {
                if let Some(delivery) = delivery {
                    let conversation_scope = delivery
                        .target
                        .unwrap_or_else(|| outbound.conversation_scope.clone());
                    let _ = bus
                        .publish(BusMessage::DeliverAnnounce {
                            channel_type: delivery.channel,
                            connector_id: delivery.connector_id,
                            conversation_scope,
                            text: outbound.text,
                        })
                        .await;
                }
            }
            Err(error) => {
                tracing::warn!(
                    trace_id = %trace_id,
                    source_id = %source_id,
                    error = %error,
                    "failed to handle webhook inbound"
                );
            }
        }
    });

    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "trace_id": trace_id,
            "agent_id": agent_id,
            "status": "accepted"
        })),
    ))
}

fn load_webhook_config(state: &AppState) -> Result<WebhookChannelConfig, StatusCode> {
    state
        .webhook_config
        .read()
        .unwrap()
        .clone()
        .ok_or(StatusCode::NOT_FOUND)
}

fn find_delivery_for_webhook(state: &AppState, source_id: &str) -> Option<DeliveryRoutingConfig> {
    let routing = state.routing_config.read().unwrap();
    let delivery = routing
        .as_ref()?
        .bindings
        .iter()
        .find(|binding| binding.channel_type == "webhook" && binding.connector_id == source_id)
        .and_then(|binding| binding.delivery.clone())?;
    // Validate delivery mode — only "announce" is currently supported
    match delivery.mode.as_str() {
        "announce" => Some(delivery),
        unknown => {
            tracing::warn!(
                source_id = %source_id,
                mode = %unknown,
                "unsupported delivery mode, skipping delivery"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use clawhive_bus::EventBus;
    use std::collections::HashMap;
    use std::sync::{Arc, RwLock};
    use tower::ServiceExt;

    use super::*;

    fn setup_test_config(dir: &std::path::Path) {
        let config_dir = dir.join("config");
        std::fs::create_dir_all(&config_dir).unwrap();

        let main_yaml = r#"
app:
  name: test
runtime:
  max_concurrent: 4
features:
  multi_agent: false
  sub_agent: false
  tui: false
  cli: false
channels:
  telegram: null
  discord: null
  webhook:
    enabled: true
    sources:
      - source_id: test-source
        format: raw
        auth:
          method: api_key
          key: "whk_testkey1234567890"
"#;
        std::fs::write(config_dir.join("main.yaml"), main_yaml).unwrap();

        let routing_yaml = r#"
default_agent_id: test-agent
bindings: []
"#;
        std::fs::write(config_dir.join("routing.yaml"), routing_yaml).unwrap();
    }

    fn test_state(dir: &std::path::Path) -> AppState {
        let state = AppState {
            root: dir.to_path_buf(),
            bus: Arc::new(EventBus::new(16)),
            gateway: None,
            web_password_hash: Arc::new(RwLock::new(None)),
            session_store: Arc::new(RwLock::new(HashMap::new())),
            pending_openai_oauth: Arc::new(RwLock::new(HashMap::new())),
            openai_oauth_config: crate::state::default_openai_oauth_config(),
            enable_openai_oauth_callback_listener: false,
            daemon_mode: false,
            port: 8848,
            webhook_config: Arc::new(RwLock::new(None)),
            routing_config: Arc::new(RwLock::new(None)),
        };
        state.load_webhook_config_from_disk();
        state.load_routing_config_from_disk();
        state
    }

    #[tokio::test]
    async fn webhook_returns_401_without_api_key() {
        let tmp = tempfile::tempdir().unwrap();
        setup_test_config(tmp.path());
        let state = test_state(tmp.path());
        let app = webhook_router().with_state(state);

        let req = Request::builder()
            .method("POST")
            .uri("/test-source")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"key": "value"}"#))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn webhook_returns_404_for_unknown_source() {
        let tmp = tempfile::tempdir().unwrap();
        setup_test_config(tmp.path());
        let state = test_state(tmp.path());
        let app = webhook_router().with_state(state);

        let req = Request::builder()
            .method("POST")
            .uri("/unknown-source")
            .header("content-type", "application/json")
            .header("authorization", "Bearer whk_testkey1234567890")
            .body(Body::from(r#"{"key": "value"}"#))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn webhook_returns_401_for_wrong_key() {
        let tmp = tempfile::tempdir().unwrap();
        setup_test_config(tmp.path());
        let state = test_state(tmp.path());
        let app = webhook_router().with_state(state);

        let req = Request::builder()
            .method("POST")
            .uri("/test-source")
            .header("content-type", "application/json")
            .header("authorization", "Bearer whk_wrongkey")
            .body(Body::from(r#"{"key": "value"}"#))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn webhook_returns_503_when_no_gateway() {
        let tmp = tempfile::tempdir().unwrap();
        setup_test_config(tmp.path());
        let state = test_state(tmp.path());
        let app = webhook_router().with_state(state);

        let req = Request::builder()
            .method("POST")
            .uri("/test-source")
            .header("content-type", "application/json")
            .header("authorization", "Bearer whk_testkey1234567890")
            .body(Body::from(r#"{"key": "value"}"#))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn webhook_returns_422_for_non_json() {
        let tmp = tempfile::tempdir().unwrap();
        setup_test_config(tmp.path());
        let state = test_state(tmp.path());
        let app = webhook_router().with_state(state);

        let req = Request::builder()
            .method("POST")
            .uri("/test-source")
            .header("content-type", "application/json")
            .header("authorization", "Bearer whk_testkey1234567890")
            .body(Body::from("not valid json"))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    fn setup_disabled_webhook_config(dir: &std::path::Path) {
        let config_dir = dir.join("config");
        std::fs::create_dir_all(&config_dir).unwrap();

        let main_yaml = r#"
app:
  name: test
runtime:
  max_concurrent: 4
features:
  multi_agent: false
  sub_agent: false
  tui: false
  cli: false
channels:
  telegram: null
  discord: null
  webhook:
    enabled: false
    sources:
      - source_id: test-source
        format: raw
        auth:
          method: api_key
          key: "whk_testkey1234567890"
"#;
        std::fs::write(config_dir.join("main.yaml"), main_yaml).unwrap();

        let routing_yaml = r#"
default_agent_id: test-agent
bindings: []
"#;
        std::fs::write(config_dir.join("routing.yaml"), routing_yaml).unwrap();
    }

    #[tokio::test]
    async fn webhook_returns_404_when_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        setup_disabled_webhook_config(tmp.path());
        let state = test_state(tmp.path());
        let app = webhook_router().with_state(state);

        let req = Request::builder()
            .method("POST")
            .uri("/test-source")
            .header("content-type", "application/json")
            .header("authorization", "Bearer whk_testkey1234567890")
            .body(Body::from(r#"{"key": "value"}"#))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn webhook_returns_404_when_no_webhook_config() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("config");
        std::fs::create_dir_all(&config_dir).unwrap();

        let main_yaml = r#"
app:
  name: test
runtime:
  max_concurrent: 4
features:
  multi_agent: false
  sub_agent: false
  tui: false
  cli: false
channels:
  telegram: null
  discord: null
"#;
        std::fs::write(config_dir.join("main.yaml"), main_yaml).unwrap();

        let state = test_state(tmp.path());
        let app = webhook_router().with_state(state);

        let req = Request::builder()
            .method("POST")
            .uri("/test-source")
            .header("content-type", "application/json")
            .header("authorization", "Bearer whk_testkey1234567890")
            .body(Body::from(r#"{"key": "value"}"#))
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
