use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use tokio::task;
use uuid::Uuid;

pub const DIRTY_KIND_MEMORY_FILE: &str = "memory_file";
pub const DIRTY_KIND_DAILY_FILE: &str = "daily_file";
pub const DIRTY_KIND_SESSION: &str = "session";
pub const DIRTY_KIND_FACT: &str = "fact";
pub const DIRTY_KIND_SCHEMA: &str = "schema";
pub const DIRTY_KIND_EMBEDDING_MODEL: &str = "embedding_model";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirtySource {
    pub id: String,
    pub agent_id: String,
    pub source_kind: String,
    pub source_ref: String,
    pub reason: String,
    pub created_at: String,
    pub processed_at: Option<String>,
}

#[derive(Clone)]
pub struct DirtySourceStore {
    db: Arc<Mutex<Connection>>,
}

impl DirtySourceStore {
    pub fn new(db: Arc<Mutex<Connection>>) -> Self {
        Self { db }
    }

    pub async fn enqueue(
        &self,
        agent_id: &str,
        source_kind: &str,
        source_ref: &str,
        reason: &str,
    ) -> Result<()> {
        let db = Arc::clone(&self.db);
        let item = DirtySource {
            id: Uuid::new_v4().to_string(),
            agent_id: agent_id.to_owned(),
            source_kind: source_kind.to_owned(),
            source_ref: source_ref.to_owned(),
            reason: reason.to_owned(),
            created_at: Utc::now().to_rfc3339(),
            processed_at: None,
        };
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            conn.execute(
                "INSERT INTO dirty_sources (id, agent_id, source_kind, source_ref, reason, created_at, processed_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL) \
                 ON CONFLICT(agent_id, source_kind, source_ref) DO UPDATE SET \
                    reason = excluded.reason, \
                    created_at = excluded.created_at, \
                    processed_at = NULL",
                params![
                    item.id,
                    item.agent_id,
                    item.source_kind,
                    item.source_ref,
                    item.reason,
                    item.created_at,
                ],
            )?;
            Ok::<(), anyhow::Error>(())
        })
        .await??;
        Ok(())
    }

    pub async fn list_pending(&self, agent_id: &str, limit: usize) -> Result<Vec<DirtySource>> {
        let db = Arc::clone(&self.db);
        let agent_id = agent_id.to_owned();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let mut stmt = conn.prepare(
                "SELECT id, agent_id, source_kind, source_ref, reason, created_at, processed_at \
                 FROM dirty_sources \
                 WHERE agent_id = ?1 AND processed_at IS NULL \
                 ORDER BY created_at ASC \
                 LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![agent_id, limit as i64], row_to_dirty_source)?;
            let mut items = Vec::new();
            for row in rows {
                items.push(row?);
            }
            Ok::<Vec<DirtySource>, anyhow::Error>(items)
        })
        .await?
    }

    pub async fn mark_processed(&self, id: &str) -> Result<()> {
        let db = Arc::clone(&self.db);
        let id = id.to_owned();
        let processed_at = Utc::now().to_rfc3339();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            conn.execute(
                "UPDATE dirty_sources SET processed_at = ?1 WHERE id = ?2",
                params![processed_at, id],
            )?;
            Ok::<(), anyhow::Error>(())
        })
        .await??;
        Ok(())
    }

    pub async fn pending_count(&self, agent_id: &str) -> Result<i64> {
        let db = Arc::clone(&self.db);
        let agent_id = agent_id.to_owned();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let count = conn.query_row(
                "SELECT COUNT(*) FROM dirty_sources WHERE agent_id = ?1 AND processed_at IS NULL",
                params![agent_id],
                |row| row.get(0),
            )?;
            Ok::<i64, anyhow::Error>(count)
        })
        .await?
    }
}

fn row_to_dirty_source(row: &rusqlite::Row<'_>) -> rusqlite::Result<DirtySource> {
    Ok(DirtySource {
        id: row.get(0)?,
        agent_id: row.get(1)?,
        source_kind: row.get(2)?,
        source_ref: row.get(3)?,
        reason: row.get(4)?,
        created_at: row.get(5)?,
        processed_at: row.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use crate::MemoryStore;

    use super::*;

    #[tokio::test]
    async fn enqueue_is_idempotent_per_source() {
        let store = MemoryStore::open_in_memory().unwrap();
        let dirty = DirtySourceStore::new(store.db());

        dirty
            .enqueue(
                "agent-1",
                DIRTY_KIND_DAILY_FILE,
                "memory/2026-03-29.md",
                "first",
            )
            .await
            .unwrap();
        dirty
            .enqueue(
                "agent-1",
                DIRTY_KIND_DAILY_FILE,
                "memory/2026-03-29.md",
                "second",
            )
            .await
            .unwrap();

        let pending = dirty.list_pending("agent-1", 10).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].reason, "second");
    }

    #[tokio::test]
    async fn mark_processed_removes_from_pending() {
        let store = MemoryStore::open_in_memory().unwrap();
        let dirty = DirtySourceStore::new(store.db());

        dirty
            .enqueue("agent-1", DIRTY_KIND_SESSION, "s1", "append")
            .await
            .unwrap();
        let pending = dirty.list_pending("agent-1", 10).await.unwrap();
        dirty.mark_processed(&pending[0].id).await.unwrap();

        assert_eq!(dirty.pending_count("agent-1").await.unwrap(), 0);
    }
}
