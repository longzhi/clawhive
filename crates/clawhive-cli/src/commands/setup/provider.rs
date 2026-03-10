use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clawhive_auth::oauth::{
    extract_chatgpt_account_id, profile_from_setup_token, run_openai_pkce_flow,
    validate_setup_token, OpenAiOAuthConfig,
};
use clawhive_auth::{AuthProfile, TokenManager};
use console::Term;
use dialoguer::{theme::ColorfulTheme, Confirm, Input, Select};

use super::config_io::{
    display_rel, input_or_back, input_or_back_with_default, mask_secret, unix_timestamp,
};
use super::scan::ConfigState;
use super::ui::print_done;
use super::ui::ARROW;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProviderId {
    Anthropic,
    OpenAi,
    AzureOpenAi,
    Gemini,
    DeepSeek,
    Groq,
    Ollama,
    OpenRouter,
    Together,
    Fireworks,
    Qwen,
    Moonshot,
    Zhipu,
    MiniMax,
    Volcengine,
    Qianfan,
}

pub(super) const ALL_PROVIDERS: &[ProviderId] = &[
    ProviderId::Anthropic,
    ProviderId::OpenAi,
    ProviderId::AzureOpenAi,
    ProviderId::Gemini,
    ProviderId::DeepSeek,
    ProviderId::Groq,
    ProviderId::Ollama,
    ProviderId::OpenRouter,
    ProviderId::Together,
    ProviderId::Fireworks,
    ProviderId::Qwen,
    ProviderId::Moonshot,
    ProviderId::Zhipu,
    ProviderId::MiniMax,
    ProviderId::Volcengine,
    ProviderId::Qianfan,
];

impl ProviderId {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
            Self::AzureOpenAi => "azure-openai",
            Self::Gemini => "gemini",
            Self::DeepSeek => "deepseek",
            Self::Groq => "groq",
            Self::Ollama => "ollama",
            Self::OpenRouter => "openrouter",
            Self::Together => "together",
            Self::Fireworks => "fireworks",
            Self::Qwen => "qwen",
            Self::Moonshot => "moonshot",
            Self::Zhipu => "zhipu",
            Self::MiniMax => "minimax",
            Self::Volcengine => "volcengine",
            Self::Qianfan => "qianfan",
        }
    }

    fn display_name(self) -> &'static str {
        clawhive_schema::provider_presets::preset_by_id(self.as_str())
            .map(|p| p.name)
            .unwrap_or(self.as_str())
    }

    fn default_model(self) -> &'static str {
        clawhive_schema::provider_presets::preset_by_id(self.as_str())
            .map(|p| p.default_model)
            .unwrap_or("unknown")
    }

    fn api_base(self) -> &'static str {
        clawhive_schema::provider_presets::preset_by_id(self.as_str())
            .map(|p| p.api_base)
            .unwrap_or("")
    }

    fn supports_oauth(self) -> bool {
        // Anthropic subscription (setup-token) is no longer supported in the wizard.
        // The code path still exists in run_oauth_auth() for future use.
        matches!(self, Self::OpenAi)
    }

    fn needs_custom_base_url(self) -> bool {
        matches!(self, Self::AzureOpenAi)
    }
}

#[derive(Debug, Clone)]
pub(super) enum AuthChoice {
    OAuth { profile_name: String },
    ApiKey { api_key: String },
}

pub(super) async fn handle_add_provider(
    config_root: &Path,
    term: &Term,
    theme: &ColorfulTheme,
    state: &ConfigState,
    force: bool,
) -> Result<()> {
    let provider = match prompt_provider(theme)? {
        Some(p) => p,
        None => return Ok(()),
    };

    // For OpenAI we allow separate API-key ("openai") and OAuth ("openai-chatgpt")
    // configs to coexist, so only block when both are already present.
    let fully_configured = if provider == ProviderId::OpenAi {
        let has_key = state.providers.iter().any(|i| i.provider_id == "openai");
        let has_oauth = state
            .providers
            .iter()
            .any(|i| i.provider_id == "openai-chatgpt");
        has_key && has_oauth
    } else {
        state
            .providers
            .iter()
            .any(|item| item.provider_id == provider.as_str())
    };
    if fully_configured && !force {
        let actions = ["Reconfigure", "Remove", "Cancel"];
        let selected = Select::with_theme(theme)
            .with_prompt(format!("{} already configured", provider.as_str()))
            .items(&actions)
            .default(2)
            .interact()?;
        match selected {
            0 => { /* continue to reconfigure below */ }
            1 => {
                if Confirm::with_theme(theme)
                    .with_prompt(format!(
                        "Are you sure you want to remove {}?",
                        provider.as_str()
                    ))
                    .default(false)
                    .interact()?
                {
                    let path =
                        config_root.join(format!("config/providers.d/{}.yaml", provider.as_str()));
                    if path.exists() {
                        fs::remove_file(&path)?;
                    }
                    print_done(term, &format!("Provider {} removed.", provider.as_str()));
                }
                return Ok(());
            }
            _ => {
                return Ok(());
            }
        }
    }

    let api_base_override = if provider.needs_custom_base_url() {
        let base = match input_or_back(
            theme,
            "Azure OpenAI endpoint URL (e.g. https://myresource.openai.azure.com/openai/v1)",
        )? {
            Some(b) => b,
            None => return Ok(()),
        };
        Some(base)
    } else if provider == ProviderId::Ollama {
        let base = match input_or_back_with_default(theme, "Ollama API URL", provider.api_base())? {
            Some(b) => b,
            None => return Ok(()),
        };
        if base == provider.api_base() {
            None
        } else {
            Some(base)
        }
    } else {
        None
    };

    let auth = match prompt_auth_choice(theme, provider).await? {
        Some(a) => a,
        None => return Ok(()),
    };
    let path = write_provider_config_unchecked(
        config_root,
        provider,
        &auth,
        api_base_override.as_deref(),
    )?;
    print_done(
        term,
        &format!(
            "Provider configuration saved: {}",
            display_rel(config_root, &path)
        ),
    );

    Ok(())
}

fn prompt_provider(theme: &ColorfulTheme) -> Result<Option<ProviderId>> {
    let mut options: Vec<&str> = ALL_PROVIDERS.iter().map(|p| p.display_name()).collect();
    options.push("← Back");
    let selected = Select::with_theme(theme)
        .with_prompt("Choose your LLM provider")
        .items(&options)
        .default(0)
        .interact()?;

    if selected >= ALL_PROVIDERS.len() {
        return Ok(None);
    }
    Ok(Some(ALL_PROVIDERS[selected]))
}

async fn prompt_auth_choice(
    theme: &ColorfulTheme,
    provider: ProviderId,
) -> Result<Option<AuthChoice>> {
    if provider.supports_oauth() {
        let methods: Vec<&str> = match provider {
            ProviderId::Anthropic => vec![
                "Setup Token (run `claude setup-token` in terminal)",
                "API Key (from console.anthropic.com/settings/keys)",
                "← Back",
            ],
            ProviderId::OpenAi => vec![
                "OAuth Login (use your ChatGPT subscription)",
                "API Key (from platform.openai.com/api-keys)",
                "← Back",
            ],
            _ => unreachable!(),
        };
        let method = Select::with_theme(theme)
            .with_prompt("Authentication method")
            .items(&methods)
            .default(0)
            .interact()?;

        match method {
            0 => run_oauth_auth(provider).await.map(Some),
            1 => prompt_api_key(theme, provider),
            _ => Ok(None),
        }
    } else if provider == ProviderId::Ollama {
        // Ollama runs locally, no auth needed
        Ok(Some(AuthChoice::ApiKey {
            api_key: String::new(),
        }))
    } else {
        prompt_api_key(theme, provider)
    }
}

fn prompt_api_key(theme: &ColorfulTheme, provider: ProviderId) -> Result<Option<AuthChoice>> {
    let api_key = match input_or_back(theme, &format!("Paste {} API key", provider.display_name()))?
    {
        Some(k) if !k.is_empty() => k,
        Some(_) => anyhow::bail!("API key cannot be empty"),
        None => return Ok(None),
    };
    let masked = mask_secret(&api_key);
    println!("  {ARROW} Key saved: {masked}");
    Ok(Some(AuthChoice::ApiKey { api_key }))
}

async fn run_oauth_auth(provider: ProviderId) -> Result<AuthChoice> {
    let manager = TokenManager::new()?;
    let profile_name = format!("{}-{}", provider.as_str(), unix_timestamp()?);

    match provider {
        ProviderId::OpenAi => {
            let term = Term::stdout();
            let _ = term.write_line("");
            let _ = term.write_line("  Opening browser for OpenAI OAuth login...");
            let _ = term.write_line("  Complete the login in your browser.");
            let _ = term.write_line("  Waiting for callback (timeout: 5 minutes)...");
            let _ = term.write_line("");
            let client_id = "app_EMoamEEZ73f0CkXaXp7hrann";
            let config = OpenAiOAuthConfig::default_with_client(client_id);
            let http = reqwest::Client::new();
            let token = run_openai_pkce_flow(&http, &config).await?;
            let account_id = extract_chatgpt_account_id(&token.access_token);
            if let Some(ref id) = account_id {
                eprintln!("  ✓ ChatGPT account: {id}");
            } else {
                eprintln!("  ⚠ Could not extract chatgpt_account_id from token");
            }
            manager.save_profile(
                &profile_name,
                AuthProfile::OpenAiOAuth {
                    access_token: token.access_token,
                    refresh_token: token.refresh_token,
                    expires_at: unix_timestamp()? + token.expires_in,
                    chatgpt_account_id: account_id,
                },
            )?;
        }
        ProviderId::Anthropic => {
            let term = Term::stdout();
            let _ = term.write_line("");
            let _ = term
                .write_line("  To use Anthropic with your subscription, you need a setup-token.");
            let _ = term.write_line("  If you have Claude Code CLI installed, run:");
            let _ = term.write_line("");
            let _ = term.write_line("    claude setup-token");
            let _ = term.write_line("");
            let _ = term.write_line("  Then paste the token below.");
            let _ = term.write_line("");
            let token: String = Input::new()
                .with_prompt("Paste your Anthropic setup-token")
                .interact_text()
                .context("failed to read Anthropic setup-token")?;
            let http = reqwest::Client::new();
            let ok = validate_setup_token(&http, &token, "https://api.anthropic.com").await?;
            if !ok {
                anyhow::bail!(
                    "Anthropic setup-token validation failed. Check the log above for details."
                );
            }
            manager.save_profile(&profile_name, profile_from_setup_token(token))?;
        }
        _ => {
            anyhow::bail!("OAuth is not supported for {}", provider.display_name());
        }
    }

    Ok(AuthChoice::OAuth { profile_name })
}

fn write_provider_config_unchecked(
    config_root: &Path,
    provider: ProviderId,
    auth: &AuthChoice,
    api_base_override: Option<&str>,
) -> Result<PathBuf> {
    let providers_dir = config_root.join("config/providers.d");
    fs::create_dir_all(&providers_dir)
        .with_context(|| format!("failed to create {}", providers_dir.display()))?;

    let yaml = generate_provider_yaml(provider, auth, api_base_override);
    // Derive filename from the provider_id in the generated yaml so that
    // OpenAI OAuth ("openai-chatgpt") gets its own file alongside "openai".
    let pid = yaml
        .lines()
        .find_map(|l| l.strip_prefix("provider_id: "))
        .unwrap_or(provider.as_str());
    let target = providers_dir.join(format!("{pid}.yaml"));
    fs::write(&target, yaml).with_context(|| format!("failed to write {}", target.display()))?;
    Ok(target)
}

fn generate_provider_yaml(
    provider: ProviderId,
    auth: &AuthChoice,
    api_base_override: Option<&str>,
) -> String {
    let base_url = api_base_override.unwrap_or(provider.api_base());
    match auth {
        AuthChoice::OAuth { profile_name } => {
            // OpenAI OAuth uses the chatgpt codex endpoint and registers as
            // a separate "openai-chatgpt" provider so it can coexist with
            // the API-key-based "openai" provider.
            let (pid, base, model) = match provider {
                ProviderId::OpenAi => (
                    "openai-chatgpt",
                    "https://chatgpt.com/backend-api/codex",
                    "gpt-5.3-codex",
                ),
                _ => (provider.as_str(), base_url, provider.default_model()),
            };
            format!(
                "provider_id: {pid}\nenabled: true\napi_base: {base}\nauth_profile: \"{profile}\"\nmodels:\n  - {model}\n",
                pid = pid,
                base = base,
                profile = profile_name,
                model = model,
            )
        }
        AuthChoice::ApiKey { api_key } => {
            if api_key.is_empty() {
                // Ollama or other local providers without auth
                format!(
                    "provider_id: {provider}\nenabled: true\napi_base: {base}\nmodels:\n  - {model}\n",
                    provider = provider.as_str(),
                    base = base_url,
                    model = provider.default_model(),
                )
            } else {
                format!(
                    "provider_id: {provider}\nenabled: true\napi_base: {base}\napi_key: \"{key}\"\nmodels:\n  - {model}\n",
                    provider = provider.as_str(),
                    base = base_url,
                    key = api_key,
                    model = provider.default_model(),
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::config_io::ensure_required_dirs;
    use super::*;

    #[test]
    fn provider_yaml_uses_auth_profile_for_oauth() {
        let yaml = generate_provider_yaml(
            ProviderId::OpenAi,
            &AuthChoice::OAuth {
                profile_name: "openai-oauth".to_string(),
            },
            None,
        );

        assert!(yaml.contains("provider_id: openai-chatgpt"));
        assert!(yaml.contains("auth_profile: \"openai-oauth\""));
        assert!(yaml.contains("api_base: https://chatgpt.com/backend-api/codex"));
        assert!(yaml.contains("gpt-5.3-codex"));
        assert!(!yaml.contains("api_key:"));
    }

    #[test]
    fn provider_yaml_uses_api_key_for_api_key_auth() {
        let yaml = generate_provider_yaml(
            ProviderId::Anthropic,
            &AuthChoice::ApiKey {
                api_key: "sk-test-key".to_string(),
            },
            None,
        );

        assert!(yaml.contains("provider_id: anthropic"));
        assert!(yaml.contains("api_key: \"sk-test-key\""));
        assert!(!yaml.contains("auth_profile:"));
    }

    #[test]
    fn provider_yaml_openai_oauth_uses_chatgpt_provider_id() {
        let yaml = generate_provider_yaml(
            ProviderId::OpenAi,
            &AuthChoice::OAuth {
                profile_name: "openai-oauth-123".to_string(),
            },
            None,
        );

        assert!(yaml.contains("provider_id: openai-chatgpt"));
        assert!(yaml.contains("auth_profile: \"openai-oauth-123\""));
        assert!(yaml.contains("api_base: https://chatgpt.com/backend-api/codex"));
        assert!(yaml.contains("gpt-5.3-codex"));
        assert!(!yaml.contains("api_key:"));
    }

    #[test]
    fn provider_model_aliases_are_fully_qualified() {
        use super::super::config_io::provider_models_for_id;
        for provider in ALL_PROVIDERS {
            let models = provider_models_for_id(provider.as_str());
            let prefix = provider.as_str();
            assert!(
                models
                    .iter()
                    .all(|m: &String| m.starts_with(&format!("{prefix}/"))),
                "all models for {} should start with {prefix}/",
                provider.display_name()
            );
        }
    }

    #[test]
    fn provider_models_for_id_returns_known_provider_models() {
        use super::super::config_io::provider_models_for_id;
        for provider in ALL_PROVIDERS {
            let models = provider_models_for_id(provider.as_str());
            assert!(
                !models.is_empty(),
                "provider_models_for_id({}) should return models",
                provider.as_str()
            );
        }
        let unknown = provider_models_for_id("nonexistent");
        assert!(unknown.is_empty());
    }

    #[test]
    fn write_provider_config_unchecked_overwrites_existing_file() {
        let temp = tempfile::tempdir().expect("create tempdir");
        ensure_required_dirs(temp.path()).expect("create required directories");

        let target = temp.path().join("config/providers.d/openai.yaml");
        std::fs::write(&target, "old: value\n").expect("write old provider file");

        write_provider_config_unchecked(
            temp.path(),
            ProviderId::OpenAi,
            &AuthChoice::ApiKey {
                api_key: "sk-test".to_string(),
            },
            None,
        )
        .expect("write provider config");

        let updated = std::fs::read_to_string(&target).expect("read updated provider file");
        assert!(updated.contains("provider_id: openai"));
        assert!(!updated.contains("old: value"));
    }
}
