use anyhow::{anyhow, Result};
use async_trait::async_trait;
use nanocrab_provider::ToolDef;
use serde::Deserialize;

use super::tool::{ToolContext, ToolExecutor, ToolOutput};

const BRAVE_SEARCH_URL: &str = "https://api.search.brave.com/res/v1/web/search";
const DEFAULT_COUNT: u64 = 5;
const DEFAULT_TIMEOUT_SECS: u64 = 15;

pub struct WebSearchTool {
    api_key: String,
    client: reqwest::Client,
}

impl WebSearchTool {
    pub fn new(api_key: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(DEFAULT_TIMEOUT_SECS))
            .build()
            .unwrap_or_default();
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
impl ToolExecutor for WebSearchTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "web_search".into(),
            description: "Search the web using Brave Search. Returns titles, URLs, and descriptions of relevant web pages.".into(),
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
            .ok_or_else(|| anyhow!("missing 'query' field"))?;
        let count = input["count"].as_u64().unwrap_or(DEFAULT_COUNT).clamp(1, 10);
        let country = input["country"].as_str();
        let freshness = input["freshness"].as_str();

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
            .await;

        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                return Ok(ToolOutput {
                    content: format!("Search request failed: {e}"),
                    is_error: true,
                });
            }
        };

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Ok(ToolOutput {
                content: format!("Brave Search API error (HTTP {status}): {body}"),
                is_error: true,
            });
        }

        let search_resp: BraveSearchResponse = match resp.json().await {
            Ok(r) => r,
            Err(e) => {
                return Ok(ToolOutput {
                    content: format!("Failed to parse search response: {e}"),
                    is_error: true,
                });
            }
        };

        let results = search_resp
            .web
            .map(|w| w.results)
            .unwrap_or_default();

        if results.is_empty() {
            return Ok(ToolOutput {
                content: format!("No results found for: {query}"),
                is_error: false,
            });
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

        Ok(ToolOutput {
            content: output,
            is_error: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definition_has_correct_name() {
        let tool = WebSearchTool::new("test-key".into());
        let def = tool.definition();
        assert_eq!(def.name, "web_search");
        assert!(def.description.contains("Brave Search"));
    }

    #[test]
    fn definition_requires_query() {
        let tool = WebSearchTool::new("test-key".into());
        let def = tool.definition();
        let required = def.input_schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v.as_str() == Some("query")));
    }
}
