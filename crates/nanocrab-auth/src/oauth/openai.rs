use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::server::wait_for_oauth_callback;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkcePair {
    pub verifier: String,
    pub challenge: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpenAiTokenResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: i64,
}

#[derive(Debug, Clone)]
pub struct OpenAiOAuthConfig {
    pub client_id: String,
    pub redirect_uri: String,
    pub authorize_endpoint: String,
    pub token_endpoint: String,
}

impl OpenAiOAuthConfig {
    pub fn default_with_client(client_id: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            redirect_uri: "http://127.0.0.1:1455/auth/callback".to_string(),
            authorize_endpoint: "https://auth.openai.com/oauth/authorize".to_string(),
            token_endpoint: "https://auth.openai.com/oauth/token".to_string(),
        }
    }
}

pub fn generate_pkce_pair() -> PkcePair {
    let mut random = [0_u8; 32];
    rand::thread_rng().fill_bytes(&mut random);
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random);

    let challenge = {
        let digest = Sha256::digest(verifier.as_bytes());
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
    };

    PkcePair {
        verifier,
        challenge,
    }
}

pub fn build_authorize_url(
    authorize_endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    code_challenge: &str,
    state: &str,
) -> String {
    format!(
        "{authorize_endpoint}?response_type=code&client_id={}&redirect_uri={}&state={}&code_challenge={}&code_challenge_method=S256",
        urlencoding::encode(client_id),
        urlencoding::encode(redirect_uri),
        urlencoding::encode(state),
        urlencoding::encode(code_challenge)
    )
}

pub fn open_authorize_url(url: &str) -> Result<()> {
    webbrowser::open(url)
        .map(|_| ())
        .with_context(|| format!("failed to open browser for {url}"))
}

pub async fn run_openai_pkce_flow(
    http: &reqwest::Client,
    config: &OpenAiOAuthConfig,
) -> Result<OpenAiTokenResponse> {
    let pkce = generate_pkce_pair();
    let state = uuid::Uuid::new_v4().to_string();

    let authorize_url = build_authorize_url(
        &config.authorize_endpoint,
        &config.client_id,
        &config.redirect_uri,
        &pkce.challenge,
        &state,
    );

    open_authorize_url(&authorize_url)?;

    let callback = wait_for_oauth_callback(state, Duration::from_secs(300)).await?;

    exchange_code_for_tokens(
        http,
        &config.token_endpoint,
        &config.client_id,
        &config.redirect_uri,
        &callback.code,
        &pkce.verifier,
    )
    .await
}

pub async fn exchange_code_for_tokens(
    http: &reqwest::Client,
    token_endpoint: &str,
    client_id: &str,
    redirect_uri: &str,
    code: &str,
    code_verifier: &str,
) -> Result<OpenAiTokenResponse> {
    let payload = [
        ("grant_type", "authorization_code"),
        ("client_id", client_id),
        ("redirect_uri", redirect_uri),
        ("code", code),
        ("code_verifier", code_verifier),
    ];

    let response = http
        .post(token_endpoint)
        .header("content-type", "application/x-www-form-urlencoded")
        .form(&payload)
        .send()
        .await
        .context("failed to exchange oauth code for tokens")?;

    let status = response.status();
    if !status.is_success() {
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "<failed to read error body>".to_string());
        anyhow::bail!("openai token exchange failed ({status}): {body}");
    }

    let tokens = response
        .json::<OpenAiTokenResponse>()
        .await
        .context("invalid OpenAI token response payload")?;

    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::{build_authorize_url, exchange_code_for_tokens, generate_pkce_pair};
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn pkce_pair_has_valid_lengths() {
        let pair = generate_pkce_pair();
        assert!(pair.verifier.len() >= 43 && pair.verifier.len() <= 128);
        assert!(pair.challenge.len() >= 43);
    }

    #[test]
    fn authorize_url_contains_required_parameters() {
        let url = build_authorize_url(
            "https://auth.openai.com/oauth/authorize",
            "client-123",
            "http://127.0.0.1:1455/auth/callback",
            "challenge-abc",
            "state-xyz",
        );

        assert!(url.contains("response_type=code"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("client_id=client-123"));
    }

    #[tokio::test]
    async fn exchange_code_for_tokens_sends_expected_payload() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .and(body_string_contains("grant_type=authorization_code"))
            .and(body_string_contains("client_id=client-123"))
            .and(body_string_contains("code=code-xyz"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "at_123",
                    "refresh_token": "rt_456",
                    "expires_in": 3600
                })),
            )
            .mount(&server)
            .await;

        let http = reqwest::Client::new();
        let token = exchange_code_for_tokens(
            &http,
            &format!("{}/oauth/token", server.uri()),
            "client-123",
            "http://127.0.0.1:1455/auth/callback",
            "code-xyz",
            "verifier-abc",
        )
        .await
        .expect("token exchange should succeed");

        assert_eq!(token.access_token, "at_123");
        assert_eq!(token.refresh_token, "rt_456");
        assert_eq!(token.expires_in, 3600);
    }
}
