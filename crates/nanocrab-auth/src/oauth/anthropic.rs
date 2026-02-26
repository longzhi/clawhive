use std::io::{self, Write};

use anyhow::{anyhow, Result};

use crate::AuthProfile;

pub fn prompt_setup_token() -> Result<String> {
    print!("Paste your Anthropic setup-token: ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    normalize_setup_token(&input)
}

pub fn profile_from_setup_token(token: impl Into<String>) -> AuthProfile {
    AuthProfile::AnthropicSession {
        session_token: token.into(),
    }
}

/// Required beta flags for Anthropic OAuth (setup-token) auth.
/// Without these the API returns 401 "OAuth authentication is currently not supported".
pub const ANTHROPIC_OAUTH_BETAS: &str =
    "oauth-2025-04-20,claude-code-20250219,interleaved-thinking-2025-05-14";

const SETUP_TOKEN_PREFIX: &str = "sk-ant-oat01-";
pub async fn validate_setup_token(
    http: &reqwest::Client,
    token: &str,
    base_url: &str,
) -> Result<bool> {
    if !token.starts_with(SETUP_TOKEN_PREFIX) {
        anyhow::bail!(
            "Invalid setup-token format. Expected token starting with {SETUP_TOKEN_PREFIX}\n\
             Generate one with: claude setup-token"
        );
    }
    // Use /v1/messages with a minimal request — /v1/models rejects OAuth tokens.
    let url = format!("{base_url}/v1/messages");
    let body = serde_json::json!({
        "model": "claude-haiku-4-5",
        "max_tokens": 1,
        "messages": [{"role": "user", "content": "hi"}]
    });
    let response = http
        .post(&url)
        .header("authorization", format!("Bearer {token}"))
        .header("anthropic-version", "2023-06-01")
        .header("anthropic-beta", ANTHROPIC_OAUTH_BETAS)
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|e| anyhow!("failed to validate setup-token: {e}"))?;
    let status = response.status();
    if status.is_success() {
        return Ok(true);
    }
    let resp_body = response.text().await.unwrap_or_default();
    eprintln!("setup-token validation failed: HTTP {status} — {resp_body}");
    Ok(false)
}

fn normalize_setup_token(input: &str) -> Result<String> {
    let trimmed = input.trim().to_string();
    if trimmed.is_empty() {
        anyhow::bail!("setup-token cannot be empty");
    }
    Ok(trimmed)
}

#[cfg(test)]
mod tests {
    use super::{normalize_setup_token, profile_from_setup_token, validate_setup_token};
    use crate::AuthProfile;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn validate_setup_token_returns_true_on_2xx() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header(
                "authorization",
                "Bearer sk-ant-oat01-test-token-abc123",
            ))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let ok = validate_setup_token(
            &reqwest::Client::new(),
            "sk-ant-oat01-test-token-abc123",
            &server.uri(),
        )
        .await
        .expect("request should succeed");

        assert!(ok);
    }

    #[test]
    fn normalize_setup_token_rejects_empty() {
        assert!(normalize_setup_token("  \n").is_err());
    }

    #[test]
    fn profile_from_setup_token_maps_to_anthropic_session() {
        let profile = profile_from_setup_token("setup-token-123");
        assert!(matches!(
            profile,
            AuthProfile::AnthropicSession { session_token } if session_token == "setup-token-123"
        ));
    }
}
