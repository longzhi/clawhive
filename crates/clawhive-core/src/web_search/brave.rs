use async_trait::async_trait;
use serde::Deserialize;

use super::provider::{SearchError, SearchProvider, SearchResult};

const BRAVE_SEARCH_URL: &str = "https://api.search.brave.com/res/v1/web/search";

pub struct BraveSearchProvider {
    api_key: String,
    client: reqwest::Client,
}

impl BraveSearchProvider {
    pub fn new(api_key: String, client: reqwest::Client) -> Self {
        Self { api_key, client }
    }
}

#[derive(Deserialize)]
struct BraveSearchResponse {
    web: Option<BraveWebResults>,
}

#[derive(Deserialize)]
struct BraveWebResults {
    results: Vec<BraveWebResult>,
}

#[derive(Deserialize)]
struct BraveWebResult {
    title: String,
    url: String,
    description: String,
    #[serde(default)]
    age: Option<String>,
}

#[async_trait]
impl SearchProvider for BraveSearchProvider {
    fn name(&self) -> &str {
        "brave"
    }

    async fn search(
        &self,
        query: &str,
        count: u8,
        country: Option<&str>,
        freshness: Option<&str>,
    ) -> std::result::Result<Vec<SearchResult>, SearchError> {
        let mut params = vec![
            ("q".to_string(), query.to_string()),
            ("count".to_string(), count.to_string()),
        ];
        if let Some(c) = country {
            params.push(("country".to_string(), c.to_string()));
        }
        if let Some(f) = freshness {
            params.push(("freshness".to_string(), f.to_string()));
        }

        let resp = self
            .client
            .get(BRAVE_SEARCH_URL)
            .header("X-Subscription-Token", &self.api_key)
            .header("Accept", "application/json")
            .query(&params)
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
                "Brave Search API error (HTTP {status}): {body}"
            )));
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(SearchError::Fatal(format!(
                "Brave Search API error (HTTP {status}): {body}"
            )));
        }

        let search_resp: BraveSearchResponse = resp
            .json()
            .await
            .map_err(|e| SearchError::Fatal(format!("parse error: {e}")))?;

        Ok(search_resp
            .web
            .map(|w| w.results)
            .unwrap_or_default()
            .into_iter()
            .map(|r| SearchResult {
                title: r.title,
                url: r.url,
                description: r.description,
                age: r.age,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_brave() {
        let client = reqwest::Client::new();
        let p = BraveSearchProvider::new("test-key".into(), client);
        assert_eq!(p.name(), "brave");
    }
}
