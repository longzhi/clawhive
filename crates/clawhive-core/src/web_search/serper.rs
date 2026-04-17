use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::provider::{SearchError, SearchProvider, SearchResult};

const SERPER_SEARCH_URL: &str = "https://google.serper.dev/search";

pub struct SerperSearchProvider {
    api_key: String,
    client: reqwest::Client,
}

impl SerperSearchProvider {
    pub fn new(api_key: String, client: reqwest::Client) -> Self {
        Self { api_key, client }
    }
}

#[derive(Serialize)]
struct SerperRequest {
    q: String,
    num: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    gl: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tbs: Option<String>,
}

#[derive(Deserialize)]
struct SerperResponse {
    #[serde(default)]
    organic: Vec<SerperOrganicResult>,
}

#[derive(Deserialize)]
struct SerperOrganicResult {
    title: String,
    link: String,
    #[serde(default)]
    snippet: String,
}

/// Map clawhive freshness codes to Serper tbs (Google time-based search) values.
fn map_freshness(freshness: &str) -> Option<String> {
    match freshness {
        "pd" | "d" => Some("qdr:d".into()),
        "pw" | "w" => Some("qdr:w".into()),
        "pm" | "m" => Some("qdr:m".into()),
        "py" | "y" => Some("qdr:y".into()),
        _ => None,
    }
}

#[async_trait]
impl SearchProvider for SerperSearchProvider {
    fn name(&self) -> &str {
        "serper"
    }

    async fn search(
        &self,
        query: &str,
        count: u8,
        country: Option<&str>,
        freshness: Option<&str>,
    ) -> std::result::Result<Vec<SearchResult>, SearchError> {
        let body = SerperRequest {
            q: query.to_string(),
            num: count,
            gl: country.map(|c| c.to_lowercase()),
            tbs: freshness.and_then(map_freshness),
        };

        let resp = self
            .client
            .post(SERPER_SEARCH_URL)
            .header("X-API-KEY", &self.api_key)
            .header("Content-Type", "application/json")
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
                "Serper API error (HTTP {status}): {body}"
            )));
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(SearchError::Fatal(format!(
                "Serper API error (HTTP {status}): {body}"
            )));
        }

        let serper_resp: SerperResponse = resp
            .json()
            .await
            .map_err(|e| SearchError::Fatal(format!("parse error: {e}")))?;

        Ok(serper_resp
            .organic
            .into_iter()
            .map(|r| SearchResult {
                title: r.title,
                url: r.link,
                description: r.snippet,
                age: None,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_is_serper() {
        let client = reqwest::Client::new();
        let p = SerperSearchProvider::new("test-key".into(), client);
        assert_eq!(p.name(), "serper");
    }
}
