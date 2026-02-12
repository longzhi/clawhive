use chrono::Utc;
use nanocrab_schema::{InboundMessage, OutboundMessage};
use uuid::Uuid;

pub struct TelegramAdapter {
    pub connector_id: String,
}

impl TelegramAdapter {
    pub fn new(connector_id: impl Into<String>) -> Self {
        Self {
            connector_id: connector_id.into(),
        }
    }

    pub fn to_inbound(&self, chat_id: i64, user_id: i64, text: impl Into<String>) -> InboundMessage {
        InboundMessage {
            trace_id: Uuid::new_v4(),
            channel_type: "telegram".to_string(),
            connector_id: self.connector_id.clone(),
            conversation_scope: format!("chat:{chat_id}"),
            user_scope: format!("user:{user_id}"),
            text: text.into(),
            at: Utc::now(),
        }
    }

    pub fn render_outbound(&self, outbound: &OutboundMessage) -> String {
        format!("[telegram:{}] {}", outbound.conversation_scope, outbound.text)
    }
}
