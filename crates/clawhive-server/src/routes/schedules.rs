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

pub async fn list_schedules(
    State(state): State<AppState>,
) -> Result<Json<Vec<ScheduleListItem>>, StatusCode> {
    let manager = make_manager(&state)?;
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
    let manager = make_manager(&state)?;
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
    let manager = make_manager(&state)?;
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
    let manager = make_manager(&state)?;
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
            })
            .collect(),
    ))
}

pub async fn get_schedule_detail(
    State(state): State<AppState>,
    Path(schedule_id): Path<String>,
) -> Result<Json<ScheduleConfig>, StatusCode> {
    let path = state
        .root
        .join(format!("config/schedules.d/{schedule_id}.yaml"));
    let content = std::fs::read_to_string(&path).map_err(|_| StatusCode::NOT_FOUND)?;
    let config: ScheduleConfig =
        serde_yaml::from_str(&content).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(config))
}

pub async fn update_schedule(
    State(state): State<AppState>,
    Path(schedule_id): Path<String>,
    Json(mut config): Json<ScheduleConfig>,
) -> Result<StatusCode, StatusCode> {
    let path = state
        .root
        .join(format!("config/schedules.d/{schedule_id}.yaml"));
    if !path.exists() {
        return Err(StatusCode::NOT_FOUND);
    }
    // Ensure schedule_id matches the path
    config.schedule_id = schedule_id;
    let yaml = serde_yaml::to_string(&config).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    std::fs::write(&path, yaml).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::NO_CONTENT)
}

fn make_manager(state: &AppState) -> Result<ScheduleManager, StatusCode> {
    ScheduleManager::new(
        &state.root.join("config/schedules.d"),
        &state.root.join("data/schedules"),
        Arc::clone(&state.bus),
    )
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

fn schedule_error_status(error: anyhow::Error) -> StatusCode {
    let message = error.to_string();
    if message.contains("schedule not found") {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::BAD_REQUEST
    }
}

async fn create_schedule(
    State(state): State<AppState>,
    Json(config): Json<ScheduleConfig>,
) -> Result<(StatusCode, Json<ScheduleConfig>), StatusCode> {
    if config.schedule_id.trim().is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let schedules_dir = state.root.join("config/schedules.d");
    std::fs::create_dir_all(&schedules_dir).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let path = schedules_dir.join(format!("{}.yaml", config.schedule_id));
    if path.exists() {
        return Err(StatusCode::CONFLICT);
    }

    let yaml = serde_yaml::to_string(&config).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    std::fs::write(&path, yaml).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok((StatusCode::CREATED, Json(config)))
}

async fn delete_schedule(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    let path = state.root.join(format!("config/schedules.d/{id}.yaml"));
    if !path.exists() {
        return Err(StatusCode::NOT_FOUND);
    }
    std::fs::remove_file(&path).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{body::Body, http::Request};
    use clawhive_bus::EventBus;
    use tower::ServiceExt;

    use super::router;
    use crate::state::AppState;

    fn write_file(path: &std::path::Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    fn setup_state() -> (AppState, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();

        write_file(
            &root.join("config/schedules.d/daily.yaml"),
            "schedule_id: daily\nenabled: true\nname: Daily\nschedule:\n  kind: every\n  interval_ms: 60000\nagent_id: clawhive-main\nsession_mode: isolated\ntask: ping\n",
        );

        write_file(&root.join("data/schedules/state.json"), "{}");

        (
            AppState {
                root: root.to_path_buf(),
                bus: Arc::new(EventBus::new(16)),
                gateway: None,
                web_password_hash: Arc::new(std::sync::RwLock::new(None)),
                session_store: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
                daemon_mode: false,
                port: 3000,
            },
            tmp,
        )
    }

    #[tokio::test]
    async fn list_returns_schedule_items() {
        let (state, _tmp) = setup_state();
        let app = router().with_state(state);

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::OK);
    }

    #[tokio::test]
    async fn toggle_updates_yaml() {
        let (state, _tmp) = setup_state();
        let app = router().with_state(state.clone());

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
        let yaml =
            std::fs::read_to_string(state.root.join("config/schedules.d/daily.yaml")).unwrap();
        assert!(yaml.contains("enabled: false"));
    }

    #[tokio::test]
    async fn run_missing_schedule_returns_not_found() {
        let (state, _tmp) = setup_state();
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
        let (state, _tmp) = setup_state();
        let app = router().with_state(state.clone());

        let body = r#"{
  "schedule_id": "test-sched",
  "name": "Test Schedule",
  "enabled": true,
  "schedule": { "kind": "every", "interval_ms": 60000 },
  "agent_id": "clawhive-main",
  "session_mode": "isolated",
  "task": "test task"
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
        assert!(state
            .root
            .join("config/schedules.d/test-sched.yaml")
            .exists());
    }

    #[tokio::test]
    async fn create_duplicate_schedule_returns_409() {
        let (state, _tmp) = setup_state();

        let body = r#"{
  "schedule_id": "daily",
  "name": "Daily Duplicate",
  "enabled": true,
  "schedule": { "kind": "every", "interval_ms": 60000 },
  "agent_id": "clawhive-main",
  "session_mode": "isolated",
  "task": "test task"
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
        let (state, _tmp) = setup_state();
        let schedule_path = state.root.join("config/schedules.d/daily.yaml");
        assert!(schedule_path.exists());

        let app = router().with_state(state.clone());
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
        assert!(!schedule_path.exists());
    }

    #[tokio::test]
    async fn delete_nonexistent_schedule_returns_404() {
        let (state, _tmp) = setup_state();
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
}
