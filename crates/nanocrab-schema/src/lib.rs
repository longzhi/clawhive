use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundMessage {
    pub trace_id: Uuid,
    pub channel_type: String,
    pub connector_id: String,
    pub conversation_scope: String,
    pub user_scope: String,
    pub text: String,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboundMessage {
    pub trace_id: Uuid,
    pub channel_type: String,
    pub connector_id: String,
    pub conversation_scope: String,
    pub text: String,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    Inbound(InboundMessage),
    Outbound(OutboundMessage),
}
