use clawhive_memory::SessionMessage;
use clawhive_provider::{ContentBlock, LlmMessage};

use crate::config::FullAgentConfig;
use crate::session::SessionResetPolicy;
use crate::tool::ConversationMessage;

pub(super) const SLOW_LLM_ROUND_WARN_MS: u64 = 30_000;
pub(super) const SLOW_TOOL_EXEC_WARN_MS: u64 = 10_000;

pub(super) fn build_messages_from_history(history_messages: &[SessionMessage]) -> Vec<LlmMessage> {
    let mut messages = Vec::new();
    let mut prev_timestamp = None;

    for hist_msg in history_messages {
        if let (Some(prev_ts), Some(curr_ts)) = (prev_timestamp, hist_msg.timestamp) {
            let gap: chrono::TimeDelta = curr_ts - prev_ts;
            if gap.num_minutes() >= 30 {
                let gap_text = format_time_gap(gap);
                messages.push(LlmMessage {
                    role: "user".to_string(),
                    content: vec![ContentBlock::Text {
                        text: format!(
                            "[{gap_text} of inactivity has passed since the last message]"
                        ),
                    }],
                });
            }
        }

        prev_timestamp = hist_msg.timestamp;

        messages.push(LlmMessage {
            role: hist_msg.role.clone(),
            content: vec![ContentBlock::Text {
                text: hist_msg.content.clone(),
            }],
        });
    }

    messages
}

pub(super) fn format_time_gap(gap: chrono::TimeDelta) -> String {
    let hours = gap.num_hours();
    let minutes = gap.num_minutes();
    if hours >= 24 {
        let days = hours / 24;
        format!("{days} day(s)")
    } else if hours >= 1 {
        format!("{hours} hour(s)")
    } else {
        format!("{minutes} minute(s)")
    }
}

pub(super) fn extract_source_after_prefix(text: &str, prefix: &str) -> Option<String> {
    let rest = text[prefix.len()..]
        .trim_start_matches([' ', ':', '\u{ff1a}'])
        .trim();
    if rest.is_empty() {
        None
    } else {
        Some(rest.to_string())
    }
}

pub(super) fn has_install_skill_intent_prefix(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }

    let lower = trimmed.to_ascii_lowercase();
    let en_prefixes = ["install skill from", "install this skill", "install skill"];
    if en_prefixes.iter().any(|prefix| lower.starts_with(prefix)) {
        return true;
    }

    let cn_prefixes = [
        "安装这个skill:",
        "安装这个 skill:",
        "安装skill:",
        "安装 skill:",
        "安装技能:",
        "安装这个skill",
        "安装这个 skill",
        "安装skill",
        "安装 skill",
        "安装技能",
    ];
    cn_prefixes.iter().any(|prefix| trimmed.starts_with(prefix))
}

pub(super) fn is_skill_install_intent_without_source(text: &str) -> bool {
    if !has_install_skill_intent_prefix(text) {
        return false;
    }
    detect_skill_install_intent(text).is_none()
}

pub fn detect_skill_install_intent(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    let lower = trimmed.to_ascii_lowercase();
    let en_prefixes = ["install skill from", "install this skill", "install skill"];
    for prefix in en_prefixes {
        if lower.starts_with(prefix) {
            return extract_source_after_prefix(trimmed, prefix);
        }
    }

    let cn_prefixes = [
        "安装这个skill:",
        "安装这个 skill:",
        "安装skill:",
        "安装 skill:",
        "安装技能:",
        "安装这个skill",
        "安装这个 skill",
        "安装skill",
        "安装 skill",
        "安装技能",
    ];
    for prefix in cn_prefixes {
        if trimmed.starts_with(prefix) {
            return extract_source_after_prefix(trimmed, prefix);
        }
    }

    None
}

pub(super) fn filter_no_reply(text: &str) -> String {
    let trimmed = text.trim();

    if trimmed == "NO_REPLY" {
        return String::new();
    }

    let text = trimmed
        .strip_prefix("NO_REPLY")
        .unwrap_or(trimmed)
        .strip_suffix("NO_REPLY")
        .unwrap_or(trimmed)
        .trim();

    text.to_string()
}

pub(super) fn is_slow_latency_ms(duration_ms: u64, threshold_ms: u64) -> bool {
    duration_ms >= threshold_ms
}

pub(super) fn history_message_limit(agent: &FullAgentConfig) -> usize {
    agent
        .memory_policy
        .as_ref()
        .and_then(|policy| policy.limit_history_turns)
        .map(|turns| (turns as usize) * 2)
        .unwrap_or(10)
}

pub(super) fn session_reset_policy_for(agent: &FullAgentConfig) -> SessionResetPolicy {
    let policy = agent.memory_policy.as_ref();
    let default_policy = SessionResetPolicy::default();
    SessionResetPolicy {
        idle_minutes: policy
            .and_then(|memory| memory.idle_minutes)
            .or(default_policy.idle_minutes),
        daily_at_hour: policy
            .and_then(|memory| memory.daily_at_hour)
            .or(default_policy.daily_at_hour),
    }
}

pub(super) fn is_explicit_web_search_request(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }

    let lower = trimmed.to_ascii_lowercase();
    lower.contains("web_search")
        || lower.contains("web search")
        || trimmed.contains("联网搜索")
        || trimmed.contains("上网搜索")
        || trimmed.contains("实时搜索")
}

pub(super) fn should_inject_web_search_reminder(
    must_use_web_search: bool,
    web_search_reminder_injected: bool,
    web_search_called: bool,
    tool_use_count: usize,
) -> bool {
    must_use_web_search
        && !web_search_reminder_injected
        && !web_search_called
        && tool_use_count == 0
}

pub(super) fn should_retry_fabricated_scheduled_response(
    is_scheduled_task: bool,
    retry_count: u32,
    total_tool_calls: usize,
    current_tool_calls: usize,
    response_text: &str,
) -> bool {
    if !is_scheduled_task {
        return false;
    }

    if retry_count >= 2 || total_tool_calls > 0 || current_tool_calls > 0 {
        return false;
    }

    let text = response_text.to_lowercase();
    [
        "i ran",
        "i executed",
        "i wrote",
        "i saved",
        "i updated",
        "i created",
        "i called",
        "已执行",
        "已运行",
        "已写入",
        "已保存",
        "已更新",
        "已创建",
        "已经完成",
    ]
    .iter()
    .any(|k| text.contains(k))
}

pub(super) fn should_retry_incomplete_scheduled_thought(
    is_scheduled_task: bool,
    retry_count: u32,
    total_tool_calls: usize,
    response_text: &str,
) -> bool {
    let max_retries: u32 = if is_scheduled_task { 2 } else { 1 };
    if retry_count >= max_retries || total_tool_calls == 0 {
        return false;
    }

    let text = response_text.to_lowercase();
    let is_short = response_text.len() < 500;
    let has_intent_phrase = [
        "let me ",
        "now let me",
        "i will ",
        "i'll ",
        "let me write",
        "let me compile",
        "let me create",
        "let me generate",
        "让我",
        "我来",
        "接下来",
    ]
    .iter()
    .any(|k| text.contains(k));

    is_short && has_intent_phrase
}

pub(super) fn collect_recent_messages(
    messages: &[LlmMessage],
    limit: usize,
) -> Vec<ConversationMessage> {
    let mut collected = Vec::new();

    for message in messages.iter().rev() {
        let mut parts = Vec::new();
        for block in &message.content {
            if let ContentBlock::Text { text } = block {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    parts.push(trimmed.to_string());
                }
            }
        }

        if !parts.is_empty() {
            collected.push(ConversationMessage {
                role: message.role.clone(),
                content: parts.join("\n"),
            });
            if collected.len() >= limit {
                break;
            }
        }
    }

    collected.reverse();
    collected
}

pub(super) fn repair_tool_pairing(messages: &mut Vec<LlmMessage>) {
    if messages.is_empty() {
        return;
    }

    let assistant_idx = messages
        .iter()
        .rposition(|message| message.role == "assistant");
    let Some(assistant_idx) = assistant_idx else {
        return;
    };

    let assistant_message = &messages[assistant_idx];
    let tool_use_ids: Vec<&str> = assistant_message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolUse { id, .. } => Some(id.as_str()),
            _ => None,
        })
        .collect();

    if tool_use_ids.is_empty() {
        return;
    }

    let Some(next_message) = messages.get(assistant_idx + 1) else {
        tracing::warn!(
            unpaired_tool_uses = ?tool_use_ids,
            "repair_tool_pairing: removing dangling assistant tool_use message"
        );
        messages.truncate(assistant_idx);
        return;
    };

    if next_message.role != "user" {
        tracing::warn!(
            unpaired_tool_uses = ?tool_use_ids,
            next_role = %next_message.role,
            "repair_tool_pairing: removing assistant tool_use message without user tool results"
        );
        messages.truncate(assistant_idx);
        return;
    }

    let tool_result_ids: Vec<&str> = next_message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
            _ => None,
        })
        .collect();

    let all_paired = tool_use_ids
        .iter()
        .all(|tool_use_id| tool_result_ids.contains(tool_use_id));

    if !all_paired {
        tracing::warn!(
            unpaired_tool_uses = ?tool_use_ids,
            tool_result_ids = ?tool_result_ids,
            "repair_tool_pairing: removing unpaired assistant+tool messages"
        );
        messages.truncate(assistant_idx);
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use clawhive_memory::SessionMessage;
    use clawhive_provider::{ContentBlock, LlmMessage};

    use crate::orchestrator::test_helpers::{
        agent_with_memory_policy, assistant_with_tool_use, message_roles, user_with_tool_result,
    };

    use super::*;

    #[test]
    fn repair_tool_pairing_removes_unpaired_tool_use_messages() {
        let mut messages = vec![
            LlmMessage::user("question"),
            assistant_with_tool_use("tool-1"),
            LlmMessage::user("ordinary follow-up"),
        ];

        repair_tool_pairing(&mut messages);

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, "user");
    }

    #[test]
    fn repair_tool_pairing_removes_dangling_last_assistant_tool_use() {
        let mut messages = vec![
            LlmMessage::user("question"),
            assistant_with_tool_use("tool-1"),
        ];

        repair_tool_pairing(&mut messages);

        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, "user");
    }

    #[test]
    fn repair_tool_pairing_keeps_properly_paired_messages() {
        let expected = vec![
            LlmMessage::user("question"),
            assistant_with_tool_use("tool-1"),
            user_with_tool_result("tool-1"),
        ];
        let mut messages = expected.clone();

        repair_tool_pairing(&mut messages);

        assert_eq!(message_roles(&messages), message_roles(&expected));
        assert_eq!(messages.len(), expected.len());
    }

    #[test]
    fn repair_tool_pairing_handles_empty_messages() {
        let mut messages = Vec::new();

        repair_tool_pairing(&mut messages);

        assert!(messages.is_empty());
    }

    #[test]
    fn repair_tool_pairing_ignores_messages_without_tool_use() {
        let expected = vec![
            LlmMessage::user("question"),
            LlmMessage::assistant("answer"),
        ];
        let mut messages = expected.clone();

        repair_tool_pairing(&mut messages);

        assert_eq!(message_roles(&messages), message_roles(&expected));
        assert_eq!(messages.len(), expected.len());
    }

    #[test]
    fn history_message_limit_defaults_to_10() {
        let agent = agent_with_memory_policy(None);

        assert_eq!(history_message_limit(&agent), 10);
    }

    #[test]
    fn history_message_limit_converts_turns() {
        let agent = agent_with_memory_policy(Some(crate::config::MemoryPolicyConfig {
            mode: "session".to_string(),
            write_scope: "session".to_string(),
            idle_minutes: Some(30),
            daily_at_hour: Some(4),
            limit_history_turns: Some(7),
            max_injected_chars: 6000,
            daily_summary_interval: 0,
        }));

        assert_eq!(history_message_limit(&agent), 14);
    }

    #[test]
    fn format_time_gap_prefers_days_hours_minutes() {
        assert_eq!(
            format_time_gap(chrono::Duration::minutes(45)),
            "45 minute(s)"
        );
        assert_eq!(format_time_gap(chrono::Duration::hours(3)), "3 hour(s)");
        assert_eq!(format_time_gap(chrono::Duration::hours(49)), "2 day(s)");
    }

    #[test]
    fn build_history_messages_inserts_inactivity_markers() {
        let history = vec![
            SessionMessage {
                role: "user".to_string(),
                content: "first".to_string(),
                timestamp: Some(Utc.with_ymd_and_hms(2026, 1, 1, 10, 0, 0).unwrap()),
            },
            SessionMessage {
                role: "assistant".to_string(),
                content: "second".to_string(),
                timestamp: Some(Utc.with_ymd_and_hms(2026, 1, 1, 10, 40, 0).unwrap()),
            },
            SessionMessage {
                role: "user".to_string(),
                content: "third".to_string(),
                timestamp: Some(Utc.with_ymd_and_hms(2026, 1, 1, 10, 50, 0).unwrap()),
            },
        ];

        let messages = build_messages_from_history(&history);

        assert_eq!(messages.len(), 4);
        assert_eq!(messages[0].role, "user");
        assert_eq!(messages[1].role, "user");
        assert_eq!(
            messages[1].content,
            vec![ContentBlock::Text {
                text: "[40 minute(s) of inactivity has passed since the last message]".to_string()
            }]
        );
        assert_eq!(messages[2].role, "assistant");
        assert_eq!(messages[3].role, "user");
    }

    #[test]
    fn slow_latency_threshold_detects_warn_boundary() {
        assert!(!is_slow_latency_ms(9_999, 10_000));
        assert!(is_slow_latency_ms(10_000, 10_000));
        assert!(is_slow_latency_ms(25_000, 10_000));
    }

    #[test]
    fn explicit_web_search_request_detection() {
        assert!(is_explicit_web_search_request(
            "请使用 web_search 工具搜索 OpenAI 最新新闻"
        ));
        assert!(is_explicit_web_search_request(
            "please use web search tool for this"
        ));
        assert!(!is_explicit_web_search_request("你觉得这个功能怎么样"));
    }

    #[test]
    fn web_search_reminder_injection_predicate() {
        assert!(should_inject_web_search_reminder(true, false, false, 0));
        assert!(!should_inject_web_search_reminder(true, true, false, 0));
        assert!(!should_inject_web_search_reminder(false, false, false, 0));
        assert!(!should_inject_web_search_reminder(true, false, true, 0));
        assert!(!should_inject_web_search_reminder(true, false, false, 1));
    }

    #[test]
    fn scheduled_retry_only_when_claiming_execution_without_tools() {
        assert!(should_retry_fabricated_scheduled_response(
            true,
            0,
            0,
            0,
            "I executed all steps and saved the file.",
        ));

        assert!(!should_retry_fabricated_scheduled_response(
            true,
            0,
            0,
            0,
            "以下是今日技术摘要：...",
        ));

        assert!(!should_retry_fabricated_scheduled_response(
            true,
            0,
            1,
            0,
            "I executed all steps and saved the file.",
        ));
    }

    #[test]
    fn fabricated_response_skipped_in_conversation() {
        // Conversations have a human in the loop — never retry for fabrication
        assert!(!should_retry_fabricated_scheduled_response(
            false,
            0,
            0,
            0,
            "I created the file and saved it.",
        ));
        assert!(!should_retry_fabricated_scheduled_response(
            false,
            0,
            0,
            0,
            "I updated the config.",
        ));
    }

    #[test]
    fn fabricated_response_scheduled_still_allows_two_retries() {
        assert!(should_retry_fabricated_scheduled_response(
            true,
            0,
            0,
            0,
            "已创建文件",
        ));
        assert!(should_retry_fabricated_scheduled_response(
            true,
            1,
            0,
            0,
            "已创建文件",
        ));
        assert!(!should_retry_fabricated_scheduled_response(
            true,
            2,
            0,
            0,
            "已创建文件",
        ));
    }

    #[test]
    fn incomplete_thought_detected_in_conversation() {
        assert!(should_retry_incomplete_scheduled_thought(
            false,
            0,
            1,
            "让我来处理这个问题",
        ));
    }

    #[test]
    fn incomplete_thought_conversation_max_one_retry() {
        assert!(should_retry_incomplete_scheduled_thought(
            false,
            0,
            1,
            "Let me fix that.",
        ));
        assert!(!should_retry_incomplete_scheduled_thought(
            false,
            1,
            1,
            "Let me fix that.",
        ));
    }

    #[test]
    fn incomplete_thought_scheduled_still_allows_two_retries() {
        assert!(should_retry_incomplete_scheduled_thought(
            true,
            0,
            1,
            "I will create the file.",
        ));
        assert!(should_retry_incomplete_scheduled_thought(
            true,
            1,
            1,
            "I will create the file.",
        ));
        assert!(!should_retry_incomplete_scheduled_thought(
            true,
            2,
            1,
            "I will create the file.",
        ));
    }
}
