pub mod frontend;
pub mod routes;
pub mod state;
pub mod webhook_auth;

use anyhow::Result;
use axum::{
    extract::{Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    Json, Router,
};
use std::time::{Duration, Instant};
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;

use crate::state::AppState;

pub(crate) const SESSION_COOKIE_NAME: &str = "clawhive_session";
pub(crate) const SESSION_TTL: Duration = Duration::from_secs(24 * 60 * 60);
pub(crate) const SETUP_COOKIE_NAME: &str = "clawhive_setup";
pub(crate) const SETUP_TTL: Duration = Duration::from_secs(30 * 60);

pub fn create_router(state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        .nest("/api", routes::api_router())
        .nest("/hook", routes::webhook::webhook_router())
        .fallback(frontend::frontend_handler)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

fn is_exempt_path(path: &str, state: &AppState) -> bool {
    if path.starts_with("/hook/") {
        return true;
    }

    // Setup status and auth endpoints are always exempt
    if path.starts_with("/api/setup")
        || path == "/api/auth/status"
        || path == "/api/auth/login"
        || path == "/api/auth/check"
        || (path == "/api/auth/set-password" && state.web_password_hash.read().unwrap().is_none())
    {
        return true;
    }

    // During initial setup (no providers or no active agents), allow the
    // write endpoints that the setup wizard needs so users can complete
    // configuration before authentication is possible.
    if is_needs_setup(state) && is_setup_wizard_path(path) {
        return true;
    }

    false
}

fn is_setup_wizard_path(path: &str) -> bool {
    let setup_paths = [
        "/api/providers",
        "/api/agents",
        "/api/channels/",
        "/api/routing",
        "/api/auth/openai/",
    ];
    setup_paths.iter().any(|p| path.starts_with(p))
}

/// Quick check: system needs setup if no provider yaml or no enabled agent.
fn is_needs_setup(state: &AppState) -> bool {
    let providers_dir = state.root.join("config/providers.d");
    let has_providers = std::fs::read_dir(&providers_dir)
        .map(|entries| {
            entries
                .flatten()
                .any(|e| e.path().extension().and_then(|x| x.to_str()) == Some("yaml"))
        })
        .unwrap_or(false);
    if !has_providers {
        return true;
    }

    let agents_dir = state.root.join("config/agents.d");
    let has_active_agents = std::fs::read_dir(&agents_dir)
        .map(|entries| {
            entries.flatten().any(|e| {
                let path = e.path();
                if path.extension().and_then(|x| x.to_str()) != Some("yaml") {
                    return false;
                }
                std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|c| serde_yaml::from_str::<serde_yaml::Value>(&c).ok())
                    .map(|v| v["enabled"].as_bool().unwrap_or(false))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);
    !has_active_agents
}

fn unauthorized_response() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({ "error": "Authentication required" })),
    )
        .into_response()
}

fn extract_cookie_token(headers: &HeaderMap, cookie_name: &str) -> Option<String> {
    let cookie_header = headers.get(header::COOKIE)?.to_str().ok()?;
    for pair in cookie_header.split(';') {
        let mut parts = pair.trim().splitn(2, '=');
        let Some(name) = parts.next() else {
            continue;
        };
        let Some(value) = parts.next() else {
            continue;
        };
        if name == cookie_name {
            return Some(value.to_string());
        }
    }
    None
}

pub(crate) fn extract_session_token(headers: &HeaderMap) -> Option<String> {
    extract_cookie_token(headers, SESSION_COOKIE_NAME)
}

pub(crate) fn extract_setup_token(headers: &HeaderMap) -> Option<String> {
    extract_cookie_token(headers, SETUP_COOKIE_NAME)
}

fn create_expiring_token(state: &AppState, ttl: Duration) -> String {
    let token = uuid::Uuid::new_v4().to_string();
    let expires_at = Instant::now() + ttl;
    if let Ok(mut sessions) = state.session_store.write() {
        sessions.insert(token.clone(), expires_at);
    }
    token
}

pub(crate) fn create_session(state: &AppState) -> String {
    create_expiring_token(state, SESSION_TTL)
}

pub(crate) fn create_setup_session(state: &AppState) -> String {
    create_expiring_token(state, SETUP_TTL)
}

pub(crate) fn remove_session(state: &AppState, token: &str) {
    if let Ok(mut sessions) = state.session_store.write() {
        sessions.remove(token);
    }
}

pub(crate) fn is_valid_session(state: &AppState, token: &str) -> bool {
    let now = Instant::now();
    let Ok(mut sessions) = state.session_store.write() else {
        return false;
    };
    sessions.retain(|_, expires_at| *expires_at > now);
    sessions
        .get(token)
        .is_some_and(|expires_at| *expires_at > now)
}

async fn auth_middleware(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let path = request.uri().path();

    if !path.starts_with("/api/")
        || is_exempt_path(path, &state)
        || state.web_password_hash.read().unwrap().is_none()
    {
        return next.run(request).await;
    }

    if is_setup_wizard_path(path)
        && extract_setup_token(request.headers())
            .as_deref()
            .is_some_and(|token| is_valid_session(&state, token))
    {
        return next.run(request).await;
    }

    let Some(token) = extract_session_token(request.headers()) else {
        return unauthorized_response();
    };

    if !is_valid_session(&state, &token) {
        return unauthorized_response();
    }

    next.run(request).await
}

pub async fn serve(state: AppState, addr: &str) -> Result<()> {
    let app = create_router(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("clawhive-server listening on {}", addr);
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
    };
    use clawhive_bus::EventBus;
    use tower::ServiceExt;

    use crate::{create_router, state::AppState};

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
                web_password_hash: Arc::new(std::sync::RwLock::new(web_password_hash)),
                session_store: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
                pending_openai_oauth: Arc::new(std::sync::RwLock::new(
                    std::collections::HashMap::new(),
                )),
                openai_oauth_config: crate::state::default_openai_oauth_config(),
                enable_openai_oauth_callback_listener: true,
                daemon_mode: false,
                port: 3000,
                webhook_config: Arc::new(std::sync::RwLock::new(None)),
                routing_config: Arc::new(std::sync::RwLock::new(None)),
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
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
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
    async fn openai_oauth_start_is_public_during_initial_setup() {
        let (mut state, _tmp) = setup_state(Some(hash_password("correct")));
        state.enable_openai_oauth_callback_listener = false;
        let app = create_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/openai/start")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn setup_status_issues_setup_cookie_when_initial_setup_is_needed() {
        let (state, _tmp) = setup_state(Some(hash_password("correct")));
        let app = create_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/setup/status")
                    .body(Body::empty())
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
        assert!(set_cookie.contains("clawhive_setup="));
    }

    #[tokio::test]
    async fn setup_cookie_allows_setup_wizard_routes_after_setup_minimum_is_met() {
        let (state, tmp) = setup_state(Some(hash_password("correct")));
        let app = create_router(state);

        let setup_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/setup/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let cookie = cookie_pair(
            setup_response
                .headers()
                .get("set-cookie")
                .unwrap()
                .to_str()
                .unwrap(),
        );

        std::fs::create_dir_all(tmp.path().join("config/providers.d")).unwrap();
        std::fs::create_dir_all(tmp.path().join("config/agents.d")).unwrap();
        std::fs::write(
            tmp.path().join("config/providers.d/openai.yaml"),
            "provider_id: openai\napi_base: https://api.openai.com/v1\nmodels: [gpt-5]\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("config/agents.d/main.yaml"),
            "enabled: true\nidentity:\n  name: Test\n  emoji: \"🐝\"\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("config/routing.yaml"),
            "bindings: []\ndefault_agent_id: null\n",
        )
        .unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/routing")
                    .header("cookie", cookie)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }
}
