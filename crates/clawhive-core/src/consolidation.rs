use std::sync::Arc;

use anyhow::{anyhow, Result};
use clawhive_memory::embedding::EmbeddingProvider;
use clawhive_memory::file_store::MemoryFileStore;
use clawhive_memory::search_index::SearchIndex;
use clawhive_memory::session::SessionReader;
use clawhive_provider::LlmMessage;

use super::router::LlmRouter;

const CONSOLIDATION_INCREMENTAL_SYSTEM_PROMPT: &str = r#"You are a memory consolidation system. You maintain a personal knowledge base (MEMORY.md)
by integrating new daily observations.

Rules:
- Preserve existing important knowledge that is still valid
- Add new stable facts, user preferences, and behavioral patterns from daily notes
- Remove or update information that has been contradicted by newer observations
- Do NOT rewrite the full MEMORY.md
- If no long-term memory changes are needed, output exactly [KEEP]
- Otherwise output ONLY incremental patch instructions using one or more of these blocks:
  [ADD] section="Section Name"
  content to add here
  [/ADD]

  [UPDATE]
  [OLD]exact text to find in existing memory[/OLD]
  [NEW]replacement text[/NEW]
  [/UPDATE]
- For [UPDATE], copy the OLD text exactly from the existing MEMORY.md
- No explanations, no Markdown fences, no extra prose"#;

const CONSOLIDATION_FULL_OVERWRITE_SYSTEM_PROMPT: &str = r#"You are a memory consolidation system. You maintain a personal knowledge base (MEMORY.md)
by integrating new daily observations.

Rules:
- Preserve existing important knowledge that is still valid
- Add new stable facts, user preferences, and behavioral patterns from daily notes
- Remove or update information that has been contradicted by newer observations
- Use clear Markdown formatting with headers for organization
- Be concise - only keep information that is useful for future conversations
- Output the COMPLETE updated MEMORY.md content (not a diff)"#;

#[derive(Debug, Clone)]
pub struct AddInstruction {
    pub section: String,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct UpdateInstruction {
    pub old: String,
    pub new: String,
}

#[derive(Debug, Clone)]
pub struct MemoryPatch {
    pub adds: Vec<AddInstruction>,
    pub updates: Vec<UpdateInstruction>,
    pub keep: bool,
}

pub struct HippocampusConsolidator {
    file_store: MemoryFileStore,
    router: Arc<LlmRouter>,
    model_primary: String,
    model_fallbacks: Vec<String>,
    lookback_days: usize,
    search_index: Option<SearchIndex>,
    embedding_provider: Option<Arc<dyn EmbeddingProvider>>,
    reindex_file_store: Option<MemoryFileStore>,
    reindex_session_reader: Option<SessionReader>,
}

#[derive(Debug)]
pub struct ConsolidationReport {
    pub daily_files_read: usize,
    pub memory_updated: bool,
    pub reindexed: bool,
    pub summary: String,
}

impl HippocampusConsolidator {
    pub fn new(
        file_store: MemoryFileStore,
        router: Arc<LlmRouter>,
        model_primary: String,
        model_fallbacks: Vec<String>,
    ) -> Self {
        Self {
            file_store,
            router,
            model_primary,
            model_fallbacks,
            lookback_days: 7,
            search_index: None,
            embedding_provider: None,
            reindex_file_store: None,
            reindex_session_reader: None,
        }
    }

    pub fn with_lookback_days(mut self, days: usize) -> Self {
        self.lookback_days = days;
        self
    }

    pub fn with_search_index(mut self, index: SearchIndex) -> Self {
        self.search_index = Some(index);
        self
    }

    pub fn with_embedding_provider(mut self, provider: Arc<dyn EmbeddingProvider>) -> Self {
        self.embedding_provider = Some(provider);
        self
    }

    pub fn with_file_store_for_reindex(mut self, file_store: MemoryFileStore) -> Self {
        self.reindex_file_store = Some(file_store);
        self
    }

    pub fn with_session_reader_for_reindex(mut self, reader: SessionReader) -> Self {
        self.reindex_session_reader = Some(reader);
        self
    }

    pub async fn consolidate(&self) -> Result<ConsolidationReport> {
        let current_memory = self.file_store.read_long_term().await?;
        let recent_daily = self
            .file_store
            .read_recent_daily(self.lookback_days)
            .await?;

        if recent_daily.is_empty() {
            return Ok(ConsolidationReport {
                daily_files_read: 0,
                memory_updated: false,
                reindexed: false,
                summary: "No daily files found in lookback window; skipped consolidation."
                    .to_string(),
            });
        }

        let mut daily_sections = String::new();
        for (date, content) in &recent_daily {
            daily_sections.push_str(&format!("### {}\n{}\n\n", date.format("%Y-%m-%d"), content));
        }

        let response = self
            .request_consolidation(
                CONSOLIDATION_INCREMENTAL_SYSTEM_PROMPT,
                build_incremental_user_prompt(&current_memory, &daily_sections),
            )
            .await?;

        let patch_output = strip_markdown_fence(&response.text);
        match parse_patch(&patch_output) {
            Ok(patch) => {
                if patch.keep {
                    tracing::info!("Consolidation returned [KEEP]; leaving MEMORY.md unchanged");
                    return Ok(ConsolidationReport {
                        daily_files_read: recent_daily.len(),
                        memory_updated: false,
                        reindexed: false,
                        summary: "Consolidation returned [KEEP]; MEMORY.md unchanged.".to_string(),
                    });
                }

                let updated_memory = apply_patch(&current_memory, &patch);
                return self
                    .finalize_updated_memory(updated_memory, &current_memory, recent_daily.len())
                    .await;
            }
            Err(error) => {
                tracing::warn!(error = %error, "Patch parsing failed, falling back to full overwrite");
            }
        }

        let response = self
            .request_consolidation(
                CONSOLIDATION_FULL_OVERWRITE_SYSTEM_PROMPT,
                build_full_overwrite_user_prompt(&current_memory, &daily_sections),
            )
            .await?;

        let updated_memory = strip_markdown_fence(&response.text);
        if updated_memory.trim() == "[KEEP]" {
            tracing::info!("Consolidation returned [KEEP]; leaving MEMORY.md unchanged");
            return Ok(ConsolidationReport {
                daily_files_read: recent_daily.len(),
                memory_updated: false,
                reindexed: false,
                summary: "Consolidation returned [KEEP]; MEMORY.md unchanged.".to_string(),
            });
        }

        self.finalize_updated_memory(updated_memory, &current_memory, recent_daily.len())
            .await
    }

    async fn request_consolidation(
        &self,
        system_prompt: &str,
        user_prompt: String,
    ) -> Result<clawhive_provider::LlmResponse> {
        self.router
            .chat(
                &self.model_primary,
                &self.model_fallbacks,
                Some(system_prompt.to_string()),
                vec![LlmMessage::user(user_prompt)],
                4096,
            )
            .await
    }

    async fn finalize_updated_memory(
        &self,
        updated_memory: String,
        current_memory: &str,
        daily_files_read: usize,
    ) -> Result<ConsolidationReport> {
        if let Err(error) = validate_consolidation_output(&updated_memory, current_memory) {
            tracing::warn!(error = %error, "Skipping consolidation write due to invalid LLM output");
            return Ok(ConsolidationReport {
                daily_files_read,
                memory_updated: false,
                reindexed: false,
                summary: "Consolidation skipped because LLM output failed validation.".to_string(),
            });
        }

        if updated_memory == current_memory {
            tracing::info!("Consolidation patch produced no effective MEMORY.md changes");
            return Ok(ConsolidationReport {
                daily_files_read,
                memory_updated: false,
                reindexed: false,
                summary: "Consolidation produced no MEMORY.md changes.".to_string(),
            });
        }

        self.file_store.write_long_term(&updated_memory).await?;

        let reindexed = if let (Some(index), Some(provider), Some(fs), Some(reader)) = (
            &self.search_index,
            &self.embedding_provider,
            &self.reindex_file_store,
            &self.reindex_session_reader,
        ) {
            match index.index_all(fs, reader, provider.as_ref()).await {
                Ok(count) => {
                    tracing::info!("Post-consolidation reindex: {count} chunks indexed");
                    true
                }
                Err(e) => {
                    tracing::warn!("Post-consolidation reindex failed: {e}");
                    false
                }
            }
        } else {
            false
        };

        Ok(ConsolidationReport {
            daily_files_read,
            memory_updated: true,
            reindexed,
            summary: format!("Consolidated {daily_files_read} daily files into MEMORY.md."),
        })
    }
}

fn build_incremental_user_prompt(current_memory: &str, daily_sections: &str) -> String {
    format!(
        "## Current MEMORY.md\n{}\n\n## Recent Daily Observations\n{}\nReturn ONLY incremental patch instructions in [ADD]/[UPDATE]/[KEEP] format. Do not rewrite the full MEMORY.md.",
        current_memory, daily_sections
    )
}

fn build_full_overwrite_user_prompt(current_memory: &str, daily_sections: &str) -> String {
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

fn strip_markdown_fence(text: &str) -> String {
    let trimmed = text.trim();
    let without_prefix = trimmed
        .strip_prefix("```markdown")
        .or_else(|| trimmed.strip_prefix("```md"))
        .or_else(|| trimmed.strip_prefix("```"))
        .unwrap_or(trimmed)
        .trim_start();
    without_prefix
        .strip_suffix("```")
        .unwrap_or(without_prefix)
        .trim_end()
        .to_string()
}

fn validate_consolidation_output(output: &str, existing: &str) -> Result<()> {
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

pub struct ConsolidationScheduler {
    consolidator: Arc<HippocampusConsolidator>,
    interval_hours: u64,
}

impl ConsolidationScheduler {
    pub fn new(consolidator: Arc<HippocampusConsolidator>, interval_hours: u64) -> Self {
        Self {
            consolidator,
            interval_hours,
        }
    }

    pub fn start(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(tokio::time::Duration::from_secs(self.interval_hours * 3600));
            interval.tick().await;
            loop {
                interval.tick().await;
                tracing::info!("Running scheduled hippocampus consolidation...");
                match self.consolidator.consolidate().await {
                    Ok(report) => {
                        tracing::info!(
                            "Consolidation complete: daily_files_read={}, memory_updated={}, summary={}",
                            report.daily_files_read,
                            report.memory_updated,
                            report.summary
                        );
                    }
                    Err(err) => {
                        tracing::error!("Consolidation failed: {err}");
                    }
                }
            }
        })
    }

    pub async fn run_once(&self) -> Result<ConsolidationReport> {
        self.consolidator.consolidate().await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use anyhow::Result;
    use async_trait::async_trait;
    use clawhive_memory::embedding::EmbeddingProvider;
    use clawhive_memory::file_store::MemoryFileStore;
    use clawhive_memory::session::SessionReader;
    use clawhive_provider::{ProviderRegistry, StubProvider};
    use tempfile::TempDir;

    use super::{
        apply_patch, parse_patch, validate_consolidation_output, AddInstruction,
        ConsolidationReport, ConsolidationScheduler, HippocampusConsolidator, MemoryPatch,
        UpdateInstruction,
    };
    use crate::router::LlmRouter;

    fn build_router() -> Arc<LlmRouter> {
        let mut registry = ProviderRegistry::new();
        registry.register("anthropic", Arc::new(StubProvider));
        let aliases = HashMap::from([(
            "sonnet".to_string(),
            "anthropic/claude-sonnet-4-5".to_string(),
        )]);
        Arc::new(LlmRouter::new(registry, aliases, vec![]))
    }

    fn build_file_store() -> Result<(TempDir, MemoryFileStore)> {
        let dir = TempDir::new()?;
        let store = MemoryFileStore::new(dir.path());
        Ok((dir, store))
    }

    #[test]
    fn consolidation_report_default_fields() {
        let report = ConsolidationReport {
            daily_files_read: 0,
            memory_updated: false,
            reindexed: false,
            summary: "none".to_string(),
        };

        assert_eq!(report.daily_files_read, 0);
        assert!(!report.memory_updated);
        assert!(!report.reindexed);
        assert_eq!(report.summary, "none");
    }

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
    fn parse_patch_add() -> Result<()> {
        let patch = parse_patch(
            r#"[ADD] section="Profile"
Learns quickly.
[/ADD]"#,
        )?;

        assert!(!patch.keep);
        assert!(patch.updates.is_empty());
        assert_eq!(patch.adds.len(), 1);
        assert_eq!(patch.adds[0].section, "Profile");
        assert_eq!(patch.adds[0].content, "Learns quickly.");
        Ok(())
    }

    #[test]
    fn parse_patch_update() -> Result<()> {
        let patch = parse_patch(
            r#"[UPDATE]
[OLD]Likes tea[/OLD]
[NEW]Likes green tea[/NEW]
[/UPDATE]"#,
        )?;

        assert!(!patch.keep);
        assert!(patch.adds.is_empty());
        assert_eq!(patch.updates.len(), 1);
        assert_eq!(patch.updates[0].old, "Likes tea");
        assert_eq!(patch.updates[0].new, "Likes green tea");
        Ok(())
    }

    #[test]
    fn parse_patch_keep() -> Result<()> {
        let patch = parse_patch("[KEEP]")?;

        assert!(patch.keep);
        assert!(patch.adds.is_empty());
        assert!(patch.updates.is_empty());
        Ok(())
    }

    #[test]
    fn parse_patch_mixed() -> Result<()> {
        let patch = parse_patch(
            r#"[ADD] section="Profile"
Prefers concise answers.
[/ADD]

[UPDATE]
[OLD]Works in software[/OLD]
[NEW]Builds Rust systems[/NEW]
[/UPDATE]

[ADD] section="Projects"
Working on Clawhive memory safety.
[/ADD]"#,
        )?;

        assert!(!patch.keep);
        assert_eq!(patch.adds.len(), 2);
        assert_eq!(patch.updates.len(), 1);
        assert_eq!(patch.adds[1].section, "Projects");
        assert_eq!(patch.updates[0].old, "Works in software");
        Ok(())
    }

    #[test]
    fn parse_patch_empty_returns_error() {
        let result = parse_patch("   \n\t");
        assert!(result.is_err());
    }

    #[test]
    fn apply_patch_add_to_existing_section() {
        let existing = "# Profile\nLearns quickly.\n\n# Preferences\nLikes tea.\n";
        let patch = MemoryPatch {
            adds: vec![AddInstruction {
                section: "Profile".to_string(),
                content: "Prefers concise answers.".to_string(),
            }],
            updates: vec![],
            keep: false,
        };

        let updated = apply_patch(existing, &patch);

        assert_eq!(
            updated,
            "# Profile\nLearns quickly.\n\nPrefers concise answers.\n\n# Preferences\nLikes tea.\n"
        );
    }

    #[test]
    fn apply_patch_add_creates_new_section() {
        let existing = "# Profile\nLearns quickly.\n";
        let patch = MemoryPatch {
            adds: vec![AddInstruction {
                section: "Projects".to_string(),
                content: "Working on memory safety fixes.".to_string(),
            }],
            updates: vec![],
            keep: false,
        };

        let updated = apply_patch(existing, &patch);

        assert_eq!(
            updated,
            "# Profile\nLearns quickly.\n\n## Projects\nWorking on memory safety fixes.\n"
        );
    }

    #[test]
    fn apply_patch_update_replaces_text() {
        let existing = "# Profile\nLikes tea.\n";
        let patch = MemoryPatch {
            adds: vec![],
            updates: vec![UpdateInstruction {
                old: "Likes tea.".to_string(),
                new: "Likes green tea.".to_string(),
            }],
            keep: false,
        };

        let updated = apply_patch(existing, &patch);

        assert_eq!(updated, "# Profile\nLikes green tea.\n");
    }

    #[test]
    fn apply_patch_update_skips_missing() {
        let existing = "# Profile\nLikes tea.\n";
        let patch = MemoryPatch {
            adds: vec![],
            updates: vec![UpdateInstruction {
                old: "Missing fact".to_string(),
                new: "New fact".to_string(),
            }],
            keep: false,
        };

        let updated = apply_patch(existing, &patch);
        assert_eq!(updated, existing);
    }

    #[test]
    fn apply_patch_preserves_unmodified() {
        let existing = "# Profile\nLikes tea.\n\n# Preferences\nPrefers concise answers.\n";
        let patch = MemoryPatch {
            adds: vec![AddInstruction {
                section: "Preferences".to_string(),
                content: "Avoids fluff.".to_string(),
            }],
            updates: vec![UpdateInstruction {
                old: "Likes tea.".to_string(),
                new: "Likes green tea.".to_string(),
            }],
            keep: false,
        };

        let updated = apply_patch(existing, &patch);

        assert_eq!(
            updated,
            "# Profile\nLikes green tea.\n\n# Preferences\nPrefers concise answers.\n\nAvoids fluff.\n"
        );
    }

    #[test]
    fn hippocampus_new_default_lookback() -> Result<()> {
        let (_dir, file_store) = build_file_store()?;
        let consolidator =
            HippocampusConsolidator::new(file_store, build_router(), "sonnet".to_string(), vec![]);

        assert_eq!(consolidator.lookback_days, 7);
        Ok(())
    }

    #[test]
    fn hippocampus_with_lookback_days() -> Result<()> {
        let (_dir, file_store) = build_file_store()?;
        let consolidator =
            HippocampusConsolidator::new(file_store, build_router(), "sonnet".to_string(), vec![])
                .with_lookback_days(30);

        assert_eq!(consolidator.lookback_days, 30);
        Ok(())
    }

    #[test]
    fn consolidation_scheduler_new() -> Result<()> {
        let (_dir, file_store) = build_file_store()?;
        let consolidator = Arc::new(HippocampusConsolidator::new(
            file_store,
            build_router(),
            "sonnet".to_string(),
            vec![],
        ));

        let scheduler = ConsolidationScheduler::new(Arc::clone(&consolidator), 24);
        assert_eq!(scheduler.interval_hours, 24);
        Ok(())
    }

    #[tokio::test]
    async fn consolidation_no_daily_files_returns_early() -> Result<()> {
        let (_dir, file_store) = build_file_store()?;
        file_store.write_long_term("# Memory\n\nExisting").await?;

        let consolidator =
            HippocampusConsolidator::new(file_store, build_router(), "sonnet".to_string(), vec![]);

        let report = consolidator.consolidate().await?;
        assert_eq!(report.daily_files_read, 0);
        assert!(!report.memory_updated);
        assert!(report.summary.contains("No daily files found"));
        Ok(())
    }

    #[tokio::test]
    async fn consolidation_triggers_reindex_after_write() -> Result<()> {
        use chrono::Local;
        use clawhive_memory::search_index::SearchIndex;
        use clawhive_memory::store::MemoryStore;

        // Create temp dir and file store
        let (dir, file_store) = build_file_store()?;
        let session_reader = SessionReader::new(dir.path());

        // Write MEMORY.md
        file_store
            .write_long_term("# Existing Memory\n\nSome knowledge.")
            .await?;

        // Write today's daily file
        let today = Local::now().date_naive();
        file_store
            .write_daily(today, "## Today's Observations\n\nLearned something new.")
            .await?;

        // Create in-memory MemoryStore and SearchIndex
        let memory_store = MemoryStore::open_in_memory()?;
        let search_index = SearchIndex::new(memory_store.db());

        // Create a stub embedding provider
        let embedding_provider = Arc::new(StubEmbeddingProvider);

        // Create consolidator with re-indexing enabled
        let consolidator = HippocampusConsolidator::new(
            file_store.clone(),
            build_router(),
            "sonnet".to_string(),
            vec![],
        )
        .with_search_index(search_index.clone())
        .with_embedding_provider(embedding_provider)
        .with_file_store_for_reindex(file_store)
        .with_session_reader_for_reindex(session_reader);

        // Run consolidation
        let report = consolidator.consolidate().await?;

        // Verify consolidation succeeded
        assert!(report.memory_updated);
        assert_eq!(report.daily_files_read, 1);

        // Verify re-indexing happened
        assert!(report.reindexed);

        Ok(())
    }

    struct StubEmbeddingProvider;

    #[async_trait]
    impl EmbeddingProvider for StubEmbeddingProvider {
        async fn embed(
            &self,
            texts: &[String],
        ) -> anyhow::Result<clawhive_memory::embedding::EmbeddingResult> {
            let embeddings = texts.iter().map(|_| vec![0.1; 384]).collect();
            Ok(clawhive_memory::embedding::EmbeddingResult {
                embeddings,
                model: "stub".to_string(),
                dimensions: 384,
            })
        }

        fn model_id(&self) -> &str {
            "stub"
        }

        fn dimensions(&self) -> usize {
            384
        }
    }
}
