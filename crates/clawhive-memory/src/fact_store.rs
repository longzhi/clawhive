use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::task;
use uuid::Uuid;

use crate::memory_lineage::MemoryLineageStore;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fact {
    pub id: String,
    pub agent_id: String,
    pub content: String,
    pub fact_type: String,
    pub importance: f64,
    pub confidence: f64,
    pub salience: u8,
    pub status: String,
    pub occurred_at: Option<String>,
    pub recorded_at: String,
    pub source_type: String,
    pub source_session: Option<String>,
    pub access_count: i64,
    pub last_accessed: Option<String>,
    pub superseded_by: Option<String>,
    pub supersede_reason: Option<String>,
    #[serde(default = "default_affect")]
    pub affect: String,
    #[serde(default = "default_affect_intensity")]
    pub affect_intensity: f64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FactHistory {
    pub id: String,
    pub fact_id: String,
    pub event: String,
    pub old_content: Option<String>,
    pub new_content: Option<String>,
    pub reason: Option<String>,
    pub created_at: String,
}

pub fn generate_fact_id(agent_id: &str, content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(agent_id.as_bytes());
    hasher.update(content.as_bytes());
    let hash = hasher.finalize();
    let hash_bytes: [u8; 16] = hash[..16].try_into().unwrap_or([0u8; 16]);
    Uuid::from_bytes(hash_bytes).to_string()
}

pub fn default_salience_for_type(fact_type: &str) -> u8 {
    match fact_type {
        "preference" => 70,
        "rule" => 75,
        "decision" => 65,
        "person" => 50,
        "event" => 40,
        "procedure" => 70,
        _ => 50,
    }
}

fn default_affect() -> String {
    "neutral".to_string()
}

fn default_affect_intensity() -> f64 {
    0.0
}

fn normalize_affect(value: &str) -> &str {
    match value {
        "neutral" | "frustrated" | "excited" | "uncertain" | "urgent" | "satisfied" => value,
        _ => "neutral",
    }
}

fn normalize_affect_intensity(value: f64) -> f64 {
    value.clamp(0.0, 1.0)
}

fn apply_affect_salience_boost(salience: u8, affect: &str, affect_intensity: f64) -> u8 {
    let normalized_affect = normalize_affect(affect);
    if normalized_affect == "neutral" {
        return salience;
    }
    let boosted = (f64::from(salience) * (1.0 + normalize_affect_intensity(affect_intensity) * 0.2))
        .round() as u8;
    boosted.min(100)
}

pub(crate) fn decay_factor_for_type(fact_type: &str) -> f64 {
    match fact_type {
        "rule" => 0.99,
        "preference" => 0.97,
        "decision" => 0.98,
        "event" => 0.93,
        "person" => 0.98,
        "procedure" => 0.99,
        _ => 0.95,
    }
}

fn validated_status(value: &str) -> Result<&str> {
    match value {
        "active" | "superseded" | "retracted" | "expired" | "deleted" | "archived" => Ok(value),
        _ => Err(anyhow!("invalid fact status: {value}")),
    }
}

#[derive(Clone)]
pub struct FactStore {
    db: Arc<Mutex<Connection>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfidenceDecaySummary {
    pub decayed_count: usize,
    pub archived_count: usize,
}

impl FactStore {
    pub fn new(db: Arc<Mutex<Connection>>) -> Self {
        Self { db }
    }

    pub fn db(&self) -> Arc<Mutex<Connection>> {
        Arc::clone(&self.db)
    }

    pub async fn insert_fact(&self, fact: &Fact) -> Result<()> {
        self.insert_fact_with_canonical_key(fact, None).await
    }

    pub async fn insert_fact_with_canonical_key(
        &self,
        fact: &Fact,
        canonical_key: Option<&str>,
    ) -> Result<()> {
        let db = Arc::clone(&self.db);
        let fact = fact.clone();
        let fact_for_lineage = fact.clone();
        let canonical_key = canonical_key
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            conn.execute(
                r#"
                INSERT INTO facts (
                    id, agent_id, content, fact_type, importance, confidence, salience,
                    status, occurred_at, recorded_at, source_type, source_session,
                    access_count, last_accessed, superseded_by, supersede_reason, affect, affect_intensity, created_at, updated_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)
                "#,
                params![
                    fact.id,
                    fact.agent_id,
                    fact.content,
                    fact.fact_type,
                    fact.importance,
                    fact.confidence,
                    apply_affect_salience_boost(
                    if fact.salience == 0 {
                        default_salience_for_type(&fact.fact_type)
                    } else {
                        fact.salience
                    },
                    &fact.affect,
                    fact.affect_intensity,
                    ),
                    fact.status,
                    fact.occurred_at,
                    fact.recorded_at,
                    fact.source_type,
                    fact.source_session,
                    fact.access_count,
                    fact.last_accessed,
                    fact.superseded_by,
                    fact.supersede_reason,
                    normalize_affect(&fact.affect),
                    normalize_affect_intensity(fact.affect_intensity),
                    fact.created_at,
                    fact.updated_at,
                ],
            )?;
            Ok::<(), anyhow::Error>(())
        })
        .await??;

        let lineage_store = MemoryLineageStore::new(Arc::clone(&self.db));
        if let Some(canonical_key) = canonical_key.as_deref() {
            match lineage_store
                .ensure_canonical_with_key(
                    &fact_for_lineage.agent_id,
                    "fact",
                    Some(canonical_key),
                    &fact_for_lineage.content,
                )
                .await
            {
                Ok(canonical) => {
                    if let Err(error) = lineage_store
                        .attach_source(
                            &fact_for_lineage.agent_id,
                            &canonical.canonical_id,
                            "fact",
                            &fact_for_lineage.id,
                            "promoted",
                        )
                        .await
                    {
                        tracing::warn!(
                            fact_id = %fact_for_lineage.id,
                            agent_id = %fact_for_lineage.agent_id,
                            error = %error,
                            "failed to attach keyed lineage for inserted fact"
                        );
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        fact_id = %fact_for_lineage.id,
                        agent_id = %fact_for_lineage.agent_id,
                        error = %error,
                        "failed to create keyed lineage for inserted fact"
                    );
                }
            }
        } else if let Err(error) = lineage_store.link_fact(&fact_for_lineage).await {
            tracing::warn!(
                fact_id = %fact_for_lineage.id,
                agent_id = %fact_for_lineage.agent_id,
                error = %error,
                "failed to create lineage for inserted fact"
            );
        }

        Ok(())
    }

    pub async fn get_active_facts(&self, agent_id: &str) -> Result<Vec<Fact>> {
        let db = Arc::clone(&self.db);
        let agent_id = agent_id.to_owned();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let mut stmt = conn.prepare(
                "SELECT id, agent_id, content, fact_type, importance, confidence, COALESCE(salience, 50), status, \
                 occurred_at, recorded_at, source_type, source_session, access_count, \
                 last_accessed, superseded_by, supersede_reason, COALESCE(affect, 'neutral'), COALESCE(affect_intensity, 0.0), created_at, updated_at \
                 FROM facts WHERE agent_id = ?1 AND status = 'active' \
                 ORDER BY importance DESC, updated_at DESC",
            )?;
            let rows = stmt.query_map(params![agent_id], row_to_fact)?;
            let mut facts = Vec::new();
            for row in rows {
                facts.push(row?);
            }
            Ok(facts)
        })
        .await?
    }

    pub async fn find_by_id(&self, fact_id: &str) -> Result<Option<Fact>> {
        let id = fact_id.to_owned();
        let db = Arc::clone(&self.db);
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let result = conn.query_row(
                "SELECT id, agent_id, content, fact_type, importance, confidence, COALESCE(salience, 50), status, \
                 occurred_at, recorded_at, source_type, source_session, access_count, \
                 last_accessed, superseded_by, supersede_reason, COALESCE(affect, 'neutral'), COALESCE(affect_intensity, 0.0), created_at, updated_at \
                 FROM facts WHERE id = ?1",
                params![id],
                row_to_fact,
            );
            match result {
                Ok(fact) => Ok(Some(fact)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(e.into()),
            }
        })
        .await?
    }

    pub async fn find_by_content(&self, agent_id: &str, content: &str) -> Result<Option<Fact>> {
        let id = generate_fact_id(agent_id, content);
        let db = Arc::clone(&self.db);
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let result = conn.query_row(
                "SELECT id, agent_id, content, fact_type, importance, confidence, COALESCE(salience, 50), status, \
                 occurred_at, recorded_at, source_type, source_session, access_count, \
                 last_accessed, superseded_by, supersede_reason, COALESCE(affect, 'neutral'), COALESCE(affect_intensity, 0.0), created_at, updated_at \
                 FROM facts WHERE id = ?1",
                params![id],
                row_to_fact,
            );
            match result {
                Ok(fact) => Ok(Some(fact)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(e) => Err(e.into()),
            }
        })
        .await?
    }

    pub async fn supersede(&self, old_fact_id: &str, new_fact: &Fact, reason: &str) -> Result<()> {
        let db = Arc::clone(&self.db);
        let old_id = old_fact_id.to_owned();
        let new_fact = new_fact.clone();
        let new_fact_for_lineage = new_fact.clone();
        let reason = reason.to_owned();
        let now = Utc::now().to_rfc3339();
        task::spawn_blocking(move || {
            let mut conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;

            let tx = conn.transaction()?;

            let old_content: Option<String> = tx
                .query_row(
                    "SELECT content FROM facts WHERE id = ?1",
                    params![old_id],
                    |r| r.get(0),
                )
                .ok();

            let updated = tx.execute(
                "UPDATE facts SET status = 'superseded', superseded_by = ?1, updated_at = ?2 WHERE id = ?3",
                params![new_fact.id, now, old_id],
            )?;
            if updated == 0 {
                return Err(anyhow!("fact not found: {old_id}"));
            }

            tx.execute(
                r#"
                INSERT INTO facts (
                    id, agent_id, content, fact_type, importance, confidence, salience,
                    status, occurred_at, recorded_at, source_type, source_session,
                    access_count, last_accessed, superseded_by, supersede_reason, affect, affect_intensity, created_at, updated_at
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)
                "#,
                params![
                    new_fact.id,
                    new_fact.agent_id,
                    new_fact.content,
                    new_fact.fact_type,
                    new_fact.importance,
                    new_fact.confidence,
                    apply_affect_salience_boost(
                    if new_fact.salience == 0 {
                        default_salience_for_type(&new_fact.fact_type)
                    } else {
                        new_fact.salience
                    },
                    &new_fact.affect,
                    new_fact.affect_intensity,
                    ),
                    new_fact.status,
                    new_fact.occurred_at,
                    new_fact.recorded_at,
                    new_fact.source_type,
                    new_fact.source_session,
                    new_fact.access_count,
                    new_fact.last_accessed,
                    new_fact.superseded_by,
                    Some(reason.clone()),
                    normalize_affect(&new_fact.affect),
                    normalize_affect_intensity(new_fact.affect_intensity),
                    new_fact.created_at,
                    new_fact.updated_at,
                ],
            )?;

            tx.execute(
                "INSERT INTO fact_history (id, fact_id, event, old_content, new_content, reason, created_at) \
                 VALUES (?1, ?2, 'SUPERSEDE', ?3, ?4, ?5, ?6)",
                params![
                    Uuid::new_v4().to_string(),
                    old_id,
                    old_content,
                    new_fact.content,
                    reason,
                    now,
                ],
            )?;

            tx.commit()?;
            Ok(())
        })
        .await??;

        if let Err(error) = MemoryLineageStore::new(Arc::clone(&self.db))
            .link_fact(&new_fact_for_lineage)
            .await
        {
            tracing::warn!(
                fact_id = %new_fact_for_lineage.id,
                agent_id = %new_fact_for_lineage.agent_id,
                error = %error,
                "failed to create lineage for superseding fact"
            );
        }

        Ok(())
    }

    pub async fn update_status(&self, fact_id: &str, new_status: &str, reason: &str) -> Result<()> {
        let db = Arc::clone(&self.db);
        let fact_id = fact_id.to_owned();
        let new_status = new_status.to_owned();
        let reason = reason.to_owned();
        let now = Utc::now().to_rfc3339();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;

            let old_status: String = conn.query_row(
                "SELECT status FROM facts WHERE id = ?1",
                params![fact_id],
                |r| r.get(0),
            )?;

            let new_status = validated_status(&new_status)?;

            let event = match new_status {
                "retracted" => "RETRACT",
                "expired" => "EXPIRE",
                "deleted" => "DELETE",
                "archived" => "ARCHIVE",
                _ => "UPDATE",
            };

            conn.execute(
                "UPDATE facts SET status = ?1, updated_at = ?2 WHERE id = ?3",
                params![new_status, now, fact_id],
            )?;

            conn.execute(
                "INSERT INTO fact_history (id, fact_id, event, old_content, new_content, reason, created_at) \
                 VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6)",
                params![
                    Uuid::new_v4().to_string(),
                    fact_id,
                    event,
                    old_status,
                    reason,
                    now,
                ],
            )?;
            Ok(())
        })
        .await?
    }

    pub async fn get_injected_facts(&self, agent_id: &str) -> Result<Vec<Fact>> {
        let db = Arc::clone(&self.db);
        let agent_id = agent_id.to_owned();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let mut stmt = conn.prepare(
                "SELECT id, agent_id, content, fact_type, importance, confidence, COALESCE(salience, 50), status, \
                 occurred_at, recorded_at, source_type, source_session, access_count, \
                 last_accessed, superseded_by, supersede_reason, COALESCE(affect, 'neutral'), COALESCE(affect_intensity, 0.0), created_at, updated_at \
                 FROM facts \
                 WHERE agent_id = ?1 AND status = 'active' AND COALESCE(salience, 50) >= 60 AND confidence >= 0.5 \
                 ORDER BY COALESCE(salience, 50) DESC, updated_at DESC \
                 LIMIT 50",
            )?;
            let rows = stmt.query_map(params![agent_id], row_to_fact)?;
            let mut facts = Vec::new();
            for row in rows {
                facts.push(row?);
            }
            Ok(facts)
        })
        .await?
    }

    pub async fn record_add(&self, fact: &Fact) -> Result<()> {
        let db = Arc::clone(&self.db);
        let fact_id = fact.id.clone();
        let content = fact.content.clone();
        let now = Utc::now().to_rfc3339();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            conn.execute(
                "INSERT INTO fact_history (id, fact_id, event, old_content, new_content, reason, created_at) \
                 VALUES (?1, ?2, 'ADD', NULL, ?3, NULL, ?4)",
                params![Uuid::new_v4().to_string(), fact_id, content, now],
            )?;
            Ok(())
        })
        .await?
    }

    /// Increment access_count and set last_accessed for the given fact IDs.
    pub async fn bump_access(&self, fact_ids: &[String]) -> Result<()> {
        if fact_ids.is_empty() {
            return Ok(());
        }
        let db = Arc::clone(&self.db);
        let ids = fact_ids.to_vec();
        let now = Utc::now().to_rfc3339();
        task::spawn_blocking(move || {
            let conn = db.lock().map_err(|_| anyhow!("lock failed"))?;
            let placeholders: String = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            conn.execute(
                &format!(
                    "UPDATE facts SET access_count = access_count + 1, last_accessed = ?1 \
                     WHERE id IN ({placeholders})"
                ),
                rusqlite::params_from_iter(std::iter::once(now).chain(ids)),
            )?;
            Ok::<(), anyhow::Error>(())
        })
        .await??;
        Ok(())
    }

    pub async fn apply_confidence_decay(&self, agent_id: &str) -> Result<ConfidenceDecaySummary> {
        let db = Arc::clone(&self.db);
        let agent_id = agent_id.to_owned();
        let now = Utc::now().to_rfc3339();

        task::spawn_blocking(move || {
            let mut conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let tx = conn.transaction()?;

            let facts = {
                let mut stmt = tx.prepare(
                    "SELECT id, agent_id, content, fact_type, importance, confidence, COALESCE(salience, 50), status, \
                     occurred_at, recorded_at, source_type, source_session, access_count, \
                     last_accessed, superseded_by, supersede_reason, COALESCE(affect, 'neutral'), COALESCE(affect_intensity, 0.0), created_at, updated_at \
                     FROM facts WHERE agent_id = ?1 AND status = 'active'",
                )?;
                let rows = stmt.query_map(params![agent_id], row_to_fact)?;
                let mut loaded = Vec::new();
                for row in rows {
                    loaded.push(row?);
                }
                loaded
            };

            let mut decayed_count = 0usize;
            let mut archived_count = 0usize;

            for fact in facts {
                let boost = (1.0 + (fact.access_count.max(0) as f64)).ln() * 0.05;
                let boosted_confidence = (fact.confidence + boost).min(1.0);
                let decay_factor = (decay_factor_for_type(&fact.fact_type)
                    * (1.0 + normalize_affect_intensity(fact.affect_intensity) * 0.02))
                    .min(1.0);
                let decayed_confidence = (boosted_confidence * decay_factor).clamp(0.0, 1.0);

                let should_archive =
                    decayed_confidence < 0.2 && fact.access_count < 3 && fact.salience < 30;

                if should_archive {
                    tx.execute(
                        "UPDATE facts SET confidence = ?1, status = 'archived', updated_at = ?2 WHERE id = ?3",
                        params![decayed_confidence, now, fact.id],
                    )?;
                    tx.execute(
                        "INSERT INTO fact_history (id, fact_id, event, old_content, new_content, reason, created_at) \
                         VALUES (?1, ?2, 'ARCHIVE', ?3, NULL, 'confidence_decay_archive', ?4)",
                        params![Uuid::new_v4().to_string(), fact.id, fact.content, now],
                    )?;
                    archived_count += 1;
                } else {
                    tx.execute(
                        "UPDATE facts SET confidence = ?1, updated_at = ?2 WHERE id = ?3",
                        params![decayed_confidence, now, fact.id],
                    )?;
                }

                decayed_count += 1;
            }

            tx.commit()?;

            Ok::<ConfidenceDecaySummary, anyhow::Error>(ConfidenceDecaySummary {
                decayed_count,
                archived_count,
            })
        })
        .await?
    }

    pub async fn get_history(&self, fact_id: &str) -> Result<Vec<FactHistory>> {
        let db = Arc::clone(&self.db);
        let fact_id = fact_id.to_owned();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let mut stmt = conn.prepare(
                "SELECT id, fact_id, event, old_content, new_content, reason, created_at \
                 FROM fact_history WHERE fact_id = ?1 ORDER BY created_at DESC",
            )?;
            let rows = stmt.query_map(params![fact_id], |r| {
                Ok(FactHistory {
                    id: r.get(0)?,
                    fact_id: r.get(1)?,
                    event: r.get(2)?,
                    old_content: r.get(3)?,
                    new_content: r.get(4)?,
                    reason: r.get(5)?,
                    created_at: r.get(6)?,
                })
            })?;
            let mut history = Vec::new();
            for row in rows {
                history.push(row?);
            }
            Ok(history)
        })
        .await?
    }
}

fn row_to_fact(r: &rusqlite::Row) -> rusqlite::Result<Fact> {
    Ok(Fact {
        id: r.get(0)?,
        agent_id: r.get(1)?,
        content: r.get(2)?,
        fact_type: r.get(3)?,
        importance: r.get(4)?,
        confidence: r.get(5)?,
        salience: r.get(6)?,
        status: r.get(7)?,
        occurred_at: r.get(8)?,
        recorded_at: r.get(9)?,
        source_type: r.get(10)?,
        source_session: r.get(11)?,
        access_count: r.get(12)?,
        last_accessed: r.get(13)?,
        superseded_by: r.get(14)?,
        supersede_reason: r.get(15)?,
        affect: r.get(16)?,
        affect_intensity: r.get(17)?,
        created_at: r.get(18)?,
        updated_at: r.get(19)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_lineage::MemoryLineageStore;
    use crate::MemoryStore;

    fn make_fact(agent_id: &str, content: &str, fact_type: &str) -> Fact {
        let now = Utc::now().to_rfc3339();
        Fact {
            id: generate_fact_id(agent_id, content),
            agent_id: agent_id.to_owned(),
            content: content.to_owned(),
            fact_type: fact_type.to_owned(),
            importance: 0.5,
            confidence: 1.0,
            salience: default_salience_for_type(fact_type),
            status: "active".to_owned(),
            occurred_at: None,
            recorded_at: now.clone(),
            source_type: "consolidation".to_owned(),
            source_session: None,
            access_count: 0,
            last_accessed: None,
            superseded_by: None,
            supersede_reason: None,
            affect: default_affect(),
            affect_intensity: default_affect_intensity(),
            created_at: now.clone(),
            updated_at: now,
        }
    }

    #[tokio::test]
    async fn insert_and_get_active_facts() {
        let store = MemoryStore::open_in_memory().unwrap();
        let fact_store = FactStore::new(store.db());

        let fact = make_fact("agent-1", "User prefers Rust", "preference");
        fact_store.insert_fact(&fact).await.unwrap();
        fact_store.record_add(&fact).await.unwrap();

        let facts = fact_store.get_active_facts("agent-1").await.unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].content, "User prefers Rust");

        let empty = fact_store.get_active_facts("agent-2").await.unwrap();
        assert!(empty.is_empty());
    }

    #[tokio::test]
    async fn find_by_content() {
        let store = MemoryStore::open_in_memory().unwrap();
        let fact_store = FactStore::new(store.db());

        let fact = make_fact("agent-1", "User lives in Tokyo", "event");
        fact_store.insert_fact(&fact).await.unwrap();

        let found = fact_store
            .find_by_content("agent-1", "User lives in Tokyo")
            .await
            .unwrap();
        assert!(found.is_some());

        let not_found = fact_store
            .find_by_content("agent-1", "User lives in Berlin")
            .await
            .unwrap();
        assert!(not_found.is_none());
    }

    #[tokio::test]
    async fn supersede_old_fact() {
        let store = MemoryStore::open_in_memory().unwrap();
        let fact_store = FactStore::new(store.db());

        let old = make_fact("agent-1", "User lives in Berlin", "event");
        fact_store.insert_fact(&old).await.unwrap();
        fact_store.record_add(&old).await.unwrap();

        let new = make_fact("agent-1", "User lives in Tokyo", "event");
        fact_store
            .supersede(&old.id, &new, "User moved to Tokyo")
            .await
            .unwrap();

        let active = fact_store.get_active_facts("agent-1").await.unwrap();
        let contents: Vec<&str> = active.iter().map(|f| f.content.as_str()).collect();
        assert_eq!(contents, vec!["User lives in Tokyo"]);

        let history = fact_store.get_history(&old.id).await.unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].event, "SUPERSEDE");
        assert_eq!(history[1].event, "ADD");
    }

    #[tokio::test]
    async fn retract_fact() {
        let store = MemoryStore::open_in_memory().unwrap();
        let fact_store = FactStore::new(store.db());

        let fact = make_fact("agent-1", "Wrong info", "event");
        fact_store.insert_fact(&fact).await.unwrap();

        fact_store
            .update_status(&fact.id, "retracted", "User corrected this")
            .await
            .unwrap();

        let active = fact_store.get_active_facts("agent-1").await.unwrap();
        assert!(active.is_empty());

        let history = fact_store.get_history(&fact.id).await.unwrap();
        assert_eq!(history[0].event, "RETRACT");
    }

    #[tokio::test]
    async fn bump_access_increments_count_and_sets_last_accessed() {
        let store = MemoryStore::open_in_memory().unwrap();
        let fact_store = FactStore::new(store.db());
        let fact = make_fact("agent-1", "Test access counting", "preference");
        fact_store.insert_fact(&fact).await.unwrap();

        fact_store
            .bump_access(std::slice::from_ref(&fact.id))
            .await
            .unwrap();
        fact_store
            .bump_access(std::slice::from_ref(&fact.id))
            .await
            .unwrap();

        let loaded = fact_store
            .find_by_content("agent-1", "Test access counting")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.access_count, 2);
        assert!(loaded.last_accessed.is_some());
    }

    #[tokio::test]
    async fn insert_fact_creates_lineage() {
        let store = MemoryStore::open_in_memory().unwrap();
        let fact_store = FactStore::new(store.db());
        let lineage_store = MemoryLineageStore::new(store.db());

        let fact = make_fact("agent-1", "User likes black coffee", "preference");
        fact_store.insert_fact(&fact).await.unwrap();

        let links = lineage_store
            .get_links_for_source("agent-1", "fact", &fact.id)
            .await
            .unwrap();
        assert_eq!(links.len(), 1);

        let canonical = lineage_store
            .get_canonical(&links[0].canonical_id)
            .await
            .unwrap()
            .expect("canonical should exist");
        assert_eq!(canonical.summary, "User likes black coffee");
    }

    #[test]
    fn default_salience_for_type_rules() {
        assert_eq!(default_salience_for_type("preference"), 70);
        assert_eq!(default_salience_for_type("rule"), 75);
        assert_eq!(default_salience_for_type("decision"), 65);
        assert_eq!(default_salience_for_type("person"), 50);
        assert_eq!(default_salience_for_type("event"), 40);
        assert_eq!(default_salience_for_type("procedure"), 70);
        assert_eq!(default_salience_for_type("unknown"), 50);
    }

    #[tokio::test]
    async fn archived_status_is_excluded_from_active_facts() {
        let store = MemoryStore::open_in_memory().unwrap();
        let fact_store = FactStore::new(store.db());
        let fact = make_fact("agent-1", "Old preference", "preference");
        fact_store.insert_fact(&fact).await.unwrap();

        fact_store
            .update_status(&fact.id, "archived", "stale")
            .await
            .unwrap();

        let active = fact_store.get_active_facts("agent-1").await.unwrap();
        assert!(active.is_empty());
    }

    #[tokio::test]
    async fn insert_persists_salience_column() {
        let store = MemoryStore::open_in_memory().unwrap();
        let fact_store = FactStore::new(store.db());
        let fact = make_fact("agent-1", "Has salience", "procedure");
        fact_store.insert_fact(&fact).await.unwrap();

        let conn = store.db();
        let conn = conn.lock().unwrap();
        let salience: i64 = conn
            .query_row(
                "SELECT salience FROM facts WHERE id = ?1",
                params![fact.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(salience, 70);
    }

    #[tokio::test]
    async fn supersede_stores_supersede_reason() {
        let store = MemoryStore::open_in_memory().unwrap();
        let fact_store = FactStore::new(store.db());

        let old = make_fact("agent-1", "User lives in Berlin", "event");
        fact_store.insert_fact(&old).await.unwrap();
        let new = make_fact("agent-1", "User lives in Tokyo", "event");

        fact_store
            .supersede(&old.id, &new, "User moved to Tokyo")
            .await
            .unwrap();

        let conn = store.db();
        let conn = conn.lock().unwrap();
        let reason: Option<String> = conn
            .query_row(
                "SELECT supersede_reason FROM facts WHERE id = ?1",
                params![new.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(reason.as_deref(), Some("User moved to Tokyo"));
    }

    #[tokio::test]
    async fn get_injected_facts_filters_orders_and_limits() {
        let store = MemoryStore::open_in_memory().unwrap();
        let fact_store = FactStore::new(store.db());

        for i in 0..55 {
            let mut fact = make_fact("agent-1", &format!("rule-{i}"), "rule");
            fact.importance = 0.1;
            fact.confidence = 0.9;
            fact_store.insert_fact(&fact).await.unwrap();
        }

        let mut low_conf = make_fact("agent-1", "low-conf", "rule");
        low_conf.confidence = 0.4;
        fact_store.insert_fact(&low_conf).await.unwrap();

        let mut low_salience = make_fact("agent-1", "low-salience", "event");
        low_salience.confidence = 0.9;
        fact_store.insert_fact(&low_salience).await.unwrap();

        let archived = make_fact("agent-1", "archived-fact", "rule");
        fact_store.insert_fact(&archived).await.unwrap();
        fact_store
            .update_status(&archived.id, "archived", "archive for test")
            .await
            .unwrap();

        let injected = fact_store.get_injected_facts("agent-1").await.unwrap();
        assert_eq!(injected.len(), 50);
        assert!(injected.iter().all(|f| f.status == "active"));
        assert!(injected.iter().all(|f| f.confidence >= 0.5));
        assert!(injected.iter().all(|f| f.salience >= 60));

        for pair in injected.windows(2) {
            assert!(pair[0].salience >= pair[1].salience);
        }
    }

    #[tokio::test]
    async fn apply_confidence_decay_updates_event_and_rule_with_expected_decay_factors() {
        let store = MemoryStore::open_in_memory().unwrap();
        let fact_store = FactStore::new(store.db());

        let event = make_fact("agent-1", "Event fact", "event");
        let rule = make_fact("agent-1", "Rule fact", "rule");
        fact_store.insert_fact(&event).await.unwrap();
        fact_store.insert_fact(&rule).await.unwrap();

        let summary = fact_store.apply_confidence_decay("agent-1").await.unwrap();
        assert_eq!(summary.decayed_count, 2);
        assert_eq!(summary.archived_count, 0);

        let decayed_event = fact_store
            .find_by_content("agent-1", "Event fact")
            .await
            .unwrap()
            .unwrap();
        let decayed_rule = fact_store
            .find_by_content("agent-1", "Rule fact")
            .await
            .unwrap()
            .unwrap();

        assert!((decayed_event.confidence - 0.93).abs() < 1e-9);
        assert!((decayed_rule.confidence - 0.99).abs() < 1e-9);
    }

    #[tokio::test]
    async fn apply_confidence_decay_boosts_by_log_access_before_decay() {
        let store = MemoryStore::open_in_memory().unwrap();
        let fact_store = FactStore::new(store.db());

        let mut fact = make_fact("agent-1", "High access fact", "event");
        fact.confidence = 0.5;
        fact.access_count = 100;
        fact_store.insert_fact(&fact).await.unwrap();

        let summary = fact_store.apply_confidence_decay("agent-1").await.unwrap();
        assert_eq!(summary.decayed_count, 1);

        let decayed = fact_store
            .find_by_content("agent-1", "High access fact")
            .await
            .unwrap()
            .unwrap();
        let boost = (1.0_f64 + 100.0).ln() * 0.05;
        let expected = (0.5 + boost).min(1.0) * 0.93;
        assert!((decayed.confidence - expected).abs() < 1e-9);
    }

    #[tokio::test]
    async fn apply_confidence_decay_archives_low_value_facts_and_records_archive_reason() {
        let store = MemoryStore::open_in_memory().unwrap();
        let fact_store = FactStore::new(store.db());

        let mut low_value = make_fact("agent-1", "Low value", "event");
        low_value.confidence = 0.15;
        low_value.salience = 20;
        low_value.access_count = 0;
        fact_store.insert_fact(&low_value).await.unwrap();

        let summary = fact_store.apply_confidence_decay("agent-1").await.unwrap();
        assert_eq!(summary.decayed_count, 1);
        assert_eq!(summary.archived_count, 1);

        let loaded = fact_store
            .find_by_content("agent-1", "Low value")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.status, "archived");

        let history = fact_store.get_history(&low_value.id).await.unwrap();
        let archive = history
            .iter()
            .find(|entry| entry.event == "ARCHIVE")
            .unwrap();
        assert_eq!(archive.reason.as_deref(), Some("confidence_decay_archive"));
    }

    #[tokio::test]
    async fn apply_confidence_decay_skips_archived_facts() {
        let store = MemoryStore::open_in_memory().unwrap();
        let fact_store = FactStore::new(store.db());

        let mut archived = make_fact("agent-1", "Already archived", "event");
        archived.status = "archived".to_string();
        archived.confidence = 0.42;
        fact_store.insert_fact(&archived).await.unwrap();

        let summary = fact_store.apply_confidence_decay("agent-1").await.unwrap();
        assert_eq!(summary.decayed_count, 0);
        assert_eq!(summary.archived_count, 0);

        let loaded = fact_store
            .find_by_content("agent-1", "Already archived")
            .await
            .unwrap()
            .unwrap();
        assert!((loaded.confidence - 0.42).abs() < 1e-9);
    }

    #[tokio::test]
    async fn insert_fact_applies_affect_salience_boost_for_frustrated_fact() {
        let store = MemoryStore::open_in_memory().unwrap();
        let fact_store = FactStore::new(store.db());

        let mut fact = make_fact("agent-1", "Frustrating blocker persists", "event");
        fact.salience = 50;
        fact.affect = "frustrated".to_string();
        fact.affect_intensity = 0.7;
        fact_store.insert_fact(&fact).await.unwrap();

        let loaded = fact_store
            .find_by_content("agent-1", "Frustrating blocker persists")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.salience, 57);
    }

    #[tokio::test]
    async fn insert_fact_keeps_salience_for_neutral_affect() {
        let store = MemoryStore::open_in_memory().unwrap();
        let fact_store = FactStore::new(store.db());

        let mut fact = make_fact("agent-1", "Neutral status update", "event");
        fact.salience = 50;
        fact.affect = "neutral".to_string();
        fact.affect_intensity = 0.0;
        fact_store.insert_fact(&fact).await.unwrap();

        let loaded = fact_store
            .find_by_content("agent-1", "Neutral status update")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.salience, 50);
    }

    #[tokio::test]
    async fn insert_fact_caps_affect_salience_boost_at_twenty_percent() {
        let store = MemoryStore::open_in_memory().unwrap();
        let fact_store = FactStore::new(store.db());

        let mut fact = make_fact("agent-1", "Urgent outage triage", "event");
        fact.salience = 50;
        fact.affect = "urgent".to_string();
        fact.affect_intensity = 1.0;
        fact_store.insert_fact(&fact).await.unwrap();

        let loaded = fact_store
            .find_by_content("agent-1", "Urgent outage triage")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.salience, 60);
    }

    #[tokio::test]
    async fn old_facts_keep_default_affect_without_behavior_change() {
        let store = MemoryStore::open_in_memory().unwrap();
        let fact_store = FactStore::new(store.db());

        let fact = make_fact("agent-1", "Legacy fact shape", "event");
        fact_store.insert_fact(&fact).await.unwrap();

        let loaded = fact_store
            .find_by_content("agent-1", "Legacy fact shape")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(loaded.affect, "neutral");
        assert!((loaded.affect_intensity - 0.0).abs() < 1e-9);
        assert_eq!(loaded.salience, default_salience_for_type("event"));
    }

    #[tokio::test]
    async fn apply_confidence_decay_slows_decay_for_higher_affect_intensity() {
        let store = MemoryStore::open_in_memory().unwrap();
        let fact_store = FactStore::new(store.db());

        let mut fact = make_fact("agent-1", "Emotionally charged fact", "event");
        fact.affect = "frustrated".to_string();
        fact.affect_intensity = 1.0;
        fact_store.insert_fact(&fact).await.unwrap();

        fact_store.apply_confidence_decay("agent-1").await.unwrap();

        let loaded = fact_store
            .find_by_content("agent-1", "Emotionally charged fact")
            .await
            .unwrap()
            .unwrap();
        let expected_decay_factor = (0.93_f64 * (1.0 + 1.0 * 0.02)).min(1.0);
        assert!((loaded.confidence - expected_decay_factor).abs() < 1e-9);
    }

    #[test]
    fn decay_factor_for_type_maps_expected_values() {
        assert!((decay_factor_for_type("rule") - 0.99).abs() < 1e-9);
        assert!((decay_factor_for_type("preference") - 0.97).abs() < 1e-9);
        assert!((decay_factor_for_type("decision") - 0.98).abs() < 1e-9);
        assert!((decay_factor_for_type("event") - 0.93).abs() < 1e-9);
        assert!((decay_factor_for_type("person") - 0.98).abs() < 1e-9);
        assert!((decay_factor_for_type("procedure") - 0.99).abs() < 1e-9);
        assert!((decay_factor_for_type("unknown") - 0.95).abs() < 1e-9);
    }
}
