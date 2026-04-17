//! Amazon Bedrock provider using the Converse API.
//!
//! Uses hand-rolled SigV4 signing (via `aws-sigv4`) and a hand-written
//! AWS event-stream decoder — no heavyweight AWS SDK dependencies.
//!
//! See `docs/plans/2026-04-17-bedrock-provider-design.md` for design rationale.

pub mod converse;
pub mod eventstream;
pub mod sigv4;

use std::time::Duration;

use crate::bedrock::sigv4::AwsCredentials;

/// Bedrock Runtime provider.
///
/// HTTP calls and Converse request/response plumbing are wired up in later
/// batches; this type currently exposes only the static URL builder used by
/// the HTTP path.
#[derive(Debug, Clone)]
pub struct BedrockProvider {
    // `http` and `creds` are consumed by the chat / stream paths landed in
    // Batch 4+; silence dead-code warnings until then to keep the clippy gate
    // green during incremental landing.
    #[allow(dead_code)]
    http: reqwest::Client,
    #[allow(dead_code)]
    creds: AwsCredentials,
    region: String,
    /// Override base URL — tests set this to point at wiremock. `None` means
    /// the real AWS endpoint is used.
    base_url: Option<String>,
}

impl BedrockProvider {
    pub fn new(creds: AwsCredentials, region: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .unwrap_or_default();
        Self {
            http,
            creds,
            region: region.into(),
            base_url: None,
        }
    }

    /// For testing against a wiremock server — overrides the AWS endpoint host.
    pub fn new_with_base_url(
        creds: AwsCredentials,
        region: impl Into<String>,
        base_url: impl Into<String>,
    ) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()
            .unwrap_or_default();
        Self {
            http,
            creds,
            region: region.into(),
            base_url: Some(base_url.into()),
        }
    }

    /// Build a Bedrock Runtime endpoint URL for the given model id.
    ///
    /// `streaming = true` targets `/converse-stream`; otherwise `/converse`.
    /// Model ids containing `:` (e.g. `anthropic.claude-3-5-sonnet-20241022-v2:0`)
    /// must be percent-encoded into the path.
    #[allow(dead_code)]
    pub(crate) fn build_url(&self, model_id: &str, streaming: bool) -> String {
        let encoded = urlencoding::encode(model_id);
        let suffix = if streaming {
            "converse-stream"
        } else {
            "converse"
        };
        match &self.base_url {
            Some(base) => format!(
                "{}/model/{}/{}",
                base.trim_end_matches('/'),
                encoded,
                suffix
            ),
            None => format!(
                "https://bedrock-runtime.{}.amazonaws.com/model/{}/{}",
                self.region, encoded, suffix
            ),
        }
    }

    #[allow(dead_code)]
    pub(crate) fn region(&self) -> &str {
        &self.region
    }
}

#[cfg(test)]
mod mod_tests {
    use super::*;

    fn fake_provider() -> BedrockProvider {
        BedrockProvider::new(
            AwsCredentials {
                access_key_id: "AKIA".into(),
                secret_access_key: "secret".into(),
                session_token: None,
            },
            "us-west-2",
        )
    }

    #[test]
    fn endpoint_encodes_colon_in_model_id() {
        let p = fake_provider();
        let url = p.build_url("anthropic.claude-3-5-sonnet-20241022-v2:0", false);
        assert_eq!(
            url,
            "https://bedrock-runtime.us-west-2.amazonaws.com/model/anthropic.claude-3-5-sonnet-20241022-v2%3A0/converse"
        );
    }

    #[test]
    fn endpoint_streaming_suffix() {
        let p = fake_provider();
        let url = p.build_url("meta.llama3-1-70b-instruct-v1:0", true);
        assert!(url.ends_with("/converse-stream"));
    }

    #[test]
    fn endpoint_inference_profile() {
        let p = fake_provider();
        let url = p.build_url("us.anthropic.claude-sonnet-4-20250514-v1:0", false);
        assert!(url.contains("us.anthropic.claude-sonnet-4-20250514-v1%3A0"));
    }

    #[test]
    fn endpoint_with_base_url_override() {
        let p = BedrockProvider::new_with_base_url(
            AwsCredentials {
                access_key_id: "k".into(),
                secret_access_key: "s".into(),
                session_token: None,
            },
            "us-east-1",
            "http://127.0.0.1:9999",
        );
        let url = p.build_url("some.model-v1:0", false);
        assert_eq!(
            url,
            "http://127.0.0.1:9999/model/some.model-v1%3A0/converse"
        );
    }
}
