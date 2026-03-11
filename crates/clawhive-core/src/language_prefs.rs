use std::collections::HashMap;
use std::sync::RwLock;

use clawhive_memory::SessionMessage;
use clawhive_schema::InboundMessage;

/// Language detected from user input or response text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResponseLanguage {
    Chinese,
    English,
}

impl ResponseLanguage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chinese => "zh",
            Self::English => "en",
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Chinese => "Chinese",
            Self::English => "English",
        }
    }
}

/// Tracks per-user language preferences across sessions.
#[derive(Debug, Default)]
pub(crate) struct LanguagePrefs {
    prefs: RwLock<HashMap<String, ResponseLanguage>>,
}

impl LanguagePrefs {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, user_scope: &str) -> Option<ResponseLanguage> {
        self.prefs
            .read()
            .ok()
            .and_then(|p| p.get(user_scope).copied())
    }

    pub fn set(&self, user_scope: &str, language: ResponseLanguage) {
        if let Ok(mut p) = self.prefs.write() {
            p.insert(user_scope.to_string(), language);
        }
    }

    /// Detect language from inbound message text, falling back to history.
    pub fn resolve_target_language(
        &self,
        inbound: &InboundMessage,
        history_messages: &[SessionMessage],
    ) -> Option<ResponseLanguage> {
        if let Some(explicit) = detect_explicit_language_preference(&inbound.text) {
            self.set(&inbound.user_scope, explicit);
            return Some(explicit);
        }

        if let Some(current) = detect_text_language(&inbound.text) {
            self.set(&inbound.user_scope, current);
            return Some(current);
        }

        self.get(&inbound.user_scope)
            .or_else(|| detect_recent_user_language(history_messages))
    }
}

pub(crate) fn apply_language_policy_prompt(
    system_prompt: &mut String,
    target_language: Option<ResponseLanguage>,
) {
    if let Some(language) = target_language {
        system_prompt.push_str(&language_policy_prompt(language));
    }
}

pub(crate) fn log_language_guard(
    agent_id: &str,
    inbound: &InboundMessage,
    reply_text: &str,
    target_language: Option<ResponseLanguage>,
    is_streaming: bool,
) {
    if is_language_guard_exempt(&inbound.text) {
        return;
    }
    let Some(target) = target_language else {
        return;
    };
    let Some(detected) = detect_response_language(reply_text) else {
        return;
    };
    if detected == target {
        return;
    }

    tracing::warn!(
        agent_id = %agent_id,
        channel_type = %inbound.channel_type,
        connector_id = %inbound.connector_id,
        conversation_scope = %inbound.conversation_scope,
        user_scope = %inbound.user_scope,
        target_language = %target.as_str(),
        detected_language = %detected.as_str(),
        is_streaming,
        "language_guard: response language mismatch"
    );
}

fn detect_explicit_language_preference(text: &str) -> Option<ResponseLanguage> {
    let lower = text.to_ascii_lowercase();
    let wants_chinese = text.contains("用中文")
        || text.contains("说中文")
        || text.contains("中文回复")
        || text.contains("回复中文")
        || lower.contains("reply in chinese")
        || lower.contains("respond in chinese")
        || lower.contains("speak chinese")
        || lower.contains("in chinese");
    let wants_english = text.contains("用英文")
        || text.contains("说英文")
        || text.contains("英文回复")
        || text.contains("回复英文")
        || lower.contains("reply in english")
        || lower.contains("respond in english")
        || lower.contains("speak english")
        || lower.contains("in english");

    match (wants_chinese, wants_english) {
        (true, false) => Some(ResponseLanguage::Chinese),
        (false, true) => Some(ResponseLanguage::English),
        _ => None,
    }
}

fn is_cjk_char(ch: char) -> bool {
    matches!(
        ch as u32,
        0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xF900..=0xFAFF
            | 0x20000..=0x2A6DF
            | 0x2A700..=0x2B73F
            | 0x2B740..=0x2B81F
            | 0x2B820..=0x2CEAF
    )
}

pub(crate) fn detect_text_language(text: &str) -> Option<ResponseLanguage> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    let cjk_count = trimmed.chars().filter(|ch| is_cjk_char(*ch)).count();
    if cjk_count >= 1 {
        return Some(ResponseLanguage::Chinese);
    }

    let ascii_letters = trimmed
        .chars()
        .filter(|ch| ch.is_ascii_alphabetic())
        .count();
    let alpha_words = trimmed
        .split_whitespace()
        .filter(|word| word.chars().all(|ch| ch.is_ascii_alphabetic()))
        .count();
    if ascii_letters >= 6 || alpha_words >= 2 {
        return Some(ResponseLanguage::English);
    }

    None
}

fn detect_recent_user_language(history_messages: &[SessionMessage]) -> Option<ResponseLanguage> {
    history_messages
        .iter()
        .rev()
        .filter(|message| message.role == "user")
        .find_map(|message| detect_text_language(&message.content))
}

pub(crate) fn language_policy_prompt(target_language: ResponseLanguage) -> String {
    format!(
        "\n\n## Language Policy\nRespond in {} by default, matching the user's latest message language. Keep code blocks, file paths, command names, and URLs in their original form.",
        target_language.display_name()
    )
}

pub(crate) fn is_language_guard_exempt(user_text: &str) -> bool {
    let lower = user_text.to_ascii_lowercase();
    user_text.contains("翻译")
        || user_text.contains("双语")
        || user_text.contains("中英")
        || lower.contains("translate")
        || lower.contains("translation")
        || lower.contains("bilingual")
}

fn normalize_text_for_language_detection(text: &str) -> String {
    let mut in_code_fence = false;
    let mut lines = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_code_fence = !in_code_fence;
            continue;
        }
        if in_code_fence {
            continue;
        }
        lines.push(line.replace('`', " "));
    }

    let joined = lines.join("\n");
    joined
        .split_whitespace()
        .filter(|token| {
            !token.starts_with("http://")
                && !token.starts_with("https://")
                && !token.starts_with("www.")
                && !token.contains("://")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn detect_response_language(text: &str) -> Option<ResponseLanguage> {
    let normalized = normalize_text_for_language_detection(text);
    detect_text_language(&normalized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_text_language_handles_cjk_and_english() {
        assert_eq!(
            detect_text_language("请你用中文回答我"),
            Some(ResponseLanguage::Chinese)
        );
        assert_eq!(
            detect_text_language("Please answer in English"),
            Some(ResponseLanguage::English)
        );
    }

    #[test]
    fn detect_text_language_returns_none_for_ambiguous_short_input() {
        assert_eq!(detect_text_language("ok"), None);
        assert_eq!(detect_text_language("1"), None);
    }

    #[test]
    fn language_policy_prompt_includes_target_language() {
        let zh = language_policy_prompt(ResponseLanguage::Chinese);
        assert!(zh.contains("Chinese"));
        let en = language_policy_prompt(ResponseLanguage::English);
        assert!(en.contains("English"));
    }
}
