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

fn validate_callback(query: CallbackQuery, expected_state: &str) -> std::result::Result<OAuthCallback, (StatusCode, String)> {
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
    use super::{validate_callback, CallbackQuery};
    use axum::http::StatusCode;

    #[test]
    fn validate_callback_accepts_valid_query() {
        let query = CallbackQuery {
            code: Some("code-123".to_string()),
            state: Some("state-abc".to_string()),
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
        };

        let err = validate_callback(query, "state-abc").expect_err("state mismatch should fail");
        assert_eq!(err.0, StatusCode::UNAUTHORIZED);
    }
}
