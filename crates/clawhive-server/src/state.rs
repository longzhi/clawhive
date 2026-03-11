use clawhive_auth::oauth::{OpenAiOAuthConfig, OPENAI_OAUTH_CLIENT_ID};
use clawhive_gateway::Gateway;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Instant;

#[derive(Debug, Clone)]
pub struct PendingOpenAiOAuth {
    pub expected_state: String,
    pub code_verifier: String,
    pub created_at: Instant,
    pub callback_code: Option<String>,
    pub callback_listener_active: bool,
    pub callback_listener_message: Option<String>,
    pub callback_listener_shutdown: Option<tokio::sync::broadcast::Sender<()>>,
}

pub fn default_openai_oauth_config() -> OpenAiOAuthConfig {
    OpenAiOAuthConfig::default_with_client(OPENAI_OAUTH_CLIENT_ID)
}

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
    pub pending_openai_oauth: Arc<RwLock<HashMap<String, PendingOpenAiOAuth>>>,
    pub openai_oauth_config: OpenAiOAuthConfig,
    pub enable_openai_oauth_callback_listener: bool,
    /// Whether the server was started in daemon mode (for restart).
    pub daemon_mode: bool,
    /// HTTP port the server is listening on (for restart).
    pub port: u16,
}
