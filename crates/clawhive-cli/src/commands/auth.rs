use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use clap::Subcommand;
use clawhive_auth::oauth::{
    extract_chatgpt_account_id, profile_from_setup_token, prompt_setup_token, run_openai_pkce_flow,
    validate_setup_token, OpenAiOAuthConfig,
};
use clawhive_auth::{AuthProfile, AuthStore, TokenManager};

#[derive(Subcommand)]
pub enum AuthCommands {
    #[command(about = "Login with OpenAI OAuth or Anthropic setup-token")]
    Login {
        #[command(subcommand)]
        provider: AuthLoginProvider,
    },
    #[command(about = "Show active auth profile")]
    Status,
    #[command(about = "Clear auth profiles")]
    Logout,
    #[command(about = "Reset web console password")]
    ResetPassword,
}

#[derive(Subcommand)]
pub enum AuthLoginProvider {
    #[command(about = "Login with OpenAI PKCE OAuth")]
    Openai,
    #[command(about = "Login with Anthropic setup-token")]
    Anthropic,
}

pub async fn handle_auth_command(cmd: AuthCommands, config_root: &Path) -> Result<()> {
    let manager = TokenManager::new()?;

    match cmd {
        AuthCommands::Login { provider } => match provider {
            AuthLoginProvider::Openai => {
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
                let now = now_unix_ts()?;
                manager.save_profile(
                    "openai-oauth",
                    AuthProfile::OpenAiOAuth {
                        access_token: token.access_token,
                        refresh_token: token.refresh_token,
                        expires_at: now + token.expires_in,
                        chatgpt_account_id: account_id,
                    },
                )?;

                println!("OpenAI OAuth login completed. Active profile: openai-oauth");
            }
            AuthLoginProvider::Anthropic => {
                let token = prompt_setup_token()?;
                let http = reqwest::Client::new();
                let endpoint = "https://api.anthropic.com/v1/models";

                let valid = validate_setup_token(&http, &token, endpoint).await?;
                if !valid {
                    anyhow::bail!("Anthropic setup-token validation failed");
                }

                manager.save_profile("anthropic-session", profile_from_setup_token(token))?;
                println!("Anthropic setup-token saved. Active profile: anthropic-session");
            }
        },
        AuthCommands::Status => {
            let store = manager.load_store()?;
            print_status(&store);
        }
        AuthCommands::Logout => {
            manager.save_store(&AuthStore::default())?;
            println!("All auth profiles removed.");
        }
        AuthCommands::ResetPassword => {
            clear_web_password_hash(config_root)?;
            println!(
                "Web console password has been reset. You can set a new one at the login page."
            );
        }
    }

    Ok(())
}

fn print_status(store: &AuthStore) {
    match &store.active_profile {
        Some(active) => {
            println!("Active profile: {active}");
            for (name, profile) in &store.profiles {
                println!("- {name}: {}", profile_kind(profile));
            }
        }
        None => println!("No active auth profile."),
    }
}

fn profile_kind(profile: &AuthProfile) -> &'static str {
    match profile {
        AuthProfile::ApiKey { .. } => "ApiKey",
        AuthProfile::OpenAiOAuth { .. } => "OpenAiOAuth",
        AuthProfile::AnthropicSession { .. } => "AnthropicSession",
    }
}

fn now_unix_ts() -> Result<i64> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| anyhow!("system clock before unix epoch: {e}"))?;
    Ok(now.as_secs() as i64)
}

fn clear_web_password_hash(root: &Path) -> Result<()> {
    let path = root.join("config/main.yaml");
    let content = std::fs::read_to_string(&path).context("failed to read config/main.yaml")?;
    let mut doc: serde_yaml::Value =
        serde_yaml::from_str(&content).context("failed to parse main.yaml")?;
    if let serde_yaml::Value::Mapping(ref mut map) = doc {
        map.remove(serde_yaml::Value::String("web_password_hash".to_string()));
    }
    let yaml = serde_yaml::to_string(&doc).context("failed to serialize main.yaml")?;
    std::fs::write(&path, yaml).context("failed to write main.yaml")?;
    Ok(())
}
