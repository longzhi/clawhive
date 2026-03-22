#![cfg(feature = "weixin")]

use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use clawhive_channels::weixin::{ILinkClient, WeixinSession};

fn test_session(base_url: &str) -> WeixinSession {
    WeixinSession {
        bot_token: "test_bot_token_123".to_string(),
        base_url: base_url.to_string(),
        bot_id: "bot_001".to_string(),
        user_id: "owner_001".to_string(),
        saved_at: "2026-01-01T00:00:00Z".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Test 1: send_text posts correct structure and headers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn weixin_send_text_message() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/ilink/bot/sendmessage"))
        .and(header("AuthorizationType", "ilink_bot_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ret": 0,
            "errcode": 0,
            "errmsg": "ok"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let session = test_session(&server.uri());
    let client = ILinkClient::new(session, tmp.path());

    client
        .send_text("user@im.wechat", "hello", "ctx_token")
        .await
        .expect("send_text should succeed");

    // Verify the mock was called exactly once (expect(1) above ensures this on drop).
    // Additionally retrieve the request to validate body structure.
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1, "expected exactly one request");

    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["msg"]["to_user_id"], "user@im.wechat");
    assert_eq!(body["msg"]["item_list"][0]["type"], 1);
    assert_eq!(body["msg"]["item_list"][0]["text_item"]["text"], "hello");
    assert_eq!(body["msg"]["context_token"], "ctx_token");
}

// ---------------------------------------------------------------------------
// Test 2: get_updates parses messages correctly
// ---------------------------------------------------------------------------

#[tokio::test]
async fn weixin_getupdates_parses_messages() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/ilink/bot/getupdates"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ret": 0,
            "errcode": 0,
            "errmsg": "",
            "msgs": [{
                "message_id": 42,
                "from_user_id": "sender_abc",
                "to_user_id": "bot_001",
                "create_time_ms": 1700000000000_u64,
                "session_id": "sess_1",
                "group_id": "",
                "message_type": 1,
                "message_state": 0,
                "item_list": [{
                    "type": 1,
                    "text_item": { "text": "hi there" }
                }],
                "context_token": "ctx_abc"
            }],
            "get_updates_buf": "cursor_next"
        })))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let session = test_session(&server.uri());
    let client = ILinkClient::new(session, tmp.path());

    let resp = client
        .get_updates("")
        .await
        .expect("get_updates should succeed");

    assert_eq!(resp.ret, 0);
    assert_eq!(resp.msgs.len(), 1);
    assert_eq!(resp.msgs[0].from_user_id, "sender_abc");
    assert_eq!(resp.msgs[0].item_list.len(), 1);
    assert_eq!(
        resp.msgs[0].item_list[0].text_item.as_ref().unwrap().text,
        "hi there"
    );
    assert_eq!(resp.get_updates_buf, "cursor_next");
}

// ---------------------------------------------------------------------------
// Test 3: get_updates handles session expired (errcode -14)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn weixin_getupdates_session_expired() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/ilink/bot/getupdates"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ret": 0,
            "errcode": -14,
            "errmsg": "session expired",
            "msgs": [],
            "get_updates_buf": ""
        })))
        .mount(&server)
        .await;

    let tmp = tempfile::tempdir().unwrap();
    let session = test_session(&server.uri());
    let client = ILinkClient::new(session, tmp.path());

    let resp = client
        .get_updates("old_cursor")
        .await
        .expect("should parse even with errcode");

    assert_eq!(resp.errcode, -14);
    assert_eq!(resp.errmsg, "session expired");
    assert!(resp.msgs.is_empty());
}

// ---------------------------------------------------------------------------
// Test 4: cursor persistence (save + load round-trip)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn weixin_cursor_persistence() {
    let tmp = tempfile::tempdir().unwrap();
    let session = test_session("http://unused");
    let client = ILinkClient::new(session, tmp.path());

    // Initially empty
    assert_eq!(client.load_cursor(), "");

    // Save and reload
    client.save_cursor("cursor_v2");
    assert_eq!(client.load_cursor(), "cursor_v2");
}

// ---------------------------------------------------------------------------
// Test 5: context token in-memory cache
// ---------------------------------------------------------------------------

#[tokio::test]
async fn weixin_context_token_cache() {
    let tmp = tempfile::tempdir().unwrap();
    let session = test_session("http://unused");
    let client = ILinkClient::new(session, tmp.path());

    // No token yet
    assert!(client.get_context_token("user1").await.is_none());

    // Set and retrieve
    client.set_context_token("user1", "token_abc").await;
    assert_eq!(
        client.get_context_token("user1").await,
        Some("token_abc".to_string())
    );

    // Different user has no token
    assert!(client.get_context_token("user2").await.is_none());
}
