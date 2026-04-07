use anyhow::Result;
use clawhive_memory::dirty_sources::{DirtySourceStore, DIRTY_KIND_MEMORY_FILE};
use clawhive_memory::fact_store::FactStore;
use clawhive_memory::memory_lineage::MemoryLineageStore;

use super::matching::{
    best_matching_candidate_for_item, best_matching_memory_item_for_fact, normalize_lineage_text,
    should_link_supersedes,
};
use super::text_utils::{dedup_paragraphs, validate_consolidation_output};
use super::{ConsolidationReport, HippocampusConsolidator, PromotionCandidate};
use crate::memory_document::{MemoryDocument, MEMORY_SECTION_ORDER};

impl HippocampusConsolidator {
    pub(super) async fn finalize_updated_memory(
        &self,
        updated_memory: String,
        current_memory: &str,
        daily_files_read: usize,
        memory_candidates: &[PromotionCandidate],
        latest_daily_date: Option<chrono::NaiveDate>,
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
            let mut trace_details = serde_json::json!({
                "daily_files_read": daily_files_read,
                "reindexed": reindexed,
                "facts_extracted": 0,
                "memory_chars": updated_memory.len(),
            });
            if let Some(date) = latest_daily_date {
                trace_details["latest_daily_date"] =
                    serde_json::Value::String(date.format("%Y-%m-%d").to_string());
            }
            store
                .record_trace(
                    &self.agent_id,
                    "consolidation",
                    &trace_details.to_string(),
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
}
