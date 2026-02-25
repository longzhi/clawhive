pub mod audit;
pub mod config;
pub mod consolidation;
pub mod context;
pub mod file_tools;
pub mod heartbeat;
pub mod memory_tools;
pub mod orchestrator;
pub mod persona;
pub mod policy;
pub mod router;
pub mod schedule_tool;
pub mod session;
pub mod session_lock;
pub mod shell_tool;
pub mod skill;
pub mod slash_commands;
pub mod web_search_tool;
pub mod web_fetch_tool;
pub mod subagent;
pub mod subagent_tool;
pub mod tool;
pub mod templates;
pub mod workspace;

pub use audit::*;
pub use config::*;
pub use consolidation::*;
pub use context::*;
pub use file_tools::*;
pub use heartbeat::*;
pub use memory_tools::*;
pub use orchestrator::*;
pub use persona::*;
pub use policy::*;
pub use router::*;
pub use schedule_tool::*;
pub use session::*;
pub use session_lock::*;
pub use shell_tool::*;
pub use skill::*;
pub use slash_commands::*;
pub use web_search_tool::*;
pub use web_fetch_tool::*;
pub use subagent::*;
pub use subagent_tool::*;
pub use tool::*;
pub use templates::*;
pub use workspace::*;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPolicy {
    pub primary: String,
    #[serde(default)]
    pub fallbacks: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub agent_id: String,
    pub enabled: bool,
    pub model_policy: ModelPolicy,
}

#[derive(Debug, Clone)]
pub struct WeakReActConfig {
    pub max_steps: usize,
    pub repeat_guard: usize,
}

impl Default for WeakReActConfig {
    fn default() -> Self {
        Self {
            max_steps: 4,
            repeat_guard: 2,
        }
    }
}
