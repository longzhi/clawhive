use std::sync::Arc;

use chrono::Utc;
use clawhive_bus::{EventBus, Topic};
use clawhive_gateway::Gateway;
use clawhive_schema::BusMessage;
use clawhive_schema::{ActionKind, Attachment, AttachmentKind, InboundMessage, OutboundMessage};
use teloxide::net::Download;
use teloxide::prelude::*;
use teloxide::types::{
    BotCommand, ChatAction, Message, MessageEntityKind, MessageId, ParseMode, ReactionType,
};
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

    pub fn to_inbound(
        &self,
        chat_id: i64,
        user_id: i64,
        text: &str,
        message_id: Option<i32>,
    ) -> InboundMessage {
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

        // Register bot commands menu with Telegram
        let commands = vec![
            BotCommand::new("new", "Start a fresh session"),
            BotCommand::new("status", "Show session status"),
            BotCommand::new("model", "Show current model info"),
            BotCommand::new("help", "Show available commands"),
        ];
        if let Err(e) = bot.set_my_commands(commands).await {
            tracing::warn!("Failed to register Telegram bot commands: {e}");
        }

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
                let has_photo = msg.photo().is_some();
                let text = msg
                    .text()
                    .or_else(|| msg.caption())
                    .unwrap_or("")
                    .to_string();

                // Skip messages with no text and no photo
                if text.is_empty() && !has_photo {
                    return Ok::<(), teloxide::RequestError>(());
                }

                let chat_id = msg.chat.id;
                let user_id = msg.from.as_ref().map(|user| user.id.0 as i64).unwrap_or(0);
                let (is_mention, mention_target) = detect_mention(&msg);
                let message_id = msg.id.0;

                let mut inbound = adapter.to_inbound(chat_id.0, user_id, &text, Some(message_id));
                inbound.is_mention = is_mention;
                inbound.mention_target = mention_target;
                inbound.thread_id = msg.thread_id.map(|thread| thread.0.to_string());

                // Download photo if present
                if let Some(photos) = msg.photo() {
                    // Pick the largest photo (last in array)
                    if let Some(photo) = photos.last() {
                        match download_photo(&bot, &photo.file.id).await {
                            Ok((base64_data, mime)) => {
                                inbound.attachments.push(Attachment {
                                    kind: AttachmentKind::Image,
                                    url: base64_data,
                                    mime_type: Some(mime),
                                    file_name: None,
                                    size: Some(photo.file.size as u64),
                                });
                            }
                            Err(e) => {
                                tracing::warn!("Failed to download photo: {e}");
                            }
                        }
                    }
                }

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
                            if outbound.text.is_empty() {
                                tracing::warn!("outbound text is empty, skipping send");
                            } else {
                                let html = md_to_telegram_html(&outbound.text);
                                if let Err(err) = send_long_html(&bot, chat_id, &html).await {
                                    tracing::error!("failed to send reply: {err}");
                                }
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
        let html = md_to_telegram_html(&text);
        if let Err(e) = send_long_html(&bot, chat, &html).await {
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
        let Some(message_id) = action
            .message_id
            .as_ref()
            .and_then(|id| id.parse::<i32>().ok())
        else {
            tracing::warn!("Missing or invalid message_id for action");
            continue;
        };

        let chat = ChatId(chat_id);
        let msg_id = MessageId(message_id);

        match action.action {
            ActionKind::React { ref emoji } => {
                let reaction = ReactionType::Emoji {
                    emoji: emoji.clone(),
                };
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
                let html = md_to_telegram_html(new_text);
                if let Err(e) = bot
                    .edit_message_text(chat, msg_id, &html)
                    .parse_mode(ParseMode::Html)
                    .await
                {
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

/// Download a Telegram photo by file_id, returning (base64_data, mime_type).
async fn download_photo(bot: &Bot, file_id: &str) -> anyhow::Result<(String, String)> {
    use base64::Engine;

    let file = bot.get_file(file_id).await?;
    let file_path = &file.path;

    let mut buf = Vec::new();
    bot.download_file(file_path, &mut buf).await?;

    let mime = if file_path.ends_with(".png") {
        "image/png"
    } else if file_path.ends_with(".gif") {
        "image/gif"
    } else if file_path.ends_with(".webp") {
        "image/webp"
    } else {
        "image/jpeg"
    };

    let base64_data = base64::engine::general_purpose::STANDARD.encode(&buf);
    Ok((base64_data, mime.to_string()))
}

/// Maximum length for a single Telegram message.
const TELEGRAM_MAX_LEN: usize = 4096;

/// Convert standard Markdown to Telegram-supported HTML subset.
///
/// Telegram HTML supports: `<b>`, `<i>`, `<code>`, `<pre>`, `<s>`, `<u>`, `<a>`.
/// We convert the most common Markdown patterns LLMs produce.
fn md_to_telegram_html(md: &str) -> String {
    // Step 1: Escape HTML entities in the raw markdown first.
    // We do this on a per-segment basis to avoid double-escaping inside code blocks.
    let mut result = String::with_capacity(md.len());
    let mut chars: &str = md;

    // Process fenced code blocks first — they should not have inline formatting applied.
    // We'll split on ``` boundaries.
    let mut segments: Vec<(String, bool)> = Vec::new(); // (text, is_code_block)
    loop {
        if let Some(start) = chars.find("```") {
            // Text before the code fence
            let before = &chars[..start];
            if !before.is_empty() {
                segments.push((before.to_string(), false));
            }
            let after_opening = &chars[start + 3..];
            // Find the closing ```
            if let Some(end) = after_opening.find("```") {
                let block_content = &after_opening[..end];
                segments.push((block_content.to_string(), true));
                chars = &after_opening[end + 3..];
            } else {
                // No closing fence — treat rest as code block
                segments.push((after_opening.to_string(), true));
                break;
            }
        } else {
            if !chars.is_empty() {
                segments.push((chars.to_string(), false));
            }
            break;
        }
    }

    for (segment, is_code_block) in &segments {
        if *is_code_block {
            // Extract optional language hint from first line
            let (lang, code) = if let Some(newline_pos) = segment.find('\n') {
                let first_line = segment[..newline_pos].trim();
                if !first_line.is_empty()
                    && first_line.chars().all(|c| {
                        c.is_alphanumeric() || c == '-' || c == '_' || c == '+' || c == '#'
                    })
                {
                    (Some(first_line), &segment[newline_pos + 1..])
                } else {
                    // No language hint — strip leading newline
                    (None, &segment[newline_pos + 1..])
                }
            } else {
                (None, segment.as_str())
            };
            let escaped_code = escape_html(code);
            // Trim trailing newline inside <pre> for cleaner display
            let trimmed = escaped_code.trim_end_matches('\n');
            if let Some(lang) = lang {
                result.push_str(&format!(
                    "<pre><code class=\"language-{lang}\">{trimmed}</code></pre>"
                ));
            } else {
                result.push_str(&format!("<pre><code>{trimmed}</code></pre>"));
            }
        } else {
            let escaped = escape_html(segment);
            let formatted = apply_inline_formatting(&escaped);
            result.push_str(&formatted);
        }
    }

    result
}

/// Escape `<`, `>`, `&` for Telegram HTML.
fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Apply inline Markdown formatting to already-HTML-escaped text.
fn apply_inline_formatting(text: &str) -> String {
    let mut result = String::with_capacity(text.len());

    let lines: Vec<&str> = text.split('\n').collect();
    for (i, line) in lines.iter().enumerate() {
        // Convert unordered list markers at line start: "- " or "* " → "• "
        let line = if let Some(rest) = line.strip_prefix("- ") {
            format!("• {rest}")
        } else if let Some(rest) = line.strip_prefix("* ") {
            format!("• {rest}")
        } else {
            line.to_string()
        };

        // Apply inline formatting using a char-by-char parser
        result.push_str(&apply_inline_spans(&line));

        if i < lines.len() - 1 {
            result.push('\n');
        }
    }
    result
}

/// Parse inline spans: **bold**, *italic*, `code`, ~~strikethrough~~.
/// Operates on HTML-escaped text (so `<` is already `&lt;` etc.).
fn apply_inline_spans(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;

    while !rest.is_empty() {
        // Inline code: `...`
        if let Some(after) = rest.strip_prefix('`') {
            if let Some(end) = after.find('`') {
                out.push_str("<code>");
                out.push_str(&after[..end]);
                out.push_str("</code>");
                rest = &after[end + 1..];
                continue;
            }
        }

        // Strikethrough: ~~...~~
        if let Some(after) = rest.strip_prefix("~~") {
            if let Some(end) = after.find("~~") {
                out.push_str("<s>");
                out.push_str(&after[..end]);
                out.push_str("</s>");
                rest = &after[end + 2..];
                continue;
            }
        }

        // Bold: **...**
        if let Some(after) = rest.strip_prefix("**") {
            if let Some(end) = after.find("**") {
                out.push_str("<b>");
                out.push_str(&after[..end]);
                out.push_str("</b>");
                rest = &after[end + 2..];
                continue;
            }
        }

        // Italic: *...* (single asterisk, not **)
        if let Some(after) = rest.strip_prefix('*') {
            if !after.starts_with('*') {
                if let Some(end) = find_closing_italic(after) {
                    let inner = &after[..end];
                    if !inner.is_empty() {
                        out.push_str("<i>");
                        out.push_str(inner);
                        out.push_str("</i>");
                        rest = &after[end + 1..];
                        continue;
                    }
                }
            }
        }

        // Consume one character
        let ch = rest.chars().next().unwrap();
        out.push(ch);
        rest = &rest[ch.len_utf8()..];
    }

    out
}

/// Find closing `*` for italic that is not preceded by a space and not `**`.
fn find_closing_italic(text: &str) -> Option<usize> {
    let mut prev_space = false;
    for (i, ch) in text.char_indices() {
        if ch == '*' {
            // Check next char is not also * (that would be **)
            let next_is_star = text[i + 1..].starts_with('*');
            if !next_is_star && !prev_space {
                return Some(i);
            }
        }
        prev_space = ch == ' ';
    }
    None
}

/// Send a potentially long HTML message, splitting at safe boundaries if needed.
async fn send_long_html(
    bot: &Bot,
    chat_id: ChatId,
    html: &str,
) -> Result<(), teloxide::RequestError> {
    if html.len() <= TELEGRAM_MAX_LEN {
        bot.send_message(chat_id, html)
            .parse_mode(ParseMode::Html)
            .await?;
        return Ok(());
    }

    // Split into chunks at newline boundaries
    let mut remaining = html;
    while !remaining.is_empty() {
        if remaining.len() <= TELEGRAM_MAX_LEN {
            bot.send_message(chat_id, remaining)
                .parse_mode(ParseMode::Html)
                .await?;
            break;
        }

        // Find a newline boundary to split at
        let split_at = remaining[..TELEGRAM_MAX_LEN]
            .rfind('\n')
            .unwrap_or(TELEGRAM_MAX_LEN);
        let (chunk, rest) = remaining.split_at(split_at);
        // Skip the newline itself if we split at one
        let rest = rest.strip_prefix('\n').unwrap_or(rest);

        bot.send_message(chat_id, chunk)
            .parse_mode(ParseMode::Html)
            .await?;
        remaining = rest;
    }

    Ok(())
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

    #[test]
    fn md_html_escapes_entities() {
        assert_eq!(
            md_to_telegram_html("a < b & c > d"),
            "a &lt; b &amp; c &gt; d"
        );
    }

    #[test]
    fn md_html_bold() {
        assert_eq!(md_to_telegram_html("**hello**"), "<b>hello</b>");
    }

    #[test]
    fn md_html_italic() {
        assert_eq!(md_to_telegram_html("*hello*"), "<i>hello</i>");
    }

    #[test]
    fn md_html_inline_code() {
        assert_eq!(md_to_telegram_html("`code here`"), "<code>code here</code>");
    }

    #[test]
    fn md_html_strikethrough() {
        assert_eq!(md_to_telegram_html("~~deleted~~"), "<s>deleted</s>");
    }

    #[test]
    fn md_html_code_block_with_lang() {
        let input = "```rust\nfn main() {}\n```";
        let expected = "<pre><code class=\"language-rust\">fn main() {}</code></pre>";
        assert_eq!(md_to_telegram_html(input), expected);
    }

    #[test]
    fn md_html_code_block_no_lang() {
        let input = "```\nhello world\n```";
        let expected = "<pre><code>hello world</code></pre>";
        assert_eq!(md_to_telegram_html(input), expected);
    }

    #[test]
    fn md_html_code_block_escapes_html() {
        let input = "```\n<div>&</div>\n```";
        let expected = "<pre><code>&lt;div&gt;&amp;&lt;/div&gt;</code></pre>";
        assert_eq!(md_to_telegram_html(input), expected);
    }

    #[test]
    fn md_html_list_bullets() {
        let input = "- item one\n- item two";
        let expected = "• item one\n• item two";
        assert_eq!(md_to_telegram_html(input), expected);
    }

    #[test]
    fn md_html_star_list_bullets() {
        let input = "* item one\n* item two";
        let expected = "• item one\n• item two";
        assert_eq!(md_to_telegram_html(input), expected);
    }

    #[test]
    fn md_html_mixed_formatting() {
        let input = "**bold** and *italic* and `code`";
        let expected = "<b>bold</b> and <i>italic</i> and <code>code</code>";
        assert_eq!(md_to_telegram_html(input), expected);
    }

    #[test]
    fn md_html_plain_text_unchanged() {
        assert_eq!(md_to_telegram_html("hello world"), "hello world");
    }

    #[test]
    fn md_html_nested_bold_in_text() {
        let input = "this is **very important** info";
        let expected = "this is <b>very important</b> info";
        assert_eq!(md_to_telegram_html(input), expected);
    }
}
