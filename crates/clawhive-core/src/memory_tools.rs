use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use clawhive_memory::embedding::EmbeddingProvider;
use clawhive_memory::fact_store::{self, Fact, FactStore};
use clawhive_memory::file_store::MemoryFileStore;
use clawhive_memory::search_index::{SearchIndex, TimeRange};
use clawhive_memory::{MemoryStore, RecentExplicitMemoryWrite, SessionMemoryStateRecord};
use clawhive_provider::ToolDef;

use crate::memory_document::MemoryDocument;

use super::memory_retrieval::{
    classify_chunk_source, find_matching_fact, search_memory, source_label, MemoryHit,
    MemorySearchParams,
};
use super::tool::{ToolContext, ToolExecutor, ToolOutput};

pub struct MemorySearchTool {
    fact_store: FactStore,
    search_index: SearchIndex,
    embedding_provider: Arc<dyn EmbeddingProvider>,
    agent_id: String,
}

impl MemorySearchTool {
    pub fn new(
        fact_store: FactStore,
        search_index: SearchIndex,
        embedding_provider: Arc<dyn EmbeddingProvider>,
        agent_id: String,
    ) -> Self {
        Self {
            fact_store,
            search_index,
            embedding_provider,
            agent_id,
        }
    }
}

#[async_trait]
impl ToolExecutor for MemorySearchTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "memory_search".into(),
            description: "Search through remembered facts and indexed memory. Returns results with source labels. Use memory_get to read full content of interesting files.".into(),
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
                    },
                    "time_range": {
                        "type": "object",
                        "description": "Optional occurred_at/date filter. Supports YYYY-MM or YYYY-MM-DD.",
                        "properties": {
                            "from": {
                                "type": "string",
                                "description": "Start date (inclusive), e.g. 2026-03 or 2026-03-15"
                            },
                            "to": {
                                "type": "string",
                                "description": "End date (inclusive), e.g. 2026-03 or 2026-03-31"
                            }
                        }
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
        let max_results = input["max_results"].as_u64().unwrap_or(6) as usize;
        let time_range = input
            .get("time_range")
            .and_then(|v| v.as_object())
            .map(|obj| TimeRange {
                from: obj
                    .get("from")
                    .and_then(|v| v.as_str())
                    .map(ToOwned::to_owned),
                to: obj
                    .get("to")
                    .and_then(|v| v.as_str())
                    .map(ToOwned::to_owned),
            });

        match search_memory(
            &self.fact_store,
            &self.search_index,
            self.embedding_provider.as_ref(),
            &self.agent_id,
            query,
            MemorySearchParams {
                max_results,
                min_score: 0.35,
                time_range,
            },
        )
        .await
        {
            Ok(results) if results.is_empty() => Ok(ToolOutput {
                content: "No relevant memories found.".into(),
                is_error: false,
            }),
            Ok(results) => {
                let mut output = String::new();
                for r in &results {
                    output.push_str(&format_memory_hit(r));
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

fn format_memory_hit(hit: &MemoryHit) -> String {
    match hit {
        MemoryHit::Fact(hit) => {
            let snippet: String = hit.fact.content.chars().take(200).collect();
            let truncated = if hit.fact.content.chars().count() > 200 {
                "..."
            } else {
                ""
            };
            format!(
                "- [fact:{fact_type}] [{source}] (score: {score:.2}) {snippet}{truncated}\n",
                fact_type = hit.fact.fact_type,
                source = source_label(super::memory_retrieval::MemorySourceKind::Fact),
                score = hit.score,
            )
        }
        MemoryHit::Chunk(hit) => {
            format!(
                "- [{path}:{start}-{end}] [{source}] (score: {score:.2}) {snippet}\n",
                path = hit.path,
                start = hit.start_line,
                end = hit.end_line,
                source = source_label(classify_chunk_source(&hit.source, &hit.path)),
                score = hit.score,
                snippet = hit.snippet,
            )
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

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
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

pub struct MemoryWriteTool {
    fact_store: FactStore,
    file_store: MemoryFileStore,
    memory: Arc<MemoryStore>,
    agent_id: String,
}

impl MemoryWriteTool {
    pub fn new(
        fact_store: FactStore,
        file_store: MemoryFileStore,
        memory: Arc<MemoryStore>,
        agent_id: String,
    ) -> Self {
        Self {
            fact_store,
            file_store,
            memory,
            agent_id,
        }
    }

    async fn record_explicit_write_marker(&self, ctx: &ToolContext, fact: &Fact) -> Result<()> {
        if ctx.session_key().is_empty() {
            return Ok(());
        }

        let Some(session) = self.memory.get_session(ctx.session_key()).await? else {
            return Ok(());
        };

        let mut state = self
            .memory
            .get_session_memory_state(&self.agent_id, &session.session_id)
            .await?
            .unwrap_or(SessionMemoryStateRecord {
                agent_id: self.agent_id.clone(),
                session_id: session.session_id.clone(),
                session_key: session.session_key.clone(),
                last_flushed_turn: 0,
                last_boundary_flush_at: None,
                pending_flush: false,
                recent_explicit_writes: Vec::new(),
                open_episodes: Vec::new(),
            });

        let normalized_content = super::memory_retrieval::normalize_text(&fact.content);
        if normalized_content.is_empty() {
            return Ok(());
        }

        state.recent_explicit_writes.retain(|marker| {
            let normalized_marker = super::memory_retrieval::normalize_text(&marker.summary);
            !normalized_marker.is_empty()
        });
        if state.recent_explicit_writes.iter().any(|marker| {
            let normalized_marker = super::memory_retrieval::normalize_text(&marker.summary);
            !normalized_marker.is_empty()
                && super::memory_retrieval::are_near_duplicates(
                    &normalized_marker,
                    &normalized_content,
                )
        }) {
            return Ok(());
        }

        state
            .recent_explicit_writes
            .push(RecentExplicitMemoryWrite {
                turn_index: session.interaction_count.saturating_add(1),
                memory_ref: fact.id.clone(),
                canonical_id: None,
                summary: fact.content.clone(),
                recorded_at: chrono::Utc::now(),
            });
        if state.recent_explicit_writes.len() > 16 {
            let overflow = state.recent_explicit_writes.len() - 16;
            state.recent_explicit_writes.drain(0..overflow);
        }
        self.memory.upsert_session_memory_state(state).await
    }
}

#[async_trait]
impl ToolExecutor for MemoryWriteTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "memory_write".into(),
            description: "Store a fact about the user or conversation for future reference. Use this to remember important preferences, decisions, or events.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "content": {
                        "type": "string",
                        "description": "The fact to remember (e.g., 'User prefers dark mode')"
                    },
                    "fact_type": {
                        "type": "string",
                        "enum": ["preference", "decision", "event", "person", "rule"],
                        "description": "Type of fact"
                    }
                },
                "required": ["content", "fact_type"]
            }),
        }
    }

    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
        let content = input["content"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing 'content' field"))?;
        let fact_type = input["fact_type"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing 'fact_type' field"))?;

        let active_facts = self.fact_store.get_active_facts(&self.agent_id).await?;
        if let Some(existing) = find_matching_fact(&active_facts, content) {
            return Ok(ToolOutput {
                content: format!("Already remembered: {}", existing.content),
                is_error: false,
            });
        }

        let long_term = self.file_store.read_long_term().await?;
        if !long_term.trim().is_empty() {
            let doc = MemoryDocument::parse(&long_term);
            let existing_memory_item = crate::memory_document::MEMORY_SECTION_ORDER
                .iter()
                .flat_map(|heading| doc.section_items(heading))
                .find(|item| super::memory_retrieval::is_matching_memory_content(item, content));
            if let Some(existing) = existing_memory_item {
                return Ok(ToolOutput {
                    content: format!("Already remembered: {}", existing),
                    is_error: false,
                });
            }
        }

        let now = chrono::Utc::now().to_rfc3339();
        let fact = Fact {
            id: fact_store::generate_fact_id(&self.agent_id, content),
            agent_id: self.agent_id.clone(),
            content: content.to_owned(),
            fact_type: fact_type.to_owned(),
            importance: 0.5,
            confidence: 1.0,
            status: "active".to_owned(),
            occurred_at: None,
            recorded_at: now.clone(),
            source_type: "explicit_user_memory".to_owned(),
            source_session: self
                .memory
                .get_session(ctx.session_key())
                .await?
                .map(|session| session.session_id),
            access_count: 0,
            last_accessed: None,
            superseded_by: None,
            created_at: now.clone(),
            updated_at: now,
        };

        match self.fact_store.insert_fact(&fact).await {
            Ok(()) => {
                let _ = self.fact_store.record_add(&fact).await;
                if let Err(error) = self.record_explicit_write_marker(ctx, &fact).await {
                    tracing::warn!(
                        %error,
                        agent_id = %self.agent_id,
                        session_key = %ctx.session_key(),
                        "Failed to persist explicit memory write marker"
                    );
                }
                Ok(ToolOutput {
                    content: format!("Remembered: {content}"),
                    is_error: false,
                })
            }
            Err(e) => Ok(ToolOutput {
                content: format!("Failed to store fact: {e}"),
                is_error: true,
            }),
        }
    }
}

pub struct MemoryForgetTool {
    fact_store: FactStore,
    agent_id: String,
}

impl MemoryForgetTool {
    pub fn new(fact_store: FactStore, agent_id: String) -> Self {
        Self {
            fact_store,
            agent_id,
        }
    }
}

#[async_trait]
impl ToolExecutor for MemoryForgetTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "memory_forget".into(),
            description: "Forget or retract a previously stored fact. Use when the user says something is no longer true or asks you to forget something.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "content": {
                        "type": "string",
                        "description": "The fact content to forget (must match an existing fact)"
                    },
                    "reason": {
                        "type": "string",
                        "description": "Why this fact is being retracted"
                    }
                },
                "required": ["content", "reason"]
            }),
        }
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let content = input["content"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing 'content' field"))?;
        let reason = input["reason"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing 'reason' field"))?;

        match self
            .fact_store
            .find_by_content(&self.agent_id, content)
            .await
        {
            Ok(Some(fact)) if fact.status == "active" => {
                match self
                    .fact_store
                    .update_status(&fact.id, "retracted", reason)
                    .await
                {
                    Ok(()) => Ok(ToolOutput {
                        content: format!("Forgotten: {content}"),
                        is_error: false,
                    }),
                    Err(e) => Ok(ToolOutput {
                        content: format!("Failed to retract fact: {e}"),
                        is_error: true,
                    }),
                }
            }
            Ok(_) => Ok(ToolOutput {
                content: format!("No active fact found matching: {content}"),
                is_error: false,
            }),
            Err(e) => Ok(ToolOutput {
                content: format!("Failed to look up fact: {e}"),
                is_error: true,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clawhive_memory::embedding::StubEmbeddingProvider;
    use clawhive_memory::fact_store::FactStore;
    use clawhive_memory::memory_lineage::MemoryLineageStore;
    use clawhive_memory::search_index::{SearchIndex, SearchResult};
    use clawhive_memory::{file_store::MemoryFileStore, MemoryStore};
    use std::sync::Arc;
    use tempfile::TempDir;

    fn setup() -> (TempDir, Arc<MemoryStore>, MemorySearchTool, MemoryGetTool) {
        let tmp = TempDir::new().unwrap();
        let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
        let fact_store = FactStore::new(memory.db());
        let search_index = SearchIndex::new(memory.db(), "test-agent");
        let embedding: Arc<dyn EmbeddingProvider> = Arc::new(StubEmbeddingProvider::new(8));
        let file_store = MemoryFileStore::new(tmp.path());

        let search_tool = MemorySearchTool::new(
            fact_store,
            search_index,
            embedding,
            "test-agent".to_string(),
        );
        let get_tool = MemoryGetTool::new(file_store);
        (tmp, memory, search_tool, get_tool)
    }

    #[test]
    fn memory_search_tool_definition() {
        let (_tmp, _memory, tool, _) = setup();
        let def = tool.definition();
        assert_eq!(def.name, "memory_search");
        assert!(def.input_schema["properties"]["query"].is_object());
    }

    #[test]
    fn memory_get_tool_definition() {
        let (_tmp, _memory, _, tool) = setup();
        let def = tool.definition();
        assert_eq!(def.name, "memory_get");
        assert!(def.input_schema["properties"]["key"].is_object());
    }

    #[test]
    fn memory_write_tool_definition() {
        let (tmp, memory, _, _) = setup();
        let tool = MemoryWriteTool::new(
            FactStore::new(memory.db()),
            MemoryFileStore::new(tmp.path()),
            memory.clone(),
            "agent-1".to_string(),
        );

        let def = tool.definition();

        assert_eq!(def.name, "memory_write");
        assert!(def.input_schema["properties"]["content"].is_object());
        assert!(def.input_schema["properties"]["fact_type"].is_object());
    }

    #[test]
    fn memory_forget_tool_definition() {
        let (_tmp, memory, _, _) = setup();
        let tool = MemoryForgetTool::new(FactStore::new(memory.db()), "agent-1".to_string());

        let def = tool.definition();

        assert_eq!(def.name, "memory_forget");
        assert!(def.input_schema["properties"]["content"].is_object());
        assert!(def.input_schema["properties"]["reason"].is_object());
    }

    #[test]
    fn format_memory_hit_chunk_uses_snippet_field() {
        let chunk = SearchResult {
            chunk_id: "chunk-1".to_string(),
            path: "MEMORY.md".to_string(),
            source: "long_term".to_string(),
            start_line: 1,
            end_line: 2,
            snippet: "short snippet...".to_string(),
            text: "very long full chunk text that should not be printed directly".to_string(),
            score: 0.9,
        };

        let rendered = format_memory_hit(&MemoryHit::Chunk(chunk));
        assert!(rendered.contains("short snippet..."));
        assert!(!rendered.contains("very long full chunk text"));
    }

    #[tokio::test]
    async fn memory_search_returns_results() {
        let (_tmp, _memory, tool, _) = setup();
        let ctx = ToolContext::builtin();
        let result = tool
            .execute(serde_json::json!({"query": "test query"}), &ctx)
            .await
            .unwrap();
        // With empty index, should return empty but not error
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn memory_search_includes_matching_facts() {
        let (tmp, memory, tool, _) = setup();
        let ctx = ToolContext::builtin();
        let fact_store = FactStore::new(memory.db());
        let write_tool = MemoryWriteTool::new(
            fact_store,
            MemoryFileStore::new(tmp.path()),
            memory.clone(),
            "test-agent".to_string(),
        );

        write_tool
            .execute(
                serde_json::json!({
                    "content": "User prefers Chinese replies",
                    "fact_type": "preference"
                }),
                &ctx,
            )
            .await
            .unwrap();

        let result = tool
            .execute(serde_json::json!({"query": "Chinese replies"}), &ctx)
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("[fact:preference]"));
        assert!(result.content.contains("[fact]"));
        assert!(result.content.contains("Chinese replies"));
    }

    #[tokio::test]
    async fn memory_search_honors_time_range_for_daily_chunks() {
        let (_tmp, _memory, tool, _) = setup();
        let ctx = ToolContext::builtin();

        tool.search_index
            .index_file(
                "memory/2026-03-01.md",
                "# March\n\nrelease planning and sprint notes",
                "daily",
                tool.embedding_provider.as_ref(),
            )
            .await
            .unwrap();
        tool.search_index
            .index_file(
                "memory/2026-04-01.md",
                "# April\n\nrelease planning and sprint notes",
                "daily",
                tool.embedding_provider.as_ref(),
            )
            .await
            .unwrap();

        let result = tool
            .execute(
                serde_json::json!({
                    "query": "release planning",
                    "time_range": {
                        "from": "2026-03",
                        "to": "2026-03"
                    }
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("memory/2026-03-01.md"));
        assert!(!result.content.contains("memory/2026-04-01.md"));
    }

    #[tokio::test]
    async fn memory_get_long_term() {
        let (tmp, _memory, _, tool) = setup();
        let ctx = ToolContext::builtin();
        let file_store = MemoryFileStore::new(tmp.path());
        file_store
            .write_long_term("# Long term memory")
            .await
            .unwrap();

        let result = tool
            .execute(serde_json::json!({"key": "MEMORY.md"}), &ctx)
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("Long term memory"));
    }

    #[tokio::test]
    async fn memory_write_stores_active_fact() {
        let (tmp, memory, _, _) = setup();
        let ctx = ToolContext::builtin();
        let fact_store = FactStore::new(memory.db());
        let lineage_store = MemoryLineageStore::new(memory.db());
        let tool = MemoryWriteTool::new(
            fact_store.clone(),
            MemoryFileStore::new(tmp.path()),
            memory.clone(),
            "agent-1".to_string(),
        );

        let result = tool
            .execute(
                serde_json::json!({
                    "content": "User prefers dark mode",
                    "fact_type": "preference"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        assert_eq!(result.content, "Remembered: User prefers dark mode");

        let fact = fact_store
            .find_by_content("agent-1", "User prefers dark mode")
            .await
            .unwrap()
            .expect("fact should be stored");
        assert_eq!(fact.status, "active");
        assert_eq!(fact.fact_type, "preference");

        let links = lineage_store
            .get_links_for_source("agent-1", "fact", &fact.id)
            .await
            .unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(fact.source_type, "explicit_user_memory");
    }

    #[tokio::test]
    async fn memory_write_is_idempotent_for_exact_duplicate() {
        let (tmp, memory, _, _) = setup();
        let ctx = ToolContext::builtin();
        let fact_store = FactStore::new(memory.db());
        let tool = MemoryWriteTool::new(
            fact_store.clone(),
            MemoryFileStore::new(tmp.path()),
            memory.clone(),
            "agent-1".to_string(),
        );

        let first = tool
            .execute(
                serde_json::json!({
                    "content": "User prefers dark mode",
                    "fact_type": "preference"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(first.content, "Remembered: User prefers dark mode");

        let second = tool
            .execute(
                serde_json::json!({
                    "content": "User prefers dark mode",
                    "fact_type": "preference"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(second.content, "Already remembered: User prefers dark mode");

        let facts = fact_store.get_active_facts("agent-1").await.unwrap();
        assert_eq!(facts.len(), 1);
    }

    #[tokio::test]
    async fn memory_write_suppresses_near_duplicate_fact() {
        let (tmp, memory, _, _) = setup();
        let ctx = ToolContext::builtin();
        let fact_store = FactStore::new(memory.db());
        let tool = MemoryWriteTool::new(
            fact_store.clone(),
            MemoryFileStore::new(tmp.path()),
            memory.clone(),
            "agent-1".to_string(),
        );

        tool.execute(
            serde_json::json!({
                "content": "User prefers Chinese replies",
                "fact_type": "preference"
            }),
            &ctx,
        )
        .await
        .unwrap();

        let second = tool
            .execute(
                serde_json::json!({
                    "content": "User prefers Chinese replies for future answers",
                    "fact_type": "preference"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert_eq!(
            second.content,
            "Already remembered: User prefers Chinese replies"
        );

        let facts = fact_store.get_active_facts("agent-1").await.unwrap();
        assert_eq!(facts.len(), 1);
    }

    #[tokio::test]
    async fn memory_write_suppresses_content_already_in_long_term_memory() {
        let (tmp, memory, _, _) = setup();
        let ctx = ToolContext::builtin();
        let fact_store = FactStore::new(memory.db());
        let file_store = MemoryFileStore::new(tmp.path());
        file_store
            .write_long_term(
                "# MEMORY.md\n\n## 长期项目主线\n\n- Memory refactor is now section-based\n\n## 持续性背景脉络\n\n## 关键历史决策\n",
            )
            .await
            .unwrap();
        let tool = MemoryWriteTool::new(
            fact_store.clone(),
            file_store,
            memory.clone(),
            "agent-1".to_string(),
        );

        let result = tool
            .execute(
                serde_json::json!({
                    "content": "Memory refactor is now section-based",
                    "fact_type": "decision"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert_eq!(
            result.content,
            "Already remembered: Memory refactor is now section-based"
        );
        let facts = fact_store.get_active_facts("agent-1").await.unwrap();
        assert!(facts.is_empty());
    }

    #[tokio::test]
    async fn memory_write_records_recent_explicit_marker_for_current_session() {
        let (tmp, memory, _, _) = setup();
        let fact_store = FactStore::new(memory.db());
        let tool = MemoryWriteTool::new(
            fact_store.clone(),
            MemoryFileStore::new(tmp.path()),
            memory.clone(),
            "agent-1".to_string(),
        );
        memory
            .upsert_session(clawhive_memory::SessionRecord {
                session_key: "chat:1".to_string(),
                session_id: "session-123".to_string(),
                agent_id: "agent-1".to_string(),
                created_at: chrono::Utc::now(),
                last_active: chrono::Utc::now(),
                ttl_seconds: 1800,
                interaction_count: 4,
            })
            .await
            .unwrap();
        let ctx = ToolContext::builtin().with_session_key("chat:1");

        tool.execute(
            serde_json::json!({
                "content": "User prefers concise replies",
                "fact_type": "preference"
            }),
            &ctx,
        )
        .await
        .unwrap();

        let state = memory
            .get_session_memory_state("agent-1", "session-123")
            .await
            .unwrap()
            .expect("session memory state");
        assert_eq!(state.recent_explicit_writes.len(), 1);
        let marker = &state.recent_explicit_writes[0];
        assert_eq!(marker.turn_index, 5);
        assert_eq!(marker.summary, "User prefers concise replies");
    }

    #[tokio::test]
    async fn memory_forget_retracts_existing_fact() {
        let (tmp, memory, _, _) = setup();
        let ctx = ToolContext::builtin();
        let fact_store = FactStore::new(memory.db());
        let write_tool = MemoryWriteTool::new(
            fact_store.clone(),
            MemoryFileStore::new(tmp.path()),
            memory.clone(),
            "agent-1".to_string(),
        );
        let forget_tool = MemoryForgetTool::new(fact_store.clone(), "agent-1".to_string());

        write_tool
            .execute(
                serde_json::json!({
                    "content": "User moved to Berlin",
                    "fact_type": "event"
                }),
                &ctx,
            )
            .await
            .unwrap();

        let result = forget_tool
            .execute(
                serde_json::json!({
                    "content": "User moved to Berlin",
                    "reason": "User corrected this"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        assert_eq!(result.content, "Forgotten: User moved to Berlin");

        let fact = fact_store
            .find_by_content("agent-1", "User moved to Berlin")
            .await
            .unwrap()
            .expect("fact should still exist with updated status");
        assert_eq!(fact.status, "retracted");
    }
}
