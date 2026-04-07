use anyhow::Result;

use clawhive_provider::LlmMessage;

use super::matching::dedup_memory_candidates;
use super::patch::{apply_patch, parse_patch, strip_markdown_fence};
use super::prompts::{
    CONSOLIDATION_FULL_OVERWRITE_SYSTEM_PROMPT, CONSOLIDATION_INCREMENTAL_SYSTEM_PROMPT,
    PROMOTION_CANDIDATE_SYSTEM_PROMPT, SECTION_MERGE_SYSTEM_PROMPT,
};
use super::text_utils::compute_line_diff;
use super::{ConsolidationReport, HippocampusConsolidator, PromotionCandidate};
use crate::memory_document::{MemoryDocument, MEMORY_SECTION_ORDER};

impl HippocampusConsolidator {
    async fn last_consolidation_daily_date(&self) -> Option<chrono::NaiveDate> {
        if let Some(ref store) = self.memory_store {
            match store.last_consolidation_daily_date(&self.agent_id).await {
                Ok(date) => date,
                Err(error) => {
                    tracing::warn!(
                        agent_id = %self.agent_id,
                        error = %error,
                        "Failed to query last consolidation date"
                    );
                    None
                }
            }
        } else {
            None
        }
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

        // Incremental: only read daily files after last successful consolidation.
        // Falls back to lookback_days window when no prior consolidation record exists.
        let (recent_daily, incremental) = match self.last_consolidation_daily_date().await {
            Some(since) => {
                let files = self.file_store.read_daily_since(since).await?;
                if files.is_empty() {
                    tracing::info!(
                        agent_id = %self.agent_id,
                        since = %since,
                        "No new daily files since last consolidation; skipping"
                    );
                }
                (files, true)
            }
            None => {
                tracing::info!(
                    agent_id = %self.agent_id,
                    lookback_days = self.lookback_days,
                    "No prior consolidation record; using lookback window"
                );
                let files = self
                    .file_store
                    .read_recent_daily(self.lookback_days)
                    .await?;
                (files, false)
            }
        };
        let _ = incremental;
        // Newest daily file date — recorded in trace for next incremental run.
        let latest_daily_date = recent_daily.first().map(|(d, _)| *d);

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
            .consolidate_by_section(
                &current_memory,
                &daily_sections,
                recent_daily.len(),
                latest_daily_date,
            )
            .await
        {
            Ok(report) => report,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "Section-based consolidation failed, falling back to legacy consolidation"
                );
                self.legacy_consolidate(
                    &current_memory,
                    &daily_sections,
                    recent_daily.len(),
                    latest_daily_date,
                )
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

        // When memory wasn't updated, finalize_updated_memory was skipped so no
        // trace was recorded.  Write a lightweight trace here so the next run can
        // still pick up latest_daily_date for incremental mode.
        if !report.memory_updated && latest_daily_date.is_some() {
            if let Some(ref store) = self.memory_store {
                let mut details = serde_json::json!({
                    "daily_files_read": report.daily_files_read,
                    "reindexed": false,
                    "facts_extracted": 0,
                    "memory_unchanged": true,
                });
                if let Some(date) = latest_daily_date {
                    details["latest_daily_date"] =
                        serde_json::Value::String(date.format("%Y-%m-%d").to_string());
                }
                store
                    .record_trace(&self.agent_id, "consolidation", &details.to_string(), None)
                    .await;
            }
        }

        self.reconcile_recent_fact_conflicts().await;
        Ok(report)
    }

    async fn legacy_consolidate(
        &self,
        current_memory: &str,
        daily_sections: &str,
        daily_files_read: usize,
        latest_daily_date: Option<chrono::NaiveDate>,
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
                    .finalize_updated_memory(
                        updated_memory,
                        current_memory,
                        daily_files_read,
                        &[],
                        latest_daily_date,
                    )
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
                                latest_daily_date,
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

        self.finalize_updated_memory(
            updated_memory,
            current_memory,
            daily_files_read,
            &[],
            latest_daily_date,
        )
        .await
    }

    async fn consolidate_by_section(
        &self,
        current_memory: &str,
        daily_sections: &str,
        daily_files_read: usize,
        latest_daily_date: Option<chrono::NaiveDate>,
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
            latest_daily_date,
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

    pub(super) async fn request_consolidation(
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
}

// Re-export prompt builder functions used in this module
use super::patch::{
    build_full_overwrite_user_prompt, build_incremental_user_prompt,
    build_promotion_candidate_prompt, build_section_merge_prompt,
};

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use anyhow::Result;
    use chrono::{Local, Utc};
    use clawhive_memory::fact_store::{generate_fact_id, Fact, FactStore};
    use clawhive_memory::memory_lineage::MemoryLineageStore;
    use clawhive_memory::session::SessionReader;
    use clawhive_memory::store::MemoryStore;

    use crate::consolidation::test_helpers::*;
    use crate::consolidation::HippocampusConsolidator;

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
        use clawhive_memory::search_index::SearchIndex;

        let (dir, file_store) = build_file_store()?;
        let session_reader = SessionReader::new(dir.path());

        file_store
            .write_long_term("# Existing Memory\n\nSome knowledge.")
            .await?;

        let today = Local::now().date_naive();
        file_store
            .write_daily(today, "## Today's Observations\n\nLearned something new.")
            .await?;

        let memory_store = Arc::new(MemoryStore::open_in_memory()?);
        let search_index = SearchIndex::new(memory_store.db(), "agent-1");

        let embedding_provider = Arc::new(StubEmbeddingProvider);

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

        let report = consolidator.consolidate().await?;

        assert!(report.memory_updated);
        assert_eq!(report.daily_files_read, 1);
        assert!(report.reindexed);

        Ok(())
    }

    #[tokio::test]
    async fn consolidation_reconciles_recent_conflicting_facts_without_creating_new_ones(
    ) -> Result<()> {
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
    async fn section_based_consolidation_updates_only_target_section() -> Result<()> {
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
}
