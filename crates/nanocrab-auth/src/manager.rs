use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

use crate::profile::{AuthProfile, AuthStore};

const AUTH_STORE_FILE: &str = "auth-profiles.json";

#[derive(Debug, Clone)]
pub struct OpenAiRefreshConfig {
    pub token_endpoint: String,
    pub client_id: String,
}

#[derive(Debug, Deserialize)]
struct OpenAiRefreshResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    expires_in: i64,
}

#[derive(Debug, Clone)]
pub struct TokenManager {
    store_path: PathBuf,
}

impl TokenManager {
    pub fn new() -> Result<Self> {
        let home = std::env::var("HOME").map_err(|_| anyhow!("HOME is not set"))?;
        let config_dir = Path::new(&home).join(".nanocrab").join("config");
        Ok(Self::from_config_dir(config_dir))
    }

    pub fn from_config_dir(config_dir: impl Into<PathBuf>) -> Self {
        let config_dir = config_dir.into();
        Self {
            store_path: config_dir.join(AUTH_STORE_FILE),
        }
    }

    pub fn store_path(&self) -> &Path {
        &self.store_path
    }

    pub fn load_store(&self) -> Result<AuthStore> {
        if !self.store_path.exists() {
            return Ok(AuthStore::default());
        }

        let content = fs::read_to_string(&self.store_path)
            .with_context(|| format!("failed to read {}", self.store_path.display()))?;

        match serde_json::from_str::<AuthStore>(&content) {
            Ok(store) => Ok(store),
            Err(_) => Ok(AuthStore::default()),
        }
    }

    pub fn save_store(&self, store: &AuthStore) -> Result<()> {
        if let Some(parent) = self.store_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        let payload = serde_json::to_string_pretty(store).context("serialize auth store")?;
        fs::write(&self.store_path, payload)
            .with_context(|| format!("failed to write {}", self.store_path.display()))?;

        Ok(())
    }

    pub fn get_active_profile(&self) -> Result<Option<AuthProfile>> {
        let store = self.load_store()?;
        let active = store
            .active_profile
            .and_then(|name| store.profiles.get(&name).cloned());
        Ok(active)
    }

    pub fn save_profile(
        &self,
        profile_name: impl Into<String>,
        profile: AuthProfile,
    ) -> Result<()> {
        let profile_name = profile_name.into();
        let mut store = self.load_store()?;
        store.profiles.insert(profile_name.clone(), profile);
        store.active_profile = Some(profile_name);
        self.save_store(&store)
    }

    pub async fn refresh_if_needed(
        &self,
        http: &reqwest::Client,
        profile_name: &str,
        refresh_config: &OpenAiRefreshConfig,
    ) -> Result<Option<AuthProfile>> {
        if let Some(parent) = self.store_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        let lock_path = self.store_path.with_extension("lock");
        let lock_file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .read(true)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("failed to open lock file {}", lock_path.display()))?;
        let mut lock = fd_lock::RwLock::new(lock_file);
        let _guard = lock.write()?;

        let mut store = self.load_store()?;
        let (refresh_token, should_refresh) = match store.profiles.get(profile_name) {
            Some(AuthProfile::OpenAiOAuth {
                refresh_token,
                expires_at,
                ..
            }) => {
                let deadline = now_unix_ts()? + Duration::from_secs(300).as_secs() as i64;
                (refresh_token.clone(), *expires_at <= deadline)
            }
            _ => return Ok(None),
        };

        if !should_refresh {
            return Ok(None);
        }

        let payload = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token.as_str()),
            ("client_id", refresh_config.client_id.as_str()),
        ];

        let response = http
            .post(&refresh_config.token_endpoint)
            .header("content-type", "application/x-www-form-urlencoded")
            .form(&payload)
            .send()
            .await
            .context("failed to refresh OpenAI access token")?;

        let status = response.status();
        if !status.is_success() {
            let body = response
                .text()
                .await
                .unwrap_or_else(|_| "<failed to read error body>".to_string());
            anyhow::bail!("openai refresh failed ({status}): {body}");
        }

        let body = response
            .json::<OpenAiRefreshResponse>()
            .await
            .context("invalid OpenAI refresh response payload")?;

        let new_profile = AuthProfile::OpenAiOAuth {
            access_token: body.access_token,
            refresh_token: body.refresh_token.unwrap_or(refresh_token),
            expires_at: now_unix_ts()? + body.expires_in,
            chatgpt_account_id: None,
        };

        store
            .profiles
            .insert(profile_name.to_string(), new_profile.clone());
        self.save_store(&store)?;

        Ok(Some(new_profile))
    }
}

fn now_unix_ts() -> Result<i64> {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| anyhow!("system clock before unix epoch: {e}"))?;
    Ok(dur.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use crate::profile::AuthProfile;

    use super::{OpenAiRefreshConfig, TokenManager};
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn save_profile_creates_directory_and_file() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let manager = TokenManager::from_config_dir(temp.path().join("nested").join("config"));

        manager
            .save_profile(
                "openai-main",
                AuthProfile::ApiKey {
                    provider_id: "openai".to_string(),
                    api_key: "sk-test".to_string(),
                },
            )
            .expect("save profile");

        assert!(manager.store_path().exists());
    }

    #[test]
    fn invalid_json_returns_default_store() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let manager = TokenManager::from_config_dir(temp.path());

        std::fs::write(manager.store_path(), "{ this is invalid json ")
            .expect("write invalid json");

        let store = manager.load_store().expect("load store should not fail");
        assert!(store.active_profile.is_none());
        assert!(store.profiles.is_empty());
    }

    #[tokio::test]
    async fn refresh_if_needed_updates_expiring_openai_profile() {
        let temp = tempfile::tempdir().expect("create temp dir");
        let manager = TokenManager::from_config_dir(temp.path());

        manager
            .save_profile(
                "openai-main",
                AuthProfile::OpenAiOAuth {
                    access_token: "old-at".to_string(),
                    refresh_token: "old-rt".to_string(),
                    expires_at: 0,
                    chatgpt_account_id: None,
                },
            )
            .expect("save profile");

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("refresh_token=old-rt"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "new-at",
                    "refresh_token": "new-rt",
                    "expires_in": 3600
                })),
            )
            .mount(&server)
            .await;

        let updated = manager
            .refresh_if_needed(
                &reqwest::Client::new(),
                "openai-main",
                &OpenAiRefreshConfig {
                    token_endpoint: format!("{}/oauth/token", server.uri()),
                    client_id: "client-123".to_string(),
                },
            )
            .await
            .expect("refresh should not error");

        assert!(matches!(
            updated,
            Some(AuthProfile::OpenAiOAuth { access_token, refresh_token, .. })
                if access_token == "new-at" && refresh_token == "new-rt"
        ));
    }
}
