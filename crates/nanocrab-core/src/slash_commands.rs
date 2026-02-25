//! Slash command parser and handler for user-facing commands like /reset, /new, /model.
//!
//! Commands are processed before reaching the LLM. This module provides:
//! - Command parsing from message text
//! - Command execution returning either a direct response or a modified message flow

use chrono::Utc;

/// Parsed slash command from user input
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashCommand {
    /// /new - Start a fresh session
    /// Optional model hint (e.g., "/new opus")
    New { model_hint: Option<String> },
    /// /model - Show current model info
    Model,
    /// /status - Show session status
    Status,
}

/// Result of executing a slash command
#[derive(Debug)]
pub enum CommandResult {
    /// Direct response to user (command fully handled)
    DirectResponse(String),
    /// Session was reset; continue with post-reset flow
    SessionReset {
        model_hint: Option<String>,
        post_reset_prompt: String,
    },
    /// Not a command; pass through to normal handling
    NotACommand,
}

/// Parse a message to check if it starts with a slash command
pub fn parse_command(text: &str) -> Option<SlashCommand> {
    let trimmed = text.trim();
    if !trimmed.starts_with('/') {
        return None;
    }

    let mut parts = trimmed.split_whitespace();
    let cmd = parts.next()?;
    let rest: Vec<&str> = parts.collect();

    match cmd.to_lowercase().as_str() {
        "/new" => {
            let model_hint = rest.first().map(|s| s.to_string());
            Some(SlashCommand::New { model_hint })
        }
        "/model" => Some(SlashCommand::Model),
        "/status" => Some(SlashCommand::Status),
        _ => None,
    }
}

/// Build the post-reset system prompt that instructs the agent to read startup files
pub fn build_post_reset_prompt(agent_id: &str) -> String {
    let now = Utc::now();
    let date_str = now.format("%Y-%m-%d %H:%M:%S UTC").to_string();
    let today = now.format("%Y-%m-%d").to_string();
    let yesterday = (now - chrono::Duration::days(1)).format("%Y-%m-%d").to_string();

    format!(
        r#"[{date_str}] ⚠️ Post-Reset Audit: Session has been reset. The following startup files should be read:
  - AGENTS.md (if exists)
  - SOUL.md (if exists)
  - USER.md (if exists)
  - memory/{today}.md (today's notes, if exists)
  - memory/{yesterday}.md (yesterday's notes, if exists)
  - MEMORY.md (long-term memory, if in main session)

Please read these files using the read tool before continuing. This ensures your operating context is restored after session reset.

A new session was started via /new or /reset. Greet the user in your configured persona, if one is provided. Be yourself - use your defined voice, mannerisms, and mood. Keep it to 1-3 sentences and ask what they want to do.

Agent: {agent_id}
Session: new"#
    )
}

/// Format a status response
pub fn format_status_response(agent_id: &str, model: &str, session_key: &str) -> String {
    format!(
        "📊 **Session Status**\n\
         Agent: `{agent_id}`\n\
         Model: `{model}`\n\
         Session: `{session_key}`"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_new_command() {
        assert_eq!(
            parse_command("/new"),
            Some(SlashCommand::New { model_hint: None })
        );
        assert_eq!(
            parse_command("/new opus"),
            Some(SlashCommand::New {
                model_hint: Some("opus".to_string())
            })
        );
        assert_eq!(
            parse_command("  /new  sonnet  "),
            Some(SlashCommand::New {
                model_hint: Some("sonnet".to_string())
            })
        );
    }

    #[test]
    fn parse_model_command() {
        assert_eq!(parse_command("/model"), Some(SlashCommand::Model));
        assert_eq!(parse_command("/MODEL"), Some(SlashCommand::Model));
    }

    #[test]
    fn parse_status_command() {
        assert_eq!(parse_command("/status"), Some(SlashCommand::Status));
    }

    #[test]
    fn parse_not_a_command() {
        assert_eq!(parse_command("hello"), None);
        assert_eq!(parse_command(""), None);
        assert_eq!(parse_command("not a /command"), None);
        assert_eq!(parse_command("/unknown"), None);
        assert_eq!(parse_command("/reset"), None); // /reset not supported
    }

    #[test]
    fn post_reset_prompt_contains_key_elements() {
        let prompt = build_post_reset_prompt("test-agent");
        assert!(prompt.contains("Post-Reset Audit"));
        assert!(prompt.contains("AGENTS.md"));
        assert!(prompt.contains("SOUL.md"));
        assert!(prompt.contains("MEMORY.md"));
        assert!(prompt.contains("test-agent"));
    }

    #[test]
    fn status_response_format() {
        let status = format_status_response("main", "claude-sonnet", "session:123");
        assert!(status.contains("main"));
        assert!(status.contains("claude-sonnet"));
        assert!(status.contains("session:123"));
    }
}
