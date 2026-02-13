pub mod config;
pub mod consolidation;
pub mod memory_tools;
pub mod orchestrator;
pub mod persona;
pub mod router;
pub mod session;
pub mod skill;
pub mod subagent;
pub mod tool;

pub use config::*;
pub use consolidation::*;
pub use memory_tools::*;
pub use orchestrator::*;
pub use persona::*;
pub use router::*;
pub use session::*;
pub use skill::*;
pub use subagent::*;
pub use tool::*;

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
