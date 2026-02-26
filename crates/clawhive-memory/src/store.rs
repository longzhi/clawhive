use crate::migrations::run_migrations;
use crate::models::{Concept, ConceptStatus, ConceptType, Episode, Link, LinkRelation};
use anyhow::{anyhow, Result};
use chrono::{DateTime, TimeDelta, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use tokio::task;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub session_key: String,
    pub agent_id: String,
    pub created_at: DateTime<Utc>,
    pub last_active: DateTime<Utc>,
    pub ttl_seconds: i64,
}

#[derive(Debug, Clone)]
pub struct MemoryContext {
    pub recent_episodes: Vec<Episode>,
    pub relevant_episodes: Vec<Episode>,
    pub active_concepts: Vec<Concept>,
}

impl MemoryContext {
    pub fn to_prompt_text(&self) -> String {
        let mut parts = Vec::new();

        if !self.recent_episodes.is_empty() {
            parts.push("## Recent Conversation".to_string());
            for ep in &self.recent_episodes {
                parts.push(format!(
                    "[{}] {}: {}",
                    ep.ts.format("%m-%d %H:%M"),
                    ep.speaker,
                    ep.text
                ));
            }
        }

        if !self.active_concepts.is_empty() {
            parts.push("\n## Known Facts".to_string());
            for c in &self.active_concepts {
                parts.push(format!(
                    "- [{:?}] {}: {} (confidence: {:.1})",
                    c.concept_type, c.key, c.value, c.confidence
                ));
            }
        }

        parts.join("\n")
    }
}

#[derive(Clone)]
pub struct MemoryStore {
    db: Arc<Mutex<Connection>>,
}

/// Initialize sqlite-vec extension. Must be called before Connection::open().
fn init_sqlite_vec() {
    use rusqlite::ffi::{sqlite3, sqlite3_api_routines, sqlite3_auto_extension};

    type Sqlite3AutoExtFn =
        unsafe extern "C" fn(*mut sqlite3, *mut *mut i8, *const sqlite3_api_routines) -> i32;

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

    pub fn db(&self) -> Arc<Mutex<Connection>> {
        Arc::clone(&self.db)
    }

    pub async fn insert_episode(&self, episode: Episode) -> Result<()> {
        let db = Arc::clone(&self.db);
        task::spawn_blocking(move || {
            let tags = serde_json::to_string(&episode.tags)?;
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            conn.execute(
                r#"
                INSERT INTO episodes (
                    id, ts, session_id, speaker, text, tags, importance, context_hash, source_ref
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                "#,
                params![
                    episode.id.to_string(),
                    episode.ts.to_rfc3339(),
                    episode.session_id,
                    episode.speaker,
                    episode.text,
                    tags,
                    episode.importance,
                    episode.context_hash,
                    episode.source_ref,
                ],
            )?;
            Ok::<(), anyhow::Error>(())
        })
        .await??;

        Ok(())
    }

    pub async fn recent_episodes(&self, session_id: &str, limit: usize) -> Result<Vec<Episode>> {
        let db = Arc::clone(&self.db);
        let session_id = session_id.to_owned();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let mut stmt = conn.prepare(
                r#"
                SELECT id, ts, session_id, speaker, text, tags, importance, context_hash, source_ref
                FROM episodes
                WHERE session_id = ?1
                ORDER BY ts DESC
                LIMIT ?2
                "#,
            )?;
            let rows = stmt.query_map(params![session_id, limit as i64], row_to_episode)?;
            let mut episodes = Vec::new();
            for row in rows {
                episodes.push(row?);
            }
            Ok::<Vec<Episode>, anyhow::Error>(episodes)
        })
        .await?
    }

    pub async fn search_episodes(
        &self,
        query: &str,
        days: i64,
        limit: usize,
    ) -> Result<Vec<Episode>> {
        let db = Arc::clone(&self.db);
        let query_like = format!("%{query}%");
        let delta =
            TimeDelta::try_days(days).ok_or_else(|| anyhow!("invalid days value: {days}"))?;
        let cutoff = (Utc::now() - delta).to_rfc3339();

        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let mut stmt = conn.prepare(
                r#"
                SELECT id, ts, session_id, speaker, text, tags, importance, context_hash, source_ref
                FROM episodes
                WHERE text LIKE ?1 AND ts >= ?2
                ORDER BY ts DESC
                LIMIT ?3
                "#,
            )?;
            let rows = stmt.query_map(params![query_like, cutoff, limit as i64], row_to_episode)?;
            let mut episodes = Vec::new();
            for row in rows {
                episodes.push(row?);
            }
            Ok::<Vec<Episode>, anyhow::Error>(episodes)
        })
        .await?
    }

    pub async fn upsert_concept(&self, concept: Concept) -> Result<()> {
        let db = Arc::clone(&self.db);
        task::spawn_blocking(move || {
            let evidence = serde_json::to_string(&concept.evidence)?;
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            conn.execute(
                r#"
                INSERT INTO concepts (
                    id, concept_type, key, value, confidence, evidence, first_seen, last_verified, status
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                ON CONFLICT(key) DO UPDATE SET
                    id = excluded.id,
                    concept_type = excluded.concept_type,
                    value = excluded.value,
                    confidence = excluded.confidence,
                    evidence = excluded.evidence,
                    last_verified = excluded.last_verified,
                    status = excluded.status
                "#,
                params![
                    concept.id.to_string(),
                    concept_type_as_str(&concept.concept_type),
                    concept.key,
                    concept.value,
                    concept.confidence,
                    evidence,
                    concept.first_seen.to_rfc3339(),
                    concept.last_verified.to_rfc3339(),
                    concept_status_as_str(&concept.status),
                ],
            )?;
            Ok::<(), anyhow::Error>(())
        })
        .await??;

        Ok(())
    }

    pub async fn get_concepts_by_type(&self, concept_type: ConceptType) -> Result<Vec<Concept>> {
        let db = Arc::clone(&self.db);
        let concept_type = concept_type_as_str(&concept_type).to_owned();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let mut stmt = conn.prepare(
                r#"
                SELECT id, concept_type, key, value, confidence, evidence, first_seen, last_verified, status
                FROM concepts
                WHERE concept_type = ?1
                ORDER BY last_verified DESC
                "#,
            )?;
            let rows = stmt.query_map(params![concept_type], row_to_concept)?;
            let mut concepts = Vec::new();
            for row in rows {
                concepts.push(row?);
            }
            Ok::<Vec<Concept>, anyhow::Error>(concepts)
        })
        .await?
    }

    pub async fn get_active_concepts(&self) -> Result<Vec<Concept>> {
        let db = Arc::clone(&self.db);
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let mut stmt = conn.prepare(
                r#"
                SELECT id, concept_type, key, value, confidence, evidence, first_seen, last_verified, status
                FROM concepts
                WHERE status = 'Active'
                ORDER BY last_verified DESC
                "#,
            )?;
            let rows = stmt.query_map([], row_to_concept)?;
            let mut concepts = Vec::new();
            for row in rows {
                concepts.push(row?);
            }
            Ok::<Vec<Concept>, anyhow::Error>(concepts)
        })
        .await?
    }

    pub async fn find_concept_by_key(&self, key: &str) -> Result<Option<Concept>> {
        let db = Arc::clone(&self.db);
        let key = key.to_owned();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let mut stmt = conn.prepare(
                r#"
                SELECT id, concept_type, key, value, confidence, evidence, first_seen, last_verified, status
                FROM concepts
                WHERE key = ?1
                LIMIT 1
                "#,
            )?;
            let mut rows = stmt.query(params![key])?;
            if let Some(row) = rows.next()? {
                return Ok::<Option<Concept>, anyhow::Error>(Some(row_to_concept(row)?));
            }
            Ok::<Option<Concept>, anyhow::Error>(None)
        })
        .await?
    }

    pub async fn insert_link(&self, link: Link) -> Result<()> {
        let db = Arc::clone(&self.db);
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            conn.execute(
                r#"
                INSERT INTO links (id, episode_id, concept_id, relation, created_at)
                VALUES (?1, ?2, ?3, ?4, ?5)
                "#,
                params![
                    link.id.to_string(),
                    link.episode_id.to_string(),
                    link.concept_id.to_string(),
                    link_relation_as_str(&link.relation),
                    link.created_at.to_rfc3339(),
                ],
            )?;
            Ok::<(), anyhow::Error>(())
        })
        .await??;

        Ok(())
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
                SELECT session_key, agent_id, created_at, last_active, ttl_seconds
                FROM sessions
                WHERE session_key = ?1
                LIMIT 1
                "#,
            )?;
            let mut rows = stmt.query(params![key])?;
            if let Some(row) = rows.next()? {
                let created_at_raw: String = row.get(2)?;
                let last_active_raw: String = row.get(3)?;
                let session = SessionRecord {
                    session_key: row.get(0)?,
                    agent_id: row.get(1)?,
                    created_at: parse_datetime_sql(&created_at_raw)?,
                    last_active: parse_datetime_sql(&last_active_raw)?,
                    ttl_seconds: row.get(4)?,
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
                INSERT INTO sessions (session_key, agent_id, created_at, last_active, ttl_seconds)
                VALUES (?1, ?2, ?3, ?4, ?5)
                ON CONFLICT(session_key) DO UPDATE SET
                    agent_id = excluded.agent_id,
                    created_at = excluded.created_at,
                    last_active = excluded.last_active,
                    ttl_seconds = excluded.ttl_seconds
                "#,
                params![
                    session.session_key,
                    session.agent_id,
                    session.created_at.to_rfc3339(),
                    session.last_active.to_rfc3339(),
                    session.ttl_seconds,
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

    pub async fn retrieve_context(
        &self,
        session_id: &str,
        query: &str,
        episode_limit: usize,
        concept_limit: usize,
    ) -> Result<MemoryContext> {
        let recent = self.recent_episodes(session_id, episode_limit).await?;
        let searched = if !query.is_empty() {
            self.search_episodes(query, 7, episode_limit).await?
        } else {
            vec![]
        };
        let concepts = self.get_active_concepts().await?;

        Ok(MemoryContext {
            recent_episodes: recent,
            relevant_episodes: searched,
            active_concepts: concepts.into_iter().take(concept_limit).collect(),
        })
    }

    pub async fn mark_stale_concepts(&self, days_inactive: i64) -> Result<usize> {
        let db = Arc::clone(&self.db);
        let delta = TimeDelta::try_days(days_inactive)
            .ok_or_else(|| anyhow!("invalid days value: {days_inactive}"))?;
        let cutoff = (Utc::now() - delta).to_rfc3339();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let affected = conn.execute(
                "UPDATE concepts SET status = 'Stale' WHERE status = 'Active' AND last_verified < ?1",
                params![cutoff],
            )?;
            Ok::<usize, anyhow::Error>(affected)
        })
        .await?
    }

    pub async fn purge_old_episodes(&self, days: i64) -> Result<usize> {
        let db = Arc::clone(&self.db);
        let delta =
            TimeDelta::try_days(days).ok_or_else(|| anyhow!("invalid days value: {days}"))?;
        let cutoff = (Utc::now() - delta).to_rfc3339();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let affected = conn.execute(
                "DELETE FROM episodes WHERE ts < ?1 AND importance < 0.3",
                params![cutoff],
            )?;
            Ok::<usize, anyhow::Error>(affected)
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
        let now = Utc::now().timestamp();

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
}

fn concept_type_as_str(v: &ConceptType) -> &'static str {
    match v {
        ConceptType::Fact => "Fact",
        ConceptType::Preference => "Preference",
        ConceptType::Rule => "Rule",
        ConceptType::Entity => "Entity",
        ConceptType::TaskState => "TaskState",
    }
}

fn concept_status_as_str(v: &ConceptStatus) -> &'static str {
    match v {
        ConceptStatus::Active => "Active",
        ConceptStatus::Stale => "Stale",
        ConceptStatus::Conflicted => "Conflicted",
    }
}

fn link_relation_as_str(v: &LinkRelation) -> &'static str {
    match v {
        LinkRelation::Supports => "Supports",
        LinkRelation::Contradicts => "Contradicts",
        LinkRelation::Updates => "Updates",
    }
}

fn parse_concept_type(s: &str) -> ConceptType {
    match s {
        "Fact" => ConceptType::Fact,
        "Preference" => ConceptType::Preference,
        "Rule" => ConceptType::Rule,
        "Entity" => ConceptType::Entity,
        "TaskState" => ConceptType::TaskState,
        _ => ConceptType::Fact,
    }
}

fn parse_concept_status(s: &str) -> ConceptStatus {
    match s {
        "Active" => ConceptStatus::Active,
        "Stale" => ConceptStatus::Stale,
        "Conflicted" => ConceptStatus::Conflicted,
        _ => ConceptStatus::Stale,
    }
}

fn parse_datetime_sql(raw: &str) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })
}

fn parse_uuid_sql(raw: &str) -> rusqlite::Result<Uuid> {
    Uuid::parse_str(raw).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn row_to_episode(row: &Row<'_>) -> rusqlite::Result<Episode> {
    let id_raw: String = row.get(0)?;
    let ts_raw: String = row.get(1)?;
    let tags_raw: String = row.get(5)?;
    let tags: Vec<String> = serde_json::from_str(&tags_raw).unwrap_or_default();

    Ok(Episode {
        id: parse_uuid_sql(&id_raw)?,
        ts: parse_datetime_sql(&ts_raw)?,
        session_id: row.get(2)?,
        speaker: row.get(3)?,
        text: row.get(4)?,
        tags,
        importance: row.get(6)?,
        context_hash: row.get(7)?,
        source_ref: row.get(8)?,
    })
}

fn row_to_concept(row: &Row<'_>) -> rusqlite::Result<Concept> {
    let id_raw: String = row.get(0)?;
    let concept_type_raw: String = row.get(1)?;
    let evidence_raw: String = row.get(5)?;
    let first_seen_raw: String = row.get(6)?;
    let last_verified_raw: String = row.get(7)?;
    let status_raw: String = row.get(8)?;
    let evidence: Vec<String> = serde_json::from_str(&evidence_raw).unwrap_or_default();

    Ok(Concept {
        id: parse_uuid_sql(&id_raw)?,
        concept_type: parse_concept_type(&concept_type_raw),
        key: row.get(2)?,
        value: row.get(3)?,
        confidence: row.get(4)?,
        evidence,
        first_seen: parse_datetime_sql(&first_seen_raw)?,
        last_verified: parse_datetime_sql(&last_verified_raw)?,
        status: parse_concept_status(&status_raw),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_episode(session_id: &str, text: &str, offset_seconds: i64) -> Episode {
        Episode {
            id: Uuid::new_v4(),
            ts: Utc::now() + TimeDelta::seconds(offset_seconds),
            session_id: session_id.to_owned(),
            speaker: "user".to_owned(),
            text: text.to_owned(),
            tags: vec!["tag-a".to_owned(), "tag-b".to_owned()],
            importance: 0.6,
            context_hash: None,
            source_ref: None,
        }
    }

    fn make_concept(key: &str, value: &str, status: ConceptStatus) -> Concept {
        let now = Utc::now();
        Concept {
            id: Uuid::new_v4(),
            concept_type: ConceptType::Fact,
            key: key.to_owned(),
            value: value.to_owned(),
            confidence: 0.9,
            evidence: vec!["episode:1".to_owned()],
            first_seen: now,
            last_verified: now,
            status,
        }
    }

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
            "INSERT INTO files (path, source, hash, mtime, size) VALUES ('test.md', 'memory', 'abc', 1234, 100)",
            [],
        )
        .expect("insert files");

        db.execute(
            "INSERT INTO chunks (id, path, source, start_line, end_line, hash, model, text, embedding, updated_at) VALUES ('c1', 'test.md', 'memory', 1, 10, 'h1', 'openai', 'hello world', '', 1234)",
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
            "INSERT INTO embedding_cache (provider, model, provider_key, hash, embedding, dims, updated_at) VALUES ('openai', 'text-embedding-3-small', 'key1', 'hash1', '[]', 1536, 1234)",
            [],
        )
        .expect("insert embedding_cache");
    }

    #[tokio::test]
    async fn insert_and_recent_episodes() {
        let store = MemoryStore::open_in_memory().expect("store");
        let episode = make_episode("s1", "hello memory", 0);
        let expected_id = episode.id;
        store.insert_episode(episode).await.expect("insert");

        let episodes = store.recent_episodes("s1", 10).await.expect("recent");
        assert_eq!(episodes.len(), 1);
        assert_eq!(episodes[0].id, expected_id);
    }

    #[tokio::test]
    async fn recent_episodes_limit_and_order() {
        let store = MemoryStore::open_in_memory().expect("store");
        store
            .insert_episode(make_episode("s1", "first", -10))
            .await
            .expect("insert first");
        store
            .insert_episode(make_episode("s1", "second", -5))
            .await
            .expect("insert second");
        store
            .insert_episode(make_episode("s1", "third", 0))
            .await
            .expect("insert third");

        let episodes = store.recent_episodes("s1", 2).await.expect("recent");
        assert_eq!(episodes.len(), 2);
        assert_eq!(episodes[0].text, "third");
        assert_eq!(episodes[1].text, "second");
    }

    #[tokio::test]
    async fn search_episodes_by_text() {
        let store = MemoryStore::open_in_memory().expect("store");
        store
            .insert_episode(make_episode("s1", "project alpha planning", 0))
            .await
            .expect("insert alpha");
        store
            .insert_episode(make_episode("s1", "daily random note", 0))
            .await
            .expect("insert random");

        let episodes = store
            .search_episodes("alpha", 30, 10)
            .await
            .expect("search");
        assert_eq!(episodes.len(), 1);
        assert!(episodes[0].text.contains("alpha"));
    }

    #[tokio::test]
    async fn upsert_concept_insert() {
        let store = MemoryStore::open_in_memory().expect("store");
        let concept = make_concept("pref.theme", "light", ConceptStatus::Active);
        store.upsert_concept(concept.clone()).await.expect("upsert");

        let loaded = store
            .find_concept_by_key("pref.theme")
            .await
            .expect("find")
            .expect("exists");
        assert_eq!(loaded.value, "light");
        assert_eq!(loaded.status, ConceptStatus::Active);
    }

    #[tokio::test]
    async fn upsert_concept_update() {
        let store = MemoryStore::open_in_memory().expect("store");
        let mut concept = make_concept("user.locale", "en-US", ConceptStatus::Active);
        store
            .upsert_concept(concept.clone())
            .await
            .expect("upsert first");

        concept.value = "zh-CN".to_owned();
        concept.status = ConceptStatus::Stale;
        concept.last_verified = Utc::now() + TimeDelta::seconds(3);
        store.upsert_concept(concept).await.expect("upsert second");

        let loaded = store
            .find_concept_by_key("user.locale")
            .await
            .expect("find")
            .expect("exists");
        assert_eq!(loaded.value, "zh-CN");
        assert_eq!(loaded.status, ConceptStatus::Stale);
    }

    #[tokio::test]
    async fn get_active_concepts() {
        let store = MemoryStore::open_in_memory().expect("store");
        store
            .upsert_concept(make_concept("active.one", "v1", ConceptStatus::Active))
            .await
            .expect("upsert active");
        store
            .upsert_concept(make_concept("stale.one", "v2", ConceptStatus::Stale))
            .await
            .expect("upsert stale");

        let concepts = store.get_active_concepts().await.expect("active list");
        assert_eq!(concepts.len(), 1);
        assert_eq!(concepts[0].key, "active.one");
    }

    #[tokio::test]
    async fn insert_link() {
        let store = MemoryStore::open_in_memory().expect("store");
        let episode = make_episode("s1", "link me", 0);
        let concept = make_concept("entity.robot", "clawhive", ConceptStatus::Active);
        let episode_id = episode.id;
        let concept_id = concept.id;

        store.insert_episode(episode).await.expect("insert episode");
        store.upsert_concept(concept).await.expect("upsert concept");

        let link = Link {
            id: Uuid::new_v4(),
            episode_id,
            concept_id,
            relation: LinkRelation::Supports,
            created_at: Utc::now(),
        };
        store.insert_link(link).await.expect("insert link");

        let conn = store.db.lock().expect("lock");
        let count: i64 = conn
            .query_row("SELECT COUNT(1) FROM links", [], |row| row.get(0))
            .expect("count links");
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn session_crud() {
        let store = MemoryStore::open_in_memory().expect("store");
        let missing = store.get_session("abc").await.expect("get missing");
        assert!(missing.is_none());

        let now = Utc::now();
        let rec = SessionRecord {
            session_key: "abc".to_owned(),
            agent_id: "agent-1".to_owned(),
            created_at: now,
            last_active: now,
            ttl_seconds: 3600,
        };

        store.upsert_session(rec).await.expect("upsert session");
        let loaded = store
            .get_session("abc")
            .await
            .expect("get session")
            .expect("session exists");

        assert_eq!(loaded.session_key, "abc");
        assert_eq!(loaded.agent_id, "agent-1");
        assert_eq!(loaded.ttl_seconds, 3600);
    }

    #[tokio::test]
    async fn delete_session_removes_existing_record() {
        let store = MemoryStore::open_in_memory().expect("store");
        let now = Utc::now();
        let rec = SessionRecord {
            session_key: "abc".to_owned(),
            agent_id: "agent-1".to_owned(),
            created_at: now,
            last_active: now,
            ttl_seconds: 3600,
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
    async fn mark_stale_concepts_marks_old_concepts() {
        let store = MemoryStore::open_in_memory().expect("store");
        let mut old = make_concept("old.concept", "old", ConceptStatus::Active);
        let mut fresh = make_concept("fresh.concept", "fresh", ConceptStatus::Active);
        old.last_verified = Utc::now() - TimeDelta::days(40);
        fresh.last_verified = Utc::now() - TimeDelta::days(2);

        store.upsert_concept(old).await.expect("upsert old");
        store.upsert_concept(fresh).await.expect("upsert fresh");

        let affected = store.mark_stale_concepts(30).await.expect("mark stale");
        assert_eq!(affected, 1);

        let old_loaded = store
            .find_concept_by_key("old.concept")
            .await
            .expect("find old")
            .expect("old exists");
        let fresh_loaded = store
            .find_concept_by_key("fresh.concept")
            .await
            .expect("find fresh")
            .expect("fresh exists");

        assert_eq!(old_loaded.status, ConceptStatus::Stale);
        assert_eq!(fresh_loaded.status, ConceptStatus::Active);
    }

    #[tokio::test]
    async fn purge_old_episodes_deletes_low_importance_old_episodes() {
        let store = MemoryStore::open_in_memory().expect("store");
        let mut old_low = make_episode("s1", "old low", 0);
        old_low.ts = Utc::now() - TimeDelta::days(120);
        old_low.importance = 0.1;

        store.insert_episode(old_low).await.expect("insert old low");

        let purged = store.purge_old_episodes(90).await.expect("purge old");
        assert_eq!(purged, 1);

        let remained = store
            .search_episodes("old low", 365, 10)
            .await
            .expect("search");
        assert!(remained.is_empty());
    }

    #[tokio::test]
    async fn purge_old_episodes_keeps_high_importance_episodes() {
        let store = MemoryStore::open_in_memory().expect("store");
        let mut old_high = make_episode("s1", "old high", 0);
        old_high.ts = Utc::now() - TimeDelta::days(120);
        old_high.importance = 0.95;

        store
            .insert_episode(old_high)
            .await
            .expect("insert old high");

        let purged = store.purge_old_episodes(90).await.expect("purge old");
        assert_eq!(purged, 0);

        let remained = store
            .search_episodes("old high", 365, 10)
            .await
            .expect("search");
        assert_eq!(remained.len(), 1);
    }

    #[tokio::test]
    async fn retrieve_context_empty_db() {
        let store = MemoryStore::open_in_memory().unwrap();
        let ctx = store
            .retrieve_context("session:1", "", 10, 20)
            .await
            .unwrap();
        assert!(ctx.recent_episodes.is_empty());
        assert!(ctx.relevant_episodes.is_empty());
        assert!(ctx.active_concepts.is_empty());
    }

    #[tokio::test]
    async fn retrieve_context_with_episodes() {
        let store = MemoryStore::open_in_memory().unwrap();
        let ep = Episode {
            id: Uuid::new_v4(),
            ts: Utc::now(),
            session_id: "session:1".into(),
            speaker: "user".into(),
            text: "hello world".into(),
            tags: vec![],
            importance: 0.5,
            context_hash: None,
            source_ref: None,
        };
        store.insert_episode(ep).await.unwrap();
        let ctx = store
            .retrieve_context("session:1", "", 10, 20)
            .await
            .unwrap();
        assert_eq!(ctx.recent_episodes.len(), 1);
    }

    #[test]
    fn memory_context_to_prompt_text_format() {
        let ctx = MemoryContext {
            recent_episodes: vec![],
            relevant_episodes: vec![],
            active_concepts: vec![],
        };
        assert!(ctx.to_prompt_text().is_empty());
    }

    #[test]
    fn memory_context_to_prompt_text_with_data() {
        let ctx = MemoryContext {
            recent_episodes: vec![Episode {
                id: Uuid::new_v4(),
                ts: Utc::now(),
                session_id: "s:1".into(),
                speaker: "user".into(),
                text: "hello bot".into(),
                tags: vec![],
                importance: 0.5,
                context_hash: None,
                source_ref: None,
            }],
            relevant_episodes: vec![],
            active_concepts: vec![Concept {
                id: Uuid::new_v4(),
                concept_type: ConceptType::Preference,
                key: "lang".into(),
                value: "Rust".into(),
                confidence: 0.9,
                evidence: vec![],
                first_seen: Utc::now(),
                last_verified: Utc::now(),
                status: ConceptStatus::Active,
            }],
        };
        let text = ctx.to_prompt_text();
        assert!(text.contains("## Recent Conversation"));
        assert!(text.contains("user: hello bot"));
        assert!(text.contains("## Known Facts"));
        assert!(text.contains("lang"));
        assert!(text.contains("Rust"));
        assert!(text.contains("0.9"));
    }

    #[tokio::test]
    async fn get_concepts_by_type_filters_correctly() {
        let store = MemoryStore::open_in_memory().unwrap();
        store
            .upsert_concept(make_concept("fact.one", "v1", ConceptStatus::Active))
            .await
            .unwrap();

        let mut pref = make_concept("pref.theme", "dark", ConceptStatus::Active);
        pref.concept_type = ConceptType::Preference;
        store.upsert_concept(pref).await.unwrap();

        let facts = store.get_concepts_by_type(ConceptType::Fact).await.unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].key, "fact.one");

        let prefs = store
            .get_concepts_by_type(ConceptType::Preference)
            .await
            .unwrap();
        assert_eq!(prefs.len(), 1);
        assert_eq!(prefs[0].key, "pref.theme");
    }

    #[tokio::test]
    async fn retrieve_context_with_episodes_and_concepts() {
        let store = MemoryStore::open_in_memory().unwrap();
        store
            .insert_episode(make_episode("session:ctx", "context query test", 0))
            .await
            .unwrap();
        store
            .upsert_concept(make_concept("ctx.key", "ctx value", ConceptStatus::Active))
            .await
            .unwrap();

        let ctx = store
            .retrieve_context("session:ctx", "context", 10, 20)
            .await
            .unwrap();
        assert_eq!(ctx.recent_episodes.len(), 1);
        assert!(!ctx.relevant_episodes.is_empty());
        assert_eq!(ctx.active_concepts.len(), 1);
    }

    #[tokio::test]
    async fn search_episodes_respects_time_window() {
        let store = MemoryStore::open_in_memory().unwrap();
        let mut old_ep = make_episode("s1", "old searchable", 0);
        old_ep.ts = Utc::now() - TimeDelta::days(60);
        store.insert_episode(old_ep).await.unwrap();

        let results = store.search_episodes("searchable", 7, 10).await.unwrap();
        assert!(results.is_empty());

        let results = store.search_episodes("searchable", 90, 10).await.unwrap();
        assert_eq!(results.len(), 1);
    }
}
