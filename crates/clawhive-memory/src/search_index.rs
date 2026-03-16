use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use tokio::task;

use crate::chunker::{chunk_markdown, ChunkerConfig};
use crate::embedding::EmbeddingProvider;
use crate::session::{SessionEntry, SessionReader};

#[derive(Clone)]
pub struct SearchIndex {
    db: Arc<Mutex<Connection>>,
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub chunk_id: String,
    pub path: String,
    pub source: String,
    pub start_line: i64,
    pub end_line: i64,
    pub text: String,
    pub score: f64,
}

impl SearchIndex {
    pub fn new(db: Arc<Mutex<Connection>>) -> Self {
        Self { db }
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
        let file_hash_for_check = change_hash.to_owned();
        let unchanged = task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let existing: Option<String> = conn
                .query_row(
                    "SELECT hash FROM files WHERE path = ?1",
                    params![path_owned],
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
            task::spawn_blocking(move || {
                let conn = db
                    .lock()
                    .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
                let tx = conn.unchecked_transaction()?;
                tx.execute(
                    "DELETE FROM chunks_fts WHERE path = ?1",
                    params![path_owned],
                )?;
                tx.execute(
                    "DELETE FROM chunks_vec WHERE chunk_id IN (SELECT id FROM chunks WHERE path = ?1)",
                    params![path_owned],
                )?;
                tx.execute("DELETE FROM chunks WHERE path = ?1", params![path_owned])?;
                tx.execute(
                    r#"
                    INSERT INTO files(path, source, hash, mtime, size)
                    VALUES (?1, ?2, ?3, ?4, ?5)
                    ON CONFLICT(path) DO UPDATE SET
                        source = excluded.source,
                        hash = excluded.hash,
                        mtime = excluded.mtime,
                        size = excluded.size
                    "#,
                    params![path_owned, source_owned, file_hash_for_write, now_ts, size],
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
        let size = content.len() as i64;
        let mut rows = Vec::with_capacity(text_chunks.len());
        for (idx, chunk) in text_chunks.iter().enumerate() {
            let prefix_len = chunk.hash.len().min(8);
            let chunk_id = format!(
                "{}:{}-{}:{}",
                path,
                chunk.start_line,
                chunk.end_line,
                &chunk.hash[..prefix_len]
            );
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
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let tx = conn.unchecked_transaction()?;

            tx.execute("DELETE FROM chunks_fts WHERE path = ?1", params![path_owned])?;
            tx.execute(
                "DELETE FROM chunks_vec WHERE chunk_id IN (SELECT id FROM chunks WHERE path = ?1)",
                params![path_owned],
            )?;
            tx.execute("DELETE FROM chunks WHERE path = ?1", params![path_owned])?;

            for (chunk_id, start_line, end_line, hash, model, text, embedding) in rows {
                tx.execute(
                    r#"
                    INSERT INTO chunks(
                        id, path, source, start_line, end_line, hash, model, text, embedding, updated_at
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
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
                        now_ts
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
                INSERT INTO files(path, source, hash, mtime, size)
                VALUES (?1, ?2, ?3, ?4, ?5)
                ON CONFLICT(path) DO UPDATE SET
                    source = excluded.source,
                    hash = excluded.hash,
                    mtime = excluded.mtime,
                    size = excluded.size
                "#,
                params![path_owned, source_owned, file_hash_for_write, now_ts, size],
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
        let entries = reader.load_all_entries(session_id).await?;
        let mut last_timestamp = None;
        let mut messages = Vec::new();

        for entry in entries {
            match entry {
                SessionEntry::Session { timestamp, .. } => {
                    last_timestamp = Some(timestamp);
                }
                SessionEntry::Message {
                    timestamp, message, ..
                } => {
                    last_timestamp = Some(timestamp);
                    if matches!(message.role.as_str(), "user" | "assistant") {
                        messages.push(format!("{}: {}", message.role, message.content));
                    }
                }
                SessionEntry::ToolCall { timestamp, .. }
                | SessionEntry::ToolResult { timestamp, .. }
                | SessionEntry::Compaction { timestamp, .. }
                | SessionEntry::ModelChange { timestamp, .. } => {
                    last_timestamp = Some(timestamp);
                }
            }
        }

        if messages.is_empty() {
            return Ok(0);
        }

        let content = messages.join("\n");
        let content_len = content.len();
        let change_hash = format!(
            "session:{}:{}:{}",
            messages.len(),
            content_len,
            last_timestamp.map_or(0, |timestamp| timestamp.timestamp_millis())
        );
        let path = format!("sessions/{session_id}");

        self.index_content(&path, &content, "session", &change_hash, provider)
            .await
    }

    pub async fn index_sessions(
        &self,
        reader: &SessionReader,
        provider: &dyn EmbeddingProvider,
    ) -> Result<usize> {
        let sessions = reader.list_sessions().await?;
        let mut total = 0;

        for session_id in &sessions {
            match self.index_session(session_id, reader, provider).await {
                Ok(count) => total += count,
                Err(error) => {
                    tracing::warn!(session_id = %session_id, %error, "failed to index session");
                }
            }
        }

        // Remove stale session entries that no longer have backing JSONL files.
        // This handles /new (reset) and manual session deletions.
        let active_paths: std::collections::HashSet<String> =
            sessions.iter().map(|id| format!("sessions/{id}")).collect();

        let db = Arc::clone(&self.db);
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;

            let mut stmt =
                conn.prepare("SELECT path FROM files WHERE source = 'session'")?;
            let indexed_paths: Vec<String> = stmt
                .query_map([], |row| row.get(0))?
                .filter_map(|r| r.ok())
                .collect();

            for path in indexed_paths {
                if !active_paths.contains(&path) {
                    tracing::info!(path = %path, "removing stale session index");
                    let tx = conn.unchecked_transaction()?;
                    tx.execute("DELETE FROM chunks_fts WHERE path = ?1", params![&path])?;
                    tx.execute(
                        "DELETE FROM chunks_vec WHERE chunk_id IN (SELECT id FROM chunks WHERE path = ?1)",
                        params![&path],
                    )?;
                    tx.execute("DELETE FROM chunks WHERE path = ?1", params![&path])?;
                    tx.execute("DELETE FROM files WHERE path = ?1", params![&path])?;
                    tx.commit()?;
                }
            }

            Ok::<(), anyhow::Error>(())
        })
        .await??;

        Ok(total)
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
    ) -> Result<Vec<SearchResult>> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }

        let target_results = if max_results == 0 { 6 } else { max_results };
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
                let mut stmt = conn.prepare(
                    r#"
                    SELECT v.chunk_id, c.path, c.source, c.start_line, c.end_line, c.text, v.distance
                    FROM chunks_vec v
                    JOIN chunks c ON c.id = v.chunk_id
                    WHERE v.embedding MATCH ?1 AND k = ?2
                    "#,
                )?;
                let rows = stmt.query_map(params![query_embedding_json, candidate_limit as i64], |r| {
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

                let mut out = Vec::new();
                for row in rows {
                    let (chunk_id, path, source, start_line, end_line, text, distance) = row?;
                    let score = (1.0_f64 - distance).max(0.0_f64);
                    out.push((chunk_id, path, source, start_line, end_line, text, score));
                }
                out.sort_by(|a, b| b.6.total_cmp(&a.6));
                out.truncate(candidate_limit);
                return Ok::<Vec<(String, String, String, i64, i64, String, f64)>, anyhow::Error>(
                    out,
                );
            }

            let mut stmt = conn.prepare(
                "SELECT id, path, source, start_line, end_line, text, embedding FROM chunks",
            )?;
            let rows = stmt.query_map([], |r| {
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
        let mut bm25_candidates = if safe_fts_query.is_empty() {
            Vec::new()
        } else {
            let db = Arc::clone(&self.db);
            match task::spawn_blocking(move || {
                let conn = db
                    .lock()
                    .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
                let mut stmt = conn.prepare(
                    r#"
                    SELECT id, path, source, start_line, end_line, text, bm25(chunks_fts) AS rank
                    FROM chunks_fts
                    WHERE chunks_fts MATCH ?1
                    ORDER BY rank
                    LIMIT ?2
                    "#,
                )?;
                let rows =
                    stmt.query_map(params![safe_fts_query, candidate_limit as i64], |r| {
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
            .map(|item| SearchResult {
                chunk_id: item.chunk_id,
                path: item.path,
                source: item.source,
                start_line: item.start_line,
                end_line: item.end_line,
                text: item.text,
                score: if use_vectors {
                    (item.vector_score * 0.7) + (item.bm25_score * 0.3)
                } else {
                    item.bm25_score // BM25-only mode
                },
            })
            .filter(|item| item.score >= min_score)
            .collect::<Vec<SearchResult>>();

        // --- Temporal Decay ---
        // Boost recent memories, decay older ones (half-life = 30 days)
        let half_life_days = 30.0_f64;
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
            }
        }

        results.sort_by(|a, b| b.score.total_cmp(&a.score));

        // --- MMR (Maximal Marginal Relevance) ---
        // Re-rank to reduce redundancy (lambda=0.7: balance relevance + diversity)
        let mmr_lambda = 0.7_f64;
        let mmr_results = mmr_rerank(&results, mmr_lambda, target_results);

        Ok(mmr_results)
    }
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
        let index = SearchIndex::new(Arc::clone(&db));
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
        let index = SearchIndex::new(Arc::clone(&db));
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
        let index = SearchIndex::new(Arc::clone(&db));
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
        let index = SearchIndex::new(Arc::clone(&db));
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
        let index = SearchIndex::new(Arc::clone(&db));
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
        let index = SearchIndex::new(Arc::clone(&db));
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
        let index = SearchIndex::new(db);
        let provider = StubEmbeddingProvider::new(8);

        index
            .index_file(
                "MEMORY.md",
                "# Projects\n\nClawhive memory architecture document",
                "long_term",
                &provider,
            )
            .await?;

        let results = index.search("architecture", &provider, 6, 0.0).await?;
        assert!(!results.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn search_hybrid_returns_results() -> Result<()> {
        let db = test_db()?;
        let index = SearchIndex::new(db);
        let provider = StubEmbeddingProvider::new(8);

        index
            .index_file(
                "MEMORY.md",
                "# Rust\n\nTokio async runtime details",
                "long_term",
                &provider,
            )
            .await?;

        let results = index.search("tokio runtime", &provider, 6, 0.0).await?;
        assert!(!results.is_empty());
        assert!(results[0].score >= 0.0);
        Ok(())
    }

    #[tokio::test]
    async fn search_respects_min_score() -> Result<()> {
        let db = test_db()?;
        let index = SearchIndex::new(db);
        let provider = StubEmbeddingProvider::new(8);

        index
            .index_file(
                "MEMORY.md",
                "# Topic\n\napple banana",
                "long_term",
                &provider,
            )
            .await?;

        let loose = index.search("apple", &provider, 6, 0.0).await?;
        let strict = index.search("apple", &provider, 6, 0.95).await?;
        assert!(strict.len() <= loose.len());
        Ok(())
    }

    #[tokio::test]
    async fn search_respects_max_results() -> Result<()> {
        let db = test_db()?;
        let index = SearchIndex::new(db);
        let provider = StubEmbeddingProvider::new(8);

        for i in 0..10 {
            let path = format!("memory/2026-02-{:02}.md", i + 1);
            let content = format!("# Day {}\n\nkeyword repeated", i + 1);
            index
                .index_file(&path, &content, "daily", &provider)
                .await?;
        }

        let results = index.search("keyword", &provider, 3, 0.0).await?;
        assert!(results.len() <= 3);
        Ok(())
    }

    #[tokio::test]
    async fn search_uses_vec_index() -> Result<()> {
        let db = test_db()?;
        let index = SearchIndex::new(Arc::clone(&db));
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

        let results = index.search("topic", &provider, 3, 0.0).await?;
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
        let index = SearchIndex::new(db);
        let provider = StubEmbeddingProvider::new(8);

        let results = index.search("anything", &provider, 6, 0.0).await?;
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
        let index = SearchIndex::new(db);
        let provider = StubEmbeddingProvider::new(8);

        index
            .index_file(
                "MEMORY.md",
                "# Title\n\nhello world",
                "long_term",
                &provider,
            )
            .await?;

        let results = index.search("hello OR world*", &provider, 6, 0.0).await?;
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

        let index = SearchIndex::new(db);
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
        let index = SearchIndex::new(Arc::clone(&db));
        let provider = StubEmbeddingProvider::new(8);

        let total = index
            .index_all(&file_store, &session_reader, &provider)
            .await?;
        assert!(total > 0);

        let conn = db.lock().expect("lock");
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))?;
        assert!(count > 0);
        let session_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM chunks WHERE source = 'session' AND path = 'sessions/s1'",
            [],
            |r| r.get(0),
        )?;
        assert!(session_count > 0);
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
        let index = SearchIndex::new(Arc::clone(&db));
        let provider = StubEmbeddingProvider::new(8);

        let count = index.index_session("s1", &reader, &provider).await?;

        assert!(count > 0);
        let conn = db.lock().expect("lock");
        let row: (String, String, String) = conn.query_row(
            "SELECT path, source, text FROM chunks WHERE path = 'sessions/s1' LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        assert_eq!(row.0, "sessions/s1");
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
        let index = SearchIndex::new(Arc::clone(&db));
        let provider = StubEmbeddingProvider::new(8);

        index.index_session("s1", &reader, &provider).await?;

        let conn = db.lock().expect("lock");
        let text: String = conn.query_row(
            "SELECT GROUP_CONCAT(text, ' ') FROM chunks WHERE path = 'sessions/s1'",
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
        let index = SearchIndex::new(Arc::clone(&db));
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
        let index = SearchIndex::new(Arc::clone(&db));
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
                "SELECT COUNT(*) FROM files WHERE path = 'sessions/s2'",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(stale_files, 0);
        }
        Ok(())
    }

    #[tokio::test]
    async fn search_scores_are_normalized() -> Result<()> {
        let db = test_db()?;
        let index = SearchIndex::new(db);
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

        let results = index.search("pasta cooking", &provider, 6, 0.0).await?;
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
        let index = SearchIndex::new(db);
        let provider = StubEmbeddingProvider::new(8);

        index
            .index_file(
                "MEMORY.md",
                "# Title\n\nSome content here about testing",
                "long_term",
                &provider,
            )
            .await?;

        let results = index.search("testing content", &provider, 6, 0.0).await?;
        assert!(!results.is_empty());
        Ok(())
    }
}
