//! Integration tests for the Bedrock provider using wiremock.

use clawhive_provider::bedrock::sigv4::AwsCredentials;
use clawhive_provider::bedrock::BedrockProvider;
use clawhive_provider::{LlmProvider, LlmRequest};
use wiremock::matchers::{header_exists, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn creds() -> AwsCredentials {
    AwsCredentials {
        access_key_id: "AKIA_TEST".into(),
        secret_access_key: "test-secret".into(),
        session_token: None,
    }
}

#[tokio::test]
async fn chat_happy_path() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path(
            "/model/anthropic.claude-3-5-sonnet-20241022-v2%3A0/converse",
        ))
        .and(header_exists("authorization"))
        .and(header_exists("x-amz-date"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "output": {
                "message": {
                    "role": "assistant",
                    "content": [ {"text": "hello from bedrock"} ]
                }
            },
            "stopReason": "end_turn",
            "usage": { "inputTokens": 12, "outputTokens": 7 }
        })))
        .mount(&server)
        .await;

    let provider = BedrockProvider::new_with_base_url(creds(), "us-west-2", server.uri());
    let req = LlmRequest::simple(
        "anthropic.claude-3-5-sonnet-20241022-v2:0".into(),
        None,
        "ping".into(),
    );
    let resp = provider.chat(req).await.unwrap();
    assert_eq!(resp.text, "hello from bedrock");
    assert_eq!(resp.input_tokens, Some(12));
    assert_eq!(resp.output_tokens, Some(7));
    assert_eq!(resp.stop_reason.as_deref(), Some("end_turn"));
}

#[tokio::test]
async fn chat_api_error_4xx() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "message": "malformed input"
        })))
        .mount(&server)
        .await;

    let provider = BedrockProvider::new_with_base_url(creds(), "us-west-2", server.uri());
    let req = LlmRequest::simple(
        "anthropic.claude-3-5-sonnet-20241022-v2:0".into(),
        None,
        "x".into(),
    );
    let err = provider.chat(req).await.err().unwrap();
    let s = err.to_string();
    assert!(s.contains("400"), "expected 400 in: {s}");
    assert!(
        s.contains("malformed input"),
        "expected AWS message in: {s}"
    );
}

#[tokio::test]
#[ignore = "requires real AWS credentials; run with AWS_BEDROCK_TEST_*"]
async fn live_chat_smoke() {
    let access_key_id = std::env::var("AWS_BEDROCK_TEST_ACCESS_KEY_ID").unwrap();
    let secret_access_key = std::env::var("AWS_BEDROCK_TEST_SECRET_ACCESS_KEY").unwrap();
    let region = std::env::var("AWS_BEDROCK_TEST_REGION").unwrap_or_else(|_| "us-west-2".into());
    let model = std::env::var("AWS_BEDROCK_TEST_MODEL")
        .unwrap_or_else(|_| "anthropic.claude-3-5-haiku-20241022-v1:0".into());
    let creds = AwsCredentials {
        access_key_id,
        secret_access_key,
        session_token: std::env::var("AWS_BEDROCK_TEST_SESSION_TOKEN").ok(),
    };
    let provider = BedrockProvider::new(creds, region);
    let req = LlmRequest::simple(model, Some("Be brief.".into()), "say 'bedrock ok'".into());
    let resp = provider.chat(req).await.unwrap();
    assert!(!resp.text.is_empty(), "empty response");
    eprintln!("live response: {:?}", resp);
}
