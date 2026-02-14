use axum::{
    extract::{Path, State},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

use crate::state::AppState;

#[derive(Serialize)]
pub struct ProviderSummary {
    pub provider_id: String,
    pub enabled: bool,
    pub api_base: String,
    pub api_key_env: String,
    pub key_configured: bool,
    pub models: Vec<String>,
}

#[derive(Serialize)]
pub struct TestResult {
    pub ok: bool,
    pub message: String,
}

#[derive(Deserialize)]
pub struct SetKeyRequest {
    pub api_key: String,
}

#[derive(Serialize)]
pub struct SetKeyResult {
    pub ok: bool,
    pub provider_id: String,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(list_providers))
        .route("/{id}", get(get_provider).put(update_provider))
        .route("/{id}/key", post(set_api_key))
        .route("/{id}/test", post(test_provider))
}

async fn list_providers(State(state): State<AppState>) -> Json<Vec<ProviderSummary>> {
    let providers_dir = state.root.join("config/providers.d");
    let mut providers = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&providers_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
                continue;
            }
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(val) = serde_yaml::from_str::<serde_yaml::Value>(&content) {
                    let api_key_env = val["api_key_env"].as_str().unwrap_or("").to_string();
                    let has_direct_key = val["api_key"]
                        .as_str()
                        .map(|k| !k.is_empty())
                        .unwrap_or(false);
                    let key_configured =
                        has_direct_key || std::env::var(&api_key_env).is_ok();

                    providers.push(ProviderSummary {
                        provider_id: val["provider_id"].as_str().unwrap_or("").to_string(),
                        enabled: val["enabled"].as_bool().unwrap_or(false),
                        api_base: val["api_base"].as_str().unwrap_or("").to_string(),
                        api_key_env,
                        key_configured,
                        models: val["models"]
                            .as_sequence()
                            .map(|seq| {
                                seq.iter()
                                    .filter_map(|v| v.as_str().map(String::from))
                                    .collect()
                            })
                            .unwrap_or_default(),
                    });
                }
            }
        }
    }

    Json(providers)
}

async fn get_provider(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, axum::http::StatusCode> {
    let path = state.root.join(format!("config/providers.d/{id}.yaml"));
    let content =
        std::fs::read_to_string(&path).map_err(|_| axum::http::StatusCode::NOT_FOUND)?;
    let val: serde_yaml::Value =
        serde_yaml::from_str(&content).map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    let json =
        serde_json::to_value(val).map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(json))
}

async fn update_provider(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(provider): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, axum::http::StatusCode> {
    let path = state.root.join(format!("config/providers.d/{id}.yaml"));
    let yaml_val: serde_yaml::Value = serde_json::from_value(provider.clone())
        .map_err(|_| axum::http::StatusCode::BAD_REQUEST)?;
    let yaml = serde_yaml::to_string(&yaml_val)
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    std::fs::write(&path, yaml).map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(provider))
}

async fn set_api_key(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SetKeyRequest>,
) -> Result<Json<SetKeyResult>, axum::http::StatusCode> {
    let path = state.root.join(format!("config/providers.d/{id}.yaml"));
    let content =
        std::fs::read_to_string(&path).map_err(|_| axum::http::StatusCode::NOT_FOUND)?;
    let mut val: serde_yaml::Value = serde_yaml::from_str(&content)
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;

    val["api_key"] = serde_yaml::Value::String(body.api_key);

    let yaml =
        serde_yaml::to_string(&val).map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    std::fs::write(&path, yaml).map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;

    tracing::info!("API key written to config for provider {id}");

    Ok(Json(SetKeyResult {
        ok: true,
        provider_id: id,
    }))
}

async fn test_provider(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<TestResult> {
    let path = state.root.join(format!("config/providers.d/{id}.yaml"));
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => {
            return Json(TestResult {
                ok: false,
                message: "Provider config not found".to_string(),
            })
        }
    };

    let val: serde_yaml::Value = match serde_yaml::from_str(&content) {
        Ok(v) => v,
        Err(_) => {
            return Json(TestResult {
                ok: false,
                message: "Invalid YAML".to_string(),
            })
        }
    };

    let has_direct_key = val["api_key"]
        .as_str()
        .map(|k| !k.is_empty())
        .unwrap_or(false);
    let api_key_env = val["api_key_env"].as_str().unwrap_or("");
    let has_env_key = std::env::var(api_key_env).is_ok();

    if !has_direct_key && !has_env_key {
        return Json(TestResult {
            ok: false,
            message: "API key not configured".to_string(),
        });
    }

    Json(TestResult {
        ok: true,
        message: "API key configured".to_string(),
    })
}
