use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct FeishuEvent {
    pub schema: Option<String>,
    pub header: FeishuEventHeader,
    pub event: FeishuEventBody,
}

#[derive(Debug, Deserialize)]
pub struct FeishuEventHeader {
    pub event_id: String,
    pub event_type: String,
    pub create_time: String,
    pub token: String,
    pub app_id: String,
    pub tenant_key: String,
}

#[derive(Debug, Deserialize)]
pub struct FeishuEventBody {
    pub sender: FeishuSender,
    pub message: FeishuMessage,
}

#[derive(Debug, Deserialize)]
pub struct FeishuSender {
    pub sender_id: FeishuSenderId,
    pub sender_type: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct FeishuSenderId {
    pub open_id: String,
}

#[derive(Debug, Deserialize)]
pub struct FeishuMessage {
    pub message_id: String,
    pub chat_id: String,
    pub chat_type: String,
    pub message_type: String,
    pub content: String,
    #[serde(default)]
    pub mentions: Option<Vec<FeishuMention>>,
}

#[derive(Debug, Deserialize)]
pub struct FeishuMention {
    pub key: String,
    pub id: FeishuSenderId,
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct FeishuTextContent {
    pub text: String,
}

#[derive(Debug, Deserialize)]
pub struct FeishuImageContent {
    pub image_key: String,
}

#[derive(Debug, Deserialize)]
pub struct FeishuFileContent {
    pub file_key: String,
    #[serde(default)]
    pub file_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct FeishuCardActionEvent {
    pub schema: Option<String>,
    pub header: FeishuEventHeader,
    pub event: FeishuCardActionBody,
}

#[derive(Debug, Deserialize)]
pub struct FeishuCardActionBody {
    pub operator: FeishuCardOperator,
    #[serde(default)]
    pub token: Option<String>,
    pub action: FeishuCardAction,
    pub context: FeishuCardContext,
}

#[derive(Debug, Deserialize)]
pub struct FeishuCardOperator {
    pub open_id: String,
}

#[derive(Debug, Deserialize)]
pub struct FeishuCardAction {
    pub value: serde_json::Value,
    pub tag: String,
}

#[derive(Debug, Deserialize)]
pub struct FeishuCardContext {
    pub open_message_id: String,
    pub open_chat_id: String,
}

#[derive(Debug, Serialize)]
pub struct FeishuAckResponse {
    pub code: i32,
    pub headers: std::collections::HashMap<String, String>,
    pub data: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct WsEndpointResponse {
    pub code: i32,
    #[serde(default)]
    pub msg: String,
    pub data: Option<WsEndpointData>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct WsEndpointData {
    #[serde(rename = "URL")]
    pub url: String,
    pub client_config: Option<FeishuClientConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct FeishuClientConfig {
    #[serde(default = "default_reconnect_count")]
    pub reconnect_count: i32,
    #[serde(default = "default_reconnect_interval")]
    pub reconnect_interval: u64,
    #[serde(default = "default_reconnect_nonce")]
    pub reconnect_nonce: u64,
    #[serde(default = "default_ping_interval")]
    pub ping_interval: u64,
}

fn default_reconnect_count() -> i32 {
    -1
}
fn default_reconnect_interval() -> u64 {
    120
}
fn default_reconnect_nonce() -> u64 {
    30
}
fn default_ping_interval() -> u64 {
    120
}

impl Default for FeishuClientConfig {
    fn default() -> Self {
        Self {
            reconnect_count: -1,
            reconnect_interval: 120,
            reconnect_nonce: 30,
            ping_interval: 120,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_message_event_payload() {
        let payload = r#"{
            "schema": "2.0",
            "header": { "event_id": "evt_001", "event_type": "im.message.receive_v1", "create_time": "1609296809", "token": "test_token", "app_id": "cli_xxx", "tenant_key": "tenant_xxx" },
            "event": {
                "sender": { "sender_id": {"open_id": "ou_test"}, "sender_type": "user" },
                "message": { "message_id": "om_test", "chat_id": "oc_test", "chat_type": "group", "message_type": "text", "content": "{\"text\":\"hello\"}" }
            }
        }"#;
        let event: FeishuEvent = serde_json::from_str(payload).unwrap();
        assert_eq!(event.header.event_type, "im.message.receive_v1");
        let content: FeishuTextContent =
            serde_json::from_str(&event.event.message.content).unwrap();
        assert_eq!(content.text, "hello");
    }

    #[test]
    fn parse_image_content() {
        let c: FeishuImageContent =
            serde_json::from_str(r#"{"image_key": "img_v2_abc123"}"#).unwrap();
        assert_eq!(c.image_key, "img_v2_abc123");
    }

    #[test]
    fn parse_file_content() {
        let c: FeishuFileContent =
            serde_json::from_str(r#"{"file_key": "file_abc123", "file_name": "doc.pdf"}"#).unwrap();
        assert_eq!(c.file_key, "file_abc123");
        assert_eq!(c.file_name, Some("doc.pdf".to_string()));
    }

    #[test]
    fn parse_file_content_without_name() {
        let c: FeishuFileContent = serde_json::from_str(r#"{"file_key": "file_abc123"}"#).unwrap();
        assert_eq!(c.file_name, None);
    }

    #[test]
    fn parse_card_action_event() {
        let payload = r#"{
            "schema": "2.0",
            "header": { "event_id": "evt_card_001", "event_type": "card.action.trigger", "create_time": "1609296809", "token": "test_token", "app_id": "cli_xxx", "tenant_key": "tenant_xxx" },
            "event": {
                "operator": { "open_id": "ou_user1" }, "token": "card_token_123",
                "action": { "value": { "action": "approve_allow", "short_id": "abc123" }, "tag": "button" },
                "context": { "open_message_id": "om_msg1", "open_chat_id": "oc_chat1" }
            }
        }"#;
        let event: FeishuCardActionEvent = serde_json::from_str(payload).unwrap();
        assert_eq!(event.header.event_type, "card.action.trigger");
        assert_eq!(
            event
                .event
                .action
                .value
                .get("action")
                .unwrap()
                .as_str()
                .unwrap(),
            "approve_allow"
        );
    }
}
