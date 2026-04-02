use std::collections::{BTreeMap, BTreeSet};

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

const DEFAULT_TOPIC: &str = "General";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SummaryClass {
    Discard,
    Daily,
    Fact,
    Memory,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SummaryCandidate {
    pub content: String,
    pub classification: SummaryClass,
    #[serde(default)]
    pub topic: String,
    #[serde(default)]
    pub importance: f32,
    #[serde(default)]
    pub fact_type: Option<String>,
    #[serde(default)]
    pub duplicate_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DailyTopicBlock {
    pub topic: String,
    pub items: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RetainedSummaryCandidates {
    pub daily: Vec<SummaryCandidate>,
    pub facts: Vec<SummaryCandidate>,
    pub memory: Vec<SummaryCandidate>,
}

pub fn build_summary_prompt() -> String {
    r#"Summarize this conversation by extracting memory candidates.

Return a JSON array only. Each item must contain:
- "content": concise standalone statement
- "classification": one of "discard", "daily", "fact", "memory"
- "topic": short topic label
- "importance": 0.0 to 1.0
- "fact_type": optional for "fact" items; one of "preference", "decision", "event", "person", "rule"
- "duplicate_key": optional short key for deduplication

Rules:
- Prefer under-selection over over-selection
- Discard greetings, identity chatter, small talk, and repeated hello/hi exchanges
- Discard raw command output, system stats, port listings, and one-off execution receipts
- Discard bilingual restatements of the same point
- Keep only decisions, meaningful changes, blockers, or context likely to matter in later turns
- Use "daily" only for short-term context worth keeping beyond this turn
- Use "fact" for stable preferences, rules, identities, or durable decisions
- Use "memory" for long-lived narrative context
- Topic labels should be short and reusable
"#
    .to_string()
}

pub fn parse_candidates(raw: &str) -> Option<Vec<SummaryCandidate>> {
    let stripped = strip_json_fence(raw);
    // 1. Try strict JSON array
    if let Ok(parsed) = serde_json::from_str::<Vec<SummaryCandidate>>(&stripped) {
        return Some(parsed);
    }
    // 2. Try extracting JSON array embedded in surrounding text
    if let Some(json_result) = extract_embedded_json(&stripped) {
        return Some(json_result);
    }
    // 3. Try bullet/list fallback (-, *, numbered)
    parse_list_fallback(&stripped)
}

/// Return a diagnostic string describing why `parse_candidates` would fail.
/// Useful for structured logging when the parse returns `None`.
pub fn parse_candidates_error(raw: &str) -> String {
    let stripped = strip_json_fence(raw);

    let strict_err = match serde_json::from_str::<Vec<SummaryCandidate>>(&stripped) {
        Ok(_) => return "no error (strict JSON parsed successfully)".to_string(),
        Err(e) => e,
    };

    let embedded_err = if let Some(start) = stripped.find('[') {
        if let Some(end) = stripped.rfind(']') {
            if end > start {
                let slice = &stripped[start..=end];
                match serde_json::from_str::<Vec<SummaryCandidate>>(slice) {
                    Ok(_) => return "no error (embedded JSON parsed successfully)".to_string(),
                    Err(e) => Some(e),
                }
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    match embedded_err {
        Some(emb) => format!("strict: {strict_err}; embedded: {emb}",),
        None => format!("strict: {strict_err}; no embedded JSON array found",),
    }
}

pub fn retain_daily_candidates(candidates: Vec<SummaryCandidate>) -> Vec<SummaryCandidate> {
    retain_summary_candidates(candidates).daily
}

pub fn retain_summary_candidates(candidates: Vec<SummaryCandidate>) -> RetainedSummaryCandidates {
    let mut seen_daily = BTreeSet::new();
    let mut seen_promotions = BTreeSet::new();
    let mut retained = RetainedSummaryCandidates::default();

    for candidate in candidates {
        if candidate.classification == SummaryClass::Discard {
            continue;
        }

        let trimmed = normalize_item(&candidate.content);
        if trimmed.is_empty() || looks_low_value(&trimmed) {
            continue;
        }

        let topic = normalize_topic(&candidate.topic);
        let dedup_key = candidate
            .duplicate_key
            .as_deref()
            .map(normalize_item)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| trimmed.to_lowercase());

        let normalized = SummaryCandidate {
            content: trimmed,
            topic: topic.clone(),
            ..candidate
        };

        match normalized.classification {
            SummaryClass::Discard => {}
            SummaryClass::Daily => {
                if seen_daily.insert((topic, dedup_key)) {
                    retained.daily.push(normalized);
                }
            }
            SummaryClass::Fact | SummaryClass::Memory => {
                let class_key = match normalized.classification {
                    SummaryClass::Fact => "fact",
                    SummaryClass::Memory => "memory",
                    _ => unreachable!(),
                };
                if seen_promotions.insert((class_key.to_string(), dedup_key)) {
                    match normalized.classification {
                        SummaryClass::Fact => retained.facts.push(normalized),
                        SummaryClass::Memory => retained.memory.push(normalized),
                        _ => {}
                    }
                }
            }
        }
    }

    retained
}

pub fn group_daily_candidates(candidates: &[SummaryCandidate]) -> Vec<DailyTopicBlock> {
    let mut grouped: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for candidate in candidates {
        grouped
            .entry(normalize_topic(&candidate.topic))
            .or_default()
            .push(candidate.content.clone());
    }

    grouped
        .into_iter()
        .map(|(topic, mut items)| {
            items.sort();
            items.dedup();
            DailyTopicBlock { topic, items }
        })
        .collect()
}

pub fn merge_daily_blocks(
    date: NaiveDate,
    existing: Option<&str>,
    blocks: &[DailyTopicBlock],
) -> Option<String> {
    if blocks.is_empty() {
        return None;
    }

    let mut parsed = DailyDocument::parse(date, existing.unwrap_or_default());
    let mut changed = false;

    for block in blocks {
        let section = parsed.sections.entry(block.topic.clone()).or_default();
        let before_len = section.len();
        for item in &block.items {
            if !section
                .iter()
                .any(|existing| normalize_item(existing) == normalize_item(item))
            {
                section.push(item.clone());
            }
        }
        if section.len() != before_len {
            changed = true;
        }
    }

    if !changed && existing.is_some() {
        return None;
    }

    Some(parsed.render())
}

fn normalize_topic(topic: &str) -> String {
    let trimmed = topic.trim();
    if trimmed.is_empty() {
        DEFAULT_TOPIC.to_string()
    } else {
        trimmed.to_string()
    }
}

fn normalize_item(value: &str) -> String {
    value
        .trim()
        .trim_start_matches("- ")
        .trim()
        .replace("\r\n", "\n")
}

fn looks_low_value(value: &str) -> bool {
    let lower = value.to_lowercase();
    let compact = lower.replace([' ', '\t', '\n', '\r'], "");

    if lower.len() <= 12 {
        let trivial = [
            "hi",
            "hello",
            "hey",
            "你好",
            "您好",
            "嗨",
            "早上好",
            "晚上好",
            "收到",
        ];
        if trivial.iter().any(|item| compact == *item) {
            return true;
        }
    }

    let status_markers = [
        "task triggered successfully",
        "stop requested",
        "start requested",
        "uptime",
        "whoami",
        "lsof",
        "df -h",
        "port ",
        "delivered",
    ];
    if status_markers.iter().any(|marker| lower.contains(marker)) {
        return true;
    }

    let small_talk_markers = [
        "just greeted",
        "casual greeting",
        "exchange of greetings",
        "only greeted",
        "greeted the assistant",
        "greeted the user",
        "问候",
        "打了招呼",
        "简单打了招呼",
        "身份试探",
    ];
    small_talk_markers
        .iter()
        .any(|marker| lower.contains(marker))
}

fn strip_json_fence(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some(body) = trimmed.strip_prefix("```json") {
        return body.trim().trim_end_matches("```").trim().to_string();
    }
    if let Some(body) = trimmed.strip_prefix("```") {
        return body.trim().trim_end_matches("```").trim().to_string();
    }
    trimmed.to_string()
}

/// Try to extract a JSON array from text that contains it embedded in prose.
fn extract_embedded_json(raw: &str) -> Option<Vec<SummaryCandidate>> {
    // Find the first '[' and last ']' to extract the JSON array
    let start = raw.find('[')?;
    let end = raw.rfind(']')?;
    if end <= start {
        return None;
    }
    let json_slice = &raw[start..=end];
    serde_json::from_str::<Vec<SummaryCandidate>>(json_slice).ok()
}

/// Parse bullet/list formats: `- item`, `* item`, `1. item`, `1) item`
fn parse_list_fallback(raw: &str) -> Option<Vec<SummaryCandidate>> {
    let items: Vec<String> = raw
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            // - bullet or * bullet
            trimmed
                .strip_prefix("- ")
                .or_else(|| trimmed.strip_prefix("* "))
                // Numbered: 1. item or 1) item
                .or_else(|| {
                    let rest = trimmed.trim_start_matches(|c: char| c.is_ascii_digit());
                    rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") "))
                })
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToOwned::to_owned)
        })
        .collect();

    if items.is_empty() {
        None
    } else {
        Some(
            items
                .into_iter()
                .map(|content| SummaryCandidate {
                    content,
                    classification: SummaryClass::Daily,
                    topic: DEFAULT_TOPIC.to_string(),
                    importance: 0.5,
                    fact_type: None,
                    duplicate_key: None,
                })
                .collect(),
        )
    }
}

#[derive(Debug, Default)]
struct DailyDocument {
    date: Option<NaiveDate>,
    preface: Vec<String>,
    sections: BTreeMap<String, Vec<String>>,
}

impl DailyDocument {
    fn parse(date: NaiveDate, raw: &str) -> Self {
        let mut doc = DailyDocument {
            date: Some(date),
            ..Self::default()
        };
        let mut current_topic: Option<String> = None;

        for line in raw.lines() {
            if line.starts_with("# ") {
                continue;
            }

            if let Some(topic) = line.strip_prefix("## ") {
                current_topic = Some(topic.trim().to_string());
                doc.sections.entry(topic.trim().to_string()).or_default();
                continue;
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            if let Some(item) = trimmed.strip_prefix("- ") {
                if let Some(topic) = current_topic.as_ref() {
                    doc.sections
                        .entry(topic.clone())
                        .or_default()
                        .push(item.trim().to_string());
                } else {
                    doc.preface.push(trimmed.to_string());
                }
            } else if let Some(topic) = current_topic.as_ref() {
                doc.sections
                    .entry(topic.clone())
                    .or_default()
                    .push(trimmed.to_string());
            } else {
                doc.preface.push(trimmed.to_string());
            }
        }

        doc
    }

    fn render(&self) -> String {
        let mut out = Vec::new();
        let date = self.date.expect("daily document must have a date");
        out.push(format!("# {}", date.format("%Y-%m-%d")));
        out.push(String::new());

        if !self.preface.is_empty() {
            for line in &self.preface {
                out.push(line.clone());
            }
            out.push(String::new());
        }

        let mut first_section = true;
        for (topic, items) in &self.sections {
            if items.is_empty() {
                continue;
            }
            if !first_section {
                out.push(String::new());
            }
            first_section = false;
            out.push(format!("## {topic}"));
            out.push(String::new());
            for item in items {
                out.push(format!("- {}", normalize_item(item)));
            }
        }

        out.push(String::new());
        out.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use chrono::NaiveDate;

    use super::{
        group_daily_candidates, merge_daily_blocks, parse_candidates, retain_daily_candidates,
        retain_summary_candidates, DailyTopicBlock, SummaryCandidate, SummaryClass,
    };

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).expect("valid date")
    }

    #[test]
    fn parse_candidates_accepts_json_fence() {
        let raw = "```json\n[{\"content\":\"keep\",\"classification\":\"daily\",\"topic\":\"Proj\",\"importance\":0.8}]\n```";
        let parsed = parse_candidates(raw).expect("json should parse");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].classification, SummaryClass::Daily);
    }

    #[test]
    fn parse_candidates_falls_back_to_bullets() {
        let raw = "- fallback summary 1\n- fallback summary 2";
        let parsed = parse_candidates(raw).expect("bullet fallback should parse");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].topic, "General");
    }

    #[test]
    fn parse_candidates_handles_star_bullets() {
        let raw = "* summary A\n* summary B";
        let parsed = parse_candidates(raw).expect("star bullets should parse");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].content, "summary A");
    }

    #[test]
    fn parse_candidates_handles_numbered_list() {
        let raw = "1. first item\n2. second item\n3. third item";
        let parsed = parse_candidates(raw).expect("numbered list should parse");
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].content, "first item");
    }

    #[test]
    fn parse_candidates_handles_embedded_json() {
        let raw = "Here is the analysis:\n[{\"content\":\"keep\",\"classification\":\"daily\",\"topic\":\"Proj\",\"importance\":0.8}]\nDone.";
        let parsed = parse_candidates(raw).expect("embedded json should parse");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].content, "keep");
    }

    #[test]
    fn retain_daily_candidates_drops_greetings_and_duplicates() {
        let input = vec![
            SummaryCandidate {
                content: "The user greeted the assistant with hi.".to_string(),
                classification: SummaryClass::Daily,
                topic: "Chat".to_string(),
                importance: 0.1,
                fact_type: None,
                duplicate_key: None,
            },
            SummaryCandidate {
                content: "Confirmed memory redesign will use section-based consolidation."
                    .to_string(),
                classification: SummaryClass::Daily,
                topic: "Memory".to_string(),
                importance: 0.8,
                fact_type: None,
                duplicate_key: Some("memory-redesign".to_string()),
            },
            SummaryCandidate {
                content: "Confirmed memory redesign will use section-based consolidation."
                    .to_string(),
                classification: SummaryClass::Daily,
                topic: "Memory".to_string(),
                importance: 0.9,
                fact_type: None,
                duplicate_key: Some("memory-redesign".to_string()),
            },
        ];

        let kept = retain_daily_candidates(input);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].topic, "Memory");
    }

    #[test]
    fn retain_summary_candidates_preserves_fact_and_memory_candidates() {
        let retained = retain_summary_candidates(vec![
            SummaryCandidate {
                content: "User prefers Chinese replies".to_string(),
                classification: SummaryClass::Fact,
                topic: "Prefs".to_string(),
                importance: 0.9,
                fact_type: Some("preference".to_string()),
                duplicate_key: Some("reply-lang".to_string()),
            },
            SummaryCandidate {
                content: "Memory refactor is now section-based".to_string(),
                classification: SummaryClass::Memory,
                topic: "Architecture".to_string(),
                importance: 0.8,
                fact_type: None,
                duplicate_key: Some("memory-refactor".to_string()),
            },
            SummaryCandidate {
                content: "hello".to_string(),
                classification: SummaryClass::Daily,
                topic: "".to_string(),
                importance: 0.1,
                fact_type: None,
                duplicate_key: None,
            },
        ]);

        assert!(retained.daily.is_empty());
        assert_eq!(retained.facts.len(), 1);
        assert_eq!(retained.facts[0].content, "User prefers Chinese replies");
        assert_eq!(retained.memory.len(), 1);
        assert_eq!(
            retained.memory[0].content,
            "Memory refactor is now section-based"
        );
    }

    #[test]
    fn group_daily_candidates_groups_by_topic() {
        let grouped = group_daily_candidates(&[
            SummaryCandidate {
                content: "A".to_string(),
                classification: SummaryClass::Daily,
                topic: "Proj".to_string(),
                importance: 0.5,
                fact_type: None,
                duplicate_key: None,
            },
            SummaryCandidate {
                content: "B".to_string(),
                classification: SummaryClass::Daily,
                topic: "Proj".to_string(),
                importance: 0.5,
                fact_type: None,
                duplicate_key: None,
            },
        ]);

        assert_eq!(
            grouped,
            vec![DailyTopicBlock {
                topic: "Proj".to_string(),
                items: vec!["A".to_string(), "B".to_string()],
            }]
        );
    }

    #[test]
    fn merge_daily_blocks_creates_structured_document() {
        let output = merge_daily_blocks(
            date(2026, 3, 29),
            None,
            &[DailyTopicBlock {
                topic: "Memory".to_string(),
                items: vec!["Decided to refactor".to_string()],
            }],
        )
        .expect("document should be created");

        assert!(output.contains("# 2026-03-29"));
        assert!(output.contains("## Memory"));
        assert!(output.contains("- Decided to refactor"));
    }

    #[test]
    fn merge_daily_blocks_merges_into_existing_topic() {
        let existing = "# 2026-03-29\n\n## Memory\n\n- Decided to refactor\n";
        let output = merge_daily_blocks(
            date(2026, 3, 29),
            Some(existing),
            &[DailyTopicBlock {
                topic: "Memory".to_string(),
                items: vec![
                    "Decided to refactor".to_string(),
                    "Will use section routing".to_string(),
                ],
            }],
        )
        .expect("document should change");

        assert_eq!(output.matches("## Memory").count(), 1);
        assert_eq!(output.matches("- Decided to refactor").count(), 1);
        assert!(output.contains("- Will use section routing"));
    }
}
