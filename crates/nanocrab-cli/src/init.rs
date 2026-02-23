use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use console::Term;
use dialoguer::{theme::ColorfulTheme, Password, Select};
use nanocrab_auth::oauth::{profile_from_setup_token, run_openai_pkce_flow, validate_setup_token, OpenAiOAuthConfig};
use nanocrab_auth::{AuthProfile, TokenManager};

use crate::init_ui::{print_done, print_logo, print_step};

const TOTAL_STEPS: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderId {
    Anthropic,
    OpenAi,
}

impl ProviderId {
    fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
        }
    }

    fn default_model(self) -> &'static str {
        match self {
            Self::Anthropic => "claude-sonnet-4-5",
            Self::OpenAi => "gpt-4o-mini",
        }
    }

    fn api_base(self) -> &'static str {
        match self {
            Self::Anthropic => "https://api.anthropic.com/v1",
            Self::OpenAi => "https://api.openai.com/v1",
        }
    }

    fn default_api_key_env(self) -> &'static str {
        match self {
            Self::Anthropic => "ANTHROPIC_API_KEY",
            Self::OpenAi => "OPENAI_API_KEY",
        }
    }
}

#[derive(Debug, Clone)]
enum AuthChoice {
    OAuth { profile_name: String },
    ApiKey { env_var: String },
}

pub async fn run_init(config_root: &Path, force: bool) -> Result<()> {
    let term = Term::stdout();
    let theme = ColorfulTheme::default();

    print_logo(&term);
    print_step(&term, 1, TOTAL_STEPS, "LLM Provider");

    let provider = prompt_provider(&theme)?;
    let auth = prompt_auth_choice(&theme, provider).await?;
    write_provider_config(config_root, provider, &auth, force)?;

    print_done(&term, "Provider configuration generated.");
    term.write_line("Remaining wizard steps will be implemented in the next tasks.")?;

    Ok(())
}

fn prompt_provider(theme: &ColorfulTheme) -> Result<ProviderId> {
    let options = ["Anthropic", "OpenAI"];
    let selected = Select::with_theme(theme)
        .with_prompt("Choose your LLM provider")
        .items(&options)
        .default(0)
        .interact()?;

    match selected {
        0 => Ok(ProviderId::Anthropic),
        1 => Ok(ProviderId::OpenAi),
        _ => Err(anyhow!("invalid provider selection index: {selected}")),
    }
}

async fn prompt_auth_choice(theme: &ColorfulTheme, provider: ProviderId) -> Result<AuthChoice> {
    let methods = ["OAuth Login (use your subscription)", "API Key"];
    let method = Select::with_theme(theme)
        .with_prompt("Authentication method")
        .items(&methods)
        .default(0)
        .interact()?;

    match method {
        0 => run_oauth_auth(provider).await,
        1 => {
            let api_key = Password::with_theme(theme)
                .with_prompt(format!("Paste {} API key", provider.as_str()))
                .allow_empty_password(false)
                .interact()?;
            if api_key.trim().is_empty() {
                anyhow::bail!("API key cannot be empty");
            }
            Ok(AuthChoice::ApiKey {
                env_var: provider.default_api_key_env().to_string(),
            })
        }
        _ => Err(anyhow!("invalid auth method index: {method}")),
    }
}

async fn run_oauth_auth(provider: ProviderId) -> Result<AuthChoice> {
    let manager = TokenManager::new()?;
    let profile_name = format!("{}-{}", provider.as_str(), unix_timestamp()?);

    match provider {
        ProviderId::OpenAi => {
            let client_id = std::env::var("OPENAI_OAUTH_CLIENT_ID")
                .map_err(|_| anyhow!("OPENAI_OAUTH_CLIENT_ID is not set"))?;
            let config = OpenAiOAuthConfig::default_with_client(client_id);
            let http = reqwest::Client::new();
            let token = run_openai_pkce_flow(&http, &config).await?;
            manager.save_profile(
                &profile_name,
                AuthProfile::OpenAiOAuth {
                    access_token: token.access_token,
                    refresh_token: token.refresh_token,
                    expires_at: unix_timestamp()? + token.expires_in,
                },
            )?;
        }
        ProviderId::Anthropic => {
            let token = Password::new()
                .with_prompt("Paste your Anthropic setup-token")
                .allow_empty_password(false)
                .interact()
                .context("failed to read Anthropic setup-token")?;
            let http = reqwest::Client::new();
            let ok = validate_setup_token(&http, &token, "https://api.anthropic.com/v1/models").await?;
            if !ok {
                anyhow::bail!("Anthropic setup-token validation failed");
            }
            manager.save_profile(&profile_name, profile_from_setup_token(token))?;
        }
    }

    Ok(AuthChoice::OAuth { profile_name })
}

fn write_provider_config(config_root: &Path, provider: ProviderId, auth: &AuthChoice, force: bool) -> Result<PathBuf> {
    let providers_dir = config_root.join("config/providers.d");
    fs::create_dir_all(&providers_dir)
        .with_context(|| format!("failed to create {}", providers_dir.display()))?;

    let target = providers_dir.join(format!("{}.yaml", provider.as_str()));
    if target.exists() && !force {
        anyhow::bail!(
            "provider config already exists: {} (use --force to overwrite)",
            target.display()
        );
    }

    let yaml = generate_provider_yaml(provider, auth);
    fs::write(&target, yaml).with_context(|| format!("failed to write {}", target.display()))?;
    Ok(target)
}

fn generate_provider_yaml(provider: ProviderId, auth: &AuthChoice) -> String {
    match auth {
        AuthChoice::OAuth { profile_name } => format!(
            "provider_id: {provider}\nenabled: true\napi_base: {base}\napi_key_env: {env}\nauth_profile: \"{profile}\"\nmodels:\n  - {model}\n",
            provider = provider.as_str(),
            base = provider.api_base(),
            env = provider.default_api_key_env(),
            profile = profile_name,
            model = provider.default_model(),
        ),
        AuthChoice::ApiKey { env_var } => format!(
            "provider_id: {provider}\nenabled: true\napi_base: {base}\napi_key_env: {env}\nmodels:\n  - {model}\n",
            provider = provider.as_str(),
            base = provider.api_base(),
            env = env_var,
            model = provider.default_model(),
        ),
    }
}

fn unix_timestamp() -> Result<i64> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| anyhow!("system clock before unix epoch: {e}"))?;
    Ok(now.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::{generate_provider_yaml, AuthChoice, ProviderId};

    #[test]
    fn provider_yaml_uses_auth_profile_for_oauth() {
        let yaml = generate_provider_yaml(
            ProviderId::OpenAi,
            &AuthChoice::OAuth {
                profile_name: "openai-oauth".to_string(),
            },
        );

        assert!(yaml.contains("provider_id: openai"));
        assert!(yaml.contains("auth_profile: \"openai-oauth\""));
        assert!(yaml.contains("api_key_env: OPENAI_API_KEY"));
        assert!(!yaml.contains("api_key:"));
    }

    #[test]
    fn provider_yaml_uses_api_key_env_for_api_key_auth() {
        let yaml = generate_provider_yaml(
            ProviderId::Anthropic,
            &AuthChoice::ApiKey {
                env_var: "ANTHROPIC_API_KEY".to_string(),
            },
        );

        assert!(yaml.contains("provider_id: anthropic"));
        assert!(yaml.contains("api_key_env: ANTHROPIC_API_KEY"));
        assert!(!yaml.contains("auth_profile:"));
    }
}
