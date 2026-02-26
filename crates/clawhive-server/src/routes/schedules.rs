use std::sync::Arc;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, patch, post},
    Json, Router,
};
use chrono::{TimeZone, Utc};
use clawhive_scheduler::{RunStatus, ScheduleManager, ScheduleType, SessionMode};
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
        .route("/", get(list_schedules))
        .route("/{id}/run", post(run_schedule))
        .route("/{id}", patch(toggle_schedule))
        .route("/{id}/history", get(schedule_history))
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
}
