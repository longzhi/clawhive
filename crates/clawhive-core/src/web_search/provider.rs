use async_trait::async_trait;

/// Unified search result returned by all providers.
#[derive(Debug, Clone)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub description: String,
    pub age: Option<String>,
}

/// Errors that can occur during search.
#[derive(Debug, Clone, thiserror::Error)]
pub enum SearchError {
    /// Retryable: rate limit, server error, timeout — triggers fallback.
    #[error("retryable: {0}")]
    Retryable(String),
    /// Non-retryable: bad API key, invalid request — does NOT trigger fallback.
    #[error("fatal: {0}")]
    Fatal(String),
}

/// Trait implemented by each search provider (Brave, Tavily, Serper).
#[async_trait]
pub trait SearchProvider: Send + Sync {
    fn name(&self) -> &str;

    async fn search(
        &self,
        query: &str,
        count: u8,
        country: Option<&str>,
        freshness: Option<&str>,
    ) -> std::result::Result<Vec<SearchResult>, SearchError>;
}
