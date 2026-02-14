use std::path::PathBuf;
use std::sync::Arc;

/// Shared application state accessible from all route handlers.
#[derive(Clone)]
pub struct AppState {
    /// Root directory of the nanocrab project (contains config/, memory/, sessions/)
    pub root: PathBuf,
    /// Reference to the event bus for SSE streaming
    pub bus: Arc<nanocrab_bus::EventBus>,
}
