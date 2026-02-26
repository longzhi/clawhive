use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::debug;

use crate::persona::Persona;

/// Default heartbeat prompt sent to agents.
pub const DEFAULT_HEARTBEAT_PROMPT: &str =
    "Read HEARTBEAT.md if it exists (workspace context). Follow it strictly. \
     Do not infer or repeat old tasks from prior chats. \
     If nothing needs attention, reply HEARTBEAT_OK.";

/// Heartbeat configuration.
#[derive(Debug, Clone)]
pub struct HeartbeatConfig {
    /// Interval between heartbeats (0 = disabled).
    pub interval: Duration,
    /// Custom heartbeat prompt (uses DEFAULT_HEARTBEAT_PROMPT if None).
    pub prompt: Option<String>,
    /// Maximum characters in response to consider as "ack" (suppress delivery).
    pub ack_max_chars: usize,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(30 * 60), // 30 minutes
            prompt: None,
            ack_max_chars: 50,
        }
    }
}

impl HeartbeatConfig {
    pub fn disabled() -> Self {
        Self {
            interval: Duration::ZERO,
            ..Default::default()
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.interval > Duration::ZERO
    }

    pub fn prompt(&self) -> &str {
        self.prompt.as_deref().unwrap_or(DEFAULT_HEARTBEAT_PROMPT)
    }
}

/// Check if a response is a heartbeat acknowledgment (should be suppressed).
pub fn is_heartbeat_ack(response: &str, max_chars: usize) -> bool {
    let trimmed = response.trim();
    if trimmed.len() > max_chars {
        return false;
    }

    // Check for HEARTBEAT_OK anywhere in the response
    trimmed.contains("HEARTBEAT_OK")
}

/// Check if HEARTBEAT.md has meaningful content worth processing.
/// Empty files or files with only comments/headers should skip the heartbeat.
pub fn should_skip_heartbeat(heartbeat_content: &str) -> bool {
    !heartbeat_content.lines().any(|line| {
        let trimmed = line.trim();
        !trimmed.is_empty() && !trimmed.starts_with('#')
    })
}

/// Heartbeat manager that runs periodic heartbeats for agents.
pub struct HeartbeatManager {
    config: HeartbeatConfig,
    #[allow(dead_code)]
    running: Arc<RwLock<bool>>,
}

impl HeartbeatManager {
    pub fn new(config: HeartbeatConfig) -> Self {
        Self {
            config,
            running: Arc::new(RwLock::new(false)),
        }
    }

    /// Check if heartbeat should run based on persona's HEARTBEAT.md content.
    pub fn should_run(&self, persona: &Persona) -> bool {
        if !self.config.is_enabled() {
            return false;
        }

        // Skip if HEARTBEAT.md is empty or only comments
        if should_skip_heartbeat(&persona.heartbeat_md) {
            debug!(
                "Skipping heartbeat for {} - no tasks in HEARTBEAT.md",
                persona.agent_id
            );
            return false;
        }

        true
    }

    /// Get the heartbeat prompt to send.
    pub fn prompt(&self) -> &str {
        self.config.prompt()
    }

    /// Check if response should be suppressed (is an ack).
    pub fn should_suppress_response(&self, response: &str) -> bool {
        is_heartbeat_ack(response, self.config.ack_max_chars)
    }

    /// Get heartbeat interval.
    pub fn interval(&self) -> Duration {
        self.config.interval
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_heartbeat_ack() {
        assert!(is_heartbeat_ack("HEARTBEAT_OK", 50));
        assert!(is_heartbeat_ack("  HEARTBEAT_OK  ", 50));
        assert!(is_heartbeat_ack("Nothing to report. HEARTBEAT_OK", 50));
        assert!(!is_heartbeat_ack("Here's what I found: ...", 50));
        assert!(!is_heartbeat_ack(&"x".repeat(100), 50)); // Too long
    }

    #[test]
    fn test_should_skip_heartbeat() {
        assert!(should_skip_heartbeat(""));
        assert!(should_skip_heartbeat("# HEARTBEAT.md\n\n# Just comments"));
        assert!(should_skip_heartbeat("   \n\n   "));
        assert!(!should_skip_heartbeat("# HEARTBEAT.md\n- Check email"));
        assert!(!should_skip_heartbeat("Check calendar"));
    }

    #[test]
    fn test_heartbeat_config() {
        let config = HeartbeatConfig::default();
        assert!(config.is_enabled());
        assert_eq!(config.interval, Duration::from_secs(30 * 60));

        let disabled = HeartbeatConfig::disabled();
        assert!(!disabled.is_enabled());
    }
}
