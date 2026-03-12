use clawhive_auth::oauth::{OpenAiOAuthConfig, OPENAI_OAUTH_CLIENT_ID};
use clawhive_core::config::{RoutingConfig, WebhookChannelConfig};
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
    /// Cached webhook channel config (loaded at startup, refreshed on CRUD writes).
    pub webhook_config: Arc<RwLock<Option<WebhookChannelConfig>>>,
    /// Cached routing config (loaded at startup).
    pub routing_config: Arc<RwLock<Option<RoutingConfig>>>,
}

impl AppState {
    /// Refresh the webhook config cache from a parsed main.yaml value.
    pub fn refresh_webhook_cache(&self, main_yaml: &serde_yaml::Value) {
        let cfg = main_yaml
            .get("channels")
            .and_then(|c| c.get("webhook").cloned())
            .and_then(|w| serde_yaml::from_value::<WebhookChannelConfig>(w).ok())
            .filter(|c| c.enabled);
        *self.webhook_config.write().unwrap() = cfg;
    }

    /// Load webhook config from disk into the cache.
    pub fn load_webhook_config_from_disk(&self) {
        let path = self.root.join("config/main.yaml");
        let cfg = std::fs::read_to_string(&path)
            .ok()
            .and_then(|content| serde_yaml::from_str::<serde_yaml::Value>(&content).ok())
            .and_then(|val| val.get("channels")?.get("webhook").cloned())
            .and_then(|w| serde_yaml::from_value::<WebhookChannelConfig>(w).ok())
            .filter(|c| c.enabled);
        *self.webhook_config.write().unwrap() = cfg;
    }

    /// Load routing config from disk into the cache.
    pub fn load_routing_config_from_disk(&self) {
        let path = self.root.join("config/routing.yaml");
        let cfg = std::fs::read_to_string(&path)
            .ok()
            .and_then(|content| serde_yaml::from_str::<RoutingConfig>(&content).ok());
        *self.routing_config.write().unwrap() = cfg;
    }
}
