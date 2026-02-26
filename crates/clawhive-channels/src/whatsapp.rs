//! WhatsApp channel integration via HTTP bridge.
//!
//! This module provides WhatsApp functionality by connecting to an external
//! WhatsApp Web bridge service (e.g., whatsapp-web.js based).
//!
//! The bridge is expected to expose:
//! - GET  /messages?since=<timestamp> - poll new messages
//! - POST /messages - send a message
//! - GET  /status - connection status
//! - GET  /qr - QR code for linking (if not authenticated)

use std::sync::Arc;

use anyhow::{anyhow, Result};
use chrono::Utc;
use clawhive_gateway::Gateway;
use clawhive_schema::InboundMessage;
use serde::{Deserialize, Serialize};
use tokio::time::{interval, Duration};
use uuid::Uuid;

/// WhatsApp message from the bridge.
#[derive(Debug, Clone, Deserialize)]
pub struct BridgeMessage {
    pub id: String,
    pub from: String,
    pub chat_id: String,
    pub body: String,
    pub timestamp: i64,
    #[serde(default)]
    pub is_group: bool,
    #[serde(default)]
    pub mentioned: bool,
}

/// Send message request.
#[derive(Debug, Clone, Serialize)]
pub struct SendMessageRequest {
    pub chat_id: String,
    pub body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quoted_msg_id: Option<String>,
}

/// Bridge status response.
#[derive(Debug, Clone, Deserialize)]
pub struct BridgeStatus {
    pub connected: bool,
    pub authenticated: bool,
    #[serde(default)]
    pub phone_number: Option<String>,
}

/// WhatsApp adapter.
pub struct WhatsAppAdapter {
    connector_id: String,
}

impl WhatsAppAdapter {
    pub fn new(connector_id: impl Into<String>) -> Self {
        Self {
            connector_id: connector_id.into(),
        }
    }

    /// Convert a bridge message to InboundMessage.
    pub fn to_inbound(&self, msg: &BridgeMessage) -> InboundMessage {
        InboundMessage {
            trace_id: Uuid::new_v4(),
            channel_type: "whatsapp".to_string(),
            connector_id: self.connector_id.clone(),
            conversation_scope: format!("chat:{}", msg.chat_id),
            user_scope: format!("user:{}", normalize_phone(&msg.from)),
            text: msg.body.clone(),
            at: Utc::now(),
            thread_id: None,
            is_mention: msg.mentioned || !msg.is_group,
            mention_target: None,
        }
    }
}

/// Normalize a phone number.
fn normalize_phone(phone: &str) -> String {
    phone
        .chars()
        .filter(|c| c.is_ascii_digit() || *c == '+')
        .collect()
}

/// WhatsApp bot configuration.
#[derive(Debug, Clone)]
pub struct WhatsAppBotConfig {
    /// Bridge URL (e.g., http://localhost:3000)
    pub bridge_url: String,
    /// Connector ID
    pub connector_id: String,
    /// Poll interval in seconds
    pub poll_interval_secs: u64,
}

impl WhatsAppBotConfig {
    pub fn new(bridge_url: impl Into<String>, connector_id: impl Into<String>) -> Self {
        Self {
            bridge_url: bridge_url.into(),
            connector_id: connector_id.into(),
            poll_interval_secs: 2,
        }
    }

    pub fn with_poll_interval(mut self, secs: u64) -> Self {
        self.poll_interval_secs = secs;
        self
    }
}

/// WhatsApp bot using HTTP bridge.
pub struct WhatsAppBot {
    config: WhatsAppBotConfig,
    gateway: Arc<Gateway>,
    client: reqwest::Client,
}

impl WhatsAppBot {
    pub fn new(config: WhatsAppBotConfig, gateway: Arc<Gateway>) -> Self {
        Self {
            config,
            gateway,
            client: reqwest::Client::new(),
        }
    }

    /// Check bridge status.
    pub async fn check_status(&self) -> Result<BridgeStatus> {
        let url = format!("{}/status", self.config.bridge_url);
        let resp = self.client.get(&url).send().await?;

        if !resp.status().is_success() {
            return Err(anyhow!("Bridge returned status {}", resp.status()));
        }

        Ok(resp.json().await?)
    }

    /// Get QR code for linking.
    pub async fn get_qr_code(&self) -> Result<String> {
        let url = format!("{}/qr", self.config.bridge_url);
        let resp = self.client.get(&url).send().await?;

        if !resp.status().is_success() {
            return Err(anyhow!("Failed to get QR code: {}", resp.status()));
        }

        Ok(resp.text().await?)
    }

    /// Send a message.
    pub async fn send_message(&self, chat_id: &str, body: &str) -> Result<()> {
        let url = format!("{}/messages", self.config.bridge_url);
        let req = SendMessageRequest {
            chat_id: chat_id.to_string(),
            body: body.to_string(),
            quoted_msg_id: None,
        };

        let resp = self.client.post(&url).json(&req).send().await?;

        if !resp.status().is_success() {
            return Err(anyhow!("Failed to send message: {}", resp.status()));
        }

        Ok(())
    }

    /// Run the WhatsApp bot with polling.
    pub async fn run_impl(self) -> Result<()> {
        // Check initial status
        let status = self.check_status().await?;

        if !status.authenticated {
            tracing::warn!("WhatsApp not authenticated. Use GET {}/qr to link.", self.config.bridge_url);
        }

        if !status.connected {
            tracing::warn!("WhatsApp bridge not connected");
        }

        tracing::info!(
            "WhatsApp bot started (connector: {}, phone: {:?})",
            self.config.connector_id,
            status.phone_number
        );

        let adapter = Arc::new(WhatsAppAdapter::new(&self.config.connector_id));
        let mut last_timestamp: i64 = Utc::now().timestamp();
        let mut poll_timer = interval(Duration::from_secs(self.config.poll_interval_secs));

        loop {
            poll_timer.tick().await;

            match self.poll_messages(last_timestamp).await {
                Ok(messages) => {
                    for msg in messages {
                        let ts = msg.timestamp;

                        let inbound = adapter.to_inbound(&msg);

                        tracing::debug!(
                            "WhatsApp message from {} in {}: {}",
                            msg.from,
                            msg.chat_id,
                            msg.body
                        );

                        if let Err(e) = self.gateway.handle_inbound(inbound).await {
                            tracing::error!("Failed to handle WhatsApp message: {e}");
                        }

                        if ts > last_timestamp {
                            last_timestamp = ts;
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Failed to poll WhatsApp messages: {e}");
                }
            }
        }
    }

    async fn poll_messages(&self, since: i64) -> Result<Vec<BridgeMessage>> {
        let url = format!("{}/messages?since={}", self.config.bridge_url, since);
        let resp = self.client.get(&url).send().await?;

        if !resp.status().is_success() {
            return Err(anyhow!("Bridge returned status {}", resp.status()));
        }

        Ok(resp.json().await?)
    }
}

#[async_trait::async_trait]
impl super::ChannelBot for WhatsAppBot {
    fn channel_type(&self) -> &str {
        "whatsapp"
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
    fn normalize_phone_removes_formatting() {
        assert_eq!(normalize_phone("+1 (555) 123-4567"), "+15551234567");
        assert_eq!(normalize_phone("1234567890"), "1234567890");
    }

    #[test]
    fn adapter_to_inbound_basic() {
        let adapter = WhatsAppAdapter::new("wa-main");
        let msg = BridgeMessage {
            id: "msg1".to_string(),
            from: "+1-555-123-4567".to_string(),
            chat_id: "+15551234567@c.us".to_string(),
            body: "hello".to_string(),
            timestamp: 1234567890,
            is_group: false,
            mentioned: false,
        };

        let inbound = adapter.to_inbound(&msg);

        assert_eq!(inbound.channel_type, "whatsapp");
        assert_eq!(inbound.connector_id, "wa-main");
        assert_eq!(inbound.conversation_scope, "chat:+15551234567@c.us");
        assert_eq!(inbound.user_scope, "user:+15551234567");
        assert_eq!(inbound.text, "hello");
        assert!(inbound.is_mention); // DM
    }

    #[test]
    fn adapter_to_inbound_group() {
        let adapter = WhatsAppAdapter::new("wa-main");
        let msg = BridgeMessage {
            id: "msg2".to_string(),
            from: "+15551234567@c.us".to_string(),
            chat_id: "123456789@g.us".to_string(),
            body: "group message".to_string(),
            timestamp: 1234567890,
            is_group: true,
            mentioned: false,
        };

        let inbound = adapter.to_inbound(&msg);
        assert!(!inbound.is_mention); // Group without mention
    }

    #[test]
    fn adapter_to_inbound_group_mention() {
        let adapter = WhatsAppAdapter::new("wa-main");
        let msg = BridgeMessage {
            id: "msg3".to_string(),
            from: "+15551234567@c.us".to_string(),
            chat_id: "123456789@g.us".to_string(),
            body: "@bot hello".to_string(),
            timestamp: 1234567890,
            is_group: true,
            mentioned: true,
        };

        let inbound = adapter.to_inbound(&msg);
        assert!(inbound.is_mention); // Group with mention
    }

    #[test]
    fn config_builder() {
        let config = WhatsAppBotConfig::new("http://localhost:3000", "wa-main")
            .with_poll_interval(5);

        assert_eq!(config.bridge_url, "http://localhost:3000");
        assert_eq!(config.poll_interval_secs, 5);
    }
}
