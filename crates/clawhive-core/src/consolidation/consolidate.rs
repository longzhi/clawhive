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
