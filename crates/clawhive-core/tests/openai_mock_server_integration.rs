use std::collections::HashMap;
use std::sync::Arc;

use clawhive_core::*;
use clawhive_provider::{
    LlmMessage, LlmProvider, LlmRequest, OpenAiProvider, ProviderRegistry, ToolDef,
};
use tokio_stream::StreamExt;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn mock_openai_response(text: &str) -> serde_json::Value {
    serde_json::json!({
        "choices": [{
            "message": {"content": text},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 10, "completion_tokens": 5}
    })
}

fn mock_openai_tool_response(tool_id: &str, tool_name: &str, tool_args: &str) -> serde_json::Value {
    serde_json::json!({
        "choices": [{
            "message": {
                "content": null,
                "tool_calls": [{
                    "id": tool_id,
                    "type": "function",
                    "function": {
                        "name": tool_name,
                        "arguments": tool_args
                    }
                }]
            },
            "finish_reason": "tool_calls"
        }],
        "usage": {"prompt_tokens": 12, "completion_tokens": 8}
    })
}

fn mock_openai_error(status: u16, message: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_json(serde_json::json!({
        "error": {
            "type": "api_error",
            "message": message
        }
    }))
}

#[tokio::test]
async fn openai_basic_chat_with_header_verification() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .and(header("authorization", "Bearer test-key"))
        .and(header("content-type", "application/json"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(mock_openai_response("Hello from OpenAI!")),
        )
        .expect(1)
        .mount(&server)
        .await;

    let provider = OpenAiProvider::new("test-key", server.uri());
    let resp = provider
        .chat(LlmRequest {
            model: "gpt-4o".into(),
            system: Some("be helpful".into()),
            messages: vec![LlmMessage::user("hi")],
            max_tokens: 128,
            tools: vec![],
        })
        .await
        .unwrap();

    assert_eq!(resp.text, "Hello from OpenAI!");
    assert_eq!(resp.input_tokens, Some(10));
    assert_eq!(resp.output_tokens, Some(5));
    assert_eq!(resp.stop_reason.as_deref(), Some("end_turn"));
}

#[tokio::test]
async fn openai_tool_calling_with_stop_reason_normalization() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(mock_openai_tool_response(
                "call_123",
                "weather",
                r#"{"city":"tokyo"}"#,
            )),
        )
        .mount(&server)
        .await;

    let provider = OpenAiProvider::new("test-key", server.uri());
    let resp = provider
        .chat(LlmRequest {
            model: "gpt-4o".into(),
            system: None,
            messages: vec![LlmMessage::user("what's the weather in tokyo?")],
            max_tokens: 128,
            tools: vec![ToolDef {
                name: "weather".into(),
                description: "Get weather".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {"city": {"type": "string"}}
                }),
            }],
        })
        .await
        .unwrap();

    assert_eq!(resp.stop_reason.as_deref(), Some("tool_use"));
    assert_eq!(resp.content.len(), 1);
    match &resp.content[0] {
        clawhive_provider::ContentBlock::ToolUse { id, name, input } => {
            assert_eq!(id, "call_123");
            assert_eq!(name, "weather");
            assert_eq!(input["city"], "tokyo");
        }
        _ => panic!("expected ToolUse block"),
    }
}

#[tokio::test]
async fn openai_streaming_text_with_deltas() {
    let server = MockServer::start().await;

    let sse_response = "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"},\"finish_reason\":null}]}\n\n\
                        data: {\"choices\":[{\"delta\":{\"content\":\"lo\"},\"finish_reason\":null}]}\n\n\
                        data: {\"choices\":[{\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n\n\
                        data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":8,\"completion_tokens\":3}}\n\n\
                        data: [DONE]\n\n";

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(sse_response))
        .mount(&server)
        .await;

    let provider = OpenAiProvider::new("test-key", server.uri());
    let mut stream = provider
        .stream(LlmRequest {
            model: "gpt-4o".into(),
            system: None,
            messages: vec![LlmMessage::user("hello")],
            max_tokens: 64,
            tools: vec![],
        })
        .await
        .unwrap();

    let mut chunks = Vec::new();
    while let Some(result) = stream.next().await {
        chunks.push(result.unwrap());
    }

    assert_eq!(chunks.len(), 4);
    assert_eq!(chunks[0].delta, "Hel");
    assert!(!chunks[0].is_final);
    assert_eq!(chunks[1].delta, "lo");
    assert!(!chunks[1].is_final);
    assert_eq!(chunks[2].delta, " world");
    assert!(!chunks[2].is_final);
    assert!(chunks[3].is_final);
    assert_eq!(chunks[3].input_tokens, Some(8));
    assert_eq!(chunks[3].output_tokens, Some(3));
    assert_eq!(chunks[3].stop_reason.as_deref(), Some("end_turn"));
}

#[tokio::test]
async fn openai_streaming_tool_calls_with_accumulated_blocks() {
    let server = MockServer::start().await;

    let sse_response = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"weather\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n\
                        data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"city\\\"\"}}]},\"finish_reason\":null}]}\n\n\
                        data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\":\\\"tokyo\\\"}\"}}]},\"finish_reason\":null}]}\n\n\
                        data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n\
                        data: [DONE]\n\n";

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(sse_response))
        .mount(&server)
        .await;

    let provider = OpenAiProvider::new("test-key", server.uri());
    let mut stream = provider
        .stream(LlmRequest {
            model: "gpt-4o".into(),
            system: None,
            messages: vec![LlmMessage::user("weather?")],
            max_tokens: 64,
            tools: vec![ToolDef {
                name: "weather".into(),
                description: "Get weather".into(),
                input_schema: serde_json::json!({"type": "object"}),
            }],
        })
        .await
        .unwrap();

    let mut chunks = Vec::new();
    while let Some(result) = stream.next().await {
        chunks.push(result.unwrap());
    }

    let final_chunk = chunks.last().unwrap();
    assert!(final_chunk.is_final);
    assert_eq!(final_chunk.stop_reason.as_deref(), Some("tool_use"));
    assert_eq!(final_chunk.content_blocks.len(), 1);
    match &final_chunk.content_blocks[0] {
        clawhive_provider::ContentBlock::ToolUse { id, name, input } => {
            assert_eq!(id, "call_1");
            assert_eq!(name, "weather");
            assert_eq!(input["city"], "tokyo");
        }
        _ => panic!("expected ToolUse block"),
    }
}

#[tokio::test]
async fn openai_error_handling_401_not_retryable() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(mock_openai_error(401, "invalid api key"))
        .mount(&server)
        .await;

    let provider = OpenAiProvider::new("test-key", server.uri());
    let err = provider
        .chat(LlmRequest {
            model: "gpt-4o".into(),
            system: None,
            messages: vec![LlmMessage::user("test")],
            max_tokens: 64,
            tools: vec![],
        })
        .await
        .unwrap_err();

    let err_text = err.to_string();
    assert!(err_text.contains("openai api error"));
    assert!(err_text.contains("401"));
    assert!(!err_text.contains("[retryable]"));
}

#[tokio::test]
async fn openai_rate_limit_429_retry_via_router() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(mock_openai_error(429, "rate limited"))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(mock_openai_response("retry succeeded")),
        )
        .mount(&server)
        .await;

    let mut registry = ProviderRegistry::new();
    registry.register(
        "openai",
        Arc::new(OpenAiProvider::new("test-key", server.uri())),
    );
    let aliases = HashMap::from([("gpt4o".to_string(), "openai/gpt-4o".to_string())]);
    let router = LlmRouter::new(registry, aliases, vec![]);

    let resp = router
        .chat(
            "gpt4o",
            &[],
            None,
            vec![LlmMessage::user("retry test")],
            128,
        )
        .await
        .unwrap();

    assert!(resp.text.contains("retry succeeded"));
}

#[tokio::test]
async fn openai_connection_error_retryable() {
    let provider = OpenAiProvider::new("test-key", "http://127.0.0.1:9");
    let err = provider
        .chat(LlmRequest {
            model: "gpt-4o".into(),
            system: None,
            messages: vec![LlmMessage::user("ping")],
            max_tokens: 64,
            tools: vec![],
        })
        .await
        .unwrap_err();

    let err_text = err.to_string();
    assert!(err_text.contains("openai api error (connect)"));
    assert!(err_text.contains("[retryable]"));
}
