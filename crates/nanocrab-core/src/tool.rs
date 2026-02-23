use std::collections::HashMap;
use std::path::Path;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use corral_core::PolicyEngine;
use nanocrab_provider::ToolDef;

pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

pub struct ToolContext {
    policy: Option<PolicyEngine>,
}

impl ToolContext {
    pub fn new(policy: PolicyEngine) -> Self {
        Self {
            policy: Some(policy),
        }
    }

    pub fn default_policy(workspace: &Path) -> Self {
        let workspace_pattern = format!("{}/**", workspace.display());
        let perms = corral_core::Permissions::builder()
            .fs_read([workspace_pattern.clone()])
            .fs_write([workspace_pattern])
            .exec_allow(["sh"])
            .network_deny()
            .env_allow(["PATH", "HOME", "TMPDIR"])
            .build();
        Self {
            policy: Some(PolicyEngine::new(perms)),
        }
    }

    pub fn check_read(&self, path: &str) -> bool {
        self.policy
            .as_ref()
            .map_or(true, |p| p.check_path_read(path))
    }

    pub fn check_write(&self, path: &str) -> bool {
        self.policy
            .as_ref()
            .map_or(true, |p| p.check_path_write(path))
    }

    pub fn check_network(&self, host: &str, port: u16) -> bool {
        self.policy
            .as_ref()
            .map_or(true, |p| p.check_network(host, port))
    }

    pub fn check_exec(&self, cmd: &str) -> bool {
        self.policy.as_ref().map_or(true, |p| p.check_exec(cmd))
    }

    pub fn policy(&self) -> Option<&PolicyEngine> {
        self.policy.as_ref()
    }
}

#[async_trait]
pub trait ToolExecutor: Send + Sync {
    fn definition(&self) -> ToolDef;
    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput>;
}

pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn ToolExecutor>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    pub fn register(&mut self, tool: Box<dyn ToolExecutor>) {
        let name = tool.definition().name.clone();
        self.tools.insert(name, tool);
    }

    pub fn tool_defs(&self) -> Vec<ToolDef> {
        self.tools.values().map(|t| t.definition()).collect()
    }

    pub async fn execute(
        &self,
        name: &str,
        input: serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| anyhow!("tool not found: {name}"))?;
        tool.execute(input, ctx).await
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoTool;

    #[async_trait]
    impl ToolExecutor for EchoTool {
        fn definition(&self) -> ToolDef {
            ToolDef {
                name: "echo".into(),
                description: "Echo input".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "text": {"type": "string"}
                    },
                    "required": ["text"]
                }),
            }
        }

        async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
            let text = input["text"].as_str().unwrap_or("").to_string();
            Ok(ToolOutput {
                content: text,
                is_error: false,
            })
        }
    }

    #[test]
    fn registry_register_and_list() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(EchoTool));
        let defs = registry.tool_defs();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "echo");
    }

    #[tokio::test]
    async fn registry_execute_known_tool() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(EchoTool));
        let ctx = ToolContext::default_policy(std::path::Path::new("/tmp"));
        let result = registry
            .execute("echo", serde_json::json!({"text": "hello"}), &ctx)
            .await
            .unwrap();
        assert_eq!(result.content, "hello");
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn registry_execute_unknown_tool() {
        let registry = ToolRegistry::new();
        let ctx = ToolContext::default_policy(std::path::Path::new("/tmp"));
        let result = registry
            .execute("nonexistent", serde_json::json!({}), &ctx)
            .await;
        assert!(result.is_err());
    }

    #[test]
    fn tool_context_default_policy_is_conservative() {
        let ctx = ToolContext::default_policy(std::path::Path::new("/workspace"));
        assert!(ctx.policy().is_some());
        assert!(ctx.check_read("/workspace/file.txt"));
        assert!(!ctx.check_read("/etc/passwd"));
    }

    #[test]
    fn tool_context_with_custom_policy() {
        let perms = corral_core::Permissions::builder()
            .fs_read(["src/**"])
            .network_allow(["api.com:443"])
            .build();
        let ctx = ToolContext::new(corral_core::PolicyEngine::new(perms));
        assert!(ctx.policy().is_some());
    }
}
