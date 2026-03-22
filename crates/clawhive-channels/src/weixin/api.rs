use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use rand::Rng;
use reqwest::header::{HeaderMap, HeaderValue};
use tokio::sync::RwLock;

use super::types::*;

const DEFAULT_BASE_URL: &str = "https://ilinkai.weixin.qq.com";
const CDN_BASE_URL: &str = "https://novac2c.cdn.weixin.qq.com/c2c";
const LONG_POLL_TIMEOUT: Duration = Duration::from_secs(40);
const API_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// ILinkClient
// ---------------------------------------------------------------------------

pub struct ILinkClient {
    poll_client: reqwest::Client,
    api_client: reqwest::Client,
    session: WeixinSession,
    context_tokens: Arc<RwLock<HashMap<String, String>>>,
    sync_path: PathBuf,
}

impl ILinkClient {
    pub fn new(session: WeixinSession, data_dir: &Path) -> Self {
        let poll_client = reqwest::Client::builder()
            .timeout(LONG_POLL_TIMEOUT)
            .build()
            .expect("failed to build poll HTTP client");

        let api_client = reqwest::Client::builder()
            .timeout(API_TIMEOUT)
            .build()
            .expect("failed to build API HTTP client");

        let sync_path = data_dir.join("sync.json");

        Self {
            poll_client,
            api_client,
            session,
            context_tokens: Arc::new(RwLock::new(HashMap::new())),
            sync_path,
        }
    }

    /// Generate X-WECHAT-UIN header: random u32 → decimal string → base64.
    fn random_uin() -> String {
        let val: u32 = rand::thread_rng().gen();
        base64::engine::general_purpose::STANDARD.encode(val.to_string().as_bytes())
    }

    /// Build common auth headers for authenticated POST requests.
    fn auth_headers(&self) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert(
            "AuthorizationType",
            HeaderValue::from_static("ilink_bot_token"),
        );
        headers.insert(
            "X-WECHAT-UIN",
            HeaderValue::from_str(&Self::random_uin())
                .map_err(|e| anyhow!("invalid UIN header: {e}"))?,
        );
        let bearer = format!("Bearer {}", self.session.bot_token);
        headers.insert(
            "Authorization",
            HeaderValue::from_str(&bearer).map_err(|e| anyhow!("invalid auth header: {e}"))?,
        );
        Ok(headers)
    }

    fn api_url(&self, path: &str) -> String {
        format!("{}{path}", self.session.base_url)
    }

    // ── Long-poll ──

    pub async fn get_updates(&self, cursor: &str) -> Result<GetUpdatesResponse> {
        let body = GetUpdatesRequest {
            get_updates_buf: cursor.to_string(),
            base_info: BaseInfo::default(),
        };
        let resp = self
            .poll_client
            .post(self.api_url("/ilink/bot/getupdates"))
            .headers(self.auth_headers()?)
            .json(&body)
            .send()
            .await
            .context("getupdates request failed")?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("getupdates HTTP {status}: {text}"));
        }
        resp.json().await.context("getupdates parse failed")
    }

    // ── Send text ──

    pub async fn send_text(&self, to_user_id: &str, text: &str, context_token: &str) -> Result<()> {
        let body = SendMessageRequest {
            msg: OutgoingMessage {
                from_user_id: String::new(),
                to_user_id: to_user_id.to_string(),
                client_id: Self::generate_client_id(),
                message_type: 2,
                message_state: 2,
                context_token: context_token.to_string(),
                item_list: vec![MessageItem {
                    item_type: 1,
                    text_item: Some(TextItem {
                        text: text.to_string(),
                    }),
                    image_item: None,
                }],
            },
            base_info: BaseInfo::default(),
        };
        let resp = self
            .api_client
            .post(self.api_url("/ilink/bot/sendmessage"))
            .headers(self.auth_headers()?)
            .json(&body)
            .send()
            .await
            .context("sendmessage request failed")?;

        let result: SendMessageResponse = resp.json().await?;
        if result.ret != 0 {
            return Err(anyhow!(
                "sendmessage error: ret={}, errcode={}, msg={}",
                result.ret,
                result.errcode,
                result.errmsg
            ));
        }
        Ok(())
    }

    // ── Send image ──

    pub async fn send_image(
        &self,
        to_user_id: &str,
        image_bytes: &[u8],
        context_token: &str,
    ) -> Result<()> {
        use super::crypto::{aes_ecb_encrypt, aes_ecb_padded_size};
        use md5::Digest;

        let aes_key: [u8; 16] = rand::thread_rng().gen();
        let filekey = hex::encode(rand::thread_rng().gen::<[u8; 16]>());
        let rawsize = image_bytes.len() as u64;
        let rawfilemd5 = format!("{:x}", md5::Md5::digest(image_bytes));
        let filesize = aes_ecb_padded_size(rawsize);
        let encrypted = aes_ecb_encrypt(&aes_key, image_bytes)?;

        // Step 1: Get upload URL
        let upload_req = GetUploadUrlRequest {
            filekey: filekey.clone(),
            media_type: 1, // IMAGE
            to_user_id: to_user_id.to_string(),
            rawsize,
            rawfilemd5,
            filesize,
            no_need_thumb: true,
            aeskey: hex::encode(aes_key),
            base_info: BaseInfo::default(),
        };
        let resp = self
            .api_client
            .post(self.api_url("/ilink/bot/getuploadurl"))
            .headers(self.auth_headers()?)
            .json(&upload_req)
            .send()
            .await?;
        let upload_resp: GetUploadUrlResponse = resp.json().await?;
        if upload_resp.ret != 0 {
            return Err(anyhow!("getuploadurl error: ret={}", upload_resp.ret));
        }

        // Step 2: Upload encrypted bytes to CDN
        let cdn_url = format!(
            "{CDN_BASE_URL}/upload?encrypted_query_param={}&filekey={filekey}",
            upload_resp.upload_param
        );
        let cdn_resp = self
            .api_client
            .post(&cdn_url)
            .header("Content-Type", "application/octet-stream")
            .body(encrypted)
            .send()
            .await
            .context("CDN upload failed")?;

        if !cdn_resp.status().is_success() {
            return Err(anyhow!("CDN upload HTTP {}", cdn_resp.status()));
        }
        let encrypt_query_param = cdn_resp
            .headers()
            .get("x-encrypted-param")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();

        // Step 3: Send message with image reference
        let aes_key_b64 =
            base64::engine::general_purpose::STANDARD.encode(hex::encode(aes_key).as_bytes());
        let body = SendMessageRequest {
            msg: OutgoingMessage {
                from_user_id: String::new(),
                to_user_id: to_user_id.to_string(),
                client_id: Self::generate_client_id(),
                message_type: 2,
                message_state: 2,
                context_token: context_token.to_string(),
                item_list: vec![MessageItem {
                    item_type: 2,
                    text_item: None,
                    image_item: Some(ImageItem {
                        media: Some(CdnMedia {
                            encrypt_query_param,
                            aes_key: aes_key_b64,
                            encrypt_type: 1,
                        }),
                        aeskey: None,
                        mid_size: filesize,
                    }),
                }],
            },
            base_info: BaseInfo::default(),
        };
        let resp = self
            .api_client
            .post(self.api_url("/ilink/bot/sendmessage"))
            .headers(self.auth_headers()?)
            .json(&body)
            .send()
            .await?;
        let result: SendMessageResponse = resp.json().await?;
        if result.ret != 0 {
            return Err(anyhow!("send image error: ret={}", result.ret));
        }
        Ok(())
    }

    // ── Download + decrypt inbound image ──

    pub async fn download_image(&self, msg_image: &ImageItem) -> Result<Vec<u8>> {
        use super::crypto::{aes_ecb_decrypt, parse_aes_key};

        let media = msg_image
            .media
            .as_ref()
            .ok_or_else(|| anyhow!("image has no media field"))?;

        // Determine AES key: prefer hex aeskey field, fall back to media.aes_key
        let key = if let Some(hex_key) = &msg_image.aeskey {
            let decoded = hex::decode(hex_key).context("invalid hex aeskey")?;
            if decoded.len() != 16 {
                return Err(anyhow!(
                    "hex aeskey is {} bytes, expected 16",
                    decoded.len()
                ));
            }
            decoded
        } else {
            parse_aes_key(&media.aes_key)?
        };

        let url = format!(
            "{CDN_BASE_URL}/download?encrypted_query_param={}",
            media.encrypt_query_param
        );
        let resp = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()?
            .get(&url)
            .send()
            .await?;
        if !resp.status().is_success() {
            return Err(anyhow!("CDN download HTTP {}", resp.status()));
        }
        let ciphertext = resp.bytes().await?.to_vec();
        aes_ecb_decrypt(&key, &ciphertext)
    }

    // ── Typing indicator ──

    pub async fn send_typing(&self, user_id: &str, context_token: &str) -> Result<()> {
        // Get typing ticket
        let config_resp: GetConfigResponse = self
            .api_client
            .post(self.api_url("/ilink/bot/getconfig"))
            .headers(self.auth_headers()?)
            .json(&GetConfigRequest {
                ilink_user_id: user_id.to_string(),
                context_token: context_token.to_string(),
                base_info: BaseInfo::default(),
            })
            .send()
            .await?
            .json()
            .await?;

        if config_resp.typing_ticket.is_empty() {
            return Ok(());
        }

        // Send typing status
        let _ = self
            .api_client
            .post(self.api_url("/ilink/bot/sendtyping"))
            .headers(self.auth_headers()?)
            .json(&SendTypingRequest {
                ilink_user_id: user_id.to_string(),
                typing_ticket: config_resp.typing_ticket,
                status: 1,
                base_info: BaseInfo::default(),
            })
            .send()
            .await;
        Ok(())
    }

    // ── Context token management ──

    pub async fn set_context_token(&self, user_id: &str, token: &str) {
        self.context_tokens
            .write()
            .await
            .insert(user_id.to_string(), token.to_string());
    }

    pub async fn get_context_token(&self, user_id: &str) -> Option<String> {
        self.context_tokens.read().await.get(user_id).cloned()
    }

    // ── Sync cursor persistence ──

    pub fn load_cursor(&self) -> String {
        std::fs::read_to_string(&self.sync_path)
            .ok()
            .and_then(|s| {
                serde_json::from_str::<serde_json::Value>(&s)
                    .ok()?
                    .get("get_updates_buf")?
                    .as_str()
                    .map(String::from)
            })
            .unwrap_or_default()
    }

    pub fn save_cursor(&self, cursor: &str) {
        let json = serde_json::json!({ "get_updates_buf": cursor });
        let _ = std::fs::write(
            &self.sync_path,
            serde_json::to_string(&json).unwrap_or_default(),
        );
    }

    // ── Helpers ──

    fn generate_client_id() -> String {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let rand_hex = hex::encode(rand::thread_rng().gen::<[u8; 4]>());
        format!("clawhive:{ts}-{rand_hex}")
    }
}

// ---------------------------------------------------------------------------
// QR Code Login (standalone, before ILinkClient is created)
// ---------------------------------------------------------------------------

pub async fn qr_login() -> Result<WeixinSession> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(40))
        .build()?;

    // Step 1: Get QR code
    let resp: QrCodeResponse = http
        .get(format!(
            "{DEFAULT_BASE_URL}/ilink/bot/get_bot_qrcode?bot_type=3"
        ))
        .send()
        .await?
        .json()
        .await
        .context("failed to get QR code")?;

    render_qr_terminal(&resp.qrcode_img_content);
    tracing::info!("scan the QR code with WeChat to log in");

    // Step 2: Poll for confirmation (up to 3 QR refreshes)
    let mut qrcode_token = resp.qrcode;
    let mut refreshes = 0u32;

    loop {
        let status_url =
            format!("{DEFAULT_BASE_URL}/ilink/bot/get_qrcode_status?qrcode={qrcode_token}");
        let status: QrCodeStatusResponse = http
            .get(&status_url)
            .header("iLink-App-ClientVersion", "1")
            .send()
            .await?
            .json()
            .await?;

        match status.status.as_str() {
            "confirmed" => {
                let session = WeixinSession {
                    bot_token: status
                        .bot_token
                        .ok_or_else(|| anyhow!("confirmed but no bot_token"))?,
                    base_url: status
                        .baseurl
                        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
                    bot_id: status
                        .ilink_bot_id
                        .ok_or_else(|| anyhow!("confirmed but no ilink_bot_id"))?,
                    user_id: status
                        .ilink_user_id
                        .ok_or_else(|| anyhow!("confirmed but no ilink_user_id"))?,
                    saved_at: chrono::Utc::now().to_rfc3339(),
                };
                tracing::info!(bot_id = %session.bot_id, "weixin login successful");
                return Ok(session);
            }
            "expired" => {
                refreshes += 1;
                if refreshes >= 3 {
                    return Err(anyhow!("QR code expired 3 times, aborting login"));
                }
                tracing::warn!("QR code expired, refreshing ({refreshes}/3)");
                let new_resp: QrCodeResponse = http
                    .get(format!(
                        "{DEFAULT_BASE_URL}/ilink/bot/get_bot_qrcode?bot_type=3"
                    ))
                    .send()
                    .await?
                    .json()
                    .await?;
                render_qr_terminal(&new_resp.qrcode_img_content);
                qrcode_token = new_resp.qrcode;
            }
            // "wait" = not scanned, "scaned" = scanned but not confirmed (sic — protocol typo)
            "wait" | "scaned" => {}
            other => {
                tracing::warn!(status = other, "unexpected QR status");
            }
        }
    }
}

/// Render a URL as a QR code in the terminal using Unicode block characters.
fn render_qr_terminal(url: &str) {
    use qrcode::QrCode;

    let code = match QrCode::new(url.as_bytes()) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(error = %e, "failed to generate QR code");
            tracing::info!(url = %url, "scan this URL manually");
            return;
        }
    };

    let string = code
        .render::<char>()
        .quiet_zone(true)
        .module_dimensions(2, 1)
        .build();

    // Print to stdout — user-facing CLI output during login
    for line in string.lines() {
        println!("{line}");
    }
    println!();
    tracing::info!(url = %url, "scan the QR code above with WeChat");
}

// ---------------------------------------------------------------------------
// Session persistence
// ---------------------------------------------------------------------------

pub fn save_session(path: &Path, session: &WeixinSession) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(session)?;
    std::fs::write(path, json).context("failed to save weixin session")
}

pub fn load_session(path: &Path) -> Option<WeixinSession> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}
