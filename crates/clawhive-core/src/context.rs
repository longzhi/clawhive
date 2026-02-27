//! Context window management and compaction.
//!
//! This module provides:
//! - Token estimation for messages
//! - Context window tracking
//! - Automatic compaction when approaching limits
//! - Tool result pruning

use std::sync::Arc;

use anyhow::Result;
use clawhive_provider::LlmMessage;

use super::router::LlmRouter;

/// Approximate token count from text (chars / 4).
/// This is a rough estimate; actual tokenization varies by model.
pub fn estimate_tokens(text: &str) -> usize {
    text.len() / 4
}

/// Estimate tokens for a single message.
pub fn estimate_message_tokens(msg: &LlmMessage) -> usize {
    let mut total = 0;
    for block in &msg.content {
        match block {
            clawhive_provider::ContentBlock::Text { text } => {
                total += estimate_tokens(text);
            }
            clawhive_provider::ContentBlock::Image { data, .. } => {
                // Rough estimate: ~85 tokens per 1KB of base64 image data
                total += data.len() / 12;
            }
            clawhive_provider::ContentBlock::ToolUse { input, .. } => {
                total += estimate_tokens(&input.to_string());
            }
            clawhive_provider::ContentBlock::ToolResult { content, .. } => {
                total += estimate_tokens(content);
            }
        }
    }
    total.max(10) // Minimum overhead per message
}

/// Estimate total tokens for a list of messages.
pub fn estimate_messages_tokens(messages: &[LlmMessage]) -> usize {
    messages.iter().map(estimate_message_tokens).sum()
}

/// Context window configuration.
#[derive(Debug, Clone)]
pub struct ContextConfig {
    /// Maximum context window size in tokens (default: 128000)
    pub max_tokens: usize,
    /// Target tokens after compaction (default: max_tokens * 0.5)
    pub target_tokens: usize,
    /// Reserve tokens for response (default: 4096)
    pub reserve_tokens: usize,
    /// Minimum messages to keep (never compact below this)
    pub min_messages: usize,
    /// Memory flush configuration
    pub memory_flush: MemoryFlushConfig,
}

/// Configuration for pre-compaction memory flush.
#[derive(Debug, Clone)]
pub struct MemoryFlushConfig {
    /// Whether memory flush is enabled
    pub enabled: bool,
    /// Tokens remaining before triggering flush (default: 8000)
    pub soft_threshold_tokens: usize,
    /// System prompt for memory flush
    pub system_prompt: String,
    /// User prompt for memory flush
    pub prompt: String,
}

impl Default for MemoryFlushConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            soft_threshold_tokens: 8000,
            system_prompt: "Session nearing compaction. Store any durable memories now.".into(),
            prompt: "Write any important notes to memory files before context is compacted. Reply with NO_REPLY if nothing to store.".into(),
        }
    }
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            max_tokens: 128_000,
            target_tokens: 64_000,
            reserve_tokens: 4096,
            min_messages: 4,
            memory_flush: MemoryFlushConfig::default(),
        }
    }
}

impl ContextConfig {
    /// Create config for a specific model's context window.
    pub fn for_model(context_window: usize) -> Self {
        Self {
            max_tokens: context_window,
            target_tokens: context_window / 2,
            reserve_tokens: 4096,
            min_messages: 4,
            memory_flush: MemoryFlushConfig::default(),
        }
    }

    /// Available tokens for messages (max - reserve).
    pub fn available_tokens(&self) -> usize {
        self.max_tokens.saturating_sub(self.reserve_tokens)
    }
}

/// Check if messages are approaching the context limit.
pub fn should_compact(messages: &[LlmMessage], config: &ContextConfig) -> bool {
    let tokens = estimate_messages_tokens(messages);
    tokens > config.available_tokens()
}

/// Prune tool results from older messages to reduce context size.
///
/// This is a soft pruning that keeps message structure intact but
/// truncates large tool results.
pub fn prune_tool_results(
    messages: &mut [LlmMessage],
    max_tool_result_chars: usize,
    keep_last_n: usize,
) {
    let len = messages.len();
    if len <= keep_last_n {
        return;
    }

    for msg in messages.iter_mut().take(len - keep_last_n) {
        for block in msg.content.iter_mut() {
            if let clawhive_provider::ContentBlock::ToolResult { content, .. } = block {
                if content.len() > max_tool_result_chars {
                    let head = &content[..max_tool_result_chars / 2];
                    let tail = &content[content.len() - max_tool_result_chars / 2..];
                    *content = format!(
                        "{}...[truncated {} chars]...{}",
                        head,
                        content.len() - max_tool_result_chars,
                        tail
                    );
                }
            }
        }
    }
}

const COMPACTION_SYSTEM_PROMPT: &str = r#"You are a conversation summarizer. Your task is to create a concise summary of the conversation history that preserves:
1. Key decisions and conclusions
2. Important context and facts mentioned
3. Current state of any ongoing tasks
4. User preferences expressed

Output a clear, structured summary. Do not include pleasantries or filler."#;

/// Compaction result.
#[derive(Debug)]
pub struct CompactionResult {
    /// Summary of the compacted messages
    pub summary: String,
    /// Number of messages that were compacted
    pub compacted_count: usize,
    /// Tokens saved by compaction
    pub tokens_saved: usize,
}

/// Result of checking context window state.
#[derive(Debug)]
pub enum ContextCheckResult {
    /// Context is fine, no action needed
    Ok,
    /// Memory flush should be triggered before compaction
    NeedsMemoryFlush {
        /// System prompt for memory flush
        system_prompt: String,
        /// User prompt for memory flush
        prompt: String,
    },
    /// Compaction was performed
    Compacted(CompactionResult),
}

/// Compact older messages into a summary.
///
/// Returns the compacted messages (summary + recent messages).
pub async fn compact_messages(
    router: &LlmRouter,
    model: &str,
    messages: Vec<LlmMessage>,
    config: &ContextConfig,
) -> Result<(Vec<LlmMessage>, CompactionResult)> {
    let current_tokens = estimate_messages_tokens(&messages);

    // If we're under the target, no compaction needed
    if current_tokens <= config.target_tokens {
        return Ok((
            messages,
            CompactionResult {
                summary: String::new(),
                compacted_count: 0,
                tokens_saved: 0,
            },
        ));
    }

    // Find the split point: keep recent messages, compact older ones
    let mut keep_tokens = 0;
    let mut split_idx = messages.len();

    for (i, msg) in messages.iter().enumerate().rev() {
        let msg_tokens = estimate_message_tokens(msg);
        if keep_tokens + msg_tokens > config.target_tokens / 2 {
            split_idx = i + 1;
            break;
        }
        keep_tokens += msg_tokens;
    }

    // Ensure we keep at least min_messages
    split_idx = split_idx.min(messages.len().saturating_sub(config.min_messages));

    if split_idx == 0 {
        // Nothing to compact
        return Ok((
            messages,
            CompactionResult {
                summary: String::new(),
                compacted_count: 0,
                tokens_saved: 0,
            },
        ));
    }

    // Build the messages to compact
    let (to_compact, to_keep) = messages.split_at(split_idx);
    let compact_tokens = estimate_messages_tokens(to_compact);

    // Create summary request
    let mut summary_content = String::from("Please summarize this conversation:\n\n");
    for msg in to_compact {
        let role = &msg.role;
        for block in &msg.content {
            if let clawhive_provider::ContentBlock::Text { text } = block {
                summary_content.push_str(&format!("{role}: {text}\n\n"));
            }
        }
    }

    let summary_response = router
        .chat(
            model,
            &[],
            Some(COMPACTION_SYSTEM_PROMPT.to_string()),
            vec![LlmMessage::user(summary_content)],
            2048,
        )
        .await?;

    let summary = summary_response.text;
    let summary_tokens = estimate_tokens(&summary);

    // Build the new message list
    let mut compacted = vec![LlmMessage::user(format!(
        "[Previous conversation summary]\n{summary}"
    ))];
    compacted.extend(to_keep.iter().cloned());

    let tokens_saved = compact_tokens.saturating_sub(summary_tokens);

    Ok((
        compacted,
        CompactionResult {
            summary,
            compacted_count: split_idx,
            tokens_saved,
        },
    ))
}

/// Context manager that tracks token usage and triggers compaction.
#[derive(Clone)]
pub struct ContextManager {
    config: ContextConfig,
    router: Arc<LlmRouter>,
}

impl ContextManager {
    pub fn new(router: Arc<LlmRouter>, config: ContextConfig) -> Self {
        Self { config, router }
    }

    /// Check context state and determine what action is needed.
    /// Does NOT perform compaction - caller should handle based on result.
    pub fn check_context(&self, messages: &[LlmMessage]) -> ContextCheckResult {
        let tokens = estimate_messages_tokens(messages);
        let available = self.config.available_tokens();

        // Check if memory flush is needed (approaching limit but not over)
        if self.config.memory_flush.enabled {
            let flush_threshold =
                available.saturating_sub(self.config.memory_flush.soft_threshold_tokens);
            if tokens >= flush_threshold && tokens < available {
                return ContextCheckResult::NeedsMemoryFlush {
                    system_prompt: self.config.memory_flush.system_prompt.clone(),
                    prompt: self.config.memory_flush.prompt.clone(),
                };
            }
        }

        ContextCheckResult::Ok
    }

    /// Check if messages need compaction and perform it if necessary.
    pub async fn ensure_within_limits(
        &self,
        model: &str,
        mut messages: Vec<LlmMessage>,
    ) -> Result<(Vec<LlmMessage>, Option<CompactionResult>)> {
        // First try soft pruning
        prune_tool_results(&mut messages, 4000, 3);

        if !should_compact(&messages, &self.config) {
            return Ok((messages, None));
        }

        // Need to compact
        let (compacted, result) =
            compact_messages(&self.router, model, messages, &self.config).await?;

        tracing::info!(
            "Compacted {} messages, saved {} tokens",
            result.compacted_count,
            result.tokens_saved
        );

        Ok((compacted, Some(result)))
    }

    /// Get current token estimate for messages.
    pub fn estimate_tokens(&self, messages: &[LlmMessage]) -> usize {
        estimate_messages_tokens(messages)
    }

    /// Check if approaching context limit.
    pub fn is_approaching_limit(&self, messages: &[LlmMessage]) -> bool {
        let tokens = estimate_messages_tokens(messages);
        tokens > self.config.available_tokens() * 80 / 100 // 80% threshold
    }

    /// Check if we should trigger memory flush before compaction.
    /// Returns true if:
    /// 1. Memory flush is enabled
    /// 2. Tokens are approaching the flush threshold
    /// 3. Compaction would be needed soon
    pub fn should_trigger_memory_flush(&self, messages: &[LlmMessage]) -> bool {
        if !self.config.memory_flush.enabled {
            return false;
        }

        let tokens = estimate_messages_tokens(messages);
        let flush_threshold = self
            .config
            .available_tokens()
            .saturating_sub(self.config.memory_flush.soft_threshold_tokens);

        tokens >= flush_threshold && tokens < self.config.available_tokens()
    }

    /// Get memory flush prompts.
    pub fn memory_flush_prompts(&self) -> (&str, &str) {
        (
            &self.config.memory_flush.system_prompt,
            &self.config.memory_flush.prompt,
        )
    }

    /// Get the context config.
    pub fn config(&self) -> &ContextConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_estimate_tokens() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("hello"), 1); // 5 chars / 4 = 1
        assert_eq!(estimate_tokens("hello world test"), 4); // 16 chars / 4 = 4
    }

    #[test]
    fn test_context_config_default() {
        let config = ContextConfig::default();
        assert_eq!(config.max_tokens, 128_000);
        assert_eq!(config.available_tokens(), 128_000 - 4096);
    }

    #[test]
    fn test_context_config_for_model() {
        let config = ContextConfig::for_model(200_000);
        assert_eq!(config.max_tokens, 200_000);
        assert_eq!(config.target_tokens, 100_000);
    }

    #[test]
    fn test_should_compact() {
        let config = ContextConfig {
            max_tokens: 1000,
            target_tokens: 500,
            reserve_tokens: 100,
            min_messages: 2,
            memory_flush: MemoryFlushConfig::default(),
        };

        // Small messages - no compact needed
        let small_msgs = vec![LlmMessage::user("hello".to_string())];
        assert!(!should_compact(&small_msgs, &config));

        // Large message - should compact
        let large_text = "a".repeat(4000); // ~1000 tokens
        let large_msgs = vec![LlmMessage::user(large_text)];
        assert!(should_compact(&large_msgs, &config));
    }

    #[test]
    fn test_prune_tool_results() {
        let mut messages = vec![
            LlmMessage {
                role: "user".into(),
                content: vec![clawhive_provider::ContentBlock::ToolResult {
                    tool_use_id: "1".into(),
                    content: "a".repeat(10000),
                    is_error: false,
                }],
            },
            LlmMessage::user("recent".to_string()),
        ];

        prune_tool_results(&mut messages, 200, 1);

        // First message should be truncated, second kept
        if let clawhive_provider::ContentBlock::ToolResult { content, .. } = &messages[0].content[0]
        {
            assert!(content.len() < 10000);
            assert!(content.contains("truncated"));
        } else {
            panic!("Expected tool result");
        }
    }

    #[test]
    fn test_memory_flush_config_default() {
        let config = MemoryFlushConfig::default();
        assert!(config.enabled);
        assert_eq!(config.soft_threshold_tokens, 8000);
    }

    #[test]
    fn test_context_config_includes_memory_flush() {
        let config = ContextConfig::default();
        assert!(config.memory_flush.enabled);
    }
}
