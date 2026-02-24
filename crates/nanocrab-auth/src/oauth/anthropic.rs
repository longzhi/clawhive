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

pub async fn validate_setup_token(http: &reqwest::Client, token: &str, endpoint: &str) -> Result<bool> {
    let response = http
        .get(endpoint)
        .header("authorization", format!("Bearer {token}"))
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .send()
        .await
        .map_err(|e| anyhow!("failed to validate setup-token: {e}"))?;

    Ok(response.status().is_success())
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

        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", "Bearer setup-token-123"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let ok = validate_setup_token(
            &reqwest::Client::new(),
            "setup-token-123",
            &format!("{}/v1/models", server.uri()),
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
