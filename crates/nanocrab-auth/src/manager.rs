use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use crate::profile::{AuthProfile, AuthStore};

const AUTH_STORE_FILE: &str = "auth-profiles.json";

#[derive(Debug, Clone)]
pub struct TokenManager {
    store_path: PathBuf,
}

impl TokenManager {
    pub fn new() -> Result<Self> {
        let home = std::env::var("HOME").map_err(|_| anyhow!("HOME is not set"))?;
        let config_dir = Path::new(&home).join(".config").join("nanocrab");
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
}

#[cfg(test)]
mod tests {
    use crate::profile::AuthProfile;

    use super::TokenManager;

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
}
