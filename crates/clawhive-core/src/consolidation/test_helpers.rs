use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use chrono::Utc;
use clawhive_memory::embedding::EmbeddingProvider;
use clawhive_memory::file_store::MemoryFileStore;
use clawhive_memory::store::MemoryStore;
use clawhive_provider::{LlmProvider, LlmRequest, LlmResponse, ProviderRegistry, StubProvider};
use tempfile::TempDir;

use crate::router::LlmRouter;

pub(super) fn build_router() -> Arc<LlmRouter> {
    let mut registry = ProviderRegistry::new();
    registry.register("anthropic", Arc::new(StubProvider));
    let aliases = HashMap::from([(
        "sonnet".to_string(),
        "anthropic/claude-sonnet-4-5".to_string(),
    )]);
    Arc::new(LlmRouter::new(registry, aliases, vec![]))
}

pub(super) fn build_file_store() -> Result<(TempDir, MemoryFileStore)> {
    let dir = TempDir::new()?;
    let store = MemoryFileStore::new(dir.path());
    Ok((dir, store))
}

pub(super) fn insert_chunk_access_count(
    memory_store: &Arc<MemoryStore>,
    agent_id: &str,
    path: &str,
    chunk_id: &str,
    access_count: i64,
) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    let hash = format!("hash-{chunk_id}");
    let sql = format!(
        "INSERT INTO chunks (id, path, source, start_line, end_line, hash, model, text, embedding, updated_at, created_at, access_count, agent_id, last_accessed) \
         VALUES ('{chunk_id}', '{path}', 'daily', 1, 1, '{hash}', '', 'chunk', '', '{now}', '{now}', {access_count}, '{agent_id}', NULL)"
    );
    let db = memory_store.db();
    let conn = db.lock().expect("lock db");
    conn.execute(&sql, [])?;
    Ok(())
}

pub(super) fn build_router_with_provider(provider: Arc<dyn LlmProvider>) -> Arc<LlmRouter> {
    let mut registry = ProviderRegistry::new();
    registry.register("anthropic", provider);
    let aliases = HashMap::from([(
        "sonnet".to_string(),
        "anthropic/claude-sonnet-4-5".to_string(),
    )]);
    Arc::new(LlmRouter::new(registry, aliases, vec![]))
}

pub(super) struct SequenceProvider {
    pub(super) responses: Vec<String>,
    pub(super) call_count: AtomicUsize,
}

impl SequenceProvider {
    pub(super) fn new(responses: Vec<String>) -> Arc<Self> {
        Arc::new(Self {
            responses,
            call_count: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl LlmProvider for SequenceProvider {
    async fn chat(&self, _request: LlmRequest) -> Result<LlmResponse> {
        let index = self.call_count.fetch_add(1, Ordering::SeqCst);
        let text = self.responses.get(index).cloned().unwrap_or_default();
        Ok(LlmResponse {
            text,
            content: vec![],
            input_tokens: None,
            output_tokens: None,
            stop_reason: Some("end_turn".to_string()),
        })
    }
}

pub(super) struct FailAtCallProvider {
    responses: Vec<String>,
    fail_at: usize,
    call_count: AtomicUsize,
}

impl FailAtCallProvider {
    pub(super) fn new(responses: Vec<String>, fail_at: usize) -> Arc<Self> {
        Arc::new(Self {
            responses,
            fail_at,
            call_count: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl LlmProvider for FailAtCallProvider {
    async fn chat(&self, _request: LlmRequest) -> Result<LlmResponse> {
        let index = self.call_count.fetch_add(1, Ordering::SeqCst);
        if index == self.fail_at {
            return Err(anyhow!("forced llm failure"));
        }
        let text = self.responses.get(index).cloned().unwrap_or_default();
        Ok(LlmResponse {
            text,
            content: vec![],
            input_tokens: None,
            output_tokens: None,
            stop_reason: Some("end_turn".to_string()),
        })
    }
}

pub(super) struct KeywordEmbeddingProvider;

#[async_trait]
impl EmbeddingProvider for KeywordEmbeddingProvider {
    async fn embed(
        &self,
        texts: &[String],
    ) -> anyhow::Result<clawhive_memory::embedding::EmbeddingResult> {
        let embeddings = texts
            .iter()
            .map(|text| {
                if text.contains("lives in") {
                    vec![1.0, 0.0]
                } else {
                    vec![0.0, 1.0]
                }
            })
            .collect();
        Ok(clawhive_memory::embedding::EmbeddingResult {
            embeddings,
            model: "keyword".to_string(),
            dimensions: 2,
        })
    }

    fn model_id(&self) -> &str {
        "keyword"
    }

    fn dimensions(&self) -> usize {
        2
    }
}

pub(super) struct StubEmbeddingProvider;

#[async_trait]
impl EmbeddingProvider for StubEmbeddingProvider {
    async fn embed(
        &self,
        texts: &[String],
    ) -> anyhow::Result<clawhive_memory::embedding::EmbeddingResult> {
        let embeddings = texts.iter().map(|_| vec![0.1; 384]).collect();
        Ok(clawhive_memory::embedding::EmbeddingResult {
            embeddings,
            model: "stub".to_string(),
            dimensions: 384,
        })
    }

    fn model_id(&self) -> &str {
        "stub"
    }

    fn dimensions(&self) -> usize {
        384
    }
}
