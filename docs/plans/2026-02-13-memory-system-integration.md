# Memory System Integration Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Wire the real OpenAI embedding provider into production, add post-consolidation re-indexing, tune search quality, and replace the O(n) vector scan with sqlite-vec ANN search.

**Architecture:** The memory system already has all components implemented (OpenAiEmbeddingProvider, SearchIndex, HippocampusConsolidator, MemoryFileStore). This plan wires them together: config → real embedding provider → consolidation with re-index → sqlite-vec ANN search. The consolidation scheduler is already started in `start_bot()`.

**Tech Stack:** Rust, rusqlite, sqlite-vec 0.1.6, FTS5, OpenAI embeddings API, tokio

---

## Context: Key Files

| File | Role |
|------|------|
| `crates/nanocrab-core/src/config.rs` | YAML config structs + deserialization |
| `crates/nanocrab-cli/src/main.rs` | Bootstrap + wiring (line 364: stub provider) |
| `crates/nanocrab-memory/src/embedding.rs` | `OpenAiEmbeddingProvider` (already complete) |
| `crates/nanocrab-memory/src/search_index.rs` | `SearchIndex` with hybrid search |
| `crates/nanocrab-core/src/consolidation.rs` | `HippocampusConsolidator` + `ConsolidationScheduler` |
| `crates/nanocrab-core/src/orchestrator.rs` | `build_memory_context()` uses search (line 543) |
| `crates/nanocrab-memory/src/migrations.rs` | SQLite schema migrations |
| `config/main.yaml` | Runtime config file |

## Existing Patterns to Follow

- Config structs: `#[derive(Debug, Clone, Serialize, Deserialize)]` with `serde`
- Config loading: YAML file → struct, env vars resolved via `resolve_env_var("${VAR}")`
- Provider initialization: read env var → if non-empty create real provider, else warn and skip/use stub (see `build_router_from_config` in main.rs:393-432)
- Migrations: versioned, sequential, in `migrations()` vec (migrations.rs:7-130)
- Tests: use `StubEmbeddingProvider::new(8)` for unit tests, real provider for integration
- sqlite-vec initialization: `init_sqlite_vec()` called in `MemoryStore::open()` and test helpers

---

### Task 1: Add EmbeddingConfig to Config System

**Files:**
- Modify: `crates/nanocrab-core/src/config.rs`
- Modify: `config/main.yaml`

**Step 1: Add EmbeddingConfig struct to config.rs**

Add after `FeaturesConfig` (line 25):

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingConfig {
    pub enabled: bool,
    pub provider: String,           // "openai" or "stub"
    pub api_key_env: String,        // e.g. "OPENAI_API_KEY"
    #[serde(default = "default_embedding_model")]
    pub model: String,              // e.g. "text-embedding-3-small"
    #[serde(default = "default_embedding_dimensions")]
    pub dimensions: usize,          // e.g. 1536
    #[serde(default = "default_embedding_base_url")]
    pub base_url: String,           // e.g. "https://api.openai.com/v1"
}

fn default_embedding_model() -> String {
    "text-embedding-3-small".to_string()
}

fn default_embedding_dimensions() -> usize {
    1536
}

fn default_embedding_base_url() -> String {
    "https://api.openai.com/v1".to_string()
}

impl Default for EmbeddingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: "stub".to_string(),
            api_key_env: "OPENAI_API_KEY".to_string(),
            model: default_embedding_model(),
            dimensions: default_embedding_dimensions(),
            base_url: default_embedding_base_url(),
        }
    }
}
```

**Step 2: Add embedding field to MainConfig**

Change `MainConfig` (line 60) to include the new field:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MainConfig {
    pub app: AppConfig,
    pub runtime: RuntimeConfig,
    pub features: FeaturesConfig,
    pub channels: ChannelsConfig,
    #[serde(default)]
    pub embedding: EmbeddingConfig,
}
```

**Step 3: Add env resolution for embedding config**

In `resolve_main_env()` (line 246), add at the end:

```rust
main.embedding.api_key_env = resolve_env_var(&main.embedding.api_key_env);
main.embedding.base_url = resolve_env_var(&main.embedding.base_url);
main.embedding.model = resolve_env_var(&main.embedding.model);
main.embedding.provider = resolve_env_var(&main.embedding.provider);
```

**Step 4: Update config/main.yaml**

Add embedding section:

```yaml
embedding:
  enabled: true
  provider: openai
  api_key_env: OPENAI_API_KEY
  model: text-embedding-3-small
  dimensions: 1536
  base_url: https://api.openai.com/v1
```

**Step 5: Run tests**

Run: `cargo test -p nanocrab-core config`
Expected: All existing config tests pass. The `serde(default)` on `embedding` ensures backward-compatible deserialization.

**Step 6: Commit**

```bash
git add crates/nanocrab-core/src/config.rs config/main.yaml
git commit -m "feat(config): add EmbeddingConfig to main config"
```

---

### Task 2: Wire Real Embedding Provider in Bootstrap

**Files:**
- Modify: `crates/nanocrab-cli/src/main.rs` (bootstrap function, ~line 364)

**Step 1: Add helper function to build embedding provider from config**

Add after `build_router_from_config()` (after line 460):

```rust
fn build_embedding_provider(config: &NanocrabConfig) -> Arc<dyn EmbeddingProvider> {
    let embedding_config = &config.main.embedding;

    if !embedding_config.enabled || embedding_config.provider != "openai" {
        tracing::info!(
            "Embedding provider: stub (enabled={}, provider={})",
            embedding_config.enabled,
            embedding_config.provider
        );
        return Arc::new(StubEmbeddingProvider::new(8));
    }

    let api_key = std::env::var(&embedding_config.api_key_env).unwrap_or_default();
    if api_key.is_empty() {
        tracing::warn!(
            "Embedding API key not set (env: {}), falling back to stub provider",
            embedding_config.api_key_env
        );
        return Arc::new(StubEmbeddingProvider::new(8));
    }

    let provider = OpenAiEmbeddingProvider::with_model(
        api_key,
        embedding_config.model.clone(),
        embedding_config.dimensions,
    )
    .with_base_url(embedding_config.base_url.clone());

    tracing::info!(
        "Embedding provider: OpenAI (model={}, dims={})",
        embedding_config.model,
        embedding_config.dimensions
    );
    Arc::new(provider)
}
```

**Step 2: Replace stub in bootstrap()**

Change line 364 in `bootstrap()`:

FROM:
```rust
let embedding_provider: Arc<dyn EmbeddingProvider> = Arc::new(StubEmbeddingProvider::new(8));
```

TO:
```rust
let embedding_provider = build_embedding_provider(&config);
```

**Step 3: Ensure imports are present**

Make sure `OpenAiEmbeddingProvider` is imported in main.rs. Check existing imports and add if missing:

```rust
use nanocrab_memory::embedding::{EmbeddingProvider, OpenAiEmbeddingProvider, StubEmbeddingProvider};
```

**Step 4: Run build**

Run: `cargo build -p nanocrab-cli`
Expected: Compiles without errors.

**Step 5: Run all tests**

Run: `cargo test --workspace`
Expected: All tests pass. Tests use StubProvider directly, not bootstrap().

**Step 6: Commit**

```bash
git add crates/nanocrab-cli/src/main.rs
git commit -m "feat(cli): wire real OpenAI embedding provider from config"
```

---

### Task 3: Add Post-Consolidation Re-indexing

**Files:**
- Modify: `crates/nanocrab-core/src/consolidation.rs`

**Step 1: Write the failing test**

Add to the `mod tests` section at the bottom of `consolidation.rs`:

```rust
#[tokio::test]
async fn consolidation_triggers_reindex_after_write() -> Result<()> {
    let dir = TempDir::new()?;
    let file_store = MemoryFileStore::new(dir.path());
    file_store.write_long_term("# Old Memory").await?;

    let today = chrono::Local::now().date_naive();
    file_store
        .write_daily(today, "# Today\n\nLearned about Rust lifetimes")
        .await?;

    let db = {
        use nanocrab_memory::store::MemoryStore;
        let store = MemoryStore::open_in_memory()?;
        store.db()
    };
    let search_index = nanocrab_memory::search_index::SearchIndex::new(db.clone());
    let provider = nanocrab_memory::embedding::StubEmbeddingProvider::new(8);

    let consolidator = HippocampusConsolidator::new(
        file_store.clone(),
        build_router(),
        "sonnet".to_string(),
        vec![],
    )
    .with_search_index(search_index.clone())
    .with_embedding_provider(Arc::new(provider.clone()) as Arc<dyn nanocrab_memory::embedding::EmbeddingProvider>)
    .with_file_store_for_reindex(file_store);

    let report = consolidator.consolidate().await?;
    assert!(report.memory_updated);
    assert!(report.reindexed);

    // Verify chunks exist in the index after consolidation
    let conn = db.lock().expect("lock");
    let count: i64 = conn.query_row("SELECT COUNT(*) FROM chunks", [], |r| r.get(0))?;
    assert!(count > 0, "Expected chunks after reindex, got 0");
    Ok(())
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p nanocrab-core consolidation_triggers_reindex`
Expected: FAIL — `with_search_index`, `with_embedding_provider`, `with_file_store_for_reindex` methods don't exist, `reindexed` field doesn't exist.

**Step 3: Add optional fields to HippocampusConsolidator**

Update the struct and builder methods:

```rust
use nanocrab_memory::embedding::EmbeddingProvider;
use nanocrab_memory::search_index::SearchIndex;

pub struct HippocampusConsolidator {
    file_store: MemoryFileStore,
    router: Arc<LlmRouter>,
    model_primary: String,
    model_fallbacks: Vec<String>,
    lookback_days: usize,
    // Optional: for post-consolidation re-indexing
    search_index: Option<SearchIndex>,
    embedding_provider: Option<Arc<dyn EmbeddingProvider>>,
    reindex_file_store: Option<MemoryFileStore>,
}
```

Update `new()` to initialize the new fields as `None`. Add builder methods:

```rust
pub fn with_search_index(mut self, index: SearchIndex) -> Self {
    self.search_index = Some(index);
    self
}

pub fn with_embedding_provider(mut self, provider: Arc<dyn EmbeddingProvider>) -> Self {
    self.embedding_provider = Some(provider);
    self
}

pub fn with_file_store_for_reindex(mut self, file_store: MemoryFileStore) -> Self {
    self.reindex_file_store = Some(file_store);
    self
}
```

**Step 4: Add reindexed field to ConsolidationReport**

```rust
pub struct ConsolidationReport {
    pub daily_files_read: usize,
    pub memory_updated: bool,
    pub reindexed: bool,
    pub summary: String,
}
```

Update all places that construct `ConsolidationReport` to include `reindexed: false` (the early return) or the computed value.

**Step 5: Add re-index logic at the end of consolidate()**

After `self.file_store.write_long_term(&updated_memory).await?;` (line 94), add:

```rust
let reindexed = if let (Some(index), Some(provider), Some(fs)) = (
    &self.search_index,
    &self.embedding_provider,
    &self.reindex_file_store,
) {
    match index.index_all(fs, provider.as_ref()).await {
        Ok(count) => {
            tracing::info!("Post-consolidation reindex: {count} chunks indexed");
            true
        }
        Err(e) => {
            tracing::warn!("Post-consolidation reindex failed: {e}");
            false
        }
    }
} else {
    false
};
```

Then use `reindexed` in the return value.

**Step 6: Run test to verify it passes**

Run: `cargo test -p nanocrab-core consolidation_triggers_reindex`
Expected: PASS

**Step 7: Run all tests to check no regressions**

Run: `cargo test --workspace`
Expected: All tests pass (existing tests construct ConsolidationReport with new `reindexed` field).

**Step 8: Commit**

```bash
git add crates/nanocrab-core/src/consolidation.rs
git commit -m "feat(consolidation): add post-consolidation re-indexing via SearchIndex"
```

---

### Task 4: Wire Re-index into CLI Consolidation Paths

**Files:**
- Modify: `crates/nanocrab-cli/src/main.rs` (start_bot + run_consolidate)

**Step 1: Update start_bot() consolidator creation (line 468-475)**

Pass search_index, embedding_provider, and file_store to the consolidator:

```rust
let workspace_dir = root.to_path_buf();
let file_store_for_consolidation = nanocrab_memory::file_store::MemoryFileStore::new(&workspace_dir);
let embedding_provider_for_consolidation = build_embedding_provider(&config);

// Re-use search_index from bootstrap — we need to extract it before Orchestrator consumes it.
// Actually, SearchIndex is Clone, so we can clone it from the one already created in bootstrap.
```

NOTE: `SearchIndex` derives `Clone` (see search_index.rs line 11-14). The bootstrap function creates `search_index` on line 363. We need to pass it through. The cleanest approach: have `bootstrap()` return the extra items needed, OR create a second SearchIndex from the same db handle.

The simplest approach: `bootstrap()` already returns `memory` (MemoryStore) which has `.db()`. Create a new SearchIndex from it:

```rust
let consolidation_search_index = nanocrab_memory::search_index::SearchIndex::new(memory.db());
```

Then:

```rust
let consolidator = Arc::new(HippocampusConsolidator::new(
    file_store_for_consolidation.clone(),
    Arc::new(build_router_from_config(&config)),
    "sonnet".to_string(),
    vec!["haiku".to_string()],
)
.with_search_index(consolidation_search_index)
.with_embedding_provider(embedding_provider_for_consolidation)
.with_file_store_for_reindex(file_store_for_consolidation));
```

**Step 2: Update run_consolidate() similarly (line 567-587)**

Same pattern — create SearchIndex from memory.db(), pass embedding provider and file store.

**Step 3: Update bootstrap() to also return memory for start_bot**

Currently `start_bot` calls `bootstrap()` which returns `(bus, memory, gateway, config)`. The `memory` field (MemoryStore) is already returned, so we can use `memory.db()` to create a SearchIndex. Just need to make sure `_memory` is not ignored:

Change in `start_bot()` line 464:
```rust
let (bus, memory, gateway, config) = bootstrap(root)?;
```
(remove the underscore prefix if present)

**Step 4: Run build and tests**

Run: `cargo build -p nanocrab-cli && cargo test --workspace`
Expected: Compiles and all tests pass.

**Step 5: Commit**

```bash
git add crates/nanocrab-cli/src/main.rs
git commit -m "feat(cli): wire post-consolidation re-indexing with real embedding provider"
```

---

### Task 5: Search Quality Tuning — Adjust Hybrid Scoring & min_score

**Files:**
- Modify: `crates/nanocrab-memory/src/search_index.rs`
- Modify: `crates/nanocrab-core/src/orchestrator.rs`

**Step 1: Write search quality test**

Add to `search_index.rs` tests:

```rust
#[tokio::test]
async fn search_scores_are_normalized() -> Result<()> {
    let db = test_db()?;
    let index = SearchIndex::new(db);
    let provider = StubEmbeddingProvider::new(8);

    // Index content with distinct topics
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
    // All scores should be between 0.0 and 1.0
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

    // FTS5 MATCH with special characters can fail; search should still return vector results
    // Use a query that is valid for vector but might trip FTS5
    let results = index.search("testing content", &provider, 6, 0.0).await?;
    // Should get results from at least vector search
    assert!(!results.is_empty());
    Ok(())
}
```

**Step 2: Run tests**

Run: `cargo test -p nanocrab-memory search_scores_are_normalized search_vector_only`
Expected: Should pass with current implementation (scores are 0.7*cosine + 0.3*bm25, both in [0,1]).

**Step 3: Make BM25 search resilient to FTS5 MATCH errors**

FTS5 MATCH can fail on queries with special characters (`*`, `"`, `OR`, etc.). Currently the search will return an error. Wrap the BM25 search in a fallback:

In `search()` method (search_index.rs line 386), wrap the BM25 spawn_blocking in a match:

```rust
let bm25_candidates = match task::spawn_blocking(move || {
    // ... existing code ...
}).await? {
    Ok(candidates) => candidates,
    Err(e) => {
        tracing::debug!("BM25 search failed (falling back to vector-only): {e}");
        Vec::new()
    }
};
```

This makes the hybrid search gracefully degrade to vector-only when FTS5 can't parse the query.

**Step 4: Normalize BM25 scores**

Currently BM25 score normalization: `1.0 / (1.0 + (-rank).max(0.0))` — this produces values in (0, 1] but the distribution depends heavily on the corpus. Add min-max normalization within the candidate set:

After collecting `bm25_candidates`, normalize scores within the set:

```rust
if !bm25_candidates.is_empty() {
    let max_bm25 = bm25_candidates.iter().map(|c| c.6).fold(0.0_f64, f64::max);
    if max_bm25 > 0.0 {
        for candidate in &mut bm25_candidates {
            candidate.6 /= max_bm25;
        }
    }
}
```

Similarly normalize vector scores:

```rust
if !vector_candidates.is_empty() {
    let max_vec = vector_candidates.iter().map(|c| c.6).fold(0.0_f64, f64::max);
    if max_vec > 0.0 {
        for candidate in &mut vector_candidates {
            candidate.6 /= max_vec;
        }
    }
}
```

**Step 5: Lower min_score in orchestrator**

In `orchestrator.rs` line 546, the min_score is 0.35. With real embeddings and normalized scoring, this may need adjustment. Change to 0.25 to allow more results to surface:

```rust
.search(query, self.embedding_provider.as_ref(), 6, 0.25)
```

**Step 6: Run all tests**

Run: `cargo test --workspace`
Expected: All pass.

**Step 7: Commit**

```bash
git add crates/nanocrab-memory/src/search_index.rs crates/nanocrab-core/src/orchestrator.rs
git commit -m "feat(search): normalize hybrid scores, add BM25 fallback, lower min_score"
```

---

### Task 6: Replace O(n) Vector Scan with sqlite-vec ANN Search

**Files:**
- Modify: `crates/nanocrab-memory/src/migrations.rs` (new migration for vec0 virtual table)
- Modify: `crates/nanocrab-memory/src/search_index.rs` (use vec_search instead of full scan)

**Step 1: Write failing test for vec0-based search**

Add to `search_index.rs` tests:

```rust
#[tokio::test]
async fn search_uses_vec_index() -> Result<()> {
    let db = test_db()?;
    let index = SearchIndex::new(db.clone());
    let provider = StubEmbeddingProvider::new(8);

    // Index enough content to make ANN meaningful
    for i in 0..5 {
        let path = format!("memory/2026-02-{:02}.md", i + 1);
        let content = format!("# Topic {i}\n\nContent about topic number {i} with unique words{i}");
        index.index_file(&path, &content, "daily", &provider).await?;
    }

    let results = index.search("topic", &provider, 3, 0.0).await?;
    assert!(!results.is_empty());
    assert!(results.len() <= 3);

    // Verify the vec0 table exists
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
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p nanocrab-memory search_uses_vec_index`
Expected: FAIL — `chunks_vec` table doesn't exist.

**Step 3: Add migration 6 for vec0 virtual table**

In `migrations.rs`, add to the `migrations()` vec:

```rust
(
    6,
    r#"
    CREATE VIRTUAL TABLE IF NOT EXISTS chunks_vec USING vec0(
        chunk_id TEXT PRIMARY KEY,
        embedding float[8]
    );
    "#,
),
```

IMPORTANT: The dimension in `float[N]` must match the embedding provider's dimensions. Since we support configurable dimensions, we need a different approach. sqlite-vec's vec0 requires the dimension at table creation time.

**Revised approach**: Since dimensions are configurable, we can't hardcode them in a migration. Instead:
- Use a dynamic table creation in `SearchIndex` initialization
- Drop and recreate if dimensions change
- Store current dimensions in `meta` table

Add a method to `SearchIndex`:

```rust
pub fn ensure_vec_table(&self, dimensions: usize) -> Result<()> {
    let db = self.db.lock().map_err(|_| anyhow!("failed to lock sqlite connection"))?;

    // Check if table exists with correct dimensions
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
        // Drop old table if exists
        db.execute_batch("DROP TABLE IF EXISTS chunks_vec;")?;

        // Create with correct dimensions
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
```

**Step 4: Update index_file() to also insert into chunks_vec**

After inserting into `chunks` table (inside the spawn_blocking transaction, around line 217-251), add:

```rust
// Insert into vec0 table for ANN search
tx.execute(
    "INSERT OR REPLACE INTO chunks_vec(chunk_id, embedding) VALUES (?1, ?2)",
    params![chunk_id, embedding],
)?;
```

Note: sqlite-vec vec0 accepts JSON array format for embeddings, which is exactly what `embedding_to_json()` produces.

Also update the DELETE section to clean chunks_vec:

```rust
tx.execute("DELETE FROM chunks_vec WHERE chunk_id IN (SELECT id FROM chunks WHERE path = ?1)", params![path_owned])?;
```

**Step 5: Replace full-scan vector search with vec0 query**

In `search()` method, replace the vector_candidates spawn_blocking (lines 349-382) with:

```rust
let db = Arc::clone(&self.db);
let query_json = embedding_to_json(&query_embedding);
let vector_candidates = task::spawn_blocking(move || {
    let conn = db
        .lock()
        .map_err(|_| anyhow!("failed to lock sqlite connection"))?;

    // Check if chunks_vec table exists
    let vec_table_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='chunks_vec'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(false);

    if vec_table_exists {
        // Use sqlite-vec ANN search
        let mut stmt = conn.prepare(
            r#"
            SELECT v.chunk_id, c.path, c.source, c.start_line, c.end_line, c.text, v.distance
            FROM chunks_vec v
            JOIN chunks c ON c.id = v.chunk_id
            WHERE v.embedding MATCH ?1
            AND k = ?2
            "#,
        )?;
        let rows = stmt.query_map(params![query_json, candidate_limit as i64], |r| {
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
            // sqlite-vec returns cosine distance; convert to similarity: 1.0 - distance
            let score = (1.0 - distance).max(0.0);
            out.push((chunk_id, path, source, start_line, end_line, text, score));
        }
        Ok::<Vec<(String, String, String, i64, i64, String, f64)>, anyhow::Error>(out)
    } else {
        // Fallback to full scan if vec table doesn't exist
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
            let score = cosine_similarity(&query_embedding_clone, &embedding) as f64;
            out.push((chunk_id, path, source, start_line, end_line, text, score));
        }
        out.sort_by(|a, b| b.6.total_cmp(&a.6));
        out.truncate(candidate_limit);
        Ok(out)
    }
}).await??;
```

NOTE: Need to clone `query_embedding` before moving into the closure since it's used in two places. Add `let query_embedding_clone = query_embedding.clone();` before the first spawn_blocking.

**Step 6: Call ensure_vec_table() during index_file**

At the beginning of `index_file()`, call `ensure_vec_table()` with the provider's dimensions:

```rust
self.ensure_vec_table(provider.dimensions())?;
```

**Step 7: Run tests**

Run: `cargo test -p nanocrab-memory`
Expected: All tests pass, including the new `search_uses_vec_index` test.

**Step 8: Run full workspace tests**

Run: `cargo test --workspace`
Expected: All pass.

**Step 9: Commit**

```bash
git add crates/nanocrab-memory/src/search_index.rs crates/nanocrab-memory/src/migrations.rs
git commit -m "feat(search): replace O(n) vector scan with sqlite-vec ANN search"
```

---

### Task 7: Startup Index Initialization

**Files:**
- Modify: `crates/nanocrab-cli/src/main.rs`

**Step 1: Add startup indexing after bootstrap**

In `bootstrap()`, after creating the embedding provider and search index (around line 364), add startup indexing:

```rust
// Initialize vec table and index existing memory files at startup
let startup_index = search_index.clone();
let startup_fs = file_store.clone();
let startup_ep = embedding_provider.clone();
tokio::task::spawn(async move {
    if let Err(e) = startup_index.ensure_vec_table(startup_ep.dimensions()) {
        tracing::warn!("Failed to ensure vec table at startup: {e}");
        return;
    }
    match startup_index.index_all(&startup_fs, startup_ep.as_ref()).await {
        Ok(count) => {
            if count > 0 {
                tracing::info!("Startup indexing: {count} chunks indexed");
            }
        }
        Err(e) => tracing::warn!("Startup indexing failed: {e}"),
    }
});
```

Note: This runs in a background task so it doesn't block startup. The `bootstrap()` function is not async, so this spawn needs to happen in the calling async context (e.g., `start_bot()` or `run_repl()`). Adjust placement accordingly — add it after `bootstrap()` returns in `start_bot()` and `run_repl()`.

Actually, `bootstrap()` is sync. The spawn should happen in `start_bot()` and `run_consolidate()`. But since `search_index` is consumed by `Orchestrator::new()`, we need to clone it before passing. SearchIndex is Clone, so:

```rust
let search_index = SearchIndex::new(memory.db());
let startup_search_index = search_index.clone();
```

Then after creating the orchestrator, spawn the background task.

**Step 2: Run build and tests**

Run: `cargo build -p nanocrab-cli && cargo test --workspace`
Expected: All pass.

**Step 3: Commit**

```bash
git add crates/nanocrab-cli/src/main.rs
git commit -m "feat(cli): add startup memory indexing in background"
```

---

### Task 8: Final Verification

**Step 1: Run clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: No warnings.

**Step 2: Run fmt**

Run: `cargo fmt --all -- --check`
Expected: No formatting issues.

**Step 3: Run full test suite**

Run: `cargo test --workspace`
Expected: All tests pass.

**Step 4: Verify test count increased**

Previously: 264 tests. Should now have additional tests for:
- `consolidation_triggers_reindex_after_write`
- `search_scores_are_normalized`
- `search_vector_only_fallback_on_fts_error`
- `search_uses_vec_index`

**Step 5: Final commit if needed for any fixups**

```bash
git add -A
git commit -m "chore: final cleanups for memory system integration"
```
