use std::sync::{Arc, Mutex};
use std::{collections::HashMap, collections::HashSet};

use anyhow::{anyhow, Result};
use chrono::Utc;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::task;
use uuid::Uuid;

use crate::fact_store::Fact;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryCanon {
    pub canonical_id: String,
    pub agent_id: String,
    pub canonical_kind: String,
    pub summary: String,
    pub status: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryLineage {
    pub id: String,
    pub agent_id: String,
    pub canonical_id: String,
    pub source_kind: String,
    pub source_ref: String,
    pub relation: String,
    pub created_at: String,
}

#[derive(Clone)]
pub struct MemoryLineageStore {
    db: Arc<Mutex<Connection>>,
}

impl MemoryLineageStore {
    pub fn new(db: Arc<Mutex<Connection>>) -> Self {
        Self { db }
    }

    pub async fn ensure_canonical(
        &self,
        agent_id: &str,
        canonical_kind: &str,
        summary: &str,
    ) -> Result<MemoryCanon> {
        self.ensure_canonical_with_key(agent_id, canonical_kind, None, summary)
            .await
    }

    pub async fn ensure_canonical_with_key(
        &self,
        agent_id: &str,
        canonical_kind: &str,
        canonical_key: Option<&str>,
        summary: &str,
    ) -> Result<MemoryCanon> {
        let db = Arc::clone(&self.db);
        let agent_id = agent_id.to_owned();
        let canonical_kind = canonical_kind.to_owned();
        let canonical_key = canonical_key
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        let summary = summary.trim().to_owned();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            ensure_canonical_inner(
                &conn,
                &agent_id,
                &canonical_kind,
                canonical_key.as_deref(),
                &summary,
            )
        })
        .await?
    }

    pub async fn attach_source(
        &self,
        agent_id: &str,
        canonical_id: &str,
        source_kind: &str,
        source_ref: &str,
        relation: &str,
    ) -> Result<MemoryLineage> {
        let db = Arc::clone(&self.db);
        let agent_id = agent_id.to_owned();
        let canonical_id = canonical_id.to_owned();
        let source_kind = source_kind.to_owned();
        let source_ref = source_ref.to_owned();
        let relation = relation.to_owned();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            attach_source_inner(
                &conn,
                &agent_id,
                &canonical_id,
                &source_kind,
                &source_ref,
                &relation,
            )
        })
        .await?
    }

    pub async fn link_fact(&self, fact: &Fact) -> Result<String> {
        let canonical = self
            .ensure_canonical_with_key(&fact.agent_id, "fact", None, &fact.content)
            .await?;
        self.attach_source(
            &fact.agent_id,
            &canonical.canonical_id,
            "fact",
            &fact.id,
            "promoted",
        )
        .await?;
        Ok(canonical.canonical_id)
    }

    pub async fn link_memory_promotion(
        &self,
        agent_id: &str,
        summary: &str,
        source_date: Option<&str>,
        section: &str,
        canonical_key: Option<&str>,
    ) -> Result<String> {
        let canonical = self
            .ensure_canonical_with_key(agent_id, "memory", canonical_key, summary)
            .await?;
        if let Some(source_date) = source_date.map(str::trim).filter(|value| !value.is_empty()) {
            self.attach_source(
                agent_id,
                &canonical.canonical_id,
                "daily_section",
                &format!("memory/{source_date}.md#{}", canonical.canonical_id),
                "derived",
            )
            .await?;
        }
        self.attach_source(
            agent_id,
            &canonical.canonical_id,
            "memory_section",
            &format!("MEMORY.md#{}#{}", section.trim(), canonical.canonical_id),
            "promoted",
        )
        .await?;
        Ok(canonical.canonical_id)
    }

    pub async fn link_memory_to_daily_canonical(
        &self,
        agent_id: &str,
        memory_canonical_id: &str,
        daily_summary: &str,
        source_date: Option<&str>,
        canonical_key: Option<&str>,
    ) -> Result<Option<String>> {
        let Some(source_date) = source_date.map(str::trim).filter(|value| !value.is_empty()) else {
            return Ok(None);
        };

        let daily_canonical = if let Some(canonical_key) = canonical_key
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            self.ensure_canonical_with_key(agent_id, "daily", Some(canonical_key), daily_summary)
                .await?
        } else if let Some(canonical) = self
            .find_canonical_by_summary(agent_id, "daily", daily_summary)
            .await?
        {
            canonical
        } else {
            tracing::debug!(
                agent_id = %agent_id,
                source_date = %source_date,
                daily_summary = %daily_summary,
                "skipping memory->daily canonical bridge because no daily canonical exists"
            );
            return Ok(None);
        };

        self.attach_source(
            agent_id,
            memory_canonical_id,
            "canonical",
            &daily_canonical.canonical_id,
            "supersedes",
        )
        .await?;

        Ok(Some(daily_canonical.canonical_id))
    }

    pub async fn link_memory_supersedes(
        &self,
        agent_id: &str,
        newer_summary: &str,
        older_summary: &str,
    ) -> Result<()> {
        let newer = self
            .ensure_canonical(agent_id, "memory", newer_summary)
            .await?;
        let older = self
            .ensure_canonical(agent_id, "memory", older_summary)
            .await?;
        self.attach_source(
            agent_id,
            &newer.canonical_id,
            "canonical",
            &older.canonical_id,
            "supersedes",
        )
        .await?;
        Ok(())
    }

    pub async fn get_canonical(&self, canonical_id: &str) -> Result<Option<MemoryCanon>> {
        let db = Arc::clone(&self.db);
        let canonical_id = canonical_id.to_owned();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let result = conn.query_row(
                "SELECT canonical_id, agent_id, canonical_kind, summary, status, created_at, updated_at \
                 FROM memory_canon WHERE canonical_id = ?1",
                params![canonical_id],
                row_to_canon,
            );
            match result {
                Ok(canonical) => Ok(Some(canonical)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(error) => Err(error.into()),
            }
        })
        .await?
    }

    pub async fn find_canonical_by_summary(
        &self,
        agent_id: &str,
        canonical_kind: &str,
        summary: &str,
    ) -> Result<Option<MemoryCanon>> {
        let db = Arc::clone(&self.db);
        let agent_id = agent_id.to_owned();
        let canonical_kind = canonical_kind.to_owned();
        let summary = summary.trim().to_owned();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let result = conn.query_row(
                "SELECT canonical_id, agent_id, canonical_kind, summary, status, created_at, updated_at \
                 FROM memory_canon WHERE agent_id = ?1 AND canonical_kind = ?2 AND summary = ?3 \
                 ORDER BY updated_at DESC LIMIT 1",
                params![agent_id, canonical_kind, summary],
                row_to_canon,
            );
            match result {
                Ok(canonical) => Ok(Some(canonical)),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
                Err(error) => Err(error.into()),
            }
        })
        .await?
    }

    pub async fn get_links_for_source(
        &self,
        agent_id: &str,
        source_kind: &str,
        source_ref: &str,
    ) -> Result<Vec<MemoryLineage>> {
        let db = Arc::clone(&self.db);
        let agent_id = agent_id.to_owned();
        let source_kind = source_kind.to_owned();
        let source_ref = source_ref.to_owned();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let mut stmt = conn.prepare(
                "SELECT id, agent_id, canonical_id, source_kind, source_ref, relation, created_at \
                 FROM memory_lineage \
                 WHERE agent_id = ?1 AND source_kind = ?2 AND source_ref = ?3 \
                 ORDER BY created_at ASC",
            )?;
            let rows =
                stmt.query_map(params![agent_id, source_kind, source_ref], row_to_lineage)?;
            let mut links = Vec::new();
            for row in rows {
                links.push(row?);
            }
            Ok(links)
        })
        .await?
    }

    pub async fn get_canonical_ids_for_sources(
        &self,
        agent_id: &str,
        source_kind: &str,
        source_refs: &[String],
    ) -> Result<HashMap<String, Vec<String>>> {
        if source_refs.is_empty() {
            return Ok(HashMap::new());
        }

        let db = Arc::clone(&self.db);
        let agent_id = agent_id.to_owned();
        let source_kind = source_kind.to_owned();
        let source_refs = source_refs.to_vec();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;

            let placeholders = source_refs
                .iter()
                .map(|_| "?")
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT source_ref, canonical_id FROM memory_lineage \
                 WHERE agent_id = ?1 AND source_kind = ?2 AND source_ref IN ({placeholders})"
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(source_refs.len() + 2);
            params.push(&agent_id);
            params.push(&source_kind);
            for source_ref in &source_refs {
                params.push(source_ref);
            }

            let rows = stmt.query_map(rusqlite::params_from_iter(params), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;

            let mut grouped: HashMap<String, HashSet<String>> = HashMap::new();
            for row in rows {
                let (source_ref, canonical_id) = row?;
                grouped.entry(source_ref).or_default().insert(canonical_id);
            }

            Ok::<HashMap<String, Vec<String>>, anyhow::Error>(
                grouped
                    .into_iter()
                    .map(|(source_ref, canonical_ids)| {
                        (source_ref, canonical_ids.into_iter().collect::<Vec<_>>())
                    })
                    .collect(),
            )
        })
        .await?
    }

    pub async fn get_superseding_canonical_ids(
        &self,
        agent_id: &str,
        canonical_ids: &[String],
    ) -> Result<HashMap<String, Vec<String>>> {
        if canonical_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let db = Arc::clone(&self.db);
        let agent_id = agent_id.to_owned();
        let canonical_ids = canonical_ids.to_vec();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;

            let placeholders = canonical_ids
                .iter()
                .map(|_| "?")
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT source_ref, canonical_id FROM memory_lineage \
                 WHERE agent_id = ?1 AND source_kind = 'canonical' AND relation = 'supersedes' \
                   AND source_ref IN ({placeholders})"
            );
            let mut stmt = conn.prepare(&sql)?;
            let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(canonical_ids.len() + 1);
            params.push(&agent_id);
            for canonical_id in &canonical_ids {
                params.push(canonical_id);
            }

            let rows = stmt.query_map(rusqlite::params_from_iter(params), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;

            let mut grouped: HashMap<String, HashSet<String>> = HashMap::new();
            for row in rows {
                let (source_ref, canonical_id) = row?;
                grouped.entry(source_ref).or_default().insert(canonical_id);
            }

            Ok::<HashMap<String, Vec<String>>, anyhow::Error>(
                grouped
                    .into_iter()
                    .map(|(source_ref, canonical_ids)| {
                        (source_ref, canonical_ids.into_iter().collect::<Vec<_>>())
                    })
                    .collect(),
            )
        })
        .await?
    }

    pub async fn attach_matching_chunks(
        &self,
        agent_id: &str,
        canonical_id: &str,
        path: &str,
        summary: &str,
        relation: &str,
    ) -> Result<usize> {
        let db = Arc::clone(&self.db);
        let agent_id = agent_id.to_owned();
        let canonical_id = canonical_id.to_owned();
        let path = path.to_owned();
        let summary = summary.trim().to_owned();
        let relation = relation.to_owned();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let normalized_summary = normalize_memory_text(&summary);
            if normalized_summary.is_empty() {
                return Ok::<usize, anyhow::Error>(0);
            }

            let mut stmt = conn.prepare(
                "SELECT id, text FROM chunks WHERE agent_id = ?1 AND path = ?2 ORDER BY start_line ASC",
            )?;
            let rows = stmt.query_map(params![&agent_id, &path], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;

            let mut attached = 0;
            for row in rows {
                let (chunk_id, text) = row?;
                if !chunk_matches_summary(&text, &normalized_summary) {
                    continue;
                }

                attach_source_inner(
                    &conn,
                    &agent_id,
                    &canonical_id,
                    "chunk",
                    &chunk_id,
                    &relation,
                )?;
                attached += 1;
            }

            Ok::<usize, anyhow::Error>(attached)
        })
        .await?
    }

    pub async fn attach_matching_chunks_in_section(
        &self,
        agent_id: &str,
        canonical_id: &str,
        path: &str,
        section_heading: &str,
        summary: &str,
        relation: &str,
    ) -> Result<usize> {
        let db = Arc::clone(&self.db);
        let agent_id = agent_id.to_owned();
        let canonical_id = canonical_id.to_owned();
        let path = path.to_owned();
        let section_heading = section_heading.trim().to_owned();
        let summary = summary.trim().to_owned();
        let relation = relation.to_owned();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let normalized_summary = normalize_memory_text(&summary);
            let normalized_heading = normalize_memory_text(&section_heading);
            if normalized_summary.is_empty() && normalized_heading.is_empty() {
                return Ok::<usize, anyhow::Error>(0);
            }

            let mut stmt = conn.prepare(
                "SELECT id, text FROM chunks WHERE agent_id = ?1 AND path = ?2 ORDER BY start_line ASC",
            )?;
            let rows = stmt.query_map(params![&agent_id, &path], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;

            let mut matched = Vec::new();
            let mut section_fallback = Vec::new();
            for row in rows {
                let (chunk_id, text) = row?;
                let normalized_chunk = normalize_memory_text(&text);
                if normalized_chunk.is_empty() {
                    continue;
                }

                if !normalized_summary.is_empty()
                    && chunk_matches_summary_normalized(&normalized_chunk, &normalized_summary)
                {
                    matched.push(chunk_id);
                    continue;
                }

                if !normalized_heading.is_empty()
                    && chunk_matches_section_heading(&normalized_chunk, &normalized_heading)
                {
                    section_fallback.push(chunk_id);
                }
            }

            let selected = if matched.is_empty() {
                section_fallback
            } else {
                matched
            };

            let mut attached = 0;
            for chunk_id in selected {
                attach_source_inner(
                    &conn,
                    &agent_id,
                    &canonical_id,
                    "chunk",
                    &chunk_id,
                    &relation,
                )?;
                attached += 1;
            }

            Ok::<usize, anyhow::Error>(attached)
        })
        .await?
    }

    pub async fn attach_matching_chunks_by_prefix(
        &self,
        agent_id: &str,
        canonical_id: &str,
        path_prefix: &str,
        summary: &str,
        relation: &str,
    ) -> Result<usize> {
        let db = Arc::clone(&self.db);
        let agent_id = agent_id.to_owned();
        let canonical_id = canonical_id.to_owned();
        let path_prefix = path_prefix.to_owned();
        let summary = summary.trim().to_owned();
        let relation = relation.to_owned();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let normalized_summary = normalize_memory_text(&summary);
            if normalized_summary.is_empty() {
                return Ok::<usize, anyhow::Error>(0);
            }

            let like_pattern = format!("{path_prefix}%");
            let mut stmt = conn.prepare(
                "SELECT id, text FROM chunks WHERE agent_id = ?1 AND path LIKE ?2 ORDER BY path ASC, start_line ASC",
            )?;
            let rows = stmt.query_map(params![&agent_id, &like_pattern], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;

            let mut attached = 0;
            for row in rows {
                let (chunk_id, text) = row?;
                if !chunk_matches_summary(&text, &normalized_summary) {
                    continue;
                }

                attach_source_inner(
                    &conn,
                    &agent_id,
                    &canonical_id,
                    "chunk",
                    &chunk_id,
                    &relation,
                )?;
                attached += 1;
            }

            Ok::<usize, anyhow::Error>(attached)
        })
        .await?
    }
}

fn ensure_canonical_inner(
    conn: &Connection,
    agent_id: &str,
    canonical_kind: &str,
    canonical_key: Option<&str>,
    summary: &str,
) -> Result<MemoryCanon> {
    let canonical_id =
        generate_canonical_id_with_key(agent_id, canonical_kind, canonical_key, summary);
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO memory_canon (canonical_id, agent_id, canonical_kind, summary, status, created_at, updated_at) \
         VALUES (?1, ?2, ?3, ?4, 'active', ?5, ?6) \
         ON CONFLICT(canonical_id) DO UPDATE SET \
           summary = excluded.summary, \
           status = 'active', \
           updated_at = excluded.updated_at",
        params![canonical_id, agent_id, canonical_kind, summary, now, now],
    )?;
    Ok(MemoryCanon {
        canonical_id,
        agent_id: agent_id.to_owned(),
        canonical_kind: canonical_kind.to_owned(),
        summary: summary.to_owned(),
        status: "active".to_owned(),
        created_at: now.clone(),
        updated_at: now,
    })
}

fn attach_source_inner(
    conn: &Connection,
    agent_id: &str,
    canonical_id: &str,
    source_kind: &str,
    source_ref: &str,
    relation: &str,
) -> Result<MemoryLineage> {
    let id = generate_lineage_id(agent_id, canonical_id, source_kind, source_ref, relation);
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO memory_lineage (id, agent_id, canonical_id, source_kind, source_ref, relation, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
         ON CONFLICT(id) DO NOTHING",
        params![id, agent_id, canonical_id, source_kind, source_ref, relation, now],
    )?;
    Ok(MemoryLineage {
        id,
        agent_id: agent_id.to_owned(),
        canonical_id: canonical_id.to_owned(),
        source_kind: source_kind.to_owned(),
        source_ref: source_ref.to_owned(),
        relation: relation.to_owned(),
        created_at: now,
    })
}

pub fn generate_canonical_id(agent_id: &str, canonical_kind: &str, summary: &str) -> String {
    generate_canonical_id_with_key(agent_id, canonical_kind, None, summary)
}

pub fn generate_canonical_id_with_key(
    agent_id: &str,
    canonical_kind: &str,
    canonical_key: Option<&str>,
    summary: &str,
) -> String {
    let identity = canonical_key
        .map(normalize_memory_text)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| normalize_memory_text(summary));
    let mut hasher = Sha256::new();
    hasher.update(agent_id.as_bytes());
    hasher.update(canonical_kind.as_bytes());
    hasher.update(identity.as_bytes());
    let hash = hasher.finalize();
    let hash_bytes: [u8; 16] = hash[..16].try_into().unwrap_or([0u8; 16]);
    Uuid::from_bytes(hash_bytes).to_string()
}

fn generate_lineage_id(
    agent_id: &str,
    canonical_id: &str,
    source_kind: &str,
    source_ref: &str,
    relation: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(agent_id.as_bytes());
    hasher.update(canonical_id.as_bytes());
    hasher.update(source_kind.as_bytes());
    hasher.update(source_ref.as_bytes());
    hasher.update(relation.as_bytes());
    let hash = hasher.finalize();
    let hash_bytes: [u8; 16] = hash[..16].try_into().unwrap_or([0u8; 16]);
    Uuid::from_bytes(hash_bytes).to_string()
}

fn normalize_memory_text(input: &str) -> String {
    input
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn chunk_matches_summary(text: &str, normalized_summary: &str) -> bool {
    let normalized_chunk = normalize_memory_text(text);
    if normalized_chunk.is_empty() {
        return false;
    }

    chunk_matches_summary_normalized(&normalized_chunk, normalized_summary)
}

fn chunk_matches_summary_normalized(normalized_chunk: &str, normalized_summary: &str) -> bool {
    if normalized_chunk.is_empty() {
        return false;
    }

    if normalized_chunk.contains(normalized_summary)
        || normalized_summary.contains(normalized_chunk)
    {
        return true;
    }

    let summary_tokens = token_set(normalized_summary);
    let chunk_tokens = token_set(normalized_chunk);
    if summary_tokens.is_empty() || chunk_tokens.is_empty() {
        return false;
    }

    let overlap = summary_tokens.intersection(&chunk_tokens).count();
    overlap >= 2 && (overlap as f64 / summary_tokens.len() as f64) >= 0.6
}

fn chunk_matches_section_heading(normalized_chunk: &str, normalized_heading: &str) -> bool {
    !normalized_heading.is_empty() && normalized_chunk.contains(normalized_heading)
}

fn token_set(text: &str) -> std::collections::HashSet<String> {
    text.split(|ch: char| !ch.is_alphanumeric())
        .map(str::trim)
        .filter(|token| token.len() >= 3)
        .map(|token| token.to_ascii_lowercase())
        .collect()
}

fn row_to_canon(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryCanon> {
    Ok(MemoryCanon {
        canonical_id: row.get(0)?,
        agent_id: row.get(1)?,
        canonical_kind: row.get(2)?,
        summary: row.get(3)?,
        status: row.get(4)?,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
    })
}

fn row_to_lineage(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryLineage> {
    Ok(MemoryLineage {
        id: row.get(0)?,
        agent_id: row.get(1)?,
        canonical_id: row.get(2)?,
        source_kind: row.get(3)?,
        source_ref: row.get(4)?,
        relation: row.get(5)?,
        created_at: row.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemoryStore;
    use rusqlite::params;

    fn make_fact(agent_id: &str, content: &str) -> Fact {
        let now = Utc::now().to_rfc3339();
        Fact {
            id: crate::fact_store::generate_fact_id(agent_id, content),
            agent_id: agent_id.to_owned(),
            content: content.to_owned(),
            fact_type: "preference".to_owned(),
            importance: 0.5,
            confidence: 1.0,
            salience: crate::fact_store::default_salience_for_type("preference"),
            status: "active".to_owned(),
            occurred_at: None,
            recorded_at: now.clone(),
            source_type: "test".to_owned(),
            source_session: None,
            access_count: 0,
            last_accessed: None,
            superseded_by: None,
            supersede_reason: None,
            created_at: now.clone(),
            updated_at: now,
        }
    }

    #[tokio::test]
    async fn link_fact_creates_canonical_and_lineage() {
        let store = MemoryStore::open_in_memory().unwrap();
        let lineage_store = MemoryLineageStore::new(store.db());
        let fact = make_fact("agent-1", "User prefers Rust");

        let canonical_id = lineage_store.link_fact(&fact).await.unwrap();
        let canonical = lineage_store
            .get_canonical(&canonical_id)
            .await
            .unwrap()
            .expect("canonical should exist");
        let links = lineage_store
            .get_links_for_source("agent-1", "fact", &fact.id)
            .await
            .unwrap();

        assert_eq!(canonical.canonical_kind, "fact");
        assert_eq!(canonical.summary, "User prefers Rust");
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].canonical_id, canonical_id);
        assert_eq!(links[0].relation, "promoted");
    }

    #[tokio::test]
    async fn link_fact_is_idempotent() {
        let store = MemoryStore::open_in_memory().unwrap();
        let lineage_store = MemoryLineageStore::new(store.db());
        let fact = make_fact("agent-1", "User prefers Rust");

        let first = lineage_store.link_fact(&fact).await.unwrap();
        let second = lineage_store.link_fact(&fact).await.unwrap();
        let links = lineage_store
            .get_links_for_source("agent-1", "fact", &fact.id)
            .await
            .unwrap();

        assert_eq!(first, second);
        assert_eq!(links.len(), 1);
    }

    #[tokio::test]
    async fn link_memory_promotion_creates_daily_and_memory_links() {
        let store = MemoryStore::open_in_memory().unwrap();
        let lineage_store = MemoryLineageStore::new(store.db());

        let canonical_id = lineage_store
            .link_memory_promotion(
                "agent-1",
                "Clawhive memory refactor adopts section-based consolidation",
                Some("2026-03-29"),
                "长期项目主线",
                None,
            )
            .await
            .unwrap();

        let canonical = lineage_store
            .get_canonical(&canonical_id)
            .await
            .unwrap()
            .expect("canonical should exist");
        let daily_links = lineage_store
            .get_links_for_source(
                "agent-1",
                "daily_section",
                &format!("memory/2026-03-29.md#{canonical_id}"),
            )
            .await
            .unwrap();
        let memory_links = lineage_store
            .get_links_for_source(
                "agent-1",
                "memory_section",
                &format!("MEMORY.md#长期项目主线#{canonical_id}"),
            )
            .await
            .unwrap();

        assert_eq!(canonical.canonical_kind, "memory");
        assert_eq!(daily_links.len(), 1);
        assert_eq!(daily_links[0].relation, "derived");
        assert_eq!(memory_links.len(), 1);
        assert_eq!(memory_links[0].relation, "promoted");
    }

    #[tokio::test]
    async fn link_memory_promotion_reuses_canonical_for_same_key() {
        let store = MemoryStore::open_in_memory().unwrap();
        let lineage_store = MemoryLineageStore::new(store.db());

        let first = lineage_store
            .link_memory_promotion(
                "agent-1",
                "Use incremental patch consolidation for memory",
                Some("2026-03-28"),
                "长期项目主线",
                Some("memory-consolidation"),
            )
            .await
            .unwrap();
        let second = lineage_store
            .link_memory_promotion(
                "agent-1",
                "Use section-based consolidation for memory",
                Some("2026-03-29"),
                "长期项目主线",
                Some("memory-consolidation"),
            )
            .await
            .unwrap();
        let canonical = lineage_store
            .get_canonical(&second)
            .await
            .unwrap()
            .expect("canonical should exist");

        assert_eq!(first, second);
        assert_eq!(
            second,
            generate_canonical_id_with_key(
                "agent-1",
                "memory",
                Some("memory-consolidation"),
                "Use section-based consolidation for memory"
            )
        );
        assert_eq!(
            canonical.summary,
            "Use section-based consolidation for memory"
        );
    }

    #[tokio::test]
    async fn link_memory_to_daily_canonical_records_supersedes_bridge() {
        let store = MemoryStore::open_in_memory().unwrap();
        let lineage_store = MemoryLineageStore::new(store.db());

        let daily = lineage_store
            .ensure_canonical_with_key(
                "agent-1",
                "daily",
                Some("memory-consolidation"),
                "Use incremental patch consolidation for memory",
            )
            .await
            .unwrap();
        let memory = lineage_store
            .ensure_canonical_with_key(
                "agent-1",
                "memory",
                Some("memory-consolidation"),
                "Use section-based consolidation for memory",
            )
            .await
            .unwrap();

        let bridged = lineage_store
            .link_memory_to_daily_canonical(
                "agent-1",
                &memory.canonical_id,
                "Use incremental patch consolidation for memory",
                Some("2026-03-29"),
                Some("memory-consolidation"),
            )
            .await
            .unwrap();
        let links = lineage_store
            .get_links_for_source("agent-1", "canonical", &daily.canonical_id)
            .await
            .unwrap();

        assert_eq!(bridged, Some(daily.canonical_id.clone()));
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].canonical_id, memory.canonical_id);
        assert_eq!(links[0].relation, "supersedes");
    }

    #[tokio::test]
    async fn link_memory_supersedes_creates_canonical_relation() {
        let store = MemoryStore::open_in_memory().unwrap();
        let lineage_store = MemoryLineageStore::new(store.db());

        lineage_store
            .link_memory_supersedes(
                "agent-1",
                "Use section-based consolidation for memory",
                "Use incremental patch consolidation for memory",
            )
            .await
            .unwrap();

        let newer_id = generate_canonical_id(
            "agent-1",
            "memory",
            "Use section-based consolidation for memory",
        );
        let older_id = generate_canonical_id(
            "agent-1",
            "memory",
            "Use incremental patch consolidation for memory",
        );
        let links = lineage_store
            .get_links_for_source("agent-1", "canonical", &older_id)
            .await
            .unwrap();

        assert_eq!(links.len(), 1);
        assert_eq!(links[0].canonical_id, newer_id);
        assert_eq!(links[0].relation, "supersedes");
    }

    #[tokio::test]
    async fn attach_matching_chunks_links_chunk_source() {
        let store = MemoryStore::open_in_memory().unwrap();
        let lineage_store = MemoryLineageStore::new(store.db());
        let canonical = lineage_store
            .ensure_canonical(
                "agent-1",
                "memory",
                "Use section based consolidation for memory",
            )
            .await
            .unwrap();
        {
            let db = store.db();
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO chunks (id, path, source, start_line, end_line, hash, model, text, embedding, updated_at, access_count, agent_id)
                 VALUES (?1, ?2, ?3, 1, 3, 'hash-1', 'stub', ?4, '', 0, 0, ?5)",
                params![
                    "chunk-1",
                    "MEMORY.md",
                    "long_term",
                    "Use section based consolidation for memory with retained items",
                    "agent-1"
                ],
            )
            .unwrap();
        }

        let attached = lineage_store
            .attach_matching_chunks(
                "agent-1",
                &canonical.canonical_id,
                "MEMORY.md",
                "Use section based consolidation for memory",
                "promoted",
            )
            .await
            .unwrap();

        let links = lineage_store
            .get_links_for_source("agent-1", "chunk", "chunk-1")
            .await
            .unwrap();
        assert_eq!(attached, 1);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].canonical_id, canonical.canonical_id);
        assert_eq!(links[0].relation, "promoted");
    }

    #[tokio::test]
    async fn attach_matching_chunks_by_prefix_links_multiple_session_chunks() {
        let store = MemoryStore::open_in_memory().unwrap();
        let lineage_store = MemoryLineageStore::new(store.db());
        let canonical = lineage_store
            .ensure_canonical(
                "agent-1",
                "daily",
                "Use section-based consolidation for memory",
            )
            .await
            .unwrap();
        {
            let db = store.db();
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO chunks (id, path, source, start_line, end_line, hash, model, text, embedding, updated_at, access_count, agent_id)
                 VALUES (?1, ?2, ?3, 1, 2, 'hash-1', 'stub', ?4, '', 0, 0, ?5)",
                params![
                    "chunk-1",
                    "sessions/session-1#turn:1",
                    "session",
                    "We should use section-based consolidation for memory",
                    "agent-1"
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO chunks (id, path, source, start_line, end_line, hash, model, text, embedding, updated_at, access_count, agent_id)
                 VALUES (?1, ?2, ?3, 3, 4, 'hash-2', 'stub', ?4, '', 0, 0, ?5)",
                params![
                    "chunk-2",
                    "sessions/session-1#turn:2",
                    "session",
                    "Another note that should not match",
                    "agent-1"
                ],
            )
            .unwrap();
        }

        let attached = lineage_store
            .attach_matching_chunks_by_prefix(
                "agent-1",
                &canonical.canonical_id,
                "sessions/session-1#",
                "Use section-based consolidation for memory",
                "raw",
            )
            .await
            .unwrap();

        let links = lineage_store
            .get_links_for_source("agent-1", "chunk", "chunk-1")
            .await
            .unwrap();
        assert_eq!(attached, 1);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].canonical_id, canonical.canonical_id);
        assert_eq!(links[0].relation, "raw");
    }

    #[tokio::test]
    async fn attach_matching_chunks_in_section_falls_back_to_section_heading() {
        let store = MemoryStore::open_in_memory().unwrap();
        let lineage_store = MemoryLineageStore::new(store.db());
        let canonical = lineage_store
            .ensure_canonical("agent-1", "memory", "Clawhive adopts layered memory")
            .await
            .unwrap();
        {
            let db = store.db();
            let conn = db.lock().unwrap();
            conn.execute(
                "INSERT INTO chunks (id, path, source, start_line, end_line, hash, model, text, embedding, updated_at, access_count, agent_id)
                 VALUES (?1, ?2, ?3, 1, 8, 'hash-1', 'stub', ?4, '', 0, 0, ?5)",
                params![
                    "chunk-1",
                    "MEMORY.md",
                    "long_term",
                    "## 长期项目主线\n- Clawhive 是长期工程主线\n- 记忆系统采用分层架构\n",
                    "agent-1"
                ],
            )
            .unwrap();
        }

        let attached = lineage_store
            .attach_matching_chunks_in_section(
                "agent-1",
                &canonical.canonical_id,
                "MEMORY.md",
                "长期项目主线",
                "Clawhive adopts layered memory",
                "promoted",
            )
            .await
            .unwrap();

        let links = lineage_store
            .get_links_for_source("agent-1", "chunk", "chunk-1")
            .await
            .unwrap();
        assert_eq!(attached, 1);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].canonical_id, canonical.canonical_id);
        assert_eq!(links[0].relation, "promoted");
    }

    #[tokio::test]
    async fn get_superseding_canonical_ids_returns_newer_links() {
        let store = MemoryStore::open_in_memory().unwrap();
        let lineage_store = MemoryLineageStore::new(store.db());
        let old = lineage_store
            .ensure_canonical("agent-1", "memory", "old memory text")
            .await
            .unwrap();
        let new = lineage_store
            .ensure_canonical("agent-1", "memory", "new memory text")
            .await
            .unwrap();

        lineage_store
            .attach_source(
                "agent-1",
                &new.canonical_id,
                "canonical",
                &old.canonical_id,
                "supersedes",
            )
            .await
            .unwrap();

        let superseding = lineage_store
            .get_superseding_canonical_ids(
                "agent-1",
                &[old.canonical_id.clone(), "missing".to_string()],
            )
            .await
            .unwrap();

        assert_eq!(
            superseding.get(&old.canonical_id),
            Some(&vec![new.canonical_id])
        );
        assert!(!superseding.contains_key("missing"));
    }

    #[tokio::test]
    async fn find_canonical_by_summary_returns_exact_memory_match() {
        let store = MemoryStore::open_in_memory().unwrap();
        let lineage_store = MemoryLineageStore::new(store.db());
        let canonical = lineage_store
            .ensure_canonical_with_key(
                "agent-1",
                "memory",
                Some("memory-consolidation"),
                "Use section-based consolidation for memory",
            )
            .await
            .unwrap();

        let found = lineage_store
            .find_canonical_by_summary(
                "agent-1",
                "memory",
                "Use section-based consolidation for memory",
            )
            .await
            .unwrap()
            .expect("canonical should exist");

        assert_eq!(found.canonical_id, canonical.canonical_id);
    }
}
