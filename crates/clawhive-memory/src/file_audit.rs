use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryFileKind {
    LongTerm,
    Daily,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditSeverity {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditFinding {
    pub code: &'static str,
    pub severity: AuditSeverity,
    pub line: Option<usize>,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanupStats {
    pub removed_prompt_leakage_lines: usize,
    pub removed_empty_headings: usize,
    pub removed_duplicate_bullets: usize,
    pub removed_trivial_chat_lines: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupResult {
    pub content: String,
    pub stats: CleanupStats,
}

pub fn audit_memory_file(path: &str, content: &str, kind: MemoryFileKind) -> Vec<AuditFinding> {
    let lines = content.lines().collect::<Vec<_>>();
    let mut findings = Vec::new();

    for (idx, line) in lines.iter().enumerate() {
        if is_prompt_leakage_line(line) {
            findings.push(AuditFinding {
                code: "prompt_leakage",
                severity: AuditSeverity::High,
                line: Some(idx + 1),
                message: format!("发现 prompt 泄漏残留：{}", line.trim()),
            });
        }
    }

    for heading_line in find_empty_heading_lines(&lines) {
        findings.push(AuditFinding {
            code: "empty_heading",
            severity: AuditSeverity::Medium,
            line: Some(heading_line),
            message: "发现空 section heading".to_string(),
        });
    }

    let duplicate_bullets = find_duplicate_bullets(&lines);
    for (normalized, duplicate_lines) in duplicate_bullets {
        let sample = duplicate_lines
            .first()
            .map(|(_, raw)| raw.as_str())
            .unwrap_or_default();
        findings.push(AuditFinding {
            code: "duplicate_bullet",
            severity: AuditSeverity::Medium,
            line: duplicate_lines.first().map(|(line, _)| *line),
            message: format!(
                "发现重复 bullet（{} 次）：{}",
                duplicate_lines.len(),
                sample.trim()
            ),
        });
        let _ = normalized;
    }

    if matches!(kind, MemoryFileKind::Daily) {
        for (idx, line) in lines.iter().enumerate() {
            if is_trivial_chat_line(line) {
                findings.push(AuditFinding {
                    code: "trivial_chat",
                    severity: AuditSeverity::Low,
                    line: Some(idx + 1),
                    message: format!("发现高置信低价值闲聊残留：{}", line.trim()),
                });
            }
        }
    }

    if path == "MEMORY.md" {
        let repeated_recent_observations = lines
            .iter()
            .filter(|line| normalize_line(line) == "## recent daily observations")
            .count();
        if repeated_recent_observations > 1 {
            findings.push(AuditFinding {
                code: "repeated_recent_daily_observations",
                severity: AuditSeverity::High,
                line: None,
                message: format!(
                    "发现重复的 `## Recent Daily Observations` section（{} 次）",
                    repeated_recent_observations
                ),
            });
        }
    }

    findings
}

pub fn cleanup_memory_file(content: &str, kind: MemoryFileKind) -> CleanupResult {
    let mut stats = CleanupStats::default();
    let mut kept = Vec::new();
    let mut previous_bullet: Option<String> = None;

    for line in content.lines() {
        if is_prompt_leakage_line(line) {
            stats.removed_prompt_leakage_lines += 1;
            continue;
        }

        if matches!(kind, MemoryFileKind::Daily) && is_trivial_chat_line(line) {
            stats.removed_trivial_chat_lines += 1;
            continue;
        }

        if let Some(normalized) = normalize_bullet_line(line) {
            if previous_bullet.as_deref() == Some(normalized.as_str()) {
                stats.removed_duplicate_bullets += 1;
                continue;
            }
            previous_bullet = Some(normalized);
        } else if !line.trim().is_empty() {
            previous_bullet = None;
        }

        kept.push(line.to_string());
    }

    let (kept, removed_empty_headings) = strip_empty_headings(&kept);
    stats.removed_empty_headings = removed_empty_headings;

    CleanupResult {
        content: normalize_trailing_newlines(&kept.join("\n")),
        stats,
    }
}

fn is_prompt_leakage_line(line: &str) -> bool {
    let normalized = normalize_line(line);
    PROMPT_LEAKAGE_MARKERS
        .iter()
        .any(|marker| normalized.contains(marker))
}

fn find_empty_heading_lines(lines: &[&str]) -> Vec<usize> {
    let mut empty = Vec::new();
    let mut idx = 0usize;
    while idx < lines.len() {
        if !is_section_heading(lines[idx]) {
            idx += 1;
            continue;
        }

        let heading_line = idx + 1;
        idx += 1;
        let mut has_content = false;
        while idx < lines.len() && !is_section_heading(lines[idx]) {
            if !lines[idx].trim().is_empty() {
                has_content = true;
            }
            idx += 1;
        }

        if !has_content {
            empty.push(heading_line);
        }
    }
    empty
}

fn find_duplicate_bullets(lines: &[&str]) -> Vec<(String, Vec<(usize, String)>)> {
    let mut seen: HashMap<String, Vec<(usize, String)>> = HashMap::new();
    for (idx, line) in lines.iter().enumerate() {
        let Some(normalized) = normalize_bullet_line(line) else {
            continue;
        };
        seen.entry(normalized)
            .or_default()
            .push((idx + 1, (*line).to_string()));
    }

    seen.into_iter()
        .filter(|(_, matches)| matches.len() > 1)
        .collect()
}

fn strip_empty_headings(lines: &[String]) -> (Vec<String>, usize) {
    let mut cleaned = Vec::new();
    let mut removed = 0usize;
    let mut idx = 0usize;

    while idx < lines.len() {
        if !is_section_heading(&lines[idx]) {
            cleaned.push(lines[idx].clone());
            idx += 1;
            continue;
        }

        let heading = lines[idx].clone();
        idx += 1;
        let mut body = Vec::new();
        let mut has_content = false;
        while idx < lines.len() && !is_section_heading(&lines[idx]) {
            if !lines[idx].trim().is_empty() {
                has_content = true;
            }
            body.push(lines[idx].clone());
            idx += 1;
        }

        if has_content {
            cleaned.push(heading);
            cleaned.extend(body);
        } else {
            removed += 1;
        }
    }

    (cleaned, removed)
}

fn is_section_heading(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with("## ") || trimmed.starts_with("### ")
}

fn normalize_bullet_line(line: &str) -> Option<String> {
    let trimmed = line.trim();
    let content = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .map(str::trim)?;
    if content.is_empty() {
        return None;
    }
    Some(normalize_line(content))
}

fn is_trivial_chat_line(line: &str) -> bool {
    let Some(normalized) = normalize_bullet_line(line) else {
        return false;
    };
    TRIVIAL_CHAT_MARKERS.contains(&normalized.as_str())
}

fn normalize_line(line: &str) -> String {
    line.trim()
        .to_lowercase()
        .chars()
        .filter(|ch| {
            !matches!(
                ch,
                ',' | '.' | '!' | '?' | ':' | ';' | '-' | '`' | '"' | '\''
            )
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn normalize_trailing_newlines(content: &str) -> String {
    let trimmed = content.trim_matches('\n');
    if trimmed.is_empty() {
        String::new()
    } else {
        format!("{trimmed}\n")
    }
}

const PROMPT_LEAKAGE_MARKERS: &[&str] = &[
    "please synthesize the daily observations",
    "## current memorymd",
    "## recent daily observations",
    "current memorymd",
    "recent daily observations",
];

const TRIVIAL_CHAT_MARKERS: &[&str] = &[
    "hi",
    "hello",
    "你好",
    "您好",
    "你是谁",
    "who are you",
    "what can you do",
];

#[cfg(test)]
mod tests {
    use super::{audit_memory_file, cleanup_memory_file, AuditSeverity, MemoryFileKind};

    #[test]
    fn audit_flags_prompt_leakage_and_empty_heading() {
        let content = "# MEMORY.md\n\n## Current MEMORY.md\n\nPlease synthesize the daily observations.\n\n## Empty\n\n## 长期项目主线\n\n- Real item\n";
        let findings = audit_memory_file("MEMORY.md", content, MemoryFileKind::LongTerm);

        assert!(findings.iter().any(|f| f.code == "prompt_leakage"));
        assert!(findings.iter().any(|f| f.code == "empty_heading"));
        assert!(findings.iter().any(|f| f.severity == AuditSeverity::High));
    }

    #[test]
    fn audit_flags_duplicate_bullets_and_trivial_chat() {
        let content = "## General\n\n- hello\n- Keep this\n- Keep this\n";
        let findings = audit_memory_file("memory/2026-03-29.md", content, MemoryFileKind::Daily);

        assert!(findings.iter().any(|f| f.code == "trivial_chat"));
        assert!(findings.iter().any(|f| f.code == "duplicate_bullet"));
    }

    #[test]
    fn cleanup_removes_high_confidence_noise_and_is_idempotent() {
        let content = "## Recent Daily Observations\n\n- hello\n- Keep this\n- Keep this\n\n## Empty\n\n## Notes\n\n- Keep that\n";
        let cleaned = cleanup_memory_file(content, MemoryFileKind::Daily);
        let cleaned_twice = cleanup_memory_file(&cleaned.content, MemoryFileKind::Daily);

        assert!(!cleaned.content.contains("Recent Daily Observations"));
        assert!(!cleaned.content.contains("- hello"));
        assert_eq!(cleaned.content.matches("- Keep this").count(), 1);
        assert!(!cleaned.content.contains("## Empty"));
        assert_eq!(cleaned.content, cleaned_twice.content);
    }
}
