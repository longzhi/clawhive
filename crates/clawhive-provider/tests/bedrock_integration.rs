//! Integration tests for the Bedrock provider using wiremock.

use clawhive_provider::bedrock::sigv4::AwsCredentials;
use clawhive_provider::bedrock::BedrockProvider;
use clawhive_provider::{LlmProvider, LlmRequest};
use tokio_stream::StreamExt;
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

fn make_eventstream_frame(event_type: &str, message_type: &str, payload: &[u8]) -> Vec<u8> {
    let mut headers = Vec::new();
    for (name, value) in [(":event-type", event_type), (":message-type", message_type)] {
        headers.push(name.len() as u8);
        headers.extend_from_slice(name.as_bytes());
        headers.push(7); // string value type
        headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
        headers.extend_from_slice(value.as_bytes());
    }
    let total_length = 12 + headers.len() + payload.len() + 4;
    let mut frame = Vec::with_capacity(total_length);
    frame.extend_from_slice(&(total_length as u32).to_be_bytes());
    frame.extend_from_slice(&(headers.len() as u32).to_be_bytes());
    let prelude_crc = crc32fast::hash(&frame[0..8]);
    frame.extend_from_slice(&prelude_crc.to_be_bytes());
    frame.extend_from_slice(&headers);
    frame.extend_from_slice(payload);
    let msg_crc = crc32fast::hash(&frame);
    frame.extend_from_slice(&msg_crc.to_be_bytes());
    frame
}

#[tokio::test]
async fn stream_happy_path_text_only() {
    let server = MockServer::start().await;

    let mut body = Vec::new();
    body.extend_from_slice(&make_eventstream_frame(
        "messageStart",
        "event",
        br#"{"role":"assistant"}"#,
    ));
    body.extend_from_slice(&make_eventstream_frame(
        "contentBlockDelta",
        "event",
        br#"{"contentBlockIndex":0,"delta":{"text":"hello "}}"#,
    ));
    body.extend_from_slice(&make_eventstream_frame(
        "contentBlockDelta",
        "event",
        br#"{"contentBlockIndex":0,"delta":{"text":"world"}}"#,
    ));
    body.extend_from_slice(&make_eventstream_frame(
        "contentBlockStop",
        "event",
        br#"{"contentBlockIndex":0}"#,
    ));
    body.extend_from_slice(&make_eventstream_frame(
        "messageStop",
        "event",
        br#"{"stopReason":"end_turn"}"#,
    ));
    body.extend_from_slice(&make_eventstream_frame(
        "metadata",
        "event",
        br#"{"usage":{"inputTokens":4,"outputTokens":2}}"#,
    ));

    Mock::given(method("POST"))
        .and(path(
            "/model/anthropic.claude-3-5-sonnet-20241022-v2%3A0/converse-stream",
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(body),
        )
        .mount(&server)
        .await;

    let provider = BedrockProvider::new_with_base_url(creds(), "us-west-2", server.uri());
    let req = LlmRequest::simple(
        "anthropic.claude-3-5-sonnet-20241022-v2:0".into(),
        None,
        "ping".into(),
    );
    let mut stream = provider.stream(req).await.unwrap();
    let mut text = String::new();
    let mut got_final = false;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.unwrap();
        text.push_str(&chunk.delta);
        if chunk.is_final {
            got_final = true;
            assert_eq!(chunk.input_tokens, Some(4));
            assert_eq!(chunk.output_tokens, Some(2));
            assert_eq!(chunk.stop_reason.as_deref(), Some("end_turn"));
        }
    }
    assert_eq!(text, "hello world");
    assert!(got_final);
}

#[tokio::test]
async fn stream_exception_frame_becomes_error() {
    let server = MockServer::start().await;

    let mut body = Vec::new();
    body.extend_from_slice(&make_eventstream_frame(
        "messageStart",
        "event",
        br#"{"role":"assistant"}"#,
    ));
    body.extend_from_slice(&make_eventstream_frame(
        "ModelStreamErrorException",
        "exception",
        br#"{"message":"kaboom"}"#,
    ));

    Mock::given(method("POST"))
        .and(path("/model/x/converse-stream"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.amazon.eventstream")
                .set_body_bytes(body),
        )
        .mount(&server)
        .await;

    let provider = BedrockProvider::new_with_base_url(creds(), "us-west-2", server.uri());
    let req = LlmRequest::simple("x".into(), None, "ping".into());
    let mut stream = provider.stream(req).await.unwrap();
    let mut saw_error = false;
    while let Some(chunk) = stream.next().await {
        if let Err(e) = chunk {
            let s = e.to_string();
            assert!(s.contains("ModelStreamErrorException"), "got: {s}");
            assert!(s.contains("kaboom"), "got: {s}");
            saw_error = true;
            break;
        }
    }
    assert!(saw_error, "expected exception to surface as Err item");
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
