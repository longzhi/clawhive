use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use clawhive_bus::{EventBus, Topic};
use clawhive_gateway::Gateway;
use clawhive_schema::{ActionKind, Attachment, AttachmentKind, InboundMessage, OutboundMessage};
use clawhive_schema::{ApprovalDisplay, BusMessage};
use teloxide::net::Download;
use teloxide::prelude::*;
use teloxide::types::{
    BotCommand, CallbackQuery, ChatAction, InlineKeyboardButton, InlineKeyboardMarkup, InputFile,
    Message, MessageEntityKind, MessageId, ParseMode, ReactionType,
};
use tokio::sync::{watch, RwLock};
use uuid::Uuid;

use crate::common::{infer_mime_from_filename, AbortOnDrop, PROGRESS_MESSAGE};

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
            message_source: None,
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
    require_mention: bool,
    allow_from: Vec<i64>,
    dm_policy: String,
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
            require_mention: true,
            allow_from: vec![],
            dm_policy: "allowlist".to_string(),
        }
    }

    pub fn with_require_mention(mut self, require: bool) -> Self {
        self.require_mention = require;
        self
    }

    pub fn with_allow_from(mut self, allow_from: Vec<String>) -> Self {
        self.allow_from = allow_from
            .iter()
            .filter_map(|s| s.parse::<i64>().ok())
            .collect();
        self
    }

    pub fn with_dm_policy(mut self, dm_policy: String) -> Self {
        self.dm_policy = dm_policy;
        self
    }

    pub async fn run_impl(self) -> anyhow::Result<()> {
        let bot = Bot::new(&self.token);

        // Register bot commands menu with Telegram
        let commands = vec![
            BotCommand::new("new", "Start a fresh session"),
            BotCommand::new("stop", "Cancel the current task"),
            BotCommand::new("status", "Show session status"),
            BotCommand::new("model", "Show or change model (e.g. /model openai/gpt-5.2)"),
            BotCommand::new("help", "Show available commands"),
            BotCommand::new("skill_analyze", "Analyze a skill before installing"),
            BotCommand::new("skill_install", "Install a skill (analyze first)"),
            BotCommand::new("skill_confirm", "Confirm a pending skill installation"),
        ];
        if let Err(e) = bot.set_my_commands(commands).await {
            tracing::warn!("Failed to register Telegram bot commands: {e}");
        }

        let adapter = Arc::new(TelegramAdapter::new(&self.connector_id));
        let gateway = self.gateway;
        let bus = self.bus;
        let connector_id = self.connector_id.clone();
        let require_mention = self.require_mention;
        let allow_from: Arc<Vec<i64>> = Arc::new(self.allow_from);

        // Create a bot holder for the delivery listener
        let bot_holder: Arc<RwLock<Option<Bot>>> = Arc::new(RwLock::new(Some(bot.clone())));

        // Spawn delivery listener for scheduled task announcements
        let bot_holder_clone = bot_holder.clone();
        let connector_id_clone = connector_id.clone();
        let bus_delivery = bus.clone();
        let bus_action = bus.clone();
        let bus_approval = bus.clone();
        tokio::spawn(spawn_delivery_listener(
            bus_delivery,
            bot_holder_clone.clone(),
            connector_id_clone.clone(),
        ));
        tokio::spawn(spawn_action_listener(
            bus_action,
            bot_holder_clone.clone(),
            connector_id_clone.clone(),
        ));
        tokio::spawn(spawn_approval_listener(
            bus_approval,
            bot_holder_clone.clone(),
            connector_id_clone.clone(),
        ));
        let bus_skill_confirm = bus.clone();
        tokio::spawn(spawn_skill_confirm_listener(
            bus_skill_confirm,
            bot_holder_clone,
            connector_id_clone,
        ));
        let gateway_for_callback = gateway.clone();
        let adapter_for_callback = adapter.clone();
        let connector_id_for_callback = self.connector_id.clone();

        let allow_from_for_msg = allow_from.clone();
        let dm_policy_for_msg = self.dm_policy.clone();
        let connector_id_for_msg = connector_id.clone();
        let message_handler = Update::filter_message().endpoint(move |bot: Bot, msg: Message| {
            let adapter = adapter.clone();
            let gateway = gateway.clone();
            let allow_from = allow_from_for_msg.clone();
            let dm_policy = dm_policy_for_msg.clone();
            let connector_id = connector_id_for_msg.clone();

            async move {
                // DM access control
                let user_id = msg.from.as_ref().map(|user| user.id.0 as i64).unwrap_or(0);
                let is_dm = !msg.chat.is_group() && !msg.chat.is_supergroup();
                if is_dm {
                    let allowed = match dm_policy.as_str() {
                        "open" => true,
                        "disabled" => false,
                        _ => {
                            // "allowlist" (default): check allow_from
                            if allow_from.is_empty() {
                                false
                            } else {
                                allow_from.contains(&user_id)
                            }
                        }
                    };
                    if !allowed {
                        if dm_policy != "open" && allow_from.is_empty() {
                            tracing::warn!(
                                connector_id = %connector_id,
                                "telegram DM rejected: dm_policy is '{}' but allow_from is empty — configure allow_from or set dm_policy to 'open'",
                                dm_policy
                            );
                        } else {
                            tracing::warn!(
                                user_id,
                                chat_id = msg.chat.id.0,
                                "telegram DM rejected: user not in allow_from list"
                            );
                        }
                        return Ok::<(), teloxide::RequestError>(());
                    }
                }

                let has_photo = msg.photo().is_some();
                let has_document = msg.document().is_some();
                let has_voice = msg.voice().is_some();
                let has_audio = msg.audio().is_some();
                let has_media = has_photo || has_document || has_voice || has_audio;
                let mut text = msg
                    .text()
                    .or_else(|| msg.caption())
                    .unwrap_or("")
                    .to_string();

                let quoted_text = msg
                    .reply_to_message()
                    .and_then(|quoted| quoted.text().or_else(|| quoted.caption()))
                    .map(|s| s.to_string());

                text = compose_inbound_text(&text, quoted_text.as_deref());

                // Normalize Telegram-style underscore commands to space format
                text = text
                    .replacen("/skill_analyze", "/skill analyze", 1)
                    .replacen("/skill_install", "/skill install", 1)
                    .replacen("/skill_confirm", "/skill confirm", 1);

                // Skip messages with no text and no media
                if text.is_empty() && !has_media {
                    return Ok::<(), teloxide::RequestError>(());
                }

                let chat_id = msg.chat.id;
                let (is_mention, mention_target) = detect_mention(&msg);
                let message_id = msg.id.0;

                // Group chat filtering: skip non-mention messages when require_mention is true
                if chat_id.0 < 0 && require_mention && !is_mention {
                    tracing::info!(
                        chat_id = chat_id.0,
                        user_id,
                        message_id,
                        require_mention,
                        "telegram inbound skipped: group message without mention"
                    );
                    return Ok::<(), teloxide::RequestError>(());
                }

                let mut inbound = adapter.to_inbound(chat_id.0, user_id, &text, Some(message_id));
                inbound.is_mention = is_mention;
                inbound.mention_target = mention_target;
                inbound.thread_id = msg.thread_id.map(|thread| thread.0.to_string());
                let trace_id = inbound.trace_id;

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

                if let Some(doc) = msg.document() {
                    let mime = doc
                        .mime_type
                        .as_ref()
                        .map(|m| m.to_string())
                        .or_else(|| infer_mime_from_filename(doc.file_name.as_deref()))
                        .unwrap_or_else(|| "application/octet-stream".to_string());

                    let kind = if mime.starts_with("image/") {
                        AttachmentKind::Image
                    } else {
                        AttachmentKind::Document
                    };

                    match download_file_as_base64(&bot, &doc.file.id).await {
                        Ok(buf) => {
                            use base64::Engine;
                            let base64_data =
                                base64::engine::general_purpose::STANDARD.encode(&buf);
                            inbound.attachments.push(Attachment {
                                kind,
                                url: base64_data,
                                mime_type: Some(mime),
                                file_name: doc.file_name.clone(),
                                size: Some(doc.file.size as u64),
                            });
                        }
                        Err(e) => {
                            tracing::warn!(
                                file_name = ?doc.file_name,
                                error = %e,
                                "failed to download telegram document"
                            );
                        }
                    }
                }

                if let Some(voice) = msg.voice() {
                    let mime = voice
                        .mime_type
                        .as_ref()
                        .map(|m| m.to_string())
                        .unwrap_or_else(|| "audio/ogg".to_string());

                    match download_file_as_base64(&bot, &voice.file.id).await {
                        Ok(buf) => {
                            use base64::Engine;
                            let base64_data =
                                base64::engine::general_purpose::STANDARD.encode(&buf);
                            inbound.attachments.push(Attachment {
                                kind: AttachmentKind::Audio,
                                url: base64_data,
                                mime_type: Some(mime),
                                file_name: None,
                                size: Some(voice.file.size as u64),
                            });
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "failed to download telegram voice");
                        }
                    }
                }

                if let Some(audio) = msg.audio() {
                    let mime = audio
                        .mime_type
                        .as_ref()
                        .map(|m| m.to_string())
                        .unwrap_or_else(|| "audio/mpeg".to_string());

                    match download_file_as_base64(&bot, &audio.file.id).await {
                        Ok(buf) => {
                            use base64::Engine;
                            let base64_data =
                                base64::engine::general_purpose::STANDARD.encode(&buf);
                            inbound.attachments.push(Attachment {
                                kind: AttachmentKind::Audio,
                                url: base64_data,
                                mime_type: Some(mime),
                                file_name: audio.file_name.clone(),
                                size: Some(audio.file.size as u64),
                            });
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "failed to download telegram audio");
                        }
                    }
                }

                let _ = bot.send_chat_action(chat_id, ChatAction::Typing).await;
                let lifecycle = gateway.resolve_turn_lifecycle(&inbound);
                let typing_ttl = lifecycle.typing_ttl_secs;
                let progress_delay = lifecycle.progress_delay_secs;

                let bot_typing = bot.clone();
                tokio::spawn(async move {
                    let (turn_done_tx, turn_done_rx) = watch::channel(false);
                    let main_handle = tokio::spawn({
                        let turn_done_tx = turn_done_tx.clone();
                        async move {
                            let result = gateway.handle_inbound(inbound).await;
                            let _ = turn_done_tx.send(true);
                            result
                        }
                    });
                    let _typing_guard = AbortOnDrop(tokio::spawn({
                        let bot = bot_typing.clone();
                        let mut turn_done_rx = turn_done_rx.clone();
                        async move {
                            let deadline = tokio::time::Instant::now()
                                + std::time::Duration::from_secs(typing_ttl);
                            loop {
                                if tokio::time::Instant::now() >= deadline {
                                    break;
                                }
                                let wait = std::time::Duration::from_secs(4).min(
                                    deadline.saturating_duration_since(tokio::time::Instant::now()),
                                );
                                tokio::select! {
                                    _ = tokio::time::sleep(wait) => {
                                        if bot
                                            .send_chat_action(chat_id, ChatAction::Typing)
                                            .await
                                            .is_err()
                                        {
                                            break;
                                        }
                                    }
                                    changed = turn_done_rx.changed() => {
                                        if changed.is_err() || *turn_done_rx.borrow() {
                                            break;
                                        }
                                    }
                                }
                            }
                        }
                    }));
                    let _progress_handle = (progress_delay > 0).then(|| {
                        tokio::spawn({
                            let bot = bot.clone();
                            let mut turn_done_rx = turn_done_rx.clone();
                            async move {
                                tokio::select! {
                                    _ = tokio::time::sleep(std::time::Duration::from_secs(progress_delay)) => {
                                        if !*turn_done_rx.borrow() {
                                            if let Err(send_err) = bot.send_message(chat_id, PROGRESS_MESSAGE).await {
                                                tracing::warn!(
                                                    trace_id = %trace_id,
                                                    chat_id = chat_id.0,
                                                    user_id,
                                                    message_id,
                                                    error = %send_err,
                                                    "failed to send telegram progress message"
                                                );
                                            }
                                        }
                                    }
                                    changed = turn_done_rx.changed() => {
                                        let _ = changed;
                                    }
                                }
                            }
                        })
                    });

                    match main_handle.await {
                        Ok(result) => match result {
                        Ok(Some(outbound)) => {
                            match attachment_text_mode(
                                &outbound.text,
                                !outbound.attachments.is_empty(),
                            ) {
                                AttachmentTextMode::Empty => {
                                    tracing::warn!(
                                        trace_id = %trace_id,
                                        chat_id = chat_id.0,
                                        user_id,
                                        message_id,
                                        "telegram outbound text is empty"
                                    );

                                    if let Some(fallback_text) =
                                        empty_outbound_fallback_text(chat_id.0)
                                    {
                                        if let Err(send_err) =
                                            bot.send_message(chat_id, fallback_text).await
                                        {
                                            tracing::error!(
                                                trace_id = %trace_id,
                                                chat_id = chat_id.0,
                                                user_id,
                                                message_id,
                                                error = %send_err,
                                                "failed to send telegram empty-outbound fallback"
                                            );
                                        }
                                    } else {
                                        tracing::info!(
                                            trace_id = %trace_id,
                                            chat_id = chat_id.0,
                                            user_id,
                                            message_id,
                                            "telegram empty outbound suppressed for non-DM chat"
                                        );
                                    }
                                }
                                AttachmentTextMode::TextOnly => {
                                    let html = md_to_telegram_html(&outbound.text);
                                    if let Err(err) = send_long_html(&bot, chat_id, &html).await {
                                        tracing::error!(
                                            trace_id = %trace_id,
                                            chat_id = chat_id.0,
                                            user_id,
                                            message_id,
                                            error = %err,
                                            "failed to send telegram reply"
                                        );
                                    }
                                }
                                AttachmentTextMode::CaptionFirstAttachment => {
                                    send_attachments(
                                        &bot,
                                        chat_id,
                                        &outbound.attachments,
                                        Some(outbound.text.as_str()),
                                    )
                                    .await;
                                }
                                AttachmentTextMode::TextThenAttachments => {
                                    let html = md_to_telegram_html(&outbound.text);
                                    if let Err(err) = send_long_html(&bot, chat_id, &html).await {
                                        tracing::error!(
                                            trace_id = %trace_id,
                                            chat_id = chat_id.0,
                                            user_id,
                                            message_id,
                                            error = %err,
                                            "failed to send telegram reply"
                                        );
                                    }
                                    send_attachments(&bot, chat_id, &outbound.attachments, None)
                                        .await;
                                }
                                AttachmentTextMode::AttachmentsOnly => {
                                    send_attachments(&bot, chat_id, &outbound.attachments, None)
                                        .await;
                                }
                            }
                        }
                        Ok(None) => {}
                        Err(err) => {
                            tracing::error!(
                                trace_id = %trace_id,
                                chat_id = chat_id.0,
                                user_id,
                                message_id,
                                error = %err,
                                "telegram gateway error"
                            );
                            let user_msg = format!("Error: {err}");
                            if let Err(send_err) = bot.send_message(chat_id, &user_msg).await {
                                tracing::error!(
                                    trace_id = %trace_id,
                                    chat_id = chat_id.0,
                                    user_id,
                                    message_id,
                                    error = %send_err,
                                    "failed to send telegram error message"
                                );
                            }
                        }
                        },
                        Err(err) => {
                            tracing::error!(
                                trace_id = %trace_id,
                                chat_id = chat_id.0,
                                user_id,
                                message_id,
                                error = %err,
                                "telegram inbound task join error"
                            );
                        }
                    }
                });

                Ok::<(), teloxide::RequestError>(())
            }
        });

        let allow_from_for_cb = allow_from.clone();
        let callback_handler =
            Update::filter_callback_query().endpoint(move |bot: Bot, q: CallbackQuery| {
                let gateway = gateway_for_callback.clone();
                let adapter = adapter_for_callback.clone();
                let connector_id = connector_id_for_callback.clone();
                let allow_from = allow_from_for_cb.clone();

                tracing::info!(callback_data = ?q.data, from = q.from.id.0, "telegram callback_query received");
                async move {
                    // User allowlist filtering
                    let cb_user_id = q.from.id.0 as i64;
                    if !allow_from.is_empty() && !allow_from.contains(&cb_user_id) {
                        tracing::warn!(
                            user_id = cb_user_id,
                            "telegram callback rejected: user not in allow_from list"
                        );
                        return Ok::<(), teloxide::RequestError>(());
                    }

                    let Some(data) = q.data else {
                        return Ok::<(), teloxide::RequestError>(());
                    };

                    // Skill confirm/cancel buttons
                    if let Some(token) = data.strip_prefix("skill_confirm:") {
                        let (chat_id, msg_id) = match &q.message {
                            Some(msg) => (msg.chat().id, msg.id()),
                            None => {
                                let _ = bot
                                    .answer_callback_query(&q.id)
                                    .text("\u{274c} Message expired")
                                    .await;
                                return Ok::<(), teloxide::RequestError>(());
                            }
                        };

                        // Answer callback immediately — skill confirm may block on approval
                        let _ = bot
                            .answer_callback_query(&q.id)
                            .text("\u{23f3} Processing installation...")
                            .await;

                        // Remove confirm/cancel buttons right away
                        let _ = bot
                            .edit_message_reply_markup(chat_id, msg_id)
                            .reply_markup(InlineKeyboardMarkup::new(
                                Vec::<Vec<InlineKeyboardButton>>::new(),
                            ))
                            .await;

                        // Process in background — handle_inbound blocks waiting for approval
                        let user_id = q.from.id.0 as i64;
                        let text = format!("/skill confirm {token}");
                        let inbound = adapter.to_inbound(chat_id.0, user_id, &text, None);
                        let inbound = InboundMessage {
                            connector_id: connector_id.clone(),
                            channel_type: "telegram".to_string(),
                            ..inbound
                        };
                        tokio::spawn(async move {
                            let reply_text = match gateway.handle_inbound(inbound).await {
                                Ok(Some(outbound)) => outbound.text,
                                Ok(None) => String::new(),
                                Err(e) => format!("\u{274c} Error: {e}"),
                            };
                            let _ = bot.send_message(chat_id, &reply_text).await;
                        });
                        return Ok::<(), teloxide::RequestError>(());
                    }
                    if data.starts_with("skill_cancel:") {
                        if let Some(msg) = &q.message {
                            let _ = bot
                                .edit_message_reply_markup(msg.chat().id, msg.id())
                                .reply_markup(InlineKeyboardMarkup::new(Vec::<
                                    Vec<InlineKeyboardButton>,
                                >::new(
                                )))
                                .await;
                        }
                        let _ = bot
                            .answer_callback_query(&q.id)
                            .text("Installation cancelled.")
                            .await;
                        return Ok::<(), teloxide::RequestError>(());
                    }

                    // Approval buttons
                    let Some(rest) = data.strip_prefix("approve:") else {
                        return Ok::<(), teloxide::RequestError>(());
                    };

                    let parts: Vec<&str> = rest.splitn(2, ':').collect();
                    if parts.len() != 2 {
                        return Ok::<(), teloxide::RequestError>(());
                    }

                    let short_id = parts[0];
                    let decision = parts[1];

                    // Extract chat_id from the callback's message
                    let (chat_id, msg_id) = match &q.message {
                        Some(msg) => (msg.chat().id, msg.id()),
                        None => {
                            let _ = bot
                                .answer_callback_query(&q.id)
                                .text("\u{274c} Message expired")
                                .await;
                            return Ok::<(), teloxide::RequestError>(());
                        }
                    };

                    // Answer callback immediately to dismiss loading indicator
                    let _ = bot
                        .answer_callback_query(&q.id)
                        .text("\u{23f3} Processing...")
                        .await;

                    // Remove inline keyboard from the approval message
                    let _ = bot
                        .edit_message_reply_markup(chat_id, msg_id)
                        .reply_markup(InlineKeyboardMarkup::new(
                            Vec::<Vec<InlineKeyboardButton>>::new(),
                        ))
                        .await;

                    let user_id = q.from.id.0 as i64;
                    let text = format!("/approve {short_id} {decision}");
                    let inbound = adapter.to_inbound(chat_id.0, user_id, &text, None);

                    // Construct synthetic inbound with proper connector_id
                    let mut inbound = InboundMessage {
                        connector_id: connector_id.clone(),
                        ..inbound
                    };
                    inbound.channel_type = "telegram".to_string();

                    // Process in background
                    tokio::spawn(async move {
                        let reply_text = match gateway.handle_inbound(inbound).await {
                            Ok(Some(outbound)) => outbound.text,
                            Ok(None) => String::new(),
                            Err(e) => format!("\u{274c} Error: {e}"),
                        };
                        let _ = bot.send_message(chat_id, &reply_text).await;
                    });

                    Ok::<(), teloxide::RequestError>(())
                }
            });

        let handler = dptree::entry()
            .branch(message_handler)
            .branch(callback_handler);

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
    // Check text entities first, then caption entities (for media messages)
    let (entities, text) = match (msg.entities(), msg.text()) {
        (Some(e), Some(t)) => (e, t),
        _ => match (msg.caption_entities(), msg.caption()) {
            (Some(e), Some(t)) => (e, t),
            _ => return (false, None),
        },
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

/// Spawn a listener for DeliverApprovalRequest messages — sends inline keyboard buttons
async fn spawn_approval_listener(
    bus: Arc<EventBus>,
    bot_holder: Arc<RwLock<Option<Bot>>>,
    connector_id: String,
) {
    let mut rx = bus.subscribe(Topic::DeliverApprovalRequest).await;
    while let Some(msg) = rx.recv().await {
        let BusMessage::DeliverApprovalRequest {
            channel_type,
            connector_id: msg_connector_id,
            conversation_scope,
            short_id,
            agent_id,
            command,
            network_target,
            summary,
        } = msg
        else {
            continue;
        };

        if channel_type != "telegram" || msg_connector_id != connector_id {
            continue;
        }

        let bot = {
            let holder = bot_holder.read().await;
            holder.clone()
        };

        let Some(bot) = bot else {
            tracing::warn!("Telegram bot not ready for approval delivery");
            continue;
        };

        let Some(chat_id) = parse_chat_id(&conversation_scope) else {
            tracing::warn!(
                "Could not parse chat ID from conversation_scope: {}",
                conversation_scope
            );
            continue;
        };

        let display = ApprovalDisplay::new(&agent_id, &command, network_target.as_deref(), summary);
        let text = display.to_html();

        let keyboard = InlineKeyboardMarkup::new(vec![vec![
            InlineKeyboardButton::callback("✅ Allow Once", format!("approve:{short_id}:allow")),
            InlineKeyboardButton::callback("🔓 Always Allow", format!("approve:{short_id}:always")),
            InlineKeyboardButton::callback("❌ Deny", format!("approve:{short_id}:deny")),
        ]]);

        let chat = ChatId(chat_id);
        match tokio::time::timeout(
            Duration::from_secs(30),
            bot.send_message(chat, &text)
                .parse_mode(ParseMode::Html)
                .reply_markup(keyboard)
                .send(),
        )
        .await
        {
            Ok(Err(e)) => {
                tracing::error!("Failed to send approval keyboard to Telegram: {e}");
            }
            Err(_) => {
                tracing::warn!(
                    chat_id,
                    "telegram approval message delivery timed out after 30s"
                );
            }
            Ok(Ok(_)) => {}
        }
    }
}

/// Spawn a listener for DeliverSkillConfirm messages — sends inline keyboard buttons
async fn spawn_skill_confirm_listener(
    bus: Arc<EventBus>,
    bot_holder: Arc<RwLock<Option<Bot>>>,
    connector_id: String,
) {
    let mut rx = bus.subscribe(Topic::DeliverSkillConfirm).await;
    while let Some(msg) = rx.recv().await {
        let BusMessage::DeliverSkillConfirm {
            channel_type,
            connector_id: msg_connector_id,
            conversation_scope,
            token,
            skill_name,
            analysis_text: _,
        } = msg
        else {
            continue;
        };

        if channel_type != "telegram" || msg_connector_id != connector_id {
            continue;
        }

        let bot = {
            let holder = bot_holder.read().await;
            holder.clone()
        };

        let Some(bot) = bot else {
            tracing::warn!("Telegram bot not ready for skill confirm delivery");
            continue;
        };

        let Some(chat_id) = parse_chat_id(&conversation_scope) else {
            tracing::warn!(
                "Could not parse chat ID from conversation_scope: {}",
                conversation_scope
            );
            continue;
        };

        let keyboard = InlineKeyboardMarkup::new(vec![vec![
            InlineKeyboardButton::callback(
                format!("\u{2705} Install {skill_name}"),
                format!("skill_confirm:{token}"),
            ),
            InlineKeyboardButton::callback(
                "\u{274c} Cancel".to_string(),
                format!("skill_cancel:{token}"),
            ),
        ]]);

        let chat = ChatId(chat_id);
        match tokio::time::timeout(
            Duration::from_secs(30),
            bot.send_message(chat, "\u{1f4e6} Confirm skill installation?")
                .reply_markup(keyboard)
                .send(),
        )
        .await
        {
            Ok(Err(e)) => {
                tracing::error!("Failed to send skill confirm keyboard to Telegram: {e}");
            }
            Err(_) => {
                tracing::warn!(
                    chat_id,
                    "telegram skill confirm message delivery timed out after 30s"
                );
            }
            Ok(Ok(_)) => {}
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
                match tokio::time::timeout(
                    Duration::from_secs(30),
                    bot.set_message_reaction(chat, msg_id)
                        .reaction(vec![reaction])
                        .send(),
                )
                .await
                {
                    Ok(Ok(_)) => {
                        tracing::debug!("Set reaction {emoji} on message {message_id}");
                    }
                    Ok(Err(e)) => {
                        tracing::error!("Failed to set reaction: {e}");
                    }
                    Err(_) => {
                        tracing::warn!("telegram set_message_reaction timed out after 30s");
                    }
                }
            }
            ActionKind::Unreact { .. } => {
                // Empty reaction list removes all reactions
                match tokio::time::timeout(
                    Duration::from_secs(30),
                    bot.set_message_reaction(chat, msg_id)
                        .reaction(Vec::<ReactionType>::new())
                        .send(),
                )
                .await
                {
                    Ok(Err(e)) => {
                        tracing::error!("Failed to remove reaction: {e}");
                    }
                    Err(_) => {
                        tracing::warn!("telegram remove reaction timed out after 30s");
                    }
                    Ok(Ok(_)) => {}
                }
            }
            ActionKind::Edit { ref new_text } => {
                let html = md_to_telegram_html(new_text);
                match tokio::time::timeout(
                    Duration::from_secs(30),
                    bot.edit_message_text(chat, msg_id, &html)
                        .parse_mode(ParseMode::Html)
                        .send(),
                )
                .await
                {
                    Ok(Err(e)) => {
                        tracing::error!("Failed to edit message: {e}");
                    }
                    Err(_) => {
                        tracing::warn!("telegram edit_message_text timed out after 30s");
                    }
                    Ok(Ok(_)) => {}
                }
            }
            ActionKind::Delete => {
                match tokio::time::timeout(
                    Duration::from_secs(30),
                    bot.delete_message(chat, msg_id).send(),
                )
                .await
                {
                    Ok(Err(e)) => {
                        tracing::error!("Failed to delete message: {e}");
                    }
                    Err(_) => {
                        tracing::warn!("telegram delete_message timed out after 30s");
                    }
                    Ok(Ok(_)) => {}
                }
            }
        }
    }
}

/// Download any Telegram file by file_id, returning base64-encoded content.
async fn download_file_as_base64(bot: &Bot, file_id: &str) -> anyhow::Result<Vec<u8>> {
    let file = bot.get_file(file_id).await?;
    let mut buf = Vec::new();
    bot.download_file(&file.path, &mut buf).await?;
    Ok(buf)
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
const TELEGRAM_CAPTION_MAX_LEN: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttachmentTextMode {
    TextOnly,
    CaptionFirstAttachment,
    TextThenAttachments,
    AttachmentsOnly,
    Empty,
}

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
        send_html_chunk_with_fallback(bot, chat_id, html).await?;
        return Ok(());
    }

    // Split into chunks at newline boundaries
    let mut remaining = html;
    while !remaining.is_empty() {
        if remaining.len() <= TELEGRAM_MAX_LEN {
            send_html_chunk_with_fallback(bot, chat_id, remaining).await?;
            break;
        }

        // Find a newline boundary to split at
        let split_at = remaining[..TELEGRAM_MAX_LEN]
            .rfind('\n')
            .unwrap_or(TELEGRAM_MAX_LEN);
        let (chunk, rest) = remaining.split_at(split_at);
        // Skip the newline itself if we split at one
        let rest = rest.strip_prefix('\n').unwrap_or(rest);

        send_html_chunk_with_fallback(bot, chat_id, chunk).await?;
        remaining = rest;
    }

    Ok(())
}

async fn send_html_chunk_with_fallback(
    bot: &Bot,
    chat_id: ChatId,
    chunk: &str,
) -> Result<(), teloxide::RequestError> {
    let send_result = match tokio::time::timeout(
        Duration::from_secs(30),
        bot.send_message(chat_id, chunk)
            .parse_mode(ParseMode::Html)
            .send(),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => {
            tracing::warn!(
                chat_id = chat_id.0,
                "telegram html message delivery timed out after 30s"
            );
            return Ok(());
        }
    };

    match send_result {
        Ok(_) => Ok(()),
        Err(err) => {
            let err_text = err.to_string();
            if !is_telegram_parse_entities_error(&err_text) {
                return Err(err);
            }

            let plain_text = strip_telegram_html_tags(chunk);
            let fallback = if plain_text.trim().is_empty() {
                chunk.to_string()
            } else {
                plain_text
            };

            tracing::warn!(
                chat_id = chat_id.0,
                chunk_len = chunk.len(),
                fallback_len = fallback.len(),
                error = %err_text,
                "telegram html parse failed, retrying plain text"
            );

            match tokio::time::timeout(
                Duration::from_secs(30),
                bot.send_message(chat_id, fallback).send(),
            )
            .await
            {
                Ok(result) => {
                    result?;
                }
                Err(_) => {
                    tracing::warn!(
                        chat_id = chat_id.0,
                        "telegram fallback message delivery timed out after 30s"
                    );
                }
            }
            Ok(())
        }
    }
}

fn is_telegram_parse_entities_error(err: &str) -> bool {
    let lower = err.to_ascii_lowercase();
    lower.contains("can't parse entities")
        || lower.contains("unsupported start tag")
        || lower.contains("can't find end tag")
}

fn strip_telegram_html_tags(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut in_tag = false;

    for ch in input.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }

    out.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
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

fn empty_outbound_fallback_text(chat_id: i64) -> Option<&'static str> {
    if chat_id > 0 {
        Some("Sorry, I got an empty response. Please try again.")
    } else {
        None
    }
}

fn attachment_text_mode(text: &str, has_attachments: bool) -> AttachmentTextMode {
    let has_text = !text.trim().is_empty();

    match (has_text, has_attachments) {
        (true, false) => AttachmentTextMode::TextOnly,
        (true, true) if text.len() <= TELEGRAM_CAPTION_MAX_LEN => {
            AttachmentTextMode::CaptionFirstAttachment
        }
        (true, true) => AttachmentTextMode::TextThenAttachments,
        (false, true) => AttachmentTextMode::AttachmentsOnly,
        (false, false) => AttachmentTextMode::Empty,
    }
}

fn compose_inbound_text(user_text: &str, quoted_text: Option<&str>) -> String {
    let trimmed_user = user_text.trim();
    if trimmed_user.starts_with('/') {
        return user_text.to_string();
    }

    let quoted = quoted_text.unwrap_or("").trim();
    if quoted.is_empty() {
        return user_text.to_string();
    }

    format!(
        "[Quoted Message]\n{}\n\n[Current Message]\n{}",
        quoted, user_text
    )
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

async fn resolve_attachment_bytes(att: &Attachment) -> anyhow::Result<Vec<u8>> {
    let url = &att.url;
    if url.starts_with('/') || url.starts_with("./") {
        return tokio::fs::read(url)
            .await
            .map_err(|e| anyhow::anyhow!("read file {url}: {e}"));
    }
    if url.starts_with("http://") || url.starts_with("https://") {
        let resp = reqwest::get(url).await?;
        return Ok(resp.bytes().await?.to_vec());
    }
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(url)
        .map_err(|e| anyhow::anyhow!("base64 decode: {e}"))
}

fn default_file_name(kind: &AttachmentKind, mime_type: &Option<String>) -> String {
    let ext = mime_type
        .as_deref()
        .and_then(|m| m.split('/').nth(1))
        .unwrap_or("bin");
    match kind {
        AttachmentKind::Image => format!("image.{ext}"),
        AttachmentKind::Video => format!("video.{ext}"),
        AttachmentKind::Audio => format!("audio.{ext}"),
        AttachmentKind::Document => format!("document.{ext}"),
        AttachmentKind::Other => format!("file.{ext}"),
    }
}

async fn send_attachments(
    bot: &Bot,
    chat_id: ChatId,
    attachments: &[Attachment],
    caption: Option<&str>,
) {
    for (i, att) in attachments.iter().enumerate() {
        let bytes = match resolve_attachment_bytes(att).await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(error = %e, "failed to resolve attachment data");
                continue;
            }
        };

        let file_name = att
            .file_name
            .clone()
            .unwrap_or_else(|| default_file_name(&att.kind, &att.mime_type));
        let cap = if i == 0 { caption } else { None };

        let result = match att.kind {
            AttachmentKind::Image => {
                let photo_input = InputFile::memory(bytes.clone()).file_name(file_name.clone());
                let mut req = bot.send_photo(chat_id, photo_input);
                if let Some(c) = cap {
                    req = req.caption(c);
                }
                match tokio::time::timeout(Duration::from_secs(30), req.send()).await {
                    Ok(Ok(_)) => Ok(()),
                    Ok(Err(photo_err)) => {
                        tracing::warn!(
                            error = %photo_err,
                            "send_photo failed, retrying as document"
                        );
                        let fallback = InputFile::memory(bytes).file_name(file_name);
                        let mut req = bot.send_document(chat_id, fallback);
                        if let Some(c) = cap {
                            req = req.caption(c);
                        }
                        match tokio::time::timeout(Duration::from_secs(30), req.send()).await {
                            Ok(r) => r.map(|_| ()),
                            Err(_) => {
                                tracing::warn!(
                                    chat_id = chat_id.0,
                                    "telegram document fallback delivery timed out after 30s"
                                );
                                Ok(())
                            }
                        }
                    }
                    Err(_) => {
                        tracing::warn!(
                            chat_id = chat_id.0,
                            "telegram send_photo timed out after 30s"
                        );
                        Ok(())
                    }
                }
            }
            _ => {
                let input = InputFile::memory(bytes).file_name(file_name);
                let mut req = bot.send_document(chat_id, input);
                if let Some(c) = cap {
                    req = req.caption(c);
                }
                match tokio::time::timeout(Duration::from_secs(30), req.send()).await {
                    Ok(r) => r.map(|_| ()),
                    Err(_) => {
                        tracing::warn!(
                            chat_id = chat_id.0,
                            "telegram send_document timed out after 30s"
                        );
                        Ok(())
                    }
                }
            }
        };

        if let Err(e) = result {
            tracing::error!(error = %e, kind = ?att.kind, "failed to send telegram attachment");
        }
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
    fn compose_inbound_text_includes_quoted_context() {
        let text = compose_inbound_text("没下文了吗？", Some("我已经把两个修复都部署好了"));
        assert!(text.contains("[Quoted Message]"));
        assert!(text.contains("我已经把两个修复都部署好了"));
        assert!(text.contains("[Current Message]"));
        assert!(text.contains("没下文了吗？"));
    }

    #[test]
    fn compose_inbound_text_keeps_command_plain() {
        let text = compose_inbound_text("/status", Some("之前那条消息"));
        assert_eq!(text, "/status");
    }

    #[test]
    fn compose_inbound_text_without_quote_keeps_original() {
        let text = compose_inbound_text("你好", None);
        assert_eq!(text, "你好");
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

    #[test]
    fn approval_html_escapes_untrusted_command_and_agent() {
        let command = "python3 - <<'PY'\nprint('<tag>')\nPY";
        let display = ApprovalDisplay::new("agent<one>", command, Some("example.com:443"), None);
        let html = display.to_html();

        assert!(html.contains("<b>Network Access Required</b>"));
        assert!(html.contains("agent&lt;one&gt;"));
        assert!(html.contains("<b>Program:</b> <code>python3</code>"));
        assert!(html.contains("<b>Target:</b> <code>example.com:443</code>"));
        assert!(!html.contains("<'PY'"));
    }

    #[test]
    fn empty_outbound_fallback_is_enabled_for_dm() {
        assert_eq!(
            empty_outbound_fallback_text(12345),
            Some("Sorry, I got an empty response. Please try again.")
        );
    }

    #[test]
    fn empty_outbound_fallback_is_disabled_for_group() {
        assert_eq!(empty_outbound_fallback_text(-10012345), None);
    }

    #[test]
    fn parse_entities_error_detection_matches_expected_error() {
        let err = "A Telegram's error: Bad Request: can't parse entities: Can't find end tag corresponding to start tag \"code\"";
        assert!(is_telegram_parse_entities_error(err));
    }

    #[test]
    fn parse_entities_error_detection_ignores_other_errors() {
        let err = "A Telegram's error: Forbidden: bot was blocked by the user";
        assert!(!is_telegram_parse_entities_error(err));
    }

    #[test]
    fn strip_telegram_html_tags_removes_tags_and_decodes_entities() {
        let text = "<b>hello</b> <code>x &lt; y</code> &amp; done";
        assert_eq!(strip_telegram_html_tags(text), "hello x < y & done");
    }

    #[test]
    fn default_file_name_uses_document_prefix_for_documents() {
        let mime = Some("application/pdf".to_string());
        assert_eq!(
            default_file_name(&AttachmentKind::Document, &mime),
            "document.pdf"
        );
    }

    #[test]
    fn attachment_text_mode_prefers_caption_for_short_text_with_attachments() {
        assert_eq!(
            attachment_text_mode("short text", true),
            AttachmentTextMode::CaptionFirstAttachment
        );
    }

    #[test]
    fn attachment_text_mode_sends_text_first_for_long_text_with_attachments() {
        let text = "x".repeat(1025);
        assert_eq!(
            attachment_text_mode(&text, true),
            AttachmentTextMode::TextThenAttachments
        );
    }

    #[test]
    fn attachment_text_mode_returns_attachments_only_without_text() {
        assert_eq!(
            attachment_text_mode("   ", true),
            AttachmentTextMode::AttachmentsOnly
        );
    }

    #[test]
    fn attachment_text_mode_keeps_text_only_behavior_without_attachments() {
        assert_eq!(
            attachment_text_mode("hello", false),
            AttachmentTextMode::TextOnly
        );
    }
}
