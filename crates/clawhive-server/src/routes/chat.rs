use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path as AxumPath, State,
    },
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use chrono::Utc;
use clawhive_bus::Topic;
use clawhive_channels::web_console::{
    conversation_id_from_session_key, find_conversation_session_file, is_web_console_user_session,
    map_attachment, parse_conversation_messages, session_key_from_path, summarize_session_content,
    token_prefix, validate_attachments, workspace_sessions_dirs, ClientMessage,
    ConversationMessage, ConversationSummary, CreateConversationRequest,
    CreateConversationResponse, ServerMessage,
};
use clawhive_schema::{BusMessage, InboundMessage};
use serde::Serialize;
use tokio::sync::{mpsc, RwLock};
use uuid::Uuid;

use crate::state::AppState;
use crate::{extract_session_token, is_valid_session};

#[derive(Debug, Clone, Serialize)]
pub struct ChatAgentInfo {
    pub agent_id: String,
    pub name: Option<String>,
    pub model: Option<String>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/ws", get(ws_handler))
        .route(
            "/conversations",
            get(list_conversations).post(create_conversation),
        )
        .route(
            "/conversations/{id}",
            axum::routing::delete(delete_conversation),
        )
        .route(
            "/conversations/{id}/messages",
            get(get_conversation_messages),
        )
        .route("/agents", get(list_chat_agents))
}

async fn ws_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let Some(token) = extract_session_token(&headers) else {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "Authentication required" })),
        )
            .into_response();
    };

    if !is_valid_session(&state, &token) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "Authentication required" })),
        )
            .into_response();
    }

    if state.gateway.is_none() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "Gateway unavailable" })),
        )
            .into_response();
    }

    ws.on_upgrade(move |socket| handle_ws_connection(socket, state, token))
        .into_response()
}

async fn handle_ws_connection(socket: WebSocket, state: AppState, token: String) {
    let Some(gateway) = state.gateway.clone() else {
        return;
    };

    let mut socket = socket;
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<ServerMessage>();
    let active_trace_ids = Arc::new(RwLock::new(HashSet::<Uuid>::new()));

    let relay_handle = {
        let out_tx = out_tx.clone();
        let active_trace_ids = Arc::clone(&active_trace_ids);
        let bus = Arc::clone(&state.bus);
        tokio::spawn(async move {
            relay_bus_events(bus, active_trace_ids, out_tx).await;
        })
    };

    let mut last_active_trace_id: Option<Uuid> = None;
    let mut heartbeat = tokio::time::interval(Duration::from_secs(5 * 60));

    loop {
        tokio::select! {
            _ = heartbeat.tick() => {
                if !is_valid_session(&state, &token) {
                    let _ = out_tx.send(ServerMessage::Error {
                        trace_id: None,
                        message: "Session expired".to_string(),
                    });
                    break;
                }
            }
            outbound = out_rx.recv() => {
                let Some(server_msg) = outbound else {
                    break;
                };
                tracing::debug!(msg_type = ?std::mem::discriminant(&server_msg), "chat ws: sending server message to client");
                let Ok(payload) = serde_json::to_string(&server_msg) else {
                    continue;
                };
                if socket.send(Message::Text(payload.into())).await.is_err() {
                    tracing::warn!("chat ws: failed to send message via websocket");
                    break;
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ClientMessage>(&text) {
                            Ok(ClientMessage::SendMessage {
                                text,
                                agent_id,
                                conversation_id,
                                attachments,
                            }) => {
                                if text.chars().count() > 10_000 {
                                    let _ = out_tx.send(ServerMessage::Error {
                                        trace_id: None,
                                        message: "Message exceeds 10,000 characters".to_string(),
                                    });
                                    continue;
                                }

                                if let Err(error) = validate_attachments(&attachments) {
                                    tracing::warn!(
                                        attachment_count = attachments.len(),
                                        error = %error,
                                        "chat ws: attachment validation failed"
                                    );
                                    let _ = out_tx.send(ServerMessage::Error {
                                        trace_id: None,
                                        message: error,
                                    });
                                    continue;
                                }

                                let conversation_id = conversation_id.unwrap_or_else(|| Uuid::new_v4().to_string());
                                let trace_id = Uuid::new_v4();
                                let user_scope = format!("user:web_{}", token_prefix(&token));
                                let inbound = InboundMessage {
                                    trace_id,
                                    channel_type: "web_console".to_string(),
                                    connector_id: "web_main".to_string(),
                                    conversation_scope: format!("chat:{conversation_id}"),
                                    user_scope,
                                    text,
                                    at: Utc::now(),
                                    thread_id: None,
                                    is_mention: false,
                                    mention_target: None,
                                    message_id: None,
                                    attachments: attachments.iter().map(map_attachment).collect(),
                                    group_context: None,
                                    message_source: None,
                                };

                                {
                                    let mut guard = active_trace_ids.write().await;
                                    guard.insert(trace_id);
                                }
                                last_active_trace_id = Some(trace_id);
                                tracing::debug!(trace_id = %trace_id, agent_id = %agent_id, "chat ws: dispatching inbound to gateway");

                                let gateway = Arc::clone(&gateway);
                                let bus = Arc::clone(&state.bus);
                                let out_tx2 = out_tx.clone();
                                let active_ids = Arc::clone(&active_trace_ids);
                                tokio::spawn(async move {
                                    match gateway.handle_inbound(inbound).await {
                                        Ok(outbound) => {
                                            // For slash commands and other early returns,
                                            // the orchestrator may not publish ReplyReady.
                                            // Publish it here as a fallback. If already
                                            // published, the relay will drop the duplicate
                                            // (trace_id already removed from active set).
                                            let _ = bus.publish(BusMessage::ReplyReady {
                                                outbound: outbound.clone(),
                                            }).await;
                                            // Also send directly to ensure delivery
                                            let trace_id = outbound.trace_id;
                                            if is_active_trace_id(&active_ids, trace_id).await {
                                                remove_active_trace_id(&active_ids, trace_id).await;
                                                let _ = out_tx2.send(ServerMessage::ReplyReady {
                                                    trace_id: trace_id.to_string(),
                                                    text: outbound.text,
                                                });
                                            }
                                        }
                                        Err(error) => {
                                            tracing::warn!(
                                                trace_id = %trace_id,
                                                agent_id = %agent_id,
                                                error = %error,
                                                "failed to handle inbound web chat message"
                                            );
                                        }
                                    }
                                });
                            }
                            Ok(ClientMessage::Cancel) => {
                                let Some(trace_id) = last_active_trace_id else {
                                    let _ = out_tx.send(ServerMessage::Error {
                                        trace_id: None,
                                        message: "No active request to cancel".to_string(),
                                    });
                                    continue;
                                };

                                if let Err(error) = state
                                    .bus
                                    .publisher()
                                    .publish(BusMessage::CancelTask { trace_id })
                                    .await
                                {
                                    let _ = out_tx.send(ServerMessage::Error {
                                        trace_id: Some(trace_id.to_string()),
                                        message: format!("Failed to cancel request: {error}"),
                                    });
                                }
                            }
                            Ok(ClientMessage::Ping) => {
                                let _ = out_tx.send(ServerMessage::Pong);
                            }
                            Err(error) => {
                                let _ = out_tx.send(ServerMessage::Error {
                                    trace_id: None,
                                    message: format!("Invalid client message: {error}"),
                                });
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        break;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        tracing::debug!(error = %error, "websocket receive error");
                        break;
                    }
                }
            }
        }
    }

    relay_handle.abort();
    drop(out_tx);
}

async fn relay_bus_events(
    bus: Arc<clawhive_bus::EventBus>,
    active_trace_ids: Arc<RwLock<HashSet<Uuid>>>,
    out_tx: mpsc::UnboundedSender<ServerMessage>,
) {
    tracing::debug!("chat relay: starting bus event subscriptions");
    let mut rx_stream = bus.subscribe(Topic::StreamDelta).await;
    let mut rx_tool_start = bus.subscribe(Topic::ToolCallStarted).await;
    let mut rx_tool_done = bus.subscribe(Topic::ToolCallCompleted).await;
    let mut rx_reply = bus.subscribe(Topic::ReplyReady).await;
    let mut rx_failed = bus.subscribe(Topic::TaskFailed).await;
    tracing::debug!("chat relay: subscribed to all topics, starting poll loop");

    let mut interval = tokio::time::interval(Duration::from_millis(100));

    loop {
        interval.tick().await;

        while let Ok(msg) = rx_stream.try_recv() {
            if let BusMessage::StreamDelta {
                trace_id,
                delta,
                is_final,
            } = msg
            {
                if !is_active_trace_id(&active_trace_ids, trace_id).await {
                    tracing::trace!(trace_id = %trace_id, "dropping stream delta for inactive trace_id");
                    continue;
                }
                if out_tx
                    .send(ServerMessage::StreamDelta {
                        trace_id: trace_id.to_string(),
                        delta,
                        is_final,
                    })
                    .is_err()
                {
                    return;
                }
            }
        }

        while let Ok(msg) = rx_tool_start.try_recv() {
            if let BusMessage::ToolCallStarted {
                trace_id,
                tool_name,
                arguments,
            } = msg
            {
                if !is_active_trace_id(&active_trace_ids, trace_id).await {
                    tracing::trace!(trace_id = %trace_id, "dropping tool_call_started for inactive trace_id");
                    continue;
                }
                if out_tx
                    .send(ServerMessage::ToolCallStarted {
                        trace_id: trace_id.to_string(),
                        tool_name,
                        arguments,
                    })
                    .is_err()
                {
                    return;
                }
            }
        }

        while let Ok(msg) = rx_tool_done.try_recv() {
            if let BusMessage::ToolCallCompleted {
                trace_id,
                tool_name,
                output,
                duration_ms,
            } = msg
            {
                if !is_active_trace_id(&active_trace_ids, trace_id).await {
                    tracing::trace!(trace_id = %trace_id, "dropping tool_call_completed for inactive trace_id");
                    continue;
                }
                if out_tx
                    .send(ServerMessage::ToolCallCompleted {
                        trace_id: trace_id.to_string(),
                        tool_name,
                        output,
                        duration_ms,
                    })
                    .is_err()
                {
                    return;
                }
            }
        }

        while let Ok(msg) = rx_reply.try_recv() {
            if let BusMessage::ReplyReady { outbound } = msg {
                let trace_id = outbound.trace_id;
                let is_active = is_active_trace_id(&active_trace_ids, trace_id).await;
                tracing::debug!(
                    trace_id = %trace_id,
                    is_active = is_active,
                    reply_len = outbound.text.len(),
                    "chat relay: received ReplyReady from bus"
                );
                if !is_active {
                    continue;
                }
                remove_active_trace_id(&active_trace_ids, trace_id).await;
                let send_result = out_tx.send(ServerMessage::ReplyReady {
                    trace_id: trace_id.to_string(),
                    text: outbound.text,
                });
                tracing::debug!(
                    trace_id = %trace_id,
                    send_ok = send_result.is_ok(),
                    "chat relay: forwarded ReplyReady to WebSocket channel"
                );
                if send_result.is_err() {
                    return;
                }
            }
        }

        while let Ok(msg) = rx_failed.try_recv() {
            if let BusMessage::TaskFailed { trace_id, error } = msg {
                if !is_active_trace_id(&active_trace_ids, trace_id).await {
                    tracing::trace!(trace_id = %trace_id, "dropping task_failed for inactive trace_id");
                    continue;
                }
                remove_active_trace_id(&active_trace_ids, trace_id).await;
                if out_tx
                    .send(ServerMessage::Error {
                        trace_id: Some(trace_id.to_string()),
                        message: error,
                    })
                    .is_err()
                {
                    return;
                }
            }
        }
    }
}

async fn is_active_trace_id(active_trace_ids: &RwLock<HashSet<Uuid>>, trace_id: Uuid) -> bool {
    active_trace_ids.read().await.contains(&trace_id)
}

async fn remove_active_trace_id(active_trace_ids: &RwLock<HashSet<Uuid>>, trace_id: Uuid) {
    active_trace_ids.write().await.remove(&trace_id);
}

async fn list_conversations(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<ConversationSummary>>, StatusCode> {
    let token = require_valid_session_token(&state, &headers)?;
    let tp = token_prefix(&token);
    let mut items: Vec<(SystemTime, ConversationSummary)> = Vec::new();

    for (agent_id, sessions_dir) in workspace_sessions_dirs(&state.root) {
        let Ok(entries) = std::fs::read_dir(&sessions_dir) else {
            continue;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
                continue;
            }

            let Some(session_key) = session_key_from_path(&path) else {
                continue;
            };

            if !is_web_console_user_session(&session_key, &tp) {
                continue;
            }

            let Some(conversation_id) = conversation_id_from_session_key(&session_key) else {
                continue;
            };

            let content = std::fs::read_to_string(&path).unwrap_or_default();
            let (message_count, last_message_at, preview) = summarize_session_content(&content);

            let modified = entry
                .metadata()
                .ok()
                .and_then(|meta| meta.modified().ok())
                .unwrap_or(SystemTime::UNIX_EPOCH);

            items.push((
                modified,
                ConversationSummary {
                    conversation_id,
                    agent_id: agent_id.clone(),
                    last_message_at,
                    message_count,
                    preview,
                },
            ));
        }
    }

    items.sort_by(|a, b| b.0.cmp(&a.0));
    Ok(Json(items.into_iter().map(|(_, item)| item).collect()))
}

async fn create_conversation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateConversationRequest>,
) -> Result<Json<CreateConversationResponse>, StatusCode> {
    let _token = require_valid_session_token(&state, &headers)?;
    if body.agent_id.trim().is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    Ok(Json(CreateConversationResponse {
        conversation_id: Uuid::new_v4().to_string(),
        agent_id: body.agent_id,
    }))
}

async fn delete_conversation(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<StatusCode, StatusCode> {
    let token = require_valid_session_token(&state, &headers)?;
    let tp = token_prefix(&token);

    let Some(path) = find_conversation_session_file(&state.root, &id, &tp) else {
        return Err(StatusCode::NOT_FOUND);
    };

    std::fs::remove_file(path).map_err(|_| StatusCode::NOT_FOUND)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_conversation_messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> Result<Json<Vec<ConversationMessage>>, StatusCode> {
    let token = require_valid_session_token(&state, &headers)?;
    let tp = token_prefix(&token);

    let Some(path) = find_conversation_session_file(&state.root, &id, &tp) else {
        return Err(StatusCode::NOT_FOUND);
    };

    let content = std::fs::read_to_string(path).map_err(|_| StatusCode::NOT_FOUND)?;
    Ok(Json(parse_conversation_messages(&content)))
}

async fn list_chat_agents(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Vec<ChatAgentInfo>>, StatusCode> {
    let _token = require_valid_session_token(&state, &headers)?;
    let agents_dir = state.root.join("config/agents.d");
    let mut agents = Vec::new();

    let Ok(entries) = std::fs::read_dir(agents_dir) else {
        return Ok(Json(agents));
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("yaml") {
            continue;
        }

        let content = std::fs::read_to_string(&path).unwrap_or_default();
        let Ok(doc) = serde_yaml::from_str::<serde_yaml::Value>(&content) else {
            continue;
        };

        if !doc["enabled"].as_bool().unwrap_or(false) {
            continue;
        }

        let agent_id = doc["agent_id"]
            .as_str()
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| {
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .to_string()
            });
        let name = doc["identity"]["name"].as_str().map(ToOwned::to_owned);
        let model = doc["model_policy"]["primary"]
            .as_str()
            .map(ToOwned::to_owned);

        agents.push(ChatAgentInfo {
            agent_id,
            name,
            model,
        });
    }

    agents.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
    Ok(Json(agents))
}

fn require_valid_session_token(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<String, StatusCode> {
    let token = extract_session_token(headers).ok_or(StatusCode::UNAUTHORIZED)?;
    if !is_valid_session(state, &token) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(token)
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
    use clawhive_bus::EventBus;
    use tower::ServiceExt;

    use crate::state::AppState;
    use crate::{create_router, SESSION_TTL};

    fn setup_state() -> (AppState, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("config")).unwrap();
        std::fs::write(root.join("config/main.yaml"), "app_name: test\n").unwrap();

        let state = AppState {
            root: root.to_path_buf(),
            bus: Arc::new(EventBus::new(16)),
            gateway: None,
            web_password_hash: Arc::new(RwLock::new(None)),
            session_store: Arc::new(RwLock::new(HashMap::new())),
            pending_openai_oauth: Arc::new(RwLock::new(HashMap::new())),
            openai_oauth_config: crate::state::default_openai_oauth_config(),
            enable_openai_oauth_callback_listener: true,
            daemon_mode: false,
            port: 3000,
            webhook_config: Arc::new(RwLock::new(None)),
            routing_config: Arc::new(RwLock::new(None)),
        };

        (state, tmp)
    }

    fn insert_session(state: &AppState, token: &str) {
        state
            .session_store
            .write()
            .unwrap()
            .insert(token.to_string(), Instant::now() + SESSION_TTL);
    }

    fn authed_get(uri: &str, token: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .header("cookie", format!("clawhive_session={token}"))
            .body(Body::empty())
            .unwrap()
    }

    fn authed_post(uri: &str, token: &str, body: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(uri)
            .header("cookie", format!("clawhive_session={token}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn authed_delete(uri: &str, token: &str) -> Request<Body> {
        Request::builder()
            .method("DELETE")
            .uri(uri)
            .header("cookie", format!("clawhive_session={token}"))
            .body(Body::empty())
            .unwrap()
    }

    /// Create a fake session JSONL file in the workspace directory.
    /// Session key format: `web_console:web_main:chat:{conv_id}:user:web_{token_prefix}`
    fn create_session_file(
        root: &std::path::Path,
        agent_id: &str,
        conv_id: &str,
        token_prefix: &str,
        content: &str,
    ) {
        let sessions_dir = root.join(format!("workspaces/{agent_id}/sessions"));
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let key = format!("web_console:web_main:chat:{conv_id}:user:web_{token_prefix}");
        std::fs::write(sessions_dir.join(format!("{key}.jsonl")), content).unwrap();
    }

    // ── Auth tests (via REST endpoints — same auth functions as WebSocket handler) ──

    #[tokio::test]
    async fn chat_requires_auth_no_cookie() {
        let (state, _tmp) = setup_state();
        let app = create_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/chat/conversations")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn chat_requires_auth_invalid_session() {
        let (state, _tmp) = setup_state();
        let app = create_router(state);

        let response = app
            .oneshot(authed_get("/api/chat/conversations", "nonexistent_token"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn chat_agents_requires_auth() {
        let (state, _tmp) = setup_state();
        let app = create_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/chat/agents")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    // ── Conversation CRUD tests ──

    #[tokio::test]
    async fn conversation_crud_lifecycle() {
        let (state, _tmp) = setup_state();
        let token = "lifecycle_test_token";
        insert_session(&state, token);
        let root = state.root.clone();

        let app = create_router(state);

        // Create a conversation
        let response = app
            .clone()
            .oneshot(authed_post(
                "/api/chat/conversations",
                token,
                r#"{"agent_id":"test-agent"}"#,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let created: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(created["agent_id"], "test-agent");
        let conv_id = created["conversation_id"].as_str().unwrap();

        // Create a matching session file so list/delete can find it
        let token_pfx: String = token.chars().take(8).collect();
        let session_content = r#"{"type":"message","timestamp":"2024-01-01T00:00:00Z","message":{"role":"user","content":"hello"}}"#;
        create_session_file(&root, "test-agent", conv_id, &token_pfx, session_content);

        // List conversations — should find one
        let response = app
            .clone()
            .oneshot(authed_get("/api/chat/conversations", token))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let convs: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0]["conversation_id"], conv_id);
        assert_eq!(convs[0]["agent_id"], "test-agent");
        assert_eq!(convs[0]["message_count"], 1);

        // Delete the conversation
        let response = app
            .clone()
            .oneshot(authed_delete(
                &format!("/api/chat/conversations/{conv_id}"),
                token,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        // List again — should be empty
        let response = app
            .oneshot(authed_get("/api/chat/conversations", token))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let convs: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert!(convs.is_empty());
    }

    #[tokio::test]
    async fn conversation_isolation_by_user() {
        let (state, _tmp) = setup_state();
        let token_a = "user_aaaa_token";
        let token_b = "user_bbbb_token";
        insert_session(&state, token_a);
        insert_session(&state, token_b);
        let root = state.root.clone();

        let pfx_a: String = token_a.chars().take(8).collect();
        let pfx_b: String = token_b.chars().take(8).collect();

        // User A has one conversation
        create_session_file(
            &root,
            "agent-1",
            "conv-aaa",
            &pfx_a,
            r#"{"type":"message","timestamp":"2024-01-01T00:00:00Z","message":{"role":"user","content":"from A"}}"#,
        );

        // User B has a different conversation
        create_session_file(
            &root,
            "agent-1",
            "conv-bbb",
            &pfx_b,
            r#"{"type":"message","timestamp":"2024-01-01T00:00:00Z","message":{"role":"user","content":"from B"}}"#,
        );

        let app = create_router(state);

        // User A sees only their conversation
        let response = app
            .clone()
            .oneshot(authed_get("/api/chat/conversations", token_a))
            .await
            .unwrap();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let convs: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0]["conversation_id"], "conv-aaa");

        // User B sees only their conversation
        let response = app
            .oneshot(authed_get("/api/chat/conversations", token_b))
            .await
            .unwrap();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let convs: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0]["conversation_id"], "conv-bbb");
    }

    // ── Agent listing tests ──

    #[tokio::test]
    async fn agents_list_returns_enabled_only() {
        let (state, _tmp) = setup_state();
        insert_session(&state, "agent_list_token");

        let agents_dir = state.root.join("config/agents.d");
        std::fs::create_dir_all(&agents_dir).unwrap();

        std::fs::write(
            agents_dir.join("enabled-agent.yaml"),
            "agent_id: enabled-agent\nenabled: true\nidentity:\n  name: Enabled Bot\nmodel_policy:\n  primary: gpt-4\n",
        )
        .unwrap();

        std::fs::write(
            agents_dir.join("disabled-agent.yaml"),
            "agent_id: disabled-agent\nenabled: false\nidentity:\n  name: Disabled Bot\nmodel_policy:\n  primary: gpt-3.5\n",
        )
        .unwrap();

        let app = create_router(state);

        let response = app
            .oneshot(authed_get("/api/chat/agents", "agent_list_token"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let agents: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();

        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0]["agent_id"], "enabled-agent");
        assert_eq!(agents[0]["name"], "Enabled Bot");
        assert_eq!(agents[0]["model"], "gpt-4");
    }
}
