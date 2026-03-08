use axum::{
    extract::State,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use bcrypt::{hash, verify, DEFAULT_COST};
use clawhive_auth::{AuthProfile, TokenManager};
use serde::{Deserialize, Serialize};

use crate::state::AppState;
use crate::{
    create_session, extract_session_token, is_valid_session, remove_session, SESSION_COOKIE_NAME,
};

#[derive(Debug, Serialize)]
pub struct AuthStatusResponse {
    pub active_profile: Option<String>,
    pub profiles: Vec<AuthProfileItem>,
}

#[derive(Debug, Serialize)]
pub struct AuthProfileItem {
    pub name: String,
    pub provider: String,
    pub kind: String,
    pub active: bool,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/status", get(status))
        .route("/login", post(login))
        .route("/check", get(check))
        .route("/logout", post(logout))
        .route("/set-password", post(set_password))
}

#[derive(Debug, Deserialize)]
struct PasswordRequest {
    password: String,
}

#[derive(Debug, Serialize)]
struct CheckResponse {
    authenticated: bool,
    auth_required: bool,
}

fn make_set_cookie(token: &str) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE_NAME}={token}; HttpOnly; Path=/; SameSite=Lax"
    ))
    .unwrap_or_else(|_| HeaderValue::from_static(""))
}

fn make_clear_cookie() -> HeaderValue {
    HeaderValue::from_static("clawhive_session=; HttpOnly; Path=/; SameSite=Lax; Max-Age=0")
}

fn write_web_password_hash(root: &std::path::Path, password_hash: &str) -> Result<(), StatusCode> {
    let path = root.join("config/main.yaml");
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc: serde_yaml::Value = serde_yaml::from_str(&content)
        .unwrap_or_else(|_| serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
    doc["web_password_hash"] = serde_yaml::Value::String(password_hash.to_string());
    let yaml = serde_yaml::to_string(&doc).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    std::fs::write(path, yaml).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(())
}

async fn login(
    State(state): State<AppState>,
    Json(body): Json<PasswordRequest>,
) -> impl IntoResponse {
    let password_hash = state.web_password_hash.read().unwrap();
    let Some(ref hash_str) = *password_hash else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Password not configured" })),
        )
            .into_response();
    };

    let Ok(valid) = verify(&body.password, hash_str) else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "Password verification failed" })),
        )
            .into_response();
    };

    if !valid {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "Invalid password" })),
        )
            .into_response();
    }

    let token = create_session(&state);
    let mut headers = HeaderMap::new();
    headers.insert(header::SET_COOKIE, make_set_cookie(&token));
    (
        StatusCode::OK,
        headers,
        Json(serde_json::json!({ "ok": true })),
    )
        .into_response()
}

async fn check(State(state): State<AppState>, headers: HeaderMap) -> Json<CheckResponse> {
    let auth_required = state.web_password_hash.read().unwrap().is_some();
    if !auth_required {
        return Json(CheckResponse {
            authenticated: false,
            auth_required,
        });
    }

    let authenticated = extract_session_token(&headers)
        .as_deref()
        .is_some_and(|token| is_valid_session(&state, token));
    Json(CheckResponse {
        authenticated,
        auth_required,
    })
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    if let Some(token) = extract_session_token(&headers) {
        remove_session(&state, &token);
    }
    let mut response_headers = HeaderMap::new();
    response_headers.insert(header::SET_COOKIE, make_clear_cookie());
    (
        StatusCode::OK,
        response_headers,
        Json(serde_json::json!({ "ok": true })),
    )
        .into_response()
}

async fn set_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<PasswordRequest>,
) -> impl IntoResponse {
    if body.password.trim().is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "Password must not be empty" })),
        )
            .into_response();
    }

    if state.web_password_hash.read().unwrap().is_some() {
        let authenticated = extract_session_token(&headers)
            .as_deref()
            .is_some_and(|token| is_valid_session(&state, token));
        if !authenticated {
            return (
                StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({ "error": "Authentication required" })),
            )
                .into_response();
        }
    }

    let Ok(password_hash) = hash(&body.password, DEFAULT_COST) else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "Failed to hash password" })),
        )
            .into_response();
    };

    let Ok(()) = write_web_password_hash(&state.root, &password_hash) else {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "Failed to update config" })),
        )
            .into_response();
    };

    // Update in-memory state so auth takes effect immediately without restart
    if let Ok(mut guard) = state.web_password_hash.write() {
        *guard = Some(password_hash.clone());
    }

    // Auto-login: create session and return cookie
    let token = create_session(&state);
    let mut headers = HeaderMap::new();
    headers.insert(header::SET_COOKIE, make_set_cookie(&token));
    (
        StatusCode::OK,
        headers,
        Json(serde_json::json!({ "ok": true })),
    )
        .into_response()
}

async fn status(_state: State<AppState>) -> Json<AuthStatusResponse> {
    let manager = match TokenManager::new() {
        Ok(manager) => manager,
        Err(_) => {
            return Json(AuthStatusResponse {
                active_profile: None,
                profiles: vec![],
            });
        }
    };

    let store = match manager.load_store() {
        Ok(store) => store,
        Err(_) => {
            return Json(AuthStatusResponse {
                active_profile: None,
                profiles: vec![],
            });
        }
    };

    let active = store.active_profile.clone();
    let mut profiles = Vec::with_capacity(store.profiles.len());

    for (name, profile) in store.profiles {
        let (provider, kind) = match profile {
            AuthProfile::ApiKey { provider_id, .. } => (provider_id, "ApiKey".to_string()),
            AuthProfile::OpenAiOAuth { .. } => ("openai".to_string(), "OpenAiOAuth".to_string()),
            AuthProfile::AnthropicSession { .. } => {
                ("anthropic".to_string(), "AnthropicSession".to_string())
            }
        };

        profiles.push(AuthProfileItem {
            active: active.as_deref() == Some(name.as_str()),
            name,
            provider,
            kind,
        });
    }

    Json(AuthStatusResponse {
        active_profile: active,
        profiles,
    })
}
