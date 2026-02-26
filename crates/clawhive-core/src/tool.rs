//! Tool execution framework with policy-based access control.
//!
//! This module provides:
//! - `ToolExecutor` trait for implementing tools
//! - `ToolRegistry` for managing available tools
//! - `ToolContext` for passing execution context and policy checks

use std::collections::HashMap;
use std::path::Path;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use clawhive_provider::ToolDef;

use super::policy::{PolicyContext, ToolOrigin};

/// Output from a tool execution.
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

/// A message from the conversation history.
#[derive(Debug, Clone)]
pub struct ConversationMessage {
    pub role: String,
    pub content: String,
}

/// Context passed to tool execution.
///
/// Contains:
/// - Policy context for permission checks (builtin vs external)
/// - Recent conversation messages for context-aware tools
/// - Source channel information for routing responses
#[derive(Clone)]
pub struct ToolContext {
    /// Policy context determines permission behavior
    policy_ctx: PolicyContext,
    /// Recent messages from the conversation
    recent_messages: Vec<ConversationMessage>,
    /// Source channel type (e.g., "discord", "telegram")
    source_channel_type: Option<String>,
    /// Source connector id (e.g., "dc_main", "tg_main")
    source_connector_id: Option<String>,
    /// Source conversation scope (e.g., "guild:123:channel:456")
    source_conversation_scope: Option<String>,
}

impl ToolContext {
    // ============================================================
    // Constructors
    // ============================================================

    /// Create a context for builtin tools (trusted, minimal policy checks).
    ///
    /// Builtin tools only check hard baseline (SSRF, sensitive paths, etc.)
    /// but skip permission declaration requirements.
    pub fn builtin() -> Self {
        Self {
            policy_ctx: PolicyContext::builtin(),
            recent_messages: Vec::new(),
            source_channel_type: None,
            source_connector_id: None,
            source_conversation_scope: None,
        }
    }

    /// Create a context for external skills (sandboxed, requires permissions).
    ///
    /// External skills must declare their required permissions in SKILL.md
    /// frontmatter. Only those declared permissions are allowed.
    pub fn external(permissions: corral_core::Permissions) -> Self {
        Self {
            policy_ctx: PolicyContext::external(permissions),
            recent_messages: Vec::new(),
            source_channel_type: None,
            source_connector_id: None,
            source_conversation_scope: None,
        }
    }

    /// Create a context with a corral PolicyEngine.
    ///
    /// This is for backward compatibility. The provided policy is used
    /// but the context is treated as external (strict checking).
    #[deprecated(note = "Use builtin() or external() instead")]
    pub fn new(policy: corral_core::PolicyEngine) -> Self {
        Self {
            policy_ctx: PolicyContext::external(policy.permissions().clone()),
            recent_messages: Vec::new(),
            source_channel_type: None,
            source_connector_id: None,
            source_conversation_scope: None,
        }
    }

    /// Create a default context scoped to a workspace directory.
    ///
    /// This creates a builtin context (trusted) for backward compatibility
    /// with code that doesn't yet distinguish builtin vs external.
    pub fn default_for_workspace(_workspace: &Path) -> Self {
        Self::builtin()
    }

    /// Legacy alias for default_for_workspace.
    #[deprecated(note = "Use builtin() or default_for_workspace() instead")]
    pub fn default_policy(workspace: &Path) -> Self {
        Self::default_for_workspace(workspace)
    }

    // ============================================================
    // Builder methods
    // ============================================================

    /// Add recent conversation messages for context-aware tools.
    pub fn with_recent_messages(mut self, recent_messages: Vec<ConversationMessage>) -> Self {
        self.recent_messages = recent_messages;
        self
    }

    /// Set the source channel information.
    pub fn with_source(
        mut self,
        channel_type: String,
        connector_id: String,
        conversation_scope: String,
    ) -> Self {
        self.source_channel_type = Some(channel_type);
        self.source_connector_id = Some(connector_id);
        self.source_conversation_scope = Some(conversation_scope);
        self
    }

    // ============================================================
    // Accessors
    // ============================================================

    /// Get the tool origin (builtin or external).
    pub fn origin(&self) -> ToolOrigin {
        self.policy_ctx.origin
    }

    /// Get the source channel type.
    pub fn source_channel_type(&self) -> Option<&str> {
        self.source_channel_type.as_deref()
    }

    /// Get the source connector ID.
    pub fn source_connector_id(&self) -> Option<&str> {
        self.source_connector_id.as_deref()
    }

    /// Get the source conversation scope.
    pub fn source_conversation_scope(&self) -> Option<&str> {
        self.source_conversation_scope.as_deref()
    }

    /// Get recent messages up to a limit.
    pub fn recent_messages(&self, limit: usize) -> Vec<ConversationMessage> {
        if limit == 0 || self.recent_messages.is_empty() {
            return Vec::new();
        }

        let start = self.recent_messages.len().saturating_sub(limit);
        self.recent_messages[start..].to_vec()
    }

    /// Get a reference to the underlying policy context.
    pub fn policy_context(&self) -> &PolicyContext {
        &self.policy_ctx
    }

    /// Legacy: get the corral PolicyEngine if this is an external context.
    #[deprecated(note = "Use policy_context() and check methods instead")]
    pub fn policy(&self) -> Option<&corral_core::PolicyEngine> {
        // This method can't really work with the new design since we don't
        // store a PolicyEngine directly anymore. Return None for now.
        None
    }

    // ============================================================
    // Permission checks
    // ============================================================

    /// Check if reading the given path is allowed.
    pub fn check_read(&self, path: &str) -> bool {
        self.policy_ctx.check_read(Path::new(path))
    }

    /// Check if writing to the given path is allowed.
    pub fn check_write(&self, path: &str) -> bool {
        self.policy_ctx.check_write(Path::new(path))
    }

    /// Check if network access to the given host:port is allowed.
    pub fn check_network(&self, host: &str, port: u16) -> bool {
        self.policy_ctx.check_network(host, port)
    }

    /// Check if executing the given command is allowed.
    pub fn check_exec(&self, cmd: &str) -> bool {
        self.policy_ctx.check_exec(cmd)
    }

    /// Check if accessing the given environment variable is allowed.
    pub fn check_env(&self, var_name: &str) -> bool {
        self.policy_ctx.check_env(var_name)
    }
}

/// Trait for implementing tools that can be invoked by the LLM.
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    /// Return the tool definition (name, description, schema).
    fn definition(&self) -> ToolDef;

    /// Execute the tool with the given input and context.
    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput>;
}

/// Registry of available tools.
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn ToolExecutor>>,
}

impl ToolRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    /// Register a tool.
    pub fn register(&mut self, tool: Box<dyn ToolExecutor>) {
        let name = tool.definition().name.clone();
        self.tools.insert(name, tool);
    }

    /// Get all tool definitions.
    pub fn tool_defs(&self) -> Vec<ToolDef> {
        self.tools.values().map(|t| t.definition()).collect()
    }

    /// Execute a tool by name.
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

    /// Check if the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Get the number of registered tools.
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Check if a tool exists.
    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
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

        async fn execute(
            &self,
            input: serde_json::Value,
            _ctx: &ToolContext,
        ) -> Result<ToolOutput> {
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
        let ctx = ToolContext::builtin();
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
        let ctx = ToolContext::builtin();
        let result = registry
            .execute("nonexistent", serde_json::json!({}), &ctx)
            .await;
        assert!(result.is_err());
    }

    #[test]
    fn builtin_context_allows_reads() {
        let ctx = ToolContext::builtin();
        assert!(ctx.check_read("/workspace/file.txt"));
        assert!(ctx.check_read("/etc/hosts"));
        // But hard baseline blocks sensitive files
        assert!(!ctx.check_read("/home/user/.ssh/id_rsa"));
    }

    #[test]
    fn builtin_context_blocks_hard_baseline() {
        let ctx = ToolContext::builtin();
        // Hard baseline denies these even for builtin
        assert!(!ctx.check_write("/etc/passwd"));
        assert!(!ctx.check_write("/home/user/.ssh/authorized_keys"));
        assert!(!ctx.check_network("192.168.1.1", 80));
        assert!(!ctx.check_exec("rm -rf /"));
    }

    #[test]
    fn external_context_requires_permissions() {
        let perms = corral_core::Permissions {
            fs: corral_core::FsPermissions {
                read: vec!["/workspace/**".into()],
                write: vec![],
            },
            network: corral_core::NetworkPermissions { allow: vec![] },
            exec: vec![],
            env: vec![],
            services: Default::default(),
        };

        let ctx = ToolContext::external(perms);

        // Allowed by permissions
        assert!(ctx.check_read("/workspace/data.txt"));

        // Not in permissions
        assert!(!ctx.check_read("/other/file.txt"));
        assert!(!ctx.check_write("/workspace/output.txt"));
        assert!(!ctx.check_exec("ls"));
    }

    #[test]
    fn context_with_source_info() {
        let ctx = ToolContext::builtin().with_source(
            "telegram".to_string(),
            "tg_main".to_string(),
            "chat:12345".to_string(),
        );

        assert_eq!(ctx.source_channel_type(), Some("telegram"));
        assert_eq!(ctx.source_connector_id(), Some("tg_main"));
        assert_eq!(ctx.source_conversation_scope(), Some("chat:12345"));
    }

    #[test]
    fn context_origin() {
        let builtin_ctx = ToolContext::builtin();
        assert_eq!(builtin_ctx.origin(), ToolOrigin::Builtin);

        let external_ctx = ToolContext::external(corral_core::Permissions::default());
        assert_eq!(external_ctx.origin(), ToolOrigin::External);
    }
}
