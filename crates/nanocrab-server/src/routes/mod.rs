pub mod agents;
pub mod channels;
pub mod events;
pub mod providers;
pub mod routing;
pub mod sessions;

use axum::Router;

use crate::state::AppState;

pub fn api_router() -> Router<AppState> {
    Router::new()
        .nest("/agents", agents::router())
        .nest("/channels", channels::router())
        .nest("/providers", providers::router())
        .nest("/routing", routing::router())
        .nest("/sessions", sessions::router())
        .nest("/events", events::router())
}
