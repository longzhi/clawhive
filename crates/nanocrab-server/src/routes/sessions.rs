use axum::{
    extract::{Path, Query, State},
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};

use crate::state::AppState;

#[derive(Serialize)]
pub struct SessionSummary {
    pub session_key: String,
    pub file_name: String,
    pub message_count: usize,
    pub last_modified: String,
}

#[derive(Serialize)]
pub struct SessionMessage {
    pub role: String,
    pub text: String,
    pub timestamp: String,
}

#[derive(Deserialize)]
pub struct SessionQuery {
    pub agent: Option<String>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_sessions))
        .route("/{key}", get(get_session_messages))
        .route("/{key}/reset", axum::routing::post(reset_session))
}

async fn list_sessions(
    State(state): State<AppState>,
    Query(_query): Query<SessionQuery>,
) -> Json<Vec<SessionSummary>> {
    let sessions_dir = state.root.join("data/sessions");
    let mut sessions = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&sessions_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let file_name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();

            let content = std::fs::read_to_string(&path).unwrap_or_default();
            let message_count = content.lines().count();

            let last_modified = entry
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .map(|t| {
                    let dt: chrono::DateTime<chrono::Utc> = t.into();
                    dt.format("%Y-%m-%d %H:%M:%S").to_string()
                })
                .unwrap_or_default();

            sessions.push(SessionSummary {
                session_key: file_name.clone(),
                file_name,
                message_count,
                last_modified,
            });
        }
    }

    sessions.sort_by(|a, b| b.last_modified.cmp(&a.last_modified));
    Json(sessions)
}

async fn get_session_messages(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> Result<Json<Vec<SessionMessage>>, axum::http::StatusCode> {
    let path = state.root.join(format!("data/sessions/{key}.jsonl"));
    let content = std::fs::read_to_string(&path).map_err(|_| axum::http::StatusCode::NOT_FOUND)?;

    let messages: Vec<SessionMessage> = content
        .lines()
        .filter_map(|line| {
            let val: serde_json::Value = serde_json::from_str(line).ok()?;
            let entry_type = val["type"].as_str()?;
            if entry_type != "message" {
                return None;
            }
            Some(SessionMessage {
                role: val["role"].as_str().unwrap_or("unknown").to_string(),
                text: val["text"].as_str().unwrap_or("").to_string(),
                timestamp: val["at"].as_str().unwrap_or("").to_string(),
            })
        })
        .collect();

    Ok(Json(messages))
}

async fn reset_session(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> Result<Json<serde_json::Value>, axum::http::StatusCode> {
    let path = state.root.join(format!("data/sessions/{key}.jsonl"));
    std::fs::remove_file(&path).map_err(|_| axum::http::StatusCode::NOT_FOUND)?;
    Ok(Json(serde_json::json!({ "status": "reset", "key": key })))
}
