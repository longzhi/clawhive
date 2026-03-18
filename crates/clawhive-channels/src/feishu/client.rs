use std::sync::Arc;

use anyhow::Result;
use tokio::sync::RwLock;

use super::types::{FeishuClientConfig, WsEndpointResponse};

const FEISHU_BASE_URL: &str = "https://open.feishu.cn/open-apis";
const FEISHU_WS_ENDPOINT: &str = "https://open.feishu.cn/callback/ws/endpoint";

pub struct FeishuClient {
    app_id: String,
    app_secret: String,
    token: Arc<RwLock<String>>,
    http: reqwest::Client,
}

impl FeishuClient {
    pub fn new(app_id: impl Into<String>, app_secret: impl Into<String>) -> Self {
        Self {
            app_id: app_id.into(),
            app_secret: app_secret.into(),
            token: Arc::new(RwLock::new(String::new())),
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
        *guard = token.to_string();

        tracing::info!(
            target: "clawhive::channel::feishu",
            "refreshed tenant_access_token"
        );
        Ok(())
    }

    pub async fn send_message(&self, chat_id: &str, msg_type: &str, content: &str) -> Result<()> {
        let token = self.token.read().await.clone();
        let resp = self
            .http
            .post(format!(
                "{FEISHU_BASE_URL}/im/v1/messages?receive_id_type=chat_id"
            ))
            .header("Authorization", format!("Bearer {token}"))
            .json(&serde_json::json!({
                "receive_id": chat_id,
                "msg_type": msg_type,
                "content": content,
            }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("feishu: send_message failed: {body}");
        }
        Ok(())
    }

    pub async fn reply_message(
        &self,
        message_id: &str,
        msg_type: &str,
        content: &str,
    ) -> Result<()> {
        let token = self.token.read().await.clone();
        let resp = self
            .http
            .post(format!(
                "{FEISHU_BASE_URL}/im/v1/messages/{message_id}/reply"
            ))
            .header("Authorization", format!("Bearer {token}"))
            .json(&serde_json::json!({
                "msg_type": msg_type,
                "content": content,
            }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("feishu: reply_message failed: {body}");
        }
        Ok(())
    }

    /// Reply to a message and return the sent message's ID.
    pub async fn reply_message_with_id(
        &self,
        message_id: &str,
        msg_type: &str,
        content: &str,
    ) -> Result<String> {
        let token = self.token.read().await.clone();
        let resp = self
            .http
            .post(format!(
                "{FEISHU_BASE_URL}/im/v1/messages/{message_id}/reply"
            ))
            .header("Authorization", format!("Bearer {token}"))
            .json(&serde_json::json!({
                "msg_type": msg_type,
                "content": content,
            }))
            .send()
            .await?
            .json::<serde_json::Value>()
            .await?;

        let sent_msg_id = resp
            .pointer("/data/message_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        Ok(sent_msg_id)
    }

    pub async fn edit_message(
        &self,
        message_id: &str,
        msg_type: &str,
        content: &str,
    ) -> Result<()> {
        let token = self.token.read().await.clone();
        let resp = self
            .http
            .patch(format!("{FEISHU_BASE_URL}/im/v1/messages/{message_id}"))
            .header("Authorization", format!("Bearer {token}"))
            .json(&serde_json::json!({
                "msg_type": msg_type,
                "content": content,
            }))
            .send()
            .await?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("feishu: edit_message failed: {body}");
        }
        Ok(())
    }

    pub async fn delete_message(&self, message_id: &str) -> Result<()> {
        let token = self.token.read().await.clone();
        let resp = self
            .http
            .delete(format!("{FEISHU_BASE_URL}/im/v1/messages/{message_id}"))
            .header("Authorization", format!("Bearer {token}"))
            .send()
            .await?;

        if !resp.status().is_success() {
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("feishu: delete_message failed: {body}");
        }
        Ok(())
    }

    /// Send an Interactive Card to a chat. Returns the sent message's ID.
    pub async fn send_card(&self, chat_id: &str, card_json: &serde_json::Value) -> Result<String> {
        let content = serde_json::to_string(card_json)?;
        let token = self.token.read().await.clone();
        let resp = self
            .http
            .post(format!(
                "{FEISHU_BASE_URL}/im/v1/messages?receive_id_type=chat_id"
            ))
            .header("Authorization", format!("Bearer {token}"))
            .json(&serde_json::json!({
                "receive_id": chat_id,
                "msg_type": "interactive",
                "content": content,
            }))
            .send()
            .await?
            .json::<serde_json::Value>()
            .await?;

        let message_id = resp
            .pointer("/data/message_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        Ok(message_id)
    }

    /// Reply with an Interactive Card. Returns the sent message's ID.
    pub async fn reply_card(
        &self,
        message_id: &str,
        card_json: &serde_json::Value,
    ) -> Result<String> {
        let content = serde_json::to_string(card_json)?;
        let token = self.token.read().await.clone();
        let resp = self
            .http
            .post(format!(
                "{FEISHU_BASE_URL}/im/v1/messages/{message_id}/reply"
            ))
            .header("Authorization", format!("Bearer {token}"))
            .json(&serde_json::json!({
                "msg_type": "interactive",
                "content": content,
            }))
            .send()
            .await?
            .json::<serde_json::Value>()
            .await?;

        let reply_msg_id = resp
            .pointer("/data/message_id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        Ok(reply_msg_id)
    }

    /// Upload an image and return the image_key.
    pub async fn upload_image(&self, image_bytes: Vec<u8>, file_name: &str) -> Result<String> {
        let token = self.token.read().await.clone();
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

    /// Upload a file and return the file_key.
    pub async fn upload_file(
        &self,
        file_bytes: Vec<u8>,
        file_name: &str,
        file_type: &str,
    ) -> Result<String> {
        let token = self.token.read().await.clone();
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

    /// Download a resource (image/file) from a user-sent message.
    pub async fn download_resource(
        &self,
        message_id: &str,
        file_key: &str,
        resource_type: &str,
    ) -> Result<Vec<u8>> {
        let token = self.token.read().await.clone();
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

    pub fn spawn_token_refresh(self: &Arc<Self>) {
        let client = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                if let Err(e) = client.refresh_token().await {
                    tracing::error!(
                        target: "clawhive::channel::feishu",
                        error = %e,
                        "failed to refresh tenant_access_token"
                    );
                }
                tokio::time::sleep(std::time::Duration::from_secs(100 * 60)).await;
            }
        });
    }
}
