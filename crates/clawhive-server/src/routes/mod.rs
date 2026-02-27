pub mod agents;
pub mod auth;
pub mod channels;
pub mod events;
pub mod providers;
pub mod routing;
pub mod schedules;
pub mod sessions;
pub mod setup;

use axum::Router;

use crate::state::AppState;

pub fn api_router() -> Router<AppState> {
    Router::new()
        .nest("/agents", agents::router())
        .nest("/auth", auth::router())
        .nest("/channels", channels::router())
        .nest("/providers", providers::router())
        .nest("/routing", routing::router())
        .nest("/schedules", schedules::router())
        .nest("/sessions", sessions::router())
        .nest("/events", events::router())
        .nest("/setup", setup::router())
}
