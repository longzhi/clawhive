use clawhive_gateway::Gateway;
use std::path::PathBuf;
use std::sync::Arc;

/// Shared application state accessible from all route handlers.
#[derive(Clone)]
pub struct AppState {
    /// Root directory of the clawhive project (contains config/, memory/, sessions/)
    pub root: PathBuf,
    /// Reference to the event bus for SSE streaming
    pub bus: Arc<clawhive_bus::EventBus>,
    /// Optional gateway handle for routes that need to inject inbound messages.
    pub gateway: Option<Arc<Gateway>>,
    /// Whether the server was started in daemon mode (for restart).
    pub daemon_mode: bool,
    /// HTTP port the server is listening on (for restart).
    pub port: u16,
}
