use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::task;

use crate::chunker::{chunk_markdown, ChunkerConfig};
use crate::dirty_sources::{
    DirtySourceStore, DIRTY_KIND_DAILY_FILE, DIRTY_KIND_EMBEDDING_MODEL, DIRTY_KIND_FACT,
    DIRTY_KIND_MEMORY_FILE, DIRTY_KIND_SCHEMA, DIRTY_KIND_SESSION,
};
use crate::embedding::EmbeddingProvider;
use crate::session::{SessionEntry, SessionReader};

#[derive(Debug, Clone)]
struct SessionIndexUnit {
    path: String,
    content: String,
    change_hash: String,
    turn_start: usize,
    turn_end: usize,
}

#[derive(Clone)]
pub struct SearchIndex {
    db: Arc<Mutex<Connection>>,
    agent_id: String,
    search_config: SearchConfig,
}

#[derive(Debug, Clone)]
pub struct SearchConfig {
    pub vector_weight: f64,
    pub bm25_weight: f64,
    pub decay_half_life_days: u64,
    pub mmr_lambda: f64,
    pub access_boost_factor: f64,
    pub hot_days: u64,
    pub warm_days: u64,
    pub cold_filter: bool,
    pub access_protect_count: u64,
    pub max_results: usize,
    pub min_score: f64,
    pub embedding_cache_ttl_days: u64,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            vector_weight: 0.7,
            bm25_weight: 0.3,
            decay_half_life_days: 30,
            mmr_lambda: 0.7,
            access_boost_factor: 0.2,
            hot_days: 7,
            warm_days: 30,
            cold_filter: true,
            access_protect_count: 5,
            max_results: 6,
            min_score: 0.35,
            embedding_cache_ttl_days: 90,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Temperature {
    Hot,
    Warm,
    Cold,
}

fn classify_temperature(
    last_accessed: Option<&str>,
    access_count: i64,
    hot_days: u64,
    warm_days: u64,
    access_protect_count: u64,
) -> Temperature {
    let days_since = last_accessed
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|time| (chrono::Utc::now() - time.with_timezone(&chrono::Utc)).num_days());

    let days_since = match days_since {
        Some(days_since) => days_since,
        None => return Temperature::Warm,
    };

    if access_count >= access_protect_count as i64 {
        if days_since <= hot_days as i64 {
            return Temperature::Hot;
        }
        return Temperature::Warm;
    }

    if days_since <= hot_days as i64 {
        Temperature::Hot
    } else if days_since <= warm_days as i64 {
        Temperature::Warm
    } else {
        Temperature::Cold
    }
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub chunk_id: String,
    pub path: String,
    pub source: String,
    pub start_line: i64,
    pub end_line: i64,
    pub snippet: String,
    pub text: String,
    pub score: f64,
    pub score_breakdown: Option<ScoreBreakdown>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScoreBreakdown {
    pub vector_score: Option<f64>,
    pub bm25_score: Option<f64>,
    pub fused_score: f64,
    pub temporal_decay: f64,
    pub access_boost: f64,
    pub temperature: String,
    pub final_score: f64,
}

#[derive(Debug, Clone)]
pub struct TimeRange {
    pub from: Option<String>,
    pub to: Option<String>,
}

impl SearchIndex {
    pub fn new(db: Arc<Mutex<Connection>>, agent_id: impl Into<String>) -> Self {
        Self::new_with_config(db, agent_id, SearchConfig::default())
    }

    pub fn new_with_config(
        db: Arc<Mutex<Connection>>,
        agent_id: impl Into<String>,
        search_config: SearchConfig,
    ) -> Self {
        Self {
            db,
            agent_id: agent_id.into(),
            search_config,
        }
    }

    pub fn config(&self) -> &SearchConfig {
        &self.search_config
    }

    pub fn ensure_vec_table(&self, dimensions: usize) -> Result<()> {
        let db = self
            .db
            .lock()
            .map_err(|_| anyhow!("failed to lock sqlite connection"))?;

        let current_dims: Option<String> = db
            .query_row(
                "SELECT value FROM meta WHERE key = 'vec_dimensions'",
                [],
                |r| r.get(0),
            )
            .optional()?;

        let needs_recreate = match current_dims {
            Some(d) => d.parse::<usize>().unwrap_or(0) != dimensions,
            None => true,
        };

        if needs_recreate {
            db.execute_batch("DROP TABLE IF EXISTS chunks_vec;")?;
            db.execute_batch(&format!(
                "CREATE VIRTUAL TABLE chunks_vec USING vec0(chunk_id TEXT PRIMARY KEY, embedding float[{dimensions}]);"
            ))?;
            db.execute(
                "INSERT INTO meta(key, value) VALUES('vec_dimensions', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![dimensions.to_string()],
            )?;
            tracing::info!("Created chunks_vec virtual table with {dimensions} dimensions");
        }

        Ok(())
    }

    pub async fn index_file(
        &self,
        path: &str,
        content: &str,
        source: &str,
        provider: &dyn EmbeddingProvider,
    ) -> Result<usize> {
        let file_hash = {
            let mut hasher = Sha256::new();
            hasher.update(content.as_bytes());
            format!("{:x}", hasher.finalize())
        };

        self.index_content(path, content, source, &file_hash, provider)
            .await
    }

    async fn index_content(
        &self,
        path: &str,
        content: &str,
        source: &str,
        change_hash: &str,
        provider: &dyn EmbeddingProvider,
    ) -> Result<usize> {
        self.ensure_vec_table(provider.dimensions())?;

        let db = Arc::clone(&self.db);
        let path_owned = path.to_owned();
        let agent_id_for_check = self.agent_id.clone();
        let file_hash_for_check = change_hash.to_owned();
        let unchanged = task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let existing: Option<String> = conn
                .query_row(
                    "SELECT hash FROM files WHERE agent_id = ?1 AND path = ?2",
                    params![agent_id_for_check, path_owned],
                    |r| r.get(0),
                )
                .optional()?;
            Ok::<bool, anyhow::Error>(existing.as_deref() == Some(file_hash_for_check.as_str()))
        })
        .await??;

        if unchanged {
            return Ok(0);
        }

        let text_chunks = chunk_markdown(content, &ChunkerConfig::default());
        if text_chunks.is_empty() {
            let db = Arc::clone(&self.db);
            let path_owned = path.to_owned();
            let source_owned = source.to_owned();
            let file_hash_for_write = change_hash.to_owned();
            let now_ts = chrono::Utc::now().timestamp();
            let size = content.len() as i64;
            let model_id = provider.model_id().to_owned();
            let agent_id = self.agent_id.clone();
            task::spawn_blocking(move || {
                let conn = db
                    .lock()
                    .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
                let tx = conn.unchecked_transaction()?;
                tx.execute(
                    "DELETE FROM chunks_fts WHERE path = ?1 AND id IN (SELECT id FROM chunks WHERE path = ?1 AND agent_id = ?2)",
                    params![path_owned, agent_id],
                )?;
                tx.execute(
                    "DELETE FROM chunks_vec WHERE chunk_id IN (SELECT id FROM chunks WHERE path = ?1 AND agent_id = ?2)",
                    params![path_owned, agent_id],
                )?;
                tx.execute(
                    "DELETE FROM chunks WHERE path = ?1 AND agent_id = ?2",
                    params![path_owned, agent_id],
                )?;
                tx.execute(
                    r#"
                    INSERT INTO files(agent_id, path, source, hash, mtime, size)
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                    ON CONFLICT(agent_id, path) DO UPDATE SET
                        source = excluded.source,
                        hash = excluded.hash,
                        mtime = excluded.mtime,
                        size = excluded.size
                    "#,
                    params![agent_id, path_owned, source_owned, file_hash_for_write, now_ts, size],
                )?;
                tx.execute(
                    r#"
                    INSERT INTO meta(key, value) VALUES('last_indexed', ?1)
                    ON CONFLICT(key) DO UPDATE SET value = excluded.value
                    "#,
                    params![now_ts.to_string()],
                )?;
                tx.execute(
                    r#"
                    INSERT INTO meta(key, value) VALUES('embedding_model', ?1)
                    ON CONFLICT(key) DO UPDATE SET value = excluded.value
                    "#,
                    params![model_id],
                )?;
                tx.commit()?;
                Ok::<(), anyhow::Error>(())
            })
            .await??;
            return Ok(0);
        }

        let hash_list = text_chunks
            .iter()
            .map(|chunk| chunk.hash.clone())
            .collect::<Vec<String>>();
        let db = Arc::clone(&self.db);
        let model_id_for_reuse = provider.model_id().to_owned();
        let reused = task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let mut stmt = conn.prepare(
                "SELECT embedding, model FROM chunks WHERE hash = ?1 AND model = ?2 AND embedding <> '' LIMIT 1",
            )?;
            let mut map = std::collections::HashMap::new();
            for hash in hash_list {
                if map.contains_key(&hash) {
                    continue;
                }
                let row = stmt
                    .query_row(params![hash.clone(), model_id_for_reuse.as_str()], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                    })
                    .optional()?;
                if let Some(value) = row {
                    map.insert(hash, value);
                }
            }
            Ok::<std::collections::HashMap<String, (String, String)>, anyhow::Error>(map)
        })
        .await??;

        let mut pending_texts = Vec::new();
        let mut pending_indices = Vec::new();
        let mut embeddings_json: Vec<Option<String>> = vec![None; text_chunks.len()];
        let mut models: Vec<String> = vec![provider.model_id().to_owned(); text_chunks.len()];

        for (idx, chunk) in text_chunks.iter().enumerate() {
            if let Some((embedding, model)) = reused.get(&chunk.hash) {
                embeddings_json[idx] = Some(embedding.clone());
                models[idx] = model.clone();
            } else {
                pending_indices.push(idx);
                pending_texts.push(chunk.text.clone());
            }
        }

        if !pending_texts.is_empty() {
            let embedded = provider.embed(&pending_texts).await?;
            if embedded.embeddings.len() != pending_indices.len() {
                return Err(anyhow!(
                    "embedding count mismatch: expected {}, got {}",
                    pending_indices.len(),
                    embedded.embeddings.len()
                ));
            }
            for (i, embedding) in embedded.embeddings.iter().enumerate() {
                let idx = pending_indices[i];
                embeddings_json[idx] = Some(embedding_to_json(embedding));
                models[idx] = embedded.model.clone();
            }
        }

        let now_ts = chrono::Utc::now().timestamp();
        let now_rfc = chrono::Utc::now().to_rfc3339();
        let size = content.len() as i64;
        let mut rows = Vec::with_capacity(text_chunks.len());
        for (idx, chunk) in text_chunks.iter().enumerate() {
            let prefix_len = chunk.hash.len().min(8);
            let chunk_id = if self.agent_id.is_empty() {
                format!(
                    "{}:{}-{}:{}",
                    path,
                    chunk.start_line,
                    chunk.end_line,
                    &chunk.hash[..prefix_len]
                )
            } else {
                format!(
                    "{}:{}:{}-{}:{}",
                    self.agent_id,
                    path,
                    chunk.start_line,
                    chunk.end_line,
                    &chunk.hash[..prefix_len]
                )
            };
            rows.push((
                chunk_id,
                chunk.start_line as i64,
                chunk.end_line as i64,
                chunk.hash.clone(),
                models[idx].clone(),
                chunk.text.clone(),
                embeddings_json[idx].clone().unwrap_or_default(),
            ));
        }

        let db = Arc::clone(&self.db);
        let path_owned = path.to_owned();
        let source_owned = source.to_owned();
        let file_hash_for_write = change_hash.to_owned();
        let model_id = provider.model_id().to_owned();
        let agent_id = self.agent_id.clone();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let tx = conn.unchecked_transaction()?;

            tx.execute(
                "DELETE FROM chunks_fts WHERE path = ?1 AND id IN (SELECT id FROM chunks WHERE path = ?1 AND agent_id = ?2)",
                params![path_owned, agent_id],
            )?;
            tx.execute(
                "DELETE FROM chunks_vec WHERE chunk_id IN (SELECT id FROM chunks WHERE path = ?1 AND agent_id = ?2)",
                params![path_owned, agent_id],
            )?;
            tx.execute(
                "DELETE FROM chunks WHERE path = ?1 AND agent_id = ?2",
                params![path_owned, agent_id],
            )?;

            for (chunk_id, start_line, end_line, hash, model, text, embedding) in rows {
                tx.execute(
                    r#"
                    INSERT INTO chunks(
                        id, path, source, start_line, end_line, hash, model, text, embedding, updated_at, last_accessed, agent_id
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                    "#,
                    params![
                        chunk_id,
                        path_owned,
                        source_owned,
                        start_line,
                        end_line,
                        hash,
                        model,
                        text,
                        embedding,
                        now_rfc,
                        now_rfc,
                        agent_id
                    ],
                )?;
                tx.execute(
                    r#"
                    INSERT INTO chunks_fts(text, id, path, source, model, start_line, end_line)
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                    "#,
                    params![
                        text,
                        chunk_id,
                        path_owned,
                        source_owned,
                        model,
                        start_line,
                        end_line
                    ],
                )?;
                tx.execute(
                    "INSERT OR REPLACE INTO chunks_vec(chunk_id, embedding) VALUES (?1, ?2)",
                    params![chunk_id, embedding],
                )?;
            }

            tx.execute(
                r#"
                INSERT INTO files(agent_id, path, source, hash, mtime, size)
                VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                ON CONFLICT(agent_id, path) DO UPDATE SET
                    source = excluded.source,
                    hash = excluded.hash,
                    mtime = excluded.mtime,
                    size = excluded.size
                "#,
                params![agent_id, path_owned, source_owned, file_hash_for_write, now_ts, size],
            )?;
            tx.execute(
                r#"
                INSERT INTO meta(key, value) VALUES('last_indexed', ?1)
                ON CONFLICT(key) DO UPDATE SET value = excluded.value
                "#,
                params![now_ts.to_string()],
            )?;
            tx.execute(
                r#"
                INSERT INTO meta(key, value) VALUES('embedding_model', ?1)
                ON CONFLICT(key) DO UPDATE SET value = excluded.value
                "#,
                params![model_id],
            )?;
            tx.commit()?;

            Ok::<(), anyhow::Error>(())
        })
        .await??;

        Ok(text_chunks.len())
    }

    pub async fn index_session(
        &self,
        session_id: &str,
        reader: &SessionReader,
        provider: &dyn EmbeddingProvider,
    ) -> Result<usize> {
        let units = self.load_session_index_units(session_id, reader).await?;
        self.index_session_units(session_id, units, provider)
            .await
            .map(|(count, _)| count)
    }

    pub async fn index_sessions(
        &self,
        reader: &SessionReader,
        provider: &dyn EmbeddingProvider,
    ) -> Result<usize> {
        let sessions = reader.list_sessions().await?;
        let mut total = 0;
        let mut active_paths = std::collections::HashSet::new();

        for session_id in &sessions {
            let units = match self.load_session_index_units(session_id, reader).await {
                Ok(units) => units,
                Err(error) => {
                    tracing::warn!(session_id = %session_id, %error, "failed to read session for indexing");
                    continue;
                }
            };

            match self.index_session_units(session_id, units, provider).await {
                Ok((count, session_paths)) => {
                    total += count;
                    active_paths.extend(session_paths);
                }
                Err(error) => {
                    tracing::warn!(session_id = %session_id, %error, "failed to index session");
                }
            }
        }

        let db = Arc::clone(&self.db);
        let agent_id = self.agent_id.clone();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;

            let mut stmt =
                conn.prepare("SELECT path FROM files WHERE source = 'session' AND agent_id = ?1")?;
            let indexed_paths: Vec<String> = stmt
                .query_map(params![agent_id.clone()], |row| row.get(0))?
                .filter_map(|r| r.ok())
                .collect();

            for path in indexed_paths {
                if !active_paths.contains(&path) {
                    tracing::info!(path = %path, agent_id = %agent_id, "removing stale session index");
                    let tx = conn.unchecked_transaction()?;
                    tx.execute(
                        "DELETE FROM chunks_fts WHERE id IN (SELECT id FROM chunks WHERE path = ?1 AND agent_id = ?2)",
                        params![&path, &agent_id],
                    )?;
                    tx.execute(
                        "DELETE FROM chunks_vec WHERE chunk_id IN (SELECT id FROM chunks WHERE path = ?1 AND agent_id = ?2)",
                        params![&path, &agent_id],
                    )?;
                    tx.execute(
                        "DELETE FROM chunks WHERE path = ?1 AND agent_id = ?2",
                        params![&path, &agent_id],
                    )?;
                    tx.execute(
                        "DELETE FROM files WHERE path = ?1 AND agent_id = ?2",
                        params![&path, &agent_id],
                    )?;
                    tx.commit()?;
                }
            }

            Ok::<(), anyhow::Error>(())
        })
        .await??;

        Ok(total)
    }

    async fn load_session_index_units(
        &self,
        session_id: &str,
        reader: &SessionReader,
    ) -> Result<Vec<SessionIndexUnit>> {
        let entries = reader.load_all_entries(session_id).await?;
        Ok(build_session_index_units(session_id, entries))
    }

    async fn index_session_units(
        &self,
        session_id: &str,
        units: Vec<SessionIndexUnit>,
        provider: &dyn EmbeddingProvider,
    ) -> Result<(usize, Vec<String>)> {
        let active_paths = units
            .iter()
            .map(|unit| unit.path.clone())
            .collect::<Vec<_>>();
        let mut total = 0;

        for unit in &units {
            total += self
                .index_content(
                    &unit.path,
                    &unit.content,
                    "session",
                    &unit.change_hash,
                    provider,
                )
                .await?;
        }

        self.cleanup_stale_session_paths(session_id, &active_paths)
            .await?;

        Ok((total, active_paths))
    }

    async fn cleanup_stale_session_paths(
        &self,
        session_id: &str,
        active_paths: &[String],
    ) -> Result<()> {
        let db = Arc::clone(&self.db);
        let agent_id = self.agent_id.clone();
        let base_path = format!("sessions/{session_id}");
        let prefix_path = format!("{base_path}#%");
        let active_paths = active_paths.to_vec();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let mut stmt = conn.prepare(
                "SELECT path FROM files WHERE agent_id = ?1 AND source = 'session' AND (path = ?2 OR path LIKE ?3)",
            )?;
            let indexed_paths: Vec<String> = stmt
                .query_map(params![&agent_id, &base_path, &prefix_path], |row| row.get(0))?
                .filter_map(|r| r.ok())
                .collect();

            for path in indexed_paths {
                if active_paths.contains(&path) {
                    continue;
                }
                let tx = conn.unchecked_transaction()?;
                tx.execute(
                    "DELETE FROM chunks_fts WHERE id IN (SELECT id FROM chunks WHERE path = ?1 AND agent_id = ?2)",
                    params![&path, &agent_id],
                )?;
                tx.execute(
                    "DELETE FROM chunks_vec WHERE chunk_id IN (SELECT id FROM chunks WHERE path = ?1 AND agent_id = ?2)",
                    params![&path, &agent_id],
                )?;
                tx.execute(
                    "DELETE FROM chunks WHERE path = ?1 AND agent_id = ?2",
                    params![&path, &agent_id],
                )?;
                tx.execute(
                    "DELETE FROM files WHERE path = ?1 AND agent_id = ?2",
                    params![&path, &agent_id],
                )?;
                tx.commit()?;
            }

            Ok::<(), anyhow::Error>(())
        })
        .await??;

        Ok(())
    }

    async fn delete_indexed_path(&self, path: &str) -> Result<()> {
        let db = Arc::clone(&self.db);
        let path = path.to_owned();
        let agent_id = self.agent_id.clone();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "DELETE FROM chunks_fts WHERE id IN (SELECT id FROM chunks WHERE path = ?1 AND agent_id = ?2)",
                params![&path, &agent_id],
            )?;
            tx.execute(
                "DELETE FROM chunks_vec WHERE chunk_id IN (SELECT id FROM chunks WHERE path = ?1 AND agent_id = ?2)",
                params![&path, &agent_id],
            )?;
            tx.execute(
                "DELETE FROM chunks WHERE path = ?1 AND agent_id = ?2",
                params![&path, &agent_id],
            )?;
            tx.execute(
                "DELETE FROM files WHERE path = ?1 AND agent_id = ?2",
                params![&path, &agent_id],
            )?;
            tx.commit()?;
            Ok::<(), anyhow::Error>(())
        })
        .await??;
        Ok(())
    }

    pub async fn index_all(
        &self,
        file_store: &crate::file_store::MemoryFileStore,
        reader: &SessionReader,
        provider: &dyn EmbeddingProvider,
    ) -> Result<usize> {
        let mut total = 0;

        let long_term = file_store.read_long_term().await?;
        total += self
            .index_file("MEMORY.md", &long_term, "long_term", provider)
            .await?;

        let daily_files = file_store.list_daily_files().await?;
        for (date, _) in daily_files {
            if let Some(content) = file_store.read_daily(date).await? {
                let path = format!("memory/{}.md", date.format("%Y-%m-%d"));
                total += self.index_file(&path, &content, "daily", provider).await?;
            }
        }

        total += self.index_sessions(reader, provider).await?;

        Ok(total)
    }

    pub async fn index_dirty(
        &self,
        file_store: &crate::file_store::MemoryFileStore,
        reader: &SessionReader,
        provider: &dyn EmbeddingProvider,
        limit: usize,
    ) -> Result<usize> {
        let dirty_store = DirtySourceStore::new(Arc::clone(&self.db));
        let pending = dirty_store
            .list_pending(&self.agent_id, limit.max(1))
            .await?;
        let mut total = 0;

        for item in pending {
            let result: Result<usize> = match item.source_kind.as_str() {
                DIRTY_KIND_MEMORY_FILE => {
                    let content = file_store.read_long_term().await?;
                    self.index_file("MEMORY.md", &content, "long_term", provider)
                        .await
                }
                DIRTY_KIND_DAILY_FILE => {
                    let path = file_store.workspace_dir().join(&item.source_ref);
                    match tokio::fs::read_to_string(&path).await {
                        Ok(content) => {
                            self.index_file(&item.source_ref, &content, "daily", provider)
                                .await
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            self.delete_indexed_path(&item.source_ref).await?;
                            Ok(0)
                        }
                        Err(error) => Err(error.into()),
                    }
                }
                DIRTY_KIND_SESSION => {
                    if reader.session_exists(&item.source_ref).await {
                        self.index_session(&item.source_ref, reader, provider).await
                    } else {
                        self.cleanup_stale_session_paths(&item.source_ref, &[])
                            .await?;
                        Ok(0)
                    }
                }
                DIRTY_KIND_FACT => Ok(0),
                DIRTY_KIND_SCHEMA | DIRTY_KIND_EMBEDDING_MODEL => {
                    self.index_all(file_store, reader, provider).await
                }
                other => Err(anyhow!("unsupported dirty source kind: {other}")),
            };

            match result {
                Ok(count) => {
                    total += count;
                }
                Err(error) => {
                    tracing::warn!(
                        agent_id = %self.agent_id,
                        source_kind = %item.source_kind,
                        source_ref = %item.source_ref,
                        %error,
                        "failed to index dirty source"
                    );
                    return Err(error);
                }
            }
        }

        Ok(total)
    }

    pub async fn process_dirty_sources(
        &self,
        dirty_store: &DirtySourceStore,
        agent_id: &str,
        file_store: &crate::file_store::MemoryFileStore,
        reader: &SessionReader,
        provider: &dyn EmbeddingProvider,
        batch_limit: usize,
    ) -> Result<usize> {
        let pending = dirty_store.list_pending(agent_id, batch_limit).await?;
        if pending.is_empty() {
            return Ok(0);
        }

        match self
            .index_dirty(file_store, reader, provider, batch_limit)
            .await
        {
            Ok(count) => {
                for item in &pending {
                    dirty_store.mark_processed(&item.id).await?;
                }

                tracing::info!(
                    agent_id = %agent_id,
                    pending = pending.len(),
                    indexed = count,
                    "Processed dirty sources"
                );

                Ok(count)
            }
            Err(error) => {
                tracing::warn!(
                    agent_id = %agent_id,
                    pending = pending.len(),
                    %error,
                    "Dirty source indexing failed, items kept pending for retry"
                );
                Err(error)
            }
        }
    }

    pub fn needs_reindex(&self, provider: &dyn EmbeddingProvider) -> Result<bool> {
        let conn = self
            .db
            .lock()
            .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
        let current: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'embedding_model'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        Ok(current.as_deref() != Some(provider.model_id()))
    }

    pub async fn search(
        &self,
        query: &str,
        provider: &dyn EmbeddingProvider,
        max_results: usize,
        min_score: f64,
        time_range: Option<TimeRange>,
    ) -> Result<Vec<SearchResult>> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }

        let target_results = if max_results == 0 {
            self.search_config.max_results
        } else {
            max_results
        };
        let candidate_limit = (target_results.saturating_mul(4)).max(1);
        let use_vectors = provider.is_semantic();

        let mut vector_candidates: Vec<(String, String, String, i64, i64, String, f64)> =
            Vec::new();

        if use_vectors {
            let embedded = provider.embed(&[query.to_owned()]).await?;
            let query_embedding = embedded
                .embeddings
                .first()
                .cloned()
                .ok_or_else(|| anyhow!("embedding provider returned empty query embedding"))?;

            let query_embedding_for_vec = query_embedding.clone();
            let query_embedding_json = embedding_to_json(&query_embedding_for_vec);

            let db = Arc::clone(&self.db);
            let agent_id = self.agent_id.clone();
            vector_candidates = task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;

            let has_vec_table: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='chunks_vec'",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(false);

            if has_vec_table {
                // vec0 virtual tables don't support arbitrary WHERE on joined tables,
                // so we over-fetch and post-filter by agent_id.
                let over_fetch_limit = candidate_limit * 4;
                let mut stmt = conn.prepare(
                    r#"
                    SELECT v.chunk_id, c.path, c.source, c.start_line, c.end_line, c.text, v.distance
                    FROM chunks_vec v
                    JOIN chunks c ON c.id = v.chunk_id
                    WHERE v.embedding MATCH ?1 AND k = ?2
                    "#,
                )?;
                let rows = stmt.query_map(params![query_embedding_json, over_fetch_limit as i64], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, f64>(6)?,
                    ))
                })?;

                // Collect chunk IDs from vec results, then filter by agent_id
                let mut vec_results = Vec::new();
                for row in rows {
                    vec_results.push(row?);
                }

                // Batch-check agent_id for returned chunk IDs
                let chunk_ids: Vec<&str> = vec_results.iter().map(|r| r.0.as_str()).collect();
                let owned_agent_ids = if chunk_ids.is_empty() {
                    std::collections::HashMap::new()
                } else {
                    let placeholders: String = chunk_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                    let sql = format!("SELECT id, agent_id FROM chunks WHERE id IN ({placeholders})");
                    let mut lookup_stmt = conn.prepare(&sql)?;
                    let lookup_rows = lookup_stmt.query_map(rusqlite::params_from_iter(&chunk_ids), |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                    })?;
                    let mut map = std::collections::HashMap::new();
                    for lr in lookup_rows {
                        let (id, aid) = lr?;
                        map.insert(id, aid);
                    }
                    map
                };

                let mut out = Vec::new();
                for (chunk_id, path, source, start_line, end_line, text, distance) in vec_results {
                    if owned_agent_ids.get(&chunk_id) == Some(&agent_id) {
                        let score = (1.0_f64 - distance).max(0.0_f64);
                        out.push((chunk_id, path, source, start_line, end_line, text, score));
                    }
                }
                out.sort_by(|a, b| b.6.total_cmp(&a.6));
                out.truncate(candidate_limit);
                return Ok::<Vec<(String, String, String, i64, i64, String, f64)>, anyhow::Error>(
                    out,
                );
            }

            let mut stmt = conn.prepare(
                "SELECT id, path, source, start_line, end_line, text, embedding FROM chunks WHERE agent_id = ?1",
            )?;
            let rows = stmt.query_map(params![agent_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                ))
            })?;

            let mut out = Vec::new();
            for row in rows {
                let (chunk_id, path, source, start_line, end_line, text, embedding_json) = row?;
                if embedding_json.trim().is_empty() {
                    continue;
                }
                let embedding = json_to_embedding(&embedding_json)?;
                let score = cosine_similarity(&query_embedding, &embedding) as f64;
                out.push((chunk_id, path, source, start_line, end_line, text, score));
            }
            out.sort_by(|a, b| b.6.total_cmp(&a.6));
            out.truncate(candidate_limit);
            Ok::<Vec<(String, String, String, i64, i64, String, f64)>, anyhow::Error>(out)
        })
        .await??;

            // Min-max normalize vector scores
            if !vector_candidates.is_empty() {
                let max_vec = vector_candidates
                    .iter()
                    .map(|c| c.6)
                    .fold(0.0_f64, f64::max);
                if max_vec > 0.0 {
                    for candidate in &mut vector_candidates {
                        candidate.6 /= max_vec;
                    }
                }
            }
        } // end if use_vectors

        let safe_fts_query = build_safe_fts_query(query);
        let has_bm25_query = !safe_fts_query.is_empty();
        let mut bm25_candidates = if !has_bm25_query {
            Vec::new()
        } else {
            let db = Arc::clone(&self.db);
            let agent_id = self.agent_id.clone();
            let safe_fts_query_for_sql = safe_fts_query.clone();
            match task::spawn_blocking(move || {
                let conn = db
                    .lock()
                    .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
                let mut stmt = conn.prepare(
                    r#"
                    SELECT f.id, c.path, c.source, c.start_line, c.end_line, c.text, bm25(chunks_fts) AS rank
                    FROM chunks_fts f
                    JOIN chunks c ON c.id = f.id
                    WHERE chunks_fts MATCH ?1 AND c.agent_id = ?2
                    ORDER BY rank
                    LIMIT ?3
                    "#,
                )?;
                let rows = stmt.query_map(
                    params![safe_fts_query_for_sql, agent_id, candidate_limit as i64],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, i64>(3)?,
                            r.get::<_, i64>(4)?,
                            r.get::<_, String>(5)?,
                            r.get::<_, f64>(6)?,
                        ))
                    },
                )?;

                let mut out = Vec::new();
                for row in rows {
                    let (chunk_id, path, source, start_line, end_line, text, rank) = row?;
                    let bm25_score = 1.0_f64 / (1.0_f64 + (-rank).max(0.0_f64));
                    out.push((
                        chunk_id, path, source, start_line, end_line, text, bm25_score,
                    ));
                }
                Ok::<Vec<(String, String, String, i64, i64, String, f64)>, anyhow::Error>(out)
            })
            .await?
            {
                Ok(candidates) => candidates,
                Err(e) => {
                    tracing::debug!("BM25 search failed (falling back to vector-only): {e}");
                    Vec::new()
                }
            }
        };

        // Min-max normalize BM25 scores
        if !bm25_candidates.is_empty() {
            let max_bm25 = bm25_candidates.iter().map(|c| c.6).fold(0.0_f64, f64::max);
            if max_bm25 > 0.0 {
                for candidate in &mut bm25_candidates {
                    candidate.6 /= max_bm25;
                }
            }
        }

        #[derive(Clone)]
        struct MergeItem {
            chunk_id: String,
            path: String,
            source: String,
            start_line: i64,
            end_line: i64,
            text: String,
            vector_score: f64,
            bm25_score: f64,
        }

        let mut merged = std::collections::HashMap::<String, MergeItem>::new();
        for (chunk_id, path, source, start_line, end_line, text, vector_score) in vector_candidates
        {
            merged.insert(
                chunk_id.clone(),
                MergeItem {
                    chunk_id,
                    path,
                    source,
                    start_line,
                    end_line,
                    text,
                    vector_score,
                    bm25_score: 0.0,
                },
            );
        }
        for (chunk_id, path, source, start_line, end_line, text, bm25_score) in bm25_candidates {
            if let Some(item) = merged.get_mut(&chunk_id) {
                item.bm25_score = bm25_score;
            } else {
                merged.insert(
                    chunk_id.clone(),
                    MergeItem {
                        chunk_id,
                        path,
                        source,
                        start_line,
                        end_line,
                        text,
                        vector_score: 0.0,
                        bm25_score,
                    },
                );
            }
        }

        let mut results = merged
            .into_values()
            .filter(|item| path_matches_time_range(&item.path, time_range.as_ref()))
            .map(|item| SearchResult {
                chunk_id: item.chunk_id,
                path: item.path,
                source: item.source,
                start_line: item.start_line,
                end_line: item.end_line,
                snippet: generate_snippet(&item.text, 200),
                text: item.text,
                score: if use_vectors {
                    (item.vector_score * self.search_config.vector_weight)
                        + (item.bm25_score * self.search_config.bm25_weight)
                } else {
                    item.bm25_score // BM25-only mode
                },
                score_breakdown: Some(ScoreBreakdown {
                    vector_score: use_vectors.then_some(item.vector_score),
                    bm25_score: if has_bm25_query {
                        Some(item.bm25_score)
                    } else {
                        None
                    },
                    fused_score: if use_vectors {
                        (item.vector_score * self.search_config.vector_weight)
                            + (item.bm25_score * self.search_config.bm25_weight)
                    } else {
                        item.bm25_score
                    },
                    temporal_decay: 1.0,
                    access_boost: 1.0,
                    temperature: "cold".to_string(),
                    final_score: if use_vectors {
                        (item.vector_score * self.search_config.vector_weight)
                            + (item.bm25_score * self.search_config.bm25_weight)
                    } else {
                        item.bm25_score
                    },
                }),
            })
            .filter(|item| item.score >= min_score)
            .collect::<Vec<SearchResult>>();

        // --- Temporal Decay ---
        // Boost recent memories, decay older ones (half-life = 30 days)
        let half_life_days = self.search_config.decay_half_life_days as f64;
        let decay_lambda = (2.0_f64).ln() / half_life_days;
        let today = chrono::Utc::now().date_naive();

        for result in &mut results {
            // Extract date from path like "memory/2026-02-25.md"
            let age_days = extract_date_from_path(&result.path)
                .map(|date| (today - date).num_days().max(0) as f64)
                .unwrap_or(0.0); // Non-dated files (MEMORY.md etc) get no decay

            if age_days > 0.0 {
                let decay = (-decay_lambda * age_days).exp();
                result.score *= decay;
                if let Some(breakdown) = result.score_breakdown.as_mut() {
                    breakdown.temporal_decay = decay;
                    breakdown.final_score = result.score;
                }
            }
        }

        {
            let chunk_ids: Vec<String> = results.iter().map(|r| r.chunk_id.clone()).collect();
            if !chunk_ids.is_empty() {
                let db = Arc::clone(&self.db);
                let counts = task::spawn_blocking(
                    move || -> Result<std::collections::HashMap<String, (i64, Option<String>)>> {
                        let conn = db.lock().map_err(|_| anyhow!("lock failed"))?;
                        let placeholders: String =
                            chunk_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                        let sql = format!("SELECT id, access_count, last_accessed FROM chunks WHERE id IN ({placeholders})");
                        let mut stmt = conn.prepare(&sql)?;
                        let mut map = std::collections::HashMap::new();
                        let rows = stmt.query_map(rusqlite::params_from_iter(&chunk_ids), |r| {
                            Ok((
                                r.get::<_, String>(0)?,
                                r.get::<_, i64>(1)?,
                                r.get::<_, Option<String>>(2)?,
                            ))
                        })?;
                        for row in rows {
                            let (id, count, last_accessed) = row?;
                            map.insert(id, (count, last_accessed));
                        }
                        Ok(map)
                    },
                )
                .await;

                if let Ok(Ok(counts)) = counts {
                    for result in &mut results {
                        let (count, last_accessed) =
                            counts.get(&result.chunk_id).cloned().unwrap_or((0, None));
                        if count > 0 {
                            let access_boost = 1.0
                                + (1.0 + count as f64).ln()
                                    * self.search_config.access_boost_factor;
                            result.score *= access_boost;
                            if let Some(breakdown) = result.score_breakdown.as_mut() {
                                breakdown.access_boost = access_boost;
                                breakdown.final_score = result.score;
                            }
                        }

                        let temperature = classify_temperature(
                            last_accessed.as_deref(),
                            count,
                            self.search_config.hot_days,
                            self.search_config.warm_days,
                            self.search_config.access_protect_count,
                        );

                        if temperature == Temperature::Hot {
                            result.score = (result.score + 0.01).min(1.0);
                        }

                        if let Some(breakdown) = result.score_breakdown.as_mut() {
                            breakdown.temperature = match temperature {
                                Temperature::Hot => "hot",
                                Temperature::Warm => "warm",
                                Temperature::Cold => "cold",
                            }
                            .to_string();
                            breakdown.final_score = result.score;
                        }
                    }

                    if self.search_config.cold_filter {
                        results.retain(|result| {
                            let (count, last_accessed) =
                                counts.get(&result.chunk_id).cloned().unwrap_or((0, None));
                            classify_temperature(
                                last_accessed.as_deref(),
                                count,
                                self.search_config.hot_days,
                                self.search_config.warm_days,
                                self.search_config.access_protect_count,
                            ) != Temperature::Cold
                        });
                    }
                }
            }
        }

        results.sort_by(|a, b| b.score.total_cmp(&a.score));

        // --- MMR (Maximal Marginal Relevance) ---
        // Re-rank to reduce redundancy (lambda=0.7: balance relevance + diversity)
        let mmr_lambda = self.search_config.mmr_lambda;
        let mmr_results = mmr_rerank(&results, mmr_lambda, target_results);

        // Bump access_count for returned chunks (fire-and-forget)
        if !mmr_results.is_empty() {
            let db = Arc::clone(&self.db);
            let ids: Vec<String> = mmr_results.iter().map(|r| r.chunk_id.clone()).collect();
            let _ = task::spawn_blocking(move || -> Result<()> {
                let conn = db.lock().map_err(|_| anyhow!("lock failed"))?;
                let placeholders: String = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
                conn.execute(
                    &format!(
                        "UPDATE chunks SET access_count = access_count + 1, last_accessed = ?1 WHERE id IN ({placeholders})"
                    ),
                    rusqlite::params_from_iter(
                        std::iter::once(chrono::Utc::now().to_rfc3339()).chain(ids.clone()),
                    ),
                )?;
                Ok(())
            })
            .await;
        }

        Ok(mmr_results)
    }
}

fn build_session_index_units(
    session_id: &str,
    entries: Vec<SessionEntry>,
) -> Vec<SessionIndexUnit> {
    let mut turns = Vec::new();
    let mut current_lines = Vec::new();
    let mut turn_index: usize = 0;
    let mut last_timestamp_millis = 0_i64;

    let finalize_turn = |units: &mut Vec<SessionIndexUnit>,
                         current_lines: &mut Vec<String>,
                         turn_index: usize,
                         last_timestamp_millis: i64| {
        if current_lines.is_empty() {
            return;
        }
        let content = current_lines.join("\n");
        let change_hash = format!(
            "session_turn:{}:{}:{}:{}",
            turn_index,
            current_lines.len(),
            content.len(),
            last_timestamp_millis
        );
        units.push(SessionIndexUnit {
            path: format!("sessions/{session_id}#turn:{turn_index}"),
            content,
            change_hash,
            turn_start: turn_index,
            turn_end: turn_index,
        });
        current_lines.clear();
    };

    for entry in entries {
        match entry {
            SessionEntry::Session { timestamp, .. } => {
                last_timestamp_millis = timestamp.timestamp_millis();
            }
            SessionEntry::Message {
                timestamp, message, ..
            } if matches!(message.role.as_str(), "user" | "assistant") => {
                last_timestamp_millis = timestamp.timestamp_millis();
                if message.role == "user" {
                    finalize_turn(
                        &mut turns,
                        &mut current_lines,
                        turn_index,
                        last_timestamp_millis,
                    );
                    turn_index += 1;
                } else if current_lines.is_empty() {
                    turn_index += 1;
                }
                current_lines.push(format!("{}: {}", message.role, message.content));
            }
            SessionEntry::Message { timestamp, .. }
            | SessionEntry::ToolCall { timestamp, .. }
            | SessionEntry::ToolResult { timestamp, .. }
            | SessionEntry::Compaction { timestamp, .. }
            | SessionEntry::ModelChange { timestamp, .. } => {
                last_timestamp_millis = timestamp.timestamp_millis();
            }
        }
    }

    finalize_turn(
        &mut turns,
        &mut current_lines,
        turn_index,
        last_timestamp_millis,
    );

    merge_session_turn_units(session_id, turns)
}

fn merge_session_turn_units(
    session_id: &str,
    turns: Vec<SessionIndexUnit>,
) -> Vec<SessionIndexUnit> {
    if turns.len() <= 1 {
        return turns;
    }

    const MAX_TOPIC_WINDOW_TURNS: usize = 3;
    const MAX_TOPIC_WINDOW_CHARS: usize = 1600;

    let mut merged = Vec::new();
    let mut current = turns[0].clone();
    let mut current_tokens = session_topic_tokens(&current.content);

    for next in turns.into_iter().skip(1) {
        let next_tokens = session_topic_tokens(&next.content);
        let can_merge = current.turn_end + 1 == next.turn_start
            && current.turn_end.saturating_sub(current.turn_start) + 1 < MAX_TOPIC_WINDOW_TURNS
            && current.content.len() + next.content.len() + 2 <= MAX_TOPIC_WINDOW_CHARS
            && topics_are_related(&current_tokens, &next_tokens);

        if can_merge {
            current.content = format!("{}\n{}", current.content, next.content);
            current.turn_end = next.turn_end;
            current.path = format!(
                "sessions/{session_id}#turn:{}-{}",
                current.turn_start, current.turn_end
            );
            current.change_hash = format!(
                "session_turn:{}-{}:{}:{}",
                current.turn_start,
                current.turn_end,
                current.content.lines().count(),
                current.content.len()
            );
            current_tokens.extend(next_tokens);
        } else {
            merged.push(current);
            current = next;
            current_tokens = next_tokens;
        }
    }

    merged.push(current);
    merged
}

fn session_topic_tokens(content: &str) -> std::collections::HashSet<String> {
    content
        .lines()
        .filter_map(|line| line.strip_prefix("user: "))
        .flat_map(|line| {
            line.split(|ch: char| !ch.is_alphanumeric())
                .map(str::trim)
                .filter(|token| token.len() >= 3)
                .map(|token| token.to_ascii_lowercase())
                .collect::<Vec<_>>()
        })
        .collect()
}

fn topics_are_related(
    current: &std::collections::HashSet<String>,
    next: &std::collections::HashSet<String>,
) -> bool {
    if current.is_empty() || next.is_empty() {
        return false;
    }

    current.intersection(next).count() >= 2
}

/// Extract a date from a path like "memory/2026-02-25.md"
fn extract_date_from_path(path: &str) -> Option<chrono::NaiveDate> {
    // Match YYYY-MM-DD pattern in the path
    let re_pattern = path
        .rsplit('/')
        .next()
        .unwrap_or(path)
        .trim_end_matches(".md");

    chrono::NaiveDate::parse_from_str(re_pattern, "%Y-%m-%d").ok()
}

fn path_matches_time_range(path: &str, time_range: Option<&TimeRange>) -> bool {
    let Some(range) = time_range else {
        return true;
    };

    if path.ends_with("MEMORY.md") {
        return true;
    }

    if path.starts_with("memory/") {
        let Some(date) = extract_date_from_path(path) else {
            return true;
        };
        return date_in_time_range(date, range);
    }

    true
}

fn date_in_time_range(date: chrono::NaiveDate, range: &TimeRange) -> bool {
    let from = range
        .from
        .as_deref()
        .and_then(|value| parse_time_boundary(value, true));
    let to = range
        .to
        .as_deref()
        .and_then(|value| parse_time_boundary(value, false));

    if let Some(from_date) = from {
        if date < from_date {
            return false;
        }
    }

    if let Some(to_date) = to {
        if date > to_date {
            return false;
        }
    }

    true
}

fn parse_time_boundary(value: &str, is_start: bool) -> Option<chrono::NaiveDate> {
    if let Ok(date) = chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return Some(date);
    }

    let (year, month) = value.split_once('-')?;
    if month.len() != 2 || year.len() != 4 || value.matches('-').count() != 1 {
        return None;
    }

    let year: i32 = year.parse().ok()?;
    let month: u32 = month.parse().ok()?;
    let start = chrono::NaiveDate::from_ymd_opt(year, month, 1)?;
    if is_start {
        return Some(start);
    }

    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let next_start = chrono::NaiveDate::from_ymd_opt(next_year, next_month, 1)?;
    Some(next_start - chrono::Duration::days(1))
}

fn generate_snippet(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }

    let truncated: String = text.chars().take(max_chars).collect();
    if let Some((pos, _)) = truncated
        .char_indices()
        .rfind(|(_, c)| matches!(*c, '.' | '。' | '!' | '?' | '\n'))
    {
        format!("{}...", truncated[..=pos].trim_end())
    } else {
        format!("{}...", truncated.trim_end())
    }
}

fn build_safe_fts_query(query: &str) -> String {
    query
        .split_whitespace()
        .filter_map(|raw_token| {
            let token = raw_token.trim_matches('"').replace(['"', '*', '^'], "");

            if token.is_empty() {
                return None;
            }

            match token.to_ascii_uppercase().as_str() {
                "AND" | "OR" | "NOT" | "NEAR" => None,
                _ => Some(format!("\"{token}\"")),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Jaccard similarity between two texts (tokenized by whitespace)
fn jaccard_similarity(a: &str, b: &str) -> f64 {
    let tokens_a: std::collections::HashSet<&str> = a.split_whitespace().collect();
    let tokens_b: std::collections::HashSet<&str> = b.split_whitespace().collect();

    if tokens_a.is_empty() && tokens_b.is_empty() {
        return 1.0;
    }

    let intersection = tokens_a.intersection(&tokens_b).count() as f64;
    let union = tokens_a.union(&tokens_b).count() as f64;

    if union == 0.0 {
        0.0
    } else {
        intersection / union
    }
}

/// MMR re-ranking: iteratively select results balancing relevance and diversity
fn mmr_rerank(candidates: &[SearchResult], lambda: f64, max_results: usize) -> Vec<SearchResult> {
    if candidates.is_empty() {
        return Vec::new();
    }

    let mut selected: Vec<SearchResult> = Vec::new();
    let mut remaining: Vec<usize> = (0..candidates.len()).collect();

    while selected.len() < max_results && !remaining.is_empty() {
        let mut best_idx = 0;
        let mut best_mmr = f64::NEG_INFINITY;

        for (ri, &ci) in remaining.iter().enumerate() {
            let relevance = candidates[ci].score;

            // Max similarity to any already-selected result
            let max_sim = selected
                .iter()
                .map(|s| jaccard_similarity(&candidates[ci].text, &s.text))
                .fold(0.0_f64, f64::max);

            let mmr_score = lambda * relevance - (1.0 - lambda) * max_sim;

            if mmr_score > best_mmr {
                best_mmr = mmr_score;
                best_idx = ri;
            }
        }

        let chosen = remaining.remove(best_idx);
        selected.push(candidates[chosen].clone());
    }

    selected
}

fn embedding_to_json(embedding: &[f32]) -> String {
    match serde_json::to_string(embedding) {
        Ok(json) => json,
        Err(_) => "[]".to_owned(),
    }
}

fn json_to_embedding(json: &str) -> Result<Vec<f32>> {
    let out = serde_json::from_str::<Vec<f32>>(json)?;
    Ok(out)
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return 0.0;
    }

    let mut dot = 0.0_f32;
    let mut norm_a = 0.0_f32;
    let mut norm_b = 0.0_f32;
    for (x, y) in a.iter().zip(b.iter()) {
        dot += x * y;
        norm_a += x * x;
        norm_b += y * y;
    }

    if norm_a <= f32::EPSILON || norm_b <= f32::EPSILON {
        return 0.0;
    }

    let score = dot / (norm_a.sqrt() * norm_b.sqrt());
    score.clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::{Arc, Mutex};

    use anyhow::Result;
    use async_trait::async_trait;
    use rusqlite::Connection;
    use tempfile::TempDir;

    use crate::embedding::{EmbeddingProvider, EmbeddingResult, StubEmbeddingProvider};
    use crate::file_store::MemoryFileStore;
    use crate::migrations::run_migrations;
    use crate::session::{SessionEntry, SessionReader, SessionWriter};

    #[derive(Clone)]
    struct NamedStubEmbeddingProvider {
        dims: usize,
        model: String,
        value: f32,
    }

    #[derive(Clone)]
    struct KeywordVectorProvider;

    #[async_trait]
    impl EmbeddingProvider for KeywordVectorProvider {
        async fn embed(&self, texts: &[String]) -> Result<EmbeddingResult> {
            let embeddings = texts
                .iter()
                .map(|text| {
                    if text.trim() == "keyword" || text.contains("vector-target") {
                        vec![1.0, 0.0]
                    } else {
                        vec![0.0, 1.0]
                    }
                })
                .collect();

            Ok(EmbeddingResult {
                embeddings,
                model: "keyword-vector".to_string(),
                dimensions: 2,
            })
        }

        fn model_id(&self) -> &str {
            "keyword-vector"
        }

        fn dimensions(&self) -> usize {
            2
        }
    }

    impl NamedStubEmbeddingProvider {
        fn new(dims: usize, model: &str, value: f32) -> Self {
            Self {
                dims,
                model: model.to_string(),
                value,
            }
        }
    }

    #[async_trait]
    impl EmbeddingProvider for NamedStubEmbeddingProvider {
        async fn embed(&self, texts: &[String]) -> Result<EmbeddingResult> {
            Ok(EmbeddingResult {
                embeddings: texts.iter().map(|_| vec![self.value; self.dims]).collect(),
                model: self.model.clone(),
                dimensions: self.dims,
            })
        }

        fn model_id(&self) -> &str {
            &self.model
        }

        fn dimensions(&self) -> usize {
            self.dims
        }

        fn is_semantic(&self) -> bool {
            false
        }
    }

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

    fn test_db() -> Result<Arc<Mutex<Connection>>> {
        init_sqlite_vec();
        let conn = Connection::open_in_memory()?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        run_migrations(&conn)?;
        Ok(Arc::new(Mutex::new(conn)))
    }

    #[tokio::test]
    async fn index_file_creates_chunks() -> Result<()> {
        let db = test_db()?;
        let index = SearchIndex::new(Arc::clone(&db), "test-agent");
        let provider = StubEmbeddingProvider::new(8);

        let count = index
            .index_file(
                "MEMORY.md",
                "# Title\n\nhello world",
                "long_term",
                &provider,
            )
            .await?;

        assert!(count > 0);
        let conn = db.lock().expect("lock");
        let db_count: i64 = conn.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))?;
        assert!(db_count > 0);
        Ok(())
    }

    #[tokio::test]
    async fn index_file_skips_unchanged() -> Result<()> {
        let db = test_db()?;
        let index = SearchIndex::new(Arc::clone(&db), "test-agent");
        let provider = StubEmbeddingProvider::new(8);

        let first = index
            .index_file("MEMORY.md", "same content", "long_term", &provider)
            .await?;
        assert!(first > 0);

        let second = index
            .index_file("MEMORY.md", "same content", "long_term", &provider)
            .await?;
        assert_eq!(second, 0);
        Ok(())
    }

    #[tokio::test]
    async fn index_file_reindexes_changed() -> Result<()> {
        let db = test_db()?;
        let index = SearchIndex::new(Arc::clone(&db), "test-agent");
        let provider = StubEmbeddingProvider::new(8);

        index
            .index_file("MEMORY.md", "# A\n\nfirst", "long_term", &provider)
            .await?;
        let changed = index
            .index_file("MEMORY.md", "# A\n\nsecond changed", "long_term", &provider)
            .await?;
        assert!(changed > 0);

        let conn = db.lock().expect("lock");
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM chunks WHERE path = 'MEMORY.md'",
            [],
            |r| r.get(0),
        )?;
        assert!(count > 0);
        let has_new: i64 = conn.query_row(
            "SELECT COUNT(*) FROM chunks WHERE path = 'MEMORY.md' AND text LIKE '%second changed%'",
            [],
            |r| r.get(0),
        )?;
        assert!(has_new > 0);
        Ok(())
    }

    #[tokio::test]
    async fn index_file_populates_fts() -> Result<()> {
        let db = test_db()?;
        let index = SearchIndex::new(Arc::clone(&db), "test-agent");
        let provider = StubEmbeddingProvider::new(8);

        index
            .index_file(
                "MEMORY.md",
                "# Grocery\n\nBuy banana and milk",
                "long_term",
                &provider,
            )
            .await?;

        let conn = db.lock().expect("lock");
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM chunks_fts WHERE chunks_fts MATCH 'banana'",
            [],
            |r| r.get(0),
        )?;
        assert!(count > 0);
        Ok(())
    }

    #[tokio::test]
    async fn index_file_stores_embeddings() -> Result<()> {
        let db = test_db()?;
        let index = SearchIndex::new(Arc::clone(&db), "test-agent");
        let provider = StubEmbeddingProvider::new(8);

        index
            .index_file(
                "MEMORY.md",
                "# Embedding\n\nstore vectors",
                "long_term",
                &provider,
            )
            .await?;

        let conn = db.lock().expect("lock");
        let embedding: String = conn.query_row(
            "SELECT embedding FROM chunks WHERE path='MEMORY.md' LIMIT 1",
            [],
            |r| r.get(0),
        )?;
        assert!(!embedding.is_empty());
        assert!(embedding.starts_with('['));
        Ok(())
    }

    #[tokio::test]
    async fn reuse_ignores_different_model_embeddings() -> Result<()> {
        let db = test_db()?;
        let index = SearchIndex::new(Arc::clone(&db), "test-agent");
        let first_provider = NamedStubEmbeddingProvider::new(8, "stub-a", 1.0);
        let second_provider = NamedStubEmbeddingProvider::new(8, "stub-b", 2.0);
        let content = "# Shared\n\nchunk text reused across models";

        index
            .index_file("MEMORY.md", content, "long_term", &first_provider)
            .await?;

        {
            let conn = db.lock().expect("lock");
            let original_models: i64 = conn.query_row(
                "SELECT COUNT(*) FROM chunks WHERE path = 'MEMORY.md' AND model = 'stub-a'",
                [],
                |r| r.get(0),
            )?;
            assert!(original_models > 0);
            conn.execute(
                "UPDATE files SET hash = 'force-reindex' WHERE path = 'MEMORY.md'",
                [],
            )?;
        }

        index
            .index_file("MEMORY.md", content, "long_term", &second_provider)
            .await?;

        let conn = db.lock().expect("lock");
        let reused_old_model: i64 = conn.query_row(
            "SELECT COUNT(*) FROM chunks WHERE path = 'MEMORY.md' AND model = 'stub-a'",
            [],
            |r| r.get(0),
        )?;
        let new_model_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM chunks WHERE path = 'MEMORY.md' AND model = 'stub-b'",
            [],
            |r| r.get(0),
        )?;
        let embedding: String = conn.query_row(
            "SELECT embedding FROM chunks WHERE path = 'MEMORY.md' LIMIT 1",
            [],
            |r| r.get(0),
        )?;

        assert_eq!(reused_old_model, 0);
        assert!(new_model_count > 0);
        assert_eq!(embedding, embedding_to_json(&[2.0; 8]));
        Ok(())
    }

    #[tokio::test]
    async fn search_bm25_only() -> Result<()> {
        let db = test_db()?;
        let index = SearchIndex::new(db, "test-agent");
        let provider = StubEmbeddingProvider::new(8);

        index
            .index_file(
                "MEMORY.md",
                "# Projects\n\nClawhive memory architecture document",
                "long_term",
                &provider,
            )
            .await?;

        let results = index
            .search("architecture", &provider, 6, 0.0, None)
            .await?;
        assert!(!results.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn search_hybrid_returns_results() -> Result<()> {
        let db = test_db()?;
        let index = SearchIndex::new(db, "test-agent");
        let provider = StubEmbeddingProvider::new(8);

        index
            .index_file(
                "MEMORY.md",
                "# Rust\n\nTokio async runtime details",
                "long_term",
                &provider,
            )
            .await?;

        let results = index
            .search("tokio runtime", &provider, 6, 0.0, None)
            .await?;
        assert!(!results.is_empty());
        assert!(results[0].score >= 0.0);
        Ok(())
    }

    #[tokio::test]
    async fn search_respects_min_score() -> Result<()> {
        let db = test_db()?;
        let index = SearchIndex::new(db, "test-agent");
        let provider = StubEmbeddingProvider::new(8);

        index
            .index_file(
                "MEMORY.md",
                "# Topic\n\napple banana",
                "long_term",
                &provider,
            )
            .await?;

        let loose = index.search("apple", &provider, 6, 0.0, None).await?;
        let strict = index.search("apple", &provider, 6, 0.95, None).await?;
        assert!(strict.len() <= loose.len());
        Ok(())
    }

    #[tokio::test]
    async fn search_respects_max_results() -> Result<()> {
        let db = test_db()?;
        let index = SearchIndex::new(db, "test-agent");
        let provider = StubEmbeddingProvider::new(8);

        for i in 0..10 {
            let path = format!("memory/2026-02-{:02}.md", i + 1);
            let content = format!("# Day {}\n\nkeyword repeated", i + 1);
            index
                .index_file(&path, &content, "daily", &provider)
                .await?;
        }

        let results = index.search("keyword", &provider, 3, 0.0, None).await?;
        assert!(results.len() <= 3);
        Ok(())
    }

    #[test]
    fn classify_temperature_basic() {
        let now = chrono::Utc::now();
        let recent = (now - chrono::Duration::days(1)).to_rfc3339();
        let warm = (now - chrono::Duration::days(15)).to_rfc3339();
        let old = (now - chrono::Duration::days(60)).to_rfc3339();

        assert_eq!(
            classify_temperature(Some(recent.as_str()), 0, 7, 30, 5),
            Temperature::Hot
        );
        assert_eq!(
            classify_temperature(Some(warm.as_str()), 0, 7, 30, 5),
            Temperature::Warm
        );
        assert_eq!(
            classify_temperature(Some(old.as_str()), 0, 7, 30, 5),
            Temperature::Cold
        );
        assert_eq!(classify_temperature(None, 0, 7, 30, 5), Temperature::Warm);
        assert_eq!(
            classify_temperature(Some("not-a-timestamp"), 0, 7, 30, 5),
            Temperature::Warm
        );
        assert_eq!(
            classify_temperature(Some(old.as_str()), 10, 7, 30, 5),
            Temperature::Warm
        );
    }

    #[tokio::test]
    async fn cold_filter_removes_cold_from_results() -> Result<()> {
        let provider = StubEmbeddingProvider::new(8);
        let now = chrono::Utc::now();
        let hot_ts = (now - chrono::Duration::days(1)).to_rfc3339();
        let warm_ts = (now - chrono::Duration::days(20)).to_rfc3339();
        let cold_ts = (now - chrono::Duration::days(60)).to_rfc3339();

        let db_filtered = test_db()?;
        let index_filtered = SearchIndex::new_with_config(
            Arc::clone(&db_filtered),
            "test-agent",
            SearchConfig {
                cold_filter: true,
                min_score: 0.0,
                ..SearchConfig::default()
            },
        );

        index_filtered
            .index_file(
                "memory/hot.md",
                "# Topic\n\nshared keyword hot",
                "daily",
                &provider,
            )
            .await?;
        index_filtered
            .index_file(
                "memory/warm.md",
                "# Topic\n\nshared keyword warm",
                "daily",
                &provider,
            )
            .await?;
        index_filtered
            .index_file(
                "memory/cold.md",
                "# Topic\n\nshared keyword cold",
                "daily",
                &provider,
            )
            .await?;

        {
            let conn = db_filtered.lock().expect("lock");
            conn.execute(
                "UPDATE chunks SET last_accessed = ?1, access_count = 0 WHERE path = 'memory/hot.md'",
                params![hot_ts],
            )?;
            conn.execute(
                "UPDATE chunks SET last_accessed = ?1, access_count = 0 WHERE path = 'memory/warm.md'",
                params![warm_ts],
            )?;
            conn.execute(
                "UPDATE chunks SET last_accessed = ?1, access_count = 0 WHERE path = 'memory/cold.md'",
                params![cold_ts],
            )?;
        }

        let filtered = index_filtered
            .search("shared keyword", &provider, 10, 0.0, None)
            .await?;
        assert!(filtered.iter().all(|r| r.path != "memory/cold.md"));

        let db_unfiltered = test_db()?;
        let index_unfiltered = SearchIndex::new_with_config(
            Arc::clone(&db_unfiltered),
            "test-agent",
            SearchConfig {
                cold_filter: false,
                min_score: 0.0,
                ..SearchConfig::default()
            },
        );

        index_unfiltered
            .index_file(
                "memory/hot.md",
                "# Topic\n\nshared keyword hot",
                "daily",
                &provider,
            )
            .await?;
        index_unfiltered
            .index_file(
                "memory/warm.md",
                "# Topic\n\nshared keyword warm",
                "daily",
                &provider,
            )
            .await?;
        index_unfiltered
            .index_file(
                "memory/cold.md",
                "# Topic\n\nshared keyword cold",
                "daily",
                &provider,
            )
            .await?;

        {
            let conn = db_unfiltered.lock().expect("lock");
            conn.execute(
                "UPDATE chunks SET last_accessed = ?1, access_count = 0 WHERE path = 'memory/hot.md'",
                params![hot_ts],
            )?;
            conn.execute(
                "UPDATE chunks SET last_accessed = ?1, access_count = 0 WHERE path = 'memory/warm.md'",
                params![warm_ts],
            )?;
            conn.execute(
                "UPDATE chunks SET last_accessed = ?1, access_count = 0 WHERE path = 'memory/cold.md'",
                params![cold_ts],
            )?;
        }

        let unfiltered = index_unfiltered
            .search("shared keyword", &provider, 10, 0.0, None)
            .await?;
        assert!(unfiltered.iter().any(|r| r.path == "memory/cold.md"));

        Ok(())
    }

    #[tokio::test]
    async fn access_protect_prevents_cold_demotion() -> Result<()> {
        let provider = StubEmbeddingProvider::new(8);
        let db = test_db()?;
        let index = SearchIndex::new_with_config(
            Arc::clone(&db),
            "test-agent",
            SearchConfig {
                cold_filter: true,
                access_protect_count: 5,
                min_score: 0.0,
                ..SearchConfig::default()
            },
        );

        index
            .index_file(
                "memory/protected.md",
                "# Topic\n\nshared keyword protected",
                "daily",
                &provider,
            )
            .await?;

        let stale = (chrono::Utc::now() - chrono::Duration::days(60)).to_rfc3339();
        {
            let conn = db.lock().expect("lock");
            conn.execute(
                "UPDATE chunks SET last_accessed = ?1, access_count = 10 WHERE path = 'memory/protected.md'",
                params![stale],
            )?;
        }

        let results = index
            .search("shared keyword", &provider, 10, 0.0, None)
            .await?;
        assert!(results.iter().any(|r| r.path == "memory/protected.md"));

        Ok(())
    }

    #[tokio::test]
    async fn search_uses_configured_weights_not_hardcoded() -> Result<()> {
        let provider = KeywordVectorProvider;

        let db_bm25 = test_db()?;
        let index_bm25 = SearchIndex::new_with_config(
            Arc::clone(&db_bm25),
            "test-agent",
            SearchConfig {
                vector_weight: 0.0,
                bm25_weight: 1.0,
                min_score: 0.0,
                ..SearchConfig::default()
            },
        );

        index_bm25
            .index_file(
                "memory/bm25-wins.md",
                "# Repeated\n\nkeyword keyword keyword",
                "daily",
                &provider,
            )
            .await?;
        index_bm25
            .index_file(
                "memory/vector-wins.md",
                "# Vector\n\nvector-target",
                "daily",
                &provider,
            )
            .await?;

        let bm25_results = index_bm25
            .search("keyword", &provider, 2, 0.0, None)
            .await?;
        assert_eq!(bm25_results[0].path, "memory/bm25-wins.md");

        let db_vector = test_db()?;
        let index_vector = SearchIndex::new_with_config(
            Arc::clone(&db_vector),
            "test-agent",
            SearchConfig {
                vector_weight: 1.0,
                bm25_weight: 0.0,
                min_score: 0.0,
                ..SearchConfig::default()
            },
        );

        index_vector
            .index_file(
                "memory/bm25-wins.md",
                "# Repeated\n\nkeyword keyword keyword",
                "daily",
                &provider,
            )
            .await?;
        index_vector
            .index_file(
                "memory/vector-wins.md",
                "# Vector\n\nvector-target",
                "daily",
                &provider,
            )
            .await?;

        let vector_results = index_vector
            .search("keyword", &provider, 2, 0.0, None)
            .await?;
        assert_eq!(vector_results[0].path, "memory/vector-wins.md");

        Ok(())
    }

    #[tokio::test]
    async fn search_uses_vec_index() -> Result<()> {
        let db = test_db()?;
        let index = SearchIndex::new(Arc::clone(&db), "test-agent");
        let provider = StubEmbeddingProvider::new(8);

        index.ensure_vec_table(provider.dimensions())?;

        for i in 0..5 {
            let path = format!("memory/2026-02-{:02}.md", i + 1);
            let content =
                format!("# Topic {i}\n\nContent about topic number {i} with unique words{i}");
            index
                .index_file(&path, &content, "daily", &provider)
                .await?;
        }

        let results = index.search("topic", &provider, 3, 0.0, None).await?;
        assert!(!results.is_empty());
        assert!(results.len() <= 3);

        let conn = db.lock().expect("lock");
        let table_exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='chunks_vec'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(false);
        assert!(table_exists, "chunks_vec virtual table should exist");
        Ok(())
    }

    #[tokio::test]
    async fn search_empty_index_returns_empty() -> Result<()> {
        let db = test_db()?;
        let index = SearchIndex::new(db, "test-agent");
        let provider = StubEmbeddingProvider::new(8);

        let results = index.search("anything", &provider, 6, 0.0, None).await?;
        assert!(results.is_empty());
        Ok(())
    }

    #[test]
    fn test_build_safe_fts_query_strips_operators() {
        assert_eq!(
            build_safe_fts_query("hello OR world AND NOT bad"),
            r#""hello" "world" "bad""#
        );
    }

    #[test]
    fn test_build_safe_fts_query_escapes_quotes() {
        assert_eq!(build_safe_fts_query(r#""test*""#), r#""test""#);
    }

    #[test]
    fn test_build_safe_fts_query_empty_input() {
        assert_eq!(build_safe_fts_query(""), "");
        assert_eq!(build_safe_fts_query("OR AND NOT"), "");
    }

    #[tokio::test]
    async fn test_search_with_fts_special_chars() -> Result<()> {
        let db = test_db()?;
        let index = SearchIndex::new(db, "test-agent");
        let provider = StubEmbeddingProvider::new(8);

        index
            .index_file(
                "MEMORY.md",
                "# Title\n\nhello world",
                "long_term",
                &provider,
            )
            .await?;

        let results = index
            .search("hello OR world*", &provider, 6, 0.0, None)
            .await?;
        assert!(!results.is_empty());
        Ok(())
    }

    #[test]
    fn cosine_similarity_identical() {
        let a = vec![1.0_f32, 2.0, 3.0];
        let score = cosine_similarity(&a, &a);
        assert!((score - 1.0).abs() < 1e-6);
    }

    #[test]
    fn cosine_similarity_orthogonal() {
        let a = vec![1.0_f32, 0.0];
        let b = vec![0.0_f32, 1.0];
        let score = cosine_similarity(&a, &b);
        assert!(score.abs() < 1e-6);
    }

    #[test]
    fn embedding_json_roundtrip() -> Result<()> {
        let input = vec![0.1_f32, -0.2, 3.5];
        let json = embedding_to_json(&input);
        let output = json_to_embedding(&json)?;
        assert_eq!(input.len(), output.len());
        for (a, b) in input.iter().zip(output.iter()) {
            assert!((a - b).abs() < 1e-6);
        }
        Ok(())
    }

    #[test]
    fn needs_reindex_detects_model_change() -> Result<()> {
        let db = test_db()?;
        {
            let conn = db.lock().expect("lock");
            conn.execute(
                "INSERT INTO meta(key, value) VALUES('embedding_model', 'not-stub')",
                [],
            )?;
        }

        let index = SearchIndex::new(db, "test-agent");
        let provider = StubEmbeddingProvider::new(8);
        let needs = index.needs_reindex(&provider)?;
        assert!(needs);
        Ok(())
    }

    #[tokio::test]
    async fn index_all_indexes_files() -> Result<()> {
        let dir = TempDir::new()?;
        let file_store = MemoryFileStore::new(dir.path());
        let session_writer = SessionWriter::new(dir.path());
        let session_reader = SessionReader::new(dir.path());
        file_store
            .write_long_term("# Long\n\nlong term facts")
            .await?;
        file_store
            .write_daily(
                chrono::NaiveDate::from_ymd_opt(2026, 2, 13).expect("valid date"),
                "# Daily\n\ndaily notes",
            )
            .await?;
        session_writer.start_session("s1", "main").await?;
        session_writer
            .append_message("s1", "user", "session note")
            .await?;

        let db = test_db()?;
        let index = SearchIndex::new(Arc::clone(&db), "test-agent");
        let provider = StubEmbeddingProvider::new(8);

        let total = index
            .index_all(&file_store, &session_reader, &provider)
            .await?;
        assert!(total > 0);

        let conn = db.lock().expect("lock");
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))?;
        assert!(count > 0);
        let session_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM chunks WHERE source = 'session' AND path LIKE 'sessions/s1#turn:%'",
            [],
            |r| r.get(0),
        )?;
        assert!(session_count > 0);
        Ok(())
    }

    #[tokio::test]
    async fn index_dirty_indexes_memory_file() -> Result<()> {
        let dir = TempDir::new()?;
        let file_store = MemoryFileStore::new(dir.path());
        let session_reader = SessionReader::new(dir.path());
        file_store.write_long_term("# Long\n\ndirty memory").await?;

        let db = test_db()?;
        let dirty = crate::dirty_sources::DirtySourceStore::new(Arc::clone(&db));
        dirty
            .enqueue(
                "test-agent",
                crate::dirty_sources::DIRTY_KIND_MEMORY_FILE,
                "MEMORY.md",
                "memory_written",
            )
            .await?;

        let index = SearchIndex::new(Arc::clone(&db), "test-agent");
        let provider = StubEmbeddingProvider::new(8);
        let total = index
            .index_dirty(&file_store, &session_reader, &provider, 10)
            .await?;

        assert!(total > 0);
        let conn = db.lock().expect("lock");
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM chunks WHERE agent_id = 'test-agent' AND path = 'MEMORY.md'",
            [],
            |r| r.get(0),
        )?;
        assert!(count > 0);
        Ok(())
    }

    #[tokio::test]
    async fn index_dirty_indexes_session() -> Result<()> {
        let dir = TempDir::new()?;
        let writer = SessionWriter::new(dir.path());
        let reader = SessionReader::new(dir.path());
        writer.start_session("s1", "main").await?;
        writer.append_message("s1", "user", "dirty session").await?;

        let db = test_db()?;
        let dirty = crate::dirty_sources::DirtySourceStore::new(Arc::clone(&db));
        dirty
            .enqueue(
                "test-agent",
                crate::dirty_sources::DIRTY_KIND_SESSION,
                "s1",
                "session_appended",
            )
            .await?;

        let index = SearchIndex::new(Arc::clone(&db), "test-agent");
        let provider = StubEmbeddingProvider::new(8);
        let total = index
            .index_dirty(&MemoryFileStore::new(dir.path()), &reader, &provider, 10)
            .await?;

        assert!(total > 0);
        let conn = db.lock().expect("lock");
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM chunks WHERE agent_id = 'test-agent' AND path LIKE 'sessions/s1#turn:%'",
            [],
            |r| r.get(0),
        )?;
        assert!(count > 0);
        Ok(())
    }

    #[tokio::test]
    async fn index_dirty_removes_missing_daily_file() -> Result<()> {
        let dir = TempDir::new()?;
        let file_store = MemoryFileStore::new(dir.path());
        let session_reader = SessionReader::new(dir.path());

        let db = test_db()?;
        let index = SearchIndex::new(Arc::clone(&db), "test-agent");
        let provider = StubEmbeddingProvider::new(8);
        index
            .index_file(
                "memory/2026-03-29.md",
                "# Daily\n\nobsolete",
                "daily",
                &provider,
            )
            .await?;

        let dirty = crate::dirty_sources::DirtySourceStore::new(Arc::clone(&db));
        dirty
            .enqueue(
                "test-agent",
                crate::dirty_sources::DIRTY_KIND_DAILY_FILE,
                "memory/2026-03-29.md",
                "daily_deleted",
            )
            .await?;

        let total = index
            .index_dirty(&file_store, &session_reader, &provider, 10)
            .await?;
        assert_eq!(total, 0);

        let conn = db.lock().expect("lock");
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM chunks WHERE agent_id = 'test-agent' AND path = 'memory/2026-03-29.md'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(count, 0);
        Ok(())
    }

    #[tokio::test]
    async fn process_dirty_sources_marks_all_pending_processed() -> Result<()> {
        let dir = TempDir::new()?;
        let file_store = MemoryFileStore::new(dir.path());
        let session_writer = SessionWriter::new(dir.path());
        let session_reader = SessionReader::new(dir.path());

        file_store
            .write_long_term("# Long\n\nlong-term dirty content")
            .await?;
        let daily_date = chrono::NaiveDate::from_ymd_opt(2026, 4, 1).expect("valid date");
        file_store
            .write_daily(daily_date, "# Daily\n\ndaily dirty content")
            .await?;
        session_writer.start_session("s1", "main").await?;
        session_writer
            .append_message("s1", "user", "session dirty content")
            .await?;

        let db = test_db()?;
        let dirty = crate::dirty_sources::DirtySourceStore::new(Arc::clone(&db));
        dirty
            .enqueue(
                "test-agent",
                crate::dirty_sources::DIRTY_KIND_MEMORY_FILE,
                "MEMORY.md",
                "memory_written",
            )
            .await?;
        dirty
            .enqueue(
                "test-agent",
                crate::dirty_sources::DIRTY_KIND_DAILY_FILE,
                "memory/2026-04-01.md",
                "daily_written",
            )
            .await?;
        dirty
            .enqueue(
                "test-agent",
                crate::dirty_sources::DIRTY_KIND_SESSION,
                "s1",
                "session_appended",
            )
            .await?;

        let index = SearchIndex::new(Arc::clone(&db), "test-agent");
        let provider = StubEmbeddingProvider::new(8);

        let indexed = index
            .process_dirty_sources(
                &dirty,
                "test-agent",
                &file_store,
                &session_reader,
                &provider,
                10,
            )
            .await?;

        assert!(indexed > 0);
        assert_eq!(dirty.pending_count("test-agent").await?, 0);

        Ok(())
    }

    #[tokio::test]
    async fn process_dirty_sources_keeps_pending_on_index_failure() -> Result<()> {
        let dir = TempDir::new()?;
        let file_store = MemoryFileStore::new(dir.path());
        let session_reader = SessionReader::new(dir.path());

        let db = test_db()?;
        let dirty = crate::dirty_sources::DirtySourceStore::new(Arc::clone(&db));
        dirty
            .enqueue(
                "test-agent",
                "unsupported_kind",
                "bad-ref",
                "forced_failure",
            )
            .await?;

        let index = SearchIndex::new(Arc::clone(&db), "test-agent");
        let provider = StubEmbeddingProvider::new(8);

        let result = index
            .process_dirty_sources(
                &dirty,
                "test-agent",
                &file_store,
                &session_reader,
                &provider,
                10,
            )
            .await;

        assert!(result.is_err());
        assert_eq!(dirty.pending_count("test-agent").await?, 1);
        Ok(())
    }

    #[tokio::test]
    async fn test_index_session_creates_chunks() -> Result<()> {
        let dir = TempDir::new()?;
        let writer = SessionWriter::new(dir.path());
        let reader = SessionReader::new(dir.path());
        writer.start_session("s1", "main").await?;
        writer.append_message("s1", "user", "hello").await?;
        writer.append_message("s1", "assistant", "hi").await?;
        writer
            .append(
                "s1",
                SessionEntry::ToolCall {
                    id: "tool-1".to_owned(),
                    timestamp: chrono::Utc::now(),
                    tool: "bash".to_owned(),
                    input: serde_json::json!({"command": "pwd"}),
                },
            )
            .await?;

        let db = test_db()?;
        let index = SearchIndex::new(Arc::clone(&db), "test-agent");
        let provider = StubEmbeddingProvider::new(8);

        let count = index.index_session("s1", &reader, &provider).await?;

        assert!(count > 0);
        let conn = db.lock().expect("lock");
        let row: (String, String, String) = conn.query_row(
            "SELECT path, source, text FROM chunks WHERE path LIKE 'sessions/s1#turn:%' ORDER BY path LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        assert_eq!(row.0, "sessions/s1#turn:1");
        assert_eq!(row.1, "session");
        assert!(row.2.contains("user: hello"));
        assert!(row.2.contains("assistant: hi"));
        assert!(!row.2.contains("pwd"));
        Ok(())
    }

    #[tokio::test]
    async fn test_index_session_skips_tool_entries() -> Result<()> {
        let dir = TempDir::new()?;
        let writer = SessionWriter::new(dir.path());
        let reader = SessionReader::new(dir.path());
        writer.start_session("s1", "main").await?;
        writer
            .append(
                "s1",
                SessionEntry::ToolCall {
                    id: "tool-call".to_owned(),
                    timestamp: chrono::Utc::now(),
                    tool: "read".to_owned(),
                    input: serde_json::json!({"file": "/tmp/secret.txt"}),
                },
            )
            .await?;
        writer
            .append(
                "s1",
                SessionEntry::ToolResult {
                    id: "tool-result".to_owned(),
                    timestamp: chrono::Utc::now(),
                    tool: "read".to_owned(),
                    output: serde_json::json!({"content": "secret"}),
                },
            )
            .await?;
        writer
            .append(
                "s1",
                SessionEntry::Compaction {
                    id: "compact-1".to_owned(),
                    timestamp: chrono::Utc::now(),
                    summary: "summary".to_owned(),
                    dropped_before: "m1".to_owned(),
                },
            )
            .await?;
        writer
            .append(
                "s1",
                SessionEntry::ModelChange {
                    id: "model-1".to_owned(),
                    timestamp: chrono::Utc::now(),
                    model: "gpt-5".to_owned(),
                },
            )
            .await?;
        writer.append_message("s1", "user", "keep this").await?;

        let db = test_db()?;
        let index = SearchIndex::new(Arc::clone(&db), "test-agent");
        let provider = StubEmbeddingProvider::new(8);

        index.index_session("s1", &reader, &provider).await?;

        let conn = db.lock().expect("lock");
        let text: String = conn.query_row(
            "SELECT GROUP_CONCAT(text, ' ') FROM chunks WHERE path LIKE 'sessions/s1#turn:%'",
            [],
            |r| r.get(0),
        )?;
        assert!(text.contains("user: keep this"));
        assert!(!text.contains("/tmp/secret.txt"));
        assert!(!text.contains("secret"));
        assert!(!text.contains("summary"));
        assert!(!text.contains("gpt-5"));
        Ok(())
    }

    #[tokio::test]
    async fn test_index_sessions_multiple() -> Result<()> {
        let dir = TempDir::new()?;
        let writer = SessionWriter::new(dir.path());
        let reader = SessionReader::new(dir.path());
        writer.start_session("s1", "main").await?;
        writer.append_message("s1", "user", "alpha").await?;
        writer.start_session("s2", "main").await?;
        writer.append_message("s2", "assistant", "beta").await?;

        let db = test_db()?;
        let index = SearchIndex::new(Arc::clone(&db), "test-agent");
        let provider = StubEmbeddingProvider::new(8);

        let total = index.index_sessions(&reader, &provider).await?;

        assert!(total > 0);
        let conn = db.lock().expect("lock");
        let indexed_paths: i64 = conn.query_row(
            "SELECT COUNT(DISTINCT path) FROM chunks WHERE source = 'session'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(indexed_paths, 2);
        Ok(())
    }

    #[tokio::test]
    async fn test_index_sessions_removes_stale_entries() -> Result<()> {
        let dir = TempDir::new()?;
        let writer = SessionWriter::new(dir.path());
        let reader = SessionReader::new(dir.path());
        writer.start_session("s1", "main").await?;
        writer.append_message("s1", "user", "alpha").await?;
        writer.start_session("s2", "main").await?;
        writer.append_message("s2", "user", "beta").await?;

        let db = test_db()?;
        let index = SearchIndex::new(Arc::clone(&db), "test-agent");
        let provider = StubEmbeddingProvider::new(8);

        index.index_sessions(&reader, &provider).await?;
        {
            let conn = db.lock().expect("lock");
            let count: i64 = conn.query_row(
                "SELECT COUNT(DISTINCT path) FROM chunks WHERE source = 'session'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(count, 2);
        }

        let s2_path = dir.path().join("sessions").join("s2.jsonl");
        std::fs::remove_file(&s2_path).expect("remove s2 file");

        index.index_sessions(&reader, &provider).await?;
        {
            let conn = db.lock().expect("lock");
            let remaining: i64 = conn.query_row(
                "SELECT COUNT(DISTINCT path) FROM chunks WHERE source = 'session'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(remaining, 1);

            let stale_files: i64 = conn.query_row(
                "SELECT COUNT(*) FROM files WHERE agent_id = 'test-agent' AND path LIKE 'sessions/s2%'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(stale_files, 0);
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_index_session_splits_turns() -> Result<()> {
        let dir = TempDir::new()?;
        let writer = SessionWriter::new(dir.path());
        let reader = SessionReader::new(dir.path());
        writer.start_session("s1", "main").await?;
        writer
            .append_message("s1", "user", "first question")
            .await?;
        writer
            .append_message("s1", "assistant", "first answer")
            .await?;
        writer
            .append_message("s1", "user", "second question")
            .await?;
        writer
            .append_message("s1", "assistant", "second answer")
            .await?;

        let db = test_db()?;
        let index = SearchIndex::new(Arc::clone(&db), "test-agent");
        let provider = StubEmbeddingProvider::new(8);

        let count = index.index_session("s1", &reader, &provider).await?;

        assert_eq!(count, 2);
        let conn = db.lock().expect("lock");
        let paths: Vec<String> = conn
            .prepare("SELECT path FROM chunks WHERE source = 'session' ORDER BY path")?
            .query_map([], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        assert_eq!(paths, vec!["sessions/s1#turn:1", "sessions/s1#turn:2"]);
        Ok(())
    }

    #[tokio::test]
    async fn test_index_session_merges_related_turns_into_topic_window() -> Result<()> {
        let dir = TempDir::new()?;
        let writer = SessionWriter::new(dir.path());
        let reader = SessionReader::new(dir.path());
        writer.start_session("s1", "main").await?;
        writer
            .append_message("s1", "user", "How do I use Rust Vec push?")
            .await?;
        writer
            .append_message("s1", "assistant", "Use Vec push to append items in Rust.")
            .await?;
        writer
            .append_message("s1", "user", "What about Vec insert in Rust collections?")
            .await?;
        writer
            .append_message(
                "s1",
                "assistant",
                "Vec insert adds an item at an index in Rust.",
            )
            .await?;

        let db = test_db()?;
        let index = SearchIndex::new(Arc::clone(&db), "test-agent");
        let provider = StubEmbeddingProvider::new(8);

        let count = index.index_session("s1", &reader, &provider).await?;

        assert_eq!(count, 1);
        let conn = db.lock().expect("lock");
        let rows: Vec<(String, String)> = conn
            .prepare("SELECT path, text FROM chunks WHERE source = 'session' ORDER BY path")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<(String, String)>, _>>()?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "sessions/s1#turn:1-2");
        assert!(rows[0].1.contains("user: How do I use Rust Vec push?"));
        assert!(rows[0]
            .1
            .contains("user: What about Vec insert in Rust collections?"));
        Ok(())
    }

    #[tokio::test]
    async fn test_index_session_keeps_unrelated_turns_separate() -> Result<()> {
        let dir = TempDir::new()?;
        let writer = SessionWriter::new(dir.path());
        let reader = SessionReader::new(dir.path());
        writer.start_session("s1", "main").await?;
        writer
            .append_message("s1", "user", "How do I use Rust Vec push?")
            .await?;
        writer
            .append_message("s1", "assistant", "Use Vec push to append items in Rust.")
            .await?;
        writer
            .append_message("s1", "user", "What is the weather in Singapore today?")
            .await?;
        writer
            .append_message(
                "s1",
                "assistant",
                "I cannot fetch weather without a tool call.",
            )
            .await?;

        let db = test_db()?;
        let index = SearchIndex::new(Arc::clone(&db), "test-agent");
        let provider = StubEmbeddingProvider::new(8);

        let count = index.index_session("s1", &reader, &provider).await?;

        assert_eq!(count, 2);
        let conn = db.lock().expect("lock");
        let paths: Vec<String> = conn
            .prepare("SELECT path FROM chunks WHERE source = 'session' ORDER BY path")?
            .query_map([], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()?;
        assert_eq!(paths, vec!["sessions/s1#turn:1", "sessions/s1#turn:2"]);
        Ok(())
    }

    #[tokio::test]
    async fn index_file_tracks_files_per_agent_scope() -> Result<()> {
        let db = test_db()?;
        let provider = StubEmbeddingProvider::new(8);
        let index_a = SearchIndex::new(Arc::clone(&db), "agent-a");
        let index_b = SearchIndex::new(Arc::clone(&db), "agent-b");

        index_a
            .index_file("MEMORY.md", "# A\n\nalpha", "long_term", &provider)
            .await?;
        index_b
            .index_file("MEMORY.md", "# B\n\nbeta", "long_term", &provider)
            .await?;

        let conn = db.lock().expect("lock");
        let file_rows: i64 = conn.query_row(
            "SELECT COUNT(*) FROM files WHERE path = 'MEMORY.md'",
            [],
            |r| r.get(0),
        )?;
        let chunk_rows: i64 = conn.query_row(
            "SELECT COUNT(DISTINCT agent_id) FROM chunks WHERE path = 'MEMORY.md'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(file_rows, 2);
        assert_eq!(chunk_rows, 2);
        Ok(())
    }

    #[tokio::test]
    async fn search_scores_are_normalized() -> Result<()> {
        let db = test_db()?;
        let index = SearchIndex::new(db, "test-agent");
        let provider = StubEmbeddingProvider::new(8);

        index
            .index_file(
                "MEMORY.md",
                "# Cooking\n\nI love making pasta with tomato sauce and fresh basil",
                "long_term",
                &provider,
            )
            .await?;
        index
            .index_file(
                "memory/2026-02-13.md",
                "# Programming\n\nRust async runtime with tokio and futures",
                "daily",
                &provider,
            )
            .await?;

        let results = index
            .search("pasta cooking", &provider, 6, 0.0, None)
            .await?;
        assert!(!results.is_empty());
        for result in &results {
            assert!(result.score >= 0.0, "Score below 0: {}", result.score);
            assert!(result.score <= 1.0, "Score above 1: {}", result.score);
        }
        Ok(())
    }

    #[tokio::test]
    async fn search_vector_only_fallback_on_fts_error() -> Result<()> {
        let db = test_db()?;
        let index = SearchIndex::new(db, "test-agent");
        let provider = StubEmbeddingProvider::new(8);

        index
            .index_file(
                "MEMORY.md",
                "# Title\n\nSome content here about testing",
                "long_term",
                &provider,
            )
            .await?;

        let results = index
            .search("testing content", &provider, 6, 0.0, None)
            .await?;
        assert!(!results.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn search_with_time_range_filters_daily_chunks_by_path_date() -> Result<()> {
        let db = test_db()?;
        let index = SearchIndex::new(db, "test-agent");
        let provider = StubEmbeddingProvider::new(8);

        index
            .index_file(
                "memory/2026-03-01.md",
                "# March\n\nrelease planning and sprint notes",
                "daily",
                &provider,
            )
            .await?;
        index
            .index_file(
                "memory/2026-04-01.md",
                "# April\n\nrelease planning and sprint notes",
                "daily",
                &provider,
            )
            .await?;
        index
            .index_file(
                "MEMORY.md",
                "# Long Term\n\nrelease planning principles",
                "long_term",
                &provider,
            )
            .await?;

        let results = index
            .search(
                "release planning",
                &provider,
                10,
                0.0,
                Some(TimeRange {
                    from: Some("2026-03".to_string()),
                    to: Some("2026-03".to_string()),
                }),
            )
            .await?;

        assert!(results.iter().any(|r| r.path == "memory/2026-03-01.md"));
        assert!(results.iter().all(|r| r.path != "memory/2026-04-01.md"));
        assert!(results.iter().any(|r| r.path == "MEMORY.md"));
        Ok(())
    }

    #[test]
    fn generate_snippet_truncates_at_sentence_boundary() {
        let text =
            "First sentence. Second sentence. Third sentence is much longer and continues beyond the limit.";
        let snippet = generate_snippet(text, 50);
        assert!(snippet.len() <= 55);
        assert!(snippet.ends_with("..."));
        assert!(snippet.contains("First sentence."));
    }

    #[test]
    fn generate_snippet_returns_full_text_when_short() {
        let text = "Short text.";
        let snippet = generate_snippet(text, 200);
        assert_eq!(snippet, "Short text.");
    }

    #[tokio::test]
    async fn search_returns_snippet_instead_of_full_chunk_text() -> Result<()> {
        let db = test_db()?;
        let index = SearchIndex::new(db, "test-agent");
        let provider = StubEmbeddingProvider::new(8);
        let long_text = format!("# Notes\n\n{}", "Long sentence with details. ".repeat(20));

        index
            .index_file("memory/2026-03-20.md", &long_text, "daily", &provider)
            .await?;

        let results = index.search("details", &provider, 3, 0.0, None).await?;
        assert!(!results.is_empty());
        assert!(results[0].snippet.len() <= 230);
        assert!(results[0].text.len() >= results[0].snippet.len());
        Ok(())
    }

    #[tokio::test]
    async fn search_result_includes_score_breakdown() -> Result<()> {
        let db = test_db()?;
        let index = SearchIndex::new(db, "test-agent");
        let provider = StubEmbeddingProvider::new(8);

        index
            .index_file(
                "memory/2026-03-20.md",
                "# Notes\n\nRust async runtime and tokio execution details",
                "daily",
                &provider,
            )
            .await?;

        let results = index
            .search("tokio runtime", &provider, 3, 0.0, None)
            .await?;
        assert!(!results.is_empty());

        let breakdown = results[0]
            .score_breakdown
            .as_ref()
            .expect("score breakdown should exist");

        let base = breakdown.fused_score * breakdown.temporal_decay * breakdown.access_boost;
        let expected_final = if breakdown.temperature == "hot" {
            (base + 0.01).min(1.0)
        } else {
            base
        };
        assert!((breakdown.final_score - expected_final).abs() < 1e-6);
        assert!((results[0].score - breakdown.final_score).abs() < 1e-6);

        Ok(())
    }
}
