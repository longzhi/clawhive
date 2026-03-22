use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Common
// ---------------------------------------------------------------------------

/// Base info included in every POST request body.
#[derive(Debug, Clone, Serialize)]
pub struct BaseInfo {
    pub channel_version: String,
}

impl Default for BaseInfo {
    fn default() -> Self {
        Self {
            channel_version: "1.0.2".to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// QR Code Login
// ---------------------------------------------------------------------------

/// Response from GET /ilink/bot/get_bot_qrcode?bot_type=3
#[derive(Debug, Clone, Deserialize)]
pub struct QrCodeResponse {
    pub qrcode: String,
    pub qrcode_img_content: String,
}

/// Response from GET /ilink/bot/get_qrcode_status?qrcode=<token>
#[derive(Debug, Clone, Deserialize)]
pub struct QrCodeStatusResponse {
    pub status: String,
    #[serde(default)]
    pub bot_token: Option<String>,
    #[serde(default)]
    pub ilink_bot_id: Option<String>,
    #[serde(default)]
    pub baseurl: Option<String>,
    #[serde(default)]
    pub ilink_user_id: Option<String>,
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
    pub saved_at: String,
}

// ---------------------------------------------------------------------------
// GetUpdates (long-polling)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct GetUpdatesRequest {
    pub get_updates_buf: String,
    pub base_info: BaseInfo,
}

#[derive(Debug, Deserialize)]
pub struct GetUpdatesResponse {
    #[serde(default)]
    pub ret: i32,
    #[serde(default)]
    pub errcode: i32,
    #[serde(default)]
    pub errmsg: String,
    #[serde(default)]
    pub msgs: Vec<WeixinMessage>,
    #[serde(default)]
    pub get_updates_buf: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WeixinMessage {
    #[serde(default)]
    pub message_id: u64,
    #[serde(default)]
    pub from_user_id: String,
    #[serde(default)]
    pub to_user_id: String,
    #[serde(default)]
    pub create_time_ms: u64,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub group_id: String,
    /// 1 = user message, 2 = bot message
    #[serde(default)]
    pub message_type: u32,
    /// 0 = new, 1 = generating, 2 = finish
    #[serde(default)]
    pub message_state: u32,
    #[serde(default)]
    pub item_list: Vec<MessageItem>,
    #[serde(default)]
    pub context_token: String,
}

// ---------------------------------------------------------------------------
// Message items
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageItem {
    /// 1 = text, 2 = image, 3 = voice, 4 = file, 5 = video
    #[serde(rename = "type")]
    pub item_type: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_item: Option<TextItem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_item: Option<ImageItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextItem {
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageItem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media: Option<CdnMedia>,
    /// Hex-encoded AES key (inbound images, preferred over media.aes_key)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aeskey: Option<String>,
    #[serde(default)]
    pub mid_size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CdnMedia {
    #[serde(default)]
    pub encrypt_query_param: String,
    #[serde(default)]
    pub aes_key: String,
    #[serde(default)]
    pub encrypt_type: u32,
}

// ---------------------------------------------------------------------------
// SendMessage
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct SendMessageRequest {
    pub msg: OutgoingMessage,
    pub base_info: BaseInfo,
}

#[derive(Debug, Serialize)]
pub struct OutgoingMessage {
    pub from_user_id: String,
    pub to_user_id: String,
    pub client_id: String,
    /// Always 2 for bot messages
    pub message_type: u32,
    /// 0 = new, 1 = generating, 2 = finish
    pub message_state: u32,
    pub context_token: String,
    pub item_list: Vec<MessageItem>,
}

#[derive(Debug, Deserialize)]
pub struct SendMessageResponse {
    #[serde(default)]
    pub ret: i32,
    #[serde(default)]
    pub errcode: i32,
    #[serde(default)]
    pub errmsg: String,
}

// ---------------------------------------------------------------------------
// CDN Upload
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct GetUploadUrlRequest {
    pub filekey: String,
    /// 1 = image, 2 = video, 3 = file, 4 = voice
    pub media_type: u32,
    pub to_user_id: String,
    pub rawsize: u64,
    pub rawfilemd5: String,
    pub filesize: u64,
    pub no_need_thumb: bool,
    pub aeskey: String,
    pub base_info: BaseInfo,
}

#[derive(Debug, Deserialize)]
pub struct GetUploadUrlResponse {
    #[serde(default)]
    pub ret: i32,
    #[serde(default)]
    pub upload_param: String,
}

// ---------------------------------------------------------------------------
// GetConfig + Typing indicator
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct GetConfigRequest {
    pub ilink_user_id: String,
    pub context_token: String,
    pub base_info: BaseInfo,
}

#[derive(Debug, Deserialize)]
pub struct GetConfigResponse {
    #[serde(default)]
    pub ret: i32,
    #[serde(default)]
    pub typing_ticket: String,
}

#[derive(Debug, Serialize)]
pub struct SendTypingRequest {
    pub ilink_user_id: String,
    pub typing_ticket: String,
    /// 1 = typing, 2 = cancel
    pub status: u32,
    pub base_info: BaseInfo,
}
