use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::sync::RwLock;

use super::types::{FeishuClientConfig, WsEndpointResponse};

const FEISHU_BASE_URL: &str = "https://open.feishu.cn/open-apis";
const FEISHU_WS_ENDPOINT: &str = "https://open.feishu.cn/callback/ws/endpoint";

const TOKEN_PROACTIVE_REFRESH_AGE: Duration = Duration::from_secs(90 * 60);
const TOKEN_REFRESH_INTERVAL: Duration = Duration::from_secs(100 * 60);
const TOKEN_EXPIRED_CODE: i64 = 99991663;

struct TokenState {
    value: String,
    fetched_at: Instant,
}

pub struct FeishuClient {
    app_id: String,
    app_secret: String,
    token: Arc<RwLock<TokenState>>,
    http: reqwest::Client,
}

impl FeishuClient {
    pub fn new(app_id: impl Into<String>, app_secret: impl Into<String>) -> Self {
        Self {
            app_id: app_id.into(),
            app_secret: app_secret.into(),
            token: Arc::new(RwLock::new(TokenState {
                value: String::new(),
                fetched_at: Instant::now() - TOKEN_PROACTIVE_REFRESH_AGE - Duration::from_secs(1),
            })),
            http: reqwest::Client::new(),
        }
    }

    pub async fn get_ws_endpoint(&self) -> Result<(String, FeishuClientConfig)> {
        let resp = self
            .http
            .post(FEISHU_WS_ENDPOINT)
            .header("locale", "zh")
            .json(&serde_json::json!({
                "AppID": self.app_id,
                "AppSecret": self.app_secret,
            }))
            .send()
            .await?
            .json::<WsEndpointResponse>()
            .await?;

        if resp.code != 0 {
            anyhow::bail!(
                "feishu: failed to get ws endpoint: code={}, msg={}",
                resp.code,
                resp.msg
            );
        }

        let data = resp
            .data
            .ok_or_else(|| anyhow::anyhow!("feishu: ws endpoint response missing data"))?;
        let config = data.client_config.unwrap_or_default();
        Ok((data.url, config))
    }

    pub async fn refresh_token(&self) -> Result<()> {
        let resp = self
            .http
            .post(format!(
                "{FEISHU_BASE_URL}/auth/v3/tenant_access_token/internal"
            ))
            .json(&serde_json::json!({
                "app_id": self.app_id,
                "app_secret": self.app_secret,
            }))
            .send()
            .await?
            .json::<serde_json::Value>()
            .await?;

        let token = resp["tenant_access_token"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("feishu: missing tenant_access_token"))?;

        let mut guard = self.token.write().await;
        guard.value = token.to_string();
        guard.fetched_at = Instant::now();

        tracing::info!(
            target: "clawhive::channel::feishu",
            "refreshed tenant_access_token"
        );
        Ok(())
    }

    pub async fn get_bot_open_id(&self) -> Result<String> {
        let token = self.get_token().await;
        let resp = self
            .http
            .get(format!("{FEISHU_BASE_URL}/bot/v3/info/"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await?
            .json::<serde_json::Value>()
            .await?;

        resp.pointer("/bot/open_id")
            .and_then(|v| v.as_str())
            .map(String::from)
            .ok_or_else(|| anyhow::anyhow!("feishu: missing bot open_id in /bot/v3/info response"))
    }

    async fn get_token(&self) -> String {
        {
            let guard = self.token.read().await;
            if !guard.value.is_empty() && guard.fetched_at.elapsed() < TOKEN_PROACTIVE_REFRESH_AGE {
                return guard.value.clone();
            }
        }
        if let Err(e) = self.refresh_token().await {
            tracing::warn!(
                target: "clawhive::channel::feishu",
                error = %e,
                "proactive token refresh failed, using cached token"
            );
        }
        self.token.read().await.value.clone()
    }

    pub fn spawn_token_refresh(self: &Arc<Self>) {
        let client = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(TOKEN_REFRESH_INTERVAL).await;

                let mut backoff = Duration::from_secs(5);
                loop {
                    match client.refresh_token().await {
                        Ok(()) => break,
                        Err(e) => {
                            tracing::error!(
                                target: "clawhive::channel::feishu",
                                error = %e,
                                retry_secs = backoff.as_secs(),
                                "failed to refresh tenant_access_token, retrying"
                            );
                            tokio::time::sleep(backoff).await;
                            backoff = (backoff * 2).min(Duration::from_secs(60));
                        }
                    }
                }
            }
        });
    }

    fn is_token_expired(resp: &serde_json::Value) -> bool {
        resp.pointer("/code")
            .and_then(|v| v.as_i64())
            .is_some_and(|c| c == TOKEN_EXPIRED_CODE)
    }

    fn extract_api_error(resp: &serde_json::Value, method: &str) -> Option<anyhow::Error> {
        let code = resp.pointer("/code").and_then(|v| v.as_i64()).unwrap_or(0);
        if code != 0 {
            Some(anyhow::anyhow!("feishu: {method} failed: {resp}"))
        } else {
            None
        }
    }

    fn extract_message_id(resp: &serde_json::Value) -> String {
        resp.pointer("/data/message_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    }

    pub async fn send_message(&self, chat_id: &str, msg_type: &str, content: &str) -> Result<()> {
        let body = serde_json::json!({
            "receive_id": chat_id,
            "msg_type": msg_type,
            "content": content,
        });

        for attempt in 0..2 {
            let token = self.get_token().await;
            let resp: serde_json::Value = self
                .http
                .post(format!(
                    "{FEISHU_BASE_URL}/im/v1/messages?receive_id_type=chat_id"
                ))
                .header("Authorization", format!("Bearer {token}"))
                .json(&body)
                .send()
                .await?
                .json()
                .await?;

            if attempt == 0 && Self::is_token_expired(&resp) {
                self.refresh_token().await?;
                continue;
            }
            if let Some(e) = Self::extract_api_error(&resp, "send_message") {
                return Err(e);
            }
            return Ok(());
        }
        Ok(())
    }

    pub async fn reply_message(
        &self,
        message_id: &str,
        msg_type: &str,
        content: &str,
    ) -> Result<()> {
        let body = serde_json::json!({
            "msg_type": msg_type,
            "content": content,
        });

        for attempt in 0..2 {
            let token = self.get_token().await;
            let resp: serde_json::Value = self
                .http
                .post(format!(
                    "{FEISHU_BASE_URL}/im/v1/messages/{message_id}/reply"
                ))
                .header("Authorization", format!("Bearer {token}"))
                .json(&body)
                .send()
                .await?
                .json()
                .await?;

            if attempt == 0 && Self::is_token_expired(&resp) {
                self.refresh_token().await?;
                continue;
            }
            if let Some(e) = Self::extract_api_error(&resp, "reply_message") {
                return Err(e);
            }
            return Ok(());
        }
        Ok(())
    }

    pub async fn reply_message_with_id(
        &self,
        message_id: &str,
        msg_type: &str,
        content: &str,
    ) -> Result<String> {
        let body = serde_json::json!({
            "msg_type": msg_type,
            "content": content,
        });

        for attempt in 0..2 {
            let token = self.get_token().await;
            let resp: serde_json::Value = self
                .http
                .post(format!(
                    "{FEISHU_BASE_URL}/im/v1/messages/{message_id}/reply"
                ))
                .header("Authorization", format!("Bearer {token}"))
                .json(&body)
                .send()
                .await?
                .json()
                .await?;

            if attempt == 0 && Self::is_token_expired(&resp) {
                self.refresh_token().await?;
                continue;
            }
            if let Some(e) = Self::extract_api_error(&resp, "reply_message_with_id") {
                return Err(e);
            }
            return Ok(Self::extract_message_id(&resp));
        }
        Ok(String::new())
    }

    pub async fn edit_message(
        &self,
        message_id: &str,
        msg_type: &str,
        content: &str,
    ) -> Result<()> {
        let body = serde_json::json!({
            "msg_type": msg_type,
            "content": content,
        });

        for attempt in 0..2 {
            let token = self.get_token().await;
            let resp: serde_json::Value = self
                .http
                .patch(format!("{FEISHU_BASE_URL}/im/v1/messages/{message_id}"))
                .header("Authorization", format!("Bearer {token}"))
                .json(&body)
                .send()
                .await?
                .json()
                .await?;

            if attempt == 0 && Self::is_token_expired(&resp) {
                self.refresh_token().await?;
                continue;
            }
            if let Some(e) = Self::extract_api_error(&resp, "edit_message") {
                return Err(e);
            }
            return Ok(());
        }
        Ok(())
    }

    pub async fn delete_message(&self, message_id: &str) -> Result<()> {
        for attempt in 0..2 {
            let token = self.get_token().await;
            let resp: serde_json::Value = self
                .http
                .delete(format!("{FEISHU_BASE_URL}/im/v1/messages/{message_id}"))
                .header("Authorization", format!("Bearer {token}"))
                .send()
                .await?
                .json()
                .await?;

            if attempt == 0 && Self::is_token_expired(&resp) {
                self.refresh_token().await?;
                continue;
            }
            if let Some(e) = Self::extract_api_error(&resp, "delete_message") {
                return Err(e);
            }
            return Ok(());
        }
        Ok(())
    }

    pub async fn send_card(&self, chat_id: &str, card_json: &serde_json::Value) -> Result<String> {
        let content = serde_json::to_string(card_json)?;
        let body = serde_json::json!({
            "receive_id": chat_id,
            "msg_type": "interactive",
            "content": content,
        });

        for attempt in 0..2 {
            let token = self.get_token().await;
            let resp: serde_json::Value = self
                .http
                .post(format!(
                    "{FEISHU_BASE_URL}/im/v1/messages?receive_id_type=chat_id"
                ))
                .header("Authorization", format!("Bearer {token}"))
                .json(&body)
                .send()
                .await?
                .json()
                .await?;

            if attempt == 0 && Self::is_token_expired(&resp) {
                self.refresh_token().await?;
                continue;
            }
            if let Some(e) = Self::extract_api_error(&resp, "send_card") {
                return Err(e);
            }
            return Ok(Self::extract_message_id(&resp));
        }
        Ok(String::new())
    }

    pub async fn reply_card(
        &self,
        message_id: &str,
        card_json: &serde_json::Value,
    ) -> Result<String> {
        let content = serde_json::to_string(card_json)?;
        let body = serde_json::json!({
            "msg_type": "interactive",
            "content": content,
        });

        for attempt in 0..2 {
            let token = self.get_token().await;
            let resp: serde_json::Value = self
                .http
                .post(format!(
                    "{FEISHU_BASE_URL}/im/v1/messages/{message_id}/reply"
                ))
                .header("Authorization", format!("Bearer {token}"))
                .json(&body)
                .send()
                .await?
                .json()
                .await?;

            if attempt == 0 && Self::is_token_expired(&resp) {
                self.refresh_token().await?;
                continue;
            }
            if let Some(e) = Self::extract_api_error(&resp, "reply_card") {
                return Err(e);
            }
            return Ok(Self::extract_message_id(&resp));
        }
        Ok(String::new())
    }

    pub async fn upload_image(&self, image_bytes: Vec<u8>, file_name: &str) -> Result<String> {
        let token = self.get_token().await;
        let part = reqwest::multipart::Part::bytes(image_bytes)
            .file_name(file_name.to_string())
            .mime_str("application/octet-stream")?;
        let form = reqwest::multipart::Form::new()
            .text("image_type", "message")
            .part("image", part);

        let resp = self
            .http
            .post(format!("{FEISHU_BASE_URL}/im/v1/images"))
            .header("Authorization", format!("Bearer {token}"))
            .multipart(form)
            .send()
            .await?
            .json::<serde_json::Value>()
            .await?;

        let image_key = resp
            .pointer("/data/image_key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("feishu: upload_image missing image_key: {resp}"))?
            .to_string();
        Ok(image_key)
    }

    pub async fn upload_file(
        &self,
        file_bytes: Vec<u8>,
        file_name: &str,
        file_type: &str,
    ) -> Result<String> {
        let token = self.get_token().await;
        let part = reqwest::multipart::Part::bytes(file_bytes)
            .file_name(file_name.to_string())
            .mime_str("application/octet-stream")?;
        let form = reqwest::multipart::Form::new()
            .text("file_type", file_type.to_string())
            .text("file_name", file_name.to_string())
            .part("file", part);

        let resp = self
            .http
            .post(format!("{FEISHU_BASE_URL}/im/v1/files"))
            .header("Authorization", format!("Bearer {token}"))
            .multipart(form)
            .send()
            .await?
            .json::<serde_json::Value>()
            .await?;

        let file_key = resp
            .pointer("/data/file_key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("feishu: upload_file missing file_key: {resp}"))?
            .to_string();
        Ok(file_key)
    }

    pub async fn download_resource(
        &self,
        message_id: &str,
        file_key: &str,
        resource_type: &str,
    ) -> Result<Vec<u8>> {
        let token = self.get_token().await;
        let resp = self
            .http
            .get(format!(
                "{FEISHU_BASE_URL}/im/v1/messages/{message_id}/resources/{file_key}?type={resource_type}"
            ))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("feishu: download_resource failed ({}): {body}", status);
        }
        Ok(resp.bytes().await?.to_vec())
    }
}
