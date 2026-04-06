use crate::migrations::run_migrations;
use anyhow::{anyhow, Result};
use chrono::{DateTime, TimeDelta, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use tokio::task;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub session_key: String,
    pub session_id: String,
    pub agent_id: String,
    pub created_at: DateTime<Utc>,
    pub last_active: DateTime<Utc>,
    pub ttl_seconds: i64,
    pub interaction_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RecentExplicitMemoryWrite {
    pub turn_index: u64,
    pub memory_ref: String,
    pub canonical_id: Option<String>,
    pub summary: String,
    pub recorded_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeTaskStateRecord {
    Exploring,
    Executing,
    Delivered,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeStatusRecord {
    Open,
    Closed,
    FlushPending,
    Flushed,
}

fn default_episode_status() -> EpisodeStatusRecord {
    EpisodeStatusRecord::Open
}

fn default_flush_phase() -> String {
    FlushPhase::Idle.as_str().to_owned()
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FlushPhase {
    Idle,
    Captured,
    Summarized,
    FactsWritten,
    DailyWritten,
    Archived,
    Done,
}

impl FlushPhase {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Idle => "idle",
            Self::Captured => "captured",
            Self::Summarized => "summarized",
            Self::FactsWritten => "facts_written",
            Self::DailyWritten => "daily_written",
            Self::Archived => "archived",
            Self::Done => "done",
        }
    }

    // Inherent method kept alongside FromStr impl for call-site ergonomics
    // (avoids requiring `use std::str::FromStr` at every call site)
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "captured" => Self::Captured,
            "summarized" => Self::Summarized,
            "facts_written" => Self::FactsWritten,
            "daily_written" => Self::DailyWritten,
            "archived" => Self::Archived,
            "done" => Self::Done,
            _ => Self::Idle,
        }
    }
}

impl std::str::FromStr for FlushPhase {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(FlushPhase::from_str(s))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EpisodeStateRecord {
    pub episode_id: String,
    pub start_turn: u64,
    pub end_turn: u64,
    #[serde(default = "default_episode_status")]
    pub status: EpisodeStatusRecord,
    pub task_state: EpisodeTaskStateRecord,
    pub topic_sketch: String,
    pub last_activity_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionMemoryStateRecord {
    pub agent_id: String,
    pub session_id: String,
    pub session_key: String,
    pub last_flushed_turn: u64,
    pub last_boundary_flush_at: Option<DateTime<Utc>>,
    pub pending_flush: bool,
    #[serde(default = "default_flush_phase")]
    pub flush_phase: String,
    #[serde(default)]
    pub flush_phase_updated_at: Option<String>,
    #[serde(default)]
    pub flush_summary_cache: Option<String>,
    #[serde(default)]
    pub recent_explicit_writes: Vec<RecentExplicitMemoryWrite>,
    #[serde(default)]
    pub open_episodes: Vec<EpisodeStateRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TraceRecord {
    pub timestamp: String,
    pub operation: String,
    pub details: String,
    pub duration_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MemoryStatsRecord {
    pub chunk_count: i64,
    pub fact_count: i64,
    pub pending_dirty_sources: i64,
    pub embedding_cache_count: i64,
    pub trace_count: i64,
}

#[derive(Clone)]
pub struct MemoryStore {
    db: Arc<Mutex<Connection>>,
}

/// Initialize sqlite-vec extension. Must be called before Connection::open().
fn init_sqlite_vec() {
    use rusqlite::ffi::{sqlite3, sqlite3_api_routines, sqlite3_auto_extension};

    type Sqlite3AutoExtFn = unsafe extern "C" fn(
        *mut sqlite3,
        *mut *mut std::ffi::c_char,
        *const sqlite3_api_routines,
    ) -> i32;

    unsafe {
        sqlite3_auto_extension(Some(std::mem::transmute::<*const (), Sqlite3AutoExtFn>(
            sqlite_vec::sqlite3_vec_init as *const (),
        )));
    }
}

impl MemoryStore {
    pub fn open(path: &str) -> Result<Self> {
        init_sqlite_vec();
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        run_migrations(&conn)?;
        Ok(Self {
            db: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn open_in_memory() -> Result<Self> {
        init_sqlite_vec();
        let conn = Connection::open_in_memory()?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        run_migrations(&conn)?;
        Ok(Self {
            db: Arc::new(Mutex::new(conn)),
        })
    }

    pub async fn record_trace(
        &self,
        agent_id: &str,
        operation: &str,
        details: &str,
        duration_ms: Option<i64>,
    ) {
        let db = Arc::clone(&self.db);
        let agent_id = agent_id.to_owned();
        let operation = operation.to_owned();
        let details = details.to_owned();

        let _ = task::spawn_blocking(move || -> Result<()> {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            conn.execute(
                "INSERT INTO memory_trace (agent_id, operation, details, duration_ms) VALUES (?1, ?2, ?3, ?4)",
                params![agent_id, operation, details, duration_ms],
            )?;
            Ok(())
        })
        .await;
    }

    /// Returns the `latest_daily_date` field from the most recent successful
    /// consolidation trace for the given agent, if any.
    pub async fn last_consolidation_daily_date(
        &self,
        agent_id: &str,
    ) -> Result<Option<chrono::NaiveDate>> {
        let db = Arc::clone(&self.db);
        let agent_id = agent_id.to_owned();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let details_json: Option<String> = conn
                .query_row(
                    "SELECT details FROM memory_trace \
                     WHERE agent_id = ?1 AND operation = 'consolidation' \
                     ORDER BY timestamp DESC LIMIT 1",
                    params![agent_id],
                    |row| row.get(0),
                )
                .optional()?;

            if let Some(json_str) = details_json {
                let parsed: serde_json::Value = serde_json::from_str(&json_str)?;
                if let Some(date_str) = parsed.get("latest_daily_date").and_then(|v| v.as_str()) {
                    let date = chrono::NaiveDate::parse_from_str(date_str, "%Y-%m-%d")?;
                    return Ok(Some(date));
                }
            }
            Ok(None)
        })
        .await?
    }

    pub fn db(&self) -> Arc<Mutex<Connection>> {
        Arc::clone(&self.db)
    }

    pub async fn list_traces(
        &self,
        agent_id: &str,
        limit: usize,
        since: Option<&str>,
    ) -> Result<Vec<TraceRecord>> {
        let db = Arc::clone(&self.db);
        let agent_id = agent_id.to_owned();
        let since = since.map(ToOwned::to_owned);
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let mut rows_out = Vec::new();

            if let Some(since_value) = since {
                let mut stmt = conn.prepare(
                    "SELECT timestamp, operation, details, duration_ms\n                     FROM memory_trace\n                     WHERE agent_id = ?1\n                       AND timestamp >= ?2\n                     ORDER BY timestamp DESC\n                     LIMIT ?3",
                )?;
                let rows = stmt.query_map(params![agent_id, since_value, limit as i64], |row| {
                    Ok(TraceRecord {
                        timestamp: row.get(0)?,
                        operation: row.get(1)?,
                        details: row.get(2)?,
                        duration_ms: row.get(3)?,
                    })
                })?;
                for row in rows {
                    rows_out.push(row?);
                }
            } else {
                let mut stmt = conn.prepare(
                    "SELECT timestamp, operation, details, duration_ms\n                     FROM memory_trace\n                     WHERE agent_id = ?1\n                     ORDER BY timestamp DESC\n                     LIMIT ?2",
                )?;
                let rows = stmt.query_map(params![agent_id, limit as i64], |row| {
                    Ok(TraceRecord {
                        timestamp: row.get(0)?,
                        operation: row.get(1)?,
                        details: row.get(2)?,
                        duration_ms: row.get(3)?,
                    })
                })?;
                for row in rows {
                    rows_out.push(row?);
                }
            }

            Ok::<Vec<TraceRecord>, anyhow::Error>(rows_out)
        })
        .await?
    }

    pub async fn stats_for_agent(&self, agent_id: &str) -> Result<MemoryStatsRecord> {
        let db = Arc::clone(&self.db);
        let agent_id = agent_id.to_owned();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;

            let chunk_count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM chunks WHERE agent_id = ?1",
                params![agent_id],
                |row| row.get(0),
            )?;
            let fact_count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM facts WHERE agent_id = ?1",
                params![agent_id],
                |row| row.get(0),
            )?;
            let pending_dirty_sources: i64 = conn.query_row(
                "SELECT COUNT(*) FROM dirty_sources WHERE agent_id = ?1 AND processed_at IS NULL",
                params![agent_id],
                |row| row.get(0),
            )?;
            let trace_count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM memory_trace WHERE agent_id = ?1",
                params![agent_id],
                |row| row.get(0),
            )?;
            let embedding_cache_count: i64 =
                conn.query_row("SELECT COUNT(*) FROM embedding_cache", [], |row| row.get(0))?;

            Ok::<MemoryStatsRecord, anyhow::Error>(MemoryStatsRecord {
                chunk_count,
                fact_count,
                pending_dirty_sources,
                embedding_cache_count,
                trace_count,
            })
        })
        .await?
    }

    pub async fn get_session(&self, key: &str) -> Result<Option<SessionRecord>> {
        let db = Arc::clone(&self.db);
        let key = key.to_owned();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let mut stmt = conn.prepare(
                r#"
                SELECT session_key, session_id, agent_id, created_at, last_active, ttl_seconds, interaction_count
                FROM sessions
                WHERE session_key = ?1
                LIMIT 1
                "#,
            )?;
            let mut rows = stmt.query(params![key])?;
            if let Some(row) = rows.next()? {
                let created_at_raw: String = row.get(3)?;
                let last_active_raw: String = row.get(4)?;
                let session = SessionRecord {
                    session_key: row.get(0)?,
                    session_id: row.get(1)?,
                    agent_id: row.get(2)?,
                    created_at: parse_datetime_sql(&created_at_raw)?,
                    last_active: parse_datetime_sql(&last_active_raw)?,
                    ttl_seconds: row.get(5)?,
                    interaction_count: row.get(6)?,
                };
                return Ok::<Option<SessionRecord>, anyhow::Error>(Some(session));
            }
            Ok::<Option<SessionRecord>, anyhow::Error>(None)
        })
        .await?
    }

    pub async fn upsert_session(&self, session: SessionRecord) -> Result<()> {
        let db = Arc::clone(&self.db);
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            conn.execute(
                r#"
                INSERT INTO sessions (
                    session_key,
                    session_id,
                    agent_id,
                    created_at,
                    last_active,
                    ttl_seconds,
                    interaction_count
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                ON CONFLICT(session_key) DO UPDATE SET
                    session_id = excluded.session_id,
                    agent_id = excluded.agent_id,
                    created_at = excluded.created_at,
                    last_active = excluded.last_active,
                    ttl_seconds = excluded.ttl_seconds,
                    interaction_count = excluded.interaction_count
                "#,
                params![
                    session.session_key,
                    session.session_id,
                    session.agent_id,
                    session.created_at.to_rfc3339(),
                    session.last_active.to_rfc3339(),
                    session.ttl_seconds,
                    session.interaction_count,
                ],
            )?;
            Ok::<(), anyhow::Error>(())
        })
        .await??;

        Ok(())
    }

    pub async fn delete_session(&self, session_key: &str) -> Result<bool> {
        let db = Arc::clone(&self.db);
        let key = session_key.to_string();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|e| anyhow!("failed to lock sqlite connection: {e}"))?;
            let deleted =
                conn.execute("DELETE FROM sessions WHERE session_key = ?1", params![key])?;
            Ok::<bool, anyhow::Error>(deleted > 0)
        })
        .await?
    }

    pub async fn get_session_memory_state(
        &self,
        agent_id: &str,
        session_id: &str,
    ) -> Result<Option<SessionMemoryStateRecord>> {
        let db = Arc::clone(&self.db);
        let agent_id = agent_id.to_owned();
        let session_id = session_id.to_owned();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let mut stmt = conn.prepare(
                r#"
                SELECT agent_id, session_id, session_key, last_flushed_turn, last_boundary_flush_at, pending_flush, flush_phase, flush_phase_updated_at, flush_summary_cache, recent_explicit_writes, open_episodes
                FROM session_memory_state
                WHERE agent_id = ?1 AND session_id = ?2
                LIMIT 1
                "#,
            )?;
            let mut rows = stmt.query(params![agent_id, session_id])?;
            if let Some(row) = rows.next()? {
                let last_boundary_flush_at_raw: Option<String> = row.get(4)?;
                let flush_phase_raw: Option<String> = row.get(6)?;
                let recent_explicit_writes_raw: String = row.get(9)?;
                let open_episodes_raw: String = row.get(10)?;
                return Ok::<Option<SessionMemoryStateRecord>, anyhow::Error>(Some(
                    SessionMemoryStateRecord {
                        agent_id: row.get(0)?,
                        session_id: row.get(1)?,
                        session_key: row.get(2)?,
                        last_flushed_turn: row.get(3)?,
                        last_boundary_flush_at: last_boundary_flush_at_raw
                            .as_deref()
                            .map(parse_datetime_sql)
                            .transpose()?,
                        pending_flush: row.get::<_, i64>(5)? != 0,
                        flush_phase: FlushPhase::from_str(
                            flush_phase_raw.as_deref().unwrap_or(FlushPhase::Idle.as_str()),
                        )
                        .as_str()
                        .to_owned(),
                        flush_phase_updated_at: row.get(7)?,
                        flush_summary_cache: row.get(8)?,
                        recent_explicit_writes: serde_json::from_str(
                            &recent_explicit_writes_raw,
                        )?,
                        open_episodes: serde_json::from_str(&open_episodes_raw)?,
                    },
                ));
            }
            Ok::<Option<SessionMemoryStateRecord>, anyhow::Error>(None)
        })
        .await?
    }

    pub async fn upsert_session_memory_state(&self, state: SessionMemoryStateRecord) -> Result<()> {
        let db = Arc::clone(&self.db);
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            conn.execute(
                r#"
                INSERT INTO session_memory_state (
                    agent_id,
                    session_id,
                    session_key,
                    last_flushed_turn,
                    last_boundary_flush_at,
                    pending_flush,
                    flush_phase,
                    flush_phase_updated_at,
                    flush_summary_cache,
                    recent_explicit_writes,
                    open_episodes,
                    updated_at
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, datetime('now'))
                ON CONFLICT(agent_id, session_id) DO UPDATE SET
                    session_key = excluded.session_key,
                    last_flushed_turn = excluded.last_flushed_turn,
                    last_boundary_flush_at = excluded.last_boundary_flush_at,
                    pending_flush = excluded.pending_flush,
                    flush_phase = excluded.flush_phase,
                    flush_phase_updated_at = excluded.flush_phase_updated_at,
                    flush_summary_cache = excluded.flush_summary_cache,
                    recent_explicit_writes = excluded.recent_explicit_writes,
                    open_episodes = excluded.open_episodes,
                    updated_at = datetime('now')
                "#,
                params![
                    state.agent_id,
                    state.session_id,
                    state.session_key,
                    state.last_flushed_turn,
                    state.last_boundary_flush_at.map(|dt| dt.to_rfc3339()),
                    if state.pending_flush { 1_i64 } else { 0_i64 },
                    FlushPhase::from_str(&state.flush_phase).as_str(),
                    state.flush_phase_updated_at,
                    state.flush_summary_cache,
                    serde_json::to_string(&state.recent_explicit_writes)?,
                    serde_json::to_string(&state.open_episodes)?,
                ],
            )?;
            Ok::<(), anyhow::Error>(())
        })
        .await??;

        Ok(())
    }

    pub async fn delete_session_memory_state(
        &self,
        agent_id: &str,
        session_id: &str,
    ) -> Result<bool> {
        let db = Arc::clone(&self.db);
        let agent_id = agent_id.to_owned();
        let session_id = session_id.to_owned();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|e| anyhow!("failed to lock sqlite connection: {e}"))?;
            let deleted = conn.execute(
                "DELETE FROM session_memory_state WHERE agent_id = ?1 AND session_id = ?2",
                params![agent_id, session_id],
            )?;
            Ok::<bool, anyhow::Error>(deleted > 0)
        })
        .await?
    }

    pub async fn list_pending_session_memory_states_for_session_key(
        &self,
        agent_id: &str,
        session_key: &str,
        limit: usize,
    ) -> Result<Vec<SessionMemoryStateRecord>> {
        let db = Arc::clone(&self.db);
        let agent_id = agent_id.to_owned();
        let session_key = session_key.to_owned();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let mut stmt = conn.prepare(
                r#"
                SELECT agent_id, session_id, session_key, last_flushed_turn, last_boundary_flush_at, pending_flush, flush_phase, flush_phase_updated_at, flush_summary_cache, recent_explicit_writes, open_episodes
                FROM session_memory_state
                WHERE agent_id = ?1
                  AND session_key = ?2
                  AND pending_flush = 1
                ORDER BY updated_at ASC
                LIMIT ?3
                "#,
            )?;
            let mut rows = stmt.query(params![agent_id, session_key, limit as i64])?;
            let mut states = Vec::new();
            while let Some(row) = rows.next()? {
                let last_boundary_flush_at_raw: Option<String> = row.get(4)?;
                let flush_phase_raw: Option<String> = row.get(6)?;
                let recent_explicit_writes_raw: String = row.get(9)?;
                let open_episodes_raw: String = row.get(10)?;
                states.push(SessionMemoryStateRecord {
                    agent_id: row.get(0)?,
                    session_id: row.get(1)?,
                    session_key: row.get(2)?,
                    last_flushed_turn: row.get(3)?,
                    last_boundary_flush_at: last_boundary_flush_at_raw
                        .as_deref()
                        .map(parse_datetime_sql)
                        .transpose()?,
                    pending_flush: row.get::<_, i64>(5)? != 0,
                    flush_phase: FlushPhase::from_str(
                        flush_phase_raw.as_deref().unwrap_or(FlushPhase::Idle.as_str()),
                    )
                    .as_str()
                    .to_owned(),
                    flush_phase_updated_at: row.get(7)?,
                    flush_summary_cache: row.get(8)?,
                    recent_explicit_writes: serde_json::from_str(&recent_explicit_writes_raw)?,
                    open_episodes: serde_json::from_str(&open_episodes_raw)?,
                });
            }
            Ok::<Vec<SessionMemoryStateRecord>, anyhow::Error>(states)
        })
        .await?
    }

    pub async fn try_acquire_flush_lock(&self, agent_id: &str, session_id: &str) -> Result<bool> {
        let db = Arc::clone(&self.db);
        let agent_id = agent_id.to_owned();
        let session_id = session_id.to_owned();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let now = Utc::now().to_rfc3339();
            let updated = conn.execute(
                r#"
                UPDATE session_memory_state
                SET flush_phase = ?3,
                    flush_phase_updated_at = ?4,
                    updated_at = datetime('now')
                WHERE agent_id = ?1
                  AND session_id = ?2
                  AND flush_phase = ?5
                "#,
                params![
                    agent_id,
                    session_id,
                    FlushPhase::Captured.as_str(),
                    now,
                    FlushPhase::Idle.as_str(),
                ],
            )?;
            Ok::<bool, anyhow::Error>(updated > 0)
        })
        .await?
    }

    pub async fn advance_flush_phase(
        &self,
        agent_id: &str,
        session_id: &str,
        new_phase: FlushPhase,
        summary_cache: Option<String>,
    ) -> Result<()> {
        let db = Arc::clone(&self.db);
        let agent_id = agent_id.to_owned();
        let session_id = session_id.to_owned();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let now = Utc::now().to_rfc3339();
            match summary_cache {
                Some(cache) => {
                    conn.execute(
                        r#"
                        UPDATE session_memory_state
                        SET flush_phase = ?3,
                            flush_phase_updated_at = ?4,
                            flush_summary_cache = ?5,
                            updated_at = datetime('now')
                        WHERE agent_id = ?1
                          AND session_id = ?2
                        "#,
                        params![agent_id, session_id, new_phase.as_str(), now, cache],
                    )?;
                }
                None => {
                    conn.execute(
                        r#"
                        UPDATE session_memory_state
                        SET flush_phase = ?3,
                            flush_phase_updated_at = ?4,
                            updated_at = datetime('now')
                        WHERE agent_id = ?1
                          AND session_id = ?2
                        "#,
                        params![agent_id, session_id, new_phase.as_str(), now],
                    )?;
                }
            }
            Ok::<(), anyhow::Error>(())
        })
        .await??;

        Ok(())
    }

    pub async fn reset_flush_phase(&self, agent_id: &str, session_id: &str) -> Result<()> {
        let db = Arc::clone(&self.db);
        let agent_id = agent_id.to_owned();
        let session_id = session_id.to_owned();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let now = Utc::now().to_rfc3339();
            conn.execute(
                r#"
                UPDATE session_memory_state
                SET flush_phase = ?3,
                    flush_phase_updated_at = ?4,
                    flush_summary_cache = NULL,
                    updated_at = datetime('now')
                WHERE agent_id = ?1
                  AND session_id = ?2
                "#,
                params![agent_id, session_id, FlushPhase::Idle.as_str(), now],
            )?;
            Ok::<(), anyhow::Error>(())
        })
        .await??;

        Ok(())
    }

    /// Re-arm the flush timeout without changing the phase (Path A: resume from cache).
    pub async fn refresh_flush_phase_timestamp(
        &self,
        agent_id: &str,
        session_id: &str,
    ) -> Result<()> {
        let db = Arc::clone(&self.db);
        let agent_id = agent_id.to_owned();
        let session_id = session_id.to_owned();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let now = Utc::now().to_rfc3339();
            conn.execute(
                r#"
                UPDATE session_memory_state
                SET flush_phase_updated_at = ?3,
                    updated_at = datetime('now')
                WHERE agent_id = ?1
                  AND session_id = ?2
                "#,
                params![agent_id, session_id, now],
            )?;
            Ok::<(), anyhow::Error>(())
        })
        .await??;
        Ok(())
    }

    pub async fn find_dead_flushes(
        &self,
        timeout_minutes: i64,
    ) -> Result<Vec<SessionMemoryStateRecord>> {
        let db = Arc::clone(&self.db);
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let cutoff = (Utc::now() - TimeDelta::minutes(timeout_minutes)).to_rfc3339();
            let mut stmt = conn.prepare(
                r#"
                SELECT agent_id, session_id, session_key, last_flushed_turn, last_boundary_flush_at, pending_flush, flush_phase, flush_phase_updated_at, flush_summary_cache, recent_explicit_writes, open_episodes
                FROM session_memory_state
                WHERE flush_phase != ?1
                  AND flush_phase_updated_at IS NOT NULL
                  AND flush_phase_updated_at < ?2
                ORDER BY flush_phase_updated_at ASC
                "#,
            )?;
            let mut rows = stmt.query(params![FlushPhase::Idle.as_str(), cutoff])?;
            let mut states = Vec::new();
            while let Some(row) = rows.next()? {
                let last_boundary_flush_at_raw: Option<String> = row.get(4)?;
                let flush_phase_raw: Option<String> = row.get(6)?;
                let recent_explicit_writes_raw: String = row.get(9)?;
                let open_episodes_raw: String = row.get(10)?;
                states.push(SessionMemoryStateRecord {
                    agent_id: row.get(0)?,
                    session_id: row.get(1)?,
                    session_key: row.get(2)?,
                    last_flushed_turn: row.get(3)?,
                    last_boundary_flush_at: last_boundary_flush_at_raw
                        .as_deref()
                        .map(parse_datetime_sql)
                        .transpose()?,
                    pending_flush: row.get::<_, i64>(5)? != 0,
                        flush_phase: FlushPhase::from_str(
                        flush_phase_raw.as_deref().unwrap_or(FlushPhase::Idle.as_str()),
                    )
                    .as_str()
                    .to_owned(),
                    flush_phase_updated_at: row.get(7)?,
                    flush_summary_cache: row.get(8)?,
                    recent_explicit_writes: serde_json::from_str(&recent_explicit_writes_raw)?,
                    open_episodes: serde_json::from_str(&open_episodes_raw)?,
                });
            }
            Ok::<Vec<SessionMemoryStateRecord>, anyhow::Error>(states)
        })
        .await?
    }

    pub async fn find_stale_open_episode_states(
        &self,
        agent_id: &str,
        idle_minutes: i64,
        limit: usize,
    ) -> Result<Vec<SessionMemoryStateRecord>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let db = Arc::clone(&self.db);
        let agent_id = agent_id.to_owned();
        let idle_minutes = idle_minutes.max(1);
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let mut stmt = conn.prepare(
                r#"
                SELECT agent_id, session_id, session_key, last_flushed_turn, last_boundary_flush_at, pending_flush, flush_phase, flush_phase_updated_at, flush_summary_cache, recent_explicit_writes, open_episodes
                FROM session_memory_state
                WHERE agent_id = ?1
                  AND open_episodes LIKE '%"status":"Open"%'
                ORDER BY updated_at ASC
                LIMIT ?2
                "#,
            )?;
            let mut rows = stmt.query(params![agent_id, limit as i64])?;
            let cutoff = Utc::now() - TimeDelta::minutes(idle_minutes);
            let mut states = Vec::new();
            while let Some(row) = rows.next()? {
                let last_boundary_flush_at_raw: Option<String> = row.get(4)?;
                let flush_phase_raw: Option<String> = row.get(6)?;
                let recent_explicit_writes_raw: String = row.get(9)?;
                let open_episodes_raw: String = row.get(10)?;
                let open_episodes: Vec<EpisodeStateRecord> = serde_json::from_str(&open_episodes_raw)?;
                let has_stale_open_episode = open_episodes.iter().any(|episode| {
                    episode.status == EpisodeStatusRecord::Open && episode.last_activity_at <= cutoff
                });
                if !has_stale_open_episode {
                    continue;
                }

                states.push(SessionMemoryStateRecord {
                    agent_id: row.get(0)?,
                    session_id: row.get(1)?,
                    session_key: row.get(2)?,
                    last_flushed_turn: row.get(3)?,
                    last_boundary_flush_at: last_boundary_flush_at_raw
                        .as_deref()
                        .map(parse_datetime_sql)
                        .transpose()?,
                    pending_flush: row.get::<_, i64>(5)? != 0,
                    flush_phase: FlushPhase::from_str(
                        flush_phase_raw.as_deref().unwrap_or(FlushPhase::Idle.as_str()),
                    )
                    .as_str()
                    .to_owned(),
                    flush_phase_updated_at: row.get(7)?,
                    flush_summary_cache: row.get(8)?,
                    recent_explicit_writes: serde_json::from_str(&recent_explicit_writes_raw)?,
                    open_episodes,
                });
            }
            Ok::<Vec<SessionMemoryStateRecord>, anyhow::Error>(states)
        })
        .await?
    }

    pub async fn cleanup_session_memory_state(&self, retention_days: u64) -> Result<usize> {
        let db = Arc::clone(&self.db);
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let retention_modifier = format!("-{} days", retention_days);
            let mut stmt = conn.prepare(
                r#"
                SELECT sms.agent_id, sms.session_id, sms.session_key
                FROM session_memory_state sms
                WHERE sms.pending_flush = 0
                  AND sms.updated_at < datetime('now', ?1)
                "#,
            )?;
            let mut rows = stmt.query(params![retention_modifier])?;
            let mut candidates = Vec::new();
            while let Some(row) = rows.next()? {
                candidates.push((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ));
            }
            drop(rows);
            drop(stmt);

            let mut active_stmt = conn.prepare(
                r#"
                SELECT agent_id, session_key, COALESCE(NULLIF(session_id, ''), session_key)
                FROM sessions
                "#,
            )?;
            let mut active_rows = active_stmt.query([])?;
            let mut active_sessions = std::collections::HashSet::new();
            while let Some(row) = active_rows.next()? {
                active_sessions.insert((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ));
            }
            drop(active_rows);
            drop(active_stmt);

            let mut to_delete = Vec::new();
            for (agent_id, session_id, session_key) in candidates {
                if !active_sessions.contains(&(
                    agent_id.clone(),
                    session_key.clone(),
                    session_id.clone(),
                )) {
                    to_delete.push((agent_id, session_id));
                }
            }

            let mut removed = 0;
            for (agent_id, session_id) in to_delete {
                removed += conn.execute(
                    "DELETE FROM session_memory_state WHERE agent_id = ?1 AND session_id = ?2",
                    params![agent_id, session_id],
                )?;
            }
            Ok::<usize, anyhow::Error>(removed)
        })
        .await?
    }

    // ============================================================
    // Embedding Cache
    // ============================================================

    /// Get cached embedding by hash.
    pub async fn get_embedding_cache(
        &self,
        provider: &str,
        model: &str,
        provider_key: &str,
        hash: &str,
    ) -> Result<Option<Vec<f32>>> {
        let db = Arc::clone(&self.db);
        let provider = provider.to_string();
        let model = model.to_string();
        let provider_key = provider_key.to_string();
        let hash = hash.to_string();

        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let mut stmt = conn.prepare_cached(
                "SELECT embedding FROM embedding_cache 
                 WHERE provider = ?1 AND model = ?2 AND provider_key = ?3 AND hash = ?4",
            )?;
            let embedding: Option<String> = stmt
                .query_row(params![provider, model, provider_key, hash], |row| {
                    row.get(0)
                })
                .optional()?;

            match embedding {
                Some(json) => {
                    let vec: Vec<f32> = serde_json::from_str(&json)?;
                    Ok(Some(vec))
                }
                None => Ok(None),
            }
        })
        .await?
    }

    /// Store embedding in cache.
    pub async fn set_embedding_cache(
        &self,
        provider: &str,
        model: &str,
        provider_key: &str,
        hash: &str,
        embedding: &[f32],
        dims: usize,
    ) -> Result<()> {
        let db = Arc::clone(&self.db);
        let provider = provider.to_string();
        let model = model.to_string();
        let provider_key = provider_key.to_string();
        let hash = hash.to_string();
        let embedding_json = serde_json::to_string(embedding)?;
        let now = Utc::now().to_rfc3339();

        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            conn.execute(
                "INSERT OR REPLACE INTO embedding_cache 
                 (provider, model, provider_key, hash, embedding, dims, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    provider,
                    model,
                    provider_key,
                    hash,
                    embedding_json,
                    dims as i64,
                    now
                ],
            )?;
            Ok::<(), anyhow::Error>(())
        })
        .await?
    }

    /// Clear all cached embeddings for a provider/model.
    pub async fn clear_embedding_cache(&self, provider: &str, model: &str) -> Result<usize> {
        let db = Arc::clone(&self.db);
        let provider = provider.to_string();
        let model = model.to_string();

        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let affected = conn.execute(
                "DELETE FROM embedding_cache WHERE provider = ?1 AND model = ?2",
                params![provider, model],
            )?;
            Ok::<usize, anyhow::Error>(affected)
        })
        .await?
    }

    pub async fn cleanup_expired_embedding_cache(&self, ttl_days: u64) -> Result<usize> {
        if ttl_days == 0 {
            return Ok(0);
        }

        let db = Arc::clone(&self.db);
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|e| anyhow!("failed to lock sqlite connection: {e}"))?;
            let cutoff = (Utc::now() - chrono::TimeDelta::days(ttl_days as i64)).to_rfc3339();
            let deleted = conn.execute(
                "DELETE FROM embedding_cache WHERE updated_at < ?1",
                params![cutoff],
            )?;
            Ok::<usize, anyhow::Error>(deleted)
        })
        .await?
    }
}

fn parse_datetime_sql(raw: &str) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn open_in_memory_succeeds() {
        let store = MemoryStore::open_in_memory();
        assert!(store.is_ok());
    }

    #[tokio::test]
    async fn sqlite_vec_extension_loaded() {
        let store = MemoryStore::open_in_memory().expect("store");
        let db = store.db.lock().expect("lock");
        let version: String = db
            .query_row("SELECT vec_version()", [], |row| row.get(0))
            .expect("vec_version");
        assert!(!version.is_empty());
    }

    #[tokio::test]
    async fn new_search_tables_created() {
        let store = MemoryStore::open_in_memory().expect("store");
        let db = store.db.lock().expect("lock");

        db.execute("INSERT INTO meta (key, value) VALUES ('test', 'value')", [])
            .expect("insert meta");
        let v: String = db
            .query_row("SELECT value FROM meta WHERE key = 'test'", [], |r| {
                r.get(0)
            })
            .expect("select meta");
        assert_eq!(v, "value");

        db.execute(
            "INSERT INTO files (agent_id, path, source, hash, mtime, size) VALUES ('test-agent', 'test.md', 'memory', 'abc', 1234, 100)",
            [],
        )
        .expect("insert files");

        db.execute(
            "INSERT INTO chunks (id, path, source, start_line, end_line, hash, model, text, embedding, updated_at) VALUES ('c1', 'test.md', 'memory', 1, 10, 'h1', 'openai', 'hello world', '', '2024-01-01T00:00:00Z')",
            [],
        )
        .expect("insert chunks");

        db.execute(
            "INSERT INTO chunks_fts (text, id, path, source, model, start_line, end_line) VALUES ('hello world', 'c1', 'test.md', 'memory', 'openai', 1, 10)",
            [],
        )
        .expect("insert chunks_fts");

        let count: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM chunks_fts WHERE chunks_fts MATCH 'hello'",
                [],
                |r| r.get(0),
            )
            .expect("fts search");
        assert_eq!(count, 1);

        db.execute(
            "INSERT INTO embedding_cache (provider, model, provider_key, hash, embedding, dims, updated_at) VALUES ('openai', 'text-embedding-3-small', 'key1', 'hash1', '[]', 1536, '2024-01-01T00:00:00Z')",
            [],
        )
        .expect("insert embedding_cache");
    }

    #[tokio::test]
    async fn session_crud() {
        let store = MemoryStore::open_in_memory().expect("store");
        let missing = store.get_session("abc").await.expect("get missing");
        assert!(missing.is_none());

        let now = Utc::now();
        let rec = SessionRecord {
            session_key: "abc".to_owned(),
            session_id: "session-abc".to_owned(),
            agent_id: "agent-1".to_owned(),
            created_at: now,
            last_active: now,
            ttl_seconds: 3600,
            interaction_count: 3,
        };

        store.upsert_session(rec).await.expect("upsert session");
        let loaded = store
            .get_session("abc")
            .await
            .expect("get session")
            .expect("session exists");

        assert_eq!(loaded.session_key, "abc");
        assert_eq!(loaded.session_id, "session-abc");
        assert_eq!(loaded.agent_id, "agent-1");
        assert_eq!(loaded.ttl_seconds, 3600);
        assert_eq!(loaded.interaction_count, 3);
    }

    #[tokio::test]
    async fn delete_session_removes_existing_record() {
        let store = MemoryStore::open_in_memory().expect("store");
        let now = Utc::now();
        let rec = SessionRecord {
            session_key: "abc".to_owned(),
            session_id: "session-abc".to_owned(),
            agent_id: "agent-1".to_owned(),
            created_at: now,
            last_active: now,
            ttl_seconds: 3600,
            interaction_count: 1,
        };

        store.upsert_session(rec).await.expect("upsert session");
        let deleted = store.delete_session("abc").await.expect("delete session");
        assert!(deleted);
        let loaded = store
            .get_session("abc")
            .await
            .expect("get session after delete");
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn delete_session_returns_false_when_missing() {
        let store = MemoryStore::open_in_memory().expect("store");
        let deleted = store
            .delete_session("does-not-exist")
            .await
            .expect("delete missing session");
        assert!(!deleted);
    }

    #[tokio::test]
    async fn session_memory_state_crud() {
        let store = MemoryStore::open_in_memory().expect("store");
        let missing = store
            .get_session_memory_state("agent-1", "abc")
            .await
            .expect("get missing state");
        assert!(missing.is_none());

        let state = SessionMemoryStateRecord {
            agent_id: "agent-1".to_owned(),
            session_id: "session-abc".to_owned(),
            session_key: "abc".to_owned(),
            last_flushed_turn: 7,
            last_boundary_flush_at: Some(Utc::now()),
            pending_flush: true,
            flush_phase: FlushPhase::Idle.as_str().to_owned(),
            flush_phase_updated_at: None,
            flush_summary_cache: None,
            recent_explicit_writes: vec![RecentExplicitMemoryWrite {
                turn_index: 8,
                memory_ref: "fact-1".to_owned(),
                canonical_id: None,
                summary: "User prefers Chinese replies".to_owned(),
                recorded_at: Utc::now(),
            }],
            open_episodes: vec![EpisodeStateRecord {
                episode_id: "session-abc:8".to_owned(),
                start_turn: 7,
                end_turn: 8,
                status: EpisodeStatusRecord::Open,
                task_state: EpisodeTaskStateRecord::Delivered,
                topic_sketch: "prefers chinese replies".to_owned(),
                last_activity_at: Utc::now(),
            }],
        };
        store
            .upsert_session_memory_state(state.clone())
            .await
            .expect("upsert state");

        let loaded = store
            .get_session_memory_state("agent-1", "session-abc")
            .await
            .expect("get state")
            .expect("state exists");
        assert_eq!(loaded, state);

        let deleted = store
            .delete_session_memory_state("agent-1", "session-abc")
            .await
            .expect("delete state");
        assert!(deleted);
        let loaded = store
            .get_session_memory_state("agent-1", "session-abc")
            .await
            .expect("get state after delete");
        assert!(loaded.is_none());
    }

    #[tokio::test]
    async fn session_memory_state_survives_store_reopen() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let db_path = dir.path().join("memory.db");
        let store = MemoryStore::open(db_path.to_str().expect("db path")).expect("store");

        let recorded_at = Utc::now();
        let state = SessionMemoryStateRecord {
            agent_id: "agent-1".to_owned(),
            session_id: "session-abc".to_owned(),
            session_key: "abc".to_owned(),
            last_flushed_turn: 3,
            last_boundary_flush_at: Some(recorded_at),
            pending_flush: true,
            flush_phase: FlushPhase::Idle.as_str().to_owned(),
            flush_phase_updated_at: None,
            flush_summary_cache: None,
            recent_explicit_writes: vec![RecentExplicitMemoryWrite {
                turn_index: 4,
                memory_ref: "fact-1".to_owned(),
                canonical_id: Some("canon-1".to_owned()),
                summary: "User prefers concise replies".to_owned(),
                recorded_at,
            }],
            open_episodes: vec![EpisodeStateRecord {
                episode_id: "session-abc:4".to_owned(),
                start_turn: 4,
                end_turn: 4,
                status: EpisodeStatusRecord::Open,
                task_state: EpisodeTaskStateRecord::Delivered,
                topic_sketch: "prefers concise replies".to_owned(),
                last_activity_at: recorded_at,
            }],
        };
        store
            .upsert_session_memory_state(state.clone())
            .await
            .expect("upsert state");
        drop(store);

        let reopened = MemoryStore::open(db_path.to_str().expect("db path")).expect("reopen");
        let loaded = reopened
            .get_session_memory_state("agent-1", "session-abc")
            .await
            .expect("get state after reopen")
            .expect("state exists after reopen");

        assert_eq!(loaded, state);
    }

    #[tokio::test]
    async fn list_pending_session_memory_states_filters_by_session_key() {
        let store = MemoryStore::open_in_memory().expect("store");
        let now = Utc::now();

        for (session_id, session_key, pending_flush) in [
            ("pending-a", "chat-1", true),
            ("pending-b", "chat-1", true),
            ("not-pending", "chat-1", false),
            ("other-chat", "chat-2", true),
        ] {
            store
                .upsert_session_memory_state(SessionMemoryStateRecord {
                    agent_id: "agent-1".to_owned(),
                    session_id: session_id.to_owned(),
                    session_key: session_key.to_owned(),
                    last_flushed_turn: 0,
                    last_boundary_flush_at: None,
                    pending_flush,
                    flush_phase: FlushPhase::Idle.as_str().to_owned(),
                    flush_phase_updated_at: None,
                    flush_summary_cache: None,
                    recent_explicit_writes: Vec::new(),
                    open_episodes: Vec::new(),
                })
                .await
                .expect("upsert state");
            let conn = store.db();
            let guard = conn.lock().expect("lock db");
            guard
                .execute(
                    "UPDATE session_memory_state SET updated_at = ?1 WHERE agent_id = ?2 AND session_id = ?3",
                    params![now.to_rfc3339(), "agent-1", session_id],
                )
                .expect("touch updated_at");
        }

        let pending = store
            .list_pending_session_memory_states_for_session_key("agent-1", "chat-1", 8)
            .await
            .expect("list pending");
        let ids = pending
            .into_iter()
            .map(|state| state.session_id)
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["pending-a".to_string(), "pending-b".to_string()]);
    }

    #[tokio::test]
    async fn cleanup_session_memory_state_removes_only_inactive_non_pending_rows() {
        let store = MemoryStore::open_in_memory().expect("store");
        let old_ts = (Utc::now() - TimeDelta::days(45)).to_rfc3339();

        store
            .upsert_session(SessionRecord {
                session_key: "active-chat".to_owned(),
                session_id: "active-session".to_owned(),
                agent_id: "agent-1".to_owned(),
                created_at: Utc::now(),
                last_active: Utc::now(),
                ttl_seconds: 1800,
                interaction_count: 0,
            })
            .await
            .expect("upsert active session");

        for (session_id, session_key, pending_flush, updated_at) in [
            ("stale-cleanup", "old-chat", false, old_ts.clone()),
            ("keep-pending", "old-chat", true, old_ts.clone()),
            ("keep-active", "active-chat", false, Utc::now().to_rfc3339()),
        ] {
            store
                .upsert_session_memory_state(SessionMemoryStateRecord {
                    agent_id: "agent-1".to_owned(),
                    session_id: session_id.to_owned(),
                    session_key: session_key.to_owned(),
                    last_flushed_turn: 0,
                    last_boundary_flush_at: None,
                    pending_flush,
                    flush_phase: FlushPhase::Idle.as_str().to_owned(),
                    flush_phase_updated_at: None,
                    flush_summary_cache: None,
                    recent_explicit_writes: Vec::new(),
                    open_episodes: Vec::new(),
                })
                .await
                .expect("upsert state");
            let conn = store.db();
            let guard = conn.lock().expect("lock db");
            guard
                .execute(
                    "UPDATE session_memory_state SET updated_at = ?1 WHERE agent_id = ?2 AND session_id = ?3",
                    params![updated_at, "agent-1", session_id],
                )
                .expect("set old updated_at");
        }

        let removed = store
            .cleanup_session_memory_state(30)
            .await
            .expect("cleanup states");
        assert_eq!(removed, 1);

        assert!(store
            .get_session_memory_state("agent-1", "stale-cleanup")
            .await
            .expect("load stale-cleanup")
            .is_none());
        assert!(store
            .get_session_memory_state("agent-1", "keep-pending")
            .await
            .expect("load keep-pending")
            .is_some());
        assert!(store
            .get_session_memory_state("agent-1", "keep-active")
            .await
            .expect("load keep-active")
            .is_some());
    }

    #[tokio::test]
    async fn try_acquire_flush_lock_only_succeeds_once() {
        let store = MemoryStore::open_in_memory().expect("store");
        store
            .upsert_session_memory_state(SessionMemoryStateRecord {
                agent_id: "agent-1".to_owned(),
                session_id: "session-1".to_owned(),
                session_key: "chat-1".to_owned(),
                last_flushed_turn: 0,
                last_boundary_flush_at: None,
                pending_flush: true,
                flush_phase: "idle".to_owned(),
                flush_phase_updated_at: None,
                flush_summary_cache: None,
                recent_explicit_writes: Vec::new(),
                open_episodes: Vec::new(),
            })
            .await
            .expect("upsert state");

        let s1 = store.clone();
        let s2 = store.clone();
        let (r1, r2) = tokio::join!(
            s1.try_acquire_flush_lock("agent-1", "session-1"),
            s2.try_acquire_flush_lock("agent-1", "session-1")
        );

        let successes = [r1.expect("first lock"), r2.expect("second lock")]
            .into_iter()
            .filter(|ok| *ok)
            .count();
        assert_eq!(successes, 1);

        let third = store
            .try_acquire_flush_lock("agent-1", "session-1")
            .await
            .expect("third lock");
        assert!(!third);

        let loaded = store
            .get_session_memory_state("agent-1", "session-1")
            .await
            .expect("load state")
            .expect("state exists");
        assert_eq!(loaded.flush_phase, "captured");
        assert!(loaded.flush_phase_updated_at.is_some());
    }

    #[tokio::test]
    async fn advance_and_reset_flush_phase_round_trip() {
        let store = MemoryStore::open_in_memory().expect("store");
        store
            .upsert_session_memory_state(SessionMemoryStateRecord {
                agent_id: "agent-1".to_owned(),
                session_id: "session-2".to_owned(),
                session_key: "chat-1".to_owned(),
                last_flushed_turn: 0,
                last_boundary_flush_at: None,
                pending_flush: true,
                flush_phase: "idle".to_owned(),
                flush_phase_updated_at: None,
                flush_summary_cache: None,
                recent_explicit_writes: Vec::new(),
                open_episodes: Vec::new(),
            })
            .await
            .expect("upsert state");

        store
            .advance_flush_phase(
                "agent-1",
                "session-2",
                FlushPhase::Summarized,
                Some("cached summary".to_owned()),
            )
            .await
            .expect("advance phase");

        let advanced = store
            .get_session_memory_state("agent-1", "session-2")
            .await
            .expect("load advanced")
            .expect("state exists");
        assert_eq!(advanced.flush_phase, "summarized");
        assert_eq!(
            advanced.flush_summary_cache.as_deref(),
            Some("cached summary")
        );
        assert!(advanced.flush_phase_updated_at.is_some());

        store
            .reset_flush_phase("agent-1", "session-2")
            .await
            .expect("reset phase");

        let reset = store
            .get_session_memory_state("agent-1", "session-2")
            .await
            .expect("load reset")
            .expect("state exists");
        assert_eq!(reset.flush_phase, "idle");
        assert_eq!(reset.flush_summary_cache, None);
    }

    #[tokio::test]
    async fn find_dead_flushes_returns_stale_non_idle_only() {
        let store = MemoryStore::open_in_memory().expect("store");

        for (session_id, phase, updated_at) in [
            (
                "stale",
                "captured",
                (Utc::now() - TimeDelta::minutes(120)).to_rfc3339(),
            ),
            (
                "fresh",
                "summarized",
                (Utc::now() - TimeDelta::minutes(2)).to_rfc3339(),
            ),
            (
                "idle",
                "idle",
                (Utc::now() - TimeDelta::minutes(180)).to_rfc3339(),
            ),
        ] {
            store
                .upsert_session_memory_state(SessionMemoryStateRecord {
                    agent_id: "agent-1".to_owned(),
                    session_id: session_id.to_owned(),
                    session_key: "chat-1".to_owned(),
                    last_flushed_turn: 0,
                    last_boundary_flush_at: None,
                    pending_flush: true,
                    flush_phase: phase.to_owned(),
                    flush_phase_updated_at: Some(updated_at),
                    flush_summary_cache: None,
                    recent_explicit_writes: Vec::new(),
                    open_episodes: Vec::new(),
                })
                .await
                .expect("upsert state");
        }

        let stale = store
            .find_dead_flushes(30)
            .await
            .expect("find stale flushes");
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].session_id, "stale");

        let none = store
            .find_dead_flushes(300)
            .await
            .expect("find stale flushes empty");
        assert!(none.is_empty());
    }

    #[tokio::test]
    async fn find_stale_open_episode_states_returns_only_sessions_past_idle_cutoff() {
        let store = MemoryStore::open_in_memory().expect("store");
        let old = Utc::now() - TimeDelta::minutes(90);
        let fresh = Utc::now() - TimeDelta::minutes(5);

        for (session_id, last_activity_at) in [("stale", old), ("fresh", fresh)] {
            store
                .upsert_session_memory_state(SessionMemoryStateRecord {
                    agent_id: "agent-1".to_owned(),
                    session_id: session_id.to_owned(),
                    session_key: format!("chat-{session_id}"),
                    last_flushed_turn: 0,
                    last_boundary_flush_at: None,
                    pending_flush: false,
                    flush_phase: FlushPhase::Idle.as_str().to_owned(),
                    flush_phase_updated_at: None,
                    flush_summary_cache: None,
                    recent_explicit_writes: Vec::new(),
                    open_episodes: vec![EpisodeStateRecord {
                        episode_id: format!("{session_id}:1"),
                        start_turn: 1,
                        end_turn: 1,
                        status: EpisodeStatusRecord::Open,
                        task_state: EpisodeTaskStateRecord::Exploring,
                        topic_sketch: "topic".to_string(),
                        last_activity_at,
                    }],
                })
                .await
                .expect("upsert state");
        }

        let stale = store
            .find_stale_open_episode_states("agent-1", 30, 5)
            .await
            .expect("find stale open episodes");
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].session_id, "stale");
    }

    #[tokio::test]
    async fn cleanup_expired_embedding_cache_removes_only_old_entries() {
        let store = MemoryStore::open_in_memory().expect("store");
        let old_ts = (Utc::now() - TimeDelta::days(45)).to_rfc3339();
        let fresh_ts = Utc::now().to_rfc3339();

        {
            let db = store.db();
            let conn = db.lock().expect("lock db");
            conn.execute(
                "INSERT INTO embedding_cache (provider, model, provider_key, hash, embedding, dims, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params!["openai", "text-embedding-3-small", "key1", "hash-old", "[0.1,0.2]", 2_i64, old_ts],
            )
            .expect("insert old embedding cache");
            conn.execute(
                "INSERT INTO embedding_cache (provider, model, provider_key, hash, embedding, dims, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params!["openai", "text-embedding-3-small", "key1", "hash-fresh", "[0.3,0.4]", 2_i64, fresh_ts],
            )
            .expect("insert fresh embedding cache");
        }

        let deleted = store
            .cleanup_expired_embedding_cache(30)
            .await
            .expect("cleanup embedding cache");
        assert_eq!(deleted, 1);

        {
            let db = store.db();
            let conn = db.lock().expect("lock db");
            let remaining_old: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM embedding_cache WHERE hash = 'hash-old'",
                    [],
                    |row| row.get(0),
                )
                .expect("count old");
            let remaining_fresh: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM embedding_cache WHERE hash = 'hash-fresh'",
                    [],
                    |row| row.get(0),
                )
                .expect("count fresh");
            assert_eq!(remaining_old, 0);
            assert_eq!(remaining_fresh, 1);
        }
    }

    #[tokio::test]
    async fn refresh_flush_phase_timestamp_updates_only_timestamp() {
        let store = MemoryStore::open_in_memory().expect("store");
        let old_ts = (Utc::now() - TimeDelta::minutes(30)).to_rfc3339();

        store
            .upsert_session_memory_state(SessionMemoryStateRecord {
                agent_id: "agent-1".to_owned(),
                session_id: "session-refresh".to_owned(),
                session_key: "chat-1".to_owned(),
                last_flushed_turn: 0,
                last_boundary_flush_at: None,
                pending_flush: true,
                flush_phase: "summarized".to_owned(),
                flush_phase_updated_at: Some(old_ts.clone()),
                flush_summary_cache: Some("some cached data".to_owned()),
                recent_explicit_writes: Vec::new(),
                open_episodes: Vec::new(),
            })
            .await
            .expect("upsert state");

        store
            .refresh_flush_phase_timestamp("agent-1", "session-refresh")
            .await
            .expect("refresh timestamp");

        let updated = store
            .get_session_memory_state("agent-1", "session-refresh")
            .await
            .expect("load updated")
            .expect("state exists");

        assert_eq!(updated.flush_phase, "summarized");
        assert_eq!(
            updated.flush_summary_cache.as_deref(),
            Some("some cached data")
        );
        let new_ts = updated.flush_phase_updated_at.expect("timestamp set");
        assert_ne!(new_ts, old_ts, "timestamp should be updated");
    }

    #[tokio::test]
    async fn list_traces_respects_since_and_limit() {
        let store = MemoryStore::open_in_memory().expect("store");

        {
            let db = store.db();
            let conn = db.lock().expect("lock db");
            conn.execute(
                "INSERT INTO memory_trace (agent_id, operation, details, duration_ms, timestamp) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    "agent-a",
                    "search",
                    "{}",
                    10_i64,
                    "2026-02-28T10:00:00Z"
                ],
            )
            .expect("insert old trace");
            conn.execute(
                "INSERT INTO memory_trace (agent_id, operation, details, duration_ms, timestamp) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    "agent-a",
                    "section_merge",
                    "{\"section\":\"关键历史决策\",\"diff\":[\"+ 新增\"]}",
                    20_i64,
                    "2026-03-02T10:00:00Z"
                ],
            )
            .expect("insert new trace");
        }

        let traces = store
            .list_traces("agent-a", 10, Some("2026-03-01"))
            .await
            .expect("list traces");
        assert_eq!(traces.len(), 1);
        assert_eq!(traces[0].operation, "section_merge");

        let limited = store
            .list_traces("agent-a", 1, None)
            .await
            .expect("list traces with limit");
        assert_eq!(limited.len(), 1);
    }
}
