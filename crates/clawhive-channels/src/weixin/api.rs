use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use rand::Rng;
use reqwest::header::{HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use super::crypto;
use super::types::*;

const CDN_BASE: &str = "https://novac2c.cdn.weixin.qq.com/c2c";
const ILINK_QR_BASE: &str = "https://oai.weixin.qq.com/ilink";

// ---------------------------------------------------------------------------
// Session persistence
// ---------------------------------------------------------------------------

pub use super::types::WeixinSession;

/// Save a session to a JSON file.
pub fn save_session(session: &WeixinSession, path: &Path) -> Result<()> {
    let json = serde_json::to_string_pretty(session)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, json)?;
    tracing::info!(path = %path.display(), "weixin session saved");
    Ok(())
}

/// Load a session from a JSON file.
pub fn load_session(path: &Path) -> Result<WeixinSession> {
    let json = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read session from {}", path.display()))?;
    let session: WeixinSession =
        serde_json::from_str(&json).context("failed to parse weixin session")?;
    Ok(session)
}

// ---------------------------------------------------------------------------
// Cursor persistence
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CursorState {
    cursor: String,
}

// ---------------------------------------------------------------------------
// ILinkClient
// ---------------------------------------------------------------------------

pub struct ILinkClient {
    /// HTTP client with 40s timeout for long-poll requests.
    poll_client: reqwest::Client,
    /// HTTP client with 30s timeout for normal API calls.
    api_client: reqwest::Client,
    session: WeixinSession,
    context_tokens: Arc<RwLock<HashMap<String, String>>>,
    sync_path: PathBuf,
}

impl ILinkClient {
    pub fn new(session: WeixinSession, data_dir: &Path) -> Self {
        let poll_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(40))
            .build()
            .expect("failed to build poll HTTP client");

        let api_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("failed to build API HTTP client");

        let sync_path = data_dir.join("weixin_cursor.json");

        Self {
            poll_client,
            api_client,
            session,
            context_tokens: Arc::new(RwLock::new(HashMap::new())),
            sync_path,
        }
    }

    /// Generate a random X-WECHAT-UIN header value.
    fn random_uin() -> String {
        let mut rng = rand::thread_rng();
        let n: u32 = rng.gen();
        let decimal = n.to_string();
        let engine = base64::engine::general_purpose::STANDARD;
        engine.encode(decimal.as_bytes())
    }

    /// Build common auth headers for iLink API requests.
    fn auth_headers(&self) -> Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        headers.insert("AuthorizationType", HeaderValue::from_static("bot_token"));
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

    /// Long-poll for new messages.
    pub async fn get_updates(&self, cursor: Option<&str>) -> Result<GetUpdatesResponse> {
        let url = format!("{}/ilink/bot/getupdates", self.session.base_url);
        let body = GetUpdatesRequest {
            base_info: BaseInfo::default(),
            cursor: cursor.map(|s| s.to_string()),
        };

        let resp = self
            .poll_client
            .post(&url)
            .headers(self.auth_headers()?)
            .json(&body)
            .send()
            .await
            .context("get_updates request failed")?;

        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            return Err(anyhow!("get_updates HTTP {status}: {text}"));
        }

        let data: GetUpdatesResponse = resp.json().await.context("get_updates parse failed")?;
        Ok(data)
    }

    /// Send a text message to a user.
    pub async fn send_text(
        &self,
        to_user_id: &str,
        text: &str,
        context_token: Option<&str>,
    ) -> Result<()> {
        let url = format!("{}/ilink/bot/sendmessage", self.session.base_url);
        let body = SendMessageRequest {
            base_info: BaseInfo::default(),
            to_user_id: to_user_id.to_string(),
            message: OutgoingMessage {
                item_list: vec![OutgoingItem {
                    item_type: "text".to_string(),
                    text_item: Some(OutgoingTextItem {
                        content: text.to_string(),
                    }),
                    image_item: None,
                }],
            },
            context_token: context_token.map(|s| s.to_string()),
        };

        let resp = self
            .api_client
            .post(&url)
            .headers(self.auth_headers()?)
            .json(&body)
            .send()
            .await
            .context("send_text request failed")?;

        let status = resp.status();
        let data: SendMessageResponse = resp.json().await.context("send_text parse failed")?;
        if let Some(code) = data.errcode {
            if code != 0 {
                return Err(anyhow!(
                    "send_text failed (HTTP {status}): code={code}, msg={}",
                    data.errmsg.unwrap_or_default()
                ));
            }
        }
        Ok(())
    }

    /// Send an image message: encrypt, upload to CDN, then send via API.
    pub async fn send_image(
        &self,
        to_user_id: &str,
        image_bytes: &[u8],
        context_token: Option<&str>,
    ) -> Result<()> {
        // 1. Generate AES key and encrypt
        let mut rng = rand::thread_rng();
        let mut aes_key = [0u8; 16];
        rng.fill(&mut aes_key);

        let encrypted = crypto::aes_ecb_encrypt(&aes_key, image_bytes)?;
        let file_size = image_bytes.len() as u64;
        let encrypted_size = encrypted.len() as u64;

        // 2. Get upload URL
        let upload_url_api = format!("{}/ilink/bot/getuploadurl", self.session.base_url);
        let upload_req = GetUploadUrlRequest {
            base_info: BaseInfo::default(),
            file_type: "image".to_string(),
            file_name: format!("{}.png", Self::generate_client_id()),
            file_size: encrypted_size,
        };

        let resp = self
            .api_client
            .post(&upload_url_api)
            .headers(self.auth_headers()?)
            .json(&upload_req)
            .send()
            .await
            .context("getuploadurl request failed")?;

        let upload_info: GetUploadUrlResponse =
            resp.json().await.context("getuploadurl parse failed")?;

        // 3. Upload encrypted bytes to CDN
        let cdn_upload_url = format!(
            "{CDN_BASE}/upload?encrypted_query_param={}&filekey={}",
            upload_info.upload_param, upload_info.filekey
        );
        let upload_resp = self
            .api_client
            .post(&cdn_upload_url)
            .header("Content-Type", "application/octet-stream")
            .body(encrypted)
            .send()
            .await
            .context("CDN upload failed")?;

        let encrypted_param = upload_resp
            .headers()
            .get("x-encrypted-param")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        // 4. Send image message
        let engine = base64::engine::general_purpose::STANDARD;
        let aes_key_b64 = engine.encode(aes_key);

        let url = format!("{}/ilink/bot/sendmessage", self.session.base_url);
        let body = SendMessageRequest {
            base_info: BaseInfo::default(),
            to_user_id: to_user_id.to_string(),
            message: OutgoingMessage {
                item_list: vec![OutgoingItem {
                    item_type: "image".to_string(),
                    text_item: None,
                    image_item: Some(OutgoingImageItem {
                        url: format!(
                            "{CDN_BASE}/download?encrypted_query_param={}",
                            encrypted_param
                        ),
                        encrypted_param: Some(encrypted_param),
                        encrypted_aes_key: Some(aes_key_b64),
                        file_size: Some(file_size),
                        encrypted_file_size: Some(encrypted_size),
                    }),
                }],
            },
            context_token: context_token.map(|s| s.to_string()),
        };

        let resp = self
            .api_client
            .post(&url)
            .headers(self.auth_headers()?)
            .json(&body)
            .send()
            .await
            .context("send_image sendmessage failed")?;

        let data: SendMessageResponse = resp
            .json()
            .await
            .context("send_image sendmessage parse failed")?;
        if let Some(code) = data.errcode {
            if code != 0 {
                return Err(anyhow!(
                    "send_image failed: code={code}, msg={}",
                    data.errmsg.unwrap_or_default()
                ));
            }
        }
        Ok(())
    }

    /// Download and decrypt an image from CDN.
    pub async fn download_image(&self, image: &ImageItem) -> Result<Vec<u8>> {
        let media = image
            .media_info
            .as_ref()
            .ok_or_else(|| anyhow!("image has no media_info"))?;

        let query_param = media
            .encrypt_query_param
            .as_deref()
            .ok_or_else(|| anyhow!("image media_info missing encrypt_query_param"))?;

        let download_url = format!("{CDN_BASE}/download?encrypted_query_param={query_param}");

        let resp = reqwest::get(&download_url)
            .await
            .context("CDN download failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("CDN download HTTP {status}: {body}"));
        }

        let encrypted_bytes = resp.bytes().await?.to_vec();

        // Decrypt if AES key is present
        if let Some(aes_key_b64) = &media.aes_key {
            let key = crypto::parse_aes_key(aes_key_b64)?;
            let decrypted = crypto::aes_ecb_decrypt(&key, &encrypted_bytes)?;
            Ok(decrypted)
        } else {
            // No encryption
            Ok(encrypted_bytes)
        }
    }

    /// Send a typing indicator to a user.
    pub async fn send_typing(&self, user_id: &str, context_token: &str) -> Result<()> {
        // First call getconfig (required before sendtyping)
        let config_url = format!("{}/ilink/bot/getconfig", self.session.base_url);
        let config_body = GetConfigRequest {
            base_info: BaseInfo::default(),
        };

        let _config_resp = self
            .api_client
            .post(&config_url)
            .headers(self.auth_headers()?)
            .json(&config_body)
            .send()
            .await
            .context("getconfig request failed")?;

        // Send typing indicator
        let typing_url = format!("{}/ilink/bot/sendtyping", self.session.base_url);
        let typing_body = SendTypingRequest {
            base_info: BaseInfo::default(),
            to_user_id: user_id.to_string(),
            context_token: context_token.to_string(),
        };

        self.api_client
            .post(&typing_url)
            .headers(self.auth_headers()?)
            .json(&typing_body)
            .send()
            .await
            .context("sendtyping request failed")?;

        Ok(())
    }

    /// Set context_token for a user.
    pub async fn set_context_token(&self, user_id: &str, token: &str) {
        let mut guard = self.context_tokens.write().await;
        guard.insert(user_id.to_string(), token.to_string());
    }

    /// Get cached context_token for a user.
    pub async fn get_context_token(&self, user_id: &str) -> Option<String> {
        let guard = self.context_tokens.read().await;
        guard.get(user_id).cloned()
    }

    /// Load the persisted cursor for long-polling.
    pub fn load_cursor(&self) -> Option<String> {
        let data = std::fs::read_to_string(&self.sync_path).ok()?;
        let state: CursorState = serde_json::from_str(&data).ok()?;
        if state.cursor.is_empty() {
            None
        } else {
            Some(state.cursor)
        }
    }

    /// Persist the cursor for long-polling.
    pub fn save_cursor(&self, cursor: &str) -> Result<()> {
        if let Some(parent) = self.sync_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let state = CursorState {
            cursor: cursor.to_string(),
        };
        let json = serde_json::to_string(&state)?;
        std::fs::write(&self.sync_path, json)?;
        Ok(())
    }

    /// Generate a unique client ID.
    pub fn generate_client_id() -> String {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let mut rng = rand::thread_rng();
        let r: u64 = rng.gen();
        format!("clawhive:{ts}-{:016x}", r)
    }
}

// ---------------------------------------------------------------------------
// QR login flow
// ---------------------------------------------------------------------------

/// Perform QR code login and return a session.
pub async fn qr_login() -> Result<WeixinSession> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()?;

    let mut attempts = 0;
    const MAX_REFRESHES: u32 = 3;

    loop {
        attempts += 1;
        if attempts > MAX_REFRESHES {
            return Err(anyhow!(
                "QR login failed after {MAX_REFRESHES} refresh attempts"
            ));
        }

        // 1. Get QR code
        let qr_resp: QrCodeResponse = client
            .post(format!("{ILINK_QR_BASE}/qrlogin/getqrcode"))
            .json(&serde_json::json!({ "base_info": BaseInfo::default() }))
            .send()
            .await
            .context("getqrcode request failed")?
            .json()
            .await
            .context("getqrcode parse failed")?;

        tracing::info!(uuid = %qr_resp.uuid, "weixin QR code generated");

        // 2. Render QR in terminal
        render_qr_terminal(&qr_resp.qr_code_url);

        // 3. Poll for scan status
        let mut scanned = false;
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;

            let status_resp: QrCodeStatusResponse = client
                .post(format!("{ILINK_QR_BASE}/qrlogin/getstatus"))
                .json(&serde_json::json!({
                    "base_info": BaseInfo::default(),
                    "uuid": qr_resp.uuid,
                }))
                .send()
                .await
                .context("getstatus request failed")?
                .json()
                .await
                .context("getstatus parse failed")?;

            match status_resp.status.as_str() {
                "waiting" => {
                    if !scanned {
                        tracing::debug!("waiting for QR scan...");
                    }
                }
                "scanned" => {
                    if !scanned {
                        tracing::info!("QR code scanned, waiting for confirmation...");
                        scanned = true;
                    }
                }
                "confirmed" => {
                    let session = WeixinSession {
                        bot_token: status_resp
                            .bot_token
                            .ok_or_else(|| anyhow!("confirmed but no bot_token"))?,
                        base_url: status_resp
                            .base_url
                            .ok_or_else(|| anyhow!("confirmed but no base_url"))?,
                        bot_id: status_resp
                            .bot_id
                            .ok_or_else(|| anyhow!("confirmed but no bot_id"))?,
                        user_id: status_resp
                            .user_id
                            .ok_or_else(|| anyhow!("confirmed but no user_id"))?,
                        saved_at: Some(chrono::Utc::now().to_rfc3339()),
                    };
                    tracing::info!(bot_id = %session.bot_id, "weixin login confirmed");
                    return Ok(session);
                }
                "expired" => {
                    tracing::warn!(
                        "QR code expired, refreshing (attempt {attempts}/{MAX_REFRESHES})"
                    );
                    break; // Break inner loop → refresh QR
                }
                other => {
                    tracing::warn!(status = %other, "unexpected QR status");
                }
            }
        }
    }
}

/// Render a QR code in the terminal using Unicode block characters.
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
        .quiet_zone(false)
        .module_dimensions(2, 1)
        .build();

    // Print directly to stdout — this is user-facing CLI output during login
    for line in string.lines() {
        println!("{line}");
    }
    println!();
    tracing::info!(url = %url, "scan the QR code above to log in");
}
