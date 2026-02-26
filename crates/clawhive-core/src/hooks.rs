//! Hook system for extending agent lifecycle.
//!
//! Hooks allow intercepting and modifying behavior at key points:
//! - before_model_resolve: Override model selection
//! - before_prompt_build: Inject context into prompts
//! - before_tool_call / after_tool_call: Intercept tool execution
//! - before_compaction / after_compaction: Observe compaction
//! - message_received / message_sending: Message lifecycle

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use clawhive_provider::LlmMessage;
use tokio::sync::RwLock;

/// Hook context passed to all hooks.
#[derive(Debug, Clone)]
pub struct HookContext {
    /// Agent ID
    pub agent_id: String,
    /// Session key
    pub session_key: String,
    /// Current model
    pub model: String,
    /// Additional metadata
    pub metadata: HashMap<String, String>,
}

impl HookContext {
    pub fn new(agent_id: &str, session_key: &str, model: &str) -> Self {
        Self {
            agent_id: agent_id.to_string(),
            session_key: session_key.to_string(),
            model: model.to_string(),
            metadata: HashMap::new(),
        }
    }

    pub fn with_metadata(mut self, key: &str, value: &str) -> Self {
        self.metadata.insert(key.to_string(), value.to_string());
        self
    }
}

/// Result of a model resolution hook.
#[derive(Debug, Clone, Default)]
pub struct ModelResolveResult {
    /// Override the model (None = no change)
    pub model_override: Option<String>,
    /// Override thinking mode
    pub thinking_override: Option<String>,
}

/// Result of a prompt build hook.
#[derive(Debug, Clone, Default)]
pub struct PromptBuildResult {
    /// Prepend this context to the system prompt
    pub prepend_context: Option<String>,
    /// Append this context to the system prompt
    pub append_context: Option<String>,
    /// Override the entire system prompt
    pub system_prompt_override: Option<String>,
}

/// Tool call information.
#[derive(Debug, Clone)]
pub struct ToolCallInfo {
    pub tool_name: String,
    pub tool_id: String,
    pub input: serde_json::Value,
}

/// Result of a before_tool_call hook.
#[derive(Debug, Clone, Default)]
pub struct BeforeToolCallResult {
    /// Skip this tool call entirely
    pub skip: bool,
    /// Override the input
    pub input_override: Option<serde_json::Value>,
    /// Custom result to return instead of executing
    pub custom_result: Option<String>,
}

/// Result of an after_tool_call hook.
#[derive(Debug, Clone, Default)]
pub struct AfterToolCallResult {
    /// Override the tool result
    pub result_override: Option<String>,
    /// Mark as error
    pub is_error: Option<bool>,
}

/// Compaction information.
#[derive(Debug, Clone)]
pub struct CompactionInfo {
    pub messages_before: usize,
    pub messages_after: usize,
    pub tokens_saved: usize,
    pub summary: String,
}

/// Message information.
#[derive(Debug, Clone)]
pub struct MessageInfo {
    pub channel_type: String,
    pub connector_id: String,
    pub conversation_scope: String,
    pub text: String,
    pub is_inbound: bool,
}

/// Hook trait for implementing custom hooks.
#[async_trait]
pub trait Hook: Send + Sync {
    /// Hook name for identification.
    fn name(&self) -> &str;

    /// Called before model resolution.
    async fn before_model_resolve(&self, _ctx: &HookContext) -> anyhow::Result<ModelResolveResult> {
        Ok(ModelResolveResult::default())
    }

    /// Called before building the prompt, with access to messages.
    async fn before_prompt_build(
        &self,
        _ctx: &HookContext,
        _messages: &[LlmMessage],
    ) -> anyhow::Result<PromptBuildResult> {
        Ok(PromptBuildResult::default())
    }

    /// Called before executing a tool.
    async fn before_tool_call(
        &self,
        _ctx: &HookContext,
        _tool: &ToolCallInfo,
    ) -> anyhow::Result<BeforeToolCallResult> {
        Ok(BeforeToolCallResult::default())
    }

    /// Called after executing a tool.
    async fn after_tool_call(
        &self,
        _ctx: &HookContext,
        _tool: &ToolCallInfo,
        _result: &str,
        _is_error: bool,
    ) -> anyhow::Result<AfterToolCallResult> {
        Ok(AfterToolCallResult::default())
    }

    /// Called before compaction.
    async fn before_compaction(
        &self,
        _ctx: &HookContext,
        _message_count: usize,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    /// Called after compaction.
    async fn after_compaction(
        &self,
        _ctx: &HookContext,
        _info: &CompactionInfo,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    /// Called when a message is received.
    async fn message_received(
        &self,
        _ctx: &HookContext,
        _message: &MessageInfo,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    /// Called before a message is sent.
    async fn message_sending(
        &self,
        _ctx: &HookContext,
        _message: &mut MessageInfo,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    /// Called after a message is sent.
    async fn message_sent(&self, _ctx: &HookContext, _message: &MessageInfo) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Registry for managing hooks.
#[derive(Clone)]
pub struct HookRegistry {
    hooks: Arc<RwLock<Vec<Arc<dyn Hook>>>>,
}

impl HookRegistry {
    pub fn new() -> Self {
        Self {
            hooks: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Register a hook.
    pub async fn register(&self, hook: Arc<dyn Hook>) {
        let mut hooks = self.hooks.write().await;
        hooks.push(hook);
    }

    /// Run before_model_resolve hooks.
    pub async fn run_before_model_resolve(
        &self,
        ctx: &HookContext,
    ) -> anyhow::Result<ModelResolveResult> {
        let hooks = self.hooks.read().await;
        let mut result = ModelResolveResult::default();

        for hook in hooks.iter() {
            let hook_result = hook.before_model_resolve(ctx).await?;
            if hook_result.model_override.is_some() {
                result.model_override = hook_result.model_override;
            }
            if hook_result.thinking_override.is_some() {
                result.thinking_override = hook_result.thinking_override;
            }
        }

        Ok(result)
    }

    /// Run before_prompt_build hooks.
    pub async fn run_before_prompt_build(
        &self,
        ctx: &HookContext,
        messages: &[LlmMessage],
    ) -> anyhow::Result<PromptBuildResult> {
        let hooks = self.hooks.read().await;
        let mut result = PromptBuildResult::default();
        let mut prepends = Vec::new();
        let mut appends = Vec::new();

        for hook in hooks.iter() {
            let hook_result = hook.before_prompt_build(ctx, messages).await?;
            if let Some(prepend) = hook_result.prepend_context {
                prepends.push(prepend);
            }
            if let Some(append) = hook_result.append_context {
                appends.push(append);
            }
            if hook_result.system_prompt_override.is_some() {
                result.system_prompt_override = hook_result.system_prompt_override;
            }
        }

        if !prepends.is_empty() {
            result.prepend_context = Some(prepends.join("\n\n"));
        }
        if !appends.is_empty() {
            result.append_context = Some(appends.join("\n\n"));
        }

        Ok(result)
    }

    /// Run before_tool_call hooks.
    pub async fn run_before_tool_call(
        &self,
        ctx: &HookContext,
        tool: &ToolCallInfo,
    ) -> anyhow::Result<BeforeToolCallResult> {
        let hooks = self.hooks.read().await;
        let mut result = BeforeToolCallResult::default();

        for hook in hooks.iter() {
            let hook_result = hook.before_tool_call(ctx, tool).await?;
            if hook_result.skip {
                result.skip = true;
                result.custom_result = hook_result.custom_result;
                break;
            }
            if hook_result.input_override.is_some() {
                result.input_override = hook_result.input_override;
            }
            if hook_result.custom_result.is_some() {
                result.custom_result = hook_result.custom_result;
            }
        }

        Ok(result)
    }

    /// Run after_tool_call hooks.
    pub async fn run_after_tool_call(
        &self,
        ctx: &HookContext,
        tool: &ToolCallInfo,
        result_text: &str,
        is_error: bool,
    ) -> anyhow::Result<AfterToolCallResult> {
        let hooks = self.hooks.read().await;
        let mut result = AfterToolCallResult::default();

        for hook in hooks.iter() {
            let hook_result = hook
                .after_tool_call(ctx, tool, result_text, is_error)
                .await?;
            if hook_result.result_override.is_some() {
                result.result_override = hook_result.result_override;
            }
            if hook_result.is_error.is_some() {
                result.is_error = hook_result.is_error;
            }
        }

        Ok(result)
    }

    /// Run before_compaction hooks.
    pub async fn run_before_compaction(
        &self,
        ctx: &HookContext,
        message_count: usize,
    ) -> anyhow::Result<()> {
        let hooks = self.hooks.read().await;
        for hook in hooks.iter() {
            hook.before_compaction(ctx, message_count).await?;
        }
        Ok(())
    }

    /// Run after_compaction hooks.
    pub async fn run_after_compaction(
        &self,
        ctx: &HookContext,
        info: &CompactionInfo,
    ) -> anyhow::Result<()> {
        let hooks = self.hooks.read().await;
        for hook in hooks.iter() {
            hook.after_compaction(ctx, info).await?;
        }
        Ok(())
    }

    /// Run message_received hooks.
    pub async fn run_message_received(
        &self,
        ctx: &HookContext,
        message: &MessageInfo,
    ) -> anyhow::Result<()> {
        let hooks = self.hooks.read().await;
        for hook in hooks.iter() {
            hook.message_received(ctx, message).await?;
        }
        Ok(())
    }

    /// Run message_sending hooks.
    pub async fn run_message_sending(
        &self,
        ctx: &HookContext,
        message: &mut MessageInfo,
    ) -> anyhow::Result<()> {
        let hooks = self.hooks.read().await;
        for hook in hooks.iter() {
            hook.message_sending(ctx, message).await?;
        }
        Ok(())
    }

    /// Run message_sent hooks.
    pub async fn run_message_sent(
        &self,
        ctx: &HookContext,
        message: &MessageInfo,
    ) -> anyhow::Result<()> {
        let hooks = self.hooks.read().await;
        for hook in hooks.iter() {
            hook.message_sent(ctx, message).await?;
        }
        Ok(())
    }
}

impl Default for HookRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestHook {
        name: String,
        model_override: Option<String>,
    }

    #[async_trait]
    impl Hook for TestHook {
        fn name(&self) -> &str {
            &self.name
        }

        async fn before_model_resolve(
            &self,
            _ctx: &HookContext,
        ) -> anyhow::Result<ModelResolveResult> {
            Ok(ModelResolveResult {
                model_override: self.model_override.clone(),
                thinking_override: None,
            })
        }
    }

    #[tokio::test]
    async fn test_hook_registry_model_override() {
        let registry = HookRegistry::new();
        registry
            .register(Arc::new(TestHook {
                name: "test".to_string(),
                model_override: Some("custom-model".to_string()),
            }))
            .await;

        let ctx = HookContext::new("agent1", "session1", "default-model");
        let result = registry.run_before_model_resolve(&ctx).await.unwrap();

        assert_eq!(result.model_override, Some("custom-model".to_string()));
    }

    #[tokio::test]
    async fn test_hook_registry_no_hooks() {
        let registry = HookRegistry::new();
        let ctx = HookContext::new("agent1", "session1", "model");

        let result = registry.run_before_model_resolve(&ctx).await.unwrap();
        assert!(result.model_override.is_none());
    }

    struct PrependHook {
        prepend: String,
    }

    #[async_trait]
    impl Hook for PrependHook {
        fn name(&self) -> &str {
            "prepend"
        }

        async fn before_prompt_build(
            &self,
            _ctx: &HookContext,
            _messages: &[LlmMessage],
        ) -> anyhow::Result<PromptBuildResult> {
            Ok(PromptBuildResult {
                prepend_context: Some(self.prepend.clone()),
                append_context: None,
                system_prompt_override: None,
            })
        }
    }

    #[tokio::test]
    async fn test_multiple_hooks_combine() {
        let registry = HookRegistry::new();
        registry
            .register(Arc::new(PrependHook {
                prepend: "Context A".to_string(),
            }))
            .await;
        registry
            .register(Arc::new(PrependHook {
                prepend: "Context B".to_string(),
            }))
            .await;

        let ctx = HookContext::new("agent1", "session1", "model");
        let result = registry.run_before_prompt_build(&ctx, &[]).await.unwrap();

        assert_eq!(
            result.prepend_context,
            Some("Context A\n\nContext B".to_string())
        );
    }
}
