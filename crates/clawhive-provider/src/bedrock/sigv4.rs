//! SigV4 signing for Bedrock Runtime requests.
//!
//! Thin wrapper around `aws-sigv4` that takes static credentials + request
//! parts and returns the set of HTTP headers to add to the outgoing request.
//! Deliberately does not pull in `aws-config` / `aws-sdk-*` / any credential
//! resolver chain — Bedrock provider receives credentials via config only.

use anyhow::{Context, Result};
use aws_credential_types::Credentials;
use aws_sigv4::http_request::{
    sign, PayloadChecksumKind, SignableBody, SignableRequest, SigningSettings,
};
use aws_sigv4::sign::v4;
use aws_smithy_runtime_api::client::identity::Identity;
use reqwest::header::{HeaderName, HeaderValue};
use std::time::SystemTime;

/// Static AWS credentials. No credential provider chain — caller supplies them directly.
#[derive(Debug, Clone)]
pub struct AwsCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
}

/// Service name for AWS Bedrock runtime (used in SigV4 credential scope).
///
/// Note: the hostname is `bedrock-runtime.{region}.amazonaws.com`, but the
/// SigV4 service name in the credential scope is just `bedrock`. Do not
/// "correct" this to `bedrock-runtime` — AWS will return `InvalidSignatureException`.
const SERVICE_NAME: &str = "bedrock";

/// Sign a Bedrock HTTP request with the current timestamp.
///
/// Returns the headers (name, value) pairs that the caller must add to the
/// outgoing request. Always includes `Authorization`, `x-amz-date`, and
/// `x-amz-content-sha256`. Also includes `x-amz-security-token` when the
/// supplied credentials carry a session token.
pub fn sign_bedrock_request(
    creds: &AwsCredentials,
    region: &str,
    method: &str,
    url: &str,
    body: &[u8],
) -> Result<Vec<(HeaderName, HeaderValue)>> {
    sign_bedrock_request_at(creds, region, method, url, body, SystemTime::now())
}

/// Same as [`sign_bedrock_request`], but with an explicit timestamp (for
/// deterministic tests).
pub fn sign_bedrock_request_at(
    creds: &AwsCredentials,
    region: &str,
    method: &str,
    url: &str,
    body: &[u8],
    now: SystemTime,
) -> Result<Vec<(HeaderName, HeaderValue)>> {
    let identity: Identity = Credentials::new(
        &creds.access_key_id,
        &creds.secret_access_key,
        creds.session_token.clone(),
        None,
        "clawhive-static-credentials",
    )
    .into();

    // Bedrock expects `x-amz-content-sha256` in the canonical request, so we
    // opt into `XAmzSha256` (default is `NoHeader`). `session_token_mode`
    // intentionally stays at the default (`Include`) — Bedrock signs the
    // session token into the canonical request.
    let mut settings = SigningSettings::default();
    settings.payload_checksum_kind = PayloadChecksumKind::XAmzSha256;
    let signing_params = v4::SigningParams::builder()
        .identity(&identity)
        .region(region)
        .name(SERVICE_NAME)
        .time(now)
        .settings(settings)
        .build()
        .context("build sigv4 signing params")?
        .into();

    let signable = SignableRequest::new(method, url, std::iter::empty(), SignableBody::Bytes(body))
        .context("construct signable request")?;

    let (instructions, _signature) = sign(signable, &signing_params)
        .context("sigv4 sign failed")?
        .into_parts();

    let (headers, _params) = instructions.into_parts();
    let mut out = Vec::with_capacity(headers.len());
    for h in headers {
        let name = HeaderName::from_bytes(h.name().as_bytes())
            .with_context(|| format!("invalid header name from sigv4: {}", h.name()))?;
        let value = HeaderValue::from_str(h.value())
            .with_context(|| format!("invalid header value from sigv4: {}", h.name()))?;
        out.push((name, value));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::{Duration, UNIX_EPOCH};

    fn fixed_time() -> SystemTime {
        // 2024-01-15T12:00:00Z
        UNIX_EPOCH + Duration::from_secs(1_705_320_000)
    }

    #[test]
    fn sign_produces_expected_headers() {
        let creds = AwsCredentials {
            access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            session_token: None,
        };
        let body = br#"{"messages":[]}"#;
        let url = "https://bedrock-runtime.us-west-2.amazonaws.com/model/anthropic.claude-3-5-sonnet-20241022-v2%3A0/converse";
        let headers =
            sign_bedrock_request_at(&creds, "us-west-2", "POST", url, body, fixed_time()).unwrap();
        let map: HashMap<String, String> = headers
            .iter()
            .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap().to_string()))
            .collect();
        assert!(map.contains_key("authorization"));
        assert!(map["authorization"].starts_with("AWS4-HMAC-SHA256 "));
        assert!(map["authorization"]
            .contains("Credential=AKIAIOSFODNN7EXAMPLE/20240115/us-west-2/bedrock/aws4_request"));
        assert_eq!(map["x-amz-date"], "20240115T120000Z");
        // Verify `x-amz-content-sha256` is the *actual* SHA256 of the body,
        // not a constant / UNSIGNED-PAYLOAD / precomputed stub.
        use sha2::{Digest, Sha256};
        let expected = hex::encode(Sha256::digest(body));
        assert_eq!(map["x-amz-content-sha256"], expected);
    }

    #[test]
    fn sign_different_bodies_produce_different_signatures() {
        let creds = AwsCredentials {
            access_key_id: "AKIA".into(),
            secret_access_key: "secret".into(),
            session_token: None,
        };
        let url = "https://bedrock-runtime.us-west-2.amazonaws.com/model/x/converse";
        let auth = |body: &[u8]| -> String {
            let headers =
                sign_bedrock_request_at(&creds, "us-west-2", "POST", url, body, fixed_time())
                    .unwrap();
            headers
                .into_iter()
                .find(|(k, _)| k.as_str() == "authorization")
                .map(|(_, v)| v.to_str().unwrap().to_string())
                .unwrap()
        };
        assert_ne!(auth(b"body-one"), auth(b"body-two"));
    }

    #[test]
    fn sign_includes_session_token_when_present() {
        let creds = AwsCredentials {
            access_key_id: "AKIA".into(),
            secret_access_key: "secret".into(),
            session_token: Some("FwoGZ...".into()),
        };
        let headers = sign_bedrock_request_at(
            &creds,
            "us-east-1",
            "POST",
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/x/converse",
            b"{}",
            fixed_time(),
        )
        .unwrap();
        assert!(headers
            .iter()
            .any(|(k, _)| k.as_str() == "x-amz-security-token"));
    }

    #[test]
    fn sign_omits_session_token_when_absent() {
        let creds = AwsCredentials {
            access_key_id: "AKIA".into(),
            secret_access_key: "secret".into(),
            session_token: None,
        };
        let headers = sign_bedrock_request_at(
            &creds,
            "us-east-1",
            "POST",
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/x/converse",
            b"{}",
            fixed_time(),
        )
        .unwrap();
        assert!(!headers
            .iter()
            .any(|(k, _)| k.as_str() == "x-amz-security-token"));
    }
}
