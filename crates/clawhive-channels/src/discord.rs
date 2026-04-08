use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use clawhive_bus::{EventBus, Topic};
use clawhive_gateway::Gateway;
use clawhive_schema::{
    ApprovalDisplay, Attachment, AttachmentKind, BusMessage, InboundMessage, OutboundMessage,
};
use serenity::all::{
    ButtonStyle, ChannelId, Client, Command, CommandInteraction, CommandOptionType,
    ComponentInteraction, Context, CreateActionRow, CreateAttachment, CreateButton, CreateCommand,
    CreateCommandOption, CreateInteractionResponse, CreateInteractionResponseFollowup,
    CreateInteractionResponseMessage, CreateMessage, EditInteractionResponse, EventHandler,
    GatewayIntents, Http, Interaction, Message, Ready,
};
use serenity::async_trait;
use serenity::model::application::CommandDataOptionValue;
use tokio::sync::{watch, RwLock};
use uuid::Uuid;

use crate::common::{infer_mime_from_filename, AbortOnDrop, PROGRESS_MESSAGE};

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
        message_id: Option<u64>,
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
            message_id: message_id.map(|id| id.to_string()),
            attachments: vec![],
            message_source: None,
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
    allowed_groups: Vec<String>,
    require_mention: bool,
}

impl DiscordBot {
    pub fn new(token: String, connector_id: String, gateway: Arc<Gateway>) -> Self {
        Self {
            token,
            connector_id,
            gateway,
            bus: None,
            allowed_groups: Vec::new(),
            require_mention: true,
        }
    }

    pub fn with_bus(mut self, bus: Arc<EventBus>) -> Self {
        self.bus = Some(bus);
        self
    }

    pub fn with_groups(mut self, groups: Vec<String>) -> Self {
        self.allowed_groups = groups;
        self
    }

    pub fn with_require_mention(mut self, require: bool) -> Self {
        self.require_mention = require;
        self
    }

    pub async fn run_impl(self) -> anyhow::Result<()> {
        // Note: GUILD_MEMBERS is a privileged intent, must be enabled in Discord Developer Portal
        let intents = GatewayIntents::GUILD_MESSAGES
            | GatewayIntents::DIRECT_MESSAGES
            | GatewayIntents::MESSAGE_CONTENT
            | GatewayIntents::GUILD_MEMBERS;

        let http_holder: Arc<RwLock<Option<Arc<Http>>>> = Arc::new(RwLock::new(None));
        let connector_id_for_delivery = self.connector_id.clone();

        let handler = DiscordHandler {
            connector_id: self.connector_id,
            gateway: self.gateway,
            http_holder: http_holder.clone(),
            http_client: reqwest::Client::new(),
            allowed_groups: self.allowed_groups,
            require_mention: self.require_mention,
        };

        // Spawn delivery listener if bus is available
        if let Some(bus) = self.bus {
            let http_holder_clone = http_holder.clone();
            let connector_id_delivery = connector_id_for_delivery.clone();
            let bus_clone = bus.clone();
            tokio::spawn(async move {
                spawn_delivery_listener(bus_clone, http_holder_clone, connector_id_delivery).await;
            });

            let http_holder_approval = http_holder.clone();
            let connector_id_approval = connector_id_for_delivery.clone();
            let bus_approval = bus.clone();
            tokio::spawn(async move {
                spawn_approval_listener(bus_approval, http_holder_approval, connector_id_approval)
                    .await;
            });

            let http_holder_skill = http_holder.clone();
            let connector_id_skill = connector_id_for_delivery;
            tokio::spawn(async move {
                spawn_skill_confirm_listener(bus, http_holder_skill, connector_id_skill).await;
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
    http_client: reqwest::Client,
    allowed_groups: Vec<String>,
    require_mention: bool,
}

#[async_trait]
impl EventHandler for DiscordHandler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        tracing::info!(
            "discord bot connected: {} ({})",
            ready.user.name,
            self.connector_id
        );
        // Register slash commands from centralized registry
        let commands = build_discord_commands();
        if let Err(e) = Command::set_global_commands(&ctx.http, commands).await {
            tracing::warn!("Failed to register Discord slash commands: {e}");
        }

        // Store HTTP client for delivery listener
        let mut holder = self.http_holder.write().await;
        *holder = Some(ctx.http.clone());
    }

    async fn message(&self, ctx: Context, msg: Message) {
        if msg.author.bot {
            return;
        }

        let text = msg.content.trim();
        if text.is_empty() && msg.attachments.is_empty() {
            return;
        }

        let adapter = DiscordAdapter::new(self.connector_id.clone());
        let guild_id = msg.guild_id.map(|id| id.get());
        let channel_id = msg.channel_id;
        let user_id = msg.author.id.get();
        let current_user_id = ctx.cache.current_user().id;
        let is_user_mention = msg.mentions.iter().any(|u| u.id == current_user_id);
        // Also detect role mentions: in private channels, bots may not appear in the
        // member list but can be mentioned via their managed role (@BotName App).
        let is_role_mention = !msg.mention_roles.is_empty()
            && msg.guild_id.is_some()
            && ctx
                .cache
                .guild(msg.guild_id.expect("guild_id checked above"))
                .is_some_and(|guild| {
                    guild.roles.values().any(|role| {
                        role.tags.bot_id == Some(current_user_id)
                            && msg.mention_roles.contains(&role.id)
                    })
                });
        let is_mention = is_user_mention || is_role_mention;

        // Group filtering: if groups whitelist is configured, only respond in specified channels
        if !self.allowed_groups.is_empty() && guild_id.is_some() {
            let ch = channel_id.get().to_string();
            if !self.allowed_groups.contains(&ch) {
                return;
            }
        }

        // Mention check: configurable via require_mention (DMs always pass through)
        if guild_id.is_some() && self.require_mention && !is_mention {
            return;
        }

        // Capture quoted message text from Discord reply
        let quoted_text = msg
            .referenced_message
            .as_ref()
            .map(|quoted| quoted.content.clone());

        let composed_text = compose_inbound_text(text, quoted_text.as_deref());

        let mut inbound = adapter.to_inbound(
            guild_id,
            channel_id.get(),
            user_id,
            &composed_text,
            Some(msg.id.get()),
        );
        inbound.is_mention = is_mention;
        inbound.mention_target = if is_mention {
            Some(format!("<@{}>", current_user_id.get()))
        } else {
            None
        };

        // Extract attachments — download images and supported documents as base64
        for att in &msg.attachments {
            let content_type = att.content_type.as_deref();
            let kind = infer_inbound_attachment_kind(content_type, &att.filename);
            let mime_type = infer_inbound_attachment_mime_type(content_type, &att.filename);
            // Download images and inline-able documents (text/PDF) so the orchestrator
            // receives bytes instead of a remote URL placeholder.
            let url_or_data = if should_download_inbound_attachment(&kind) {
                match download_attachment(&self.http_client, &att.url).await {
                    Ok(base64_data) => base64_data,
                    Err(e) => {
                        tracing::warn!("Failed to download Discord attachment: {e}");
                        continue;
                    }
                }
            } else {
                att.url.clone()
            };
            inbound.attachments.push(Attachment {
                kind,
                url: url_or_data,
                mime_type,
                file_name: Some(att.filename.clone()),
                size: Some(att.size as u64),
            });
        }

        let _ = channel_id.broadcast_typing(&ctx.http).await;
        let lifecycle = self.gateway.resolve_turn_lifecycle(&inbound);
        let typing_ttl = lifecycle.typing_ttl_secs;
        let progress_delay = lifecycle.progress_delay_secs;

        let gateway = self.gateway.clone();
        let http = ctx.http.clone();
        let http_typing = ctx.http.clone();
        let user_msg_id = msg.id.get().to_string();
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
                let http = http_typing.clone();
                let mut turn_done_rx = turn_done_rx.clone();
                async move {
                    let deadline =
                        tokio::time::Instant::now() + std::time::Duration::from_secs(typing_ttl);
                    loop {
                        if tokio::time::Instant::now() >= deadline {
                            break;
                        }
                        let wait = std::time::Duration::from_secs(8)
                            .min(deadline.saturating_duration_since(tokio::time::Instant::now()));
                        tokio::select! {
                            _ = tokio::time::sleep(wait) => {
                                if channel_id.broadcast_typing(&http).await.is_err() {
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
                    let http = http.clone();
                    let mut turn_done_rx = turn_done_rx.clone();
                    async move {
                        tokio::select! {
                            _ = tokio::time::sleep(std::time::Duration::from_secs(progress_delay)) => {
                                if !*turn_done_rx.borrow() {
                                    if let Err(err) = channel_id.say(&http, PROGRESS_MESSAGE).await {
                                        tracing::warn!("failed to send discord progress message: {err}");
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
                        let has_attachments = !outbound.attachments.is_empty();
                        let has_text = !outbound.text.trim().is_empty();

                        if has_text && has_attachments && outbound.text.len() <= DISCORD_MAX_LEN {
                            if let Err(err) = send_attachments(
                                channel_id,
                                &http,
                                &outbound.attachments,
                                Some(outbound.text.as_str()),
                            )
                            .await
                            {
                                tracing::error!("failed to send discord attachments: {err}");
                            }
                        } else {
                            if has_text {
                                let reply = outbound.text.as_str();
                                let reply_to = outbound.reply_to.as_deref().unwrap_or(&user_msg_id);
                                if let Err(err) =
                                    send_chunked(channel_id, &http, reply, Some(reply_to)).await
                                {
                                    tracing::error!("failed to send discord reply: {err}");
                                }
                            } else if !has_attachments {
                                let reply = "Sorry, I got an empty response. Please try again.";
                                if let Err(err) = send_chunked(channel_id, &http, reply, None).await
                                {
                                    tracing::error!("failed to send discord reply: {err}");
                                }
                            }

                            if has_attachments {
                                if let Err(err) =
                                    send_attachments(channel_id, &http, &outbound.attachments, None)
                                        .await
                                {
                                    tracing::error!("failed to send discord attachments: {err}");
                                }
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(err) => {
                        tracing::error!("discord gateway error: {err}");
                        let user_msg = format!("Error: {err}");
                        if let Err(send_err) = channel_id.say(&http, &user_msg).await {
                            tracing::error!("failed to send discord error message: {send_err}");
                        }
                    }
                },
                Err(err) => {
                    tracing::error!("discord inbound task join error: {err}");
                }
            }
        });
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        match interaction {
            Interaction::Command(cmd) => {
                self.handle_command_interaction(&ctx, cmd).await;
            }
            Interaction::Component(component) => {
                self.handle_component_interaction(&ctx, component).await;
            }
            _ => {}
        }
    }
}

impl DiscordHandler {
    async fn handle_component_interaction(&self, ctx: &Context, component: ComponentInteraction) {
        let custom_id = &component.data.custom_id;

        // Skill install confirm/cancel buttons
        if let Some(token) = custom_id.strip_prefix("skill_confirm:") {
            let text = format!("/skill confirm {token}");
            self.handle_button_command(ctx, &component, &text).await;
            return;
        }
        if custom_id.starts_with("skill_cancel:") {
            let response = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("Installation cancelled.")
                    .ephemeral(true),
            );
            let _ = component.create_response(&ctx.http, response).await;
            return;
        }

        // Approval buttons
        let Some(rest) = custom_id.strip_prefix("approve:") else {
            return;
        };

        let parts: Vec<&str> = rest.splitn(2, ':').collect();
        if parts.len() != 2 {
            return;
        }

        let short_id = parts[0];
        let decision = parts[1];
        let text = format!("/approve {short_id} {decision}");
        self.handle_button_command(ctx, &component, &text).await;
    }

    /// Route a button click through the gateway as a command and reply ephemerally.
    async fn handle_button_command(
        &self,
        ctx: &Context,
        component: &ComponentInteraction,
        text: &str,
    ) {
        // Defer first — Discord requires a response within 3 seconds,
        // but gateway.handle_inbound may take much longer.
        let defer = CreateInteractionResponse::Defer(
            CreateInteractionResponseMessage::new().ephemeral(true),
        );
        if let Err(e) = component.create_response(&ctx.http, defer).await {
            tracing::error!("Failed to defer Discord component interaction: {e}");
            return;
        }

        let adapter = DiscordAdapter::new(self.connector_id.clone());
        let guild_id = component.guild_id.map(|id| id.get());
        let channel_id = component.channel_id;
        let user_id = component.user.id.get();
        let inbound = adapter.to_inbound(guild_id, channel_id.get(), user_id, text, None);

        let reply_text = match self.gateway.handle_inbound(inbound).await {
            Ok(Some(outbound)) => outbound.text,
            Ok(None) => String::new(),
            Err(e) => format!("❌ Error: {e}"),
        };

        // Edit the deferred response with actual content.
        let builder = EditInteractionResponse::new().content(&reply_text);
        if let Err(e) = component.edit_response(&ctx.http, builder).await {
            tracing::error!("Failed to edit Discord component response: {e}");
        }
    }

    async fn handle_command_interaction(&self, ctx: &Context, cmd: CommandInteraction) {
        // Build command text (matching text command format, reuses parse_command)
        let text = match cmd.data.name.as_str() {
            "new" => {
                let model_hint = cmd.data.options.first().and_then(|o| o.value.as_str());
                match model_hint {
                    Some(hint) => format!("/new {hint}"),
                    None => "/new".to_string(),
                }
            }
            "skill" => {
                // Extract subcommand and its argument
                let mut text = "/skill".to_string();
                if let Some(sub_option) = cmd.data.options.first() {
                    let subname = sub_option.name.as_str();
                    if let CommandDataOptionValue::SubCommand(sub_options) = &sub_option.value {
                        if let Some(arg_option) = sub_options.first() {
                            if let Some(arg_value) = arg_option.value.as_str() {
                                text = format!("/skill {subname} {arg_value}");
                            }
                        }
                    }
                }
                text
            }
            "model" => {
                let model_arg = cmd.data.options.first().and_then(|o| o.value.as_str());
                match model_arg {
                    Some(m) => format!("/model {m}"),
                    None => "/model".to_string(),
                }
            }
            "help" => {
                let help_text = clawhive_schema::command_registry::help_text();
                respond_to_interaction(ctx, &cmd, &help_text).await;
                return;
            }
            other => format!("/{other}"),
        };

        // Defer response (gateway processing may exceed 3s)
        defer_interaction(ctx, &cmd).await;

        // Build InboundMessage and route through gateway
        let adapter = DiscordAdapter::new(self.connector_id.clone());
        let guild_id = cmd.guild_id.map(|id| id.get());
        let channel_id = cmd.channel_id;
        let user_id = cmd.user.id.get();
        let inbound = adapter.to_inbound(guild_id, channel_id.get(), user_id, &text, None);

        match self.gateway.handle_inbound(inbound).await {
            Ok(Some(outbound)) => {
                let reply = if outbound.text.trim().is_empty() {
                    "Sorry, I got an empty response. Please try again."
                } else {
                    outbound.text.as_str()
                };
                edit_deferred_response(ctx, &cmd, reply).await;
            }
            Ok(None) => {
                edit_deferred_response(ctx, &cmd, "No agent matched this request.").await;
            }
            Err(err) => {
                tracing::error!("discord slash command gateway error: {err}");
                let user_msg = format!("Error: {err}");
                edit_deferred_response(ctx, &cmd, &user_msg).await;
            }
        }
    }
}

/// Build Discord slash commands from the centralized command registry.
///
/// Top-level commands (e.g. "new", "status") become standalone commands.
/// Compound commands sharing a prefix (e.g. "skill list", "skill remove") are
/// grouped under a parent command with subcommands.
fn build_discord_commands() -> Vec<CreateCommand> {
    use clawhive_schema::command_registry::{command_registry, CommandDef};
    use std::collections::BTreeMap;

    let registry = command_registry();

    // Separate top-level commands from subcommand groups
    let mut top_level: Vec<&CommandDef> = Vec::new();
    let mut groups: BTreeMap<&str, Vec<&CommandDef>> = BTreeMap::new();

    for cmd in registry {
        if cmd.subcommand().is_some() {
            groups.entry(cmd.root()).or_default().push(cmd);
        } else {
            top_level.push(cmd);
        }
    }

    let mut commands = Vec::new();

    // Build top-level commands
    for cmd in &top_level {
        let mut c = CreateCommand::new(cmd.name).description(cmd.description);
        for arg in cmd.args {
            c = c.add_option(
                CreateCommandOption::new(CommandOptionType::String, arg.name, arg.description)
                    .required(arg.required),
            );
        }
        commands.push(c);
    }

    // Build grouped commands (e.g. /skill with subcommands)
    for (parent, subs) in &groups {
        let mut c = CreateCommand::new(*parent).description(format!("Manage {parent}s"));
        for sub in subs {
            let sub_name = sub.subcommand().expect("grouped cmd must have subcommand");
            let mut opt =
                CreateCommandOption::new(CommandOptionType::SubCommand, sub_name, sub.description);
            for arg in sub.args {
                opt = opt.add_sub_option(
                    CreateCommandOption::new(CommandOptionType::String, arg.name, arg.description)
                        .required(arg.required),
                );
            }
            c = c.add_option(opt);
        }
        commands.push(c);
    }

    commands
}

const DISCORD_MAX_LEN: usize = 2000;

/// Find the largest byte index <= `max` that lies on a UTF-8 char boundary.
fn floor_char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut i = max;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Split a message into chunks that fit within Discord's 2000-byte limit.
/// Tries to break at newlines, then spaces, to keep messages readable.
fn split_message(text: &str) -> Vec<&str> {
    if text.len() <= DISCORD_MAX_LEN {
        return vec![text];
    }
    let mut chunks = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        if rest.len() <= DISCORD_MAX_LEN {
            chunks.push(rest);
            break;
        }
        let safe_end = floor_char_boundary(rest, DISCORD_MAX_LEN);
        let boundary = &rest[..safe_end];
        let split_at = boundary
            .rfind('\n')
            .or_else(|| boundary.rfind(' '))
            .map(|i| i + 1)
            .unwrap_or(safe_end);
        chunks.push(&rest[..split_at]);
        rest = &rest[split_at..];
    }
    chunks
}

/// Send a potentially long message as multiple chunks.
/// If `reply_to` is provided, the first chunk is sent as a reply to that message.
async fn send_chunked(
    channel_id: ChannelId,
    http: &Http,
    text: &str,
    reply_to: Option<&str>,
) -> Result<(), serenity::Error> {
    let chunks = split_message(text);
    let mut first = true;
    for chunk in chunks {
        if first {
            first = false;
            if let Some(msg_id_str) = reply_to {
                if let Ok(msg_id) = msg_id_str.parse::<u64>() {
                    let msg_id = serenity::model::id::MessageId::new(msg_id);
                    let message = CreateMessage::new()
                        .content(chunk)
                        .reference_message((channel_id, msg_id));
                    match tokio::time::timeout(
                        Duration::from_secs(30),
                        channel_id.send_message(http, message),
                    )
                    .await
                    {
                        Ok(result) => {
                            result?;
                        }
                        Err(_) => {
                            tracing::warn!(
                                channel_id = channel_id.get(),
                                "discord message delivery timed out after 30s"
                            );
                            return Ok(());
                        }
                    }
                    continue;
                }
            }
        }
        match tokio::time::timeout(Duration::from_secs(30), channel_id.say(http, chunk)).await {
            Ok(result) => {
                result?;
            }
            Err(_) => {
                tracing::warn!(
                    channel_id = channel_id.get(),
                    "discord message delivery timed out after 30s"
                );
                return Ok(());
            }
        }
    }
    Ok(())
}

/// Defer an interaction response (for long-running commands).
async fn defer_interaction(ctx: &Context, cmd: &CommandInteraction) {
    let response = CreateInteractionResponse::Defer(CreateInteractionResponseMessage::new());
    if let Err(e) = cmd.create_response(&ctx.http, response).await {
        tracing::error!("Failed to defer Discord interaction: {e}");
    }
}

/// Edit the deferred response with actual content.
async fn edit_deferred_response(ctx: &Context, cmd: &CommandInteraction, text: &str) {
    // Use split_message which already handles char boundaries
    let chunks = split_message(text);

    // First chunk goes into the deferred response edit
    if let Some(&first) = chunks.first() {
        let builder = EditInteractionResponse::new().content(first);
        if let Err(e) = cmd.edit_response(&ctx.http, builder).await {
            tracing::error!("Failed to edit Discord interaction response: {e}");
        }
    }

    // Remaining chunks sent as followups
    for &chunk in chunks.iter().skip(1) {
        if let Err(e) = cmd
            .create_followup(
                &ctx.http,
                CreateInteractionResponseFollowup::new().content(chunk),
            )
            .await
        {
            tracing::error!("Failed to send followup: {e}");
        }
    }
}

/// Respond immediately to an interaction (for simple commands like /help).
async fn respond_to_interaction(ctx: &Context, cmd: &CommandInteraction, text: &str) {
    let response =
        CreateInteractionResponse::Message(CreateInteractionResponseMessage::new().content(text));
    if let Err(e) = cmd.create_response(&ctx.http, response).await {
        tracing::error!("Failed to respond to Discord interaction: {e}");
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

        let Some(channel_id) = parse_channel_id(&conversation_scope) else {
            if conversation_scope.starts_with("schedule:") {
                tracing::debug!(
                    "Skipping delivery for schedule scope: {}",
                    conversation_scope
                );
            } else {
                tracing::warn!(
                    "Could not parse channel ID from conversation_scope: {}",
                    conversation_scope
                );
            }
            continue;
        };

        let channel = ChannelId::new(channel_id);
        if let Err(e) = send_chunked(channel, &http, &text, None).await {
            tracing::error!("Failed to deliver announce message: {e}");
        } else {
            tracing::info!("Delivered scheduled task result to channel {}", channel_id);
        }
    }
}

/// Spawn a listener for DeliverApprovalRequest messages — sends buttons
async fn spawn_approval_listener(
    bus: Arc<EventBus>,
    http_holder: Arc<RwLock<Option<Arc<Http>>>>,
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

        if channel_type != "discord" || msg_connector_id != connector_id {
            continue;
        }

        let http = {
            let holder = http_holder.read().await;
            holder.clone()
        };

        let Some(http) = http else {
            tracing::warn!("Discord HTTP client not ready for approval delivery");
            continue;
        };

        let Some(channel_id) = parse_channel_id(&conversation_scope) else {
            if conversation_scope.starts_with("schedule:") {
                tracing::debug!(
                    "Skipping delivery for schedule scope: {}",
                    conversation_scope
                );
            } else {
                tracing::warn!(
                    "Could not parse channel ID from conversation_scope: {}",
                    conversation_scope
                );
            }
            continue;
        };

        let display = ApprovalDisplay::new(&agent_id, &command, network_target.as_deref(), summary);
        let text = display.to_markdown();

        let buttons = CreateActionRow::Buttons(vec![
            CreateButton::new(format!("approve:{short_id}:allow"))
                .label("✅ Allow Once")
                .style(ButtonStyle::Success),
            CreateButton::new(format!("approve:{short_id}:always"))
                .label("🔓 Always Allow")
                .style(ButtonStyle::Primary),
            CreateButton::new(format!("approve:{short_id}:deny"))
                .label("❌ Deny")
                .style(ButtonStyle::Danger),
        ]);

        let message = CreateMessage::new()
            .content(&text)
            .components(vec![buttons]);

        let channel = ChannelId::new(channel_id);
        match tokio::time::timeout(
            Duration::from_secs(30),
            channel.send_message(&http, message),
        )
        .await
        {
            Ok(Err(e)) => {
                tracing::error!("Failed to send approval buttons: {e}");
            }
            Err(_) => {
                tracing::warn!(
                    channel_id,
                    "discord approval message delivery timed out after 30s"
                );
            }
            Ok(Ok(_)) => {}
        }
    }
}

/// Spawn a listener for DeliverSkillConfirm messages — sends confirm/cancel buttons
async fn spawn_skill_confirm_listener(
    bus: Arc<EventBus>,
    http_holder: Arc<RwLock<Option<Arc<Http>>>>,
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

        if channel_type != "discord" || msg_connector_id != connector_id {
            continue;
        }

        let http = {
            let holder = http_holder.read().await;
            holder.clone()
        };

        let Some(http) = http else {
            tracing::warn!("Discord HTTP client not ready for skill confirm delivery");
            continue;
        };

        let Some(channel_id) = parse_channel_id(&conversation_scope) else {
            if conversation_scope.starts_with("schedule:") {
                tracing::debug!(
                    "Skipping delivery for schedule scope: {}",
                    conversation_scope
                );
            } else {
                tracing::warn!(
                    "Could not parse channel ID from conversation_scope: {}",
                    conversation_scope
                );
            }
            continue;
        };

        let buttons = CreateActionRow::Buttons(vec![
            CreateButton::new(format!("skill_confirm:{token}"))
                .label(format!("\u{2705} Install {skill_name}"))
                .style(ButtonStyle::Success),
            CreateButton::new(format!("skill_cancel:{token}"))
                .label("\u{274c} Cancel".to_string())
                .style(ButtonStyle::Danger),
        ]);

        let channel = ChannelId::new(channel_id);
        let message = CreateMessage::new().components(vec![buttons]);
        match tokio::time::timeout(
            Duration::from_secs(30),
            channel.send_message(&http, message),
        )
        .await
        {
            Ok(Err(e)) => {
                tracing::error!("Failed to send skill confirm buttons: {e}");
            }
            Err(_) => {
                tracing::warn!(
                    channel_id,
                    "discord skill confirm message delivery timed out after 30s"
                );
            }
            Ok(Ok(_)) => {}
        }
    }
}

/// Compose inbound text with optional quoted message context.
/// Commands (starting with `/`) are passed through without quoting.
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

/// Keep in sync with `is_text_mime` in `clawhive-core/src/orchestrator.rs`.
fn is_text_content_type(ct: &str) -> bool {
    ct.starts_with("text/")
        || ct == "application/json"
        || ct == "application/xml"
        || ct == "application/javascript"
        || ct == "application/x-yaml"
        || ct == "application/yaml"
        || ct == "application/toml"
        || ct == "application/x-sh"
}

fn is_inline_document_content_type(ct: &str) -> bool {
    is_text_content_type(ct) || ct == "application/pdf"
}

fn infer_inbound_attachment_kind(content_type: Option<&str>, file_name: &str) -> AttachmentKind {
    let inferred_mime = content_type
        .map(str::to_owned)
        .or_else(|| infer_mime_from_filename(Some(file_name)));

    match inferred_mime.as_deref() {
        Some(ct) if ct.starts_with("image/") => AttachmentKind::Image,
        Some(ct) if ct.starts_with("video/") => AttachmentKind::Video,
        Some(ct) if ct.starts_with("audio/") => AttachmentKind::Audio,
        Some(ct) if is_inline_document_content_type(ct) => AttachmentKind::Document,
        _ => AttachmentKind::Other,
    }
}

fn infer_inbound_attachment_mime_type(
    content_type: Option<&str>,
    file_name: &str,
) -> Option<String> {
    content_type
        .map(ToOwned::to_owned)
        .or_else(|| infer_mime_from_filename(Some(file_name)))
}

fn should_download_inbound_attachment(kind: &AttachmentKind) -> bool {
    matches!(kind, AttachmentKind::Image | AttachmentKind::Document)
}

/// Download a Discord attachment and return its content as a base64-encoded string.
async fn download_attachment(client: &reqwest::Client, url: &str) -> anyhow::Result<String> {
    use base64::Engine;

    let bytes = client.get(url).send().await?.bytes().await?;
    let base64_data = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(base64_data)
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
    channel_id: ChannelId,
    http: &Http,
    attachments: &[Attachment],
    text: Option<&str>,
) -> Result<(), serenity::Error> {
    let mut discord_attachments = Vec::new();

    for att in attachments {
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
        discord_attachments.push(CreateAttachment::bytes(bytes, file_name));
    }

    if discord_attachments.is_empty() {
        return Ok(());
    }

    let mut msg = CreateMessage::new();
    if let Some(t) = text {
        if t.len() <= DISCORD_MAX_LEN {
            msg = msg.content(t);
        }
    }
    for att in discord_attachments {
        msg = msg.add_file(att);
    }
    match tokio::time::timeout(Duration::from_secs(30), channel_id.send_message(http, msg)).await {
        Ok(result) => {
            result?;
        }
        Err(_) => {
            tracing::warn!(
                channel_id = channel_id.get(),
                "discord attachment delivery timed out after 30s"
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    #[test]
    fn adapter_to_inbound_dm_sets_fields() {
        let adapter = DiscordAdapter::new("dc_main");
        let msg = adapter.to_inbound(None, 123, 456, "hello", None);
        assert_eq!(msg.channel_type, "discord");
        assert_eq!(msg.connector_id, "dc_main");
        assert_eq!(msg.conversation_scope, "dm:123");
        assert_eq!(msg.user_scope, "user:456");
        assert_eq!(msg.text, "hello");
    }

    #[test]
    fn adapter_to_inbound_guild_sets_fields() {
        let adapter = DiscordAdapter::new("dc_main");
        let msg = adapter.to_inbound(Some(999), 123, 456, "hello", None);
        assert_eq!(msg.conversation_scope, "guild:999:channel:123");
    }

    #[test]
    fn adapter_to_inbound_defaults() {
        let adapter = DiscordAdapter::new("dc_main");
        let msg = adapter.to_inbound(None, 123, 456, "hello", None);
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
        let msg = adapter.to_inbound(None, 123, 456, text, None);
        assert_eq!(msg.text, text);
    }

    #[test]
    fn adapter_to_inbound_trace_id_unique() {
        let adapter = DiscordAdapter::new("dc_main");
        let msg1 = adapter.to_inbound(None, 123, 456, "hello", None);
        let msg2 = adapter.to_inbound(None, 123, 456, "hello", None);
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
        let msg = adapter.to_inbound(None, 123, 456, "test", None);
        assert_eq!(msg.connector_id, "dc-prod-1");
    }

    #[test]
    fn split_message_short_text_single_chunk() {
        let chunks = split_message("hello");
        assert_eq!(chunks, vec!["hello"]);
    }

    #[test]
    fn split_message_exact_limit_single_chunk() {
        let text = "a".repeat(DISCORD_MAX_LEN);
        let chunks = split_message(&text);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len(), DISCORD_MAX_LEN);
    }

    #[test]
    fn split_message_long_text_splits_at_newline() {
        let mut text = "a".repeat(1900);
        text.push('\n');
        text.push_str(&"b".repeat(500));
        let chunks = split_message(&text);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].len() <= DISCORD_MAX_LEN);
        assert!(chunks[1].len() <= DISCORD_MAX_LEN);
    }

    #[test]
    fn split_message_long_text_splits_at_space() {
        let mut text = "a".repeat(1900);
        text.push(' ');
        text.push_str(&"b".repeat(500));
        let chunks = split_message(&text);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].len() <= DISCORD_MAX_LEN);
    }

    #[test]
    fn split_message_no_break_point_hard_splits() {
        let text = "a".repeat(4500);
        let chunks = split_message(&text);
        assert!(chunks.len() >= 3);
        for chunk in &chunks {
            assert!(chunk.len() <= DISCORD_MAX_LEN);
        }
    }

    #[test]
    fn split_message_multibyte_chars_no_panic() {
        // Each Chinese char is 3 bytes in UTF-8; fill to just over DISCORD_MAX_LEN
        let text = "中".repeat(700); // 700 * 3 = 2100 bytes
        let chunks = split_message(&text);
        assert_eq!(chunks.len(), 2);
        for chunk in &chunks {
            assert!(chunk.len() <= DISCORD_MAX_LEN);
            // Every chunk must be valid UTF-8 (no split mid-char)
            assert!(chunk.is_ascii() || chunk.chars().count() > 0);
        }
    }

    #[test]
    fn compose_inbound_text_includes_quoted_context() {
        let text = compose_inbound_text("这是什么意思？", Some("之前的消息内容"));
        assert!(text.contains("[Quoted Message]"));
        assert!(text.contains("之前的消息内容"));
        assert!(text.contains("[Current Message]"));
        assert!(text.contains("这是什么意思？"));
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
    fn to_inbound_with_message_id() {
        let adapter = DiscordAdapter::new("dc_main");
        let msg = adapter.to_inbound(None, 123, 456, "hello", Some(789));
        assert_eq!(msg.message_id, Some("789".to_string()));
    }

    #[test]
    fn to_inbound_without_message_id() {
        let adapter = DiscordAdapter::new("dc_main");
        let msg = adapter.to_inbound(None, 123, 456, "hello", None);
        assert_eq!(msg.message_id, None);
    }

    #[test]
    fn floor_char_boundary_on_ascii() {
        let s = "hello world";
        assert_eq!(floor_char_boundary(s, 5), 5);
    }

    #[test]
    fn floor_char_boundary_mid_multibyte() {
        let s = "ab中"; // bytes: 61 62 e4 b8 ad
                        // index 3 is the start of '中', index 4 is mid-char
        assert_eq!(floor_char_boundary(s, 4), 2);
        assert_eq!(floor_char_boundary(s, 3), 2);
        assert_eq!(floor_char_boundary(s, 5), 5); // end of string
    }

    #[test]
    fn default_file_name_uses_kind_specific_prefixes() {
        let image = default_file_name(&AttachmentKind::Image, &Some("image/png".to_string()));
        let video = default_file_name(&AttachmentKind::Video, &Some("video/mp4".to_string()));
        let audio = default_file_name(&AttachmentKind::Audio, &Some("audio/mpeg".to_string()));
        let document =
            default_file_name(&AttachmentKind::Document, &Some("text/plain".to_string()));
        let other = default_file_name(&AttachmentKind::Other, &None);

        assert_eq!(image, "image.png");
        assert_eq!(video, "video.mp4");
        assert_eq!(audio, "audio.mpeg");
        assert_eq!(document, "document.plain");
        assert_eq!(other, "file.bin");
    }

    #[test]
    fn inline_document_content_type_includes_pdf() {
        assert!(is_inline_document_content_type("text/plain"));
        assert!(is_inline_document_content_type("application/pdf"));
        assert!(!is_inline_document_content_type(
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        ));
    }

    #[test]
    fn infer_inbound_attachment_recovers_pdf_without_content_type() {
        assert_eq!(
            infer_inbound_attachment_kind(None, "TA Dated 20 August 2024_20240820_0001.pdf"),
            AttachmentKind::Document
        );
        assert_eq!(
            infer_inbound_attachment_mime_type(None, "TA Dated 20 August 2024_20240820_0001.pdf"),
            Some("application/pdf".to_string())
        );
    }

    #[test]
    fn infer_inbound_attachment_recovers_text_without_content_type() {
        assert_eq!(
            infer_inbound_attachment_kind(None, "notes.txt"),
            AttachmentKind::Document
        );
        assert_eq!(
            infer_inbound_attachment_mime_type(None, "notes.txt"),
            Some("text/plain".to_string())
        );
    }

    #[test]
    fn should_download_inbound_attachment_for_images_and_documents() {
        assert!(should_download_inbound_attachment(&AttachmentKind::Image));
        assert!(should_download_inbound_attachment(
            &AttachmentKind::Document
        ));
        assert!(!should_download_inbound_attachment(&AttachmentKind::Other));
    }

    #[tokio::test]
    async fn resolve_attachment_bytes_decodes_base64() {
        let att = Attachment {
            kind: AttachmentKind::Document,
            url: "aGVsbG8=".to_string(),
            mime_type: Some("text/plain".to_string()),
            file_name: None,
            size: None,
        };

        let bytes = resolve_attachment_bytes(&att).await.unwrap();

        assert_eq!(bytes, b"hello");
    }

    #[tokio::test]
    async fn resolve_attachment_bytes_reads_local_file() {
        let unique = Uuid::new_v4();
        let path = std::env::temp_dir().join(format!("clawhive-discord-{unique}.txt"));
        let mut file = fs::File::create(&path).unwrap();
        file.write_all(b"discord-file").unwrap();
        let att = Attachment {
            kind: AttachmentKind::Document,
            url: path.display().to_string(),
            mime_type: Some("text/plain".to_string()),
            file_name: None,
            size: None,
        };

        let bytes = resolve_attachment_bytes(&att).await.unwrap();

        assert_eq!(bytes, b"discord-file");
        fs::remove_file(path).unwrap();
    }
}
