use clawhive_gateway::Gateway;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Instant;

/// Shared application state accessible from all route handlers.
#[derive(Clone)]
pub struct AppState {
    /// Root directory of the clawhive project (contains config/, memory/, sessions/)
    pub root: PathBuf,
    /// Reference to the event bus for SSE streaming
    pub bus: Arc<clawhive_bus::EventBus>,
    /// Optional gateway handle for routes that need to inject inbound messages.
    pub gateway: Option<Arc<Gateway>>,
    pub web_password_hash: Arc<RwLock<Option<String>>>,
    pub session_store: Arc<RwLock<HashMap<String, Instant>>>,
    /// Whether the server was started in daemon mode (for restart).
    pub daemon_mode: bool,
    /// HTTP port the server is listening on (for restart).
    pub port: u16,
}
