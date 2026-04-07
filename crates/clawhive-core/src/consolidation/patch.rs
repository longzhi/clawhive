use anyhow::{anyhow, Result};

use super::{AddInstruction, MemoryPatch, PromotionCandidate, UpdateInstruction};

pub(super) fn build_incremental_user_prompt(current_memory: &str, daily_sections: &str) -> String {
    format!(
        "## Current MEMORY.md\n{}\n\n## Recent Daily Observations\n{}\nReturn ONLY incremental patch instructions in [ADD]/[UPDATE]/[KEEP] format. Do not rewrite the full MEMORY.md.",
        current_memory, daily_sections
    )
}

pub(super) fn build_promotion_candidate_prompt(daily_sections: &str) -> String {
    format!(
        "## Recent Daily Observations\n{}\n\nReturn ONLY a JSON array of promotion candidates.",
        daily_sections
    )
}

pub(super) fn build_section_merge_prompt(
    section: &str,
    current_section: &str,
    candidates: &[PromotionCandidate],
) -> String {
    let candidate_lines = candidates
        .iter()
        .map(|candidate| format!("- {}", candidate.content))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "## Target Section\n{}\n\n## Current Section Content\n{}\n\n## Candidate Updates\n{}\n",
        section, current_section, candidate_lines
    )
}

pub(super) fn build_full_overwrite_user_prompt(
    current_memory: &str,
    daily_sections: &str,
) -> String {
    format!(
        "## Current MEMORY.md\n{}\n\n## Recent Daily Observations\n{}\nPlease synthesize the daily observations into an updated MEMORY.md.\nOutput ONLY the new MEMORY.md content, no explanations.",
        current_memory, daily_sections
    )
}

pub fn parse_patch(llm_output: &str) -> Result<MemoryPatch> {
    let output = strip_markdown_fence(llm_output);
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("memory patch output is empty"));
    }

    if trimmed == "[KEEP]" {
        return Ok(MemoryPatch {
            adds: vec![],
            updates: vec![],
            keep: true,
        });
    }

    let mut adds = Vec::new();
    let mut updates = Vec::new();
    let mut rest = trimmed;

    while !rest.trim_start().is_empty() {
        rest = rest.trim_start();
        if rest.starts_with("[ADD]") {
            let (instruction, remaining) = parse_add_instruction(rest)?;
            adds.push(instruction);
            rest = remaining;
            continue;
        }

        if rest.starts_with("[UPDATE]") {
            let (instruction, remaining) = parse_update_instruction(rest)?;
            updates.push(instruction);
            rest = remaining;
            continue;
        }

        return Err(anyhow!("memory patch output contains an unknown block"));
    }

    if adds.is_empty() && updates.is_empty() {
        return Err(anyhow!("memory patch output contained no instructions"));
    }

    Ok(MemoryPatch {
        adds,
        updates,
        keep: false,
    })
}

pub fn apply_patch(existing: &str, patch: &MemoryPatch) -> String {
    let mut updated = existing.to_string();

    for instruction in &patch.updates {
        if updated.contains(&instruction.old) {
            updated = updated.replacen(&instruction.old, &instruction.new, 1);
        } else {
            tracing::warn!(old = %instruction.old, "Skipping memory patch update because OLD text was not found");
        }
    }

    for instruction in &patch.adds {
        updated = append_to_section(&updated, instruction);
    }

    updated
}

fn parse_add_instruction(input: &str) -> Result<(AddInstruction, &str)> {
    let header_end = input
        .find('\n')
        .ok_or_else(|| anyhow!("[ADD] block is missing a section header line"))?;
    let header = input[..header_end].trim();
    let section = parse_add_section(header)?;
    let body_and_rest = &input[header_end + 1..];
    let close_index = body_and_rest
        .find("[/ADD]")
        .ok_or_else(|| anyhow!("[ADD] block is missing [/ADD]"))?;
    let content = body_and_rest[..close_index].trim();
    if content.is_empty() {
        return Err(anyhow!("[ADD] block content is empty"));
    }

    Ok((
        AddInstruction {
            section,
            content: content.to_string(),
        },
        &body_and_rest[close_index + "[/ADD]".len()..],
    ))
}

fn parse_add_section(header: &str) -> Result<String> {
    let attributes = header
        .strip_prefix("[ADD]")
        .ok_or_else(|| anyhow!("[ADD] block is malformed"))?
        .trim();
    let quoted = attributes
        .strip_prefix("section=\"")
        .ok_or_else(|| anyhow!("[ADD] block is missing section attribute"))?;
    let section_end = quoted
        .find('"')
        .ok_or_else(|| anyhow!("[ADD] section attribute is missing closing quote"))?;
    let section = quoted[..section_end].trim();
    if section.is_empty() {
        return Err(anyhow!("[ADD] section attribute is empty"));
    }

    if !quoted[section_end + 1..].trim().is_empty() {
        return Err(anyhow!("[ADD] block header contains unexpected content"));
    }

    Ok(section.to_string())
}

fn parse_update_instruction(input: &str) -> Result<(UpdateInstruction, &str)> {
    let body = input
        .strip_prefix("[UPDATE]")
        .ok_or_else(|| anyhow!("[UPDATE] block is malformed"))?;
    let close_index = body
        .find("[/UPDATE]")
        .ok_or_else(|| anyhow!("[UPDATE] block is missing [/UPDATE]"))?;
    let block = body[..close_index].trim();
    let old = extract_tag_content(block, "OLD")?;
    let new = extract_tag_content(block, "NEW")?;

    Ok((
        UpdateInstruction { old, new },
        &body[close_index + "[/UPDATE]".len()..],
    ))
}

fn extract_tag_content(block: &str, tag: &str) -> Result<String> {
    let open_tag = format!("[{tag}]");
    let close_tag = format!("[/{tag}]");
    let after_open = block
        .find(&open_tag)
        .map(|index| &block[index + open_tag.len()..])
        .ok_or_else(|| anyhow!("[{tag}] tag is missing"))?;
    let close_index = after_open
        .find(&close_tag)
        .ok_or_else(|| anyhow!("[/{tag}] tag is missing"))?;
    let content = after_open[..close_index].trim();
    if content.is_empty() {
        return Err(anyhow!("[{tag}] content is empty"));
    }

    Ok(content.to_string())
}

fn append_to_section(existing: &str, instruction: &AddInstruction) -> String {
    let Some((_, end_index)) = find_section_bounds(existing, &instruction.section) else {
        let trimmed = existing.trim_end_matches('\n');
        if trimmed.is_empty() {
            return format!(
                "## {}\n{}\n",
                instruction.section,
                instruction.content.trim()
            );
        }

        return format!(
            "{trimmed}\n\n## {}\n{}\n",
            instruction.section,
            instruction.content.trim()
        );
    };

    let before = existing[..end_index].trim_end_matches('\n');
    let after = existing[end_index..].trim_start_matches('\n');
    if after.is_empty() {
        format!("{before}\n\n{}\n", instruction.content.trim())
    } else {
        format!("{before}\n\n{}\n\n{after}", instruction.content.trim())
    }
}

fn find_section_bounds(text: &str, section: &str) -> Option<(usize, usize)> {
    let headings = [format!("# {section}"), format!("## {section}")];
    let lines = text_line_starts(text);
    let mut section_start = None;

    for (index, (start, line)) in lines.iter().enumerate() {
        let line = trim_line_ending(line);
        if headings.iter().any(|heading| heading == line) {
            section_start = Some((*start, index));
            break;
        }
    }

    let (start, index) = section_start?;
    let end = lines[index + 1..]
        .iter()
        .find(|(_, line)| is_memory_section_heading(trim_line_ending(line)))
        .map(|(line_start, _)| *line_start)
        .unwrap_or(text.len());

    Some((start, end))
}

fn text_line_starts(text: &str) -> Vec<(usize, &str)> {
    let mut lines = Vec::new();
    let mut line_start = 0;

    for (index, ch) in text.char_indices() {
        if ch == '\n' {
            lines.push((line_start, &text[line_start..=index]));
            line_start = index + 1;
        }
    }

    if line_start < text.len() {
        lines.push((line_start, &text[line_start..]));
    }

    lines
}

fn trim_line_ending(line: &str) -> &str {
    line.trim_end_matches(['\r', '\n'])
}

fn is_memory_section_heading(line: &str) -> bool {
    line.starts_with("# ") || line.starts_with("## ")
}

pub(super) fn strip_markdown_fence(text: &str) -> String {
    let trimmed = text.trim();
    let without_prefix = if let Some(rest) = trimmed.strip_prefix("```") {
        // Strip optional language tag (e.g. "json", "markdown") up to the first newline
        match rest.find('\n') {
            Some(pos)
                if rest[..pos]
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '-' || c == '_') =>
            {
                &rest[pos + 1..]
            }
            _ => rest.trim_start(),
        }
    } else {
        trimmed
    };
    without_prefix
        .strip_suffix("```")
        .unwrap_or(without_prefix)
        .trim_end()
        .to_string()
}
