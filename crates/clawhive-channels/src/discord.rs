use std::sync::Arc;

use chrono::Utc;
use clawhive_bus::{EventBus, Topic};
use clawhive_gateway::Gateway;
use clawhive_schema::{BusMessage, InboundMessage, OutboundMessage};
use serenity::all::{
    ChannelId, Client, Context, EventHandler, GatewayIntents, Http, Message, Ready,
};
use serenity::async_trait;
use tokio::sync::RwLock;
use uuid::Uuid;

pub struct DiscordAdapter {
    connector_id: String,
}

impl DiscordAdapter {
    pub fn new(connector_id: impl Into<String>) -> Self {
        Self {
            connector_id: connector_id.into(),
        }
    }

    pub fn to_inbound(
        &self,
        guild_id: Option<u64>,
        channel_id: u64,
        user_id: u64,
        text: &str,
    ) -> InboundMessage {
        let conversation_scope = match guild_id {
            Some(gid) => format!("guild:{gid}:channel:{channel_id}"),
            None => format!("dm:{channel_id}"),
        };
        InboundMessage {
            trace_id: Uuid::new_v4(),
            channel_type: "discord".to_string(),
            connector_id: self.connector_id.clone(),
            conversation_scope,
            user_scope: format!("user:{user_id}"),
            text: text.to_string(),
            at: Utc::now(),
            thread_id: None,
            is_mention: false,
            mention_target: None,
            message_id: None,
            attachments: vec![],
        }
    }

    pub fn render_outbound(&self, outbound: &OutboundMessage) -> String {
        format!(
            "[discord:{}] {}",
            outbound.conversation_scope, outbound.text
        )
    }
}

pub struct DiscordBot {
    token: String,
    connector_id: String,
    gateway: Arc<Gateway>,
    bus: Option<Arc<EventBus>>,
}

impl DiscordBot {
    pub fn new(token: String, connector_id: String, gateway: Arc<Gateway>) -> Self {
        Self {
            token,
            connector_id,
            gateway,
            bus: None,
        }
    }

    pub fn with_bus(mut self, bus: Arc<EventBus>) -> Self {
        self.bus = Some(bus);
        self
    }

    pub async fn run_impl(self) -> anyhow::Result<()> {
        let intents = GatewayIntents::GUILD_MESSAGES
            | GatewayIntents::DIRECT_MESSAGES
            | GatewayIntents::MESSAGE_CONTENT;

        let http_holder: Arc<RwLock<Option<Arc<Http>>>> = Arc::new(RwLock::new(None));
        let connector_id_for_delivery = self.connector_id.clone();

        let handler = DiscordHandler {
            connector_id: self.connector_id,
            gateway: self.gateway,
            http_holder: http_holder.clone(),
        };

        // Spawn delivery listener if bus is available
        if let Some(bus) = self.bus {
            let http_holder_clone = http_holder.clone();
            let connector_id = connector_id_for_delivery.clone();
            tokio::spawn(async move {
                spawn_delivery_listener(bus, http_holder_clone, connector_id).await;
            });
        }

        let mut client = Client::builder(self.token, intents)
            .event_handler(handler)
            .await?;
        client.start().await?;
        Ok(())
    }
}

#[async_trait]
impl crate::ChannelBot for DiscordBot {
    fn channel_type(&self) -> &str {
        "discord"
    }

    fn connector_id(&self) -> &str {
        &self.connector_id
    }

    async fn run(self: Box<Self>) -> anyhow::Result<()> {
        (*self).run_impl().await
    }
}

struct DiscordHandler {
    connector_id: String,
    gateway: Arc<Gateway>,
    http_holder: Arc<RwLock<Option<Arc<Http>>>>,
}

#[async_trait]
impl EventHandler for DiscordHandler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        tracing::info!(
            "discord bot connected: {} ({})",
            ready.user.name,
            self.connector_id
        );
        // Store HTTP client for delivery listener
        let mut holder = self.http_holder.write().await;
        *holder = Some(ctx.http.clone());
    }

    async fn message(&self, ctx: Context, msg: Message) {
        if msg.author.bot {
            return;
        }

        let text = msg.content.trim();
        if text.is_empty() {
            return;
        }

        let adapter = DiscordAdapter::new(self.connector_id.clone());
        let guild_id = msg.guild_id.map(|id| id.get());
        let channel_id = msg.channel_id;
        let user_id = msg.author.id.get();
        let current_user_id = ctx.cache.current_user().id;
        let is_mention = msg.mentions.iter().any(|u| u.id == current_user_id);

        // In guild channels, only respond if this bot is @mentioned
        if guild_id.is_some() && !is_mention {
            return;
        }

        let mut inbound = adapter.to_inbound(guild_id, channel_id.get(), user_id, text);
        inbound.is_mention = is_mention;
        inbound.mention_target = if is_mention {
            Some(format!("<@{}>", current_user_id.get()))
        } else {
            None
        };

        let _ = channel_id.broadcast_typing(&ctx.http).await;

        let gateway = self.gateway.clone();
        let http = ctx.http.clone();
        let http_typing = ctx.http.clone();
        tokio::spawn(async move {
            // Spawn a task to keep typing indicator alive
            let typing_handle = tokio::spawn({
                let http = http_typing.clone();
                async move {
                    loop {
                        tokio::time::sleep(std::time::Duration::from_secs(8)).await;
                        if channel_id.broadcast_typing(&http).await.is_err() {
                            break;
                        }
                    }
                }
            });

            let result = gateway.handle_inbound(inbound).await;

            // Stop typing indicator
            typing_handle.abort();

            match result {
                Ok(outbound) => {
                    if let Err(err) = channel_id.say(&http, outbound.text).await {
                        tracing::error!("failed to send discord reply: {err}");
                    }
                }
                Err(err) => {
                    tracing::error!("discord gateway error: {err}");
                    if let Err(send_err) = channel_id
                        .say(&http, "Internal error, please try again later.")
                        .await
                    {
                        tracing::error!("failed to send discord error message: {send_err}");
                    }
                }
            }
        });
    }
}

/// Parse conversation_scope to extract channel ID
/// Format: "guild:<guild_id>:channel:<channel_id>" or "dm:<channel_id>"
fn parse_channel_id(conversation_scope: &str) -> Option<u64> {
    if let Some(rest) = conversation_scope.strip_prefix("dm:") {
        return rest.parse().ok();
    }
    if conversation_scope.contains(":channel:") {
        let parts: Vec<&str> = conversation_scope.split(":channel:").collect();
        if parts.len() == 2 {
            return parts[1].parse().ok();
        }
    }
    None
}

/// Spawn a listener for DeliverAnnounce messages
async fn spawn_delivery_listener(
    bus: Arc<EventBus>,
    http_holder: Arc<RwLock<Option<Arc<Http>>>>,
    connector_id: String,
) {
    let mut rx = bus.subscribe(Topic::DeliverAnnounce).await;
    while let Some(msg) = rx.recv().await {
        let BusMessage::DeliverAnnounce {
            channel_type,
            connector_id: msg_connector_id,
            conversation_scope,
            text,
        } = msg
        else {
            continue;
        };

        // Only handle messages for this connector
        if channel_type != "discord" || msg_connector_id != connector_id {
            continue;
        }

        // Get HTTP client
        let http = {
            let holder = http_holder.read().await;
            holder.clone()
        };

        let Some(http) = http else {
            tracing::warn!("Discord HTTP client not ready for delivery");
            continue;
        };

        // Parse channel ID from conversation_scope
        let Some(channel_id) = parse_channel_id(&conversation_scope) else {
            tracing::warn!(
                "Could not parse channel ID from conversation_scope: {}",
                conversation_scope
            );
            continue;
        };

        let channel = ChannelId::new(channel_id);
        if let Err(e) = channel.say(&http, &text).await {
            tracing::error!("Failed to deliver announce message: {e}");
        } else {
            tracing::info!("Delivered scheduled task result to channel {}", channel_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_to_inbound_dm_sets_fields() {
        let adapter = DiscordAdapter::new("dc_main");
        let msg = adapter.to_inbound(None, 123, 456, "hello");
        assert_eq!(msg.channel_type, "discord");
        assert_eq!(msg.connector_id, "dc_main");
        assert_eq!(msg.conversation_scope, "dm:123");
        assert_eq!(msg.user_scope, "user:456");
        assert_eq!(msg.text, "hello");
    }

    #[test]
    fn adapter_to_inbound_guild_sets_fields() {
        let adapter = DiscordAdapter::new("dc_main");
        let msg = adapter.to_inbound(Some(999), 123, 456, "hello");
        assert_eq!(msg.conversation_scope, "guild:999:channel:123");
    }

    #[test]
    fn adapter_to_inbound_defaults() {
        let adapter = DiscordAdapter::new("dc_main");
        let msg = adapter.to_inbound(None, 123, 456, "hello");
        assert!(!msg.is_mention);
        assert_eq!(msg.thread_id, None);
        assert_eq!(msg.mention_target, None);
    }

    #[test]
    fn render_outbound_formats_correctly() {
        let adapter = DiscordAdapter::new("dc_main");
        let outbound = OutboundMessage {
            trace_id: Uuid::new_v4(),
            channel_type: "discord".into(),
            connector_id: "dc_main".into(),
            conversation_scope: "guild:999:channel:123".into(),
            text: "hello world".into(),
            at: Utc::now(),
            reply_to: None,
            attachments: vec![],
        };
        let rendered = adapter.render_outbound(&outbound);
        assert_eq!(rendered, "[discord:guild:999:channel:123] hello world");
    }

    #[test]
    fn adapter_to_inbound_text_preservation() {
        let adapter = DiscordAdapter::new("dc_main");
        let text = "  hello 世界 🦀  ";
        let msg = adapter.to_inbound(None, 123, 456, text);
        assert_eq!(msg.text, text);
    }

    #[test]
    fn adapter_to_inbound_trace_id_unique() {
        let adapter = DiscordAdapter::new("dc_main");
        let msg1 = adapter.to_inbound(None, 123, 456, "hello");
        let msg2 = adapter.to_inbound(None, 123, 456, "hello");
        assert_ne!(msg1.trace_id, msg2.trace_id);
    }

    #[test]
    fn render_outbound_dm_scope() {
        let adapter = DiscordAdapter::new("dc_main");
        let outbound = OutboundMessage {
            trace_id: Uuid::new_v4(),
            channel_type: "discord".into(),
            connector_id: "dc_main".into(),
            conversation_scope: "dm:789".into(),
            text: "reply text".into(),
            at: Utc::now(),
            reply_to: None,
            attachments: vec![],
        };
        let rendered = adapter.render_outbound(&outbound);
        assert_eq!(rendered, "[discord:dm:789] reply text");
    }

    #[test]
    fn adapter_connector_id_preserved() {
        let adapter = DiscordAdapter::new("dc-prod-1");
        let msg = adapter.to_inbound(None, 123, 456, "test");
        assert_eq!(msg.connector_id, "dc-prod-1");
    }
}
