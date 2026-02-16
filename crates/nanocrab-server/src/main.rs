use std::sync::Arc;

use anyhow::Result;
use nanocrab_bus::EventBus;
use nanocrab_server::state::AppState;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    let root = std::env::current_dir()?;

    let log_dir = root.join("logs");
    std::fs::create_dir_all(&log_dir)?;
    let file_appender = tracing_appender::rolling::daily(&log_dir, "nanocrab-server.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("nanocrab_server=info,tower_http=debug"));

    tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .with(tracing_subscriber::fmt::layer().with_ansi(false).with_writer(non_blocking))
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
