use std::convert::Infallible;
use std::time::Duration;

use axum::{
    extract::State,
    response::sse::{Event, KeepAlive, Sse},
    routing::get,
    Json, Router,
};
use clawhive_bus::Topic;
use futures_core::Stream;
use serde::Serialize;

use crate::state::AppState;

#[derive(Serialize)]
pub struct Metrics {
    pub agents_active: usize,
    pub agents_total: usize,
    pub sessions_total: usize,
    pub providers_total: usize,
    pub channels_total: usize,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/stream", get(event_stream))
        .route("/metrics", get(get_metrics))
}

async fn event_stream(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut rx = state.bus.subscribe(Topic::HandleIncomingMessage).await;
    let mut rx_reply = state.bus.subscribe(Topic::ReplyReady).await;
    let mut rx_failed = state.bus.subscribe(Topic::TaskFailed).await;
    let mut rx_accepted = state.bus.subscribe(Topic::MessageAccepted).await;
    let mut rx_stream = state.bus.subscribe(Topic::StreamDelta).await;
    let mut rx_mem_write = state.bus.subscribe(Topic::MemoryWriteRequested).await;
    let mut rx_mem_read = state.bus.subscribe(Topic::MemoryReadRequested).await;
    let mut rx_consolidation = state.bus.subscribe(Topic::ConsolidationCompleted).await;

    let stream = async_stream::stream! {
        let mut interval = tokio::time::interval(Duration::from_millis(100));
        loop {
            interval.tick().await;

            while let Ok(msg) = rx.try_recv() {
                if let Ok(json) = serde_json::to_string(&msg) {
                    yield Ok(Event::default().data(json));
                }
            }
            while let Ok(msg) = rx_reply.try_recv() {
                if let Ok(json) = serde_json::to_string(&msg) {
                    yield Ok(Event::default().data(json));
                }
            }
            while let Ok(msg) = rx_failed.try_recv() {
                if let Ok(json) = serde_json::to_string(&msg) {
                    yield Ok(Event::default().data(json));
                }
            }
            while let Ok(msg) = rx_accepted.try_recv() {
                if let Ok(json) = serde_json::to_string(&msg) {
                    yield Ok(Event::default().data(json));
                }
            }
            while let Ok(msg) = rx_stream.try_recv() {
                if let Ok(json) = serde_json::to_string(&msg) {
                    yield Ok(Event::default().data(json));
                }
            }
            while let Ok(msg) = rx_mem_write.try_recv() {
                if let Ok(json) = serde_json::to_string(&msg) {
                    yield Ok(Event::default().data(json));
                }
            }
            while let Ok(msg) = rx_mem_read.try_recv() {
                if let Ok(json) = serde_json::to_string(&msg) {
                    yield Ok(Event::default().data(json));
                }
            }
            while let Ok(msg) = rx_consolidation.try_recv() {
                if let Ok(json) = serde_json::to_string(&msg) {
                    yield Ok(Event::default().data(json));
                }
            }
        }
    };

    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn get_metrics(State(state): State<AppState>) -> Json<Metrics> {
    let agents_dir = state.root.join("config/agents.d");
    let mut total = 0;
    let mut active = 0;

    if let Ok(entries) = std::fs::read_dir(&agents_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
                continue;
            }
            total += 1;
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(val) = serde_yaml::from_str::<serde_yaml::Value>(&content) {
                    if val["enabled"].as_bool().unwrap_or(false) {
                        active += 1;
                    }
                }
            }
        }
    }

    let sessions_dir = state.root.join("data/sessions");
    let sessions_total = std::fs::read_dir(&sessions_dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().extension().and_then(|ext| ext.to_str()) == Some("jsonl"))
                .count()
        })
        .unwrap_or(0);

    let providers_dir = state.root.join("config/providers.d");
    let providers_total = std::fs::read_dir(&providers_dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.path().extension().and_then(|ext| ext.to_str()) == Some("yaml"))
                .count()
        })
        .unwrap_or(0);

    let main_yaml = state.root.join("config/main.yaml");
    let channels_total = std::fs::read_to_string(&main_yaml)
        .ok()
        .and_then(|content| serde_yaml::from_str::<serde_yaml::Value>(&content).ok())
        .map(|val| {
            val["channels"]
                .as_mapping()
                .map(|m| {
                    m.values()
                        .filter(|ch| {
                            ch["connectors"]
                                .as_sequence()
                                .map(|s| !s.is_empty())
                                .unwrap_or(false)
                        })
                        .count()
                })
                .unwrap_or(0)
        })
        .unwrap_or(0);

    Json(Metrics {
        agents_active: active,
        agents_total: total,
        sessions_total,
        providers_total,
        channels_total,
    })
}
