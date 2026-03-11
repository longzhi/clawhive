use axum::{
    extract::{Path, State},
    routing::{get, post},
    Json, Router,
};
use clawhive_auth::TokenManager;
use serde::{Deserialize, Serialize};

use crate::state::AppState;

#[derive(Serialize)]
pub struct ProviderSummary {
    pub provider_id: String,
    pub enabled: bool,
    pub api_base: String,
    pub key_configured: bool,
    pub auth_profile: Option<String>,
    pub models: Vec<String>,
}

#[derive(Serialize)]
pub struct TestResult {
    pub ok: bool,
    pub message: String,
}

#[derive(Deserialize)]
pub struct CreateProviderRequest {
    pub provider_id: String,
    pub api_base: String,
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub auth_profile: Option<String>,
    #[serde(default)]
    pub models: Vec<String>,
}

#[derive(Serialize)]
pub struct CreateProviderResponse {
    pub provider_id: String,
    pub enabled: bool,
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
        .route("/", get(list_providers).post(create_provider))
        .route(
            "/{id}",
            get(get_provider)
                .put(update_provider)
                .delete(delete_provider),
        )
        .route("/{id}/key", post(set_api_key))
        .route("/{id}/test", post(test_provider))
}

fn token_manager_for_root(root: &std::path::Path) -> TokenManager {
    TokenManager::from_config_dir(root.join("config"))
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
                    let has_direct_key = val["api_key"]
                        .as_str()
                        .map(|k| !k.is_empty())
                        .unwrap_or(false);
                    let key_configured = has_direct_key;

                    providers.push(ProviderSummary {
                        provider_id: val["provider_id"].as_str().unwrap_or("").to_string(),
                        enabled: val["enabled"].as_bool().unwrap_or(false),
                        api_base: val["api_base"].as_str().unwrap_or("").to_string(),
                        key_configured,
                        auth_profile: val["auth_profile"].as_str().map(ToString::to_string),
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

async fn create_provider(
    State(state): State<AppState>,
    Json(body): Json<CreateProviderRequest>,
) -> Result<Json<CreateProviderResponse>, axum::http::StatusCode> {
    if body.provider_id.trim().is_empty() {
        return Err(axum::http::StatusCode::BAD_REQUEST);
    }

    let providers_dir = state.root.join("config/providers.d");
    std::fs::create_dir_all(&providers_dir)
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;

    let path = providers_dir.join(format!("{}.yaml", body.provider_id));

    let mut val = serde_yaml::Mapping::new();
    val.insert(
        serde_yaml::Value::String("provider_id".to_string()),
        serde_yaml::Value::String(body.provider_id.clone()),
    );
    val.insert(
        serde_yaml::Value::String("enabled".to_string()),
        serde_yaml::Value::Bool(true),
    );
    val.insert(
        serde_yaml::Value::String("api_base".to_string()),
        serde_yaml::Value::String(body.api_base),
    );

    if let Some(key) = body.api_key.filter(|key| !key.trim().is_empty()) {
        val.insert(
            serde_yaml::Value::String("api_key".to_string()),
            serde_yaml::Value::String(key),
        );
    }

    if let Some(auth_profile) = body
        .auth_profile
        .filter(|auth_profile| !auth_profile.trim().is_empty())
    {
        val.insert(
            serde_yaml::Value::String("auth_profile".to_string()),
            serde_yaml::Value::String(auth_profile),
        );
    }

    val.insert(
        serde_yaml::Value::String("models".to_string()),
        serde_yaml::Value::Sequence(
            body.models
                .into_iter()
                .map(serde_yaml::Value::String)
                .collect(),
        ),
    );

    let yaml =
        serde_yaml::to_string(&val).map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    std::fs::write(&path, yaml).map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(CreateProviderResponse {
        provider_id: body.provider_id,
        enabled: true,
    }))
}

async fn get_provider(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, axum::http::StatusCode> {
    let path = state.root.join(format!("config/providers.d/{id}.yaml"));
    let content = std::fs::read_to_string(&path).map_err(|_| axum::http::StatusCode::NOT_FOUND)?;
    let val: serde_yaml::Value = serde_yaml::from_str(&content)
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
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
    let content = std::fs::read_to_string(&path).map_err(|_| axum::http::StatusCode::NOT_FOUND)?;
    let mut yaml_val: serde_yaml::Value = serde_yaml::from_str(&content)
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    let patch: serde_yaml::Value = serde_json::from_value(provider.clone())
        .map_err(|_| axum::http::StatusCode::BAD_REQUEST)?;
    merge_top_level_mapping(&mut yaml_val, patch);
    let yaml = serde_yaml::to_string(&yaml_val)
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    std::fs::write(&path, yaml).map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(provider))
}

fn merge_top_level_mapping(existing: &mut serde_yaml::Value, patch: serde_yaml::Value) {
    let Some(existing_map) = existing.as_mapping_mut() else {
        *existing = patch;
        return;
    };
    let Some(patch_map) = patch.as_mapping() else {
        *existing = patch;
        return;
    };

    for (key, value) in patch_map {
        existing_map.insert(key.clone(), value.clone());
    }
}

async fn set_api_key(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SetKeyRequest>,
) -> Result<Json<SetKeyResult>, axum::http::StatusCode> {
    let path = state.root.join(format!("config/providers.d/{id}.yaml"));
    let content = std::fs::read_to_string(&path).map_err(|_| axum::http::StatusCode::NOT_FOUND)?;
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

async fn test_provider(State(state): State<AppState>, Path(id): Path<String>) -> Json<TestResult> {
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

    let auth_profile = val["auth_profile"]
        .as_str()
        .map(str::trim)
        .filter(|profile| !profile.is_empty())
        .map(ToString::to_string);

    if let Some(profile_name) = auth_profile {
        let manager = token_manager_for_root(&state.root);
        return match manager.get_profile(&profile_name) {
            Ok(Some(_)) => Json(TestResult {
                ok: true,
                message: format!("Auth profile '{profile_name}' configured"),
            }),
            Ok(None) => Json(TestResult {
                ok: false,
                message: format!("Auth profile '{profile_name}' not found"),
            }),
            Err(_) => Json(TestResult {
                ok: false,
                message: "Failed to read auth profiles".to_string(),
            }),
        };
    }

    let has_direct_key = val["api_key"]
        .as_str()
        .map(|k| !k.is_empty())
        .unwrap_or(false);

    if !has_direct_key {
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

async fn delete_provider(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<axum::http::StatusCode, axum::http::StatusCode> {
    let path = state.root.join(format!("config/providers.d/{id}.yaml"));
    if !path.exists() {
        return Err(axum::http::StatusCode::NOT_FOUND);
    }
    std::fs::remove_file(&path).map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{body::Body, http::Request};
    use clawhive_auth::{AuthProfile, TokenManager};
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
            &root.join("config/providers.d/openai.yaml"),
            "provider_id: openai\nenabled: true\napi_base: https://api.openai.com/v1\nmodels:\n  - gpt-4o\n",
        );

        (
            AppState {
                root: root.to_path_buf(),
                bus: Arc::new(EventBus::new(16)),
                gateway: None,
                web_password_hash: Arc::new(std::sync::RwLock::new(None)),
                session_store: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
                pending_openai_oauth: Arc::new(std::sync::RwLock::new(
                    std::collections::HashMap::new(),
                )),
                openai_oauth_config: crate::state::default_openai_oauth_config(),
                enable_openai_oauth_callback_listener: true,
                daemon_mode: false,
                port: 3000,
            },
            tmp,
        )
    }

    #[tokio::test]
    async fn delete_provider_returns_204() {
        let (state, _tmp) = setup_state();
        let provider_path = state.root.join("config/providers.d/openai.yaml");
        assert!(provider_path.exists());

        let app = router().with_state(state.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/openai")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::NO_CONTENT);
        assert!(!provider_path.exists());
    }

    #[tokio::test]
    async fn delete_nonexistent_provider_returns_404() {
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

    #[tokio::test]
    async fn create_provider_writes_auth_profile() {
        let (state, _tmp) = setup_state();
        let app = router().with_state(state.clone());

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"provider_id":"openai-chatgpt","api_base":"https://chatgpt.com/backend-api/codex","auth_profile":"openai-oauth","models":["gpt-5.3-codex"]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::OK);

        let content =
            std::fs::read_to_string(state.root.join("config/providers.d/openai-chatgpt.yaml"))
                .unwrap();
        assert!(content.contains("auth_profile: openai-oauth"));
        assert!(!content.contains("api_key:"));
    }

    #[tokio::test]
    async fn update_provider_preserves_existing_auth_profile() {
        let (state, _tmp) = setup_state();
        write_file(
            &state.root.join("config/providers.d/openai-chatgpt.yaml"),
            "provider_id: openai-chatgpt\nenabled: true\napi_base: https://chatgpt.com/backend-api/codex\nauth_profile: openai-oauth\nmodels:\n  - gpt-5.3-codex\n",
        );
        let app = router().with_state(state.clone());

        let response = app
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/openai-chatgpt")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"api_base":"https://chatgpt.com/backend-api/codex","models":["gpt-5.3-codex","gpt-5.2"]}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::OK);

        let content =
            std::fs::read_to_string(state.root.join("config/providers.d/openai-chatgpt.yaml"))
                .unwrap();
        assert!(content.contains("auth_profile: openai-oauth"));
        assert!(content.contains("- gpt-5.2"));
    }

    #[tokio::test]
    async fn test_provider_accepts_configured_auth_profile() {
        let (state, _tmp) = setup_state();
        write_file(
            &state.root.join("config/providers.d/openai-chatgpt.yaml"),
            "provider_id: openai-chatgpt\nenabled: true\napi_base: https://chatgpt.com/backend-api/codex\nauth_profile: openai-oauth\nmodels:\n  - gpt-5.3-codex\n",
        );
        TokenManager::from_config_dir(state.root.join("config"))
            .save_profile(
                "openai-oauth",
                AuthProfile::OpenAiOAuth {
                    access_token: "access-token".to_string(),
                    refresh_token: "refresh-token".to_string(),
                    expires_at: 4_102_444_800,
                    chatgpt_account_id: Some("acct-123".to_string()),
                },
            )
            .unwrap();

        let app = router().with_state(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/openai-chatgpt/test")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::OK);
    }
}
