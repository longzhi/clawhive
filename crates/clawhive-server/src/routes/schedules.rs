use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, patch, post},
    Json, Router,
};
use chrono::{TimeZone, Utc};
use clawhive_scheduler::{RunStatus, ScheduleConfig, ScheduleManager, ScheduleType, SessionMode};
use serde::{Deserialize, Serialize};

use crate::state::AppState;

#[derive(Serialize)]
pub struct ScheduleListItem {
    pub schedule_id: String,
    pub name: String,
    pub description: Option<String>,
    pub enabled: bool,
    pub schedule: ScheduleType,
    pub agent_id: String,
    pub session_mode: SessionMode,
    pub next_run_at: Option<String>,
    pub last_run_status: Option<RunStatus>,
    pub last_run_at: Option<String>,
    pub consecutive_errors: u32,
}

#[derive(Serialize)]
pub struct ScheduleRunHistoryItem {
    pub started_at: String,
    pub ended_at: String,
    pub status: RunStatus,
    pub error: Option<String>,
    pub duration_ms: u64,
    pub response: Option<String>,
    pub session_key: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ToggleBody {
    pub enabled: bool,
}

#[derive(Debug, Deserialize)]
pub struct HistoryParams {
    pub limit: Option<usize>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_schedules).post(create_schedule))
        .route("/{id}/run", post(run_schedule))
        .route(
            "/{id}",
            patch(toggle_schedule)
                .put(update_schedule)
                .delete(delete_schedule),
        )
        .route("/{id}/history", get(schedule_history))
        .route("/{id}/detail", get(get_schedule_detail))
}

fn get_manager(state: &AppState) -> Result<&Arc<ScheduleManager>, StatusCode> {
    state
        .schedule_manager
        .as_ref()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)
}

fn schedule_error_status(error: anyhow::Error) -> StatusCode {
    let message = error.to_string();
    if message.contains("schedule not found") {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::BAD_REQUEST
    }
}

pub async fn list_schedules(
    State(state): State<AppState>,
) -> Result<Json<Vec<ScheduleListItem>>, StatusCode> {
    let manager = get_manager(&state)?;
    let entries = manager.list().await;

    let items = entries
        .into_iter()
        .map(|entry| ScheduleListItem {
            schedule_id: entry.config.schedule_id,
            name: entry.config.name,
            description: entry.config.description,
            enabled: entry.config.enabled,
            schedule: entry.config.schedule,
            agent_id: entry.config.agent_id,
            session_mode: entry.config.session_mode,
            next_run_at: entry
                .state
                .next_run_at_ms
                .and_then(|ms| Utc.timestamp_millis_opt(ms).single())
                .map(|dt| dt.to_rfc3339()),
            last_run_status: entry.state.last_run_status,
            last_run_at: entry
                .state
                .last_run_at_ms
                .and_then(|ms| Utc.timestamp_millis_opt(ms).single())
                .map(|dt| dt.to_rfc3339()),
            consecutive_errors: entry.state.consecutive_errors,
        })
        .collect();

    Ok(Json(items))
}

pub async fn run_schedule(
    State(state): State<AppState>,
    Path(schedule_id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    let manager = get_manager(&state)?;
    manager
        .trigger_now(&schedule_id)
        .await
        .map_err(schedule_error_status)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn toggle_schedule(
    State(state): State<AppState>,
    Path(schedule_id): Path<String>,
    Json(body): Json<ToggleBody>,
) -> Result<StatusCode, StatusCode> {
    let manager = get_manager(&state)?;
    manager
        .set_enabled(&schedule_id, body.enabled)
        .await
        .map_err(schedule_error_status)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn schedule_history(
    State(state): State<AppState>,
    Path(schedule_id): Path<String>,
    Query(params): Query<HistoryParams>,
) -> Result<Json<Vec<ScheduleRunHistoryItem>>, StatusCode> {
    let manager = get_manager(&state)?;
    let records = manager
        .recent_history(&schedule_id, params.limit.unwrap_or(20))
        .await
        .map_err(schedule_error_status)?;

    Ok(Json(
        records
            .into_iter()
            .map(|record| ScheduleRunHistoryItem {
                started_at: record.started_at.to_rfc3339(),
                ended_at: record.ended_at.to_rfc3339(),
                status: record.status,
                error: record.error,
                duration_ms: record.duration_ms,
                response: record.response,
                session_key: record.session_key,
            })
            .collect(),
    ))
}

pub async fn get_schedule_detail(
    State(state): State<AppState>,
    Path(schedule_id): Path<String>,
) -> Result<Json<ScheduleConfig>, StatusCode> {
    let manager = get_manager(&state)?;
    let view = manager
        .get_schedule(&schedule_id)
        .await
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(view.config))
}

pub async fn update_schedule(
    State(state): State<AppState>,
    Path(schedule_id): Path<String>,
    Json(patch): Json<serde_json::Value>,
) -> Result<StatusCode, StatusCode> {
    let manager = get_manager(&state)?;
    manager
        .update_schedule(&schedule_id, &patch)
        .await
        .map_err(schedule_error_status)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn create_schedule(
    State(state): State<AppState>,
    Json(config): Json<ScheduleConfig>,
) -> Result<(StatusCode, Json<ScheduleConfig>), StatusCode> {
    if config.schedule_id.trim().is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let manager = get_manager(&state)?;

    if manager.get_schedule(&config.schedule_id).await.is_some() {
        return Err(StatusCode::CONFLICT);
    }

    manager
        .add_schedule(config.clone())
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok((StatusCode::CREATED, Json(config)))
}

async fn delete_schedule(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    let manager = get_manager(&state)?;

    if manager.get_schedule(&id).await.is_none() {
        return Err(StatusCode::NOT_FOUND);
    }

    manager
        .remove_schedule(&id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{
        body::{to_bytes, Body},
        http::Request,
    };
    use clawhive_bus::EventBus;
    use clawhive_scheduler::{
        DeliveryConfig, ScheduleConfig, ScheduleManager, ScheduleType, SessionMode, SqliteStore,
    };
    use tower::ServiceExt;

    use super::router;
    use crate::state::AppState;

    async fn setup_state() -> (AppState, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("config")).unwrap();
        std::fs::create_dir_all(root.join("data")).unwrap();

        let store = SqliteStore::open(&root.join("data/scheduler.db")).unwrap();
        let bus = Arc::new(EventBus::new(16));

        let config = ScheduleConfig {
            schedule_id: "daily".to_string(),
            enabled: true,
            name: "Daily".to_string(),
            description: None,
            schedule: ScheduleType::Every {
                interval_ms: 60_000,
                anchor_ms: None,
            },
            agent_id: "clawhive-main".to_string(),
            session_mode: SessionMode::Isolated,
            payload: Some(clawhive_scheduler::TaskPayload::AgentTurn {
                message: "ping".to_string(),
                model: None,
                thinking: None,
                timeout_seconds: 300,
                light_context: false,
            }),
            timeout_seconds: 300,
            delete_after_run: false,
            delivery: DeliveryConfig::default(),
        };
        store.save_schedule_config(&config).await.unwrap();

        let manager = Arc::new(ScheduleManager::new(store, Arc::clone(&bus)).await.unwrap());

        (
            AppState {
                root: root.to_path_buf(),
                bus,
                gateway: None,
                web_password_hash: Arc::new(std::sync::RwLock::new(None)),
                session_store: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
                whatsapp_pairing: Arc::new(
                    std::sync::RwLock::new(std::collections::HashMap::new()),
                ),
                pending_openai_oauth: Arc::new(std::sync::RwLock::new(
                    std::collections::HashMap::new(),
                )),
                openai_oauth_config: crate::state::default_openai_oauth_config(),
                enable_openai_oauth_callback_listener: true,
                daemon_mode: false,
                port: 3000,
                schedule_manager: Some(manager),
                reload_coordinator: None,
            },
            tmp,
        )
    }

    #[tokio::test]
    async fn list_returns_schedule_items() {
        let (state, _tmp) = setup_state().await;
        let app = router().with_state(state);

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::OK);
    }

    #[tokio::test]
    async fn toggle_schedule_via_patch() {
        let (state, _tmp) = setup_state().await;
        let app = router().with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/daily")
                    .header("content-type", "application/json")
                    .body(Body::from("{\"enabled\":false}"))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn run_missing_schedule_returns_not_found() {
        let (state, _tmp) = setup_state().await;
        let app = router().with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/missing/run")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn create_schedule_returns_201() {
        let (state, _tmp) = setup_state().await;
        let app = router().with_state(state);

        let body = r#"{
  "schedule_id": "test-sched",
  "name": "Test Schedule",
  "enabled": true,
  "schedule": { "kind": "every", "interval_ms": 60000 },
  "agent_id": "clawhive-main",
  "session_mode": "isolated",
  "payload": { "kind": "agent_turn", "message": "test", "timeout_seconds": 300, "light_context": false }
}"#;

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::CREATED);
    }

    #[tokio::test]
    async fn create_duplicate_schedule_returns_409() {
        let (state, _tmp) = setup_state().await;

        let body = r#"{
  "schedule_id": "daily",
  "name": "Daily Duplicate",
  "enabled": true,
  "schedule": { "kind": "every", "interval_ms": 60000 },
  "agent_id": "clawhive-main",
  "session_mode": "isolated",
  "payload": { "kind": "agent_turn", "message": "test", "timeout_seconds": 300, "light_context": false }
}"#;

        let app = router().with_state(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn delete_schedule_returns_204() {
        let (state, _tmp) = setup_state().await;
        let app = router().with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/daily")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn delete_nonexistent_schedule_returns_404() {
        let (state, _tmp) = setup_state().await;
        let app = router().with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/nonexistent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn get_schedule_detail_returns_config() {
        let (state, _tmp) = setup_state().await;
        let app = router().with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/daily/detail")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["schedule_id"], "daily");
    }

    #[tokio::test]
    async fn get_schedule_detail_not_found() {
        let (state, _tmp) = setup_state().await;
        let app = router().with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/nonexistent/detail")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn history_returns_empty_for_no_runs() {
        let (state, _tmp) = setup_state().await;
        let app = router().with_state(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/daily/history")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json.as_array().unwrap().is_empty());
    }
}
