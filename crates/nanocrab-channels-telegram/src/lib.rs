use std::sync::Arc;

use chrono::Utc;
use nanocrab_gateway::Gateway;
use nanocrab_schema::{InboundMessage, OutboundMessage};
use teloxide::prelude::*;
use teloxide::types::{Message, MessageEntityKind};
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

    pub fn to_inbound(&self, chat_id: i64, user_id: i64, text: &str) -> InboundMessage {
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
}

impl TelegramBot {
    pub fn new(token: String, connector_id: String, gateway: Arc<Gateway>) -> Self {
        Self {
            token,
            connector_id,
            gateway,
        }
    }

    pub async fn run(self) -> anyhow::Result<()> {
        let bot = Bot::new(&self.token);
        let adapter = Arc::new(TelegramAdapter::new(&self.connector_id));
        let gateway = self.gateway;

        let handler = Update::filter_message().endpoint(move |bot: Bot, msg: Message| {
            let adapter = adapter.clone();
            let gateway = gateway.clone();

            async move {
                let text = match msg.text() {
                    Some(text) => text.to_string(),
                    None => return Ok::<(), teloxide::RequestError>(()),
                };

                let chat_id = msg.chat.id.0;
                let user_id = msg.from.as_ref().map(|user| user.id.0 as i64).unwrap_or(0);
                let (is_mention, mention_target) = detect_mention(&msg);

                let mut inbound = adapter.to_inbound(chat_id, user_id, &text);
                inbound.is_mention = is_mention;
                inbound.mention_target = mention_target;
                inbound.thread_id = msg.thread_id.map(|thread| thread.0.to_string());

                match gateway.handle_inbound(inbound).await {
                    Ok(outbound) => {
                        bot.send_message(msg.chat.id, outbound.text).await?;
                    }
                    Err(err) => {
                        tracing::error!("gateway error: {err}");
                        bot.send_message(msg.chat.id, "Internal error, please try again later.")
                            .await?;
                    }
                }

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
        let msg = adapter.to_inbound(123, 456, "hello");
        assert_eq!(msg.channel_type, "telegram");
        assert_eq!(msg.connector_id, "tg_main");
        assert_eq!(msg.conversation_scope, "chat:123");
        assert_eq!(msg.user_scope, "user:456");
        assert_eq!(msg.text, "hello");
        assert!(!msg.is_mention);
        assert!(msg.thread_id.is_none());
    }

    #[test]
    fn adapter_to_inbound_new_fields_defaults() {
        let adapter = TelegramAdapter::new("test");
        let msg = adapter.to_inbound(1, 2, "test");
        assert!(!msg.is_mention);
        assert_eq!(msg.mention_target, None);
        assert_eq!(msg.thread_id, None);
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
        let msg = adapter.to_inbound(-100123, 456, "group msg");
        assert_eq!(msg.conversation_scope, "chat:-100123");
    }
}
