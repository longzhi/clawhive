pub mod access_gate;
pub mod approval;
pub mod audit;
pub mod config;
pub mod consolidation;
pub mod context;
pub mod file_tools;
pub mod heartbeat;
pub mod hooks;
pub mod image_tool;
pub mod memory_tools;
pub mod message_tool;
pub mod orchestrator;
pub mod peer_registry;
pub mod persona;
pub mod policy;
pub mod router;
pub mod schedule_tool;
pub mod session;
pub mod session_lock;
pub mod shell_tool;
pub mod skill;
pub mod skill_install;
pub mod skill_install_state;
pub mod skill_tool;
pub mod slash_commands;
pub mod streaming;
pub mod subagent;
pub mod subagent_tool;
pub mod templates;
pub mod tool;
pub mod wait_tool;
pub mod web_fetch_tool;
pub mod web_search_tool;
pub mod workspace;

pub use access_gate::*;
pub use approval::*;
pub use audit::*;
pub use config::*;
pub use consolidation::*;
pub use context::*;
pub use file_tools::*;
pub use heartbeat::*;
pub use hooks::*;
pub use memory_tools::*;
pub use message_tool::*;
pub use orchestrator::*;
pub use peer_registry::*;
pub use persona::*;
pub use policy::*;
pub use router::*;
pub use schedule_tool::*;
pub use session::*;
pub use session_lock::*;
pub use shell_tool::*;
pub use skill::*;
pub use skill_install::*;
pub use skill_install_state::*;
pub use skill_tool::*;
pub use slash_commands::*;
pub use streaming::*;
pub use subagent::*;
pub use subagent_tool::*;
pub use templates::*;
pub use tool::*;
pub use web_fetch_tool::*;
pub use web_search_tool::*;
pub use workspace::*;

use clawhive_provider::ThinkingLevel;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPolicy {
    pub primary: String,
    #[serde(default)]
    pub fallbacks: Vec<String>,
    /// Thinking / reasoning effort level. None = no extended thinking (default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<ThinkingLevel>,
    /// Override context window (auto-resolved from model presets if not set).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub agent_id: String,
    pub enabled: bool,
    pub model_policy: ModelPolicy,
}

#[cfg(test)]
mod tests {

    #[test]
    fn context_config_resolves_from_model_preset() {
        let info = clawhive_schema::provider_presets::model_info("moonshot", "moonshot-v1-8k");
        assert!(info.is_some());
        let info = info.unwrap();
        assert_eq!(info.context_window, 8192);
    }
}
