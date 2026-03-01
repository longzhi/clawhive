use clawhive_core::approval::ApprovalRegistry;
use clawhive_schema::ApprovalDecision;

#[tokio::test]
async fn request_and_resolve_returns_decision() {
    let registry = ApprovalRegistry::new();
    let trace_id = uuid::Uuid::new_v4();

    let rx = registry
        .request(trace_id, "ls -la".to_string(), "agent-1".to_string())
        .await;

    registry
        .resolve(trace_id, ApprovalDecision::AllowOnce)
        .await
        .expect("resolve should succeed");

    let decision = rx.await.expect("receiver should get decision");
    assert_eq!(decision, ApprovalDecision::AllowOnce);
}

#[tokio::test]
async fn resolve_unknown_trace_id_returns_error() {
    let registry = ApprovalRegistry::new();
    let err = registry
        .resolve(uuid::Uuid::new_v4(), ApprovalDecision::Deny)
        .await
        .expect_err("unknown trace id must fail");

    assert!(err.contains("No pending approval"));
}

#[tokio::test]
async fn pending_list_contains_registered_items() {
    let registry = ApprovalRegistry::new();
    let trace_1 = uuid::Uuid::new_v4();
    let trace_2 = uuid::Uuid::new_v4();

    let _rx1 = registry
        .request(trace_1, "git status".to_string(), "agent-a".to_string())
        .await;
    let _rx2 = registry
        .request(trace_2, "cargo test".to_string(), "agent-b".to_string())
        .await;

    let pending = registry.pending_list().await;

    assert_eq!(pending.len(), 2);
    assert!(pending
        .iter()
        .any(|(trace_id, cmd, agent)| *trace_id == trace_1
            && cmd == "git status"
            && agent == "agent-a"));
    assert!(pending
        .iter()
        .any(|(trace_id, cmd, agent)| *trace_id == trace_2
            && cmd == "cargo test"
            && agent == "agent-b"));
}

#[tokio::test]
async fn resolve_by_short_id_returns_decision() {
    let registry = ApprovalRegistry::new();
    let trace_id = uuid::Uuid::new_v4();
    let short_id = trace_id.to_string()[..8].to_string();

    let rx = registry
        .request(trace_id, "echo hi".to_string(), "agent-1".to_string())
        .await;

    registry
        .resolve_by_short_id(&short_id, ApprovalDecision::AlwaysAllow)
        .await
        .expect("resolve_by_short_id should succeed");

    let decision = rx.await.expect("receiver should get decision");
    assert_eq!(decision, ApprovalDecision::AlwaysAllow);
}

#[tokio::test]
async fn resolve_by_short_id_unknown_returns_error() {
    let registry = ApprovalRegistry::new();
    let err = registry
        .resolve_by_short_id("deadbeef", ApprovalDecision::Deny)
        .await
        .expect_err("unknown short id must fail");

    assert!(err.contains("No pending approval for short id"));
}
