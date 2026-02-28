use axum::{
    extract::{Path, Query, State},
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::path::{Path as StdPath, PathBuf};

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

/// Collect session summaries from a directory of .jsonl files.
fn collect_sessions(dir: &StdPath) -> Vec<SessionSummary> {
    let mut sessions = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return sessions,
    };
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
        let message_count = content
            .lines()
            .filter(|line| {
                serde_json::from_str::<serde_json::Value>(line)
                    .ok()
                    .and_then(|v| v["type"].as_str().map(|t| t == "message"))
                    .unwrap_or(false)
            })
            .count();

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
    sessions
}

/// Find a session file by key across all workspace session dirs and the global fallback.
fn find_session_file(root: &StdPath, key: &str) -> Option<PathBuf> {
    let filename = format!("{key}.jsonl");

    // Search workspaces/*/sessions/
    if let Ok(entries) = std::fs::read_dir(root.join("workspaces")) {
        for entry in entries.flatten() {
            let candidate = entry.path().join("sessions").join(&filename);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    // Fallback: root/sessions/
    let fallback = root.join("sessions").join(&filename);
    if fallback.is_file() {
        return Some(fallback);
    }

    None
}

async fn list_sessions(
    State(state): State<AppState>,
    Query(_query): Query<SessionQuery>,
) -> Json<Vec<SessionSummary>> {
    let mut sessions = Vec::new();

    // Scan workspaces/*/sessions/
    if let Ok(entries) = std::fs::read_dir(state.root.join("workspaces")) {
        for entry in entries.flatten() {
            let sessions_dir = entry.path().join("sessions");
            sessions.extend(collect_sessions(&sessions_dir));
        }
    }

    // Fallback: root/sessions/
    sessions.extend(collect_sessions(&state.root.join("sessions")));

    // Deduplicate by session_key (first occurrence wins — workspaces scanned first)
    let mut seen = std::collections::HashSet::new();
    sessions.retain(|s| seen.insert(s.session_key.clone()));

    sessions.sort_by(|a, b| b.last_modified.cmp(&a.last_modified));
    Json(sessions)
}

async fn get_session_messages(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> Result<Json<Vec<SessionMessage>>, axum::http::StatusCode> {
    let path = find_session_file(&state.root, &key).ok_or(axum::http::StatusCode::NOT_FOUND)?;
    let content = std::fs::read_to_string(&path).map_err(|_| axum::http::StatusCode::NOT_FOUND)?;

    let messages: Vec<SessionMessage> = content
        .lines()
        .filter_map(|line| {
            let val: serde_json::Value = serde_json::from_str(line).ok()?;
            let entry_type = val["type"].as_str()?;
            if entry_type != "message" {
                return None;
            }
            let msg = &val["message"];
            let content_text = match &msg["content"] {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Array(arr) => arr
                    .iter()
                    .filter_map(|part| part["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => String::new(),
            };
            Some(SessionMessage {
                role: msg["role"].as_str().unwrap_or("unknown").to_string(),
                text: content_text,
                timestamp: val["timestamp"].as_str().unwrap_or("").to_string(),
            })
        })
        .collect();

    Ok(Json(messages))
}

async fn reset_session(
    State(state): State<AppState>,
    Path(key): Path<String>,
) -> Result<Json<serde_json::Value>, axum::http::StatusCode> {
    let path = find_session_file(&state.root, &key).ok_or(axum::http::StatusCode::NOT_FOUND)?;
    std::fs::remove_file(&path).map_err(|_| axum::http::StatusCode::NOT_FOUND)?;
    Ok(Json(serde_json::json!({ "status": "reset", "key": key })))
}
