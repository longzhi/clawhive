use std::sync::Arc;

use anyhow::{anyhow, Result};
use chrono::Utc;
use clawhive_memory::dirty_sources::{DirtySourceStore, DIRTY_KIND_MEMORY_FILE};
use clawhive_memory::embedding::EmbeddingProvider;
use clawhive_memory::fact_store::{self, Fact, FactStore};
use clawhive_memory::file_store::MemoryFileStore;
use clawhive_memory::memory_lineage::MemoryLineageStore;
use clawhive_memory::search_index::SearchIndex;
use clawhive_memory::session::SessionReader;
use clawhive_memory::MemoryStore;
use clawhive_provider::LlmMessage;

use super::memory_document::{MemoryDocument, MEMORY_SECTION_ORDER};
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

const PROMOTION_CANDIDATE_SYSTEM_PROMPT: &str = r#"You classify daily observations for memory promotion.

Return a JSON array only. Each item must contain:
- "content": concise normalized statement
- "target_kind": one of "discard", "fact", "memory"
- "target_section": one of "长期项目主线", "持续性背景脉络", "关键历史决策" when target_kind is "memory", otherwise null
- "source_date": one of the `### YYYY-MM-DD` dates from the daily observations when known, otherwise null
- "importance": 0.0 to 1.0
- "duplicate_key": optional short key for deduplication

Rules:
- discard greetings, identity chatter, small talk, raw command output, receipts, and bilingual restatements
- choose "fact" for stable rules, preferences, identities, or durable atomic decisions
- choose "memory" only for long-lived narrative context that belongs in MEMORY.md
- prefer under-selection over over-selection
- return valid JSON only"#;

const SECTION_MERGE_SYSTEM_PROMPT: &str = r#"You update one MEMORY.md section.

Rules:
- Output ONLY the new section body content, no heading and no explanation
- Keep the section concise
- Remove duplicates and transient noise
- Preserve useful durable context
- Integrate the candidate items into coherent markdown bullet points or short paragraphs
- Do not repeat what is already captured in the section unless needed for clarity"#;

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

#[derive(Debug, Clone, serde::Deserialize)]
struct PromotionCandidate {
    content: String,
    target_kind: String,
    target_section: Option<String>,
    source_date: Option<String>,
    #[serde(default = "default_importance")]
    importance: f64,
    #[serde(default)]
    duplicate_key: Option<String>,
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
    embedding_cache_ttl_days: u64,
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
            embedding_cache_ttl_days: 90,
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

    pub fn with_embedding_cache_ttl_days(mut self, ttl_days: u64) -> Self {
        self.embedding_cache_ttl_days = ttl_days;
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
        if let Some(ref store) = self.memory_store {
            if let Err(error) = store
                .cleanup_expired_embedding_cache(self.embedding_cache_ttl_days)
                .await
            {
                tracing::warn!("Embedding cache cleanup failed: {error}");
            }
        }

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

        match self
            .consolidate_by_section(&current_memory, &daily_sections, recent_daily.len())
            .await
        {
            Ok(report) => return Ok(report),
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "Section-based consolidation failed, falling back to legacy consolidation"
                );
            }
        }

        self.legacy_consolidate(&current_memory, &daily_sections, recent_daily.len())
            .await
    }

    async fn legacy_consolidate(
        &self,
        current_memory: &str,
        daily_sections: &str,
        daily_files_read: usize,
    ) -> Result<ConsolidationReport> {
        let response = self
            .request_consolidation(
                CONSOLIDATION_INCREMENTAL_SYSTEM_PROMPT,
                build_incremental_user_prompt(current_memory, daily_sections),
            )
            .await?;

        let patch_output = strip_markdown_fence(&response.text);
        match parse_patch(&patch_output) {
            Ok(patch) => {
                if patch.keep {
                    tracing::info!("Consolidation returned [KEEP]; leaving MEMORY.md unchanged");
                    let facts_extracted = self.extract_facts(&self.agent_id, daily_sections).await;
                    return Ok(ConsolidationReport {
                        daily_files_read,
                        memory_updated: false,
                        reindexed: false,
                        facts_extracted,
                        summary: "Consolidation returned [KEEP]; MEMORY.md unchanged.".to_string(),
                    });
                }

                let updated_memory = apply_patch(current_memory, &patch);
                return self
                    .finalize_updated_memory(
                        updated_memory,
                        current_memory,
                        daily_sections,
                        daily_files_read,
                        &[],
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
                build_full_overwrite_user_prompt(current_memory, daily_sections),
            )
            .await?;

        let updated_memory = strip_markdown_fence(&response.text);
        if updated_memory.trim() == "[KEEP]" {
            tracing::info!("Consolidation returned [KEEP]; leaving MEMORY.md unchanged");
            let facts_extracted = self.extract_facts(&self.agent_id, daily_sections).await;
            return Ok(ConsolidationReport {
                daily_files_read,
                memory_updated: false,
                reindexed: false,
                facts_extracted,
                summary: "Consolidation returned [KEEP]; MEMORY.md unchanged.".to_string(),
            });
        }

        self.finalize_updated_memory(
            updated_memory,
            current_memory,
            daily_sections,
            daily_files_read,
            &[],
        )
        .await
    }

    async fn consolidate_by_section(
        &self,
        current_memory: &str,
        daily_sections: &str,
        daily_files_read: usize,
    ) -> Result<ConsolidationReport> {
        let candidates = self.extract_promotion_candidates(daily_sections).await?;
        let memory_candidates = dedup_memory_candidates(candidates);

        if memory_candidates.is_empty() {
            let facts_extracted = self.extract_facts(&self.agent_id, daily_sections).await;
            return Ok(ConsolidationReport {
                daily_files_read,
                memory_updated: false,
                reindexed: false,
                facts_extracted,
                summary: "No long-term memory candidates retained.".to_string(),
            });
        }

        let mut doc = MemoryDocument::parse(current_memory);
        let mut touched_sections = 0;

        for section in MEMORY_SECTION_ORDER {
            let section_candidates = memory_candidates
                .iter()
                .filter(|candidate| candidate.target_section.as_deref() == Some(section))
                .cloned()
                .collect::<Vec<_>>();

            if section_candidates.is_empty() {
                continue;
            }

            let merged = self
                .merge_memory_section(section, &doc.section_content(section), &section_candidates)
                .await?;
            let old_content = doc.section_content(section).to_string();
            doc.replace_section(section, &merged);
            let new_content = doc.section_content(section).to_string();

            if let Some(ref store) = self.memory_store {
                store
                    .record_trace(
                        &self.agent_id,
                        "section_merge",
                        &serde_json::json!({
                            "section": section,
                            "old_len": old_content.len(),
                            "new_len": new_content.len(),
                            "diff": compute_line_diff(&old_content, &new_content),
                        })
                        .to_string(),
                        None,
                    )
                    .await;
            }
            touched_sections += 1;
        }

        if touched_sections == 0 {
            let facts_extracted = self.extract_facts(&self.agent_id, daily_sections).await;
            return Ok(ConsolidationReport {
                daily_files_read,
                memory_updated: false,
                reindexed: false,
                facts_extracted,
                summary: "No MEMORY.md sections were updated.".to_string(),
            });
        }

        self.finalize_updated_memory(
            doc.render(),
            current_memory,
            daily_sections,
            daily_files_read,
            &memory_candidates,
        )
        .await
    }

    async fn extract_promotion_candidates(
        &self,
        daily_sections: &str,
    ) -> Result<Vec<PromotionCandidate>> {
        let response = self
            .request_consolidation(
                PROMOTION_CANDIDATE_SYSTEM_PROMPT,
                build_promotion_candidate_prompt(daily_sections),
            )
            .await?;
        let parsed = strip_markdown_fence(&response.text);
        let candidates: Vec<PromotionCandidate> = serde_json::from_str(parsed.trim())?;
        Ok(candidates)
    }

    async fn merge_memory_section(
        &self,
        section: &str,
        current_section: &str,
        candidates: &[PromotionCandidate],
    ) -> Result<String> {
        let response = self
            .request_consolidation(
                SECTION_MERGE_SYSTEM_PROMPT,
                build_section_merge_prompt(section, current_section, candidates),
            )
            .await?;
        Ok(strip_markdown_fence(&response.text).trim().to_string())
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
        memory_candidates: &[PromotionCandidate],
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
            let facts_extracted = self.extract_facts(&self.agent_id, daily_sections).await;
            return Ok(ConsolidationReport {
                daily_files_read,
                memory_updated: false,
                reindexed: false,
                facts_extracted,
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
        let cleanup_result = clawhive_memory::file_audit::cleanup_memory_file(
            &deduped_memory,
            clawhive_memory::file_audit::MemoryFileKind::LongTerm,
        );
        if cleanup_result.stats.removed_prompt_leakage_lines > 0
            || cleanup_result.stats.removed_empty_headings > 0
            || cleanup_result.stats.removed_duplicate_bullets > 0
        {
            tracing::info!(
                stats = ?cleanup_result.stats,
                "Cleaned up MEMORY.md before write"
            );
        }

        self.file_store
            .write_long_term(&cleanup_result.content)
            .await?;
        self.record_memory_lineage(current_memory, &cleanup_result.content, memory_candidates)
            .await;

        let reindexed =
            if let (Some(index), Some(provider), Some(fs), Some(reader), Some(memory_store)) = (
                &self.search_index,
                &self.embedding_provider,
                &self.reindex_file_store,
                &self.reindex_session_reader,
                &self.memory_store,
            ) {
                let dirty = DirtySourceStore::new(memory_store.db());
                match dirty
                    .enqueue(
                        &self.agent_id,
                        DIRTY_KIND_MEMORY_FILE,
                        "MEMORY.md",
                        "consolidation_write",
                    )
                    .await
                {
                    Ok(()) => match index.index_dirty(fs, reader, provider.as_ref(), 8).await {
                        Ok(count) => {
                            self.record_memory_chunk_lineage(
                                &cleanup_result.content,
                                memory_candidates,
                            )
                            .await;
                            tracing::info!(
                                "Post-consolidation incremental reindex: {count} chunks indexed"
                            );
                            true
                        }
                        Err(e) => {
                            tracing::warn!("Post-consolidation incremental reindex failed: {e}");
                            false
                        }
                    },
                    Err(e) => {
                        tracing::warn!("Failed to enqueue MEMORY.md dirty source: {e}");
                        false
                    }
                }
            } else {
                false
            };

        let facts_extracted = self.extract_facts(&self.agent_id, daily_sections).await;
        self.record_fact_memory_alignment(&cleanup_result.content)
            .await;

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

    async fn record_memory_lineage(
        &self,
        previous_memory: &str,
        updated_memory: &str,
        memory_candidates: &[PromotionCandidate],
    ) {
        let Some(memory_store) = &self.memory_store else {
            return;
        };

        let previous_doc = MemoryDocument::parse(previous_memory);
        let updated_doc = MemoryDocument::parse(updated_memory);
        let lineage_store = MemoryLineageStore::new(memory_store.db());
        for section in MEMORY_SECTION_ORDER {
            let previous_items = previous_doc.section_items(section);
            let updated_items = updated_doc.section_items(section);
            if updated_items.is_empty() {
                continue;
            }

            for item in updated_items {
                let matched_candidate =
                    best_matching_candidate_for_item(section, &item, memory_candidates);
                let unchanged = previous_items
                    .iter()
                    .any(|old| normalize_lineage_text(old) == normalize_lineage_text(&item));
                let canonical_key = matched_candidate
                    .and_then(|candidate| candidate.duplicate_key.as_deref())
                    .map(str::trim)
                    .filter(|value| !value.is_empty());

                if matched_candidate.is_none() && unchanged {
                    continue;
                }

                let source_date =
                    matched_candidate.and_then(|candidate| candidate.source_date.as_deref());
                let canonical_id = match lineage_store
                    .link_memory_promotion(
                        &self.agent_id,
                        &item,
                        source_date,
                        section,
                        canonical_key,
                    )
                    .await
                {
                    Ok(canonical_id) => canonical_id,
                    Err(error) => {
                        tracing::warn!(
                            agent_id = %self.agent_id,
                            section,
                            content = %item,
                            error = %error,
                            "failed to record retained memory lineage"
                        );
                        continue;
                    }
                };

                if let Some(candidate) = matched_candidate {
                    if let Err(error) = lineage_store
                        .link_memory_to_daily_canonical(
                            &self.agent_id,
                            &canonical_id,
                            &candidate.content,
                            candidate.source_date.as_deref(),
                            canonical_key,
                        )
                        .await
                    {
                        tracing::warn!(
                            agent_id = %self.agent_id,
                            section,
                            canonical_id = %canonical_id,
                            error = %error,
                            "failed to bridge memory canonical to daily canonical"
                        );
                    }
                }

                if let Some(source_date) = source_date {
                    let daily_path = format!("memory/{source_date}.md");
                    if let Err(error) = lineage_store
                        .attach_matching_chunks(
                            &self.agent_id,
                            &canonical_id,
                            &daily_path,
                            &item,
                            "derived",
                        )
                        .await
                    {
                        tracing::warn!(
                            agent_id = %self.agent_id,
                            section,
                            path = %daily_path,
                            content = %item,
                            error = %error,
                            "failed to record daily chunk lineage"
                        );
                    }
                }

                if canonical_key.is_some() {
                    continue;
                }

                for old_item in previous_items
                    .iter()
                    .filter(|old| should_link_supersedes(old, &item))
                {
                    if let Err(error) = lineage_store
                        .link_memory_supersedes(&self.agent_id, &item, old_item)
                        .await
                    {
                        tracing::warn!(
                            agent_id = %self.agent_id,
                            section,
                            new_item = %item,
                            old_item = %old_item,
                            error = %error,
                            "failed to record memory supersedes lineage"
                        );
                    }
                }
            }
        }
    }

    async fn record_memory_chunk_lineage(
        &self,
        updated_memory: &str,
        memory_candidates: &[PromotionCandidate],
    ) {
        let Some(memory_store) = &self.memory_store else {
            return;
        };

        let updated_doc = MemoryDocument::parse(updated_memory);
        let lineage_store = MemoryLineageStore::new(memory_store.db());
        for section in MEMORY_SECTION_ORDER {
            for item in updated_doc.section_items(section) {
                let matched_candidate =
                    best_matching_candidate_for_item(section, &item, memory_candidates);
                let canonical_key = matched_candidate
                    .and_then(|candidate| candidate.duplicate_key.as_deref())
                    .map(str::trim)
                    .filter(|value| !value.is_empty());
                let canonical = match lineage_store
                    .ensure_canonical_with_key(&self.agent_id, "memory", canonical_key, &item)
                    .await
                {
                    Ok(canonical) => canonical,
                    Err(error) => {
                        tracing::warn!(
                            agent_id = %self.agent_id,
                            section,
                            content = %item,
                            error = %error,
                            "failed to ensure memory canonical before chunk linkage"
                        );
                        continue;
                    }
                };

                if let Err(error) = lineage_store
                    .attach_matching_chunks_in_section(
                        &self.agent_id,
                        &canonical.canonical_id,
                        "MEMORY.md",
                        section,
                        &item,
                        "promoted",
                    )
                    .await
                {
                    tracing::warn!(
                        agent_id = %self.agent_id,
                        section,
                        content = %item,
                        error = %error,
                        "failed to record memory chunk lineage"
                    );
                }
            }
        }
    }

    async fn record_fact_memory_alignment(&self, memory_text: &str) {
        let Some(memory_store) = &self.memory_store else {
            return;
        };

        let doc = MemoryDocument::parse(memory_text);
        let memory_items = MEMORY_SECTION_ORDER
            .iter()
            .flat_map(|section| doc.section_items(section))
            .collect::<Vec<_>>();
        if memory_items.is_empty() {
            return;
        }

        let fact_store = FactStore::new(memory_store.db());
        let lineage_store = MemoryLineageStore::new(memory_store.db());
        let facts = match fact_store.get_active_facts(&self.agent_id).await {
            Ok(facts) => facts,
            Err(error) => {
                tracing::warn!(
                    agent_id = %self.agent_id,
                    error = %error,
                    "failed to load facts for fact-memory alignment"
                );
                return;
            }
        };

        for fact in facts {
            let Some(item) = best_matching_memory_item_for_fact(&fact, &memory_items) else {
                continue;
            };
            let canonical = match lineage_store
                .find_canonical_by_summary(&self.agent_id, "memory", item)
                .await
            {
                Ok(Some(canonical)) => canonical,
                Ok(None) => continue,
                Err(error) => {
                    tracing::warn!(
                        agent_id = %self.agent_id,
                        fact_id = %fact.id,
                        error = %error,
                        "failed to resolve memory canonical for fact alignment"
                    );
                    continue;
                }
            };

            if let Err(error) = lineage_store
                .attach_source(
                    &self.agent_id,
                    &canonical.canonical_id,
                    "fact",
                    &fact.id,
                    "equivalent",
                )
                .await
            {
                tracing::warn!(
                    agent_id = %self.agent_id,
                    fact_id = %fact.id,
                    canonical_id = %canonical.canonical_id,
                    error = %error,
                    "failed to record fact-memory alignment"
                );
            }
        }
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

fn build_promotion_candidate_prompt(daily_sections: &str) -> String {
    format!(
        "## Recent Daily Observations\n{}\n\nReturn ONLY a JSON array of promotion candidates.",
        daily_sections
    )
}

fn build_section_merge_prompt(
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

fn dedup_memory_candidates(candidates: Vec<PromotionCandidate>) -> Vec<PromotionCandidate> {
    let mut kept = Vec::new();
    let mut seen = std::collections::BTreeSet::new();

    for candidate in candidates {
        if candidate.target_kind != "memory" || candidate.importance < 0.3 {
            continue;
        }

        let Some(section) = candidate.target_section.as_deref() else {
            continue;
        };
        if !MEMORY_SECTION_ORDER.contains(&section) {
            continue;
        }

        let content = candidate.content.trim();
        if content.is_empty() {
            continue;
        }

        let key = candidate
            .duplicate_key
            .as_deref()
            .map(|value| value.trim().to_lowercase())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| content.to_lowercase());

        if !seen.insert((section.to_string(), key)) {
            continue;
        }

        kept.push(PromotionCandidate {
            content: content.to_string(),
            ..candidate
        });
    }

    kept
}

fn best_matching_candidate_for_item<'a>(
    section: &str,
    item: &str,
    candidates: &'a [PromotionCandidate],
) -> Option<&'a PromotionCandidate> {
    let item_normalized = normalize_lineage_text(item);
    let item_words = tokenize_lineage_text(item);
    let section_candidates = candidates
        .iter()
        .filter(|candidate| candidate.target_section.as_deref() == Some(section))
        .collect::<Vec<_>>();

    if let Some((candidate, _)) = section_candidates
        .iter()
        .filter_map(|candidate| {
            let candidate_normalized = normalize_lineage_text(&candidate.content);
            let candidate_words = tokenize_lineage_text(&candidate.content);
            let similarity = if item_normalized.contains(&candidate_normalized)
                || candidate_normalized.contains(&item_normalized)
            {
                1.0
            } else {
                jaccard_similarity(&item_words, &candidate_words)
            };
            (similarity >= 0.55).then_some((candidate, similarity))
        })
        .max_by(|(_, left), (_, right)| left.total_cmp(right))
    {
        return Some(*candidate);
    }

    let keyed_candidates = section_candidates
        .into_iter()
        .filter(|candidate| {
            candidate
                .duplicate_key
                .as_deref()
                .map(str::trim)
                .is_some_and(|value| !value.is_empty())
        })
        .collect::<Vec<_>>();

    if keyed_candidates.len() == 1 {
        return keyed_candidates.into_iter().next();
    }

    None
}

fn should_link_supersedes(old_item: &str, new_item: &str) -> bool {
    let old_normalized = normalize_lineage_text(old_item);
    let new_normalized = normalize_lineage_text(new_item);
    if old_normalized.is_empty() || new_normalized.is_empty() || old_normalized == new_normalized {
        return false;
    }
    let old_words = tokenize_lineage_text(old_item);
    let new_words = tokenize_lineage_text(new_item);
    jaccard_similarity(&old_words, &new_words) >= 0.5
}

fn best_matching_memory_item_for_fact<'a>(
    fact: &Fact,
    memory_items: &'a [String],
) -> Option<&'a str> {
    let fact_normalized = normalize_lineage_text(&fact.content);
    let fact_words = tokenize_lineage_text(&fact.content);
    memory_items
        .iter()
        .filter_map(|item| {
            let item_normalized = normalize_lineage_text(item);
            let item_words = tokenize_lineage_text(item);
            let similarity = if item_normalized.contains(&fact_normalized)
                || fact_normalized.contains(&item_normalized)
            {
                1.0
            } else {
                jaccard_similarity(&fact_words, &item_words)
            };
            (similarity >= 0.6).then_some((item.as_str(), similarity))
        })
        .max_by(|(_, left), (_, right)| left.total_cmp(right))
        .map(|(item, _)| item)
}

fn tokenize_lineage_text(input: &str) -> std::collections::HashSet<String> {
    input
        .split(|c: char| !c.is_alphanumeric() && !('\u{4E00}'..='\u{9FFF}').contains(&c))
        .filter(|part| !part.is_empty())
        .map(|part| part.to_lowercase())
        .collect()
}

fn normalize_lineage_text(input: &str) -> String {
    input
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
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
    cron_expr: String,
    archive_retention_days: u64,
}

impl ConsolidationScheduler {
    pub fn new(
        consolidators: Vec<Arc<HippocampusConsolidator>>,
        cron_expr: String,
        archive_retention_days: u64,
    ) -> Self {
        Self {
            consolidators,
            cron_expr,
            archive_retention_days,
        }
    }

    pub fn start(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let local_tz = iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".to_string());
            loop {
                let now_ms = Utc::now().timestamp_millis();
                let schedule = clawhive_scheduler::ScheduleType::Cron {
                    expr: self.cron_expr.clone(),
                    tz: local_tz.clone(),
                };
                let next_ms = match clawhive_scheduler::compute_next_run_at_ms(&schedule, now_ms) {
                    Ok(Some(ms)) => ms,
                    Ok(None) => {
                        tracing::error!(
                            cron_expr = %self.cron_expr,
                            "Consolidation cron schedule has no upcoming fire time"
                        );
                        return;
                    }
                    Err(e) => {
                        tracing::error!(
                            cron_expr = %self.cron_expr,
                            error = %e,
                            "Failed to compute next consolidation time"
                        );
                        return;
                    }
                };
                let delay_ms = (next_ms - now_ms).max(0) as u64;
                tracing::info!(
                    cron_expr = %self.cron_expr,
                    tz = %local_tz,
                    next_run_in_secs = delay_ms / 1000,
                    "Consolidation scheduled"
                );
                tokio::time::sleep(tokio::time::Duration::from_millis(delay_ms)).await;

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
                    if let Some(store) = &consolidator.memory_store {
                        if let Err(e) = store
                            .cleanup_session_memory_state(self.archive_retention_days)
                            .await
                        {
                            tracing::warn!(
                                agent_id = %consolidator.agent_id(),
                                "Session memory state cleanup failed: {e}"
                            );
                        }
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

fn compute_line_diff(old: &str, new: &str) -> Vec<String> {
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
    use clawhive_memory::memory_lineage::MemoryLineageStore;
    use clawhive_memory::session::SessionReader;
    use clawhive_memory::store::MemoryStore;
    use clawhive_provider::{LlmProvider, LlmRequest, LlmResponse, ProviderRegistry, StubProvider};
    use tempfile::TempDir;

    use super::{
        apply_patch, compute_line_diff, dedup_paragraphs, jaccard_similarity, parse_patch,
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
    fn compute_line_diff_marks_added_and_removed_lines() {
        let old_content = "line kept\nline removed\n";
        let new_content = "line kept\nline added\n";

        let diff = compute_line_diff(old_content, new_content);

        assert_eq!(diff, vec!["- line removed", "+ line added"]);
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

        let scheduler = ConsolidationScheduler::new(
            vec![Arc::clone(&consolidator)],
            "0 4 * * *".to_string(),
            30,
        );
        assert_eq!(scheduler.cron_expr, "0 4 * * *");
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
    async fn consolidation_cleans_expired_embedding_cache_before_run() -> Result<()> {
        let (_dir, file_store) = build_file_store()?;
        file_store.write_long_term("# Memory\n\nExisting").await?;

        let memory_store = Arc::new(MemoryStore::open_in_memory()?);
        {
            let db = memory_store.db();
            let conn = db.lock().expect("lock db");
            conn.execute(
                &format!(
                    "INSERT INTO embedding_cache (provider, model, provider_key, hash, embedding, dims, updated_at) VALUES ('openai', 'text-embedding-3-small', 'key1', 'hash-old', '[0.1,0.2]', 2, '{}')",
                    (chrono::Utc::now() - chrono::TimeDelta::days(120)).to_rfc3339()
                ),
                [],
            )?;
        }

        let consolidator = HippocampusConsolidator::new(
            "agent-1".to_string(),
            file_store,
            build_router(),
            "sonnet".to_string(),
            vec![],
        )
        .with_memory_store(Arc::clone(&memory_store))
        .with_embedding_cache_ttl_days(30);

        let _ = consolidator.consolidate().await?;

        let remaining_old: i64 = {
            let db = memory_store.db();
            let conn = db.lock().expect("lock db");
            conn.query_row(
                "SELECT COUNT(*) FROM embedding_cache WHERE hash = 'hash-old'",
                [],
                |row| row.get(0),
            )?
        };
        assert_eq!(remaining_old, 0);

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
        let memory_store = Arc::new(MemoryStore::open_in_memory()?);
        let search_index = SearchIndex::new(memory_store.db(), "agent-1");

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
        .with_memory_store(Arc::clone(&memory_store))
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
            "[]".to_string(),
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

        let lineage_store = MemoryLineageStore::new(memory_store.db());
        let links = lineage_store
            .get_links_for_source("agent-1", "fact", &facts[0].id)
            .await?;
        assert_eq!(links.len(), 1);

        let history = fact_store.get_history(&old_fact.id).await?;
        assert_eq!(history[0].event, "SUPERSEDE");
        Ok(())
    }

    #[tokio::test]
    async fn section_based_consolidation_updates_only_target_section() -> Result<()> {
        use chrono::Local;

        let (_dir, file_store) = build_file_store()?;
        file_store
            .write_long_term(
                "# MEMORY.md\n\n## 长期项目主线\n\n- Existing project note\n\n## 持续性背景脉络\n\n- Keep context\n\n## 关键历史决策\n\n- Keep decision\n",
            )
            .await?;

        let today = Local::now().date_naive();
        file_store
            .write_daily(
                today,
                "## Memory\n\n- User decided to use section-based consolidation.",
            )
            .await?;

        let router = build_router_with_provider(SequenceProvider::new(vec![
            r#"[{"content":"Adopt section-based consolidation for memory refactor","target_kind":"memory","target_section":"长期项目主线","source_date":"2026-03-29","importance":0.9,"duplicate_key":"memory-refactor"}]"#.to_string(),
            "- Existing project note\n- Adopt section-based consolidation for memory refactor".to_string(),
            "[]".to_string(),
        ]));

        let memory_store = Arc::new(MemoryStore::open_in_memory()?);
        let consolidator = HippocampusConsolidator::new(
            "agent-1".to_string(),
            file_store.clone(),
            router,
            "sonnet".to_string(),
            vec![],
        )
        .with_memory_store(Arc::clone(&memory_store));

        let report = consolidator.consolidate().await?;
        let updated = file_store.read_long_term().await?;
        let lineage_store = MemoryLineageStore::new(memory_store.db());

        assert!(report.memory_updated);
        assert!(updated.contains("## 长期项目主线"));
        assert!(updated.contains("Adopt section-based consolidation for memory refactor"));
        assert!(updated.contains("## 持续性背景脉络\n\n- Keep context"));
        assert!(updated.contains("## 关键历史决策\n\n- Keep decision"));
        let canonical_id = clawhive_memory::memory_lineage::generate_canonical_id_with_key(
            "agent-1",
            "memory",
            Some("memory-refactor"),
            "Adopt section-based consolidation for memory refactor",
        );
        let daily_links = lineage_store
            .get_links_for_source(
                "agent-1",
                "daily_section",
                &format!("memory/2026-03-29.md#{canonical_id}"),
            )
            .await?;
        let memory_links = lineage_store
            .get_links_for_source(
                "agent-1",
                "memory_section",
                &format!("MEMORY.md#长期项目主线#{canonical_id}"),
            )
            .await?;
        assert_eq!(daily_links.len(), 1);
        assert_eq!(memory_links.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn section_based_consolidation_records_memory_chunk_lineage_after_reindex() -> Result<()>
    {
        use chrono::Local;
        use clawhive_memory::search_index::SearchIndex;

        let (dir, file_store) = build_file_store()?;
        file_store
            .write_long_term(
                "# MEMORY.md\n\n## 长期项目主线\n\n- Existing project note\n\n## 持续性背景脉络\n\n- Keep context\n\n## 关键历史决策\n\n- Keep decision\n",
            )
            .await?;

        let today = Local::now().date_naive();
        file_store
            .write_daily(
                today,
                "## Memory\n\n- User decided to use section-based consolidation.",
            )
            .await?;

        let router = build_router_with_provider(SequenceProvider::new(vec![
            r#"[{"content":"Adopt section-based consolidation for memory refactor","target_kind":"memory","target_section":"长期项目主线","source_date":"2026-03-29","importance":0.9,"duplicate_key":"memory-refactor"}]"#.to_string(),
            "- Existing project note\n- Adopt section-based consolidation for memory refactor".to_string(),
            "[]".to_string(),
        ]));

        let memory_store = Arc::new(MemoryStore::open_in_memory()?);
        let search_index = SearchIndex::new(memory_store.db(), "agent-1");
        let session_reader = SessionReader::new(dir.path());
        let consolidator = HippocampusConsolidator::new(
            "agent-1".to_string(),
            file_store.clone(),
            router,
            "sonnet".to_string(),
            vec![],
        )
        .with_memory_store(Arc::clone(&memory_store))
        .with_search_index(search_index.clone())
        .with_embedding_provider(Arc::new(StubEmbeddingProvider))
        .with_file_store_for_reindex(file_store.clone())
        .with_session_reader_for_reindex(session_reader);

        let report = consolidator.consolidate().await?;
        assert!(report.memory_updated);
        assert!(report.reindexed);

        let canonical_id = clawhive_memory::memory_lineage::generate_canonical_id_with_key(
            "agent-1",
            "memory",
            Some("memory-refactor"),
            "Adopt section-based consolidation for memory refactor",
        );
        let db = memory_store.db();
        let chunk_id: String = {
            let conn = db.lock().unwrap();
            conn.query_row(
                "SELECT id FROM chunks WHERE agent_id = 'agent-1' AND path = 'MEMORY.md' AND text LIKE '%Adopt section-based consolidation for memory refactor%' LIMIT 1",
                [],
                |row| row.get(0),
            )?
        };

        let lineage_store = MemoryLineageStore::new(memory_store.db());
        let chunk_links = lineage_store
            .get_links_for_source("agent-1", "chunk", &chunk_id)
            .await?;
        assert!(!chunk_links.is_empty());
        assert!(chunk_links
            .iter()
            .any(|link| link.canonical_id == canonical_id));
        Ok(())
    }

    #[tokio::test]
    async fn section_based_consolidation_records_supersedes_for_unkeyed_retained_item_rewrite(
    ) -> Result<()> {
        use chrono::Local;

        let (_dir, file_store) = build_file_store()?;
        file_store
            .write_long_term(
                "# MEMORY.md\n\n## 长期项目主线\n\n- Use incremental patch consolidation for memory\n",
            )
            .await?;

        let today = Local::now().date_naive();
        file_store
            .write_daily(
                today,
                "## Memory\n\n- Consolidation moved to section-based merge.",
            )
            .await?;

        let router = build_router_with_provider(SequenceProvider::new(vec![
            format!(
                r#"[{{"content":"Use section-based consolidation for memory","target_kind":"memory","target_section":"长期项目主线","source_date":"{}","importance":0.9}}]"#,
                today.format("%Y-%m-%d")
            ),
            "- Use section-based consolidation for memory".to_string(),
            "[]".to_string(),
        ]));

        let memory_store = Arc::new(MemoryStore::open_in_memory()?);
        let consolidator = HippocampusConsolidator::new(
            "agent-1".to_string(),
            file_store,
            router,
            "sonnet".to_string(),
            vec![],
        )
        .with_memory_store(Arc::clone(&memory_store));

        let report = consolidator.consolidate().await?;
        assert!(report.memory_updated);

        let lineage_store = MemoryLineageStore::new(memory_store.db());
        let old_id = clawhive_memory::memory_lineage::generate_canonical_id(
            "agent-1",
            "memory",
            "Use incremental patch consolidation for memory",
        );
        let new_id = clawhive_memory::memory_lineage::generate_canonical_id(
            "agent-1",
            "memory",
            "Use section-based consolidation for memory",
        );
        let supersedes_links = lineage_store
            .get_links_for_source("agent-1", "canonical", &old_id)
            .await?;

        assert_eq!(supersedes_links.len(), 1);
        assert_eq!(supersedes_links[0].canonical_id, new_id);
        assert_eq!(supersedes_links[0].relation, "supersedes");
        Ok(())
    }

    #[tokio::test]
    async fn section_based_consolidation_reuses_canonical_for_keyed_retained_item_rewrite(
    ) -> Result<()> {
        use chrono::Local;

        let (_dir, file_store) = build_file_store()?;
        file_store
            .write_long_term(
                "# MEMORY.md\n\n## 长期项目主线\n\n- Use incremental patch consolidation for memory\n",
            )
            .await?;

        let today = Local::now().date_naive();
        file_store
            .write_daily(
                today,
                "## Memory\n\n- Consolidation moved to section-based merge.",
            )
            .await?;

        let router = build_router_with_provider(SequenceProvider::new(vec![
            format!(
                r#"[{{"content":"Use section-based consolidation for memory","target_kind":"memory","target_section":"长期项目主线","source_date":"{}","importance":0.9,"duplicate_key":"memory-consolidation"}}]"#,
                today.format("%Y-%m-%d")
            ),
            "- Use section-based consolidation for memory".to_string(),
            "[]".to_string(),
        ]));

        let memory_store = Arc::new(MemoryStore::open_in_memory()?);
        let consolidator = HippocampusConsolidator::new(
            "agent-1".to_string(),
            file_store,
            router,
            "sonnet".to_string(),
            vec![],
        )
        .with_memory_store(Arc::clone(&memory_store));

        let report = consolidator.consolidate().await?;
        assert!(report.memory_updated);

        let lineage_store = MemoryLineageStore::new(memory_store.db());
        let keyed_id = clawhive_memory::memory_lineage::generate_canonical_id_with_key(
            "agent-1",
            "memory",
            Some("memory-consolidation"),
            "Use section-based consolidation for memory",
        );
        let old_text_id = clawhive_memory::memory_lineage::generate_canonical_id(
            "agent-1",
            "memory",
            "Use incremental patch consolidation for memory",
        );
        let memory_links = lineage_store
            .get_links_for_source(
                "agent-1",
                "memory_section",
                &format!("MEMORY.md#长期项目主线#{keyed_id}"),
            )
            .await?;
        let supersedes_links = lineage_store
            .get_links_for_source("agent-1", "canonical", &old_text_id)
            .await?;

        assert_eq!(memory_links.len(), 1);
        assert!(supersedes_links.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn section_based_consolidation_bridges_daily_canonical_to_memory_canonical() -> Result<()>
    {
        use chrono::Local;

        let (_dir, file_store) = build_file_store()?;
        file_store
            .write_long_term(
                "# MEMORY.md\n\n## 长期项目主线\n\n- Use incremental patch consolidation for memory\n",
            )
            .await?;

        let today = Local::now().date_naive();
        file_store
            .write_daily(
                today,
                "## Memory\n\n- Use section-based consolidation for memory",
            )
            .await?;

        let router = build_router_with_provider(SequenceProvider::new(vec![
            format!(
                r#"[{{"content":"Use section-based consolidation for memory","target_kind":"memory","target_section":"长期项目主线","source_date":"{}","importance":0.9,"duplicate_key":"memory-consolidation"}}]"#,
                today.format("%Y-%m-%d")
            ),
            "- Use section-based consolidation for memory".to_string(),
            "[]".to_string(),
        ]));

        let memory_store = Arc::new(MemoryStore::open_in_memory()?);
        let lineage_store = MemoryLineageStore::new(memory_store.db());
        let daily_canonical = lineage_store
            .ensure_canonical_with_key(
                "agent-1",
                "daily",
                Some("memory-consolidation"),
                "Use section-based consolidation for memory",
            )
            .await?;

        let consolidator = HippocampusConsolidator::new(
            "agent-1".to_string(),
            file_store,
            router,
            "sonnet".to_string(),
            vec![],
        )
        .with_memory_store(Arc::clone(&memory_store));

        let report = consolidator.consolidate().await?;
        assert!(report.memory_updated);

        let memory_canonical = clawhive_memory::memory_lineage::generate_canonical_id_with_key(
            "agent-1",
            "memory",
            Some("memory-consolidation"),
            "Use section-based consolidation for memory",
        );
        let supersedes_links = lineage_store
            .get_links_for_source("agent-1", "canonical", &daily_canonical.canonical_id)
            .await?;

        assert_eq!(supersedes_links.len(), 1);
        assert_eq!(supersedes_links[0].canonical_id, memory_canonical);
        assert_eq!(supersedes_links[0].relation, "supersedes");
        Ok(())
    }

    #[tokio::test]
    async fn section_based_consolidation_bridges_keyed_daily_canonical_across_rewrite() -> Result<()>
    {
        use chrono::Local;

        let (_dir, file_store) = build_file_store()?;
        file_store
            .write_long_term(
                "# MEMORY.md\n\n## 长期项目主线\n\n- Use incremental patch consolidation for memory\n",
            )
            .await?;

        let today = Local::now().date_naive();
        file_store
            .write_daily(
                today,
                "## Memory\n\n- Use incremental patch consolidation for memory",
            )
            .await?;

        let router = build_router_with_provider(SequenceProvider::new(vec![
            format!(
                r#"[{{"content":"Use incremental patch consolidation for memory","target_kind":"memory","target_section":"长期项目主线","source_date":"{}","importance":0.9,"duplicate_key":"memory-consolidation"}}]"#,
                today.format("%Y-%m-%d")
            ),
            "- Use section-based consolidation for memory".to_string(),
            "[]".to_string(),
        ]));

        let memory_store = Arc::new(MemoryStore::open_in_memory()?);
        let lineage_store = MemoryLineageStore::new(memory_store.db());
        let daily_canonical = lineage_store
            .ensure_canonical_with_key(
                "agent-1",
                "daily",
                Some("memory-consolidation"),
                "Use incremental patch consolidation for memory",
            )
            .await?;

        let consolidator = HippocampusConsolidator::new(
            "agent-1".to_string(),
            file_store,
            router,
            "sonnet".to_string(),
            vec![],
        )
        .with_memory_store(Arc::clone(&memory_store));

        let report = consolidator.consolidate().await?;
        assert!(report.memory_updated);

        let memory_canonical = clawhive_memory::memory_lineage::generate_canonical_id_with_key(
            "agent-1",
            "memory",
            Some("memory-consolidation"),
            "Use section-based consolidation for memory",
        );
        let supersedes_links = lineage_store
            .get_links_for_source("agent-1", "canonical", &daily_canonical.canonical_id)
            .await?;

        assert_eq!(supersedes_links.len(), 1);
        assert_eq!(supersedes_links[0].canonical_id, memory_canonical);
        assert_eq!(supersedes_links[0].relation, "supersedes");
        Ok(())
    }

    #[tokio::test]
    async fn section_based_consolidation_aligns_fact_to_memory_canonical() -> Result<()> {
        use chrono::Local;

        let (_dir, file_store) = build_file_store()?;
        file_store
            .write_long_term("# MEMORY.md\n\n## 长期项目主线\n\n- Existing project note\n")
            .await?;

        let today = Local::now().date_naive();
        file_store
            .write_daily(
                today,
                "## Memory\n\n- Use section-based consolidation for memory",
            )
            .await?;

        let router = build_router_with_provider(SequenceProvider::new(vec![
            format!(
                r#"[{{"content":"Use section-based consolidation for memory","target_kind":"memory","target_section":"长期项目主线","source_date":"{}","importance":0.9,"duplicate_key":"memory-consolidation"}}]"#,
                today.format("%Y-%m-%d")
            ),
            "- Existing project note\n- Use section-based consolidation for memory".to_string(),
            r#"[{"content":"Use section-based consolidation for memory","fact_type":"decision","importance":0.9,"occurred_at":null}]"#.to_string(),
        ]));

        let memory_store = Arc::new(MemoryStore::open_in_memory()?);
        let consolidator = HippocampusConsolidator::new(
            "agent-1".to_string(),
            file_store,
            router,
            "sonnet".to_string(),
            vec![],
        )
        .with_memory_store(Arc::clone(&memory_store));

        let report = consolidator.consolidate().await?;
        assert!(report.memory_updated);
        assert_eq!(report.facts_extracted, 1);

        let fact_store = FactStore::new(memory_store.db());
        let fact = fact_store
            .find_by_content("agent-1", "Use section-based consolidation for memory")
            .await?
            .expect("fact should exist");
        let lineage_store = MemoryLineageStore::new(memory_store.db());
        let links = lineage_store
            .get_links_for_source("agent-1", "fact", &fact.id)
            .await?;

        let mut has_memory_canonical = false;
        for link in links {
            if lineage_store
                .get_canonical(&link.canonical_id)
                .await?
                .is_some_and(|canonical| canonical.canonical_kind == "memory")
            {
                has_memory_canonical = true;
                break;
            }
        }

        assert!(has_memory_canonical);
        Ok(())
    }

    #[tokio::test]
    async fn section_based_consolidation_can_skip_memory_update_but_extract_facts() -> Result<()> {
        use chrono::Local;

        let (_dir, file_store) = build_file_store()?;
        file_store.write_long_term("# MEMORY.md\n").await?;
        let today = Local::now().date_naive();
        file_store
            .write_daily(today, "## Context\n\n- User prefers Rust over Go.")
            .await?;

        let router = build_router_with_provider(SequenceProvider::new(vec![
            "[]".to_string(),
            r#"[{"content":"User prefers Rust over Go","fact_type":"preference","importance":0.8,"occurred_at":null}]"#.to_string(),
        ]));

        let memory_store = Arc::new(MemoryStore::open_in_memory()?);
        let consolidator = HippocampusConsolidator::new(
            "agent-1".to_string(),
            file_store,
            router,
            "sonnet".to_string(),
            vec![],
        )
        .with_memory_store(memory_store);

        let report = consolidator.consolidate().await?;

        assert!(!report.memory_updated);
        assert_eq!(report.facts_extracted, 1);
        Ok(())
    }

    #[tokio::test]
    async fn daily_entries_do_not_promote_until_consolidation_runs() -> Result<()> {
        use chrono::Local;

        let (_dir, file_store) = build_file_store()?;
        file_store.write_long_term("# MEMORY.md\n").await?;
        let today = Local::now().date_naive();
        file_store
            .write_daily(
                today,
                "## Memory\n\n- Use section-based consolidation for memory",
            )
            .await?;

        let memory_store = Arc::new(MemoryStore::open_in_memory()?);
        let fact_store = FactStore::new(memory_store.db());

        let before_memory = file_store.read_long_term().await?;
        let before_facts = fact_store.get_active_facts("agent-1").await?;
        assert_eq!(before_memory.trim(), "# MEMORY.md");
        assert!(before_facts.is_empty());

        let router = build_router_with_provider(SequenceProvider::new(vec![
            format!(
                r#"[{{"content":"Use section-based consolidation for memory","target_kind":"memory","target_section":"长期项目主线","source_date":"{}","importance":0.9,"duplicate_key":"memory-consolidation"}}]"#,
                today.format("%Y-%m-%d")
            ),
            "- Use section-based consolidation for memory".to_string(),
            r#"[{"content":"Use section-based consolidation for memory","fact_type":"decision","importance":0.9,"occurred_at":null}]"#.to_string(),
        ]));

        let consolidator = HippocampusConsolidator::new(
            "agent-1".to_string(),
            file_store.clone(),
            router,
            "sonnet".to_string(),
            vec![],
        )
        .with_memory_store(Arc::clone(&memory_store));

        let report = consolidator.consolidate().await?;
        assert!(report.memory_updated);
        assert_eq!(report.facts_extracted, 1);

        let after_memory = file_store.read_long_term().await?;
        let after_facts = fact_store.get_active_facts("agent-1").await?;
        assert!(after_memory.contains("Use section-based consolidation for memory"));
        assert_eq!(after_facts.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn consolidation_sanitizes_prompt_leakage_before_memory_write() -> Result<()> {
        use chrono::Local;

        let (_dir, file_store) = build_file_store()?;
        file_store
            .write_long_term(
                "# MEMORY.md\n\n## 长期项目主线\n\n- Existing project note\n\n## 持续性背景脉络\n\n- Keep context\n\n## 关键历史决策\n\n- Keep decision\n",
            )
            .await?;

        let today = Local::now().date_naive();
        file_store
            .write_daily(today, "## Memory\n\n- User updated a durable project note.")
            .await?;

        let router = build_router_with_provider(SequenceProvider::new(vec![
            format!(
                r#"[{{"content":"Capture durable project note","target_kind":"memory","target_section":"长期项目主线","source_date":"{}","importance":0.9,"duplicate_key":"durable-note"}}]"#,
                today.format("%Y-%m-%d")
            ),
            "- Existing project note\nPlease synthesize the daily observations.\n- Another durable fact"
                .to_string(),
            "[]".to_string(),
        ]));

        let consolidator = HippocampusConsolidator::new(
            "agent-1".to_string(),
            file_store.clone(),
            router,
            "sonnet".to_string(),
            vec![],
        );

        let report = consolidator.consolidate().await?;
        assert!(report.memory_updated);

        let updated = file_store.read_long_term().await?;
        assert!(!updated.contains("Please synthesize the daily observations."));
        assert!(updated.contains("Another durable fact"));
        Ok(())
    }

    #[tokio::test]
    async fn consolidation_scheduler_does_not_run_immediately_on_start() -> Result<()> {
        use chrono::Local;

        let (_dir, file_store) = build_file_store()?;
        file_store.write_long_term("# MEMORY.md\n").await?;
        let today = Local::now().date_naive();
        file_store
            .write_daily(today, "## Context\n\n- Stable observation.")
            .await?;

        let provider =
            SequenceProvider::new(vec!["[]".to_string(), "[]".to_string(), "[]".to_string()]);
        let router = build_router_with_provider(provider.clone());
        let consolidator = Arc::new(HippocampusConsolidator::new(
            "agent-1".to_string(),
            file_store,
            router,
            "sonnet".to_string(),
            vec![],
        ));

        let scheduler =
            ConsolidationScheduler::new(vec![consolidator], "0 4 * * *".to_string(), 30);
        let handle = scheduler.start();
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
        handle.abort();

        assert_eq!(
            provider.call_count.load(Ordering::SeqCst),
            0,
            "scheduler should wait one full interval before first automatic consolidation"
        );
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
