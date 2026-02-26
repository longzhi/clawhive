use anyhow::{anyhow, Result};
use async_trait::async_trait;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;

use crate::MemoryStore;

#[derive(Debug, Clone)]
pub struct EmbeddingResult {
    pub embeddings: Vec<Vec<f32>>,
    pub model: String,
    pub dimensions: usize,
}

#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    async fn embed(&self, texts: &[String]) -> Result<EmbeddingResult>;
    fn model_id(&self) -> &str;
    fn dimensions(&self) -> usize;
}

#[derive(Clone)]
pub struct OpenAiEmbeddingProvider {
    client: reqwest::Client,
    api_key: String,
    model: String,
    dimensions: usize,
    base_url: String,
}

impl OpenAiEmbeddingProvider {
    pub fn new(api_key: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            model: "text-embedding-3-small".to_string(),
            dimensions: 1536,
            base_url: "https://api.openai.com/v1".to_string(),
        }
    }

    pub fn with_model(api_key: String, model: String, dimensions: usize) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            model,
            dimensions,
            base_url: "https://api.openai.com/v1".to_string(),
        }
    }

    pub fn with_base_url(mut self, base_url: String) -> Self {
        self.base_url = base_url;
        self
    }
}

#[async_trait]
impl EmbeddingProvider for OpenAiEmbeddingProvider {
    async fn embed(&self, texts: &[String]) -> Result<EmbeddingResult> {
        if texts.is_empty() {
            return Ok(EmbeddingResult {
                embeddings: Vec::new(),
                model: self.model.clone(),
                dimensions: self.dimensions,
            });
        }

        let endpoint = format!("{}/embeddings", self.base_url.trim_end_matches('/'));
        let request = OpenAiEmbeddingRequest {
            model: self.model.clone(),
            input: texts.to_vec(),
            encoding_format: "float".to_string(),
        };

        let response = self
            .client
            .post(endpoint)
            .header(AUTHORIZATION, format!("Bearer {}", self.api_key))
            .header(CONTENT_TYPE, "application/json")
            .json(&request)
            .send()
            .await?
            .error_for_status()?;

        let parsed: OpenAiEmbeddingResponse = response.json().await?;
        let model = parsed.model.clone();
        let embeddings = extract_ordered_embeddings(parsed)?;

        if embeddings.len() != texts.len() {
            return Err(anyhow!(
                "embedding count mismatch: expected {}, got {}",
                texts.len(),
                embeddings.len()
            ));
        }

        if embeddings.iter().any(|item| item.len() != self.dimensions) {
            return Err(anyhow!(
                "embedding dimensions mismatch with configured dimensions {}",
                self.dimensions
            ));
        }

        Ok(EmbeddingResult {
            embeddings,
            model,
            dimensions: self.dimensions,
        })
    }

    fn model_id(&self) -> &str {
        &self.model
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }
}

#[derive(Clone)]
pub struct StubEmbeddingProvider {
    dims: usize,
}

impl StubEmbeddingProvider {
    pub fn new(dims: usize) -> Self {
        Self { dims }
    }

    fn hash_to_unit_range(text: &str, index: usize) -> f32 {
        let mut hasher = Sha256::new();
        hasher.update(text.as_bytes());
        hasher.update(index.to_le_bytes());
        let hash = hasher.finalize();
        let value = u32::from_le_bytes([hash[0], hash[1], hash[2], hash[3]]);
        (value as f32 / u32::MAX as f32) * 2.0 - 1.0
    }
}

#[async_trait]
impl EmbeddingProvider for StubEmbeddingProvider {
    async fn embed(&self, texts: &[String]) -> Result<EmbeddingResult> {
        let embeddings = texts
            .iter()
            .map(|text| {
                (0..self.dims)
                    .map(|index| Self::hash_to_unit_range(text, index))
                    .collect::<Vec<f32>>()
            })
            .collect::<Vec<Vec<f32>>>();

        Ok(EmbeddingResult {
            embeddings,
            model: "stub".to_string(),
            dimensions: self.dims,
        })
    }

    fn model_id(&self) -> &str {
        "stub"
    }

    fn dimensions(&self) -> usize {
        self.dims
    }
}

#[derive(Debug, Clone, Serialize)]
struct OpenAiEmbeddingRequest {
    model: String,
    input: Vec<String>,
    encoding_format: String,
}

#[derive(Debug, Clone, Deserialize)]
struct OpenAiEmbeddingResponse {
    data: Vec<OpenAiEmbeddingData>,
    model: String,
    #[allow(dead_code)]
    usage: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize)]
struct OpenAiEmbeddingData {
    embedding: Vec<f32>,
    index: usize,
}

fn extract_ordered_embeddings(response: OpenAiEmbeddingResponse) -> Result<Vec<Vec<f32>>> {
    let mut data = response.data;
    data.sort_by_key(|item| item.index);

    for (expected_index, item) in data.iter().enumerate() {
        if item.index != expected_index {
            return Err(anyhow!(
                "missing or duplicated embedding index: expected {}, got {}",
                expected_index,
                item.index
            ));
        }
    }

    Ok(data
        .into_iter()
        .map(|item| item.embedding)
        .collect::<Vec<Vec<f32>>>())
}

/// Compute hash for cache key.
fn compute_text_hash(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    let result = hasher.finalize();
    hex::encode(&result[..16]) // Use first 16 bytes (32 hex chars)
}

/// Cached embedding provider that stores results in SQLite.
pub struct CachedEmbeddingProvider<P: EmbeddingProvider> {
    inner: P,
    store: Arc<MemoryStore>,
    provider_key: String,
}

impl<P: EmbeddingProvider> CachedEmbeddingProvider<P> {
    pub fn new(inner: P, store: Arc<MemoryStore>, provider_key: impl Into<String>) -> Self {
        Self {
            inner,
            store,
            provider_key: provider_key.into(),
        }
    }
}

#[async_trait]
impl<P: EmbeddingProvider + 'static> EmbeddingProvider for CachedEmbeddingProvider<P> {
    async fn embed(&self, texts: &[String]) -> Result<EmbeddingResult> {
        if texts.is_empty() {
            return Ok(EmbeddingResult {
                embeddings: Vec::new(),
                model: self.inner.model_id().to_string(),
                dimensions: self.inner.dimensions(),
            });
        }

        let provider = "openai"; // TODO: make this configurable
        let model = self.inner.model_id();
        let dims = self.inner.dimensions();

        // Check cache for each text
        let mut results: Vec<Option<Vec<f32>>> = Vec::with_capacity(texts.len());
        let mut uncached_indices: Vec<usize> = Vec::new();
        let mut uncached_texts: Vec<String> = Vec::new();

        for (i, text) in texts.iter().enumerate() {
            let hash = compute_text_hash(text);
            match self
                .store
                .get_embedding_cache(provider, model, &self.provider_key, &hash)
                .await
            {
                Ok(Some(embedding)) => {
                    results.push(Some(embedding));
                }
                Ok(None) | Err(_) => {
                    results.push(None);
                    uncached_indices.push(i);
                    uncached_texts.push(text.clone());
                }
            }
        }

        // Fetch uncached embeddings
        if !uncached_texts.is_empty() {
            let fresh = self.inner.embed(&uncached_texts).await?;

            // Store in cache and fill results
            for (idx, (text, embedding)) in uncached_indices
                .iter()
                .zip(uncached_texts.iter().zip(fresh.embeddings.iter()))
            {
                let hash = compute_text_hash(text);
                let _ = self
                    .store
                    .set_embedding_cache(
                        provider,
                        model,
                        &self.provider_key,
                        &hash,
                        embedding,
                        dims,
                    )
                    .await;
                results[*idx] = Some(embedding.clone());
            }
        }

        // Unwrap all results
        let embeddings: Vec<Vec<f32>> = results
            .into_iter()
            .map(|r| r.expect("all embeddings should be filled"))
            .collect();

        Ok(EmbeddingResult {
            embeddings,
            model: model.to_string(),
            dimensions: dims,
        })
    }

    fn model_id(&self) -> &str {
        self.inner.model_id()
    }

    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stub_provider_returns_correct_dims() {
        let provider = StubEmbeddingProvider::new(8);
        let inputs = vec!["hello".to_string()];
        let result = provider.embed(&inputs).await.expect("stub embed");

        assert_eq!(result.embeddings.len(), 1);
        assert_eq!(result.embeddings[0].len(), 8);
        assert_eq!(result.dimensions, 8);
    }

    #[tokio::test]
    async fn stub_provider_deterministic() {
        let provider = StubEmbeddingProvider::new(6);
        let inputs = vec!["same input".to_string()];

        let first = provider.embed(&inputs).await.expect("first");
        let second = provider.embed(&inputs).await.expect("second");

        assert_eq!(first.embeddings, second.embeddings);
    }

    #[tokio::test]
    async fn stub_provider_batch() {
        let provider = StubEmbeddingProvider::new(4);
        let inputs = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let result = provider.embed(&inputs).await.expect("batch");

        assert_eq!(result.embeddings.len(), 3);
        assert_eq!(result.embeddings.iter().filter(|v| v.len() == 4).count(), 3);
    }

    #[test]
    fn stub_provider_model_id() {
        let provider = StubEmbeddingProvider::new(16);
        assert_eq!(provider.model_id(), "stub");
        assert_eq!(provider.dimensions(), 16);
    }

    #[test]
    fn openai_provider_default_model() {
        let provider = OpenAiEmbeddingProvider::new("k".to_string());
        assert_eq!(provider.model_id(), "text-embedding-3-small");
        assert_eq!(provider.dimensions(), 1536);
    }

    #[test]
    fn openai_provider_custom_model() {
        let provider =
            OpenAiEmbeddingProvider::with_model("k".to_string(), "custom-model".to_string(), 256);
        assert_eq!(provider.model_id(), "custom-model");
        assert_eq!(provider.dimensions(), 256);
    }

    #[test]
    fn openai_provider_custom_base_url() {
        let provider = OpenAiEmbeddingProvider::new("k".to_string())
            .with_base_url("http://localhost:1234/v1".to_string());
        assert_eq!(provider.base_url, "http://localhost:1234/v1");
    }

    #[test]
    fn openai_provider_request_format() {
        let request = OpenAiEmbeddingRequest {
            model: "text-embedding-3-small".to_string(),
            input: vec!["hello".to_string(), "world".to_string()],
            encoding_format: "float".to_string(),
        };
        let json = serde_json::to_value(request).expect("serialize request");

        assert_eq!(json["model"], "text-embedding-3-small");
        assert_eq!(json["encoding_format"], "float");
        assert_eq!(json["input"][0], "hello");
        assert_eq!(json["input"][1], "world");
    }

    #[test]
    fn openai_provider_response_parsing() {
        let raw = r#"{
            "data": [
                {"embedding": [0.1, 0.2, 0.3], "index": 0},
                {"embedding": [0.4, 0.5, 0.6], "index": 1}
            ],
            "model": "text-embedding-3-small",
            "usage": {"prompt_tokens": 10, "total_tokens": 10}
        }"#;

        let parsed: OpenAiEmbeddingResponse = serde_json::from_str(raw).expect("parse response");
        assert_eq!(parsed.model, "text-embedding-3-small");
        assert_eq!(parsed.data.len(), 2);
        assert_eq!(parsed.data[0].embedding.len(), 3);
        assert_eq!(parsed.usage["total_tokens"], 10);
    }

    #[test]
    fn openai_provider_response_reordered() {
        let response = OpenAiEmbeddingResponse {
            data: vec![
                OpenAiEmbeddingData {
                    embedding: vec![0.9, 0.8],
                    index: 2,
                },
                OpenAiEmbeddingData {
                    embedding: vec![0.1, 0.2],
                    index: 0,
                },
                OpenAiEmbeddingData {
                    embedding: vec![0.5, 0.6],
                    index: 1,
                },
            ],
            model: "text-embedding-3-small".to_string(),
            usage: serde_json::json!({"prompt_tokens": 10, "total_tokens": 10}),
        };

        let ordered = extract_ordered_embeddings(response).expect("ordered embeddings");
        assert_eq!(ordered[0], vec![0.1, 0.2]);
        assert_eq!(ordered[1], vec![0.5, 0.6]);
        assert_eq!(ordered[2], vec![0.9, 0.8]);
    }
}
