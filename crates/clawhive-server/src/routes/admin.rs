use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/reload-config", post(reload_config))
        .route("/config-status", get(config_status))
}

pub async fn reload_config(State(state): State<AppState>) -> Response {
    let Some(coordinator) = state.reload_coordinator.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "reload not available" })),
        )
            .into_response();
    };

    match coordinator.reload().await {
        Ok(outcome) => Json(outcome).into_response(),
        Err(error) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": error.to_string() })),
        )
            .into_response(),
    }
}

pub async fn config_status(State(state): State<AppState>) -> Response {
    let Some(coordinator) = state.reload_coordinator.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "reload not available" })),
        )
            .into_response();
    };

    Json(coordinator.config_status()).into_response()
}
