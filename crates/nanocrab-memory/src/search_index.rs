use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use tokio::task;

use crate::chunker::{chunk_markdown, ChunkerConfig};
use crate::embedding::EmbeddingProvider;

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

        let db = Arc::clone(&self.db);
        let path_owned = path.to_owned();
        let file_hash_for_check = file_hash.clone();
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
            let file_hash_for_write = file_hash.clone();
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
        let reused = task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let mut stmt = conn.prepare(
                "SELECT embedding, model FROM chunks WHERE hash = ?1 AND embedding <> '' LIMIT 1",
            )?;
            let mut map = std::collections::HashMap::new();
            for hash in hash_list {
                if map.contains_key(&hash) {
                    continue;
                }
                let row = stmt
                    .query_row(params![hash.clone()], |r| {
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
        let file_hash_for_write = file_hash.clone();
        let model_id = provider.model_id().to_owned();
        task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
            let tx = conn.unchecked_transaction()?;

            tx.execute("DELETE FROM chunks_fts WHERE path = ?1", params![path_owned])?;
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

    pub async fn index_all(
        &self,
        file_store: &crate::file_store::MemoryFileStore,
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

        let embedded = provider.embed(&[query.to_owned()]).await?;
        let query_embedding = embedded
            .embeddings
            .first()
            .cloned()
            .ok_or_else(|| anyhow!("embedding provider returned empty query embedding"))?;

        let db = Arc::clone(&self.db);
        let vector_candidates = task::spawn_blocking(move || {
            let conn = db
                .lock()
                .map_err(|_| anyhow!("failed to lock sqlite connection"))?;
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

        let db = Arc::clone(&self.db);
        let query_owned = query.to_owned();
        let bm25_candidates = task::spawn_blocking(move || {
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
            let rows = stmt.query_map(params![query_owned, candidate_limit as i64], |r| {
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
        .await??;

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
                score: (item.vector_score * 0.7) + (item.bm25_score * 0.3),
            })
            .filter(|item| item.score >= min_score)
            .collect::<Vec<SearchResult>>();

        results.sort_by(|a, b| b.score.total_cmp(&a.score));
        results.truncate(target_results);

        Ok(results)
    }
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
    use rusqlite::Connection;
    use tempfile::TempDir;

    use crate::embedding::StubEmbeddingProvider;
    use crate::file_store::MemoryFileStore;
    use crate::migrations::run_migrations;

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
    async fn search_bm25_only() -> Result<()> {
        let db = test_db()?;
        let index = SearchIndex::new(db);
        let provider = StubEmbeddingProvider::new(8);

        index
            .index_file(
                "MEMORY.md",
                "# Projects\n\nNanocrab memory architecture document",
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
    async fn search_empty_index_returns_empty() -> Result<()> {
        let db = test_db()?;
        let index = SearchIndex::new(db);
        let provider = StubEmbeddingProvider::new(8);

        let results = index.search("anything", &provider, 6, 0.0).await?;
        assert!(results.is_empty());
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
        file_store
            .write_long_term("# Long\n\nlong term facts")
            .await?;
        file_store
            .write_daily(
                chrono::NaiveDate::from_ymd_opt(2026, 2, 13).expect("valid date"),
                "# Daily\n\ndaily notes",
            )
            .await?;

        let db = test_db()?;
        let index = SearchIndex::new(Arc::clone(&db));
        let provider = StubEmbeddingProvider::new(8);

        let total = index.index_all(&file_store, &provider).await?;
        assert!(total > 0);

        let conn = db.lock().expect("lock");
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))?;
        assert!(count > 0);
        Ok(())
    }
}
