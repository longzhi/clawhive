pub mod config;
pub mod consolidation;
pub mod file_tools;
pub mod memory_tools;
pub mod orchestrator;
pub mod persona;
pub mod router;
pub mod session;
pub mod shell_tool;
pub mod skill;
pub mod web_search_tool;
pub mod web_fetch_tool;
pub mod subagent;
pub mod subagent_tool;
pub mod tool;
pub mod workspace;

pub use config::*;
pub use consolidation::*;
pub use file_tools::*;
pub use memory_tools::*;
pub use orchestrator::*;
pub use persona::*;
pub use router::*;
pub use session::*;
pub use shell_tool::*;
pub use skill::*;
pub use web_search_tool::*;
pub use web_fetch_tool::*;
pub use subagent::*;
pub use subagent_tool::*;
pub use tool::*;
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
