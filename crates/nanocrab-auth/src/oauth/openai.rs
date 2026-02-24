use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::server::wait_for_oauth_callback;

/// Default OAuth scope required by OpenAI's authorize endpoint.
pub const OPENAI_OAUTH_SCOPE: &str = "openid profile email offline_access";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PkcePair {
    pub verifier: String,
    pub challenge: String,
}

/// First-stage token response (authorization_code grant).
/// Contains `id_token` needed for the second-stage API key exchange.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpenAiTokenResponse {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: i64,
    /// JWT id_token — used for token-exchange to get an OpenAI API key.
    #[serde(default)]
    pub id_token: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OpenAiOAuthConfig {
    pub client_id: String,
    /// Must be `http://localhost:1455/auth/callback` (NOT 127.0.0.1).
    pub redirect_uri: String,
    pub authorize_endpoint: String,
    pub token_endpoint: String,
    /// OAuth scope string.
    pub scope: String,
    /// Identifies the client application to OpenAI (e.g. "nanocrab").
    pub originator: String,
}

impl OpenAiOAuthConfig {
    pub fn default_with_client(client_id: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            redirect_uri: "http://localhost:1455/auth/callback".to_string(),
            authorize_endpoint: "https://auth.openai.com/oauth/authorize".to_string(),
            token_endpoint: "https://auth.openai.com/oauth/token".to_string(),
            scope: OPENAI_OAUTH_SCOPE.to_string(),
            originator: "nanocrab".to_string(),
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

/// Build the OpenAI OAuth authorize URL with all required parameters.
///
/// In addition to standard OAuth/PKCE params, OpenAI requires:
/// - `scope` (openid profile email offline_access)
/// - `codex_cli_simplified_flow=true`
/// - `originator` (client app identifier)
/// - `id_token_add_organizations=true` (to get account ID in the id_token)
pub fn build_authorize_url(config: &OpenAiOAuthConfig, code_challenge: &str, state: &str) -> String {
    format!(
        "{}?response_type=code&client_id={}&redirect_uri={}&scope={}&code_challenge={}&code_challenge_method=S256&state={}&codex_cli_simplified_flow=true&id_token_add_organizations=true&originator={}",
        config.authorize_endpoint,
        urlencoding::encode(&config.client_id),
        urlencoding::encode(&config.redirect_uri),
        urlencoding::encode(&config.scope),
        urlencoding::encode(code_challenge),
        urlencoding::encode(state),
        urlencoding::encode(&config.originator),
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

    let authorize_url = build_authorize_url(config, &pkce.challenge, &state);

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

/// Second-stage token exchange: swap an `id_token` for an OpenAI API key.
///
/// Uses the RFC 8693 token-exchange grant type:
///   `grant_type=urn:ietf:params:oauth:grant-type:token-exchange`
///   `requested_token=openai-api-key`
///   `subject_token=<id_token>`
///   `subject_token_type=urn:ietf:params:oauth:token-type:id_token`
pub async fn exchange_id_token_for_api_key(
    http: &reqwest::Client,
    token_endpoint: &str,
    client_id: &str,
    id_token: &str,
) -> Result<String> {
    let payload = [
        (
            "grant_type",
            "urn:ietf:params:oauth:grant-type:token-exchange",
        ),
        ("client_id", client_id),
        ("requested_token", "openai-api-key"),
        ("subject_token", id_token),
        (
            "subject_token_type",
            "urn:ietf:params:oauth:token-type:id_token",
        ),
    ];

    let response = http
        .post(token_endpoint)
        .header("content-type", "application/x-www-form-urlencoded")
        .form(&payload)
        .send()
        .await
        .context("failed to exchange id_token for OpenAI API key")?;

    let status = response.status();
    if !status.is_success() {
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "<failed to read error body>".to_string());
        anyhow::bail!("openai api-key token exchange failed ({status}): {body}");
    }

    #[derive(Deserialize)]
    struct ApiKeyResponse {
        access_token: String,
    }

    let resp = response
        .json::<ApiKeyResponse>()
        .await
        .context("invalid OpenAI api-key exchange response")?;

    Ok(resp.access_token)
}

/// Extract the ChatGPT account ID from an OAuth access_token JWT.
///
/// Decodes the JWT payload (no signature verification) and checks
/// multiple known claim keys for the account ID.
pub fn extract_chatgpt_account_id(access_token: &str) -> Option<String> {
    let payload = access_token.split('.').nth(1)?;
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&decoded).ok()?;

    // Check namespaced auth claims first (OpenAI convention)
    if let Some(auth_obj) = claims.get("https://api.openai.com/auth") {
        for sub_key in ["chatgpt_account_id", "account_id"] {
            if let Some(val) = auth_obj.get(sub_key).and_then(|v| v.as_str()) {
                if !val.trim().is_empty() {
                    return Some(val.to_string());
                }
            }
        }
    }

    // Fallback: top-level keys
    for key in ["account_id", "accountId", "acct", "sub", "https://api.openai.com/account_id"] {
        if let Some(val) = claims.get(key).and_then(|v| v.as_str()) {
            if !val.trim().is_empty() {
                return Some(val.to_string());
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::{
        build_authorize_url, exchange_code_for_tokens, exchange_id_token_for_api_key,
        extract_chatgpt_account_id, generate_pkce_pair, OpenAiOAuthConfig,
    };
    use wiremock::matchers::{body_string_contains, method, path};
    use base64::Engine;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn test_config() -> OpenAiOAuthConfig {
        OpenAiOAuthConfig {
            client_id: "client-123".to_string(),
            redirect_uri: "http://localhost:1455/auth/callback".to_string(),
            authorize_endpoint: "https://auth.openai.com/oauth/authorize".to_string(),
            token_endpoint: "https://auth.openai.com/oauth/token".to_string(),
            scope: "openid profile email offline_access".to_string(),
            originator: "nanocrab".to_string(),
        }
    }

    #[test]
    fn pkce_pair_has_valid_lengths() {
        let pair = generate_pkce_pair();
        assert!(pair.verifier.len() >= 43 && pair.verifier.len() <= 128);
        assert!(pair.challenge.len() >= 43);
    }

    #[test]
    fn authorize_url_contains_required_parameters() {
        let config = test_config();
        let url = build_authorize_url(&config, "challenge-abc", "state-xyz");

        assert!(url.contains("response_type=code"));
        assert!(url.contains("code_challenge_method=S256"));
        assert!(url.contains("client_id=client-123"));
        // OpenAI-specific required params
        assert!(url.contains("scope=openid"));
        assert!(url.contains("codex_cli_simplified_flow=true"));
        assert!(url.contains("originator=nanocrab"));
        assert!(url.contains("id_token_add_organizations=true"));
        // Must use localhost, not 127.0.0.1
        assert!(url.contains("localhost%3A1455"));
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
                    "expires_in": 3600,
                    "id_token": "id_tok_789"
                })),
            )
            .mount(&server)
            .await;

        let http = reqwest::Client::new();
        let token = exchange_code_for_tokens(
            &http,
            &format!("{}/oauth/token", server.uri()),
            "client-123",
            "http://localhost:1455/auth/callback",
            "code-xyz",
            "verifier-abc",
        )
        .await
        .expect("token exchange should succeed");

        assert_eq!(token.access_token, "at_123");
        assert_eq!(token.refresh_token, "rt_456");
        assert_eq!(token.expires_in, 3600);
        assert_eq!(token.id_token.as_deref(), Some("id_tok_789"));
    }

    #[tokio::test]
    async fn exchange_id_token_for_api_key_sends_token_exchange_grant() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/oauth/token"))
            .and(body_string_contains("grant_type=urn"))
            .and(body_string_contains("token-exchange"))
            .and(body_string_contains("requested_token=openai-api-key"))
            .and(body_string_contains("subject_token=my-id-token"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "sk-openai-api-key-xyz"
                })),
            )
            .mount(&server)
            .await;

        let http = reqwest::Client::new();
        let api_key = exchange_id_token_for_api_key(
            &http,
            &format!("{}/oauth/token", server.uri()),
            "client-123",
            "my-id-token",
        )
        .await
        .expect("api key exchange should succeed");

        assert_eq!(api_key, "sk-openai-api-key-xyz");
    }

    #[test]
    fn extract_chatgpt_account_id_from_namespaced_claims() {
        let claims = serde_json::json!({
            "https://api.openai.com/auth": {
                "chatgpt_account_id": "acct-test-123"
            },
            "sub": "user-456"
        });
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&claims).unwrap());
        let fake_jwt = format!("eyJhbGciOiJSUzI1NiJ9.{payload}.fakesig");
        assert_eq!(
            extract_chatgpt_account_id(&fake_jwt).as_deref(),
            Some("acct-test-123")
        );
    }

    #[test]
    fn extract_chatgpt_account_id_fallback_to_sub() {
        let claims = serde_json::json!({"sub": "user-789"});
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(&claims).unwrap());
        let fake_jwt = format!("eyJhbGciOiJSUzI1NiJ9.{payload}.fakesig");
        assert_eq!(
            extract_chatgpt_account_id(&fake_jwt).as_deref(),
            Some("user-789")
        );
    }

    #[test]
    fn extract_chatgpt_account_id_returns_none_for_garbage() {
        assert!(extract_chatgpt_account_id("not-a-jwt").is_none());
        assert!(extract_chatgpt_account_id("a.b.c").is_none());
    }
}
