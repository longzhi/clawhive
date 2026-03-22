use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Common
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BaseInfo {
    #[serde(default = "default_channel_version")]
    pub channel_version: String,
}

fn default_channel_version() -> String {
    "1.0.2".to_string()
}

impl Default for BaseInfo {
    fn default() -> Self {
        Self {
            channel_version: default_channel_version(),
        }
    }
}

// ---------------------------------------------------------------------------
// QR login flow
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct QrCodeResponse {
    pub uuid: String,
    pub qr_code_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct QrCodeStatusResponse {
    pub status: String,
    #[serde(default)]
    pub bot_token: Option<String>,
    #[serde(default)]
    pub base_url: Option<String>,
    #[serde(default)]
    pub bot_id: Option<String>,
    #[serde(default)]
    pub user_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Session persistence
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WeixinSession {
    pub bot_token: String,
    pub base_url: String,
    pub bot_id: String,
    pub user_id: String,
    #[serde(default)]
    pub saved_at: Option<String>,
}

// ---------------------------------------------------------------------------
// get_updates (long-polling)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetUpdatesRequest {
    pub base_info: BaseInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GetUpdatesResponse {
    #[serde(default)]
    pub messages: Vec<WeixinMessage>,
    #[serde(default)]
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WeixinMessage {
    pub from_user_id: String,
    #[serde(default)]
    pub message_type: Option<String>,
    #[serde(default)]
    pub message_state: Option<String>,
    #[serde(default)]
    pub item_list: Vec<MessageItem>,
    #[serde(default)]
    pub context_token: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MessageItem {
    #[serde(rename = "type")]
    pub item_type: String,
    #[serde(default)]
    pub text_item: Option<TextItem>,
    #[serde(default)]
    pub image_item: Option<ImageItem>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TextItem {
    pub content: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ImageItem {
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub media_info: Option<CdnMedia>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CdnMedia {
    #[serde(default)]
    pub encrypt_query_param: Option<String>,
    #[serde(default)]
    pub aes_key: Option<String>,
    #[serde(default)]
    pub file_size: Option<u64>,
}

// ---------------------------------------------------------------------------
// send_message
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendMessageRequest {
    pub base_info: BaseInfo,
    pub to_user_id: String,
    pub message: OutgoingMessage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutgoingMessage {
    pub item_list: Vec<OutgoingItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutgoingItem {
    #[serde(rename = "type")]
    pub item_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_item: Option<OutgoingTextItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_item: Option<OutgoingImageItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutgoingTextItem {
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutgoingImageItem {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encrypted_param: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encrypted_aes_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encrypted_file_size: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SendMessageResponse {
    #[serde(default)]
    pub errcode: Option<i64>,
    #[serde(default)]
    pub errmsg: Option<String>,
}

// ---------------------------------------------------------------------------
// CDN upload
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetUploadUrlRequest {
    pub base_info: BaseInfo,
    pub file_type: String,
    pub file_name: String,
    pub file_size: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GetUploadUrlResponse {
    pub upload_param: String,
    pub filekey: String,
}

// ---------------------------------------------------------------------------
// getconfig + typing indicator
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetConfigRequest {
    pub base_info: BaseInfo,
}

#[allow(dead_code)] // will be used when bot.rs handles getconfig responses
#[derive(Debug, Clone, Deserialize)]
pub struct GetConfigResponse {
    #[serde(default)]
    pub errcode: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendTypingRequest {
    pub base_info: BaseInfo,
    pub to_user_id: String,
    pub context_token: String,
}
