use std::sync::Arc;

use anyhow::{anyhow, Result};
use chrono::Utc;
use clawhive_memory::embedding::EmbeddingProvider;
use clawhive_memory::fact_store::{self, Fact, FactStore};
use clawhive_memory::file_store::MemoryFileStore;
use clawhive_memory::search_index::SearchIndex;
use clawhive_memory::session::SessionReader;
use clawhive_memory::MemoryStore;
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

const FACT_EXTRACTION_SYSTEM_PROMPT: &str = r#"You are a fact extraction system. Extract key facts from the conversation summaries below.

Return a JSON array of facts. Each fact should have:
- "content": A clear, concise statement of the fact (e.g., "User prefers Rust over Go")
- "fact_type": One of: "preference", "decision", "event", "person", "rule"
- "importance": 0.0 to 1.0 (how important this fact is for future interactions)
- "occurred_at": ISO date string if the fact has a specific date, null otherwise

Rules:
- Extract only concrete, actionable facts. Skip pleasantries and transient details.
- Each fact should be self-contained and understandable without context.
- Deduplicate: if the same fact appears multiple times, include it only once.
- Return valid JSON only. No markdown fencing, no explanation.

Example output:
[
  {"content": "User prefers Rust over Go", "fact_type": "preference", "importance": 0.8, "occurred_at": null},
  {"content": "User moved to Tokyo", "fact_type": "event", "importance": 0.7, "occurred_at": "2026-03"}
]

If no facts can be extracted, return an empty array: []"#;

#[derive(Debug, serde::Deserialize)]
struct ExtractedFact {
    content: String,
    fact_type: String,
    #[serde(default = "default_importance")]
    importance: f64,
    occurred_at: Option<String>,
}

fn default_importance() -> f64 {
    0.5
}

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
    agent_id: String,
    pub(crate) file_store: MemoryFileStore,
    router: Arc<LlmRouter>,
    model_primary: String,
    model_fallbacks: Vec<String>,
    lookback_days: usize,
    search_index: Option<SearchIndex>,
    embedding_provider: Option<Arc<dyn EmbeddingProvider>>,
    reindex_file_store: Option<MemoryFileStore>,
    reindex_session_reader: Option<SessionReader>,
    memory_store: Option<Arc<MemoryStore>>,
}

#[derive(Debug)]
pub struct ConsolidationReport {
    pub daily_files_read: usize,
    pub memory_updated: bool,
    pub reindexed: bool,
    pub facts_extracted: usize,
    pub summary: String,
}

impl HippocampusConsolidator {
    pub fn new(
        agent_id: String,
        file_store: MemoryFileStore,
        router: Arc<LlmRouter>,
        model_primary: String,
        model_fallbacks: Vec<String>,
    ) -> Self {
        Self {
            agent_id,
            file_store,
            router,
            model_primary,
            model_fallbacks,
            lookback_days: 7,
            search_index: None,
            embedding_provider: None,
            reindex_file_store: None,
            reindex_session_reader: None,
            memory_store: None,
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

    pub fn with_memory_store(mut self, store: Arc<MemoryStore>) -> Self {
        self.memory_store = Some(store);
        self
    }

    pub fn with_session_reader_for_reindex(mut self, reader: SessionReader) -> Self {
        self.reindex_session_reader = Some(reader);
        self
    }

    pub fn agent_id(&self) -> &str {
        &self.agent_id
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
                facts_extracted: 0,
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
                    let facts_extracted = self.extract_facts(&self.agent_id, &daily_sections).await;
                    return Ok(ConsolidationReport {
                        daily_files_read: recent_daily.len(),
                        memory_updated: false,
                        reindexed: false,
                        facts_extracted,
                        summary: "Consolidation returned [KEEP]; MEMORY.md unchanged.".to_string(),
                    });
                }

                let updated_memory = apply_patch(&current_memory, &patch);
                return self
                    .finalize_updated_memory(
                        updated_memory,
                        &current_memory,
                        &daily_sections,
                        recent_daily.len(),
                    )
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
            let facts_extracted = self.extract_facts(&self.agent_id, &daily_sections).await;
            return Ok(ConsolidationReport {
                daily_files_read: recent_daily.len(),
                memory_updated: false,
                reindexed: false,
                facts_extracted,
                summary: "Consolidation returned [KEEP]; MEMORY.md unchanged.".to_string(),
            });
        }

        self.finalize_updated_memory(
            updated_memory,
            &current_memory,
            &daily_sections,
            recent_daily.len(),
        )
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
        daily_sections: &str,
        daily_files_read: usize,
    ) -> Result<ConsolidationReport> {
        if let Err(error) = validate_consolidation_output(&updated_memory, current_memory) {
            tracing::warn!(error = %error, "Skipping consolidation write due to invalid LLM output");
            return Ok(ConsolidationReport {
                daily_files_read,
                memory_updated: false,
                reindexed: false,
                facts_extracted: 0,
                summary: "Consolidation skipped because LLM output failed validation.".to_string(),
            });
        }

        if updated_memory == current_memory {
            tracing::info!("Consolidation patch produced no effective MEMORY.md changes");
            return Ok(ConsolidationReport {
                daily_files_read,
                memory_updated: false,
                reindexed: false,
                facts_extracted: 0,
                summary: "Consolidation produced no MEMORY.md changes.".to_string(),
            });
        }

        let deduped_memory = dedup_paragraphs(&updated_memory);
        if deduped_memory.len() < updated_memory.len() {
            tracing::info!(
                original_len = updated_memory.len(),
                deduped_len = deduped_memory.len(),
                "Dedup reduced MEMORY.md content"
            );
        }
        self.file_store.write_long_term(&deduped_memory).await?;

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

        let facts_extracted = self.extract_facts(&self.agent_id, daily_sections).await;

        if let Some(ref store) = self.memory_store {
            store
                .record_trace(
                    &self.agent_id,
                    "consolidation",
                    &serde_json::json!({
                        "daily_files_read": daily_files_read,
                        "reindexed": reindexed,
                        "facts_extracted": facts_extracted,
                        "memory_chars": updated_memory.len(),
                    })
                    .to_string(),
                    None,
                )
                .await;
        }

        Ok(ConsolidationReport {
            daily_files_read,
            memory_updated: true,
            reindexed,
            facts_extracted,
            summary: format!("Consolidated {daily_files_read} daily files into MEMORY.md."),
        })
    }

    async fn extract_facts(&self, agent_id: &str, daily_sections: &str) -> usize {
        let Some(memory_store) = &self.memory_store else {
            return 0;
        };

        let fact_store = FactStore::new(memory_store.db());

        match self
            .extract_facts_inner(agent_id, daily_sections, &fact_store)
            .await
        {
            Ok(count) => count,
            Err(error) => {
                tracing::warn!(agent_id, error = %error, "Fact extraction failed after consolidation");
                0
            }
        }
    }

    async fn extract_facts_inner(
        &self,
        agent_id: &str,
        daily_sections: &str,
        fact_store: &FactStore,
    ) -> Result<usize> {
        let response = self
            .request_consolidation(FACT_EXTRACTION_SYSTEM_PROMPT, daily_sections.to_string())
            .await?;
        let extracted =
            serde_json::from_str::<Vec<ExtractedFact>>(&strip_markdown_fence(&response.text))?;
        if extracted.is_empty() {
            return Ok(0);
        }

        let now = Utc::now().to_rfc3339();
        let mut active_facts = fact_store.get_active_facts(agent_id).await?;

        for extracted_fact in &extracted {
            let content = extracted_fact.content.trim();
            if content.is_empty() {
                continue;
            }

            let fact = Fact {
                id: fact_store::generate_fact_id(agent_id, content),
                agent_id: agent_id.to_string(),
                content: content.to_string(),
                fact_type: extracted_fact.fact_type.trim().to_string(),
                importance: extracted_fact.importance.clamp(0.0, 1.0),
                confidence: 1.0,
                status: "active".to_string(),
                occurred_at: extracted_fact.occurred_at.clone(),
                recorded_at: now.clone(),
                source_type: "consolidation".to_string(),
                source_session: None,
                access_count: 0,
                last_accessed: None,
                superseded_by: None,
                created_at: now.clone(),
                updated_at: now.clone(),
            };

            if fact_store
                .find_by_content(agent_id, &fact.content)
                .await?
                .is_some()
            {
                continue;
            }

            if let Some(conflict) = self
                .find_conflicting_fact(&fact, &active_facts)
                .await?
                .filter(|existing| existing.agent_id == agent_id)
            {
                fact_store
                    .supersede(&conflict.id, &fact, "Updated by consolidation")
                    .await?;
                active_facts.retain(|existing| existing.id != conflict.id);
                active_facts.push(fact);
                continue;
            }

            fact_store.insert_fact(&fact).await?;
            fact_store.record_add(&fact).await?;
            active_facts.push(fact);
        }

        Ok(extracted.len())
    }

    async fn find_conflicting_fact(
        &self,
        new_fact: &Fact,
        active_facts: &[Fact],
    ) -> Result<Option<Fact>> {
        let Some(provider) = &self.embedding_provider else {
            return Ok(None);
        };
        if active_facts.is_empty() {
            return Ok(None);
        }

        let mut texts = Vec::with_capacity(active_facts.len() + 1);
        texts.push(new_fact.content.clone());
        texts.extend(active_facts.iter().map(|fact| fact.content.clone()));

        let embeddings = provider.embed(&texts).await?.embeddings;
        if embeddings.len() != texts.len() {
            return Ok(None);
        }

        let new_embedding = &embeddings[0];
        let conflict = active_facts
            .iter()
            .zip(embeddings.iter().skip(1))
            .find(|(existing, embedding)| {
                existing.id != new_fact.id && cosine_similarity(new_embedding, embedding) > 0.85
            })
            .map(|(fact, _)| fact.clone());

        Ok(conflict)
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

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
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
    consolidators: Vec<Arc<HippocampusConsolidator>>,
    interval_hours: u64,
    archive_retention_days: u64,
}

impl ConsolidationScheduler {
    pub fn new(
        consolidators: Vec<Arc<HippocampusConsolidator>>,
        interval_hours: u64,
        archive_retention_days: u64,
    ) -> Self {
        Self {
            consolidators,
            interval_hours,
            archive_retention_days,
        }
    }

    pub fn start(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(tokio::time::Duration::from_secs(self.interval_hours * 3600));
            interval.tick().await;
            loop {
                interval.tick().await;
                tracing::info!(
                    agent_count = self.consolidators.len(),
                    "Running scheduled hippocampus consolidation for all agents..."
                );
                for consolidator in &self.consolidators {
                    let agent_id = consolidator.agent_id();
                    match consolidator.consolidate().await {
                        Ok(report) => {
                            tracing::info!(
                                agent_id = %agent_id,
                                daily_files_read = report.daily_files_read,
                                memory_updated = report.memory_updated,
                                "Consolidation complete for agent"
                            );
                        }
                        Err(err) => {
                            tracing::error!(agent_id = %agent_id, "Consolidation failed: {err}");
                        }
                    }
                }
                for consolidator in &self.consolidators {
                    let ws = consolidator.file_store.workspace_dir();
                    let writer = clawhive_memory::session::SessionWriter::new(ws);
                    if let Err(e) = writer.cleanup_archived(self.archive_retention_days).await {
                        tracing::warn!(
                            agent_id = %consolidator.agent_id(),
                            "Archived session cleanup failed: {e}"
                        );
                    }
                }
            }
        })
    }

    pub async fn run_once(&self) -> Vec<(String, Result<ConsolidationReport>)> {
        let mut results = Vec::new();
        for consolidator in &self.consolidators {
            let agent_id = consolidator.agent_id().to_string();
            let result = consolidator.consolidate().await;
            results.push((agent_id, result));
        }
        results
    }
}

fn dedup_paragraphs(content: &str) -> String {
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

fn normalized_word_set(text: &str) -> std::collections::HashSet<String> {
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

fn jaccard_similarity(
    a: &std::collections::HashSet<String>,
    b: &std::collections::HashSet<String>,
) -> f64 {
    let intersection = a.intersection(b).count();
    let union = a.union(b).count();
    if union == 0 {
        return 0.0;
    }

    intersection as f64 / union as f64
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use anyhow::Result;
    use async_trait::async_trait;
    use clawhive_memory::embedding::EmbeddingProvider;
    use clawhive_memory::fact_store::{generate_fact_id, Fact, FactStore};
    use clawhive_memory::file_store::MemoryFileStore;
    use clawhive_memory::session::SessionReader;
    use clawhive_memory::store::MemoryStore;
    use clawhive_provider::{LlmProvider, LlmRequest, LlmResponse, ProviderRegistry, StubProvider};
    use tempfile::TempDir;

    use super::{
        apply_patch, dedup_paragraphs, jaccard_similarity, parse_patch,
        validate_consolidation_output, AddInstruction, ConsolidationReport, ConsolidationScheduler,
        HippocampusConsolidator, MemoryPatch, UpdateInstruction,
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
            facts_extracted: 0,
            summary: "none".to_string(),
        };

        assert_eq!(report.daily_files_read, 0);
        assert!(!report.memory_updated);
        assert!(!report.reindexed);
        assert_eq!(report.facts_extracted, 0);
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
        let consolidator = HippocampusConsolidator::new(
            "agent-1".to_string(),
            file_store,
            build_router(),
            "sonnet".to_string(),
            vec![],
        );

        assert_eq!(consolidator.lookback_days, 7);
        Ok(())
    }

    #[test]
    fn hippocampus_with_lookback_days() -> Result<()> {
        let (_dir, file_store) = build_file_store()?;
        let consolidator = HippocampusConsolidator::new(
            "agent-1".to_string(),
            file_store,
            build_router(),
            "sonnet".to_string(),
            vec![],
        )
        .with_lookback_days(30);

        assert_eq!(consolidator.lookback_days, 30);
        Ok(())
    }

    #[test]
    fn consolidation_scheduler_new() -> Result<()> {
        let (_dir, file_store) = build_file_store()?;
        let consolidator = Arc::new(HippocampusConsolidator::new(
            "agent-1".to_string(),
            file_store,
            build_router(),
            "sonnet".to_string(),
            vec![],
        ));

        let scheduler = ConsolidationScheduler::new(vec![Arc::clone(&consolidator)], 24, 30);
        assert_eq!(scheduler.interval_hours, 24);
        Ok(())
    }

    #[tokio::test]
    async fn consolidation_no_daily_files_returns_early() -> Result<()> {
        let (_dir, file_store) = build_file_store()?;
        file_store.write_long_term("# Memory\n\nExisting").await?;

        let consolidator = HippocampusConsolidator::new(
            "agent-1".to_string(),
            file_store,
            build_router(),
            "sonnet".to_string(),
            vec![],
        );

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
            "agent-1".to_string(),
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

    #[tokio::test]
    async fn consolidation_extracts_and_supersedes_conflicting_facts() -> Result<()> {
        use chrono::{Local, Utc};

        let (_dir, file_store) = build_file_store()?;
        file_store.write_long_term("# Memory\n\nExisting").await?;

        let today = Local::now().date_naive();
        file_store
            .write_daily(today, "## Observations\n\nUser moved to Tokyo.")
            .await?;

        let memory_store = Arc::new(MemoryStore::open_in_memory()?);
        let fact_store = FactStore::new(memory_store.db());
        let now = Utc::now().to_rfc3339();
        let old_fact = Fact {
            id: generate_fact_id("agent-1", "User lives in Berlin"),
            agent_id: "agent-1".to_string(),
            content: "User lives in Berlin".to_string(),
            fact_type: "event".to_string(),
            importance: 0.6,
            confidence: 1.0,
            status: "active".to_string(),
            occurred_at: None,
            recorded_at: now.clone(),
            source_type: "consolidation".to_string(),
            source_session: None,
            access_count: 0,
            last_accessed: None,
            superseded_by: None,
            created_at: now.clone(),
            updated_at: now,
        };
        fact_store.insert_fact(&old_fact).await?;
        fact_store.record_add(&old_fact).await?;

        let router = build_router_with_provider(SequenceProvider::new(vec![
            "[KEEP]".to_string(),
            r#"[{"content":"User lives in Tokyo","fact_type":"event","importance":0.9,"occurred_at":null}]"#.to_string(),
        ]));

        let consolidator = HippocampusConsolidator::new(
            "agent-1".to_string(),
            file_store,
            router,
            "sonnet".to_string(),
            vec![],
        )
        .with_memory_store(Arc::clone(&memory_store))
        .with_embedding_provider(Arc::new(KeywordEmbeddingProvider));

        let report = consolidator.consolidate().await?;

        assert_eq!(report.facts_extracted, 1);
        let facts = fact_store.get_active_facts("agent-1").await?;
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].content, "User lives in Tokyo");

        let history = fact_store.get_history(&old_fact.id).await?;
        assert_eq!(history[0].event, "SUPERSEDE");
        Ok(())
    }

    fn build_router_with_provider(provider: Arc<dyn LlmProvider>) -> Arc<LlmRouter> {
        let mut registry = ProviderRegistry::new();
        registry.register("anthropic", provider);
        let aliases = HashMap::from([(
            "sonnet".to_string(),
            "anthropic/claude-sonnet-4-5".to_string(),
        )]);
        Arc::new(LlmRouter::new(registry, aliases, vec![]))
    }

    struct SequenceProvider {
        responses: Vec<String>,
        call_count: AtomicUsize,
    }

    impl SequenceProvider {
        fn new(responses: Vec<String>) -> Arc<Self> {
            Arc::new(Self {
                responses,
                call_count: AtomicUsize::new(0),
            })
        }
    }

    #[async_trait]
    impl LlmProvider for SequenceProvider {
        async fn chat(&self, _request: LlmRequest) -> Result<LlmResponse> {
            let index = self.call_count.fetch_add(1, Ordering::SeqCst);
            let text = self.responses.get(index).cloned().unwrap_or_default();
            Ok(LlmResponse {
                text,
                content: vec![],
                input_tokens: None,
                output_tokens: None,
                stop_reason: Some("end_turn".to_string()),
            })
        }
    }

    struct KeywordEmbeddingProvider;

    #[async_trait]
    impl EmbeddingProvider for KeywordEmbeddingProvider {
        async fn embed(
            &self,
            texts: &[String],
        ) -> anyhow::Result<clawhive_memory::embedding::EmbeddingResult> {
            let embeddings = texts
                .iter()
                .map(|text| {
                    if text.contains("lives in") {
                        vec![1.0, 0.0]
                    } else {
                        vec![0.0, 1.0]
                    }
                })
                .collect();
            Ok(clawhive_memory::embedding::EmbeddingResult {
                embeddings,
                model: "keyword".to_string(),
                dimensions: 2,
            })
        }

        fn model_id(&self) -> &str {
            "keyword"
        }

        fn dimensions(&self) -> usize {
            2
        }
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
