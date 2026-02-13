use std::sync::Arc;

use chrono::Utc;
use nanocrab_gateway::Gateway;
use nanocrab_schema::{InboundMessage, OutboundMessage};
use serenity::all::{Client, Context, EventHandler, GatewayIntents, Message, Ready};
use serenity::async_trait;
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
}

impl DiscordBot {
    pub fn new(token: String, connector_id: String, gateway: Arc<Gateway>) -> Self {
        Self {
            token,
            connector_id,
            gateway,
        }
    }

    pub async fn run_impl(self) -> anyhow::Result<()> {
        let intents = GatewayIntents::GUILD_MESSAGES
            | GatewayIntents::DIRECT_MESSAGES
            | GatewayIntents::MESSAGE_CONTENT;
        let handler = DiscordHandler {
            connector_id: self.connector_id,
            gateway: self.gateway,
        };
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
}

#[async_trait]
impl EventHandler for DiscordHandler {
    async fn ready(&self, _ctx: Context, ready: Ready) {
        tracing::info!(
            "discord bot connected: {} ({})",
            ready.user.name,
            self.connector_id
        );
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
        tokio::spawn(async move {
            match gateway.handle_inbound(inbound).await {
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
        };
        let rendered = adapter.render_outbound(&outbound);
        assert_eq!(rendered, "[discord:guild:999:channel:123] hello world");
    }
}
