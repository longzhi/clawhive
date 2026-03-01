use std::path::Path;
use std::sync::Arc;

use clawhive_core::access_gate::AccessGate;
use clawhive_core::config::{ExecSecurityConfig, SandboxPolicyConfig};
use clawhive_core::file_tools::{ReadFileTool, WriteFileTool};
use clawhive_core::shell_tool::ExecuteCommandTool;
use clawhive_core::skill::SkillRegistry;
use clawhive_core::tool::{ToolContext, ToolExecutor};
use clawhive_core::web_fetch_tool::WebFetchTool;

fn create_skill_with_permissions(dir: &Path, name: &str, permissions_yaml: &str) {
    let skill_dir = dir.join(name);
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        format!(
            "---\nname: {name}\ndescription: test skill\npermissions:\n{permissions_yaml}\n---\nBody"
        ),
    )
    .unwrap();
}

fn create_skill_without_permissions(dir: &Path, name: &str) {
    let skill_dir = dir.join(name);
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: test skill\n---\nBody"),
    )
    .unwrap();
}

fn context_from_registry(registry: &SkillRegistry, _workspace: &Path) -> ToolContext {
    match registry.merged_permissions() {
        Some(perms) => ToolContext::external(perms),
        None => ToolContext::builtin(),
    }
}

#[tokio::test]
async fn e2e_skill_with_fs_permissions_allows_matching_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("allowed.txt"), "hello").unwrap();

    let skills_dir = tmp.path().join("skills");
    create_skill_with_permissions(
        &skills_dir,
        "reader",
        &format!("  fs:\n    read:\n      - \"{}/**\"", workspace.display()),
    );

    let registry = SkillRegistry::load_from_dir(&skills_dir).unwrap();
    let ctx = context_from_registry(&registry, &workspace);

    let gate = Arc::new(AccessGate::new(
        workspace.clone(),
        workspace.join("access_policy.json"),
    ));
    let tool = ReadFileTool::new(workspace.clone(), gate);
    let result = tool
        .execute(serde_json::json!({"path": "allowed.txt"}), &ctx)
        .await
        .unwrap();
    assert!(!result.is_error, "Should allow reading: {}", result.content);
    assert!(result.content.contains("hello"));
}

#[tokio::test]
async fn e2e_skill_with_fs_permissions_denies_write_when_only_read_declared() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();

    let skills_dir = tmp.path().join("skills");
    create_skill_with_permissions(
        &skills_dir,
        "readonly",
        &format!("  fs:\n    read:\n      - \"{}/**\"", workspace.display()),
    );

    let registry = SkillRegistry::load_from_dir(&skills_dir).unwrap();
    let ctx = context_from_registry(&registry, &workspace);

    let gate = Arc::new(AccessGate::new(
        workspace.clone(),
        workspace.join("access_policy.json"),
    ));
    let tool = WriteFileTool::new(workspace.clone(), gate);
    let result = tool
        .execute(
            serde_json::json!({"path": "secret.txt", "content": "hack"}),
            &ctx,
        )
        .await
        .unwrap();
    assert!(result.is_error, "Should deny write: {}", result.content);
    assert!(result.content.contains("denied"));
}

#[tokio::test]
async fn e2e_skill_with_network_permissions_denies_unlisted_host() {
    let tmp = tempfile::tempdir().unwrap();
    let skills_dir = tmp.path().join("skills");
    create_skill_with_permissions(
        &skills_dir,
        "api-only",
        "  network:\n    allow:\n      - \"api.allowed.com:443\"",
    );

    let registry = SkillRegistry::load_from_dir(&skills_dir).unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let ctx = context_from_registry(&registry, &workspace);

    let tool = WebFetchTool::new();
    let result = tool
        .execute(serde_json::json!({"url": "https://evil.com/steal"}), &ctx)
        .await
        .unwrap();
    assert!(result.is_error, "Should deny network: {}", result.content);
    assert!(result.content.contains("denied"));
}

#[tokio::test]
async fn e2e_no_permissions_uses_builtin_context() {
    // Skills without permissions use builtin context, which allows network
    // (but still subject to hard baseline SSRF protection)
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();

    let skills_dir = tmp.path().join("skills");
    create_skill_without_permissions(&skills_dir, "plain");

    let registry = SkillRegistry::load_from_dir(&skills_dir).unwrap();
    assert!(registry.merged_permissions().is_none());

    let ctx = context_from_registry(&registry, &workspace);

    // Builtin context allows external network
    let tool = WebFetchTool::new();
    let result = tool
        .execute(serde_json::json!({"url": "https://example.com"}), &ctx)
        .await
        .unwrap();
    // This should succeed (or fail for network reasons, not policy)
    // For testing, we just check it's not a policy denial
    if result.is_error {
        assert!(
            !result.content.contains("denied for"),
            "Should not be denied by policy: {}",
            result.content
        );
    }
}

#[tokio::test]
async fn e2e_multiple_skills_union_permissions() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("data.txt"), "test data").unwrap();

    let skills_dir = tmp.path().join("skills");
    create_skill_with_permissions(
        &skills_dir,
        "skill-a",
        &format!("  fs:\n    read:\n      - \"{}/**\"", workspace.display()),
    );
    create_skill_with_permissions(&skills_dir, "skill-b", "  exec:\n    - sh\n    - cat");

    let registry = SkillRegistry::load_from_dir(&skills_dir).unwrap();
    let merged = registry.merged_permissions().unwrap();

    assert!(
        !merged.fs.read.is_empty(),
        "Should have fs.read from skill-a"
    );
    assert!(
        merged.exec.contains(&"cat".to_string()),
        "Should have exec from skill-b"
    );
}

#[tokio::test]
async fn e2e_shell_tool_with_skill_permissions() {
    let tmp = tempfile::tempdir().unwrap();
    let workspace = tmp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("hello.txt"), "sandbox works").unwrap();

    let skills_dir = tmp.path().join("skills");
    create_skill_with_permissions(
        &skills_dir,
        "shell-skill",
        &format!(
            "  fs:\n    read:\n      - \"{}/**\"\n  exec:\n    - sh\n    - cat",
            workspace.display()
        ),
    );

    let registry = SkillRegistry::load_from_dir(&skills_dir).unwrap();
    let ctx = context_from_registry(&registry, &workspace);

    let gate = Arc::new(AccessGate::new(
        workspace.clone(),
        workspace.join("access_policy.json"),
    ));
    let tool = ExecuteCommandTool::new(
        workspace.clone(),
        10,
        gate,
        ExecSecurityConfig::default(),
        SandboxPolicyConfig::default(),
        None,
        None,
        "test-agent".to_string(),
    );
    let result = tool
        .execute(serde_json::json!({"command": "cat hello.txt"}), &ctx)
        .await
        .unwrap();
    assert!(!result.is_error, "Should allow: {}", result.content);
    assert!(result.content.contains("sandbox works"));
}
