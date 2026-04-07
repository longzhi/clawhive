use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use chrono::{DateTime, Datelike, Duration, Utc};
use clawhive_memory::fact_store::FactStore;
use clawhive_memory::memory_lineage::MemoryLineageStore;
use clawhive_memory::{EpisodeStatusRecord, FlushPhase, MemoryStore, SessionMemoryStateRecord};
use tokio::task;

use super::{ConsolidationReport, HippocampusConsolidator};

#[derive(Debug, Default)]
pub struct GcReport {
    pub stale_sections_pruned: usize,
    pub orphans_detected: usize,
    pub orphans_cleaned: usize,
    pub health_report_generated: bool,
    pub errors: Vec<String>,
}

pub struct ConsolidationScheduler {
    consolidators: Vec<Arc<HippocampusConsolidator>>,
    pub(super) cron_expr: String,
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

    pub(super) async fn scan_stale_sessions_and_trigger_boundary_flush(
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

    pub(super) async fn run_daily_file_lifecycle(
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

    pub(super) async fn run_gc_pipeline(
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

    pub(super) async fn collect_known_chunk_paths(
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

    pub(super) fn should_run_weekly_decay(
        last_decay_at: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> bool {
        if now.weekday() != chrono::Weekday::Sun {
            return false;
        }

        match last_decay_at {
            Some(last) => now.signed_duration_since(last) >= Duration::days(6),
            None => true,
        }
    }

    pub(super) async fn run_weekly_confidence_decay_if_due(
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
