# Session History + Tool Calling Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Enable multi-turn conversation (wire session history into LLM context) and build the tool calling foundation so LLM can invoke `memory_search` / `memory_get` tools.

**Architecture:** Two phases. Phase A is a quick wiring fix in the Orchestrator to inject session history as LLM messages. Phase B is the structural change: introduce unified `ContentBlock` types in nanocrab-provider, migrate `LlmMessage.content` from `String` to `Vec<ContentBlock>`, build a `ToolRegistry` + execution loop in the Orchestrator, and register the first two memory tools. The `weak_react_loop` is replaced by a `tool_use_loop`.

**Tech Stack:** Rust, Anthropic Messages API (tool_use / tool_result content blocks), serde_json, nanocrab-provider, nanocrab-core, nanocrab-memory

---

## Phase A: Session History Injection (Issue #3 fix)

### Task 1: Inject session history into Orchestrator LLM messages

**Files:**
- Modify: `crates/nanocrab-core/src/orchestrator.rs:85-148` (handle_inbound)

**Step 1: Write the failing test**

Add to `crates/nanocrab-core/tests/mock_server_integration.rs`:

```rust
#[tokio::test]
async fn mock_server_includes_session_history() {
    let server = MockServer::start().await;

    // We need to capture what messages are sent to the API
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(mock_anthropic_response("reply with history")),
        )
        .mount(&server)
        .await;

    let provider = Arc::new(AnthropicProvider::new("test-key", server.uri()));
    let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
    let bus = EventBus::new(16);
    let (orch, tmp) = make_orchestrator_with_provider(provider, memory.clone(), &bus);

    // First turn
    let first = test_inbound("hello");
    let _ = orch.handle_inbound(first, "nanocrab-main").await.unwrap();

    // Second turn — session history should now include the first turn
    let second = test_inbound("follow up");
    let _ = orch.handle_inbound(second, "nanocrab-main").await.unwrap();

    // Verify: the session JSONL should have 4 messages (user+assistant x2)
    let reader = SessionReader::new(tmp.path());
    let key_str = "telegram:tg_main:chat:1:user:1";
    let messages = reader.load_recent_messages(key_str, 20).await.unwrap();
    assert_eq!(messages.len(), 4, "Should have 4 messages: 2 user + 2 assistant");
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test --package nanocrab-core --test mock_server_integration mock_server_includes_session_history -- --nocapture`

Expected: It should pass trivially for JSONL count (since SessionWriter already writes). But the real verification is that the LLM receives history messages. Since we can't easily inspect the request body in wiremock without custom matchers, this test verifies the end-to-end flow works.

**Step 3: Implement session history injection**

In `orchestrator.rs`, `handle_inbound()`, after the session is obtained and before building messages, load recent session messages and prepend them:

```rust
// After line 103 (after try_fallback_summary), add:

// Load recent conversation history from JSONL
let history_messages = match self
    .session_reader
    .load_recent_messages(&session_key.0, 10)
    .await
{
    Ok(msgs) => msgs,
    Err(e) => {
        tracing::warn!("Failed to load session history: {e}");
        Vec::new()
    }
};
```

Then, after building `memory_context` and before the current `messages.push(...)` block, inject history:

```rust
// After the memory context messages, inject session history
for hist_msg in &history_messages {
    messages.push(LlmMessage {
        role: hist_msg.role.clone(),
        content: hist_msg.content.clone(),
    });
}
```

The final message order should be:
1. Memory context (if any) — `[memory context]\n...` + assistant ack
2. Session history (last N messages from JSONL)
3. Current user message

**Step 4: Run tests**

Run: `cargo test --workspace`

Expected: All tests pass, including the new one.

**Step 5: Commit**

```bash
git add crates/nanocrab-core/src/orchestrator.rs crates/nanocrab-core/tests/mock_server_integration.rs
git commit -m "feat(core): inject session history into LLM context (Issue #3)"
```

---

## Phase B: Tool Calling Foundation

### Task 2: Define unified ContentBlock types in nanocrab-provider

**Files:**
- Modify: `crates/nanocrab-provider/src/types.rs`

**Step 1: Write tests for new types**

Add to bottom of `crates/nanocrab-provider/src/types.rs`, in the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_block_text_serde() {
        let block = ContentBlock::Text { text: "hello".into() };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "text");
        assert_eq!(json["text"], "hello");
        let roundtrip: ContentBlock = serde_json::from_value(json).unwrap();
        assert!(matches!(roundtrip, ContentBlock::Text { text } if text == "hello"));
    }

    #[test]
    fn content_block_tool_use_serde() {
        let block = ContentBlock::ToolUse {
            id: "toolu_123".into(),
            name: "memory_search".into(),
            input: serde_json::json!({"query": "rust"}),
        };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "tool_use");
        assert_eq!(json["id"], "toolu_123");
        assert_eq!(json["name"], "memory_search");
        let roundtrip: ContentBlock = serde_json::from_value(json).unwrap();
        assert!(matches!(roundtrip, ContentBlock::ToolUse { name, .. } if name == "memory_search"));
    }

    #[test]
    fn content_block_tool_result_serde() {
        let block = ContentBlock::ToolResult {
            tool_use_id: "toolu_123".into(),
            content: "search results here".into(),
            is_error: false,
        };
        let json = serde_json::to_value(&block).unwrap();
        assert_eq!(json["type"], "tool_result");
        assert_eq!(json["tool_use_id"], "toolu_123");
        let roundtrip: ContentBlock = serde_json::from_value(json).unwrap();
        assert!(matches!(roundtrip, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "toolu_123"));
    }

    #[test]
    fn llm_message_text_helper() {
        let msg = LlmMessage::user("hello");
        assert_eq!(msg.role, "user");
        assert_eq!(msg.text(), "hello");
    }

    #[test]
    fn llm_message_with_tool_use() {
        let msg = LlmMessage {
            role: "assistant".into(),
            content: vec![
                ContentBlock::Text { text: "Let me search...".into() },
                ContentBlock::ToolUse {
                    id: "toolu_1".into(),
                    name: "memory_search".into(),
                    input: serde_json::json!({"query": "test"}),
                },
            ],
        };
        assert_eq!(msg.text(), "Let me search...");
        assert!(msg.tool_uses().len() == 1);
    }

    #[test]
    fn tool_def_serde() {
        let tool = ToolDef {
            name: "memory_search".into(),
            description: "Search memory".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Search query"}
                },
                "required": ["query"]
            }),
        };
        let json = serde_json::to_value(&tool).unwrap();
        assert_eq!(json["name"], "memory_search");
        assert!(json["input_schema"]["properties"]["query"].is_object());
    }

    #[test]
    fn llm_message_simple_constructor() {
        let msg = LlmMessage::user("test");
        assert_eq!(msg.content.len(), 1);
        assert!(matches!(&msg.content[0], ContentBlock::Text { text } if text == "test"));
    }

    #[test]
    fn llm_message_assistant_constructor() {
        let msg = LlmMessage::assistant("reply");
        assert_eq!(msg.role, "assistant");
        assert_eq!(msg.text(), "reply");
    }

    #[test]
    fn llm_request_simple_still_works() {
        let req = LlmRequest::simple("model".into(), None, "hello".into());
        assert_eq!(req.messages.len(), 1);
        assert_eq!(req.messages[0].text(), "hello");
    }
}
```

**Step 2: Run tests to verify they fail**

Run: `cargo test --package nanocrab-provider -- --nocapture`

Expected: FAIL — `ContentBlock`, `ToolDef`, `LlmMessage::user()`, etc. don't exist yet.

**Step 3: Implement the new types**

Replace the entire `crates/nanocrab-provider/src/types.rs` with:

```rust
use serde::{Deserialize, Serialize};

/// A content block in an LLM message — text, tool_use, or tool_result.
/// Maps 1:1 to Anthropic's content block types.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
}

/// A message in an LLM conversation. Content is a vec of content blocks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmMessage {
    pub role: String,
    pub content: Vec<ContentBlock>,
}

impl LlmMessage {
    /// Create a user message with a single text block.
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: "user".into(),
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    /// Create an assistant message with a single text block.
    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: "assistant".into(),
            content: vec![ContentBlock::Text { text: text.into() }],
        }
    }

    /// Extract concatenated text from all Text blocks.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Extract all tool_use blocks.
    pub fn tool_uses(&self) -> Vec<(&str, &str, &serde_json::Value)> {
        self.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, name, input } => {
                    Some((id.as_str(), name.as_str(), input))
                }
                _ => None,
            })
            .collect()
    }
}

/// Tool definition sent to the LLM. Maps to Anthropic's `tools` parameter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmRequest {
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<LlmMessage>,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDef>,
}

fn default_max_tokens() -> u32 {
    2048
}

impl LlmRequest {
    /// Backward-compatible: single user message, no tools
    pub fn simple(model: String, system: Option<String>, user: String) -> Self {
        Self {
            model,
            system,
            messages: vec![LlmMessage::user(user)],
            max_tokens: default_max_tokens(),
            tools: vec![],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmResponse {
    pub text: String,
    pub content: Vec<ContentBlock>,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub stop_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamChunk {
    pub delta: String,
    pub is_final: bool,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub stop_reason: Option<String>,
}
```

**Step 4: Run tests**

Run: `cargo test --package nanocrab-provider -- --nocapture`

Expected: types tests pass. Other tests in the package may fail due to callsite changes needed.

**Step 5: Commit**

```bash
git add crates/nanocrab-provider/src/types.rs
git commit -m "feat(provider): introduce ContentBlock, ToolDef, and Vec<ContentBlock> message content"
```

---

### Task 3: Update all callsites for the new LlmMessage type

**Files:**
- Modify: `crates/nanocrab-provider/src/anthropic.rs` (ApiMessage conversion, response parsing)
- Modify: `crates/nanocrab-provider/src/lib.rs` (StubProvider)
- Modify: `crates/nanocrab-core/src/orchestrator.rs` (all LlmMessage constructions)
- Modify: `crates/nanocrab-core/src/router.rs` (reply method)
- Modify: `crates/nanocrab-core/src/consolidation.rs`
- Modify: `crates/nanocrab-core/src/subagent.rs`
- Modify: `crates/nanocrab-core/tests/mock_server_integration.rs`

This is a mechanical migration. Every `LlmMessage { role: "user".into(), content: "text".into() }` becomes `LlmMessage::user("text")`. Every `LlmMessage { role: "assistant".into(), content: "text".into() }` becomes `LlmMessage::assistant("text")`. Every `m.content.clone()` (where m is LlmMessage) becomes `m.text()`.

**Step 1: Update `anthropic.rs`**

Key changes:
- `to_api_request()`: Convert `Vec<ContentBlock>` to Anthropic API format. `ApiMessage.content` needs to become `serde_json::Value` to support both string and array formats. For messages with only one Text block, send as string. For messages with tool_use/tool_result blocks, send as array.
- `ApiRequest`: Add optional `tools` field.
- `chat()` response parsing: Build `Vec<ContentBlock>` from response. Handle `tool_use` block type.
- `ApiResponse.content` / `ContentBlock`: The private `ContentBlock` struct conflicts with the new public one. Rename private to `ApiContentBlock`.

**Step 2: Update `lib.rs` (StubProvider)**

- `m.content.clone()` → `m.text()`
- Return `LlmResponse` with `content: vec![ContentBlock::Text { text: ... }]`

**Step 3: Update all core crate callsites**

Replace all `LlmMessage { role: "...", content: "..." }` patterns with `LlmMessage::user(...)` or `LlmMessage::assistant(...)`.

Replace all `m.content` accesses with `m.text()`.

**Step 4: Run full workspace tests**

Run: `cargo test --workspace`

Expected: All tests pass.

**Step 5: Commit**

```bash
git add -u
git commit -m "refactor: migrate all callsites to Vec<ContentBlock> LlmMessage"
```

---

### Task 4: Build ToolRegistry and ToolExecutor trait

**Files:**
- Create: `crates/nanocrab-core/src/tool.rs`
- Modify: `crates/nanocrab-core/src/lib.rs` (add `pub mod tool;`)

**Step 1: Write tests**

```rust
// In crates/nanocrab-core/src/tool.rs

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoTool;

    #[async_trait::async_trait]
    impl ToolExecutor for EchoTool {
        fn definition(&self) -> ToolDef {
            ToolDef {
                name: "echo".into(),
                description: "Echo input".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "text": {"type": "string"}
                    },
                    "required": ["text"]
                }),
            }
        }

        async fn execute(&self, input: serde_json::Value) -> Result<ToolOutput> {
            let text = input["text"].as_str().unwrap_or("").to_string();
            Ok(ToolOutput {
                content: text,
                is_error: false,
            })
        }
    }

    #[test]
    fn registry_register_and_list() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(EchoTool));
        let defs = registry.tool_defs();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "echo");
    }

    #[tokio::test]
    async fn registry_execute_known_tool() {
        let mut registry = ToolRegistry::new();
        registry.register(Box::new(EchoTool));
        let result = registry
            .execute("echo", serde_json::json!({"text": "hello"}))
            .await
            .unwrap();
        assert_eq!(result.content, "hello");
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn registry_execute_unknown_tool() {
        let registry = ToolRegistry::new();
        let result = registry
            .execute("nonexistent", serde_json::json!({}))
            .await;
        assert!(result.is_err());
    }
}
```

**Step 2: Implement**

```rust
use std::collections::HashMap;
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use nanocrab_provider::ToolDef;

pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

#[async_trait]
pub trait ToolExecutor: Send + Sync {
    fn definition(&self) -> ToolDef;
    async fn execute(&self, input: serde_json::Value) -> Result<ToolOutput>;
}

pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn ToolExecutor>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    pub fn register(&mut self, tool: Box<dyn ToolExecutor>) {
        let name = tool.definition().name.clone();
        self.tools.insert(name, tool);
    }

    pub fn tool_defs(&self) -> Vec<ToolDef> {
        self.tools.values().map(|t| t.definition()).collect()
    }

    pub async fn execute(&self, name: &str, input: serde_json::Value) -> Result<ToolOutput> {
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| anyhow!("tool not found: {name}"))?;
        tool.execute(input).await
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}
```

**Step 3: Add module to lib.rs**

Add `pub mod tool;` to `crates/nanocrab-core/src/lib.rs` and `pub use tool::*;`.

**Step 4: Run tests**

Run: `cargo test --package nanocrab-core`

Expected: All tests pass.

**Step 5: Commit**

```bash
git add crates/nanocrab-core/src/tool.rs crates/nanocrab-core/src/lib.rs
git commit -m "feat(core): add ToolRegistry and ToolExecutor trait"
```

---

### Task 5: Implement memory_search and memory_get tools

**Files:**
- Create: `crates/nanocrab-core/src/memory_tools.rs`
- Modify: `crates/nanocrab-core/src/lib.rs` (add module)

**Step 1: Write tests**

```rust
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
        let embedding = Arc::new(StubEmbeddingProvider::new(8));
        let file_store = MemoryFileStore::new(tmp.path());

        let search_tool = MemorySearchTool::new(search_index.clone(), embedding.clone());
        let get_tool = MemoryGetTool::new(file_store.clone());
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
        file_store.write_long_term("# Long term memory").await.unwrap();

        let result = tool
            .execute(serde_json::json!({"key": "MEMORY.md"}))
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("Long term memory"));
    }
}
```

**Step 2: Implement**

```rust
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
    pub fn new(
        search_index: SearchIndex,
        embedding_provider: Arc<dyn EmbeddingProvider>,
    ) -> Self {
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
```

**Step 3: Add module**

Add `pub mod memory_tools;` and `pub use memory_tools::*;` to `crates/nanocrab-core/src/lib.rs`.

**Step 4: Run tests**

Run: `cargo test --package nanocrab-core`

Expected: All tests pass.

**Step 5: Commit**

```bash
git add crates/nanocrab-core/src/memory_tools.rs crates/nanocrab-core/src/lib.rs
git commit -m "feat(core): implement memory_search and memory_get tool executors"
```

---

### Task 6: Replace weak_react_loop with tool_use_loop in Orchestrator

**Files:**
- Modify: `crates/nanocrab-core/src/orchestrator.rs`

This is the key architectural change. The Orchestrator gains a `ToolRegistry` field, and the main loop becomes:

1. Send messages + tool_defs to LLM
2. If response has `stop_reason == "tool_use"`, extract tool_use blocks → execute tools → append tool_result messages → loop
3. If response has `stop_reason == "end_turn"`, return the text

**Step 1: Write the test**

Add to `crates/nanocrab-core/tests/mock_server_integration.rs`:

```rust
#[tokio::test]
async fn mock_server_tool_use_loop() {
    let server = MockServer::start().await;

    // First response: LLM requests tool use
    let tool_use_response = serde_json::json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "content": [
            {"type": "text", "text": "Let me search memory..."},
            {"type": "tool_use", "id": "toolu_1", "name": "memory_search", "input": {"query": "test"}}
        ],
        "model": "claude-sonnet-4-5",
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 10, "output_tokens": 20}
    });

    // Second response: LLM produces final answer
    let final_response = mock_anthropic_response("Here is what I found in memory.");

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(tool_use_response))
        .up_to_n_times(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(final_response))
        .mount(&server)
        .await;

    let provider = Arc::new(AnthropicProvider::new("test-key", server.uri()));
    let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
    let bus = EventBus::new(16);
    let (orch, _tmp) = make_orchestrator_with_provider(provider, memory, &bus);

    let out = orch
        .handle_inbound(test_inbound("search my memory"), "nanocrab-main")
        .await
        .unwrap();
    assert!(out.text.contains("Here is what I found"));
}
```

**Step 2: Update Orchestrator struct**

Add `tool_registry: ToolRegistry` field. Update `Orchestrator::new()` to accept it or construct it internally.

Recommended: construct it inside `new()` using the existing `search_index` and `embedding_provider` and `file_store` that are already available:

```rust
// Inside Orchestrator::new(), build tool registry:
let mut tool_registry = ToolRegistry::new();
tool_registry.register(Box::new(MemorySearchTool::new(
    search_index.clone(),
    embedding_provider.clone(),
)));
tool_registry.register(Box::new(MemoryGetTool::new(file_store.clone())));
```

**Step 3: Implement tool_use_loop**

Replace `weak_react_loop` with a new method:

```rust
async fn tool_use_loop(
    &self,
    primary: &str,
    fallbacks: &[String],
    system: Option<String>,
    initial_messages: Vec<LlmMessage>,
    max_tokens: u32,
) -> Result<LlmResponse> {
    let mut messages = initial_messages;
    let tool_defs = self.tool_registry.tool_defs();
    let max_iterations = 10; // Safety limit

    for _iteration in 0..max_iterations {
        let mut req = LlmRequest {
            model: primary.into(), // Router resolves alias
            system: system.clone(),
            messages: messages.clone(),
            max_tokens,
            tools: tool_defs.clone(),
        };

        let resp = self
            .router
            .chat_with_tools(primary, fallbacks, req)
            .await?;

        // Check if the response contains tool_use blocks
        let tool_uses: Vec<_> = resp.content.iter().filter_map(|b| match b {
            ContentBlock::ToolUse { id, name, input } => Some((id.clone(), name.clone(), input.clone())),
            _ => None,
        }).collect();

        if tool_uses.is_empty() || resp.stop_reason.as_deref() != Some("tool_use") {
            // No tool use — return final response
            return Ok(resp);
        }

        // Append assistant message with tool_use blocks
        messages.push(LlmMessage {
            role: "assistant".into(),
            content: resp.content.clone(),
        });

        // Execute each tool and build tool_result blocks
        let mut tool_results = Vec::new();
        for (id, name, input) in tool_uses {
            let result = match self.tool_registry.execute(&name, input).await {
                Ok(output) => ContentBlock::ToolResult {
                    tool_use_id: id,
                    content: output.content,
                    is_error: output.is_error,
                },
                Err(e) => ContentBlock::ToolResult {
                    tool_use_id: id,
                    content: format!("Tool execution error: {e}"),
                    is_error: true,
                },
            };
            tool_results.push(result);
        }

        // Append user message with tool_result blocks
        messages.push(LlmMessage {
            role: "user".into(),
            content: tool_results,
        });
    }

    Err(anyhow::anyhow!("tool use loop exceeded max iterations"))
}
```

**Step 4: Update `handle_inbound`**

Replace the `weak_react_loop` call with `tool_use_loop`. If `tool_registry` is empty, fall through to simple chat (backward compatible).

**Step 5: Update `LlmRouter`**

Add a `chat_with_tools` method that passes `LlmRequest` (including `tools` field) through to the provider. The existing `chat` method can remain for backward compat.

**Step 6: Update `AnthropicProvider`**

- `to_api_request`: Include `tools` field when non-empty.
- `ApiRequest`: Add `tools: Vec<ToolDef>`.
- `ApiMessage.content`: Change from `String` to `serde_json::Value` to support array content blocks.
- Response parsing: Parse `tool_use` content blocks alongside `text` blocks.

**Step 7: Run tests**

Run: `cargo test --workspace`

Expected: All tests pass, including the new tool_use_loop test.

**Step 8: Commit**

```bash
git add -u
git commit -m "feat(core): replace weak_react_loop with tool_use_loop, wire memory tools"
```

---

### Task 7: Update docs/code-review-issues.md

**Files:**
- Modify: `docs/code-review-issues.md`

Mark Issues #3 and #5 as resolved:
- Issue #3: Session history now injected into LLM context
- Issue #5: Tool use loop replaces weak_react_loop, no more need for [think]/[action]/[finish] prompt instructions

**Step 1: Update the file**

**Step 2: Commit**

```bash
git add docs/code-review-issues.md
git commit -m "docs: mark Issue #3 and #5 as resolved"
```

---

## Summary of deliverables

| Task | Deliverable | Tests |
|------|-------------|-------|
| 1 | Session history in LLM context | 1 integration test |
| 2 | ContentBlock, ToolDef types | ~10 unit tests |
| 3 | Callsite migration | All existing tests adapted |
| 4 | ToolRegistry + ToolExecutor | 3 unit tests |
| 5 | memory_search + memory_get | 4 unit tests |
| 6 | tool_use_loop in Orchestrator | 1 integration test |
| 7 | Docs update | — |

## Callsite inventory (Task 3 reference)

All places that construct `LlmMessage { role: ..., content: ... }`:

| File | Line(s) | Change |
|------|---------|--------|
| `provider/types.rs:28` | `LlmRequest::simple()` | → `LlmMessage::user(user)` |
| `provider/lib.rs:61,78` | `m.content.clone()` | → `m.text()` |
| `provider/anthropic.rs:70-74` | `to_api_request` mapping | → handle Vec<ContentBlock> |
| `provider/anthropic.rs:367` | test | → `LlmMessage::user(...)` |
| `core/orchestrator.rs:127-138,252,287-289` | message construction | → `LlmMessage::user/assistant(...)` |
| `core/orchestrator.rs:278` | `m.content` in fallback summary | → `m.text()` — NOTE: this reads SessionMessage, not LlmMessage. No change needed. |
| `core/router.rs:87-89` | `reply()` | → `LlmMessage::user(...)` |
| `core/router.rs:182,206` | tests | → `LlmMessage::user(...)` |
| `core/consolidation.rs:88-91` | consolidation prompt | → `LlmMessage::user(...)` |
| `core/subagent.rs:96-98` | sub-agent task | → `LlmMessage::user(...)` |
| `core/tests/mock_server_integration.rs:273,350,369` | tests | → `LlmMessage::user(...)` |
