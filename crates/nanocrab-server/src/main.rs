use std::sync::Arc;

use anyhow::Result;
use nanocrab_bus::EventBus;
use nanocrab_server::state::AppState;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("nanocrab_server=info,tower_http=debug")),
        )
        .init();

    let root = std::env::current_dir()?;
    let bus = Arc::new(EventBus::new(128));

    let state = AppState {
        root: root.clone(),
        bus,
    };

    let addr = std::env::var("NANOCRAB_BIND").unwrap_or_else(|_| "0.0.0.0:3001".to_string());
    nanocrab_server::serve(state, &addr).await
}
