use std::sync::Arc;

use chrono::Utc;
use clawhive_bus::{EventBus, Topic};
use clawhive_gateway::Gateway;
use clawhive_schema::BusMessage;
use clawhive_schema::{ActionKind, InboundMessage, OutboundMessage};
use teloxide::prelude::*;
use teloxide::types::{ChatAction, Message, MessageEntityKind, MessageId, ReactionType};
use tokio::sync::RwLock;
use uuid::Uuid;

pub struct TelegramAdapter {
    connector_id: String,
}

impl TelegramAdapter {
    pub fn new(connector_id: impl Into<String>) -> Self {
        Self {
            connector_id: connector_id.into(),
        }
    }

    pub fn to_inbound(&self, chat_id: i64, user_id: i64, text: &str, message_id: Option<i32>) -> InboundMessage {
        InboundMessage {
            trace_id: Uuid::new_v4(),
            channel_type: "telegram".to_string(),
            connector_id: self.connector_id.clone(),
            conversation_scope: format!("chat:{chat_id}"),
            user_scope: format!("user:{user_id}"),
            text: text.to_string(),
            at: Utc::now(),
            thread_id: None,
            is_mention: false,
            mention_target: None,
            message_id: message_id.map(|id| id.to_string()),
            attachments: vec![],
        group_context: None,
        }
    }

    pub fn render_outbound(&self, outbound: &OutboundMessage) -> String {
        format!(
            "[telegram:{}] {}",
            outbound.conversation_scope, outbound.text
        )
    }
}

pub struct TelegramBot {
    token: String,
    connector_id: String,
    gateway: Arc<Gateway>,
    bus: Arc<EventBus>,
}

impl TelegramBot {
    pub fn new(
        token: String,
        connector_id: String,
        gateway: Arc<Gateway>,
        bus: Arc<EventBus>,
    ) -> Self {
        Self {
            token,
            connector_id,
            gateway,
            bus,
        }
    }

    pub async fn run_impl(self) -> anyhow::Result<()> {
        let bot = Bot::new(&self.token);
        let adapter = Arc::new(TelegramAdapter::new(&self.connector_id));
        let gateway = self.gateway;
        let bus = self.bus;
        let connector_id = self.connector_id.clone();

        // Create a bot holder for the delivery listener
        let bot_holder: Arc<RwLock<Option<Bot>>> = Arc::new(RwLock::new(Some(bot.clone())));

        // Spawn delivery listener for scheduled task announcements
        let bot_holder_clone = bot_holder.clone();
        let connector_id_clone = connector_id.clone();
        let bus_clone = bus.clone();
        tokio::spawn(spawn_delivery_listener(
            bus_clone,
            bot_holder_clone.clone(),
            connector_id_clone.clone(),
        ));
        tokio::spawn(spawn_action_listener(
            bus,
            bot_holder_clone,
            connector_id_clone,
        ));

        let handler = Update::filter_message().endpoint(move |bot: Bot, msg: Message| {
            let adapter = adapter.clone();
            let gateway = gateway.clone();

            async move {
                let text = match msg.text() {
                    Some(text) => text.to_string(),
                    None => return Ok::<(), teloxide::RequestError>(()),
                };

                let chat_id = msg.chat.id;
                let user_id = msg.from.as_ref().map(|user| user.id.0 as i64).unwrap_or(0);
                let (is_mention, mention_target) = detect_mention(&msg);
                let message_id = msg.id.0;

                let mut inbound = adapter.to_inbound(chat_id.0, user_id, &text, Some(message_id));
                inbound.is_mention = is_mention;
                inbound.mention_target = mention_target;
                inbound.thread_id = msg.thread_id.map(|thread| thread.0.to_string());

                let _ = bot.send_chat_action(chat_id, ChatAction::Typing).await;

                let bot_typing = bot.clone();
                tokio::spawn(async move {
                    // Spawn a task to keep typing indicator alive
                    let typing_handle = tokio::spawn({
                        let bot = bot_typing.clone();
                        async move {
                            loop {
                                tokio::time::sleep(std::time::Duration::from_secs(4)).await;
                                if bot
                                    .send_chat_action(chat_id, ChatAction::Typing)
                                    .await
                                    .is_err()
                                {
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
                            if let Err(err) = bot.send_message(chat_id, outbound.text).await {
                                tracing::error!("failed to send reply: {err}");
                            }
                        }
                        Err(err) => {
                            tracing::error!("gateway error: {err}");
                            if let Err(send_err) = bot
                                .send_message(chat_id, "Internal error, please try again later.")
                                .await
                            {
                                tracing::error!("failed to send error message: {send_err}");
                            }
                        }
                    }
                });

                Ok::<(), teloxide::RequestError>(())
            }
        });

        Dispatcher::builder(bot, handler)
            .enable_ctrlc_handler()
            .build()
            .dispatch()
            .await;

        Ok(())
    }
}

#[async_trait::async_trait]
impl crate::ChannelBot for TelegramBot {
    fn channel_type(&self) -> &str {
        "telegram"
    }

    fn connector_id(&self) -> &str {
        &self.connector_id
    }

    async fn run(self: Box<Self>) -> anyhow::Result<()> {
        (*self).run_impl().await
    }
}

pub fn detect_mention(msg: &Message) -> (bool, Option<String>) {
    let Some(entities) = msg.entities() else {
        return (false, None);
    };
    let Some(text) = msg.text() else {
        return (false, None);
    };

    for entity in entities {
        if !matches!(&entity.kind, MessageEntityKind::Mention) {
            continue;
        }

        if let Some((start, end)) = utf16_range_to_byte_range(text, entity.offset, entity.length) {
            return (true, Some(text[start..end].to_string()));
        }
    }

    (false, None)
}

/// Spawn a listener for DeliverAnnounce messages (for scheduled task delivery)
async fn spawn_delivery_listener(
    bus: Arc<EventBus>,
    bot_holder: Arc<RwLock<Option<Bot>>>,
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
        if channel_type != "telegram" || msg_connector_id != connector_id {
            continue;
        }

        // Get bot client
        let bot = {
            let holder = bot_holder.read().await;
            holder.clone()
        };

        let Some(bot) = bot else {
            tracing::warn!("Telegram bot not ready for delivery");
            continue;
        };

        // Parse chat ID from conversation_scope (format: "chat:123")
        let Some(chat_id) = parse_chat_id(&conversation_scope) else {
            tracing::warn!(
                "Could not parse chat ID from conversation_scope: {}",
                conversation_scope
            );
            continue;
        };

        let chat = ChatId(chat_id);
        if let Err(e) = bot.send_message(chat, &text).await {
            tracing::error!("Failed to deliver announce message to Telegram: {e}");
        } else {
            tracing::info!(
                "Delivered scheduled task result to Telegram chat {}",
                chat_id
            );
        }
    }
}

/// Spawn a listener for ActionReady messages (reactions, edits, deletes)
async fn spawn_action_listener(
    bus: Arc<EventBus>,
    bot_holder: Arc<RwLock<Option<Bot>>>,
    connector_id: String,
) {
    let mut rx = bus.subscribe(Topic::ActionReady).await;
    while let Some(msg) = rx.recv().await {
        let BusMessage::ActionReady { action } = msg else {
            continue;
        };

        // Only handle actions for this connector
        if action.channel_type != "telegram" || action.connector_id != connector_id {
            continue;
        }

        // Get bot client
        let bot = {
            let holder = bot_holder.read().await;
            holder.clone()
        };

        let Some(bot) = bot else {
            tracing::warn!("Telegram bot not ready for action");
            continue;
        };

        // Parse chat and message IDs
        let Some(chat_id) = parse_chat_id(&action.conversation_scope) else {
            tracing::warn!("Could not parse chat ID: {}", action.conversation_scope);
            continue;
        };
        let Some(message_id) = action.message_id.as_ref().and_then(|id| id.parse::<i32>().ok()) else {
            tracing::warn!("Missing or invalid message_id for action");
            continue;
        };

        let chat = ChatId(chat_id);
        let msg_id = MessageId(message_id);

        match action.action {
            ActionKind::React { ref emoji } => {
                let reaction = ReactionType::Emoji { emoji: emoji.clone() };
                if let Err(e) = bot
                    .set_message_reaction(chat, msg_id)
                    .reaction(vec![reaction])
                    .await
                {
                    tracing::error!("Failed to set reaction: {e}");
                } else {
                    tracing::debug!("Set reaction {emoji} on message {message_id}");
                }
            }
            ActionKind::Unreact { .. } => {
                // Empty reaction list removes all reactions
                if let Err(e) = bot
                    .set_message_reaction(chat, msg_id)
                    .reaction(Vec::<ReactionType>::new())
                    .await
                {
                    tracing::error!("Failed to remove reaction: {e}");
                }
            }
            ActionKind::Edit { ref new_text } => {
                if let Err(e) = bot.edit_message_text(chat, msg_id, new_text).await {
                    tracing::error!("Failed to edit message: {e}");
                }
            }
            ActionKind::Delete => {
                if let Err(e) = bot.delete_message(chat, msg_id).await {
                    tracing::error!("Failed to delete message: {e}");
                }
            }
        }
    }
}

/// Parse chat ID from conversation_scope (format: "chat:123" or "chat:-100123")
fn parse_chat_id(conversation_scope: &str) -> Option<i64> {
    let parts: Vec<&str> = conversation_scope.split(':').collect();
    if parts.len() >= 2 && parts[0] == "chat" {
        // Handle negative IDs (group chats start with -)
        parts[1..].join(":").parse().ok()
    } else {
        None
    }
}

fn utf16_range_to_byte_range(text: &str, offset: usize, length: usize) -> Option<(usize, usize)> {
    let start = utf16_offset_to_byte_idx(text, offset)?;
    let end = utf16_offset_to_byte_idx(text, offset.checked_add(length)?)?;
    Some((start, end))
}

fn utf16_offset_to_byte_idx(text: &str, target: usize) -> Option<usize> {
    if target == 0 {
        return Some(0);
    }

    let mut utf16_units = 0usize;
    for (byte_idx, ch) in text.char_indices() {
        if utf16_units == target {
            return Some(byte_idx);
        }
        utf16_units = utf16_units.checked_add(ch.len_utf16())?;
        if utf16_units == target {
            return Some(byte_idx + ch.len_utf8());
        }
    }

    if utf16_units == target {
        Some(text.len())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_to_inbound_sets_fields() {
        let adapter = TelegramAdapter::new("tg_main");
        let msg = adapter.to_inbound(123, 456, "hello", Some(789));
        assert_eq!(msg.channel_type, "telegram");
        assert_eq!(msg.connector_id, "tg_main");
        assert_eq!(msg.conversation_scope, "chat:123");
        assert_eq!(msg.user_scope, "user:456");
        assert_eq!(msg.text, "hello");
        assert!(!msg.is_mention);
        assert!(msg.thread_id.is_none());
        assert_eq!(msg.message_id, Some("789".to_string()));
    }

    #[test]
    fn adapter_to_inbound_new_fields_defaults() {
        let adapter = TelegramAdapter::new("test");
        let msg = adapter.to_inbound(1, 2, "test", None);
        assert!(!msg.is_mention);
        assert_eq!(msg.mention_target, None);
        assert_eq!(msg.thread_id, None);
        assert_eq!(msg.message_id, None);
    }

    #[test]
    fn render_outbound_formats_correctly() {
        let adapter = TelegramAdapter::new("tg_main");
        let outbound = OutboundMessage {
            trace_id: uuid::Uuid::new_v4(),
            channel_type: "telegram".into(),
            connector_id: "tg_main".into(),
            conversation_scope: "chat:123".into(),
            text: "hello world".into(),
            at: chrono::Utc::now(),
            reply_to: None,
            attachments: vec![],
        };
        let rendered = adapter.render_outbound(&outbound);
        assert_eq!(rendered, "[telegram:chat:123] hello world");
    }

    #[test]
    fn utf16_offset_ascii_basic() {
        let result = utf16_offset_to_byte_idx("hello", 0);
        assert_eq!(result, Some(0));
        let result = utf16_offset_to_byte_idx("hello", 3);
        assert_eq!(result, Some(3));
        let result = utf16_offset_to_byte_idx("hello", 5);
        assert_eq!(result, Some(5));
    }

    #[test]
    fn utf16_offset_with_emoji() {
        let text = "hi 👋 there";
        let byte_idx = utf16_offset_to_byte_idx(text, 3);
        assert_eq!(byte_idx, Some(3));
        let byte_idx = utf16_offset_to_byte_idx(text, 5);
        assert_eq!(byte_idx, Some(7));
    }

    #[test]
    fn utf16_offset_out_of_range() {
        let result = utf16_offset_to_byte_idx("hi", 10);
        assert_eq!(result, None);
    }

    #[test]
    fn utf16_range_to_byte_range_basic() {
        let result = utf16_range_to_byte_range("@bot hello", 0, 4);
        assert_eq!(result, Some((0, 4)));
    }

    #[test]
    fn adapter_to_inbound_negative_chat_id() {
        let adapter = TelegramAdapter::new("tg");
        let msg = adapter.to_inbound(-100123, 456, "group msg", Some(1));
        assert_eq!(msg.conversation_scope, "chat:-100123");
    }
}
