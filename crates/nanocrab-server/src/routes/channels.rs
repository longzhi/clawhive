use axum::{extract::State, routing::get, Json, Router};

use crate::state::AppState;

pub fn router() -> Router<AppState> {
    Router::new().route("/", get(get_channels).put(update_channels))
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
