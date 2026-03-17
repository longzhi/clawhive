pub mod admin;
pub mod agents;
pub mod attachments;
pub mod auth;
pub mod channels;
pub mod chat;
pub mod events;
pub mod providers;
pub mod routing;
pub mod schedules;
pub mod sessions;
pub mod setup;
pub mod skills;
pub mod webhook;

use axum::Router;

use crate::state::AppState;

pub fn api_router() -> Router<AppState> {
    Router::new()
        .nest("/admin", admin::router())
        .nest("/agents", agents::router())
        .nest("/auth", auth::router())
        .nest("/chat", chat::router())
        .nest("/chat/attachments", attachments::router())
        .nest("/channels", channels::router())
        .nest("/providers", providers::router())
        .nest("/routing", routing::router())
        .nest("/schedules", schedules::router())
        .nest("/sessions", sessions::router())
        .nest("/events", events::router())
        .nest("/setup", setup::router())
        .nest("/skills", skills::router())
}
