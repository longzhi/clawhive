//! Slack channel integration using Web API.
//!
//! This module provides Slack bot functionality via the Web API.
//! For real-time events, use Slack's Events API with a webhook endpoint,
//! or poll conversations.history.

use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;
use clawhive_gateway::Gateway;
use clawhive_schema::InboundMessage;
use slack_morphism::prelude::*;
use tokio::time::{interval, Duration};
use uuid::Uuid;

// Type alias for the Slack client
type HyperClient = SlackClient<SlackClientHyperHttpsConnector>;

/// Adapter for converting between Slack and internal message formats.
pub struct SlackAdapter {
    connector_id: String,
}

impl SlackAdapter {
    pub fn new(connector_id: impl Into<String>) -> Self {
        Self {
            connector_id: connector_id.into(),
        }
    }

    /// Convert a Slack message to InboundMessage.
    pub fn to_inbound(
        &self,
        channel: &str,
        user: &str,
        text: &str,
        thread_ts: Option<&str>,
        is_mention: bool,
    ) -> InboundMessage {
        InboundMessage {
            trace_id: Uuid::new_v4(),
            channel_type: "slack".to_string(),
            connector_id: self.connector_id.clone(),
            conversation_scope: format!("channel:{channel}"),
            user_scope: format!("user:{user}"),
            text: text.to_string(),
            at: Utc::now(),
            thread_id: thread_ts.map(|s| s.to_string()),
            is_mention,
            mention_target: None,
            message_id: None,
            attachments: vec![],
        group_context: None,
        }
    }
}

/// Slack bot configuration.
#[derive(Debug, Clone)]
pub struct SlackBotConfig {
    /// Bot token (xoxb-...)
    pub bot_token: String,
    /// Connector ID for this bot instance
    pub connector_id: String,
    /// Channels to monitor (channel IDs)
    pub channels: Vec<String>,
    /// Poll interval in seconds
    pub poll_interval_secs: u64,
}

impl SlackBotConfig {
    pub fn new(bot_token: impl Into<String>, connector_id: impl Into<String>) -> Self {
        Self {
            bot_token: bot_token.into(),
            connector_id: connector_id.into(),
            channels: vec![],
            poll_interval_secs: 5,
        }
    }

    pub fn with_channels(mut self, channels: Vec<String>) -> Self {
        self.channels = channels;
        self
    }

    pub fn with_poll_interval(mut self, secs: u64) -> Self {
        self.poll_interval_secs = secs;
        self
    }
}

/// Slack bot using polling (for environments without webhook support).
pub struct SlackBot {
    config: SlackBotConfig,
    gateway: Arc<Gateway>,
}

impl SlackBot {
    pub fn new(config: SlackBotConfig, gateway: Arc<Gateway>) -> Self {
        Self { config, gateway }
    }

    /// Run the Slack bot with polling.
    pub async fn run_impl(self) -> Result<()> {
        let client = SlackClient::new(SlackClientHyperHttpsConnector::new()?);
        let token = SlackApiToken::new(self.config.bot_token.clone().into());

        // Get bot user ID
        let session = client.open_session(&token);
        let auth_test = session.auth_test().await?;
        let bot_user_id = auth_test.user_id.0.clone();

        tracing::info!(
            "Slack bot connected as {} ({})",
            auth_test.user.unwrap_or_default(),
            bot_user_id
        );

        let adapter = Arc::new(SlackAdapter::new(&self.config.connector_id));

        // Track last message timestamp per channel
        let mut last_ts: std::collections::HashMap<String, String> = std::collections::HashMap::new();

        let mut poll_timer = interval(Duration::from_secs(self.config.poll_interval_secs));

        loop {
            poll_timer.tick().await;

            for channel_id in &self.config.channels {
                let oldest = last_ts.get(channel_id).map(|s| s.as_str());

                match self
                    .poll_channel(&client, &token, channel_id, oldest, &bot_user_id, &adapter)
                    .await
                {
                    Ok(Some(newest_ts)) => {
                        last_ts.insert(channel_id.clone(), newest_ts);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!("Failed to poll Slack channel {channel_id}: {e}");
                    }
                }
            }
        }
    }

    async fn poll_channel(
        &self,
        client: &HyperClient,
        token: &SlackApiToken,
        channel_id: &str,
        oldest: Option<&str>,
        bot_user_id: &str,
        adapter: &SlackAdapter,
    ) -> Result<Option<String>> {
        let session = client.open_session(token);

        let mut request = SlackApiConversationsHistoryRequest::new()
            .with_channel(SlackChannelId(channel_id.to_string()))
            .with_limit(20);

        if let Some(ts) = oldest {
            request = request.with_oldest(SlackTs(ts.to_string()));
        }

        let response = session.conversations_history(&request).await?;

        let mut newest_ts: Option<String> = None;

        for msg in response.messages.into_iter().rev() {
            // Extract timestamp
            let ts = msg.origin.ts.0.clone();

            // Track newest
            if newest_ts.is_none() || ts > *newest_ts.as_ref().unwrap() {
                newest_ts = Some(ts.clone());
            }

            // Skip if older than our cursor
            if let Some(oldest) = oldest {
                if ts.as_str() <= oldest {
                    continue;
                }
            }

            // Extract user
            let user = match &msg.sender.user {
                Some(u) => u.0.clone(),
                None => continue,
            };

            // Skip bot messages
            if user == bot_user_id {
                continue;
            }

            // Extract text
            let text = msg
                .content
                .text
                .as_ref()
                .map(|t| t.as_str())
                .unwrap_or("");

            if text.is_empty() {
                continue;
            }

            // Check for mention
            let is_mention = text.contains(&format!("<@{bot_user_id}>"));
            let thread_ts = msg.origin.thread_ts.as_ref().map(|t| t.0.as_str());

            let inbound = adapter.to_inbound(channel_id, &user, text, thread_ts, is_mention);

            tracing::debug!("Slack message from {user} in {channel_id}: {text}");

            if let Err(e) = self.gateway.handle_inbound(inbound).await {
                tracing::error!("Failed to handle Slack message: {e}");
            }
        }

        Ok(newest_ts)
    }
}

/// Send a message to Slack.
pub async fn send_slack_message(
    client: &HyperClient,
    token: &SlackApiToken,
    channel: &str,
    text: &str,
    thread_ts: Option<&str>,
) -> Result<()> {
    let session = client.open_session(token);

    let mut request = SlackApiChatPostMessageRequest::new(
        SlackChannelId(channel.to_string()).into(),
        SlackMessageContent::new().with_text(text.to_string()),
    );

    if let Some(ts) = thread_ts {
        request = request.with_thread_ts(SlackTs(ts.to_string()));
    }

    session.chat_post_message(&request).await?;
    Ok(())
}

#[async_trait::async_trait]
impl super::ChannelBot for SlackBot {
    fn channel_type(&self) -> &str {
        "slack"
    }

    fn connector_id(&self) -> &str {
        &self.config.connector_id
    }

    async fn run(self: Box<Self>) -> Result<()> {
        self.run_impl().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_to_inbound_basic() {
        let adapter = SlackAdapter::new("slack-main");
        let inbound = adapter.to_inbound("C123456", "U789", "hello", None, false);

        assert_eq!(inbound.channel_type, "slack");
        assert_eq!(inbound.connector_id, "slack-main");
        assert_eq!(inbound.conversation_scope, "channel:C123456");
        assert_eq!(inbound.user_scope, "user:U789");
        assert_eq!(inbound.text, "hello");
        assert!(!inbound.is_mention);
        assert!(inbound.thread_id.is_none());
    }

    #[test]
    fn adapter_to_inbound_with_thread() {
        let adapter = SlackAdapter::new("slack-main");
        let inbound = adapter.to_inbound(
            "C123456",
            "U789",
            "reply",
            Some("1234567890.123456"),
            true,
        );

        assert!(inbound.is_mention);
        assert_eq!(inbound.thread_id, Some("1234567890.123456".to_string()));
    }

    #[test]
    fn config_builder() {
        let config = SlackBotConfig::new("xoxb-xxx", "slack-main")
            .with_channels(vec!["C123".to_string(), "C456".to_string()])
            .with_poll_interval(10);

        assert_eq!(config.channels.len(), 2);
        assert_eq!(config.poll_interval_secs, 10);
    }
}
