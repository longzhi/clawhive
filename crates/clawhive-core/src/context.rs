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

/// Approximate token count from text.
/// CJK characters average ~1.5 tokens each; ASCII/Latin averages ~0.25 tokens per char.
/// Uses char count (not byte count) to avoid UTF-8 multi-byte skew.
pub fn estimate_tokens(text: &str) -> usize {
    let mut weighted = 0usize;
    for ch in text.chars() {
        if is_cjk_range(ch) {
            weighted += 6; // 6/4 = 1.5 tokens per CJK char
        } else {
            weighted += 1; // 1/4 = 0.25 tokens per ASCII char
        }
    }
    weighted / 4
}

/// Returns true for CJK ideographs, kana, hangul, and CJK punctuation.
fn is_cjk_range(ch: char) -> bool {
    matches!(ch,
        '\u{4E00}'..='\u{9FFF}'   // CJK Unified Ideographs
        | '\u{3400}'..='\u{4DBF}' // CJK Extension A
        | '\u{F900}'..='\u{FAFF}' // CJK Compatibility Ideographs
        | '\u{3000}'..='\u{303F}' // CJK Symbols and Punctuation
        | '\u{3040}'..='\u{30FF}' // Hiragana + Katakana
        | '\u{AC00}'..='\u{D7AF}' // Hangul Syllables
        | '\u{FF00}'..='\u{FFEF}' // Fullwidth Forms
    )
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
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            max_tokens: 128_000,
            target_tokens: 64_000,
            reserve_tokens: 4096,
            min_messages: 4,
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
        }
    }

    /// Available tokens for messages (max - reserve).
    pub fn available_tokens(&self) -> usize {
        self.max_tokens.saturating_sub(self.reserve_tokens)
    }
}

/// Check if messages are approaching the context limit.
/// Triggers at 75% of available tokens to compact proactively,
/// preventing the model from slowing down and losing focus in long tasks.
pub fn should_compact(messages: &[LlmMessage], config: &ContextConfig) -> bool {
    let tokens = estimate_messages_tokens(messages);
    tokens > config.available_tokens() * 75 / 100
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
                    let half = max_tool_result_chars / 2;
                    let head_end = content.floor_char_boundary(half);
                    let tail_start = content.ceil_char_boundary(content.len() - half);
                    let head = &content[..head_end];
                    let tail = &content[tail_start..];
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
    compaction_semaphore: Arc<tokio::sync::Semaphore>,
}

impl ContextManager {
    pub fn new(router: Arc<LlmRouter>, config: ContextConfig) -> Self {
        Self {
            config,
            router,
            compaction_semaphore: Arc::new(tokio::sync::Semaphore::new(1)),
        }
    }

    /// Return a new ContextManager with config adjusted for the given context window.
    /// Used to get per-model context limits in multi-agent scenarios.
    /// Shares the compaction semaphore so concurrent requests for the same agent
    /// cannot trigger parallel compactions.
    pub fn for_context_window(&self, context_window: usize) -> Self {
        Self {
            config: ContextConfig::for_model(context_window),
            router: self.router.clone(),
            compaction_semaphore: self.compaction_semaphore.clone(),
        }
    }

    /// Check context state and determine what action is needed.
    /// Does NOT perform compaction - caller should handle based on result.
    pub fn check_context(&self, messages: &[LlmMessage]) -> ContextCheckResult {
        let _ = messages;
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

        let _permit = match tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.compaction_semaphore.clone().acquire_owned(),
        )
        .await
        {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) => return Err(anyhow::anyhow!("compaction semaphore closed")),
            Err(_) => {
                tracing::warn!("compaction semaphore timeout after 30s, skipping compaction");
                return Ok((messages, None));
            }
        };

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

    /// Get the context config.
    pub fn config(&self) -> &ContextConfig {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use clawhive_provider::ProviderRegistry;

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
    fn test_context_check_never_requests_memory_write_before_compaction() {
        let router = Arc::new(LlmRouter::new(
            ProviderRegistry::new(),
            HashMap::new(),
            vec![],
        ));
        let manager = ContextManager::new(router, ContextConfig::default());
        let large_text = "a".repeat(200_000);
        let messages = vec![LlmMessage::user(large_text)];

        assert!(matches!(
            manager.check_context(&messages),
            ContextCheckResult::Ok
        ));
    }

    #[test]
    fn test_context_manager_shares_compaction_semaphore_across_windows() {
        let router = Arc::new(LlmRouter::new(
            ProviderRegistry::new(),
            HashMap::new(),
            vec![],
        ));
        let manager = ContextManager::new(router, ContextConfig::default());
        let adjusted = manager.for_context_window(32_000);

        assert_eq!(manager.compaction_semaphore.available_permits(), 1);
        assert_eq!(adjusted.compaction_semaphore.available_permits(), 1);

        let _permit = manager
            .compaction_semaphore
            .try_acquire()
            .expect("manager semaphore should have one permit");

        assert_eq!(manager.compaction_semaphore.available_permits(), 0);
        assert_eq!(adjusted.compaction_semaphore.available_permits(), 0);
    }
}
