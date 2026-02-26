use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use clawhive_bus::{EventBus, Topic};
use clawhive_gateway::Gateway;
use clawhive_schema::{ActionKind, BusMessage, InboundMessage, OutboundMessage};
use uuid::Uuid;
use wacore::types::events::Event;
use waproto::whatsapp as wa;
use whatsapp_rust::bot::Bot;
use whatsapp_rust_sqlite_storage::SqliteStore;
use whatsapp_rust_tokio_transport::TokioWebSocketTransportFactory;
use whatsapp_rust_ureq_http_client::UreqHttpClient;

pub struct WhatsAppAdapter {
    connector_id: String,
}

impl WhatsAppAdapter {
    pub fn new(connector_id: impl Into<String>) -> Self {
        Self {
            connector_id: connector_id.into(),
        }
    }

    pub fn to_inbound(
        &self,
        chat_jid: &str,
        sender_jid: &str,
        text: &str,
        message_id: Option<String>,
    ) -> InboundMessage {
        InboundMessage {
            trace_id: Uuid::new_v4(),
            channel_type: "whatsapp".to_string(),
            connector_id: self.connector_id.clone(),
            conversation_scope: format!("chat:{chat_jid}"),
            user_scope: format!("user:{sender_jid}"),
            text: text.to_string(),
            at: Utc::now(),
            thread_id: None,
            is_mention: false,
            mention_target: None,
            message_id,
            attachments: vec![],
            group_context: None,
        }
    }

    pub fn render_outbound(&self, outbound: &OutboundMessage) -> String {
        format!(
            "[whatsapp:{}] {}",
            outbound.conversation_scope, outbound.text
        )
    }
}

/// Start the WhatsApp channel.
///
/// `db_path` is the path to the SQLite file used for WhatsApp session persistence.
pub async fn start_whatsapp(
    connector_id: String,
    db_path: PathBuf,
    gateway: Arc<Gateway>,
    bus: Arc<EventBus>,
) -> anyhow::Result<()> {
    let adapter = Arc::new(WhatsAppAdapter::new(&connector_id));

    // Spawn action listener for reactions/edit/delete
    let bus_clone = bus.clone();
    let connector_id_clone = connector_id.clone();
    tokio::spawn(spawn_action_listener(bus_clone, connector_id_clone));

    // Spawn delivery listener for scheduled messages
    let bus_clone = bus.clone();
    let connector_id_clone = connector_id.clone();
    tokio::spawn(spawn_delivery_listener(bus_clone, connector_id_clone));

    let db_str = db_path.to_string_lossy().to_string();
    let backend = Arc::new(SqliteStore::new(&db_str).await?);

    let gateway_for_bot = gateway.clone();
    let adapter_for_bot = adapter.clone();

    let mut bot = Bot::builder()
        .with_backend(backend)
        .with_transport_factory(TokioWebSocketTransportFactory::new())
        .with_http_client(UreqHttpClient::new())
        .on_event(move |event, _client| {
            let gateway = gateway_for_bot.clone();
            let adapter = adapter_for_bot.clone();

            async move {
                match event {
                    Event::PairingQrCode { code, .. } => {
                        tracing::info!("WhatsApp QR code for pairing:\n{}", code);
                        // TODO: render QR to terminal or deliver via bus
                    }
                    Event::PairSuccess { .. } => {
                        tracing::info!("WhatsApp pairing successful!");
                    }
                    Event::Message(msg, info) => {
                        // Extract text from message
                        let text = extract_message_text(&msg);
                        if text.is_empty() {
                            return;
                        }

                        let chat_jid = info.source.chat.to_string();
                        let sender_jid = info.source.sender.to_string();
                        let msg_id = Some(info.id.clone());

                        let inbound = adapter.to_inbound(&chat_jid, &sender_jid, &text, msg_id);

                        tracing::debug!(
                            "WhatsApp message from {} in {}: {}",
                            sender_jid,
                            chat_jid,
                            text
                        );

                        match gateway.handle_inbound(inbound).await {
                            Ok(outbound) => {
                                tracing::debug!("WhatsApp reply: {}", adapter.render_outbound(&outbound));
                                // TODO: send reply via client
                                // client.send_text_message(chat_jid, &outbound.text).await
                            }
                            Err(err) => {
                                tracing::error!("Gateway error for WhatsApp message: {err}");
                            }
                        }
                    }
                    Event::Connected(_) => {
                        tracing::info!("WhatsApp connected");
                    }
                    Event::Disconnected(_) => {
                        tracing::warn!("WhatsApp disconnected");
                    }
                    _ => {}
                }
            }
        })
        .build()
        .await?;

    tracing::info!("Starting WhatsApp channel (connector: {})", connector_id);
    bot.run().await?.await?;

    Ok(())
}

/// Extract text content from a WhatsApp message.
fn extract_message_text(msg: &wa::Message) -> String {
    if let Some(ref conv) = msg.conversation {
        return conv.to_string();
    }
    if let Some(ref ext) = msg.extended_text_message {
        if let Some(ref text) = ext.text {
            return text.to_string();
        }
    }
    String::new()
}

/// Parse chat JID from conversation_scope (format: "chat:xxx@s.whatsapp.net")
fn parse_chat_jid(conversation_scope: &str) -> Option<&str> {
    conversation_scope.strip_prefix("chat:")
}

/// Listen for ActionReady events (reactions, edits, deletes)
async fn spawn_action_listener(bus: Arc<EventBus>, connector_id: String) {
    let mut rx = bus.subscribe(Topic::ActionReady).await;
    while let Some(msg) = rx.recv().await {
        let BusMessage::ActionReady { action } = msg else {
            continue;
        };

        if action.channel_type != "whatsapp" || action.connector_id != connector_id {
            continue;
        }

        let Some(_chat_jid) = parse_chat_jid(&action.conversation_scope) else {
            tracing::warn!("Could not parse WhatsApp chat JID: {}", action.conversation_scope);
            continue;
        };

        match action.action {
            ActionKind::React { ref emoji } => {
                tracing::debug!("WhatsApp reaction: {emoji} (TODO: implement)");
                // TODO: client.send_reaction(chat_jid, msg_id, emoji)
            }
            ActionKind::Edit { ref new_text } => {
                tracing::debug!("WhatsApp edit: {new_text} (TODO: implement)");
                // TODO: client.edit_message(chat_jid, msg_id, new_text)
            }
            ActionKind::Delete => {
                tracing::debug!("WhatsApp delete (TODO: implement)");
                // TODO: client.revoke_message(chat_jid, msg_id)
            }
            ActionKind::Unreact { .. } => {
                tracing::debug!("WhatsApp unreact (TODO: implement)");
            }
        }
    }
}

/// Listen for DeliverAnnounce events (scheduled task delivery)
async fn spawn_delivery_listener(bus: Arc<EventBus>, connector_id: String) {
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

        if channel_type != "whatsapp" || msg_connector_id != connector_id {
            continue;
        }

        let Some(_chat_jid) = parse_chat_jid(&conversation_scope) else {
            tracing::warn!("Could not parse WhatsApp chat JID: {}", conversation_scope);
            continue;
        };

        tracing::info!("WhatsApp delivery: {} → {} (TODO: send)", _chat_jid, text);
        // TODO: client.send_text_message(chat_jid, &text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_to_inbound_sets_fields() {
        let adapter = WhatsAppAdapter::new("wa_main");
        let msg = adapter.to_inbound("123@s.whatsapp.net", "456@s.whatsapp.net", "hello", None);
        assert_eq!(msg.channel_type, "whatsapp");
        assert_eq!(msg.connector_id, "wa_main");
        assert_eq!(msg.conversation_scope, "chat:123@s.whatsapp.net");
        assert_eq!(msg.user_scope, "user:456@s.whatsapp.net");
        assert_eq!(msg.text, "hello");
    }

    #[test]
    fn render_outbound_formats_correctly() {
        let adapter = WhatsAppAdapter::new("wa_main");
        let outbound = OutboundMessage {
            trace_id: Uuid::new_v4(),
            channel_type: "whatsapp".into(),
            connector_id: "wa_main".into(),
            conversation_scope: "chat:123@s.whatsapp.net".into(),
            text: "hi there".into(),
            at: Utc::now(),
            reply_to: None,
            attachments: vec![],
        };
        let rendered = adapter.render_outbound(&outbound);
        assert_eq!(rendered, "[whatsapp:chat:123@s.whatsapp.net] hi there");
    }

    #[test]
    fn parse_chat_jid_works() {
        assert_eq!(
            parse_chat_jid("chat:123@s.whatsapp.net"),
            Some("123@s.whatsapp.net")
        );
        assert_eq!(parse_chat_jid("invalid"), None);
    }
}
