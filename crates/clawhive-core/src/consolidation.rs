use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use chrono::{DateTime, Datelike, Duration, Utc};
use clawhive_memory::dirty_sources::{DirtySourceStore, DIRTY_KIND_MEMORY_FILE};
use clawhive_memory::embedding::EmbeddingProvider;
use clawhive_memory::fact_store::{Fact, FactStore};
use clawhive_memory::file_store::MemoryFileStore;
use clawhive_memory::memory_lineage::MemoryLineageStore;
use clawhive_memory::search_index::SearchIndex;
use clawhive_memory::session::SessionReader;
use clawhive_memory::{EpisodeStatusRecord, FlushPhase, MemoryStore, SessionMemoryStateRecord};
use clawhive_provider::LlmMessage;
use tokio::task;

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

const STALE_SECTION_CONFIRM_SYSTEM_PROMPT: &str = r#"You evaluate whether a MEMORY.md section is stale.

Return exactly one token:
- STALE: safe to archive
- KEEP: should remain in MEMORY.md"#;

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

#[derive(Debug, Clone)]
struct StaleSectionCandidate {
    section: String,
    content: String,
    staleness_score: f64,
    days_since_accessed: f64,
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
    session_idle_minutes: i64,
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
            session_idle_minutes: 30,
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

    pub fn with_session_idle_minutes(mut self, idle_minutes: i64) -> Self {
        self.session_idle_minutes = idle_minutes.max(1);
        self
    }

    pub fn with_session_reader_for_reindex(mut self, reader: SessionReader) -> Self {
        self.reindex_session_reader = Some(reader);
        self
    }

    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    pub(crate) fn search_index(&self) -> Option<&SearchIndex> {
        self.search_index.as_ref()
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
            let report = ConsolidationReport {
                daily_files_read: 0,
                memory_updated: false,
                reindexed: false,
                facts_extracted: 0,
                summary: "No daily files found in lookback window; skipped consolidation."
                    .to_string(),
            };
            self.reconcile_recent_fact_conflicts().await;
            return Ok(report);
        }

        let mut daily_sections = String::new();
        for (date, content) in &recent_daily {
            daily_sections.push_str(&format!("### {}\n{}\n\n", date.format("%Y-%m-%d"), content));
        }

        let mut report = match self
            .consolidate_by_section(&current_memory, &daily_sections, recent_daily.len())
            .await
        {
            Ok(report) => report,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "Section-based consolidation failed, falling back to legacy consolidation"
                );
                self.legacy_consolidate(&current_memory, &daily_sections, recent_daily.len())
                    .await?
            }
        };

        let stale_candidates = self.evaluate_memory_staleness().await?;
        let pruned_count = self.prune_stale_sections(&stale_candidates).await?;
        if pruned_count > 0 {
            report.memory_updated = true;
            report.summary = format!(
                "{} Pruned {pruned_count} stale section(s) into MEMORY_ARCHIVED.md.",
                report.summary
            );
        }

        self.reconcile_recent_fact_conflicts().await;
        Ok(report)
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
                    return Ok(ConsolidationReport {
                        daily_files_read,
                        memory_updated: false,
                        reindexed: false,
                        facts_extracted: 0,
                        summary: "Consolidation returned [KEEP]; MEMORY.md unchanged.".to_string(),
                    });
                }

                let updated_memory = apply_patch(current_memory, &patch);
                return self
                    .finalize_updated_memory(updated_memory, current_memory, daily_files_read, &[])
                    .await;
            }
            Err(first_error) => {
                tracing::warn!(error = %first_error, "Patch parse failed, retrying with stricter prompt");

                let retry_response = self
                    .request_consolidation(
                        CONSOLIDATION_INCREMENTAL_SYSTEM_PROMPT,
                        build_incremental_user_prompt(current_memory, daily_sections),
                    )
                    .await?;
                let retry_output = strip_markdown_fence(&retry_response.text);

                match parse_patch(&retry_output) {
                    Ok(patch) => {
                        if patch.keep {
                            tracing::info!(
                                "Consolidation retry returned [KEEP]; leaving MEMORY.md unchanged"
                            );
                            return Ok(ConsolidationReport {
                                daily_files_read,
                                memory_updated: false,
                                reindexed: false,
                                facts_extracted: 0,
                                summary: "Consolidation returned [KEEP]; MEMORY.md unchanged."
                                    .to_string(),
                            });
                        }

                        let updated_memory = apply_patch(current_memory, &patch);
                        return self
                            .finalize_updated_memory(
                                updated_memory,
                                current_memory,
                                daily_files_read,
                                &[],
                            )
                            .await;
                    }
                    Err(second_error) => {
                        tracing::warn!(
                            error = %second_error,
                            "Retry also failed, falling back to full overwrite"
                        );
                        if let Some(ref store) = self.memory_store {
                            let _ = store
                                .record_trace(
                                    &self.agent_id,
                                    "consolidation_fallback",
                                    &serde_json::json!({
                                        "first_error": first_error.to_string(),
                                        "second_error": second_error.to_string(),
                                    })
                                    .to_string(),
                                    None,
                                )
                                .await;
                        }
                    }
                }
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
            return Ok(ConsolidationReport {
                daily_files_read,
                memory_updated: false,
                reindexed: false,
                facts_extracted: 0,
                summary: "Consolidation returned [KEEP]; MEMORY.md unchanged.".to_string(),
            });
        }

        self.finalize_updated_memory(updated_memory, current_memory, daily_files_read, &[])
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
            return Ok(ConsolidationReport {
                daily_files_read,
                memory_updated: false,
                reindexed: false,
                facts_extracted: 0,
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
            return Ok(ConsolidationReport {
                daily_files_read,
                memory_updated: false,
                reindexed: false,
                facts_extracted: 0,
                summary: "No MEMORY.md sections were updated.".to_string(),
            });
        }

        self.finalize_updated_memory(
            doc.render(),
            current_memory,
            daily_files_read,
            &memory_candidates,
        )
        .await
    }

    async fn evaluate_memory_staleness(&self) -> Result<Vec<StaleSectionCandidate>> {
        let current_memory = self.file_store.read_long_term().await?;
        let doc = MemoryDocument::parse(&current_memory);
        let mut candidates = Vec::new();

        for section in MEMORY_SECTION_ORDER {
            let content = doc.section_content(section);
            if content.trim().is_empty() {
                continue;
            }

            let (total_access_count, max_last_accessed) =
                self.query_memory_section_access_stats(&content).await?;

            let Some(last_accessed) = max_last_accessed else {
                continue;
            };
            if total_access_count <= 0 {
                continue;
            }

            let Ok(last_accessed_at) = DateTime::parse_from_rfc3339(&last_accessed) else {
                tracing::warn!(section, value = %last_accessed, "Invalid last_accessed timestamp");
                continue;
            };

            let days_since_accessed = (Utc::now() - last_accessed_at.with_timezone(&Utc))
                .num_seconds()
                .max(0) as f64
                / 86_400.0;
            let staleness_score = days_since_accessed / reference_half_life_days(section);

            if staleness_score > 3.0 {
                candidates.push(StaleSectionCandidate {
                    section: section.to_string(),
                    content,
                    staleness_score,
                    days_since_accessed,
                });
            }
        }

        Ok(candidates)
    }

    async fn query_memory_section_access_stats(
        &self,
        section_text: &str,
    ) -> Result<(i64, Option<String>)> {
        if let Some(index) = &self.search_index {
            return index
                .query_section_access_stats("MEMORY.md", section_text)
                .await;
        }

        Ok((0, None))
    }

    async fn prune_stale_sections(&self, candidates: &[StaleSectionCandidate]) -> Result<usize> {
        if candidates.is_empty() {
            return Ok(0);
        }

        let mut doc = MemoryDocument::parse(&self.file_store.read_long_term().await?);
        let archived_at = Utc::now().format("%Y-%m-%d").to_string();
        let mut pruned = 0usize;

        for candidate in candidates {
            let response = self
                .request_consolidation(
                    STALE_SECTION_CONFIRM_SYSTEM_PROMPT,
                    format!(
                        "Section: {}\nStaleness score: {:.2}\nDays since last accessed: {:.1}\n\nContent:\n{}\n\nIs this section stale and safe to archive? Answer STALE if yes, KEEP if no.",
                        candidate.section,
                        candidate.staleness_score,
                        candidate.days_since_accessed,
                        candidate.content,
                    ),
                )
                .await?;

            let decision = response.text.trim().to_ascii_uppercase();
            if !(decision.contains("STALE") || decision.contains("YES")) {
                tracing::info!(section = %candidate.section, decision = %response.text, "Stale candidate rejected by LLM");
                continue;
            }

            self.file_store
                .append_archived_section(&candidate.section, &candidate.content, &archived_at)
                .await?;
            doc.remove_section(&candidate.section);
            pruned += 1;
            tracing::info!(
                section = %candidate.section,
                staleness_score = candidate.staleness_score,
                "Archived stale MEMORY.md section"
            );
        }

        if pruned > 0 {
            self.file_store.write_long_term(&doc.render()).await?;
        }

        Ok(pruned)
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

        let reindexed = if let (
            Some(index),
            Some(provider),
            Some(fs),
            Some(reader),
            Some(memory_store),
        ) = (
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
                Ok(()) => match index
                    .process_dirty_sources(&dirty, &self.agent_id, fs, reader, provider.as_ref(), 8)
                    .await
                {
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
                        "facts_extracted": 0,
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
            facts_extracted: 0,
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

    async fn reconcile_recent_fact_conflicts(&self) {
        let Some(memory_store) = &self.memory_store else {
            return;
        };

        let fact_store = FactStore::new(memory_store.db());
        let mut active_facts = match fact_store.get_active_facts(&self.agent_id).await {
            Ok(facts) => facts,
            Err(error) => {
                tracing::warn!(agent_id = %self.agent_id, error = %error, "failed to load active facts for consolidation reconciliation");
                return;
            }
        };

        if active_facts.len() < 2 {
            return;
        }

        let cutoff = (Utc::now() - Duration::hours(24)).to_rfc3339();
        let recent_facts = active_facts
            .iter()
            .filter(|fact| fact.created_at > cutoff)
            .cloned()
            .collect::<Vec<_>>();

        for recent in recent_facts {
            if active_facts.iter().all(|fact| fact.id != recent.id) {
                continue;
            }

            let others = active_facts
                .iter()
                .filter(|fact| fact.id != recent.id)
                .cloned()
                .collect::<Vec<_>>();
            if others.is_empty() {
                continue;
            }

            let (step_a, step_a_failed) = match self.find_conflicting_fact(&recent, &others).await {
                Ok(conflict) => (conflict, false),
                Err(error) => {
                    tracing::warn!(
                        agent_id = %self.agent_id,
                        recent_fact_id = %recent.id,
                        error = %error,
                        "embedding-based conflict check failed during consolidation reconciliation; falling back to step-B"
                    );
                    (None, true)
                }
            };

            let conflict = if self.embedding_provider.is_some() && !step_a_failed {
                step_a.filter(|candidate| fact_conflict_step_b_passes(&recent, candidate))
            } else {
                others
                    .into_iter()
                    .find(|candidate| fact_conflict_step_b_passes(&recent, candidate))
            };

            let Some(conflict) = conflict else {
                continue;
            };

            let confirmed = match self
                .confirm_fact_conflict_with_llm(&recent, &conflict)
                .await
            {
                Ok(true) => true,
                Ok(false) => {
                    tracing::info!(
                        agent_id = %self.agent_id,
                        recent_fact_id = %recent.id,
                        conflict_fact_id = %conflict.id,
                        "LLM rejected conflict candidate during reconciliation; skipping supersede"
                    );
                    false
                }
                Err(error) => {
                    tracing::warn!(
                        agent_id = %self.agent_id,
                        recent_fact_id = %recent.id,
                        conflict_fact_id = %conflict.id,
                        error = %error,
                        "LLM conflict confirmation failed; skipping supersede (conservative)"
                    );
                    false
                }
            };

            if !confirmed {
                continue;
            }

            if let Err(error) = self
                .supersede_with_existing_fact(&conflict, &recent, "auto_consolidation_reconcile")
                .await
            {
                tracing::warn!(
                    agent_id = %self.agent_id,
                    old_fact_id = %conflict.id,
                    replacement_fact_id = %recent.id,
                    error = %error,
                    "failed to auto-supersede fact during consolidation reconciliation"
                );
                continue;
            }

            tracing::info!(
                agent_id = %self.agent_id,
                old_fact_id = %conflict.id,
                replacement_fact_id = %recent.id,
                reason = "auto_consolidation_reconcile",
                "auto-superseded conflicting fact during 04:00 reconciliation"
            );

            active_facts.retain(|fact| fact.id != conflict.id);
        }
    }

    async fn confirm_fact_conflict_with_llm(
        &self,
        recent: &Fact,
        candidate: &Fact,
    ) -> Result<bool> {
        let prompt = format!(
            "Compare these two facts about the same user:\n\n\
            Fact A (older): \"{}\"\nType: {}\n\n\
            Fact B (newer): \"{}\"\nType: {}\n\n\
            Question: Is Fact B a direct update or correction of Fact A? \
            (e.g. preference change, updated decision, corrected information)\n\n\
            Reply with exactly \"yes\" or \"no\". \
            Answer \"yes\" only if they are clearly about the same specific subject \
            and Fact B supersedes Fact A.",
            candidate.content, candidate.fact_type, recent.content, recent.fact_type
        );

        let response = self
            .router
            .chat(
                &self.model_primary,
                &self.model_fallbacks,
                None,
                vec![LlmMessage::user(prompt)],
                16,
            )
            .await?;

        let answer = response.text.trim().to_lowercase();
        Ok(answer.starts_with("yes"))
    }

    async fn supersede_with_existing_fact(
        &self,
        old_fact: &Fact,
        replacement_fact: &Fact,
        reason: &str,
    ) -> Result<()> {
        let Some(memory_store) = &self.memory_store else {
            return Err(anyhow!("memory store unavailable for fact reconciliation"));
        };

        let db = memory_store.db();
        let old_fact_id = old_fact.id.clone();
        let old_content = old_fact.content.clone();
        let replacement_id = replacement_fact.id.clone();
        let replacement_content = replacement_fact.content.clone();
        let reason = reason.to_string();

        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let tx = conn.transaction()?;
            let now = Utc::now().to_rfc3339();
            let updated = tx.execute(
                "UPDATE facts SET status = 'superseded', superseded_by = ?1, updated_at = ?2 WHERE id = ?3 AND status = 'active'",
                [&replacement_id, &now, &old_fact_id],
            )?;
            if updated == 0 {
                tx.rollback()?;
                return Ok(());
            }

            tx.execute(
                "INSERT INTO fact_history (id, fact_id, event, old_content, new_content, reason, created_at) VALUES (?1, ?2, 'SUPERSEDE', ?3, ?4, ?5, ?6)",
                [
                    &format!(
                        "reconcile-{}-{}",
                        Utc::now().timestamp_nanos_opt().unwrap_or_default(),
                        replacement_id
                    ),
                    &old_fact_id,
                    &old_content,
                    &replacement_content,
                    &reason,
                    &now,
                ],
            )?;

            tx.commit()?;
            Ok(())
        })
        .await??;

        Ok(())
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

fn reference_half_life_days(section: &str) -> f64 {
    match section {
        "长期项目主线" => 30.0,
        "持续性背景脉络" => 60.0,
        "关键历史决策" => 90.0,
        _ => 90.0,
    }
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
                for consolidator in &self.consolidators {
                    if let Err(error) =
                        Self::scan_stale_sessions_and_trigger_boundary_flush(consolidator).await
                    {
                        tracing::warn!(
                            agent_id = %consolidator.agent_id(),
                            %error,
                            "Stale session boundary flush scan failed"
                        );
                    }
                }
                let decay_now = Utc::now();
                for consolidator in &self.consolidators {
                    if let Err(error) =
                        Self::run_weekly_confidence_decay_if_due(consolidator, decay_now).await
                    {
                        tracing::warn!(
                            agent_id = %consolidator.agent_id(),
                            %error,
                            "Weekly confidence decay check failed"
                        );
                    }
                }
                for consolidator in &self.consolidators {
                    // TODO: Unit 12 Phase 2: 180-day archive physical deletion
                    if let Err(error) = Self::run_daily_file_lifecycle(
                        consolidator,
                        self.archive_retention_days,
                        decay_now,
                    )
                    .await
                    {
                        tracing::warn!(
                            agent_id = %consolidator.agent_id(),
                            %error,
                            "Daily file lifecycle management failed"
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

    async fn scan_stale_sessions_and_trigger_boundary_flush(
        consolidator: &Arc<HippocampusConsolidator>,
    ) -> Result<()> {
        let Some(store) = &consolidator.memory_store else {
            return Ok(());
        };

        const MAX_STALE_SESSIONS_PER_SCAN: usize = 5;
        const DEAD_FLUSH_TIMEOUT_MINUTES: i64 = 10;

        let mut candidates = Vec::new();
        let dead = store.find_dead_flushes(DEAD_FLUSH_TIMEOUT_MINUTES).await?;
        candidates.extend(
            dead.into_iter()
                .filter(|state| state.agent_id == consolidator.agent_id)
                .take(MAX_STALE_SESSIONS_PER_SCAN),
        );

        if candidates.len() < MAX_STALE_SESSIONS_PER_SCAN {
            let open_episode_stale = Self::find_stale_open_episode_states(
                store,
                consolidator.agent_id(),
                consolidator.session_idle_minutes,
                MAX_STALE_SESSIONS_PER_SCAN - candidates.len(),
            )
            .await?;
            for state in open_episode_stale {
                if candidates
                    .iter()
                    .any(|existing| existing.session_id == state.session_id)
                {
                    continue;
                }
                candidates.push(state);
                if candidates.len() >= MAX_STALE_SESSIONS_PER_SCAN {
                    break;
                }
            }
        }

        for state in candidates {
            if let Err(error) = Self::trigger_stale_boundary_flush(store, state).await {
                tracing::warn!(
                    agent_id = %consolidator.agent_id(),
                    %error,
                    "Failed to trigger stale boundary flush"
                );
            }
        }

        Ok(())
    }

    async fn trigger_stale_boundary_flush(
        store: &Arc<MemoryStore>,
        mut state: SessionMemoryStateRecord,
    ) -> Result<()> {
        let now = Utc::now();
        for episode in &mut state.open_episodes {
            if episode.status == EpisodeStatusRecord::Open {
                episode.status = EpisodeStatusRecord::Closed;
                episode.last_activity_at = now;
            }
        }

        state.pending_flush = true;
        state.flush_phase = FlushPhase::Captured.as_str().to_string();
        state.flush_phase_updated_at = Some(now.to_rfc3339());

        store.upsert_session_memory_state(state).await
    }

    async fn find_stale_open_episode_states(
        store: &Arc<MemoryStore>,
        agent_id: &str,
        idle_minutes: i64,
        limit: usize,
    ) -> Result<Vec<SessionMemoryStateRecord>> {
        store
            .find_stale_open_episode_states(agent_id, idle_minutes, limit)
            .await
    }

    async fn run_daily_file_lifecycle(
        consolidator: &Arc<HippocampusConsolidator>,
        archive_retention_days: u64,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let today = now.date_naive();

        for (date, _) in consolidator.file_store.list_daily_files().await? {
            let age_days = today.signed_duration_since(date).num_days();
            if age_days <= archive_retention_days as i64 {
                continue;
            }

            match consolidator.file_store.archive_daily(date).await {
                Ok(_) => {
                    let old_path = format!("memory/{}.md", date.format("%Y-%m-%d"));
                    let new_path = format!("memory/archive/{}.md", date.format("%Y-%m-%d"));
                    let chunks_updated = if let Some(search_index) = consolidator.search_index() {
                        match search_index.update_chunk_path(&old_path, &new_path).await {
                            Ok(count) => count,
                            Err(error) => {
                                tracing::warn!(
                                    agent_id = %consolidator.agent_id(),
                                    date = %date,
                                    %error,
                                    "chunk path update failed after archive, orphan detection will catch inconsistency"
                                );
                                0
                            }
                        }
                    } else {
                        0
                    };

                    tracing::info!(
                        agent_id = %consolidator.agent_id(),
                        date = %date,
                        old_path = %old_path,
                        new_path = %new_path,
                        chunks_updated,
                        "archived daily file and updated chunk paths"
                    );
                }
                Err(error) => tracing::warn!(
                    agent_id = %consolidator.agent_id(),
                    date = %date,
                    action = "archive",
                    %error,
                    "Daily file archive failed"
                ),
            }
        }

        for (date, _) in consolidator.file_store.list_archived_files().await? {
            let age_days = today.signed_duration_since(date).num_days();
            if age_days <= 90 {
                continue;
            }

            let archived_rel_path = format!("memory/archive/{}.md", date.format("%Y-%m-%d"));
            let total_access = match &consolidator.memory_store {
                Some(store) => {
                    Self::query_archived_chunk_access_count(
                        store,
                        consolidator.agent_id(),
                        &archived_rel_path,
                    )
                    .await?
                }
                None => {
                    tracing::info!(
                        agent_id = %consolidator.agent_id(),
                        date = %date,
                        action = "retain",
                        reason = "memory_store_unavailable",
                        "Daily file lifecycle action"
                    );
                    continue;
                }
            };

            if total_access == 0 {
                consolidator.file_store.delete_archived_daily(date).await?;
                if let Some(search_index) = consolidator.search_index() {
                    if let Err(error) = search_index.delete_indexed_path(&archived_rel_path).await {
                        tracing::warn!(
                            agent_id = %consolidator.agent_id(),
                            date = %date,
                            %error,
                            "chunk deletion failed after archive file removal"
                        );
                    }
                }
                tracing::info!(
                    agent_id = %consolidator.agent_id(),
                    date = %date,
                    path = %archived_rel_path,
                    "deleted archived daily file and associated chunks"
                );
                continue;
            }

            tracing::info!(
                agent_id = %consolidator.agent_id(),
                date = %date,
                action = "retain",
                total_access_count = total_access,
                "Daily file lifecycle action"
            );
        }

        if let Some(search_index) = consolidator.search_index() {
            let known_paths = Self::collect_known_chunk_paths(consolidator).await?;
            let orphan_paths = search_index.detect_orphan_chunks(&known_paths).await?;
            if !orphan_paths.is_empty() {
                tracing::warn!(
                    agent_id = %consolidator.agent_id(),
                    orphan_paths = ?orphan_paths,
                    "detected orphan chunk paths"
                );
            }
        }

        Ok(())
    }

    async fn collect_known_chunk_paths(
        consolidator: &Arc<HippocampusConsolidator>,
    ) -> Result<Vec<String>> {
        let mut paths: HashSet<String> =
            HashSet::from(["MEMORY.md".to_string(), "MEMORY_ARCHIVED.md".to_string()]);

        for (date, _) in consolidator.file_store.list_daily_files().await? {
            paths.insert(format!("memory/{}.md", date.format("%Y-%m-%d")));
        }

        for (date, _) in consolidator.file_store.list_archived_files().await? {
            paths.insert(format!("memory/archive/{}.md", date.format("%Y-%m-%d")));
        }

        if let Some(reader) = &consolidator.reindex_session_reader {
            for session_id in reader.list_sessions().await? {
                paths.insert(format!("sessions/{session_id}"));
            }
        }

        if let Some(store) = &consolidator.memory_store {
            for path in Self::list_indexed_session_paths(store, consolidator.agent_id()).await? {
                paths.insert(path);
            }
        }

        let mut known_paths = paths.into_iter().collect::<Vec<_>>();
        known_paths.sort();
        Ok(known_paths)
    }

    async fn list_indexed_session_paths(
        store: &Arc<MemoryStore>,
        agent_id: &str,
    ) -> Result<Vec<String>> {
        let db = store.db();
        let agent_id = agent_id.to_string();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let mut stmt = conn.prepare(
                "SELECT DISTINCT path FROM files WHERE agent_id = ?1 AND source = 'session'",
            )?;
            let paths = stmt
                .query_map([&agent_id], |row| row.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<String>, _>>()?;
            Ok::<Vec<String>, anyhow::Error>(paths)
        })
        .await?
    }

    async fn query_archived_chunk_access_count(
        memory_store: &Arc<MemoryStore>,
        agent_id: &str,
        archived_path: &str,
    ) -> Result<i64> {
        let db = memory_store.db();
        let agent_id = agent_id.to_string();
        let archived_path = archived_path.to_string();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let total_access = conn.query_row(
                "SELECT COALESCE(SUM(access_count), 0) FROM chunks WHERE path = ?1 AND agent_id = ?2",
                [&archived_path, &agent_id],
                |row| row.get::<_, i64>(0),
            )?;
            Ok::<i64, anyhow::Error>(total_access)
        })
        .await?
    }

    fn should_run_weekly_decay(last_decay_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
        if now.weekday() != chrono::Weekday::Sun {
            return false;
        }

        match last_decay_at {
            Some(last) => now.signed_duration_since(last) >= Duration::days(6),
            None => true,
        }
    }

    async fn run_weekly_confidence_decay_if_due(
        consolidator: &Arc<HippocampusConsolidator>,
        now: DateTime<Utc>,
    ) -> Result<Option<clawhive_memory::fact_store::ConfidenceDecaySummary>> {
        let Some(memory_store) = &consolidator.memory_store else {
            return Ok(None);
        };

        let last_decay_at = Self::read_last_confidence_decay(memory_store).await?;
        if !Self::should_run_weekly_decay(last_decay_at, now) {
            return Ok(None);
        }

        let summary = FactStore::new(memory_store.db())
            .apply_confidence_decay(consolidator.agent_id())
            .await?;
        Self::write_last_confidence_decay(memory_store, now).await?;

        tracing::info!(
            agent_id = %consolidator.agent_id(),
            decayed_count = summary.decayed_count,
            archived_count = summary.archived_count,
            "Weekly confidence decay completed"
        );

        Ok(Some(summary))
    }

    async fn read_last_confidence_decay(store: &Arc<MemoryStore>) -> Result<Option<DateTime<Utc>>> {
        let db = store.db();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let mut stmt =
                conn.prepare("SELECT value FROM meta WHERE key = 'last_confidence_decay'")?;
            let mut rows = stmt.query([])?;
            if let Some(row) = rows.next()? {
                let raw: String = row.get(0)?;
                match DateTime::parse_from_rfc3339(&raw) {
                    Ok(parsed) => Ok(Some(parsed.with_timezone(&Utc))),
                    Err(error) => {
                        tracing::warn!(
                            value = %raw,
                            %error,
                            "Invalid last_confidence_decay marker, ignoring"
                        );
                        Ok(None)
                    }
                }
            } else {
                Ok(None)
            }
        })
        .await?
    }

    async fn write_last_confidence_decay(
        store: &Arc<MemoryStore>,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let db = store.db();
        let marker = now.to_rfc3339();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            conn.execute(
                "INSERT INTO meta(key, value) VALUES('last_confidence_decay', ?1) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [&marker],
            )?;
            Ok::<(), anyhow::Error>(())
        })
        .await??;
        Ok(())
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

pub(crate) fn normalized_word_set(text: &str) -> std::collections::HashSet<String> {
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

pub(crate) fn jaccard_similarity(
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

fn fact_conflict_step_b_passes(new_fact: &Fact, existing: &Fact) -> bool {
    if new_fact.fact_type != existing.fact_type {
        return false;
    }

    let new_tokens = normalized_word_set(&new_fact.content);
    let existing_tokens = normalized_word_set(&existing.content);
    jaccard_similarity(&new_tokens, &existing_tokens) > 0.6
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use anyhow::{anyhow, Result};
    use async_trait::async_trait;
    use chrono::Utc;
    use clawhive_memory::embedding::EmbeddingProvider;
    use clawhive_memory::fact_store::{generate_fact_id, Fact, FactStore};
    use clawhive_memory::file_store::MemoryFileStore;
    use clawhive_memory::memory_lineage::MemoryLineageStore;
    use clawhive_memory::session::SessionReader;
    use clawhive_memory::store::MemoryStore;
    use clawhive_memory::{
        EpisodeStateRecord, EpisodeStatusRecord, EpisodeTaskStateRecord, FlushPhase,
        SessionMemoryStateRecord,
    };
    use clawhive_provider::{LlmProvider, LlmRequest, LlmResponse, ProviderRegistry, StubProvider};
    use tempfile::TempDir;
    use tokio::fs;

    use super::{
        apply_patch, compute_line_diff, dedup_paragraphs, jaccard_similarity, parse_patch,
        validate_consolidation_output, AddInstruction, ConsolidationReport, ConsolidationScheduler,
        HippocampusConsolidator, MemoryPatch, StaleSectionCandidate, UpdateInstruction,
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

    fn insert_chunk_access_count(
        memory_store: &Arc<MemoryStore>,
        agent_id: &str,
        path: &str,
        chunk_id: &str,
        access_count: i64,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        let hash = format!("hash-{chunk_id}");
        let sql = format!(
            "INSERT INTO chunks (id, path, source, start_line, end_line, hash, model, text, embedding, updated_at, created_at, access_count, agent_id, last_accessed) \
             VALUES ('{chunk_id}', '{path}', 'daily', 1, 1, '{hash}', '', 'chunk', '', '{now}', '{now}', {access_count}, '{agent_id}', NULL)"
        );
        let db = memory_store.db();
        let conn = db.lock().expect("lock db");
        conn.execute(&sql, [])?;
        Ok(())
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
    fn parse_patch_retry_on_malformed_first_attempt() {
        let malformed = r#"[UPDATE]
[OLD]Likes tea[/OLD]
[NEW]Likes green tea[/NEW]"#;
        assert!(parse_patch(malformed).is_err());

        let well_formed = r#"[UPDATE]
[OLD]Likes tea[/OLD]
[NEW]Likes green tea[/NEW]
[/UPDATE]"#;
        assert!(parse_patch(well_formed).is_ok());
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
    async fn stale_scan_triggers_dead_flush_session_and_closes_open_episodes() -> Result<()> {
        let (_dir, file_store) = build_file_store()?;
        let memory_store = Arc::new(MemoryStore::open_in_memory()?);
        memory_store
            .upsert_session_memory_state(SessionMemoryStateRecord {
                agent_id: "agent-1".to_string(),
                session_id: "session-dead".to_string(),
                session_key: "chat-dead".to_string(),
                last_flushed_turn: 0,
                last_boundary_flush_at: None,
                pending_flush: false,
                flush_phase: FlushPhase::Summarized.as_str().to_string(),
                flush_phase_updated_at: Some(
                    (Utc::now() - chrono::Duration::minutes(20)).to_rfc3339(),
                ),
                flush_summary_cache: None,
                recent_explicit_writes: Vec::new(),
                open_episodes: vec![EpisodeStateRecord {
                    episode_id: "session-dead:1".to_string(),
                    start_turn: 1,
                    end_turn: 1,
                    status: EpisodeStatusRecord::Open,
                    task_state: EpisodeTaskStateRecord::Exploring,
                    topic_sketch: "topic".to_string(),
                    last_activity_at: Utc::now() - chrono::Duration::minutes(60),
                }],
            })
            .await?;

        let consolidator = Arc::new(
            HippocampusConsolidator::new(
                "agent-1".to_string(),
                file_store,
                build_router(),
                "sonnet".to_string(),
                vec![],
            )
            .with_memory_store(Arc::clone(&memory_store))
            .with_session_idle_minutes(30),
        );

        ConsolidationScheduler::scan_stale_sessions_and_trigger_boundary_flush(&consolidator)
            .await?;

        let state = memory_store
            .get_session_memory_state("agent-1", "session-dead")
            .await?
            .expect("state exists");
        assert!(state.pending_flush);
        assert_eq!(state.flush_phase, FlushPhase::Captured.as_str());
        assert!(state
            .open_episodes
            .iter()
            .all(|episode| episode.status != EpisodeStatusRecord::Open));
        Ok(())
    }

    #[tokio::test]
    async fn daily_file_lifecycle_archives_daily_file_older_than_retention_window() -> Result<()> {
        let (_dir, file_store) = build_file_store()?;
        let memory_store = Arc::new(MemoryStore::open_in_memory()?);
        let now = Utc::now();
        let old_date = (now - chrono::Duration::days(35)).date_naive();
        let new_date = (now - chrono::Duration::days(10)).date_naive();

        file_store.write_daily(old_date, "old").await?;
        file_store.write_daily(new_date, "new").await?;

        let consolidator = Arc::new(
            HippocampusConsolidator::new(
                "agent-1".to_string(),
                file_store.clone(),
                build_router(),
                "sonnet".to_string(),
                vec![],
            )
            .with_memory_store(memory_store),
        );

        ConsolidationScheduler::run_daily_file_lifecycle(&consolidator, 30, now).await?;

        assert!(file_store.read_daily(old_date).await?.is_none());
        assert!(fs::metadata(
            file_store
                .workspace_dir()
                .join(format!("memory/archive/{}.md", old_date.format("%Y-%m-%d")))
        )
        .await
        .is_ok());
        assert!(file_store.read_daily(new_date).await?.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn daily_file_lifecycle_deletes_old_archived_file_when_chunks_are_unaccessed(
    ) -> Result<()> {
        let (_dir, file_store) = build_file_store()?;
        let memory_store = Arc::new(MemoryStore::open_in_memory()?);
        let now = Utc::now();
        let archived_date = (now - chrono::Duration::days(95)).date_naive();
        let archived_rel_path = format!("memory/archive/{}.md", archived_date.format("%Y-%m-%d"));

        file_store.write_daily(archived_date, "old").await?;
        file_store.archive_daily(archived_date).await?;
        insert_chunk_access_count(&memory_store, "agent-1", &archived_rel_path, "chunk-a", 0)?;
        insert_chunk_access_count(&memory_store, "agent-1", &archived_rel_path, "chunk-b", 0)?;

        let consolidator = Arc::new(
            HippocampusConsolidator::new(
                "agent-1".to_string(),
                file_store.clone(),
                build_router(),
                "sonnet".to_string(),
                vec![],
            )
            .with_memory_store(memory_store),
        );

        ConsolidationScheduler::run_daily_file_lifecycle(&consolidator, 30, now).await?;

        assert!(
            fs::metadata(file_store.workspace_dir().join(&archived_rel_path))
                .await
                .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn daily_file_lifecycle_retains_old_archived_file_when_any_chunk_accessed() -> Result<()>
    {
        let (_dir, file_store) = build_file_store()?;
        let memory_store = Arc::new(MemoryStore::open_in_memory()?);
        let now = Utc::now();
        let archived_date = (now - chrono::Duration::days(95)).date_naive();
        let archived_rel_path = format!("memory/archive/{}.md", archived_date.format("%Y-%m-%d"));

        file_store.write_daily(archived_date, "old").await?;
        file_store.archive_daily(archived_date).await?;
        insert_chunk_access_count(&memory_store, "agent-1", &archived_rel_path, "chunk-c", 2)?;

        let consolidator = Arc::new(
            HippocampusConsolidator::new(
                "agent-1".to_string(),
                file_store.clone(),
                build_router(),
                "sonnet".to_string(),
                vec![],
            )
            .with_memory_store(memory_store),
        );

        ConsolidationScheduler::run_daily_file_lifecycle(&consolidator, 30, now).await?;

        assert!(
            fs::metadata(file_store.workspace_dir().join(&archived_rel_path))
                .await
                .is_ok()
        );
        Ok(())
    }

    #[tokio::test]
    async fn daily_file_lifecycle_updates_chunk_paths_after_archive() -> Result<()> {
        use clawhive_memory::search_index::SearchIndex;

        let (_dir, file_store) = build_file_store()?;
        let memory_store = Arc::new(MemoryStore::open_in_memory()?);
        let search_index = SearchIndex::new(memory_store.db(), "agent-1");
        let provider = StubEmbeddingProvider;
        let now = Utc::now();
        let old_date = (now - chrono::Duration::days(35)).date_naive();
        let old_path = format!("memory/{}.md", old_date.format("%Y-%m-%d"));
        let archived_path = format!("memory/archive/{}.md", old_date.format("%Y-%m-%d"));

        file_store.write_daily(old_date, "old").await?;
        search_index
            .index_file(&old_path, "# Daily\n\nold", "daily", &provider)
            .await?;

        let consolidator = Arc::new(
            HippocampusConsolidator::new(
                "agent-1".to_string(),
                file_store,
                build_router(),
                "sonnet".to_string(),
                vec![],
            )
            .with_memory_store(memory_store)
            .with_search_index(search_index),
        );

        ConsolidationScheduler::run_daily_file_lifecycle(&consolidator, 30, now).await?;

        let db = consolidator
            .memory_store
            .as_ref()
            .expect("memory store")
            .db();
        let conn = db.lock().expect("lock");
        let old_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM chunks WHERE path = ?1 AND agent_id = 'agent-1'",
            [&old_path],
            |row| row.get(0),
        )?;
        let archived_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM chunks WHERE path = ?1 AND agent_id = 'agent-1'",
            [&archived_path],
            |row| row.get(0),
        )?;

        assert_eq!(old_count, 0);
        assert!(archived_count > 0);
        Ok(())
    }

    #[tokio::test]
    async fn daily_file_lifecycle_deletes_indexed_chunks_when_archived_file_is_deleted(
    ) -> Result<()> {
        use clawhive_memory::search_index::SearchIndex;

        let (_dir, file_store) = build_file_store()?;
        let memory_store = Arc::new(MemoryStore::open_in_memory()?);
        let search_index = SearchIndex::new(memory_store.db(), "agent-1");
        let provider = StubEmbeddingProvider;
        let now = Utc::now();
        let archived_date = (now - chrono::Duration::days(95)).date_naive();
        let archived_path = format!("memory/archive/{}.md", archived_date.format("%Y-%m-%d"));

        file_store.write_daily(archived_date, "old").await?;
        file_store.archive_daily(archived_date).await?;
        search_index
            .index_file(&archived_path, "# Archive\n\nold", "daily", &provider)
            .await?;

        let consolidator = Arc::new(
            HippocampusConsolidator::new(
                "agent-1".to_string(),
                file_store.clone(),
                build_router(),
                "sonnet".to_string(),
                vec![],
            )
            .with_memory_store(memory_store)
            .with_search_index(search_index),
        );

        ConsolidationScheduler::run_daily_file_lifecycle(&consolidator, 30, now).await?;

        assert!(
            fs::metadata(file_store.workspace_dir().join(&archived_path))
                .await
                .is_err()
        );

        let db = consolidator
            .memory_store
            .as_ref()
            .expect("memory store")
            .db();
        let conn = db.lock().expect("lock");
        let chunk_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM chunks WHERE path = ?1 AND agent_id = 'agent-1'",
            [&archived_path],
            |row| row.get(0),
        )?;
        assert_eq!(chunk_count, 0);

        Ok(())
    }

    #[tokio::test]
    async fn collect_known_chunk_paths_includes_memory_daily_archive_and_session_paths(
    ) -> Result<()> {
        let (dir, file_store) = build_file_store()?;
        let memory_store = Arc::new(MemoryStore::open_in_memory()?);
        let session_writer = clawhive_memory::session::SessionWriter::new(dir.path());
        let session_reader = SessionReader::new(dir.path());
        let now = Utc::now();
        let daily_date = (now - chrono::Duration::days(1)).date_naive();
        let archived_date = (now - chrono::Duration::days(40)).date_naive();

        file_store.write_daily(daily_date, "recent").await?;
        file_store.write_daily(archived_date, "old").await?;
        file_store.archive_daily(archived_date).await?;
        session_writer.start_session("session-1", "agent-1").await?;

        let consolidator = Arc::new(
            HippocampusConsolidator::new(
                "agent-1".to_string(),
                file_store,
                build_router(),
                "sonnet".to_string(),
                vec![],
            )
            .with_memory_store(memory_store)
            .with_session_reader_for_reindex(session_reader),
        );

        let known_paths = ConsolidationScheduler::collect_known_chunk_paths(&consolidator).await?;

        assert!(known_paths.contains(&"MEMORY.md".to_string()));
        assert!(known_paths.contains(&"MEMORY_ARCHIVED.md".to_string()));
        assert!(known_paths.contains(&format!("memory/{}.md", daily_date.format("%Y-%m-%d"))));
        assert!(known_paths.contains(&format!(
            "memory/archive/{}.md",
            archived_date.format("%Y-%m-%d")
        )));
        assert!(known_paths.contains(&"sessions/session-1".to_string()));

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
    async fn consolidation_reconciles_recent_conflicting_facts_without_creating_new_ones(
    ) -> Result<()> {
        use chrono::{Local, Utc};

        let (_dir, file_store) = build_file_store()?;
        file_store.write_long_term("# Memory\n\nExisting").await?;

        let today = Local::now().date_naive();
        file_store
            .write_daily(today, "## Observations\n\nUser moved to Tokyo.")
            .await?;

        let memory_store = Arc::new(MemoryStore::open_in_memory()?);
        let fact_store = FactStore::new(memory_store.db());
        let old_created = (Utc::now() - chrono::Duration::hours(48)).to_rfc3339();
        let recent_created = Utc::now().to_rfc3339();
        let old_fact = Fact {
            id: generate_fact_id("agent-1", "User lives in Tokyo city"),
            agent_id: "agent-1".to_string(),
            content: "User lives in Tokyo city".to_string(),
            fact_type: "event".to_string(),
            importance: 0.6,
            confidence: 1.0,
            status: "active".to_string(),
            occurred_at: None,
            recorded_at: old_created.clone(),
            source_type: "consolidation".to_string(),
            source_session: None,
            access_count: 0,
            last_accessed: None,
            superseded_by: None,
            salience: 50,
            supersede_reason: None,
            affect: "neutral".to_string(),
            affect_intensity: 0.0,
            created_at: old_created.clone(),
            updated_at: old_created,
        };
        fact_store.insert_fact(&old_fact).await?;
        fact_store.record_add(&old_fact).await?;

        let recent_fact = Fact {
            id: generate_fact_id("agent-1", "User lives in Tokyo city center"),
            agent_id: "agent-1".to_string(),
            content: "User lives in Tokyo city center".to_string(),
            fact_type: "event".to_string(),
            importance: 0.9,
            confidence: 1.0,
            status: "active".to_string(),
            occurred_at: None,
            recorded_at: recent_created.clone(),
            source_type: "boundary_flush".to_string(),
            source_session: Some("session-1".to_string()),
            access_count: 0,
            last_accessed: None,
            superseded_by: None,
            salience: 50,
            supersede_reason: None,
            affect: "neutral".to_string(),
            affect_intensity: 0.0,
            created_at: recent_created.clone(),
            updated_at: recent_created,
        };
        fact_store.insert_fact(&recent_fact).await?;
        fact_store.record_add(&recent_fact).await?;

        let router = build_router_with_provider(SequenceProvider::new(vec![
            "[]".to_string(),
            "yes".to_string(),
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

        assert_eq!(report.facts_extracted, 0);
        let facts = fact_store.get_active_facts("agent-1").await?;
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].id, recent_fact.id);

        let history = fact_store.get_history(&old_fact.id).await?;
        assert_eq!(history[0].event, "SUPERSEDE");
        assert_eq!(
            history[0].new_content.as_deref(),
            Some(recent_fact.content.as_str())
        );
        Ok(())
    }

    #[tokio::test]
    async fn consolidation_reconcile_falls_back_to_step_b_when_embedding_unavailable() -> Result<()>
    {
        use chrono::{Local, Utc};

        let (_dir, file_store) = build_file_store()?;
        file_store.write_long_term("# Memory\n\nExisting").await?;
        let today = Local::now().date_naive();
        file_store
            .write_daily(today, "## Observations\n\nUser preference updated.")
            .await?;

        let memory_store = Arc::new(MemoryStore::open_in_memory()?);
        let fact_store = FactStore::new(memory_store.db());
        let old_created = (Utc::now() - chrono::Duration::hours(48)).to_rfc3339();
        let recent_created = Utc::now().to_rfc3339();

        let old_fact = Fact {
            id: generate_fact_id("agent-1", "User prefers Rust for backend services"),
            agent_id: "agent-1".to_string(),
            content: "User prefers Rust for backend services".to_string(),
            fact_type: "preference".to_string(),
            importance: 0.6,
            confidence: 1.0,
            status: "active".to_string(),
            occurred_at: None,
            recorded_at: old_created.clone(),
            source_type: "boundary_flush".to_string(),
            source_session: Some("session-old".to_string()),
            access_count: 0,
            last_accessed: None,
            superseded_by: None,
            salience: 50,
            supersede_reason: None,
            affect: "neutral".to_string(),
            affect_intensity: 0.0,
            created_at: old_created.clone(),
            updated_at: old_created,
        };
        fact_store.insert_fact(&old_fact).await?;
        fact_store.record_add(&old_fact).await?;

        let recent_fact = Fact {
            id: generate_fact_id("agent-1", "User prefers Rust for backend systems"),
            agent_id: "agent-1".to_string(),
            content: "User prefers Rust for backend systems".to_string(),
            fact_type: "preference".to_string(),
            importance: 0.9,
            confidence: 1.0,
            status: "active".to_string(),
            occurred_at: None,
            recorded_at: recent_created.clone(),
            source_type: "boundary_flush".to_string(),
            source_session: Some("session-recent".to_string()),
            access_count: 0,
            last_accessed: None,
            superseded_by: None,
            salience: 50,
            supersede_reason: None,
            affect: "neutral".to_string(),
            affect_intensity: 0.0,
            created_at: recent_created.clone(),
            updated_at: recent_created,
        };
        fact_store.insert_fact(&recent_fact).await?;
        fact_store.record_add(&recent_fact).await?;

        let router = build_router_with_provider(SequenceProvider::new(vec![
            "[]".to_string(),
            "yes".to_string(),
        ]));
        let consolidator = HippocampusConsolidator::new(
            "agent-1".to_string(),
            file_store,
            router,
            "sonnet".to_string(),
            vec![],
        )
        .with_memory_store(Arc::clone(&memory_store));

        let report = consolidator.consolidate().await?;
        assert_eq!(report.facts_extracted, 0);
        let facts = fact_store.get_active_facts("agent-1").await?;
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].id, recent_fact.id);
        Ok(())
    }

    #[tokio::test]
    async fn consolidation_reconcile_skips_supersede_when_llm_rejects_conflict() -> Result<()> {
        use chrono::{Local, Utc};

        let (_dir, file_store) = build_file_store()?;
        file_store.write_long_term("# Memory\n\nExisting").await?;
        let today = Local::now().date_naive();
        file_store
            .write_daily(today, "## Observations\n\nPreference update candidate.")
            .await?;

        let memory_store = Arc::new(MemoryStore::open_in_memory()?);
        let fact_store = FactStore::new(memory_store.db());
        let old_created = (Utc::now() - chrono::Duration::hours(48)).to_rfc3339();
        let recent_created = Utc::now().to_rfc3339();

        let old_fact = Fact {
            id: generate_fact_id("agent-1", "用户喜欢 TypeScript 项目"),
            agent_id: "agent-1".to_string(),
            content: "用户喜欢 TypeScript 项目".to_string(),
            fact_type: "preference".to_string(),
            importance: 0.6,
            confidence: 1.0,
            status: "active".to_string(),
            occurred_at: None,
            recorded_at: old_created.clone(),
            source_type: "boundary_flush".to_string(),
            source_session: Some("session-old".to_string()),
            access_count: 0,
            last_accessed: None,
            superseded_by: None,
            salience: 50,
            supersede_reason: None,
            affect: "neutral".to_string(),
            affect_intensity: 0.0,
            created_at: old_created.clone(),
            updated_at: old_created,
        };
        fact_store.insert_fact(&old_fact).await?;
        fact_store.record_add(&old_fact).await?;

        let recent_fact = Fact {
            id: generate_fact_id("agent-1", "用户喜欢 JavaScript 项目"),
            agent_id: "agent-1".to_string(),
            content: "用户喜欢 JavaScript 项目".to_string(),
            fact_type: "preference".to_string(),
            importance: 0.9,
            confidence: 1.0,
            status: "active".to_string(),
            occurred_at: None,
            recorded_at: recent_created.clone(),
            source_type: "boundary_flush".to_string(),
            source_session: Some("session-recent".to_string()),
            access_count: 0,
            last_accessed: None,
            superseded_by: None,
            salience: 50,
            supersede_reason: None,
            affect: "neutral".to_string(),
            affect_intensity: 0.0,
            created_at: recent_created.clone(),
            updated_at: recent_created,
        };
        fact_store.insert_fact(&recent_fact).await?;
        fact_store.record_add(&recent_fact).await?;

        let router = build_router_with_provider(SequenceProvider::new(vec![
            "[]".to_string(),
            "no".to_string(),
        ]));
        let consolidator = HippocampusConsolidator::new(
            "agent-1".to_string(),
            file_store,
            router,
            "sonnet".to_string(),
            vec![],
        )
        .with_memory_store(Arc::clone(&memory_store));

        let report = consolidator.consolidate().await?;
        assert_eq!(report.facts_extracted, 0);

        let active = fact_store.get_active_facts("agent-1").await?;
        assert_eq!(active.len(), 2);

        let history = fact_store.get_history(&old_fact.id).await?;
        assert!(history.iter().all(|entry| entry.event != "SUPERSEDE"));
        Ok(())
    }

    #[tokio::test]
    async fn consolidation_reconcile_skips_supersede_when_llm_confirmation_errors() -> Result<()> {
        use chrono::{Local, Utc};

        let (_dir, file_store) = build_file_store()?;
        file_store.write_long_term("# Memory\n\nExisting").await?;
        let today = Local::now().date_naive();
        file_store
            .write_daily(today, "## Observations\n\nPreference update candidate.")
            .await?;

        let memory_store = Arc::new(MemoryStore::open_in_memory()?);
        let fact_store = FactStore::new(memory_store.db());
        let old_created = (Utc::now() - chrono::Duration::hours(48)).to_rfc3339();
        let recent_created = Utc::now().to_rfc3339();

        let old_fact = Fact {
            id: generate_fact_id("agent-1", "用户喜欢 TypeScript 项目"),
            agent_id: "agent-1".to_string(),
            content: "用户喜欢 TypeScript 项目".to_string(),
            fact_type: "preference".to_string(),
            importance: 0.6,
            confidence: 1.0,
            status: "active".to_string(),
            occurred_at: None,
            recorded_at: old_created.clone(),
            source_type: "boundary_flush".to_string(),
            source_session: Some("session-old".to_string()),
            access_count: 0,
            last_accessed: None,
            superseded_by: None,
            salience: 50,
            supersede_reason: None,
            affect: "neutral".to_string(),
            affect_intensity: 0.0,
            created_at: old_created.clone(),
            updated_at: old_created,
        };
        fact_store.insert_fact(&old_fact).await?;
        fact_store.record_add(&old_fact).await?;

        let recent_fact = Fact {
            id: generate_fact_id("agent-1", "用户喜欢 JavaScript 项目"),
            agent_id: "agent-1".to_string(),
            content: "用户喜欢 JavaScript 项目".to_string(),
            fact_type: "preference".to_string(),
            importance: 0.9,
            confidence: 1.0,
            status: "active".to_string(),
            occurred_at: None,
            recorded_at: recent_created.clone(),
            source_type: "boundary_flush".to_string(),
            source_session: Some("session-recent".to_string()),
            access_count: 0,
            last_accessed: None,
            superseded_by: None,
            salience: 50,
            supersede_reason: None,
            affect: "neutral".to_string(),
            affect_intensity: 0.0,
            created_at: recent_created.clone(),
            updated_at: recent_created,
        };
        fact_store.insert_fact(&recent_fact).await?;
        fact_store.record_add(&recent_fact).await?;

        let router = build_router_with_provider(FailAtCallProvider::new(vec!["[]".to_string()], 1));
        let consolidator = HippocampusConsolidator::new(
            "agent-1".to_string(),
            file_store,
            router,
            "sonnet".to_string(),
            vec![],
        )
        .with_memory_store(Arc::clone(&memory_store));

        let report = consolidator.consolidate().await?;
        assert_eq!(report.facts_extracted, 0);

        let active = fact_store.get_active_facts("agent-1").await?;
        assert_eq!(active.len(), 2);

        let history = fact_store.get_history(&old_fact.id).await?;
        assert!(history.iter().all(|entry| entry.event != "SUPERSEDE"));
        Ok(())
    }

    #[tokio::test]
    async fn confirm_fact_conflict_with_llm_parses_yes_and_no() -> Result<()> {
        let (_dir, file_store) = build_file_store()?;
        let recent = Fact {
            id: "recent-fact".to_string(),
            agent_id: "agent-1".to_string(),
            content: "改用 Rust 了".to_string(),
            fact_type: "preference".to_string(),
            importance: 0.9,
            confidence: 1.0,
            status: "active".to_string(),
            occurred_at: None,
            recorded_at: Utc::now().to_rfc3339(),
            source_type: "boundary_flush".to_string(),
            source_session: Some("s1".to_string()),
            access_count: 0,
            last_accessed: None,
            superseded_by: None,
            salience: 50,
            supersede_reason: None,
            affect: "neutral".to_string(),
            affect_intensity: 0.0,
            created_at: Utc::now().to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
        };
        let candidate = Fact {
            id: "old-fact".to_string(),
            agent_id: "agent-1".to_string(),
            content: "喜欢 TypeScript".to_string(),
            fact_type: "preference".to_string(),
            importance: 0.6,
            confidence: 1.0,
            status: "active".to_string(),
            occurred_at: None,
            recorded_at: Utc::now().to_rfc3339(),
            source_type: "consolidation".to_string(),
            source_session: None,
            access_count: 0,
            last_accessed: None,
            superseded_by: None,
            salience: 50,
            supersede_reason: None,
            affect: "neutral".to_string(),
            affect_intensity: 0.0,
            created_at: Utc::now().to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
        };

        let yes_router = build_router_with_provider(SequenceProvider::new(vec!["yes".to_string()]));
        let yes_consolidator = HippocampusConsolidator::new(
            "agent-1".to_string(),
            file_store.clone(),
            yes_router,
            "sonnet".to_string(),
            vec![],
        );
        assert!(
            yes_consolidator
                .confirm_fact_conflict_with_llm(&recent, &candidate)
                .await?
        );

        let no_router = build_router_with_provider(SequenceProvider::new(vec!["no".to_string()]));
        let no_consolidator = HippocampusConsolidator::new(
            "agent-1".to_string(),
            file_store,
            no_router,
            "sonnet".to_string(),
            vec![],
        );
        assert!(
            !no_consolidator
                .confirm_fact_conflict_with_llm(&recent, &candidate)
                .await?
        );

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
    async fn section_based_consolidation_aligns_existing_fact_to_memory_canonical() -> Result<()> {
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
        ]));

        let memory_store = Arc::new(MemoryStore::open_in_memory()?);
        let fact_store = FactStore::new(memory_store.db());
        let now = Utc::now().to_rfc3339();
        let fact = Fact {
            id: generate_fact_id("agent-1", "Use section-based consolidation for memory"),
            agent_id: "agent-1".to_string(),
            content: "Use section-based consolidation for memory".to_string(),
            fact_type: "decision".to_string(),
            importance: 0.9,
            confidence: 1.0,
            status: "active".to_string(),
            occurred_at: None,
            recorded_at: now.clone(),
            source_type: "boundary_flush".to_string(),
            source_session: Some("session-1".to_string()),
            access_count: 0,
            last_accessed: None,
            superseded_by: None,
            salience: 50,
            supersede_reason: None,
            affect: "neutral".to_string(),
            affect_intensity: 0.0,
            created_at: now.clone(),
            updated_at: now,
        };
        fact_store.insert_fact(&fact).await?;
        fact_store.record_add(&fact).await?;

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
        assert_eq!(report.facts_extracted, 0);

        let lineage_store = MemoryLineageStore::new(memory_store.db());
        let links = lineage_store
            .get_links_for_source(
                "agent-1",
                "fact",
                &generate_fact_id("agent-1", "Use section-based consolidation for memory"),
            )
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
    async fn section_based_consolidation_does_not_extract_facts_when_memory_is_unchanged(
    ) -> Result<()> {
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
        .with_memory_store(Arc::clone(&memory_store));

        let report = consolidator.consolidate().await?;

        assert!(!report.memory_updated);
        assert_eq!(report.facts_extracted, 0);
        let fact_store = FactStore::new(memory_store.db());
        assert!(fact_store.get_active_facts("agent-1").await?.is_empty());
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
        assert_eq!(report.facts_extracted, 0);

        let after_memory = file_store.read_long_term().await?;
        let after_facts = fact_store.get_active_facts("agent-1").await?;
        assert!(after_memory.contains("Use section-based consolidation for memory"));
        assert!(after_facts.is_empty());
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
    async fn evaluate_memory_staleness_marks_only_sections_above_threshold() -> Result<()> {
        use clawhive_memory::search_index::SearchIndex;

        let (_dir, file_store) = build_file_store()?;
        file_store
            .write_long_term(
                "# MEMORY.md\n\n## 长期项目主线\n\n- stale-unit12-mainline\n\n## 持续性背景脉络\n\n- fresh-unit12-context\n\n## 关键历史决策\n\n- untouched-unit12-decision\n",
            )
            .await?;

        let memory_store = Arc::new(MemoryStore::open_in_memory()?);
        let search_index = SearchIndex::new(memory_store.db(), "agent-1");
        search_index
            .index_file(
                "MEMORY.md",
                &file_store.read_long_term().await?,
                "long_term",
                &StubEmbeddingProvider,
            )
            .await?;

        {
            let db = memory_store.db();
            let conn = db.lock().expect("lock");
            let stale_ts = (Utc::now() - chrono::Duration::days(95)).to_rfc3339();
            let fresh_ts = (Utc::now() - chrono::Duration::days(10)).to_rfc3339();
            conn.execute(
                "UPDATE chunks SET access_count = 2, last_accessed = ?1 WHERE agent_id = 'agent-1' AND path = 'MEMORY.md' AND text LIKE '%stale-unit12-mainline%'",
                [stale_ts],
            )?;
            conn.execute(
                "UPDATE chunks SET access_count = 5, last_accessed = ?1 WHERE agent_id = 'agent-1' AND path = 'MEMORY.md' AND text LIKE '%fresh-unit12-context%'",
                [fresh_ts],
            )?;
        }

        let consolidator = HippocampusConsolidator::new(
            "agent-1".to_string(),
            file_store,
            build_router(),
            "sonnet".to_string(),
            vec![],
        )
        .with_search_index(search_index)
        .with_memory_store(Arc::clone(&memory_store));

        let candidates = consolidator.evaluate_memory_staleness().await?;
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].section, "长期项目主线");
        assert!(candidates[0].staleness_score > 3.0);
        Ok(())
    }

    #[tokio::test]
    async fn prune_stale_sections_moves_section_to_archived_when_llm_confirms() -> Result<()> {
        let (_dir, file_store) = build_file_store()?;
        file_store
            .write_long_term(
                "# MEMORY.md\n\n## 长期项目主线\n\n- stale-candidate-unit12\n\n## 持续性背景脉络\n\n- keep-context\n\n## 关键历史决策\n\n- keep-decision\n",
            )
            .await?;

        let router = build_router_with_provider(SequenceProvider::new(vec!["STALE".to_string()]));
        let consolidator = HippocampusConsolidator::new(
            "agent-1".to_string(),
            file_store.clone(),
            router,
            "sonnet".to_string(),
            vec![],
        );

        let candidates = vec![StaleSectionCandidate {
            section: "长期项目主线".to_string(),
            content: "- stale-candidate-unit12".to_string(),
            staleness_score: 3.2,
            days_since_accessed: 96.0,
        }];

        let pruned = consolidator.prune_stale_sections(&candidates).await?;
        assert_eq!(pruned, 1);

        let memory = file_store.read_long_term().await?;
        let archived = file_store.read_archived_long_term().await?;
        assert!(!memory.contains("stale-candidate-unit12"));
        assert!(archived.contains("## 长期项目主线 (archived "));
        assert!(archived.contains("stale-candidate-unit12"));
        Ok(())
    }

    #[tokio::test]
    async fn prune_stale_sections_keeps_memory_when_llm_rejects_candidate() -> Result<()> {
        let (_dir, file_store) = build_file_store()?;
        file_store
            .write_long_term(
                "# MEMORY.md\n\n## 长期项目主线\n\n- keep-unit12-candidate\n\n## 持续性背景脉络\n\n- keep-context\n\n## 关键历史决策\n\n- keep-decision\n",
            )
            .await?;

        let router = build_router_with_provider(SequenceProvider::new(vec!["KEEP".to_string()]));
        let consolidator = HippocampusConsolidator::new(
            "agent-1".to_string(),
            file_store.clone(),
            router,
            "sonnet".to_string(),
            vec![],
        );

        let candidates = vec![StaleSectionCandidate {
            section: "长期项目主线".to_string(),
            content: "- keep-unit12-candidate".to_string(),
            staleness_score: 4.1,
            days_since_accessed: 123.0,
        }];

        let pruned = consolidator.prune_stale_sections(&candidates).await?;
        assert_eq!(pruned, 0);

        let memory = file_store.read_long_term().await?;
        let archived = file_store.read_archived_long_term().await?;
        assert!(memory.contains("keep-unit12-candidate"));
        assert!(!archived.contains("keep-unit12-candidate"));
        Ok(())
    }

    #[tokio::test]
    async fn weekly_confidence_decay_runs_on_sunday_and_records_meta_marker() -> Result<()> {
        use chrono::{Duration, TimeZone};

        let (_dir, file_store) = build_file_store()?;
        let memory_store = Arc::new(MemoryStore::open_in_memory()?);
        let fact_store = FactStore::new(memory_store.db());
        let fact = Fact {
            id: generate_fact_id("agent-1", "Weekly event"),
            agent_id: "agent-1".to_string(),
            content: "Weekly event".to_string(),
            fact_type: "event".to_string(),
            importance: 0.5,
            confidence: 1.0,
            salience: 40,
            status: "active".to_string(),
            occurred_at: None,
            recorded_at: Utc::now().to_rfc3339(),
            source_type: "consolidation".to_string(),
            source_session: None,
            access_count: 0,
            last_accessed: None,
            superseded_by: None,
            supersede_reason: None,
            affect: "neutral".to_string(),
            affect_intensity: 0.0,
            created_at: Utc::now().to_rfc3339(),
            updated_at: Utc::now().to_rfc3339(),
        };
        fact_store.insert_fact(&fact).await?;

        let consolidator = Arc::new(
            HippocampusConsolidator::new(
                "agent-1".to_string(),
                file_store,
                build_router(),
                "sonnet".to_string(),
                vec![],
            )
            .with_memory_store(Arc::clone(&memory_store)),
        );

        let sunday = Utc
            .with_ymd_and_hms(2026, 4, 5, 4, 0, 0)
            .single()
            .expect("valid sunday");
        let result =
            ConsolidationScheduler::run_weekly_confidence_decay_if_due(&consolidator, sunday)
                .await?;
        assert!(result.is_some());

        let loaded = fact_store
            .find_by_content("agent-1", "Weekly event")
            .await?
            .expect("fact exists");
        assert!((loaded.confidence - 0.93).abs() < 1e-9);

        {
            let conn = memory_store.db();
            let conn = conn.lock().expect("lock db");
            let marker: String = conn.query_row(
                "SELECT value FROM meta WHERE key = 'last_confidence_decay'",
                [],
                |row| row.get(0),
            )?;
            assert_eq!(marker, sunday.to_rfc3339());
        }

        let rerun = ConsolidationScheduler::run_weekly_confidence_decay_if_due(
            &consolidator,
            sunday + Duration::days(1),
        )
        .await?;
        assert!(rerun.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn weekly_confidence_decay_skips_when_not_sunday() -> Result<()> {
        use chrono::TimeZone;

        let (_dir, file_store) = build_file_store()?;
        let memory_store = Arc::new(MemoryStore::open_in_memory()?);
        let consolidator = Arc::new(
            HippocampusConsolidator::new(
                "agent-1".to_string(),
                file_store,
                build_router(),
                "sonnet".to_string(),
                vec![],
            )
            .with_memory_store(Arc::clone(&memory_store)),
        );

        let monday = Utc
            .with_ymd_and_hms(2026, 4, 6, 4, 0, 0)
            .single()
            .expect("valid monday");
        let result =
            ConsolidationScheduler::run_weekly_confidence_decay_if_due(&consolidator, monday)
                .await?;
        assert!(result.is_none());

        let conn = memory_store.db();
        let conn = conn.lock().expect("lock db");
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM meta WHERE key = 'last_confidence_decay'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(count, 0);
        Ok(())
    }

    #[test]
    fn decay_schedule_enforces_sunday_and_six_day_gap() {
        use chrono::{Duration, TimeZone};

        let sunday = Utc
            .with_ymd_and_hms(2026, 4, 5, 4, 0, 0)
            .single()
            .expect("valid sunday");
        let last = sunday - Duration::days(5);
        assert!(!ConsolidationScheduler::should_run_weekly_decay(
            Some(last),
            sunday
        ));

        let last_week = sunday - Duration::days(7);
        assert!(ConsolidationScheduler::should_run_weekly_decay(
            Some(last_week),
            sunday
        ));

        let monday = sunday + Duration::days(1);
        assert!(!ConsolidationScheduler::should_run_weekly_decay(
            None, monday
        ));
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

    struct FailAtCallProvider {
        responses: Vec<String>,
        fail_at: usize,
        call_count: AtomicUsize,
    }

    impl FailAtCallProvider {
        fn new(responses: Vec<String>, fail_at: usize) -> Arc<Self> {
            Arc::new(Self {
                responses,
                fail_at,
                call_count: AtomicUsize::new(0),
            })
        }
    }

    #[async_trait]
    impl LlmProvider for FailAtCallProvider {
        async fn chat(&self, _request: LlmRequest) -> Result<LlmResponse> {
            let index = self.call_count.fetch_add(1, Ordering::SeqCst);
            if index == self.fail_at {
                return Err(anyhow!("forced llm failure"));
            }
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
