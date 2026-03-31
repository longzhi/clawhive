use axum::http::StatusCode;
use axum::{
    extract::{Path, State},
    routing::{delete, get, post},
    Json, Router,
};
use serde::Deserialize;
use serde::Serialize;

use crate::state::AppState;
#[cfg(feature = "whatsapp")]
use crate::state::WhatsAppPairSession;
use crate::webhook_auth::{generate_api_key, hash_api_key};
#[cfg(feature = "whatsapp")]
use std::time::Instant;

pub fn router() -> Router<AppState> {
    let router = Router::new()
        .route("/", get(get_channels).put(update_channels))
        .route("/status", get(get_channels_status))
        .route(
            "/webhook/connectors",
            post(create_webhook_source).get(list_webhook_sources),
        )
        .route(
            "/webhook/connectors/{source_id}",
            delete(delete_webhook_source),
        )
        .route(
            "/webhook/connectors/{source_id}/rotate-key",
            post(rotate_webhook_source_key),
        )
        .route("/{kind}/connectors", post(add_connector))
        .route("/{kind}/connectors/{id}", delete(remove_connector))
        .route("/weixin/qr-login", post(weixin_qr_login))
        .route("/weixin/qr-status", get(weixin_qr_status));

    #[cfg(feature = "whatsapp")]
    let router = router
        .route("/whatsapp/qr-pair", post(whatsapp_qr_pair))
        .route("/whatsapp/qr-status", get(whatsapp_qr_status));

    router
}

#[derive(Serialize)]
struct ConnectorStatus {
    kind: String,
    connector_id: String,
    status: String,
}

#[derive(Deserialize)]
struct AddConnectorRequest {
    connector_id: String,
    /// Token for Telegram/Discord connectors.
    #[serde(default)]
    token: Option<String>,
    /// Feishu app_id
    #[serde(default)]
    app_id: Option<String>,
    /// Feishu app_secret
    #[serde(default)]
    app_secret: Option<String>,
    /// DingTalk client_id
    #[serde(default)]
    client_id: Option<String>,
    /// DingTalk client_secret
    #[serde(default)]
    client_secret: Option<String>,
    /// WeCom bot_id
    #[serde(default)]
    bot_id: Option<String>,
    /// WeCom secret
    #[serde(default)]
    secret: Option<String>,
    #[serde(default)]
    groups: Option<Vec<String>>,
    #[serde(default)]
    require_mention: Option<bool>,
    #[serde(default)]
    dm_policy: Option<String>,
    #[serde(default)]
    allow_from: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct CreateWebhookSourceRequest {
    source_id: String,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

async fn get_channels(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, axum::http::StatusCode> {
    let path = state.root.join("config/main.yaml");
    let content = std::fs::read_to_string(&path).map_err(|_| axum::http::StatusCode::NOT_FOUND)?;
    let val: serde_yaml::Value = serde_yaml::from_str(&content)
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;

    let channels = &val["channels"];
    let json = serde_json::to_value(channels)
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(json))
}

async fn update_channels(
    State(state): State<AppState>,
    Json(channels): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, axum::http::StatusCode> {
    let mut val = load_main_config(&state)?;

    let channels_yaml: serde_yaml::Value = serde_json::from_value(channels.clone())
        .map_err(|_| axum::http::StatusCode::BAD_REQUEST)?;
    val["channels"] = channels_yaml;

    write_main_config(&state, &val)?;

    Ok(Json(channels))
}

async fn get_channels_status(
    State(state): State<AppState>,
) -> Result<Json<Vec<ConnectorStatus>>, axum::http::StatusCode> {
    let path = state.root.join("config/main.yaml");
    let content = std::fs::read_to_string(&path).map_err(|_| axum::http::StatusCode::NOT_FOUND)?;
    let val: serde_yaml::Value = serde_yaml::from_str(&content)
        .map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;

    let mut statuses = Vec::new();
    let channels = val["channels"]
        .as_mapping()
        .ok_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;

    for (kind, channel) in channels {
        let Some(kind_str) = kind.as_str() else {
            continue;
        };
        let Some(channel_map) = channel.as_mapping() else {
            continue;
        };

        let enabled = channel_map
            .get(serde_yaml::Value::String("enabled".to_string()))
            .and_then(serde_yaml::Value::as_bool)
            .unwrap_or(false);

        let Some(connectors) = channel_map
            .get(serde_yaml::Value::String("connectors".to_string()))
            .and_then(serde_yaml::Value::as_sequence)
        else {
            continue;
        };

        for connector in connectors {
            let Some(connector_map) = connector.as_mapping() else {
                continue;
            };
            let connector_id = connector_map
                .get(serde_yaml::Value::String("connector_id".to_string()))
                .and_then(serde_yaml::Value::as_str)
                .unwrap_or_default()
                .to_string();
            if connector_id.is_empty() {
                continue;
            }

            let has_credentials = match kind_str {
                "feishu" => {
                    let app_id = connector_map
                        .get(serde_yaml::Value::String("app_id".to_string()))
                        .and_then(serde_yaml::Value::as_str)
                        .unwrap_or_default();
                    let app_secret = connector_map
                        .get(serde_yaml::Value::String("app_secret".to_string()))
                        .and_then(serde_yaml::Value::as_str)
                        .unwrap_or_default();
                    !app_id.is_empty()
                        && !app_secret.is_empty()
                        && !app_id.starts_with("${")
                        && !app_secret.starts_with("${")
                }
                "dingtalk" => {
                    let client_id = connector_map
                        .get(serde_yaml::Value::String("client_id".to_string()))
                        .and_then(serde_yaml::Value::as_str)
                        .unwrap_or_default();
                    let client_secret = connector_map
                        .get(serde_yaml::Value::String("client_secret".to_string()))
                        .and_then(serde_yaml::Value::as_str)
                        .unwrap_or_default();
                    !client_id.is_empty()
                        && !client_secret.is_empty()
                        && !client_id.starts_with("${")
                        && !client_secret.starts_with("${")
                }
                "wecom" => {
                    let bot_id = connector_map
                        .get(serde_yaml::Value::String("bot_id".to_string()))
                        .and_then(serde_yaml::Value::as_str)
                        .unwrap_or_default();
                    let secret = connector_map
                        .get(serde_yaml::Value::String("secret".to_string()))
                        .and_then(serde_yaml::Value::as_str)
                        .unwrap_or_default();
                    !bot_id.is_empty()
                        && !secret.is_empty()
                        && !bot_id.starts_with("${")
                        && !secret.starts_with("${")
                }
                "whatsapp" => {
                    // Check if session DB exists (paired)
                    let db_path = state
                        .root
                        .join("data")
                        .join(format!("whatsapp-{connector_id}.db"));
                    db_path.exists()
                }
                "imessage" => true,
                "weixin" => {
                    // Check if session file exists (logged in)
                    let session_path = state
                        .root
                        .join("data")
                        .join(format!("weixin-{connector_id}"))
                        .join("session.json");
                    session_path.exists()
                }
                "slack" => {
                    let bot_token = connector_map
                        .get(serde_yaml::Value::String("bot_token".to_string()))
                        .and_then(serde_yaml::Value::as_str)
                        .unwrap_or_default();
                    !bot_token.is_empty() && !bot_token.starts_with("${")
                }
                _ => {
                    let token = connector_map
                        .get(serde_yaml::Value::String("token".to_string()))
                        .and_then(serde_yaml::Value::as_str)
                        .unwrap_or_default();
                    !token.is_empty() && !token.starts_with("${")
                }
            };

            let status = if !enabled {
                "inactive"
            } else if !has_credentials {
                "error"
            } else {
                "connected"
            };

            statuses.push(ConnectorStatus {
                kind: kind_str.to_string(),
                connector_id,
                status: status.to_string(),
            });
        }
    }

    Ok(Json(statuses))
}

fn write_main_config(
    state: &AppState,
    val: &serde_yaml::Value,
) -> Result<(), axum::http::StatusCode> {
    let path = state.root.join("config/main.yaml");
    let yaml =
        serde_yaml::to_string(val).map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    std::fs::write(&path, yaml).map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(())
}

fn load_main_config(state: &AppState) -> Result<serde_yaml::Value, axum::http::StatusCode> {
    let path = state.root.join("config/main.yaml");
    let content = std::fs::read_to_string(&path).map_err(|_| axum::http::StatusCode::NOT_FOUND)?;
    serde_yaml::from_str(&content).map_err(|_| axum::http::StatusCode::INTERNAL_SERVER_ERROR)
}

fn connectors_mut<'a>(
    root: &'a mut serde_yaml::Value,
    kind: &str,
) -> Result<&'a mut Vec<serde_yaml::Value>, axum::http::StatusCode> {
    let channels = root["channels"]
        .as_mapping_mut()
        .ok_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    let kind_key = serde_yaml::Value::String(kind.to_string());
    // Auto-create channel kind with enabled: true if missing
    if !channels.contains_key(&kind_key) {
        let mut new_channel = serde_yaml::Mapping::new();
        new_channel.insert(
            serde_yaml::Value::String("enabled".to_string()),
            serde_yaml::Value::Bool(true),
        );
        new_channel.insert(
            serde_yaml::Value::String("connectors".to_string()),
            serde_yaml::Value::Sequence(Vec::new()),
        );
        channels.insert(kind_key.clone(), serde_yaml::Value::Mapping(new_channel));
    }
    let channel_map = channels
        .get_mut(&kind_key)
        .and_then(|v| v.as_mapping_mut())
        .ok_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    // Ensure enabled is true when adding a connector
    channel_map.insert(
        serde_yaml::Value::String("enabled".to_string()),
        serde_yaml::Value::Bool(true),
    );
    let connectors = channel_map
        .entry(serde_yaml::Value::String("connectors".to_string()))
        .or_insert_with(|| serde_yaml::Value::Sequence(Vec::new()));
    connectors
        .as_sequence_mut()
        .ok_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR)
}

fn webhook_sources_mut(
    root: &mut serde_yaml::Value,
) -> Result<&mut Vec<serde_yaml::Value>, axum::http::StatusCode> {
    let channels = root["channels"]
        .as_mapping_mut()
        .ok_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;

    let webhook_key = serde_yaml::Value::String("webhook".to_string());
    if !channels.contains_key(&webhook_key) {
        let mut webhook = serde_yaml::Mapping::new();
        webhook.insert(
            serde_yaml::Value::String("enabled".to_string()),
            serde_yaml::Value::Bool(true),
        );
        webhook.insert(
            serde_yaml::Value::String("sources".to_string()),
            serde_yaml::Value::Sequence(Vec::new()),
        );
        channels.insert(webhook_key.clone(), serde_yaml::Value::Mapping(webhook));
    }

    let webhook_map = channels
        .get_mut(&webhook_key)
        .and_then(serde_yaml::Value::as_mapping_mut)
        .ok_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR)?;
    webhook_map.insert(
        serde_yaml::Value::String("enabled".to_string()),
        serde_yaml::Value::Bool(true),
    );

    let sources = webhook_map
        .entry(serde_yaml::Value::String("sources".to_string()))
        .or_insert_with(|| serde_yaml::Value::Sequence(Vec::new()));

    sources
        .as_sequence_mut()
        .ok_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR)
}

fn masked_key_from_auth(auth: &serde_yaml::Mapping) -> Option<String> {
    let key_prefix = auth
        .get(serde_yaml::Value::String("key_prefix".to_string()))
        .and_then(serde_yaml::Value::as_str)
        .map(ToString::to_string)
        .or_else(|| {
            auth.get(serde_yaml::Value::String("key".to_string()))
                .and_then(serde_yaml::Value::as_str)
                .map(|full| full.chars().take(8).collect::<String>())
        });

    key_prefix.map(|prefix| format!("{prefix}..."))
}

async fn create_webhook_source(
    State(state): State<AppState>,
    Json(body): Json<CreateWebhookSourceRequest>,
) -> Result<(axum::http::StatusCode, Json<serde_json::Value>), axum::http::StatusCode> {
    if body.source_id.trim().is_empty() {
        return Err(axum::http::StatusCode::BAD_REQUEST);
    }

    let api_key = generate_api_key();
    let key_hash = hash_api_key(&api_key);
    let key_prefix: String = api_key.chars().take(8).collect();

    let mut main = load_main_config(&state)?;
    let sources = webhook_sources_mut(&mut main)?;
    sources.retain(|item| {
        item["source_id"]
            .as_str()
            .map(|source_id| source_id != body.source_id)
            .unwrap_or(true)
    });

    let mut auth = serde_yaml::Mapping::new();
    auth.insert(
        serde_yaml::Value::String("method".to_string()),
        serde_yaml::Value::String("api_key".to_string()),
    );
    auth.insert(
        serde_yaml::Value::String("key_hash".to_string()),
        serde_yaml::Value::String(key_hash),
    );
    auth.insert(
        serde_yaml::Value::String("key_prefix".to_string()),
        serde_yaml::Value::String(key_prefix),
    );

    let mut source = serde_yaml::Mapping::new();
    source.insert(
        serde_yaml::Value::String("source_id".to_string()),
        serde_yaml::Value::String(body.source_id.clone()),
    );
    let format = match body.format.as_deref() {
        Some("raw") | Some("generic") | Some("alertmanager") | Some("github") | None => {
            body.format.unwrap_or_else(|| "raw".to_string())
        }
        Some(unknown) => {
            tracing::warn!(format = %unknown, "unsupported webhook format");
            return Err(StatusCode::BAD_REQUEST);
        }
    };
    source.insert(
        serde_yaml::Value::String("format".to_string()),
        serde_yaml::Value::String(format),
    );
    if let Some(description) = body.description {
        source.insert(
            serde_yaml::Value::String("description".to_string()),
            serde_yaml::Value::String(description),
        );
    }
    source.insert(
        serde_yaml::Value::String("auth".to_string()),
        serde_yaml::Value::Mapping(auth),
    );

    sources.push(serde_yaml::Value::Mapping(source));
    write_main_config(&state, &main)?;

    Ok((
        axum::http::StatusCode::CREATED,
        Json(serde_json::json!({
            "source_id": body.source_id,
            "api_key": api_key,
        })),
    ))
}

async fn list_webhook_sources(
    State(state): State<AppState>,
) -> Result<Json<Vec<serde_json::Value>>, axum::http::StatusCode> {
    let main = load_main_config(&state)?;

    let sources = main["channels"]["webhook"]["sources"]
        .as_sequence()
        .cloned()
        .unwrap_or_default();

    let items = sources
        .into_iter()
        .filter_map(|source| {
            let map = source.as_mapping()?;
            let source_id = map
                .get(serde_yaml::Value::String("source_id".to_string()))
                .and_then(serde_yaml::Value::as_str)?;

            let format = map
                .get(serde_yaml::Value::String("format".to_string()))
                .and_then(serde_yaml::Value::as_str)
                .unwrap_or("raw")
                .to_string();

            let description = map
                .get(serde_yaml::Value::String("description".to_string()))
                .and_then(serde_yaml::Value::as_str)
                .map(ToString::to_string);

            let api_key_masked = map
                .get(serde_yaml::Value::String("auth".to_string()))
                .and_then(serde_yaml::Value::as_mapping)
                .and_then(masked_key_from_auth)
                .unwrap_or_else(|| "".to_string());

            Some(serde_json::json!({
                "source_id": source_id,
                "format": format,
                "description": description,
                "api_key_masked": api_key_masked,
            }))
        })
        .collect();

    Ok(Json(items))
}

async fn delete_webhook_source(
    State(state): State<AppState>,
    Path(source_id): Path<String>,
) -> Result<Json<serde_json::Value>, axum::http::StatusCode> {
    let mut main = load_main_config(&state)?;
    let sources = webhook_sources_mut(&mut main)?;
    let before = sources.len();

    sources.retain(|item| {
        item["source_id"]
            .as_str()
            .map(|existing| existing != source_id)
            .unwrap_or(true)
    });

    if sources.len() == before {
        return Err(axum::http::StatusCode::NOT_FOUND);
    }

    write_main_config(&state, &main)?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "source_id": source_id,
    })))
}

async fn rotate_webhook_source_key(
    State(state): State<AppState>,
    Path(source_id): Path<String>,
) -> Result<Json<serde_json::Value>, axum::http::StatusCode> {
    let mut main = load_main_config(&state)?;
    let sources = webhook_sources_mut(&mut main)?;

    let Some(source) = sources.iter_mut().find(|source| {
        source["source_id"]
            .as_str()
            .map(|existing| existing == source_id)
            .unwrap_or(false)
    }) else {
        return Err(axum::http::StatusCode::NOT_FOUND);
    };

    let Some(source_map) = source.as_mapping_mut() else {
        return Err(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    };

    let api_key = generate_api_key();
    let key_hash = hash_api_key(&api_key);
    let key_prefix: String = api_key.chars().take(8).collect();

    let auth = source_map
        .entry(serde_yaml::Value::String("auth".to_string()))
        .or_insert_with(|| serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
    let Some(auth_map) = auth.as_mapping_mut() else {
        return Err(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    };

    auth_map.insert(
        serde_yaml::Value::String("method".to_string()),
        serde_yaml::Value::String("api_key".to_string()),
    );
    auth_map.insert(
        serde_yaml::Value::String("key_hash".to_string()),
        serde_yaml::Value::String(key_hash),
    );
    auth_map.remove(serde_yaml::Value::String("key".to_string()));
    auth_map.insert(
        serde_yaml::Value::String("key_prefix".to_string()),
        serde_yaml::Value::String(key_prefix),
    );

    write_main_config(&state, &main)?;
    Ok(Json(serde_json::json!({
        "source_id": source_id,
        "api_key": api_key,
    })))
}

async fn add_connector(
    State(state): State<AppState>,
    Path(kind): Path<String>,
    Json(body): Json<AddConnectorRequest>,
) -> Result<Json<serde_json::Value>, axum::http::StatusCode> {
    if body.connector_id.trim().is_empty() {
        return Err(axum::http::StatusCode::BAD_REQUEST);
    }

    let mut main = load_main_config(&state)?;
    let connectors = connectors_mut(&mut main, &kind)?;

    // Remove existing connector with same ID (upsert behavior)
    connectors.retain(|item| {
        item["connector_id"]
            .as_str()
            .map(|id| id != body.connector_id)
            .unwrap_or(true)
    });

    let mut connector = serde_yaml::Mapping::new();
    connector.insert(
        serde_yaml::Value::String("connector_id".to_string()),
        serde_yaml::Value::String(body.connector_id.clone()),
    );

    // Write credential fields based on channel kind
    match kind.as_str() {
        "feishu" => {
            let app_id = body.app_id.as_deref().unwrap_or_default();
            let app_secret = body.app_secret.as_deref().unwrap_or_default();
            if app_id.is_empty() || app_secret.is_empty() {
                return Err(axum::http::StatusCode::BAD_REQUEST);
            }
            connector.insert(
                serde_yaml::Value::String("app_id".to_string()),
                serde_yaml::Value::String(app_id.to_string()),
            );
            connector.insert(
                serde_yaml::Value::String("app_secret".to_string()),
                serde_yaml::Value::String(app_secret.to_string()),
            );
        }
        "dingtalk" => {
            let client_id = body.client_id.as_deref().unwrap_or_default();
            let client_secret = body.client_secret.as_deref().unwrap_or_default();
            if client_id.is_empty() || client_secret.is_empty() {
                return Err(axum::http::StatusCode::BAD_REQUEST);
            }
            connector.insert(
                serde_yaml::Value::String("client_id".to_string()),
                serde_yaml::Value::String(client_id.to_string()),
            );
            connector.insert(
                serde_yaml::Value::String("client_secret".to_string()),
                serde_yaml::Value::String(client_secret.to_string()),
            );
        }
        "wecom" => {
            let bot_id = body.bot_id.as_deref().unwrap_or_default();
            let secret = body.secret.as_deref().unwrap_or_default();
            if bot_id.is_empty() || secret.is_empty() {
                return Err(axum::http::StatusCode::BAD_REQUEST);
            }
            connector.insert(
                serde_yaml::Value::String("bot_id".to_string()),
                serde_yaml::Value::String(bot_id.to_string()),
            );
            connector.insert(
                serde_yaml::Value::String("secret".to_string()),
                serde_yaml::Value::String(secret.to_string()),
            );
        }
        "whatsapp" => {
            let db_path = format!("~/.clawhive/data/whatsapp-{}.db", body.connector_id);
            connector.insert(
                serde_yaml::Value::String("db_path".to_string()),
                serde_yaml::Value::String(db_path),
            );
        }
        "imessage" | "weixin" => {}
        _ => {
            // Telegram, Discord, Slack, and other token-based channels
            let token = body.token.as_deref().unwrap_or_default();
            if token.is_empty() {
                return Err(axum::http::StatusCode::BAD_REQUEST);
            }
            connector.insert(
                serde_yaml::Value::String("token".to_string()),
                serde_yaml::Value::String(token.to_string()),
            );
        }
    }
    if let Some(groups) = &body.groups {
        if !groups.is_empty() {
            let groups_seq: Vec<serde_yaml::Value> = groups
                .iter()
                .map(|g| serde_yaml::Value::String(g.clone()))
                .collect();
            connector.insert(
                serde_yaml::Value::String("groups".to_string()),
                serde_yaml::Value::Sequence(groups_seq),
            );
        }
    }
    if let Some(require_mention) = body.require_mention {
        if !require_mention {
            connector.insert(
                serde_yaml::Value::String("require_mention".to_string()),
                serde_yaml::Value::Bool(false),
            );
        }
    }
    if let Some(dm_policy) = &body.dm_policy {
        connector.insert(
            serde_yaml::Value::String("dm_policy".to_string()),
            serde_yaml::Value::String(dm_policy.clone()),
        );
    }
    if let Some(allow_from) = &body.allow_from {
        if !allow_from.is_empty() {
            let allow_from_seq: Vec<serde_yaml::Value> = allow_from
                .iter()
                .map(|id| serde_yaml::Value::String(id.clone()))
                .collect();
            connector.insert(
                serde_yaml::Value::String("allow_from".to_string()),
                serde_yaml::Value::Sequence(allow_from_seq),
            );
        }
    }
    connectors.push(serde_yaml::Value::Mapping(connector));

    write_main_config(&state, &main)?;

    // Auto-create routing binding with default agent
    let routing_path = state.root.join("config/routing.yaml");
    if let Ok(content) = std::fs::read_to_string(&routing_path) {
        if let Ok(mut doc) = serde_yaml::from_str::<serde_yaml::Value>(&content) {
            let default_agent = doc
                .get("default_agent_id")
                .and_then(|v| v.as_str())
                .unwrap_or("clawhive-main")
                .to_string();

            let kinds = ["dm"];
            let new_bindings: Vec<serde_yaml::Value> = kinds
                .iter()
                .map(|k| {
                    let mut match_map = serde_yaml::Mapping::new();
                    match_map.insert("kind".into(), serde_yaml::Value::String((*k).to_string()));
                    let mut binding = serde_yaml::Mapping::new();
                    binding.insert(
                        "channel_type".into(),
                        serde_yaml::Value::String(kind.clone()),
                    );
                    binding.insert(
                        "connector_id".into(),
                        serde_yaml::Value::String(body.connector_id.clone()),
                    );
                    binding.insert("match".into(), serde_yaml::Value::Mapping(match_map));
                    binding.insert(
                        "agent_id".into(),
                        serde_yaml::Value::String(default_agent.clone()),
                    );
                    serde_yaml::Value::Mapping(binding)
                })
                .collect();

            if let Some(seq) = doc
                .get_mut("bindings")
                .and_then(|bindings| bindings.as_sequence_mut())
            {
                seq.retain(|binding| {
                    binding.get("connector_id").and_then(|v| v.as_str()) != Some(&body.connector_id)
                });
                seq.extend(new_bindings);
            } else {
                doc["bindings"] = serde_yaml::Value::Sequence(new_bindings);
            }

            if let Ok(yaml) = serde_yaml::to_string(&doc) {
                let _ = std::fs::write(&routing_path, yaml);
            }
        }
    }

    Ok(Json(serde_json::json!({
        "kind": kind,
        "connector_id": body.connector_id,
    })))
}

async fn remove_connector(
    State(state): State<AppState>,
    Path((kind, id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, axum::http::StatusCode> {
    let mut main = load_main_config(&state)?;
    let connectors = connectors_mut(&mut main, &kind)?;

    let before = connectors.len();
    connectors.retain(|item| {
        item["connector_id"]
            .as_str()
            .map(|connector_id| connector_id != id)
            .unwrap_or(true)
    });

    if connectors.len() == before {
        return Err(axum::http::StatusCode::NOT_FOUND);
    }

    write_main_config(&state, &main)?;
    Ok(Json(serde_json::json!({
        "ok": true,
        "kind": kind,
        "connector_id": id,
    })))
}

#[cfg(feature = "whatsapp")]
#[derive(Deserialize)]
struct WhatsAppPairQuery {
    connector_id: String,
}

#[cfg(feature = "whatsapp")]
#[derive(Serialize)]
struct WhatsAppPairResponse {
    ok: bool,
    status: String,
}

#[cfg(feature = "whatsapp")]
#[derive(Serialize)]
struct WhatsAppQrStatusResponse {
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    qr_data: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    qr_timeout: Option<u64>,
}

#[cfg(feature = "whatsapp")]
async fn whatsapp_qr_pair(
    State(state): State<AppState>,
    axum::extract::Query(query): axum::extract::Query<WhatsAppPairQuery>,
) -> Result<Json<WhatsAppPairResponse>, StatusCode> {
    let connector_id = query.connector_id;
    let db_path = state
        .root
        .join("data")
        .join(format!("whatsapp-{connector_id}.db"));

    {
        let mut sessions = state
            .whatsapp_pairing
            .write()
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        sessions.insert(
            connector_id.clone(),
            WhatsAppPairSession {
                status: "waiting_qr".to_string(),
                qr_data: None,
                qr_timeout: None,
                started_at: Instant::now(),
            },
        );
    }

    let sessions = state.whatsapp_pairing.clone();
    tokio::spawn(async move {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let pairing_task =
            tokio::spawn(
                async move { clawhive_channels::whatsapp::run_pairing(db_path, tx).await },
            );

        while let Some(event) = rx.recv().await {
            let mut guard = match sessions.write() {
                Ok(guard) => guard,
                Err(error) => {
                    tracing::error!(error = %error, "failed to lock whatsapp_pairing state");
                    return;
                }
            };

            if let Some(session) = guard.get_mut(&connector_id) {
                match event {
                    clawhive_channels::whatsapp::PairStatus::QrCode(data, timeout) => {
                        session.status = "qr_ready".to_string();
                        session.qr_data = Some(data);
                        session.qr_timeout = Some(timeout.as_secs());
                    }
                    clawhive_channels::whatsapp::PairStatus::Paired => {
                        session.status = "paired".to_string();
                    }
                    clawhive_channels::whatsapp::PairStatus::AlreadyPaired => {
                        session.status = "already_paired".to_string();
                    }
                    clawhive_channels::whatsapp::PairStatus::Failed(error) => {
                        tracing::error!(
                            connector_id = %connector_id,
                            error = %error,
                            "whatsapp pairing failed"
                        );
                        session.status = "failed".to_string();
                    }
                }
            }
        }

        match pairing_task.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::error!(
                    connector_id = %connector_id,
                    error = %error,
                    "whatsapp pairing task returned error"
                );
                if let Ok(mut guard) = sessions.write() {
                    if let Some(session) = guard.get_mut(&connector_id) {
                        session.status = "failed".to_string();
                    }
                }
            }
            Err(error) => {
                tracing::error!(
                    connector_id = %connector_id,
                    error = %error,
                    "whatsapp pairing task join error"
                );
                if let Ok(mut guard) = sessions.write() {
                    if let Some(session) = guard.get_mut(&connector_id) {
                        session.status = "failed".to_string();
                    }
                }
            }
        }
    });

    Ok(Json(WhatsAppPairResponse {
        ok: true,
        status: "waiting_qr".to_string(),
    }))
}

#[cfg(feature = "whatsapp")]
async fn whatsapp_qr_status(
    State(state): State<AppState>,
    axum::extract::Query(query): axum::extract::Query<WhatsAppPairQuery>,
) -> Result<Json<WhatsAppQrStatusResponse>, StatusCode> {
    let sessions = state
        .whatsapp_pairing
        .read()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let session = sessions
        .get(&query.connector_id)
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(WhatsAppQrStatusResponse {
        status: session.status.clone(),
        qr_data: session.qr_data.clone(),
        qr_timeout: session.qr_timeout,
    }))
}

// ---------------------------------------------------------------------------
// WeChat iLink QR Login
// ---------------------------------------------------------------------------

const ILINK_BASE_URL: &str = "https://ilinkai.weixin.qq.com";

#[derive(Serialize)]
struct QrLoginResponse {
    qrcode_token: String,
    qrcode_url: String,
}

async fn weixin_qr_login() -> Result<Json<QrLoginResponse>, StatusCode> {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let resp = http
        .get(format!(
            "{ILINK_BASE_URL}/ilink/bot/get_bot_qrcode?bot_type=3"
        ))
        .send()
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "failed to get weixin QR code");
            StatusCode::BAD_GATEWAY
        })?;

    #[derive(Deserialize)]
    struct ILinkQr {
        qrcode: String,
        qrcode_img_content: String,
    }

    let qr: ILinkQr = resp.json().await.map_err(|e| {
        tracing::error!(error = %e, "failed to parse weixin QR response");
        StatusCode::BAD_GATEWAY
    })?;

    Ok(Json(QrLoginResponse {
        qrcode_token: qr.qrcode,
        qrcode_url: qr.qrcode_img_content,
    }))
}

#[derive(Deserialize)]
struct QrStatusQuery {
    token: String,
    connector_id: String,
}

#[derive(Serialize)]
struct QrStatusResponse {
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    bot_id: Option<String>,
}

async fn weixin_qr_status(
    State(state): State<AppState>,
    axum::extract::Query(query): axum::extract::Query<QrStatusQuery>,
) -> Result<Json<QrStatusResponse>, StatusCode> {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(40))
        .build()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let url = format!(
        "{ILINK_BASE_URL}/ilink/bot/get_qrcode_status?qrcode={}",
        query.token
    );
    let resp = http
        .get(&url)
        .header("iLink-App-ClientVersion", "1")
        .send()
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "failed to poll weixin QR status");
            StatusCode::BAD_GATEWAY
        })?;

    #[derive(Deserialize)]
    struct ILinkStatus {
        status: String,
        #[serde(default)]
        bot_token: Option<String>,
        #[serde(default)]
        ilink_bot_id: Option<String>,
        #[serde(default)]
        baseurl: Option<String>,
        #[serde(default)]
        ilink_user_id: Option<String>,
    }

    let status: ILinkStatus = resp.json().await.map_err(|e| {
        tracing::error!(error = %e, "failed to parse weixin QR status");
        StatusCode::BAD_GATEWAY
    })?;

    if status.status == "confirmed" {
        // Save session to data dir
        let data_dir = state
            .root
            .join("data")
            .join(format!("weixin-{}", query.connector_id));
        let _ = std::fs::create_dir_all(&data_dir);
        let session_path = data_dir.join("session.json");

        let session = serde_json::json!({
            "bot_token": status.bot_token.unwrap_or_default(),
            "base_url": status.baseurl.unwrap_or_else(|| ILINK_BASE_URL.to_string()),
            "bot_id": status.ilink_bot_id.clone().unwrap_or_default(),
            "user_id": status.ilink_user_id.unwrap_or_default(),
            "saved_at": chrono::Utc::now().to_rfc3339(),
        });
        let _ = std::fs::write(
            &session_path,
            serde_json::to_string_pretty(&session).unwrap_or_default(),
        );

        tracing::info!(
            connector_id = %query.connector_id,
            bot_id = %status.ilink_bot_id.as_deref().unwrap_or(""),
            "weixin QR login confirmed via web console"
        );

        return Ok(Json(QrStatusResponse {
            status: "confirmed".to_string(),
            bot_id: status.ilink_bot_id,
        }));
    }

    Ok(Json(QrStatusResponse {
        status: status.status,
        bot_id: None,
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
        Router,
    };
    use tower::util::ServiceExt;

    use crate::state::AppState;

    fn setup_test_root() -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "clawhive-server-channels-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(root.join("config")).expect("create config dir");
        std::fs::write(
            root.join("config/main.yaml"),
            r#"channels:
  telegram:
    enabled: true
    connectors:
      - connector_id: tg_main
        token: ${TELEGRAM_BOT_TOKEN}
  discord:
    enabled: false
    connectors: []
"#,
        )
        .expect("write main.yaml");
        root
    }

    fn setup_test_app() -> (Router, std::path::PathBuf) {
        let root = setup_test_root();
        let state = AppState {
            root: root.clone(),
            bus: Arc::new(clawhive_bus::EventBus::new(16)),
            gateway: None,
            web_password_hash: Arc::new(std::sync::RwLock::new(None)),
            session_store: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
            whatsapp_pairing: Arc::new(std::sync::RwLock::new(std::collections::HashMap::new())),
            pending_openai_oauth: Arc::new(
                std::sync::RwLock::new(std::collections::HashMap::new()),
            ),
            openai_oauth_config: crate::state::default_openai_oauth_config(),
            enable_openai_oauth_callback_listener: true,
            daemon_mode: false,
            port: 3000,
            schedule_manager: None,
            reload_coordinator: None,
        };
        (
            Router::new()
                .nest("/api/channels", super::router())
                .with_state(state),
            root,
        )
    }

    fn read_connectors_len(root: &std::path::Path, kind: &str) -> usize {
        let content = std::fs::read_to_string(root.join("config/main.yaml")).expect("read yaml");
        let val: serde_yaml::Value = serde_yaml::from_str(&content).expect("parse yaml");
        val["channels"][kind]["connectors"]
            .as_sequence()
            .map(std::vec::Vec::len)
            .unwrap_or(0)
    }

    fn read_main_yaml(root: &std::path::Path) -> serde_yaml::Value {
        let content = std::fs::read_to_string(root.join("config/main.yaml")).expect("read yaml");
        serde_yaml::from_str(&content).expect("parse yaml")
    }

    #[tokio::test]
    async fn test_get_channels_status() {
        let (app, _) = setup_test_app();
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/channels/status")
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("send request");

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_add_connector() {
        let (app, root) = setup_test_app();

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/channels/telegram/connectors")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"connector_id":"tg_extra","token":"123:abc"}"#,
                    ))
                    .expect("build request"),
            )
            .await
            .expect("send request");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(read_connectors_len(&root, "telegram"), 2);
    }

    #[tokio::test]
    async fn test_delete_connector() {
        let (app, root) = setup_test_app();

        let response = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/channels/telegram/connectors/tg_main")
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("send request");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(read_connectors_len(&root, "telegram"), 0);
    }

    #[tokio::test]
    async fn test_create_webhook_source_returns_plaintext_key_once() {
        let (app, root) = setup_test_app();

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/channels/webhook/connectors")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        r#"{"source_id":"alerts","format":"raw","description":"Alert source"}"#,
                    ))
                    .expect("build request"),
            )
            .await
            .expect("send request");

        assert_eq!(response.status(), StatusCode::CREATED);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body bytes");
        let json: serde_json::Value = serde_json::from_slice(&body).expect("parse body json");
        let api_key = json["api_key"].as_str().expect("api_key string");
        assert!(api_key.starts_with("whk_"));

        let main = read_main_yaml(&root);
        let source = &main["channels"]["webhook"]["sources"][0];
        assert_eq!(source["source_id"], "alerts");
        assert_eq!(source["format"], "raw");
        assert_eq!(source["description"], "Alert source");
        assert_eq!(source["auth"]["method"], "api_key");
        assert!(source["auth"]["key_hash"]
            .as_str()
            .expect("key_hash present")
            .starts_with("sha256:"));
        assert!(source["auth"]["key"].is_null());
    }

    #[tokio::test]
    async fn test_list_webhook_sources_masks_key_prefix() {
        let (app, root) = setup_test_app();

        let create_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/channels/webhook/connectors")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"source_id":"alerts"}"#))
                    .expect("build request"),
            )
            .await
            .expect("send request");
        assert_eq!(create_response.status(), StatusCode::CREATED);
        let create_body = to_bytes(create_response.into_body(), usize::MAX)
            .await
            .expect("read create body bytes");
        let create_json: serde_json::Value =
            serde_json::from_slice(&create_body).expect("parse create body");
        let api_key = create_json["api_key"].as_str().expect("api_key string");
        let expected_prefix: String = api_key.chars().take(8).collect();

        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/api/channels/webhook/connectors")
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("send request");

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body bytes");
        let sources: Vec<serde_json::Value> =
            serde_json::from_slice(&body).expect("parse body json");
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0]["source_id"], "alerts");
        assert_eq!(
            sources[0]["api_key_masked"],
            format!("{expected_prefix}...")
        );

        let main = read_main_yaml(&root);
        assert!(
            main["channels"]["webhook"]["sources"][0]["auth"]["key_hash"]
                .as_str()
                .expect("key_hash")
                .starts_with("sha256:")
        );
    }

    #[tokio::test]
    async fn test_delete_webhook_source_removes_from_sources() {
        let (app, root) = setup_test_app();

        let create_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/channels/webhook/connectors")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"source_id":"alerts"}"#))
                    .expect("build request"),
            )
            .await
            .expect("send request");
        assert_eq!(create_response.status(), StatusCode::CREATED);

        let response = app
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/channels/webhook/connectors/alerts")
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("send request");

        assert_eq!(response.status(), StatusCode::OK);
        let main = read_main_yaml(&root);
        let sources = main["channels"]["webhook"]["sources"]
            .as_sequence()
            .expect("sources sequence");
        assert!(sources.is_empty());
    }

    #[tokio::test]
    async fn test_rotate_webhook_source_key_updates_hash_and_returns_new_key() {
        let (app, root) = setup_test_app();

        let create_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/channels/webhook/connectors")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"source_id":"alerts"}"#))
                    .expect("build request"),
            )
            .await
            .expect("send request");
        assert_eq!(create_response.status(), StatusCode::CREATED);
        let create_body = to_bytes(create_response.into_body(), usize::MAX)
            .await
            .expect("read create body bytes");
        let create_json: serde_json::Value =
            serde_json::from_slice(&create_body).expect("parse create body");
        let first_key = create_json["api_key"]
            .as_str()
            .expect("first key")
            .to_string();

        let before = read_main_yaml(&root);
        let first_hash = before["channels"]["webhook"]["sources"][0]["auth"]["key_hash"]
            .as_str()
            .expect("first hash")
            .to_string();

        let rotate_response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/channels/webhook/connectors/alerts/rotate-key")
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("send request");

        assert_eq!(rotate_response.status(), StatusCode::OK);
        let rotate_body = to_bytes(rotate_response.into_body(), usize::MAX)
            .await
            .expect("read rotate body bytes");
        let rotate_json: serde_json::Value =
            serde_json::from_slice(&rotate_body).expect("parse rotate body");
        let second_key = rotate_json["api_key"]
            .as_str()
            .expect("second key")
            .to_string();
        assert_ne!(first_key, second_key);

        let after = read_main_yaml(&root);
        let second_hash = after["channels"]["webhook"]["sources"][0]["auth"]["key_hash"]
            .as_str()
            .expect("second hash")
            .to_string();
        assert_ne!(first_hash, second_hash);
        assert!(second_hash.starts_with("sha256:"));
    }
}
