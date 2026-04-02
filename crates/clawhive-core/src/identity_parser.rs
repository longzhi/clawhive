#[derive(Debug, Clone, Default)]
pub struct ParsedIdentity {
    pub name: Option<String>,
    pub emoji: Option<String>,
    pub creature: Option<String>,
    pub vibe: Option<String>,
    pub role: Option<String>,
}

pub fn parse_identity_md(content: &str) -> ParsedIdentity {
    let mut parsed = ParsedIdentity::default();
    let lines: Vec<&str> = content.lines().collect();

    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let Some((label, inline_value)) = parse_label_line(line) else {
            i += 1;
            continue;
        };

        let value = if !inline_value.trim().is_empty() {
            normalize_value(inline_value)
        } else {
            parse_next_line_value(&lines, i + 1)
        };

        if let Some(value) = value {
            match label {
                "name" => set_if_empty(&mut parsed.name, value),
                "emoji" => set_if_empty(&mut parsed.emoji, value),
                "creature" => set_if_empty(&mut parsed.creature, value),
                "vibe" => set_if_empty(&mut parsed.vibe, value),
                "role" | "specialization" => set_if_empty(&mut parsed.role, value),
                _ => {}
            }
        }

        i += 1;
    }

    parsed
}

fn parse_label_line(line: &str) -> Option<(&str, &str)> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix('-')?.trim_start();
    let bold = rest.strip_prefix("**")?;
    let label_end = bold.find("**")?;
    let (label_with_colon, remaining) = bold.split_at(label_end);
    let label = label_with_colon
        .trim()
        .trim_end_matches(':')
        .trim()
        .to_ascii_lowercase();

    if label.is_empty() {
        return None;
    }

    let value = remaining.strip_prefix("**")?.trim_start();
    Some((label_to_static(&label)?, value))
}

fn label_to_static(label: &str) -> Option<&'static str> {
    match label {
        "name" => Some("name"),
        "emoji" => Some("emoji"),
        "creature" => Some("creature"),
        "vibe" => Some("vibe"),
        "role" => Some("role"),
        "specialization" => Some("specialization"),
        _ => None,
    }
}

fn parse_next_line_value(lines: &[&str], next_index: usize) -> Option<String> {
    let next_line = lines.get(next_index)?;
    if !next_line.starts_with(' ') && !next_line.starts_with('\t') {
        return None;
    }

    normalize_value(next_line)
}

fn normalize_value(raw: &str) -> Option<String> {
    let raw_trimmed = raw.trim();
    if raw_trimmed.is_empty() || is_italic_placeholder(raw_trimmed) {
        return None;
    }

    let cleaned = raw_trimmed
        .trim_matches(|c: char| c.is_whitespace() || c == '_' || c == '*')
        .trim();

    if cleaned.is_empty() {
        return None;
    }

    let lowered = cleaned.to_ascii_lowercase();
    if lowered.contains("pick something")
        || lowered.contains("optional")
        || lowered.contains("define your")
    {
        return None;
    }

    Some(cleaned.to_string())
}

fn is_italic_placeholder(value: &str) -> bool {
    value.starts_with("_(") && value.ends_with(")_")
}

fn set_if_empty(slot: &mut Option<String>, value: String) {
    if slot.is_none() {
        *slot = Some(value);
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_identity_md, ParsedIdentity};

    #[test]
    fn parse_empty_string_returns_all_none() {
        let parsed = parse_identity_md("");
        assert_all_none(&parsed);
    }

    #[test]
    fn parse_default_template_returns_all_none() {
        let content = r#"# IDENTITY.md - Who Am I?

_Fill this in during your first conversation. Make it yours._

- **Name:**
  _(pick something you like)_
- **Role:**
  _(General Assistant? Coding Expert? Research Agent? Define your purpose)_
- **Specialization:**
  _(what are you good at? list your strengths)_
- **Creature:**
  _(AI? robot? familiar? ghost in the machine? something weirder?)_
- **Vibe:**
  _(how do you come across? sharp? warm? chaotic? calm?)_
- **Emoji:**
  _(your signature — pick one that feels right)_
"#;

        let parsed = parse_identity_md(content);
        assert_all_none(&parsed);
    }

    #[test]
    fn parse_fully_filled_fields() {
        let content = r#"# IDENTITY.md - Who Am I?

- **Name:** Kuro
- **Role:** General Assistant & Coding Expert
- **Creature:** shadow cat
- **Vibe:** sharp, warm
- **Emoji:** 🐾
"#;

        let parsed = parse_identity_md(content);

        assert_eq!(parsed.name.as_deref(), Some("Kuro"));
        assert_eq!(
            parsed.role.as_deref(),
            Some("General Assistant & Coding Expert")
        );
        assert_eq!(parsed.creature.as_deref(), Some("shadow cat"));
        assert_eq!(parsed.vibe.as_deref(), Some("sharp, warm"));
        assert_eq!(parsed.emoji.as_deref(), Some("🐾"));
    }

    #[test]
    fn parse_partial_fill_name_and_emoji_only() {
        let content = r#"- **Name:** Kuro
- **Role:** _(optional)_
- **Specialization:**
- **Creature:** _(define your creature)_
- **Vibe:**
- **Emoji:** 🐈
"#;

        let parsed = parse_identity_md(content);

        assert_eq!(parsed.name.as_deref(), Some("Kuro"));
        assert_eq!(parsed.emoji.as_deref(), Some("🐈"));
        assert_eq!(parsed.role, None);
        assert_eq!(parsed.creature, None);
        assert_eq!(parsed.vibe, None);
    }

    #[test]
    fn parse_value_with_extra_whitespace_and_markdown_formatting() {
        let content = "- **Name:**   _**  Kuro  **_   ";

        let parsed = parse_identity_md(content);
        assert_eq!(parsed.name.as_deref(), Some("Kuro"));
    }

    #[test]
    fn parse_emoji_with_actual_emoji_character() {
        let content = "- **Emoji:** 🤖";

        let parsed = parse_identity_md(content);
        assert_eq!(parsed.emoji.as_deref(), Some("🤖"));
    }

    #[test]
    fn parse_labels_case_insensitive() {
        let content = r#"- **NAME:** Nova
- **vIbE:** calm
- **eMoJi:** ✨
"#;

        let parsed = parse_identity_md(content);

        assert_eq!(parsed.name.as_deref(), Some("Nova"));
        assert_eq!(parsed.vibe.as_deref(), Some("calm"));
        assert_eq!(parsed.emoji.as_deref(), Some("✨"));
    }

    #[test]
    fn parse_specialization_maps_to_role() {
        let content = "- **Specialization:** Systems Architect";

        let parsed = parse_identity_md(content);
        assert_eq!(parsed.role.as_deref(), Some("Systems Architect"));
    }

    #[test]
    fn parse_value_on_next_indented_line() {
        let content = "- **Role:**\n  Principal Agent";

        let parsed = parse_identity_md(content);
        assert_eq!(parsed.role.as_deref(), Some("Principal Agent"));
    }

    fn assert_all_none(parsed: &ParsedIdentity) {
        assert_eq!(parsed.name, None);
        assert_eq!(parsed.emoji, None);
        assert_eq!(parsed.creature, None);
        assert_eq!(parsed.vibe, None);
        assert_eq!(parsed.role, None);
    }
}
