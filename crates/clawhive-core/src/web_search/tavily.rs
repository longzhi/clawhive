use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::provider::{SearchError, SearchProvider, SearchResult};

const TAVILY_SEARCH_URL: &str = "https://api.tavily.com/search";

pub struct TavilySearchProvider {
    api_key: String,
    client: reqwest::Client,
}

impl TavilySearchProvider {
    pub fn new(api_key: String, client: reqwest::Client) -> Self {
        Self { api_key, client }
    }
}

#[derive(Serialize)]
struct TavilyRequest {
    query: String,
    max_results: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    time_range: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    country: Option<String>,
}

#[derive(Deserialize)]
struct TavilyResponse {
    #[serde(default)]
    results: Vec<TavilyResult>,
}

#[derive(Deserialize)]
struct TavilyResult {
    title: String,
    url: String,
    content: String,
    #[allow(dead_code)]
    score: f64,
}

/// Map clawhive freshness codes to Tavily time_range values.
fn map_freshness(freshness: &str) -> Option<String> {
    match freshness {
        "pd" | "d" => Some("day".into()),
        "pw" | "w" => Some("week".into()),
        "pm" | "m" => Some("month".into()),
        "py" | "y" => Some("year".into()),
        _ => None,
    }
}

#[async_trait]
impl SearchProvider for TavilySearchProvider {
    fn name(&self) -> &str {
        "tavily"
    }

    async fn search(
        &self,
        query: &str,
        count: u8,
        country: Option<&str>,
        freshness: Option<&str>,
    ) -> std::result::Result<Vec<SearchResult>, SearchError> {
        let body = TavilyRequest {
            query: query.to_string(),
            max_results: count,
            time_range: freshness.and_then(map_freshness),
            country: country.map(|c| c.to_string()),
        };

        let resp = self
            .client
            .post(TAVILY_SEARCH_URL)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    SearchError::Retryable(format!("timeout: {e}"))
                } else {
                    SearchError::Retryable(format!("request failed: {e}"))
                }
            })?;

        let status = resp.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            let body = resp.text().await.unwrap_or_default();
            return Err(SearchError::Retryable(format!(
                "Tavily API error (HTTP {status}): {body}"
            )));
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(SearchError::Fatal(format!(
                "Tavily API error (HTTP {status}): {body}"
            )));
        }

        let tavily_resp: TavilyResponse = resp
            .json()
            .await
            .map_err(|e| SearchError::Fatal(format!("parse error: {e}")))?;

        Ok(tavily_resp
            .results
            .into_iter()
            .map(|r| SearchResult {
                title: r.title,
                url: r.url,
                description: r.content,
                age: None,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_tavily() {
        let client = reqwest::Client::new();
        let p = TavilySearchProvider::new("test-key".into(), client);
        assert_eq!(p.name(), "tavily");
    }
}
