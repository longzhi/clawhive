use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use axum::extract::{Query, State};
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::{http::StatusCode, Router};
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio::sync::{oneshot, Mutex};

pub const OAUTH_CALLBACK_ADDR: &str = "127.0.0.1:1455";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthCallback {
    pub code: String,
    pub state: String,
}

#[derive(Debug, Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Clone)]
struct CallbackState {
    expected_state: String,
    callback_tx: Arc<Mutex<Option<oneshot::Sender<OAuthCallback>>>>,
    shutdown_tx: tokio::sync::broadcast::Sender<()>,
}

pub async fn wait_for_oauth_callback(
    expected_state: impl Into<String>,
    timeout: Duration,
) -> Result<OAuthCallback> {
    let expected_state = expected_state.into();
    let (callback_tx, callback_rx) = oneshot::channel::<OAuthCallback>();
    let (shutdown_tx, shutdown_rx) = tokio::sync::broadcast::channel::<()>(1);

    let app_state = CallbackState {
        expected_state,
        callback_tx: Arc::new(Mutex::new(Some(callback_tx))),
        shutdown_tx: shutdown_tx.clone(),
    };

    let app = Router::new()
        .route("/auth/callback", get(handle_callback))
        .with_state(app_state);

    let listener = TcpListener::bind(OAUTH_CALLBACK_ADDR)
        .await
        .with_context(|| format!("failed to bind callback server at {OAUTH_CALLBACK_ADDR}"))?;

    let server_task = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let mut rx = shutdown_rx;
                let _ = rx.recv().await;
            })
            .await
    });

    let callback = tokio::select! {
        result = callback_rx => {
            match result {
                Ok(cb) => Ok(cb),
                Err(_) => Err(anyhow!("callback channel closed before receiving OAuth code")),
            }
        }
        _ = tokio::time::sleep(timeout) => {
            Err(anyhow!("timed out waiting for OAuth callback"))
        }
    };

    let _ = shutdown_tx.send(());
    let _ = server_task.await;

    callback
}

pub fn parse_oauth_callback_input(input: &str, expected_state: &str) -> Result<OAuthCallback> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(anyhow!(
            "empty callback input; paste the full redirected URL or code=...&state=..."
        ));
    }

    let query = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        let url = reqwest::Url::parse(trimmed).context("failed to parse pasted callback URL")?;
        CallbackQuery {
            code: url
                .query_pairs()
                .find(|(k, _)| k == "code")
                .map(|(_, v)| v.into_owned()),
            state: url
                .query_pairs()
                .find(|(k, _)| k == "state")
                .map(|(_, v)| v.into_owned()),
            error: url
                .query_pairs()
                .find(|(k, _)| k == "error")
                .map(|(_, v)| v.into_owned()),
            error_description: url
                .query_pairs()
                .find(|(k, _)| k == "error_description")
                .map(|(_, v)| v.into_owned()),
        }
    } else {
        let query = trimmed.trim_start_matches('?');
        let url = reqwest::Url::parse(&format!("http://localhost/auth/callback?{query}"))
            .context("failed to parse pasted callback query")?;
        CallbackQuery {
            code: url
                .query_pairs()
                .find(|(k, _)| k == "code")
                .map(|(_, v)| v.into_owned()),
            state: url
                .query_pairs()
                .find(|(k, _)| k == "state")
                .map(|(_, v)| v.into_owned()),
            error: url
                .query_pairs()
                .find(|(k, _)| k == "error")
                .map(|(_, v)| v.into_owned()),
            error_description: url
                .query_pairs()
                .find(|(k, _)| k == "error_description")
                .map(|(_, v)| v.into_owned()),
        }
    };

    validate_callback(query, expected_state).map_err(|(_, message)| anyhow!(message))
}

async fn handle_callback(
    State(state): State<CallbackState>,
    Query(query): Query<CallbackQuery>,
) -> impl IntoResponse {
    match validate_callback(query, &state.expected_state) {
        Ok(callback) => {
            if let Some(tx) = state.callback_tx.lock().await.take() {
                let _ = tx.send(callback);
            }
            let _ = state.shutdown_tx.send(());
            (
                StatusCode::OK,
                Html("<h1>Authentication successful</h1><p>You can close this window.</p>"),
            )
                .into_response()
        }
        Err((status, message)) => (status, Html(message)).into_response(),
    }
}

fn validate_callback(
    query: CallbackQuery,
    expected_state: &str,
) -> std::result::Result<OAuthCallback, (StatusCode, String)> {
    // Check for OAuth error response first
    if let Some(error) = &query.error {
        let desc = query
            .error_description
            .as_deref()
            .unwrap_or("no description");
        return Err((
            StatusCode::BAD_REQUEST,
            format!("OAuth error: {error} \u{2014} {desc}"),
        ));
    }
    let code = query
        .code
        .filter(|v| !v.is_empty())
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "missing OAuth code".to_string()))?;
    let state = query
        .state
        .filter(|v| !v.is_empty())
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "missing OAuth state".to_string()))?;

    if state != expected_state {
        return Err((
            StatusCode::UNAUTHORIZED,
            "state mismatch for OAuth callback".to_string(),
        ));
    }

    Ok(OAuthCallback { code, state })
}

#[cfg(test)]
mod tests {
    use super::{parse_oauth_callback_input, validate_callback, CallbackQuery};
    use axum::http::StatusCode;

    #[test]
    fn validate_callback_accepts_valid_query() {
        let query = CallbackQuery {
            code: Some("code-123".to_string()),
            state: Some("state-abc".to_string()),
            error: None,
            error_description: None,
        };

        let callback = validate_callback(query, "state-abc").expect("valid callback");
        assert_eq!(callback.code, "code-123");
        assert_eq!(callback.state, "state-abc");
    }

    #[test]
    fn validate_callback_rejects_state_mismatch() {
        let query = CallbackQuery {
            code: Some("code-123".to_string()),
            state: Some("wrong-state".to_string()),
            error: None,
            error_description: None,
        };

        let err = validate_callback(query, "state-abc").expect_err("state mismatch should fail");
        assert_eq!(err.0, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn parse_oauth_callback_input_accepts_full_url() {
        let callback = parse_oauth_callback_input(
            "http://localhost:1455/auth/callback?code=code-123&state=state-abc",
            "state-abc",
        )
        .expect("full URL should parse");

        assert_eq!(callback.code, "code-123");
        assert_eq!(callback.state, "state-abc");
    }

    #[test]
    fn parse_oauth_callback_input_accepts_query_string() {
        let callback = parse_oauth_callback_input("code=code-123&state=state-abc", "state-abc")
            .expect("query string should parse");

        assert_eq!(callback.code, "code-123");
        assert_eq!(callback.state, "state-abc");
    }

    #[test]
    fn parse_oauth_callback_input_rejects_missing_state() {
        let err = parse_oauth_callback_input("code=code-123", "state-abc")
            .expect_err("missing state should fail");

        assert!(err.to_string().contains("missing OAuth state"));
    }
}
