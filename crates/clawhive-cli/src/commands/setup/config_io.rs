use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use dialoguer::{theme::ColorfulTheme, Input};

use clawhive_core::config::MainConfig;
use clawhive_schema::provider_presets::model_info;

pub(super) const BACK_SENTINEL: &str = "<";

pub(super) fn input_or_back(theme: &ColorfulTheme, prompt: &str) -> Result<Option<String>> {
    let raw: String = Input::with_theme(theme)
        .with_prompt(format!("{prompt} (< to go back)"))
        .allow_empty(true)
        .interact_text()?;
    let trimmed = raw.trim();
    if trimmed == BACK_SENTINEL || trimmed.is_empty() {
        return Ok(None);
    }
    Ok(Some(trimmed.to_string()))
}

pub(super) fn input_or_back_with_default(
    theme: &ColorfulTheme,
    prompt: &str,
    default: &str,
) -> Result<Option<String>> {
    let raw: String = Input::with_theme(theme)
        .with_prompt(format!("{prompt} (< to go back)"))
        .default(default.to_string())
        .interact_text()?;
    let trimmed = raw.trim();
    if trimmed == BACK_SENTINEL {
        return Ok(None);
    }
    Ok(Some(trimmed.to_string()))
}

/// Show first 8 and last 4 characters, mask the middle with asterisks.
pub(super) fn mask_secret(s: &str) -> String {
    if s.len() <= 16 {
        return "*".repeat(s.len());
    }
    format!("{}****{}", &s[..8], &s[s.len() - 4..])
}

pub(super) fn display_rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.display().to_string())
}

pub(super) fn ensure_required_dirs(config_root: &Path) -> Result<()> {
    for rel in ["config/agents.d", "config/providers.d"] {
        let dir = config_root.join(rel);
        fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;
    }
    Ok(())
}

pub(super) fn validate_generated_config(config_root: &Path) -> Result<()> {
    let config_path = config_root.join("config");
    clawhive_core::load_config(&config_path)
        .with_context(|| format!("config validation failed in {}", config_path.display()))?;
    Ok(())
}

/// Load `main.yaml` as a typed `MainConfig`, falling back to defaults if the
/// file is missing or incomplete.
pub(super) fn load_main_config(config_root: &Path) -> Result<MainConfig> {
    let main_path = config_root.join("config/main.yaml");
    if !main_path.exists() {
        return Ok(MainConfig::default());
    }
    let content = fs::read_to_string(&main_path)?;
    match serde_yaml::from_str::<MainConfig>(&content) {
        Ok(cfg) => Ok(cfg),
        Err(_) => {
            let raw: serde_yaml::Value = serde_yaml::from_str(&content)
                .unwrap_or(serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
            let mut cfg = MainConfig::default();
            if let Some(hash) = raw.get("web_password_hash").and_then(|v| v.as_str()) {
                cfg.web_password_hash = Some(hash.to_string());
            }
            Ok(cfg)
        }
    }
}

/// Save `MainConfig` to `config/main.yaml`.
pub(super) fn save_main_config(config_root: &Path, config: &MainConfig) -> Result<()> {
    let main_path = config_root.join("config/main.yaml");
    let yaml = serde_yaml::to_string(config)?;
    fs::write(&main_path, yaml)?;
    Ok(())
}

pub(super) fn format_model_label(model_id: &str) -> String {
    let parts: Vec<&str> = model_id.splitn(2, '/').collect();
    if parts.len() == 2 {
        if let Some(info) = model_info(parts[0], parts[1]) {
            let ctx = if info.context_window >= 1_000_000 {
                format!("{}M", info.context_window / 1_000_000)
            } else {
                format!("{}k", info.context_window / 1000)
            };
            let mut tags = vec![format!("{ctx} ctx")];
            if info.reasoning {
                tags.push("reasoning".into());
            }
            if info.vision {
                tags.push("vision".into());
            }
            return format!("{model_id} ({})", tags.join(", "));
        }
    }
    model_id.to_string()
}

pub(super) fn unix_timestamp() -> Result<i64> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| anyhow!("system clock before unix epoch: {e}"))?;
    Ok(now.as_secs() as i64)
}
