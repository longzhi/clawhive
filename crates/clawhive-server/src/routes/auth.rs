use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use bcrypt::{hash, verify, DEFAULT_COST};
use clawhive_auth::oauth::{
    build_authorize_url, exchange_code_for_tokens, extract_chatgpt_account_id, generate_pkce_pair,
    server::parse_oauth_callback_input, start_oauth_callback_listener,
};
use clawhive_auth::{AuthProfile, TokenManager};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::state::{AppState, PendingOpenAiOAuth};
use crate::{
    create_session, extract_session_token, is_valid_session, remove_session, SESSION_COOKIE_NAME,
};

const OPENAI_OAUTH_PROFILE_NAME: &str = "openai-oauth";
const OPENAI_OAUTH_FLOW_TTL: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Serialize, Deserialize)]
pub struct AuthStatusResponse {
    pub active_profile: Option<String>,
    pub profiles: Vec<AuthProfileItem>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AuthProfileItem {
    pub name: String,
    pub provider: String,
    pub kind: String,
    pub active: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OpenAiOAuthStartResponse {
    pub flow_id: String,
    pub authorize_url: String,
    pub profile_name: String,
    pub replaces_existing: bool,
}

#[derive(Debug, Deserialize)]
struct OpenAiOAuthCompleteRequest {
    flow_id: String,
    #[serde(default)]
    callback_input: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OpenAiOAuthCompleteResponse {
    pub profile_name: String,
    pub chatgpt_account_id: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OpenAiOAuthFlowStatusResponse {
    pub flow_id: String,
    pub callback_listener_active: bool,
    pub callback_captured: bool,
    pub message: Option<String>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/status", get(status))
        .route("/login", post(login))
        .route("/check", get(check))
        .route("/logout", post(logout))
        .route("/openai/start", post(start_openai_oauth))
        .route("/openai/flow/{flow_id}", get(openai_oauth_flow_status))
        .route("/openai/complete", post(complete_openai_oauth))
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

fn json_error(status: StatusCode, message: impl Into<String>) -> axum::response::Response {
    (status, Json(serde_json::json!({ "error": message.into() }))).into_response()
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

fn prune_pending_openai_oauth(state: &AppState) {
    let now = Instant::now();
    if let Ok(mut flows) = state.pending_openai_oauth.write() {
        flows.retain(|_, flow| now.duration_since(flow.created_at) < OPENAI_OAUTH_FLOW_TTL);
    }
}

fn stop_openai_oauth_listener(flow: &mut PendingOpenAiOAuth) {
    flow.callback_listener_active = false;
    if let Some(shutdown_tx) = flow.callback_listener_shutdown.take() {
        let _ = shutdown_tx.send(());
    }
}

async fn start_openai_oauth(State(state): State<AppState>) -> impl IntoResponse {
    prune_pending_openai_oauth(&state);

    let pkce = generate_pkce_pair();
    let oauth_state = uuid::Uuid::new_v4().to_string();
    let flow_id = uuid::Uuid::new_v4().to_string();
    let authorize_url =
        build_authorize_url(&state.openai_oauth_config, &pkce.challenge, &oauth_state);
    let manager = TokenManager::from_config_dir(state.root.join("config"));
    let replaces_existing = manager
        .get_profile(OPENAI_OAUTH_PROFILE_NAME)
        .ok()
        .flatten()
        .is_some();

    {
        let Ok(mut flows) = state.pending_openai_oauth.write() else {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to create OpenAI OAuth flow",
            );
        };
        flows.insert(
            flow_id.clone(),
            PendingOpenAiOAuth {
                expected_state: oauth_state.clone(),
                code_verifier: pkce.verifier,
                created_at: Instant::now(),
                callback_code: None,
                callback_listener_active: false,
                callback_listener_message: None,
                callback_listener_shutdown: None,
            },
        );
    }

    if state.enable_openai_oauth_callback_listener {
        match start_oauth_callback_listener(oauth_state.clone()).await {
            Ok(listener) => {
                let shutdown_tx = listener.shutdown_sender();
                if let Ok(mut flows) = state.pending_openai_oauth.write() {
                    if let Some(flow) = flows.get_mut(&flow_id) {
                        flow.callback_listener_active = true;
                        flow.callback_listener_message =
                            Some("Waiting for a localhost callback on this machine...".to_string());
                        flow.callback_listener_shutdown = Some(shutdown_tx);
                    }
                }

                let flow_id_for_task = flow_id.clone();
                let state_for_task = state.clone();
                tokio::spawn(async move {
                    match listener.wait(OPENAI_OAUTH_FLOW_TTL).await {
                        Ok(callback) => {
                            if let Ok(mut flows) = state_for_task.pending_openai_oauth.write() {
                                if let Some(flow) = flows.get_mut(&flow_id_for_task) {
                                    flow.callback_code = Some(callback.code);
                                    flow.callback_listener_active = false;
                                    flow.callback_listener_message = Some(
                                        "Local callback received. Finalizing login...".to_string(),
                                    );
                                    flow.callback_listener_shutdown = None;
                                }
                            }
                        }
                        Err(err) => {
                            if let Ok(mut flows) = state_for_task.pending_openai_oauth.write() {
                                if let Some(flow) = flows.get_mut(&flow_id_for_task) {
                                    flow.callback_listener_active = false;
                                    flow.callback_listener_shutdown = None;
                                    if flow.callback_code.is_none() {
                                        flow.callback_listener_message = Some(format!(
                                            "Automatic localhost callback was not completed: {err}. You can still paste the callback URL manually."
                                        ));
                                    }
                                }
                            }
                        }
                    }
                });
            }
            Err(err) => {
                if let Ok(mut flows) = state.pending_openai_oauth.write() {
                    if let Some(flow) = flows.get_mut(&flow_id) {
                        flow.callback_listener_message = Some(format!(
                            "Could not start localhost callback listener: {err}. You can still paste the callback URL manually."
                        ));
                    }
                }
            }
        }
    }

    Json(OpenAiOAuthStartResponse {
        flow_id,
        authorize_url,
        profile_name: OPENAI_OAUTH_PROFILE_NAME.to_string(),
        replaces_existing,
    })
    .into_response()
}

async fn openai_oauth_flow_status(
    State(state): State<AppState>,
    Path(flow_id): Path<String>,
) -> impl IntoResponse {
    prune_pending_openai_oauth(&state);

    let Ok(flows) = state.pending_openai_oauth.read() else {
        return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to read OpenAI OAuth flow state",
        );
    };

    let Some(flow) = flows.get(flow_id.trim()) else {
        return json_error(
            StatusCode::NOT_FOUND,
            "OpenAI OAuth flow expired or was not found. Start again.",
        );
    };

    Json(OpenAiOAuthFlowStatusResponse {
        flow_id,
        callback_listener_active: flow.callback_listener_active,
        callback_captured: flow.callback_code.is_some(),
        message: flow.callback_listener_message.clone(),
    })
    .into_response()
}

async fn complete_openai_oauth(
    State(state): State<AppState>,
    Json(body): Json<OpenAiOAuthCompleteRequest>,
) -> impl IntoResponse {
    if body.flow_id.trim().is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "Missing OpenAI OAuth flow id");
    }

    prune_pending_openai_oauth(&state);

    let flow = {
        let Ok(flows) = state.pending_openai_oauth.read() else {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to read OpenAI OAuth flow state",
            );
        };
        flows.get(body.flow_id.trim()).cloned()
    };

    let Some(flow) = flow else {
        return json_error(
            StatusCode::NOT_FOUND,
            "OpenAI OAuth flow expired or was not found. Start again.",
        );
    };

    let callback_code = if let Some(callback_input) = body
        .callback_input
        .as_deref()
        .map(str::trim)
        .filter(|input| !input.is_empty())
    {
        match parse_oauth_callback_input(callback_input, &flow.expected_state) {
            Ok(callback) => callback.code,
            Err(err) => {
                let message = err.to_string();
                let status = if message.contains("state mismatch") {
                    StatusCode::UNAUTHORIZED
                } else {
                    StatusCode::BAD_REQUEST
                };
                return json_error(status, message);
            }
        }
    } else if let Some(callback_code) = flow.callback_code.clone() {
        callback_code
    } else {
        return json_error(
            StatusCode::BAD_REQUEST,
            "OpenAI callback not received yet. Finish the browser login or paste the callback URL manually.",
        );
    };

    if let Ok(mut flows) = state.pending_openai_oauth.write() {
        if let Some(flow) = flows.get_mut(body.flow_id.trim()) {
            stop_openai_oauth_listener(flow);
            flow.callback_listener_message = Some("Completing OpenAI login...".to_string());
        }
    }

    let http = match reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
    {
        Ok(client) => client,
        Err(err) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to initialize HTTP client: {err}"),
            );
        }
    };

    let token = match exchange_code_for_tokens(
        &http,
        &state.openai_oauth_config.token_endpoint,
        &state.openai_oauth_config.client_id,
        &state.openai_oauth_config.redirect_uri,
        &callback_code,
        &flow.code_verifier,
    )
    .await
    {
        Ok(token) => token,
        Err(err) => {
            if let Ok(mut flows) = state.pending_openai_oauth.write() {
                if let Some(flow) = flows.get_mut(body.flow_id.trim()) {
                    flow.callback_listener_message =
                        Some(format!("OpenAI token exchange failed: {err}"));
                }
            }
            return json_error(StatusCode::BAD_GATEWAY, err.to_string());
        }
    };

    let chatgpt_account_id = extract_chatgpt_account_id(&token.access_token);
    let manager = TokenManager::from_config_dir(state.root.join("config"));
    let expires_at = match now_unix_ts() {
        Ok(now) => now + token.expires_in,
        Err(err) => {
            return json_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to compute token expiry: {err}"),
            );
        }
    };

    if let Err(err) = manager.save_profile(
        OPENAI_OAUTH_PROFILE_NAME,
        AuthProfile::OpenAiOAuth {
            access_token: token.access_token,
            refresh_token: token.refresh_token,
            expires_at,
            chatgpt_account_id: chatgpt_account_id.clone(),
        },
    ) {
        return json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to save OpenAI OAuth profile: {err}"),
        );
    }

    if let Ok(mut flows) = state.pending_openai_oauth.write() {
        flows.remove(body.flow_id.trim());
    }

    Json(OpenAiOAuthCompleteResponse {
        profile_name: OPENAI_OAUTH_PROFILE_NAME.to_string(),
        chatgpt_account_id,
    })
    .into_response()
}

async fn status(State(state): State<AppState>) -> Json<AuthStatusResponse> {
    let manager = TokenManager::from_config_dir(state.root.join("config"));
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

fn now_unix_ts() -> Result<i64, std::time::SystemTimeError> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, RwLock};
    use std::time::Instant;

    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
    };
    use clawhive_auth::oauth::OpenAiOAuthConfig;
    use clawhive_auth::TokenManager;
    use clawhive_bus::EventBus;
    use serde_json::Value;
    use tower::ServiceExt;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{
        router, OpenAiOAuthCompleteResponse, OpenAiOAuthFlowStatusResponse,
        OpenAiOAuthStartResponse, OPENAI_OAUTH_PROFILE_NAME,
    };
    use crate::state::{default_openai_oauth_config, AppState};
    use crate::{create_router, state::PendingOpenAiOAuth};

    fn setup_state(web_password_hash: Option<String>) -> (AppState, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("config")).unwrap();
        std::fs::write(root.join("config/main.yaml"), "app_name: test\n").unwrap();

        (
            AppState {
                root: root.to_path_buf(),
                bus: Arc::new(EventBus::new(16)),
                gateway: None,
                web_password_hash: Arc::new(RwLock::new(web_password_hash)),
                session_store: Arc::new(RwLock::new(HashMap::new())),
                pending_openai_oauth: Arc::new(RwLock::new(HashMap::new())),
                openai_oauth_config: default_openai_oauth_config(),
                enable_openai_oauth_callback_listener: false,
                daemon_mode: false,
                port: 3000,
            },
            tmp,
        )
    }

    fn hash_password(password: &str) -> String {
        bcrypt::hash(password, bcrypt::DEFAULT_COST).unwrap()
    }

    fn cookie_pair(set_cookie: &str) -> String {
        set_cookie
            .split(';')
            .next()
            .map(ToOwned::to_owned)
            .unwrap_or_default()
    }

    fn test_openai_config(server: &MockServer) -> OpenAiOAuthConfig {
        OpenAiOAuthConfig {
            client_id: "client-123".to_string(),
            redirect_uri: "http://localhost:1455/auth/callback".to_string(),
            authorize_endpoint: format!("{}/oauth/authorize", server.uri()),
            token_endpoint: format!("{}/oauth/token", server.uri()),
            scope: "openid profile email offline_access".to_string(),
            originator: "clawhive".to_string(),
        }
    }

    fn pending_flow() -> PendingOpenAiOAuth {
        PendingOpenAiOAuth {
            expected_state: "expected-state".to_string(),
            code_verifier: "verifier-123".to_string(),
            created_at: Instant::now(),
            callback_code: None,
            callback_listener_active: false,
            callback_listener_message: None,
            callback_listener_shutdown: None,
        }
    }

    #[tokio::test]
    async fn login_success_with_correct_password() {
        let (state, _tmp) = setup_state(Some(hash_password("correct")));
        let app = create_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/login")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"password":"correct"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let set_cookie = response
            .headers()
            .get("set-cookie")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        assert!(set_cookie.contains("clawhive_session="));
    }

    #[tokio::test]
    async fn login_failure_with_wrong_password() {
        let (state, _tmp) = setup_state(Some(hash_password("correct")));
        let app = create_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/login")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"password":"wrong"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn auth_status_is_public_even_when_password_is_set() {
        let (state, _tmp) = setup_state(Some(hash_password("correct")));
        let app = create_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/auth/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn protected_route_returns_401_without_cookie() {
        let (state, _tmp) = setup_state(Some(hash_password("correct")));
        let app = create_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/events/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"], "Authentication required");
    }

    #[tokio::test]
    async fn protected_route_succeeds_with_valid_session_cookie() {
        let (state, _tmp) = setup_state(Some(hash_password("correct")));
        let app = create_router(state);

        let login_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/login")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"password":"correct"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        let cookie = cookie_pair(
            login_response
                .headers()
                .get("set-cookie")
                .unwrap()
                .to_str()
                .unwrap(),
        );

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/events/metrics")
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn auth_bypass_when_no_password_configured() {
        let (state, _tmp) = setup_state(None);
        let app = create_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/auth/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn start_openai_oauth_returns_authorize_url_and_tracks_flow() {
        let server = MockServer::start().await;
        let (mut state, _tmp) = setup_state(None);
        state.openai_oauth_config = test_openai_config(&server);

        let app = router().with_state(state.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/openai/start")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: OpenAiOAuthStartResponse = serde_json::from_slice(&body).unwrap();

        assert_eq!(payload.profile_name, OPENAI_OAUTH_PROFILE_NAME);
        assert!(payload.authorize_url.contains("/oauth/authorize"));
        assert!(state
            .pending_openai_oauth
            .read()
            .unwrap()
            .contains_key(&payload.flow_id));
    }

    #[tokio::test]
    async fn complete_openai_oauth_rejects_state_mismatch() {
        let server = MockServer::start().await;
        let (mut state, _tmp) = setup_state(None);
        state.openai_oauth_config = test_openai_config(&server);
        state
            .pending_openai_oauth
            .write()
            .unwrap()
            .insert("flow-1".to_string(), pending_flow());

        let app = router().with_state(state);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/openai/complete")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"flow_id":"flow-1","callback_input":"code=code-123&state=wrong-state"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn complete_openai_oauth_saves_profile() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .and(body_string_contains("grant_type=authorization_code"))
            .and(body_string_contains("code=code-123"))
            .and(body_string_contains("code_verifier=verifier-123"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "access-token",
                "refresh_token": "refresh-token",
                "expires_in": 3600
            })))
            .mount(&server)
            .await;

        let (mut state, _tmp) = setup_state(None);
        state.openai_oauth_config = test_openai_config(&server);
        state
            .pending_openai_oauth
            .write()
            .unwrap()
            .insert("flow-1".to_string(), pending_flow());

        let app = router().with_state(state.clone());
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/openai/complete")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"flow_id":"flow-1","callback_input":"http://localhost:1455/auth/callback?code=code-123&state=expected-state"}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: OpenAiOAuthCompleteResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload.profile_name, OPENAI_OAUTH_PROFILE_NAME);

        let manager = TokenManager::from_config_dir(state.root.join("config"));
        let store = manager.load_store().unwrap();
        assert_eq!(
            store.active_profile.as_deref(),
            Some(OPENAI_OAUTH_PROFILE_NAME)
        );
        assert!(store.profiles.contains_key(OPENAI_OAUTH_PROFILE_NAME));
    }

    #[tokio::test]
    async fn flow_status_reports_captured_callback() {
        let (state, _tmp) = setup_state(None);
        state.pending_openai_oauth.write().unwrap().insert(
            "flow-1".to_string(),
            PendingOpenAiOAuth {
                callback_code: Some("code-123".to_string()),
                callback_listener_message: Some("Captured".to_string()),
                ..pending_flow()
            },
        );

        let app = router().with_state(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/openai/flow/flow-1")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: OpenAiOAuthFlowStatusResponse = serde_json::from_slice(&body).unwrap();
        assert!(payload.callback_captured);
        assert_eq!(payload.message.as_deref(), Some("Captured"));
    }
}
