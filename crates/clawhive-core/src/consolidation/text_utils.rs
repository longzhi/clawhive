use std::collections::HashSet;

use anyhow::{anyhow, Result};

pub(super) fn default_importance() -> f64 {
    0.5
}

pub(super) fn reference_half_life_days(section: &str) -> f64 {
    match section {
        "长期项目主线" => 30.0,
        "持续性背景脉络" => 60.0,
        "关键历史决策" => 90.0,
        _ => 90.0,
    }
}

pub(super) fn dedup_paragraphs(content: &str) -> String {
    let paragraphs: Vec<&str> = content.split("\n\n").collect();
    if paragraphs.len() <= 1 {
        return content.to_string();
    }

    let mut keep = vec![true; paragraphs.len()];

    for i in 0..paragraphs.len() {
        if !keep[i] {
            continue;
        }
        if paragraphs[i].trim().starts_with('#') {
            continue;
        }
        let words_i = normalized_word_set(paragraphs[i]);
        if words_i.is_empty() {
            continue;
        }

        for j in (i + 1)..paragraphs.len() {
            if !keep[j] {
                continue;
            }
            if paragraphs[j].trim().starts_with('#') {
                continue;
            }
            let words_j = normalized_word_set(paragraphs[j]);
            if words_j.is_empty() {
                continue;
            }

            let similarity = jaccard_similarity(&words_i, &words_j);
            if similarity > 0.9 {
                if paragraphs[j].len() > paragraphs[i].len() {
                    keep[i] = false;
                    tracing::warn!(
                        kept = j,
                        removed = i,
                        similarity = format!("{:.2}", similarity),
                        "Dedup: removed near-duplicate paragraph"
                    );
                    break;
                } else {
                    keep[j] = false;
                    tracing::warn!(
                        kept = i,
                        removed = j,
                        similarity = format!("{:.2}", similarity),
                        "Dedup: removed near-duplicate paragraph"
                    );
                }
            }
        }
    }

    paragraphs
        .iter()
        .enumerate()
        .filter(|(idx, _)| keep[*idx])
        .map(|(_, paragraph)| *paragraph)
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub(super) fn compute_line_diff(old: &str, new: &str) -> Vec<String> {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let mut diff = Vec::new();

    for line in &old_lines {
        if !new_lines.contains(line) && !line.trim().is_empty() {
            diff.push(format!("- {line}"));
        }
    }

    for line in &new_lines {
        if !old_lines.contains(line) && !line.trim().is_empty() {
            diff.push(format!("+ {line}"));
        }
    }

    diff
}

pub(crate) fn normalized_word_set(text: &str) -> HashSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| word.len() > 1)
        .filter(|word| {
            !matches!(
                *word,
                "an" | "and" | "all" | "for" | "in" | "of" | "on" | "the" | "their" | "to"
            )
        })
        .map(|word| word.to_string())
        .collect()
}

pub(crate) fn jaccard_similarity(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    let intersection = a.intersection(b).count();
    let union = a.union(b).count();
    if union == 0 {
        return 0.0;
    }

    intersection as f64 / union as f64
}

pub(super) fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return 0.0;
    }

    let mut dot = 0.0_f32;
    let mut norm_a = 0.0_f32;
    let mut norm_b = 0.0_f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }

    if norm_a <= f32::EPSILON || norm_b <= f32::EPSILON {
        return 0.0;
    }

    (dot / (norm_a.sqrt() * norm_b.sqrt())).clamp(0.0, 1.0)
}

pub(super) fn validate_consolidation_output(output: &str, existing: &str) -> Result<()> {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("consolidation output is empty"));
    }

    if trimmed == "[KEEP]" {
        return Ok(());
    }

    let lowered = trimmed.to_ascii_lowercase();
    for refusal in [
        "i cannot",
        "i can't",
        "i'm unable",
        "i apologize",
        "i'm sorry",
    ] {
        if lowered.starts_with(refusal) {
            return Err(anyhow!("consolidation output looks like a refusal"));
        }
    }

    let existing_len = existing.trim().len();
    if existing_len > 0 && trimmed.len() * 2 < existing_len {
        return Err(anyhow!(
            "consolidation output shrank too much compared with existing memory"
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_empty_output() {
        let result = validate_consolidation_output("   \n\t", "# Existing\n\nUseful memory.");
        assert!(result.is_err());
    }

    #[test]
    fn validate_rejects_refusal() {
        let result = validate_consolidation_output(
            "I cannot help with that request.",
            "# Existing\n\nUseful memory.",
        );
        assert!(result.is_err());
    }

    #[test]
    fn validate_rejects_drastic_shrink() {
        let existing =
            "# Existing\n\nThis memory has enough content to be considered a healthy baseline.";
        let result = validate_consolidation_output("Too short", existing);
        assert!(result.is_err());
    }

    #[test]
    fn validate_accepts_keep() {
        let result = validate_consolidation_output("[KEEP]", "# Existing\n\nUseful memory.");
        assert!(result.is_ok());
    }

    #[test]
    fn validate_accepts_normal_output() {
        let existing =
            "# Existing\n\nThis memory has enough content to be considered a healthy baseline.";
        let output = "# Updated\n\nThis memory keeps the prior knowledge and adds a little more stable detail for future use.";
        let result = validate_consolidation_output(output, existing);
        assert!(result.is_ok());
    }

    #[test]
    fn validate_accepts_when_existing_is_empty() {
        let output = "# First Memory\n\nThis is the first consolidation output and it should be accepted even if there is no prior memory content.";
        let result = validate_consolidation_output(output, "");
        assert!(result.is_ok());
    }

    #[test]
    fn dedup_paragraphs_removes_near_duplicates() {
        let input = "## Preferences\n\nUser prefers dark mode and minimal UI design for all applications.\n\nThe user prefers dark mode and minimal UI design for all of their applications.\n\n## Work\n\nUser works on Rust projects.";
        let result = dedup_paragraphs(input);

        assert!(result.contains("## Preferences"));
        assert!(result.contains("## Work"));
        assert!(result.contains("Rust projects"));

        let dark_mode_count = result.matches("dark mode").count();
        assert_eq!(
            dark_mode_count, 1,
            "Should have removed one near-duplicate paragraph"
        );
    }

    #[test]
    fn dedup_paragraphs_preserves_headers() {
        let input = "## Section A\n\nContent A about specific topic.\n\n## Section A\n\nContent B about different topic.";
        let result = dedup_paragraphs(input);

        assert_eq!(result.matches("## Section A").count(), 2);
    }

    #[test]
    fn dedup_paragraphs_no_change_when_unique() {
        let input = "First paragraph about Rust programming language.\n\nSecond paragraph about Python scripting.\n\nThird paragraph about Go concurrency.";
        let result = dedup_paragraphs(input);

        assert_eq!(result, input);
    }

    #[test]
    fn dedup_paragraphs_single_paragraph() {
        let result = dedup_paragraphs("Just one paragraph here.");

        assert_eq!(result, "Just one paragraph here.");
    }

    #[test]
    fn dedup_paragraphs_empty_input() {
        let result = dedup_paragraphs("");

        assert_eq!(result, "");
    }

    #[test]
    fn jaccard_similarity_identical_sets() {
        let a: std::collections::HashSet<String> =
            ["hello", "world"].iter().map(|s| s.to_string()).collect();
        let b = a.clone();

        assert!((jaccard_similarity(&a, &b) - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn jaccard_similarity_disjoint_sets() {
        let a: std::collections::HashSet<String> =
            ["hello", "world"].iter().map(|s| s.to_string()).collect();
        let b: std::collections::HashSet<String> =
            ["foo", "bar"].iter().map(|s| s.to_string()).collect();

        assert!(jaccard_similarity(&a, &b).abs() < f64::EPSILON);
    }

    #[test]
    fn compute_line_diff_marks_added_and_removed_lines() {
        let old_content = "line kept\nline removed\n";
        let new_content = "line kept\nline added\n";

        let diff = compute_line_diff(old_content, new_content);

        assert_eq!(diff, vec!["- line removed", "+ line added"]);
    }
}
