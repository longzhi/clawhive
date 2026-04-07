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
