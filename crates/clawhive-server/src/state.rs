use std::path::PathBuf;
use std::sync::Arc;
use clawhive_gateway::Gateway;

/// Shared application state accessible from all route handlers.
#[derive(Clone)]
pub struct AppState {
    /// Root directory of the clawhive project (contains config/, memory/, sessions/)
    pub root: PathBuf,
    /// Reference to the event bus for SSE streaming
    pub bus: Arc<clawhive_bus::EventBus>,
    /// Optional gateway handle for routes that need to inject inbound messages.
    pub gateway: Option<Arc<Gateway>>,
}
