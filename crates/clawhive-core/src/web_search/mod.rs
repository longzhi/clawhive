pub mod brave;
pub mod circuit_breaker;
pub mod provider;
pub mod serper;
pub mod tavily;

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use clawhive_provider::ToolDef;
use tokio::sync::Mutex;

use crate::tool::{ToolContext, ToolExecutor, ToolOutput};
use circuit_breaker::CircuitBreaker;
use provider::{SearchError, SearchProvider, SearchResult};

const DEFAULT_COUNT: u64 = 5;

pub struct WebSearchTool {
    providers: Vec<Arc<dyn SearchProvider>>,
    circuit_breaker: Mutex<CircuitBreaker>,
}

impl WebSearchTool {
    pub fn new(providers: Vec<Arc<dyn SearchProvider>>) -> Self {
        Self {
            providers,
            circuit_breaker: Mutex::new(CircuitBreaker::new()),
        }
    }

    fn format_results(query: &str, results: &[SearchResult]) -> String {
        if results.is_empty() {
            return format!("No results found for: {query}");
        }
        let mut output = format!("Search results for: {query}\n\n");
        for (i, r) in results.iter().enumerate() {
            output.push_str(&format!("{}. **{}**\n", i + 1, r.title));
            output.push_str(&format!("   {}\n", r.url));
            output.push_str(&format!("   {}\n", r.description));
            if let Some(ref age) = r.age {
                output.push_str(&format!("   Age: {age}\n"));
            }
            output.push('\n');
        }
        output
    }
}

#[async_trait]
impl ToolExecutor for WebSearchTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "web_search".into(),
            description:
                "Search the web. Returns titles, URLs, and descriptions of relevant web pages."
                    .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The search query"
                    },
                    "count": {
                        "type": "integer",
                        "description": "Number of results (1-10, default 5)",
                        "minimum": 1,
                        "maximum": 10
                    },
                    "country": {
                        "type": "string",
                        "description": "Country code for search region (e.g. US, CN, JP)"
                    },
                    "freshness": {
                        "type": "string",
                        "description": "Time filter: pd (past day), pw (past week), pm (past month), py (past year)"
                    }
                },
                "required": ["query"]
            }),
        }
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let query = input["query"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing 'query' field"))?;
        let count = input["count"]
            .as_u64()
            .unwrap_or(DEFAULT_COUNT)
            .clamp(1, 10) as u8;
        let country = input["country"].as_str();
        let freshness = input["freshness"].as_str();

        let mut errors = Vec::new();

        for provider in &self.providers {
            let available = {
                let cb = self.circuit_breaker.lock().await;
                cb.is_available(provider.name())
            };
            if !available {
                tracing::debug!(
                    provider = provider.name(),
                    "skipped (circuit breaker cooldown)"
                );
                continue;
            }

            match provider.search(query, count, country, freshness).await {
                Ok(results) => {
                    {
                        let mut cb = self.circuit_breaker.lock().await;
                        cb.record_success(provider.name());
                    }
                    tracing::info!(
                        provider = provider.name(),
                        results = results.len(),
                        "web search succeeded"
                    );
                    return Ok(ToolOutput {
                        content: Self::format_results(query, &results),
                        is_error: false,
                    });
                }
                Err(SearchError::Retryable(msg)) => {
                    {
                        let mut cb = self.circuit_breaker.lock().await;
                        cb.record_failure(provider.name());
                    }
                    tracing::warn!(
                        provider = provider.name(),
                        error = %msg,
                        "retryable error, trying next provider"
                    );
                    errors.push(format!("{}: {msg}", provider.name()));
                }
                Err(SearchError::Fatal(msg)) => {
                    tracing::error!(
                        provider = provider.name(),
                        error = %msg,
                        "fatal error, not retrying"
                    );
                    return Ok(ToolOutput {
                        content: format!("Search failed ({}): {msg}", provider.name()),
                        is_error: true,
                    });
                }
            }
        }

        Ok(ToolOutput {
            content: format!("All search providers failed:\n{}", errors.join("\n")),
            is_error: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct MockProvider {
        provider_name: String,
        fail_with: Option<SearchError>,
        call_count: AtomicU32,
    }

    impl MockProvider {
        fn succeeding(name: &str) -> Self {
            Self {
                provider_name: name.into(),
                fail_with: None,
                call_count: AtomicU32::new(0),
            }
        }

        fn failing_retryable(name: &str, msg: &str) -> Self {
            Self {
                provider_name: name.into(),
                fail_with: Some(SearchError::Retryable(msg.into())),
                call_count: AtomicU32::new(0),
            }
        }

        fn failing_fatal(name: &str, msg: &str) -> Self {
            Self {
                provider_name: name.into(),
                fail_with: Some(SearchError::Fatal(msg.into())),
                call_count: AtomicU32::new(0),
            }
        }

        fn calls(&self) -> u32 {
            self.call_count.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl SearchProvider for MockProvider {
        fn name(&self) -> &str {
            &self.provider_name
        }

        async fn search(
            &self,
            _q: &str,
            _count: u8,
            _country: Option<&str>,
            _freshness: Option<&str>,
        ) -> std::result::Result<Vec<SearchResult>, SearchError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            match &self.fail_with {
                Some(err) => Err(err.clone()),
                None => Ok(vec![SearchResult {
                    title: "Test".into(),
                    url: "https://example.com".into(),
                    description: "Test result".into(),
                    age: None,
                }]),
            }
        }
    }

    #[tokio::test]
    async fn first_provider_succeeds() {
        let p1 = Arc::new(MockProvider::succeeding("p1"));
        let p2 = Arc::new(MockProvider::succeeding("p2"));
        let tool = WebSearchTool::new(vec![p1.clone() as Arc<dyn SearchProvider>, p2.clone()]);
        let ctx = ToolContext::builtin();
        let input = serde_json::json!({"query": "test"});
        let output = tool.execute(input, &ctx).await.unwrap();
        assert!(!output.is_error);
        assert_eq!(p1.calls(), 1);
        assert_eq!(p2.calls(), 0);
    }

    #[tokio::test]
    async fn fallback_on_retryable_error() {
        let p1 = Arc::new(MockProvider::failing_retryable("p1", "429"));
        let p2 = Arc::new(MockProvider::succeeding("p2"));
        let tool = WebSearchTool::new(vec![p1.clone() as Arc<dyn SearchProvider>, p2.clone()]);
        let ctx = ToolContext::builtin();
        let input = serde_json::json!({"query": "test"});
        let output = tool.execute(input, &ctx).await.unwrap();
        assert!(!output.is_error);
        assert_eq!(p1.calls(), 1);
        assert_eq!(p2.calls(), 1);
    }

    #[tokio::test]
    async fn no_fallback_on_fatal_error() {
        let p1 = Arc::new(MockProvider::failing_fatal("p1", "401 unauthorized"));
        let p2 = Arc::new(MockProvider::succeeding("p2"));
        let tool = WebSearchTool::new(vec![p1.clone() as Arc<dyn SearchProvider>, p2.clone()]);
        let ctx = ToolContext::builtin();
        let input = serde_json::json!({"query": "test"});
        let output = tool.execute(input, &ctx).await.unwrap();
        assert!(output.is_error);
        assert_eq!(p1.calls(), 1);
        assert_eq!(p2.calls(), 0);
    }

    #[tokio::test]
    async fn all_providers_fail() {
        let p1 = Arc::new(MockProvider::failing_retryable("p1", "429"));
        let p2 = Arc::new(MockProvider::failing_retryable("p2", "500"));
        let tool = WebSearchTool::new(vec![p1.clone() as Arc<dyn SearchProvider>, p2.clone()]);
        let ctx = ToolContext::builtin();
        let input = serde_json::json!({"query": "test"});
        let output = tool.execute(input, &ctx).await.unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("All search providers failed"));
    }
}
