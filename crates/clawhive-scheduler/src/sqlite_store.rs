//! SQLite-based persistence for scheduler and wait tasks

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use tokio::sync::Mutex;

use crate::{RunRecord, RunStatus, ScheduleState, WaitTask, WaitTaskStatus};

/// SQLite store for scheduler persistence
pub struct SqliteStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteStore {
    /// Open or create the database at the given path
    pub fn open(db_path: &Path) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(db_path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;

        // Run migrations synchronously before wrapping in async mutex
        run_migrations(&conn)?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Schedule State
    // ─────────────────────────────────────────────────────────────────────────

    /// Load all schedule states
    pub async fn load_schedule_states(&self) -> Result<Vec<ScheduleState>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            r#"SELECT schedule_id, next_run_at_ms, running_at_ms, last_run_at_ms,
                      last_run_status, last_error, last_duration_ms, consecutive_errors
               FROM schedule_states"#,
        )?;

        let rows = stmt.query_map([], |row| {
            Ok(ScheduleState {
                schedule_id: row.get(0)?,
                next_run_at_ms: row.get(1)?,
                running_at_ms: row.get(2)?,
                last_run_at_ms: row.get(3)?,
                last_run_status: row
                    .get::<_, Option<String>>(4)?
                    .map(|s| parse_run_status(&s)),
                last_error: row.get(5)?,
                last_duration_ms: row.get(6)?,
                consecutive_errors: row.get::<_, i64>(7)? as u32,
            })
        })?;

        let mut states = Vec::new();
        for row in rows {
            states.push(row?);
        }
        Ok(states)
    }

    /// Save a schedule state
    pub async fn save_schedule_state(&self, state: &ScheduleState) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            r#"INSERT OR REPLACE INTO schedule_states
               (schedule_id, next_run_at_ms, running_at_ms, last_run_at_ms,
                last_run_status, last_error, last_duration_ms, consecutive_errors)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"#,
            params![
                state.schedule_id,
                state.next_run_at_ms,
                state.running_at_ms,
                state.last_run_at_ms,
                state.last_run_status.as_ref().map(format_run_status),
                state.last_error,
                state.last_duration_ms,
                state.consecutive_errors as i64,
            ],
        )?;
        Ok(())
    }

    /// Delete a schedule state
    pub async fn delete_schedule_state(&self, schedule_id: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "DELETE FROM schedule_states WHERE schedule_id = ?1",
            [schedule_id],
        )?;
        Ok(())
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Run History
    // ─────────────────────────────────────────────────────────────────────────

    /// Append a run record
    pub async fn append_run_record(&self, record: &RunRecord) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            r#"INSERT INTO run_history
               (schedule_id, started_at, ended_at, status, error, duration_ms)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6)"#,
            params![
                record.schedule_id,
                record.started_at.to_rfc3339(),
                record.ended_at.to_rfc3339(),
                format_run_status(&record.status),
                record.error,
                record.duration_ms as i64,
            ],
        )?;
        Ok(())
    }

    /// Get recent run records for a schedule
    pub async fn recent_runs(&self, schedule_id: &str, limit: usize) -> Result<Vec<RunRecord>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            r#"SELECT schedule_id, started_at, ended_at, status, error, duration_ms
               FROM run_history
               WHERE schedule_id = ?1
               ORDER BY started_at DESC
               LIMIT ?2"#,
        )?;

        let rows = stmt.query_map(params![schedule_id, limit as i64], |row| {
            Ok(RunRecord {
                schedule_id: row.get(0)?,
                started_at: chrono::DateTime::parse_from_rfc3339(&row.get::<_, String>(1)?)
                    .unwrap()
                    .with_timezone(&chrono::Utc),
                ended_at: chrono::DateTime::parse_from_rfc3339(&row.get::<_, String>(2)?)
                    .unwrap()
                    .with_timezone(&chrono::Utc),
                status: parse_run_status(&row.get::<_, String>(3)?),
                error: row.get(4)?,
                duration_ms: row.get::<_, i64>(5)? as u64,
            })
        })?;

        let mut records = Vec::new();
        for row in rows {
            records.push(row?);
        }
        Ok(records)
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Wait Tasks
    // ─────────────────────────────────────────────────────────────────────────

    /// Load all pending wait tasks
    pub async fn load_pending_wait_tasks(&self) -> Result<Vec<WaitTask>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            r#"SELECT id, session_key, check_cmd, success_condition, failure_condition,
                      poll_interval_ms, timeout_at_ms, created_at_ms, last_check_at_ms,
                      status, on_success_message, on_failure_message, on_timeout_message,
                      last_output, error
               FROM wait_tasks
               WHERE status IN ('pending', 'running')"#,
        )?;

        self.query_wait_tasks(&mut stmt, [])
    }

    /// Load a wait task by ID
    pub async fn get_wait_task(&self, task_id: &str) -> Result<Option<WaitTask>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            r#"SELECT id, session_key, check_cmd, success_condition, failure_condition,
                      poll_interval_ms, timeout_at_ms, created_at_ms, last_check_at_ms,
                      status, on_success_message, on_failure_message, on_timeout_message,
                      last_output, error
               FROM wait_tasks
               WHERE id = ?1"#,
        )?;

        let task = stmt
            .query_row([task_id], |row| Ok(Self::row_to_wait_task(row)))
            .optional()?;
        Ok(task)
    }

    /// List wait tasks by session
    pub async fn list_wait_tasks_by_session(&self, session_key: &str) -> Result<Vec<WaitTask>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            r#"SELECT id, session_key, check_cmd, success_condition, failure_condition,
                      poll_interval_ms, timeout_at_ms, created_at_ms, last_check_at_ms,
                      status, on_success_message, on_failure_message, on_timeout_message,
                      last_output, error
               FROM wait_tasks
               WHERE session_key = ?1
               ORDER BY created_at_ms DESC"#,
        )?;

        self.query_wait_tasks(&mut stmt, [session_key])
    }

    /// Save (insert or update) a wait task
    pub async fn save_wait_task(&self, task: &WaitTask) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            r#"INSERT OR REPLACE INTO wait_tasks
               (id, session_key, check_cmd, success_condition, failure_condition,
                poll_interval_ms, timeout_at_ms, created_at_ms, last_check_at_ms,
                status, on_success_message, on_failure_message, on_timeout_message,
                last_output, error)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)"#,
            params![
                task.id,
                task.session_key,
                task.check_cmd,
                task.success_condition,
                task.failure_condition,
                task.poll_interval_ms as i64,
                task.timeout_at_ms,
                task.created_at_ms,
                task.last_check_at_ms,
                format_wait_task_status(&task.status),
                task.on_success_message,
                task.on_failure_message,
                task.on_timeout_message,
                task.last_output,
                task.error,
            ],
        )?;
        Ok(())
    }

    /// Delete a wait task
    pub async fn delete_wait_task(&self, task_id: &str) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute("DELETE FROM wait_tasks WHERE id = ?1", [task_id])?;
        Ok(())
    }

    /// Delete completed wait tasks older than retention period
    pub async fn cleanup_old_wait_tasks(&self, before_ms: i64) -> Result<usize> {
        let conn = self.conn.lock().await;
        let count = conn.execute(
            r#"DELETE FROM wait_tasks
               WHERE status NOT IN ('pending', 'running')
               AND created_at_ms < ?1"#,
            [before_ms],
        )?;
        Ok(count)
    }

    fn query_wait_tasks<P: rusqlite::Params>(
        &self,
        stmt: &mut rusqlite::Statement,
        params: P,
    ) -> Result<Vec<WaitTask>> {
        let rows = stmt.query_map(params, |row| Ok(Self::row_to_wait_task(row)))?;
        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(row?);
        }
        Ok(tasks)
    }

    fn row_to_wait_task(row: &rusqlite::Row) -> WaitTask {
        WaitTask {
            id: row.get(0).unwrap(),
            session_key: row.get(1).unwrap(),
            check_cmd: row.get(2).unwrap(),
            success_condition: row.get(3).unwrap(),
            failure_condition: row.get(4).unwrap(),
            poll_interval_ms: row.get::<_, i64>(5).unwrap() as u64,
            timeout_at_ms: row.get(6).unwrap(),
            created_at_ms: row.get(7).unwrap(),
            last_check_at_ms: row.get(8).unwrap(),
            status: parse_wait_task_status(&row.get::<_, String>(9).unwrap()),
            on_success_message: row.get(10).unwrap(),
            on_failure_message: row.get(11).unwrap(),
            on_timeout_message: row.get(12).unwrap(),
            last_output: row.get(13).unwrap(),
            error: row.get(14).unwrap(),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Migrations
// ─────────────────────────────────────────────────────────────────────────────

fn run_migrations(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"CREATE TABLE IF NOT EXISTS __scheduler_schema_version (
            version INTEGER PRIMARY KEY,
            applied_at TEXT NOT NULL DEFAULT (datetime('now'))
        );"#,
    )?;

    let applied: std::collections::HashSet<i64> = {
        let mut stmt = conn.prepare("SELECT version FROM __scheduler_schema_version")?;
        let rows = stmt.query_map([], |row| row.get::<_, i64>(0))?;
        rows.filter_map(|r| r.ok()).collect()
    };

    let migrations: Vec<(i64, &str)> = vec![
        (
            1,
            r#"
            CREATE TABLE IF NOT EXISTS schedule_states (
                schedule_id TEXT PRIMARY KEY,
                next_run_at_ms INTEGER,
                running_at_ms INTEGER,
                last_run_at_ms INTEGER,
                last_run_status TEXT,
                last_error TEXT,
                last_duration_ms INTEGER,
                consecutive_errors INTEGER NOT NULL DEFAULT 0
            );

            CREATE TABLE IF NOT EXISTS run_history (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                schedule_id TEXT NOT NULL,
                started_at TEXT NOT NULL,
                ended_at TEXT NOT NULL,
                status TEXT NOT NULL,
                error TEXT,
                duration_ms INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_run_history_schedule ON run_history(schedule_id, started_at DESC);
            "#,
        ),
        (
            2,
            r#"
            CREATE TABLE IF NOT EXISTS wait_tasks (
                id TEXT PRIMARY KEY,
                session_key TEXT NOT NULL,
                check_cmd TEXT NOT NULL,
                success_condition TEXT NOT NULL,
                failure_condition TEXT,
                poll_interval_ms INTEGER NOT NULL,
                timeout_at_ms INTEGER NOT NULL,
                created_at_ms INTEGER NOT NULL,
                last_check_at_ms INTEGER,
                status TEXT NOT NULL DEFAULT 'pending',
                on_success_message TEXT,
                on_failure_message TEXT,
                on_timeout_message TEXT,
                last_output TEXT,
                error TEXT
            );

            CREATE INDEX IF NOT EXISTS idx_wait_tasks_session ON wait_tasks(session_key);
            CREATE INDEX IF NOT EXISTS idx_wait_tasks_status ON wait_tasks(status);
            "#,
        ),
    ];

    for (version, sql) in migrations {
        if applied.contains(&version) {
            continue;
        }
        conn.execute_batch(sql)?;
        conn.execute(
            "INSERT INTO __scheduler_schema_version(version) VALUES (?1)",
            [version],
        )?;
    }

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────────────

fn format_run_status(status: &RunStatus) -> &'static str {
    match status {
        RunStatus::Ok => "ok",
        RunStatus::Error => "error",
        RunStatus::Skipped => "skipped",
    }
}

fn parse_run_status(s: &str) -> RunStatus {
    match s {
        "ok" => RunStatus::Ok,
        "error" => RunStatus::Error,
        _ => RunStatus::Skipped,
    }
}

fn format_wait_task_status(status: &WaitTaskStatus) -> &'static str {
    match status {
        WaitTaskStatus::Pending => "pending",
        WaitTaskStatus::Running => "running",
        WaitTaskStatus::Success => "success",
        WaitTaskStatus::Failed => "failed",
        WaitTaskStatus::Timeout => "timeout",
        WaitTaskStatus::Cancelled => "cancelled",
    }
}

fn parse_wait_task_status(s: &str) -> WaitTaskStatus {
    match s {
        "pending" => WaitTaskStatus::Pending,
        "running" => WaitTaskStatus::Running,
        "success" => WaitTaskStatus::Success,
        "failed" => WaitTaskStatus::Failed,
        "timeout" => WaitTaskStatus::Timeout,
        "cancelled" => WaitTaskStatus::Cancelled,
        _ => WaitTaskStatus::Pending,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_wait_task_crud() {
        let tmp = TempDir::new().unwrap();
        let store = SqliteStore::open(&tmp.path().join("test.db")).unwrap();

        let task = WaitTask::new("test-1", "session-1", "echo ok", "contains:ok", 1000, 60000);

        // Save
        store.save_wait_task(&task).await.unwrap();

        // Get
        let loaded = store.get_wait_task("test-1").await.unwrap().unwrap();
        assert_eq!(loaded.id, "test-1");
        assert_eq!(loaded.session_key, "session-1");

        // List by session
        let list = store.list_wait_tasks_by_session("session-1").await.unwrap();
        assert_eq!(list.len(), 1);

        // Delete
        store.delete_wait_task("test-1").await.unwrap();
        assert!(store.get_wait_task("test-1").await.unwrap().is_none());
    }
}
