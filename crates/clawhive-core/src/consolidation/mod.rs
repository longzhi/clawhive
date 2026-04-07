use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use chrono::{DateTime, Datelike, Duration, Utc};
use clawhive_memory::embedding::EmbeddingProvider;
use clawhive_memory::fact_store::FactStore;
use clawhive_memory::file_store::MemoryFileStore;
use clawhive_memory::memory_lineage::MemoryLineageStore;
use clawhive_memory::search_index::SearchIndex;
use clawhive_memory::session::SessionReader;
use clawhive_memory::{EpisodeStatusRecord, FlushPhase, MemoryStore, SessionMemoryStateRecord};
use clawhive_provider::LlmMessage;
use tokio::task;

use super::memory_document::{MemoryDocument, MEMORY_SECTION_ORDER};
use super::router::LlmRouter;

mod prompts;
use prompts::*;

mod text_utils;
pub(crate) use text_utils::jaccard_similarity;
pub(crate) use text_utils::normalized_word_set;
use text_utils::*;

mod patch;
pub use patch::apply_patch;
pub use patch::parse_patch;
use patch::*;

mod matching;
use matching::*;

mod fact_reconciliation;

mod staleness;

mod lineage;

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
    model_compaction: Option<String>,
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

#[derive(Debug, Default)]
pub struct GcReport {
    pub stale_sections_pruned: usize,
    pub orphans_detected: usize,
    pub orphans_cleaned: usize,
    pub health_report_generated: bool,
    pub errors: Vec<String>,
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
            model_compaction: None,
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

    pub fn with_model_compaction(mut self, model: String) -> Self {
        self.model_compaction = Some(model);
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
                    match Self::run_gc_pipeline(
                        consolidator,
                        self.archive_retention_days,
                        decay_now,
                    )
                    .await
                    {
                        Ok(gc_report) => {
                            if !gc_report.errors.is_empty() {
                                tracing::warn!(
                                    agent_id = %consolidator.agent_id(),
                                    error_count = gc_report.errors.len(),
                                    errors = ?gc_report.errors,
                                    "GC pipeline completed with errors"
                                );
                            }
                        }
                        Err(error) => {
                            tracing::warn!(
                                agent_id = %consolidator.agent_id(),
                                %error,
                                "GC pipeline failed"
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

    async fn scan_stale_sessions_and_trigger_boundary_flush(
        consolidator: &Arc<HippocampusConsolidator>,
    ) -> Result<()> {
        let Some(store) = &consolidator.memory_store else {
            return Ok(());
        };

        const MAX_STALE_SESSIONS_PER_SCAN: usize = 5;
        const DEAD_FLUSH_TIMEOUT_MINUTES: i64 = 10;

        let mut candidates: Vec<SessionMemoryStateRecord> = Vec::new();
        let dead = store.find_dead_flushes(DEAD_FLUSH_TIMEOUT_MINUTES).await?;
        for dead_flush in dead
            .into_iter()
            .filter(|state| state.agent_id == consolidator.agent_id)
            .take(MAX_STALE_SESSIONS_PER_SCAN)
        {
            if dead_flush.flush_summary_cache.is_some() {
                match store
                    .refresh_flush_phase_timestamp(&dead_flush.agent_id, &dead_flush.session_id)
                    .await
                {
                    Ok(()) => {
                        tracing::info!(
                            agent_id = %dead_flush.agent_id,
                            session_id = %dead_flush.session_id,
                            "dead flush recovery: Path A (re-armed timeout, resuming from cache)"
                        );
                    }
                    Err(error) => {
                        tracing::warn!(
                            agent_id = %dead_flush.agent_id,
                            session_id = %dead_flush.session_id,
                            %error,
                            "dead flush recovery: Path A failed, falling back to Path B"
                        );
                        if let Err(reset_error) = store
                            .reset_flush_phase(&dead_flush.agent_id, &dead_flush.session_id)
                            .await
                        {
                            tracing::warn!(
                                agent_id = %dead_flush.agent_id,
                                session_id = %dead_flush.session_id,
                                error = %reset_error,
                                "dead flush recovery: Path B fallback also failed"
                            );
                        } else {
                            tracing::info!(
                                agent_id = %dead_flush.agent_id,
                                session_id = %dead_flush.session_id,
                                "dead flush recovery: Path B fallback (reset to idle)"
                            );
                        }
                    }
                }
            } else {
                match store
                    .reset_flush_phase(&dead_flush.agent_id, &dead_flush.session_id)
                    .await
                {
                    Ok(()) => {
                        tracing::info!(
                            agent_id = %dead_flush.agent_id,
                            session_id = %dead_flush.session_id,
                            "dead flush recovery: Path B (reset to idle, no cache present)"
                        );
                    }
                    Err(error) => {
                        tracing::warn!(
                            agent_id = %dead_flush.agent_id,
                            session_id = %dead_flush.session_id,
                            %error,
                            "dead flush recovery: Path B reset failed"
                        );
                    }
                }
            }
        }

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
                    .any(|existing: &SessionMemoryStateRecord| {
                        existing.session_id == state.session_id
                    })
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
                let has_lineage = match &consolidator.memory_store {
                    Some(store) => {
                        match Self::has_archived_chunk_lineage_refs(
                            store,
                            consolidator.agent_id(),
                            &archived_rel_path,
                        )
                        .await
                        {
                            Ok(has) => has,
                            Err(error) => {
                                tracing::warn!(
                                    agent_id = %consolidator.agent_id(),
                                    date = %date,
                                    %error,
                                    "lineage check failed, retaining file"
                                );
                                true
                            }
                        }
                    }
                    None => false,
                };

                if has_lineage {
                    tracing::info!(
                        agent_id = %consolidator.agent_id(),
                        date = %date,
                        action = "retain",
                        reason = "lineage_refs_exist",
                        "Daily file lifecycle action"
                    );
                    continue;
                }

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

    async fn run_gc_pipeline(
        consolidator: &Arc<HippocampusConsolidator>,
        archive_retention_days: u64,
        now: DateTime<Utc>,
    ) -> Result<GcReport> {
        let mut report = GcReport::default();

        match Self::run_daily_file_lifecycle(consolidator, archive_retention_days, now).await {
            Ok(()) => {}
            Err(error) => {
                tracing::warn!(
                    agent_id = %consolidator.agent_id(),
                    %error,
                    "GC Step 1 (daily file lifecycle) failed; continuing"
                );
                report.errors.push(format!("daily_lifecycle: {error}"));
            }
        }

        match consolidator.evaluate_memory_staleness().await {
            Ok(candidates) => match consolidator.prune_stale_sections(&candidates).await {
                Ok(pruned) => report.stale_sections_pruned = pruned,
                Err(error) => {
                    tracing::warn!(
                        agent_id = %consolidator.agent_id(),
                        %error,
                        "GC Step 2 (stale section prune) failed; continuing"
                    );
                    report.errors.push(format!("stale_prune: {error}"));
                }
            },
            Err(error) => {
                tracing::warn!(
                    agent_id = %consolidator.agent_id(),
                    %error,
                    "GC Step 2 (stale section evaluation) failed; continuing"
                );
                report.errors.push(format!("stale_eval: {error}"));
            }
        }

        if let Some(search_index) = consolidator.search_index() {
            let known_paths = Self::collect_known_chunk_paths(consolidator)
                .await
                .unwrap_or_default();
            match search_index.detect_orphan_chunks(&known_paths).await {
                Ok(orphan_paths) => {
                    report.orphans_detected = orphan_paths.len();
                    for path in &orphan_paths {
                        match search_index.delete_indexed_path(path).await {
                            Ok(()) => {
                                report.orphans_cleaned += 1;
                                tracing::info!(
                                    agent_id = %consolidator.agent_id(),
                                    path = %path,
                                    "GC cleaned orphan chunk path"
                                );
                            }
                            Err(error) => {
                                tracing::warn!(
                                    agent_id = %consolidator.agent_id(),
                                    path = %path,
                                    %error,
                                    "GC failed to clean orphan chunk path"
                                );
                                report
                                    .errors
                                    .push(format!("orphan_cleanup({path}): {error}"));
                            }
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        agent_id = %consolidator.agent_id(),
                        %error,
                        "GC Step 3 (orphan detection) failed; continuing"
                    );
                    report.errors.push(format!("orphan_detect: {error}"));
                }
            }
        }

        if let Some(memory_store) = &consolidator.memory_store {
            let daily_active = consolidator
                .file_store
                .list_daily_files()
                .await
                .map(|files| files.len())
                .unwrap_or(0);
            let daily_archived = consolidator
                .file_store
                .list_archived_files()
                .await
                .map(|files| files.len())
                .unwrap_or(0);

            let reporter = clawhive_memory::health::HealthReporter::new(memory_store.db());
            match reporter
                .generate(daily_active, daily_archived, report.orphans_detected)
                .await
            {
                Ok(health_report) => {
                    let workspace_dir = consolidator.file_store.workspace_dir();
                    if let Err(error) =
                        clawhive_memory::health::write_health_report(&health_report, workspace_dir)
                            .await
                    {
                        tracing::warn!(
                            agent_id = %consolidator.agent_id(),
                            %error,
                            "GC failed to write health report"
                        );
                        report.errors.push(format!("health_write: {error}"));
                    } else {
                        report.health_report_generated = true;
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        agent_id = %consolidator.agent_id(),
                        %error,
                        "GC Step 4 (health report) failed"
                    );
                    report.errors.push(format!("health_gen: {error}"));
                }
            }
        }

        tracing::info!(
            agent_id = %consolidator.agent_id(),
            stale_pruned = report.stale_sections_pruned,
            orphans_detected = report.orphans_detected,
            orphans_cleaned = report.orphans_cleaned,
            health_generated = report.health_report_generated,
            errors = report.errors.len(),
            "GC pipeline completed"
        );

        Ok(report)
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

    async fn has_archived_chunk_lineage_refs(
        memory_store: &Arc<MemoryStore>,
        agent_id: &str,
        archived_path: &str,
    ) -> Result<bool> {
        let db = memory_store.db();
        let agent_id_owned = agent_id.to_string();
        let archived_path_owned = archived_path.to_string();

        let chunk_ids: Vec<String> = task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let mut stmt =
                conn.prepare("SELECT id FROM chunks WHERE path = ?1 AND agent_id = ?2")?;
            let ids = stmt
                .query_map([&archived_path_owned, &agent_id_owned], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            Ok::<Vec<String>, anyhow::Error>(ids)
        })
        .await??;

        if chunk_ids.is_empty() {
            return Ok(false);
        }

        let lineage_store = MemoryLineageStore::new(memory_store.db());
        let refs = lineage_store
            .get_canonical_ids_for_sources(agent_id, "chunk", &chunk_ids)
            .await?;
        Ok(refs.values().any(|ids| !ids.is_empty()))
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
    async fn gc_pipeline_happy_path_prunes_stale_cleans_orphans_and_writes_health_report(
    ) -> Result<()> {
        use clawhive_memory::search_index::SearchIndex;

        let (_dir, file_store) = build_file_store()?;
        file_store
            .write_long_term(
                "# MEMORY.md\n\n## 长期项目主线\n\n- stale-unit15-mainline\n\n## 持续性背景脉络\n\n- keep-context\n\n## 关键历史决策\n\n- keep-decision\n",
            )
            .await?;

        let memory_store = Arc::new(MemoryStore::open_in_memory()?);
        let search_index = SearchIndex::new(memory_store.db(), "agent-1");
        let provider = StubEmbeddingProvider;
        search_index
            .index_file(
                "MEMORY.md",
                &file_store.read_long_term().await?,
                "long_term",
                &provider,
            )
            .await?;
        search_index
            .index_file(
                "memory/orphan.md",
                "# Orphan\n\n- chunk",
                "daily",
                &provider,
            )
            .await?;

        {
            let db = memory_store.db();
            let conn = db.lock().expect("lock");
            let stale_ts = (Utc::now() - chrono::Duration::days(95)).to_rfc3339();
            conn.execute(
                "UPDATE chunks SET access_count = 2, last_accessed = ?1 WHERE agent_id = 'agent-1' AND path = 'MEMORY.md' AND text LIKE '%stale-unit15-mainline%'",
                [stale_ts],
            )?;
        }

        let consolidator = Arc::new(
            HippocampusConsolidator::new(
                "agent-1".to_string(),
                file_store.clone(),
                build_router_with_provider(SequenceProvider::new(vec!["STALE".to_string()])),
                "sonnet".to_string(),
                vec![],
            )
            .with_search_index(search_index)
            .with_memory_store(Arc::clone(&memory_store)),
        );

        let report = ConsolidationScheduler::run_gc_pipeline(&consolidator, 30, Utc::now()).await?;

        assert_eq!(report.stale_sections_pruned, 1);
        assert_eq!(report.orphans_detected, 1);
        assert_eq!(report.orphans_cleaned, 1);
        assert!(report.health_report_generated);
        assert!(report.errors.is_empty());

        let orphan_chunks: i64 = {
            let db = memory_store.db();
            let conn = db.lock().expect("lock");
            conn.query_row(
                "SELECT COUNT(*) FROM chunks WHERE agent_id = 'agent-1' AND path = 'memory/orphan.md'",
                [],
                |row| row.get(0),
            )?
        };
        assert_eq!(orphan_chunks, 0);

        assert!(
            fs::metadata(file_store.workspace_dir().join("memory/health.json"))
                .await
                .is_ok()
        );
        Ok(())
    }

    #[tokio::test]
    async fn gc_pipeline_no_orphans_no_stale_is_noop_report() -> Result<()> {
        let (_dir, file_store) = build_file_store()?;
        let consolidator = Arc::new(HippocampusConsolidator::new(
            "agent-1".to_string(),
            file_store,
            build_router(),
            "sonnet".to_string(),
            vec![],
        ));

        let report = ConsolidationScheduler::run_gc_pipeline(&consolidator, 30, Utc::now()).await?;

        assert_eq!(report.stale_sections_pruned, 0);
        assert_eq!(report.orphans_detected, 0);
        assert_eq!(report.orphans_cleaned, 0);
        assert!(!report.health_report_generated);
        assert!(report.errors.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn gc_pipeline_continues_when_stale_prune_fails() -> Result<()> {
        use clawhive_memory::search_index::SearchIndex;

        let (_dir, file_store) = build_file_store()?;
        file_store
            .write_long_term(
                "# MEMORY.md\n\n## 长期项目主线\n\n- stale-unit15-fail\n\n## 持续性背景脉络\n\n- keep-context\n\n## 关键历史决策\n\n- keep-decision\n",
            )
            .await?;

        let memory_store = Arc::new(MemoryStore::open_in_memory()?);
        let search_index = SearchIndex::new(memory_store.db(), "agent-1");
        let provider = StubEmbeddingProvider;
        search_index
            .index_file(
                "MEMORY.md",
                &file_store.read_long_term().await?,
                "long_term",
                &provider,
            )
            .await?;
        search_index
            .index_file(
                "memory/orphan-error.md",
                "# Orphan\n\n- chunk",
                "daily",
                &provider,
            )
            .await?;

        {
            let db = memory_store.db();
            let conn = db.lock().expect("lock");
            let stale_ts = (Utc::now() - chrono::Duration::days(100)).to_rfc3339();
            conn.execute(
                "UPDATE chunks SET access_count = 2, last_accessed = ?1 WHERE agent_id = 'agent-1' AND path = 'MEMORY.md' AND text LIKE '%stale-unit15-fail%'",
                [stale_ts],
            )?;
        }

        let consolidator = Arc::new(
            HippocampusConsolidator::new(
                "agent-1".to_string(),
                file_store,
                build_router_with_provider(FailAtCallProvider::new(vec![], 0)),
                "sonnet".to_string(),
                vec![],
            )
            .with_search_index(search_index)
            .with_memory_store(Arc::clone(&memory_store)),
        );

        let report = ConsolidationScheduler::run_gc_pipeline(&consolidator, 30, Utc::now()).await?;

        assert_eq!(report.stale_sections_pruned, 0);
        assert_eq!(report.orphans_detected, 1);
        assert_eq!(report.orphans_cleaned, 1);
        assert!(report.health_report_generated);
        // After the per-candidate error handling fix, LLM failures in
        // prune_stale_sections are caught per-candidate (warn + continue)
        // instead of propagating. The pipeline completes cleanly.
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
