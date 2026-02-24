use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use clap::Subcommand;
use nanocrab_auth::oauth::{
    profile_from_setup_token, prompt_setup_token, run_openai_pkce_flow, validate_setup_token,
    OpenAiOAuthConfig,
};
use nanocrab_auth::{AuthProfile, AuthStore, TokenManager};

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
}

#[derive(Subcommand)]
pub enum AuthLoginProvider {
    #[command(about = "Login with OpenAI PKCE OAuth")]
    Openai,
    #[command(about = "Login with Anthropic setup-token")]
    Anthropic,
}

pub async fn handle_auth_command(cmd: AuthCommands) -> Result<()> {
    let manager = TokenManager::new()?;

    match cmd {
        AuthCommands::Login { provider } => match provider {
            AuthLoginProvider::Openai => {
                let client_id = "app_EMoamEEZ73f0CkXaXp7hrann";

                let config = OpenAiOAuthConfig::default_with_client(client_id);
                let http = reqwest::Client::new();
                let token = run_openai_pkce_flow(&http, &config).await?;

                let now = now_unix_ts()?;
                manager.save_profile(
                    "openai-oauth",
                    AuthProfile::OpenAiOAuth {
                        access_token: token.access_token,
                        refresh_token: token.refresh_token,
                        expires_at: now + token.expires_in,
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
