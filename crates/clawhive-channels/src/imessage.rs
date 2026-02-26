//! iMessage channel integration for macOS.
//!
//! This module provides iMessage functionality via AppleScript on macOS.
//! Requires Full Disk Access for reading the Messages database.

use std::process::Command;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use chrono::Utc;
use clawhive_gateway::Gateway;
use clawhive_schema::{InboundMessage, OutboundMessage};
use tokio::time::{interval, Duration};
use uuid::Uuid;

/// iMessage adapter for macOS.
pub struct IMessageAdapter {
    connector_id: String,
}

impl IMessageAdapter {
    pub fn new(connector_id: impl Into<String>) -> Self {
        Self {
            connector_id: connector_id.into(),
        }
    }

    /// Convert an iMessage to InboundMessage.
    pub fn to_inbound(&self, sender: &str, text: &str) -> InboundMessage {
        // Normalize phone/email to conversation scope
        let conversation_scope = format!("chat:{}", normalize_handle(sender));
        let user_scope = format!("user:{}", normalize_handle(sender));

        InboundMessage {
            trace_id: Uuid::new_v4(),
            channel_type: "imessage".to_string(),
            connector_id: self.connector_id.clone(),
            conversation_scope,
            user_scope,
            text: text.to_string(),
            at: Utc::now(),
            thread_id: None,
            is_mention: true, // DMs are always "mentions"
            mention_target: None,
            message_id: None,
            attachments: vec![],
            group_context: None,
        }
    }
}

/// Normalize a phone number or email handle.
fn normalize_handle(handle: &str) -> String {
    handle
        .trim()
        .replace([' ', '-', '(', ')'], "")
        .to_lowercase()
}

/// Send an iMessage via AppleScript.
pub fn send_imessage(to: &str, message: &str) -> Result<()> {
    let script = format!(
        r#"
        tell application "Messages"
            set targetService to 1st account whose service type = iMessage
            set targetBuddy to participant "{}" of targetService
            send "{}" to targetBuddy
        end tell
        "#,
        escape_applescript(to),
        escape_applescript(message)
    );

    let output = Command::new("osascript").arg("-e").arg(&script).output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("Failed to send iMessage: {}", stderr));
    }

    Ok(())
}

/// Escape a string for AppleScript.
fn escape_applescript(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// iMessage bot that polls for new messages.
pub struct IMessageBot {
    connector_id: String,
    gateway: Arc<Gateway>,
    poll_interval_secs: u64,
}

impl IMessageBot {
    pub fn new(connector_id: impl Into<String>, gateway: Arc<Gateway>) -> Self {
        Self {
            connector_id: connector_id.into(),
            gateway,
            poll_interval_secs: 5,
        }
    }

    pub fn with_poll_interval(mut self, secs: u64) -> Self {
        self.poll_interval_secs = secs;
        self
    }

    /// Run the iMessage bot.
    pub async fn run_impl(self) -> Result<()> {
        let adapter = Arc::new(IMessageAdapter::new(&self.connector_id));
        let gateway = self.gateway;
        let connector_id = self.connector_id.clone();

        // Track last processed message ROWID
        let mut last_rowid = get_last_message_rowid().unwrap_or(0);

        tracing::info!(
            "iMessage bot started (connector: {}, last_rowid: {})",
            connector_id,
            last_rowid
        );

        let mut poll_timer = interval(Duration::from_secs(self.poll_interval_secs));

        loop {
            poll_timer.tick().await;

            match poll_new_messages(last_rowid) {
                Ok(messages) => {
                    for (rowid, sender, text) in messages {
                        tracing::debug!("iMessage from {}: {}", sender, text);

                        let inbound = adapter.to_inbound(&sender, &text);

                        if let Err(e) = gateway.handle_inbound(inbound).await {
                            tracing::error!("Failed to handle iMessage: {e}");
                        }

                        last_rowid = rowid.max(last_rowid);
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to poll iMessages: {e}");
                }
            }
        }
    }
}

/// Get the last message ROWID from the Messages database.
fn get_last_message_rowid() -> Result<i64> {
    let db_path = expand_tilde("~/Library/Messages/chat.db");

    let output = Command::new("sqlite3")
        .arg(&db_path)
        .arg("SELECT MAX(ROWID) FROM message WHERE is_from_me = 0;")
        .output()?;

    if !output.status.success() {
        return Err(anyhow!("Failed to query Messages database"));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let rowid: i64 = stdout.trim().parse().unwrap_or(0);
    Ok(rowid)
}

/// Poll for new messages since the given ROWID.
fn poll_new_messages(since_rowid: i64) -> Result<Vec<(i64, String, String)>> {
    let db_path = expand_tilde("~/Library/Messages/chat.db");

    let query = format!(
        r#"
        SELECT m.ROWID, h.id, m.text
        FROM message m
        JOIN handle h ON m.handle_id = h.ROWID
        WHERE m.ROWID > {} AND m.is_from_me = 0 AND m.text IS NOT NULL
        ORDER BY m.ROWID ASC
        LIMIT 100;
        "#,
        since_rowid
    );

    let output = Command::new("sqlite3")
        .arg("-separator")
        .arg("\t")
        .arg(&db_path)
        .arg(&query)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("Failed to query Messages database: {}", stderr));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut results = Vec::new();

    for line in stdout.lines() {
        let parts: Vec<&str> = line.splitn(3, '\t').collect();
        if parts.len() >= 3 {
            if let Ok(rowid) = parts[0].parse::<i64>() {
                results.push((rowid, parts[1].to_string(), parts[2].to_string()));
            }
        }
    }

    Ok(results)
}

/// Expand ~ to home directory.
fn expand_tilde(path: &str) -> String {
    if path.starts_with("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return path.replacen("~", &home, 1);
        }
    }
    path.to_string()
}

#[async_trait::async_trait]
impl super::ChannelBot for IMessageBot {
    fn channel_type(&self) -> &str {
        "imessage"
    }

    fn connector_id(&self) -> &str {
        &self.connector_id
    }

    async fn run(self: Box<Self>) -> Result<()> {
        self.run_impl().await
    }
}

/// Delivery handler for sending outbound messages.
pub async fn handle_outbound(outbound: OutboundMessage) -> Result<()> {
    // Parse recipient from conversation_scope (format: "chat:+1234567890")
    let recipient = outbound
        .conversation_scope
        .strip_prefix("chat:")
        .unwrap_or(&outbound.conversation_scope);

    send_imessage(recipient, &outbound.text)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_handle_phone() {
        assert_eq!(normalize_handle("+1 (555) 123-4567"), "+15551234567");
        assert_eq!(normalize_handle("555-123-4567"), "5551234567");
    }

    #[test]
    fn normalize_handle_email() {
        assert_eq!(normalize_handle("User@Example.COM"), "user@example.com");
    }

    #[test]
    fn escape_applescript_quotes() {
        assert_eq!(escape_applescript(r#"Hello "World""#), r#"Hello \"World\""#);
    }

    #[test]
    fn escape_applescript_newlines() {
        assert_eq!(escape_applescript("Line1\nLine2"), "Line1\\nLine2");
    }

    #[test]
    fn adapter_to_inbound() {
        let adapter = IMessageAdapter::new("imessage-main");
        let inbound = adapter.to_inbound("+1 555 123-4567", "Hello");

        assert_eq!(inbound.channel_type, "imessage");
        assert_eq!(inbound.conversation_scope, "chat:+15551234567");
        assert_eq!(inbound.user_scope, "user:+15551234567");
        assert_eq!(inbound.text, "Hello");
        assert!(inbound.is_mention);
    }

    #[test]
    fn expand_tilde_works() {
        let expanded = expand_tilde("~/Library/Messages/chat.db");
        assert!(!expanded.starts_with("~"));
        assert!(expanded.contains("/Library/Messages/chat.db"));
    }
}
