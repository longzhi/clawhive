use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use nanocrab_memory::embedding::EmbeddingProvider;
use nanocrab_memory::file_store::MemoryFileStore;
use nanocrab_memory::search_index::SearchIndex;
use nanocrab_provider::ToolDef;

use super::tool::{ToolExecutor, ToolOutput};

pub struct MemorySearchTool {
    search_index: SearchIndex,
    embedding_provider: Arc<dyn EmbeddingProvider>,
}

impl MemorySearchTool {
    pub fn new(search_index: SearchIndex, embedding_provider: Arc<dyn EmbeddingProvider>) -> Self {
        Self {
            search_index,
            embedding_provider,
        }
    }
}

#[async_trait]
impl ToolExecutor for MemorySearchTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "memory_search".into(),
            description: "Search through long-term memory using semantic and keyword search. Returns relevant memory chunks ranked by relevance.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "The search query to find relevant memories"
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "Maximum number of results (default: 6)",
                        "default": 6
                    }
                },
                "required": ["query"]
            }),
        }
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolOutput> {
        let query = input["query"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing 'query' field"))?;
        let max_results = input["max_results"].as_u64().unwrap_or(6) as usize;

        match self
            .search_index
            .search(query, self.embedding_provider.as_ref(), max_results, 0.35)
            .await
        {
            Ok(results) if results.is_empty() => Ok(ToolOutput {
                content: "No relevant memories found.".into(),
                is_error: false,
            }),
            Ok(results) => {
                let mut output = String::new();
                for r in &results {
                    output.push_str(&format!(
                        "## {} (score: {:.2})\n{}\n\n",
                        r.path, r.score, r.text
                    ));
                }
                Ok(ToolOutput {
                    content: output,
                    is_error: false,
                })
            }
            Err(e) => Ok(ToolOutput {
                content: format!("Search failed: {e}"),
                is_error: true,
            }),
        }
    }
}

pub struct MemoryGetTool {
    file_store: MemoryFileStore,
}

impl MemoryGetTool {
    pub fn new(file_store: MemoryFileStore) -> Self {
        Self { file_store }
    }
}

#[async_trait]
impl ToolExecutor for MemoryGetTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "memory_get".into(),
            description: "Retrieve a specific memory file by key. Use 'MEMORY.md' for long-term memory, or 'YYYY-MM-DD' for a daily file.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "key": {
                        "type": "string",
                        "description": "The memory key: 'MEMORY.md' for long-term, or 'YYYY-MM-DD' for daily file"
                    }
                },
                "required": ["key"]
            }),
        }
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolOutput> {
        let key = input["key"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing 'key' field"))?;

        if key == "MEMORY.md" {
            match self.file_store.read_long_term().await {
                Ok(content) => Ok(ToolOutput {
                    content,
                    is_error: false,
                }),
                Err(e) => Ok(ToolOutput {
                    content: format!("Failed to read MEMORY.md: {e}"),
                    is_error: true,
                }),
            }
        } else if let Ok(date) = chrono::NaiveDate::parse_from_str(key, "%Y-%m-%d") {
            match self.file_store.read_daily(date).await {
                Ok(Some(content)) => Ok(ToolOutput {
                    content,
                    is_error: false,
                }),
                Ok(None) => Ok(ToolOutput {
                    content: format!("No daily file for {key}"),
                    is_error: false,
                }),
                Err(e) => Ok(ToolOutput {
                    content: format!("Failed to read daily file: {e}"),
                    is_error: true,
                }),
            }
        } else {
            Ok(ToolOutput {
                content: format!("Unknown memory key: {key}. Use 'MEMORY.md' or 'YYYY-MM-DD'."),
                is_error: true,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nanocrab_memory::embedding::StubEmbeddingProvider;
    use nanocrab_memory::search_index::SearchIndex;
    use nanocrab_memory::{file_store::MemoryFileStore, MemoryStore};
    use std::sync::Arc;
    use tempfile::TempDir;

    fn setup() -> (TempDir, MemorySearchTool, MemoryGetTool) {
        let tmp = TempDir::new().unwrap();
        let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
        let search_index = SearchIndex::new(memory.db());
        let embedding: Arc<dyn EmbeddingProvider> = Arc::new(StubEmbeddingProvider::new(8));
        let file_store = MemoryFileStore::new(tmp.path());

        let search_tool = MemorySearchTool::new(search_index, embedding);
        let get_tool = MemoryGetTool::new(file_store);
        (tmp, search_tool, get_tool)
    }

    #[test]
    fn memory_search_tool_definition() {
        let (_tmp, tool, _) = setup();
        let def = tool.definition();
        assert_eq!(def.name, "memory_search");
        assert!(def.input_schema["properties"]["query"].is_object());
    }

    #[test]
    fn memory_get_tool_definition() {
        let (_tmp, _, tool) = setup();
        let def = tool.definition();
        assert_eq!(def.name, "memory_get");
        assert!(def.input_schema["properties"]["key"].is_object());
    }

    #[tokio::test]
    async fn memory_search_returns_results() {
        let (_tmp, tool, _) = setup();
        let result = tool
            .execute(serde_json::json!({"query": "test query"}))
            .await
            .unwrap();
        // With empty index, should return empty but not error
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn memory_get_long_term() {
        let (tmp, _, tool) = setup();
        let file_store = MemoryFileStore::new(tmp.path());
        file_store
            .write_long_term("# Long term memory")
            .await
            .unwrap();

        let result = tool
            .execute(serde_json::json!({"key": "MEMORY.md"}))
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("Long term memory"));
    }
}
