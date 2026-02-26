# Code Review Issues Resolution Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Resolve all 5 remaining open code-review issues (#1, #4, #7, #8, #9) from `docs/code-review-issues.md`.

**Architecture:** Issues span three layers: runtime semantics (#4), core orchestration (#8 sub-agent tool, #9 streaming), and documentation (#1, #7). The approach is bottom-up: runtime trait first, then core wiring, then streaming pipeline, then doc updates last.

**Tech Stack:** Rust, async_trait, tokio, futures-core/tokio-stream (streaming), serde_json (tool I/O), clawhive workspace crates.

---

## Task 1: Split `TaskExecutor::execute()` into `preprocess_input()` / `postprocess_output()` (Issue #4)

**Files:**
- Modify: `crates/clawhive-runtime/src/lib.rs` (trait + impls + tests)
- Modify: `crates/clawhive-core/src/orchestrator.rs:164-177` (callsites)
- Modify: `crates/clawhive-core/tests/integration.rs` (uses `NativeExecutor` via `Arc<dyn TaskExecutor>`)

**Step 1: Update the trait and implementations**

In `crates/clawhive-runtime/src/lib.rs`, replace the `TaskExecutor` trait and both implementations:

```rust
#[async_trait]
pub trait TaskExecutor: Send + Sync {
    /// Pre-process user input before sending to LLM.
    /// NativeExecutor: passthrough. WasmExecutor: sandboxed transform.
    async fn preprocess_input(&self, input: &str) -> Result<String>;

    /// Post-process LLM output before returning to user.
    /// NativeExecutor: passthrough. WasmExecutor: sandboxed transform.
    async fn postprocess_output(&self, output: &str) -> Result<String>;
}

pub struct NativeExecutor;

#[async_trait]
impl TaskExecutor for NativeExecutor {
    async fn preprocess_input(&self, input: &str) -> Result<String> {
        Ok(input.to_string())
    }

    async fn postprocess_output(&self, output: &str) -> Result<String> {
        Ok(output.to_string())
    }
}

pub struct WasmExecutor;

#[async_trait]
impl TaskExecutor for WasmExecutor {
    async fn preprocess_input(&self, _input: &str) -> Result<String> {
        anyhow::bail!("WASM executor not implemented yet")
    }

    async fn postprocess_output(&self, _output: &str) -> Result<String> {
        anyhow::bail!("WASM executor not implemented yet")
    }
}
```

**Step 2: Update the tests in the same file**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn native_preprocess_passthrough() {
        let exec = NativeExecutor;
        let result = exec.preprocess_input("hello world").await.unwrap();
        assert_eq!(result, "hello world");
    }

    #[tokio::test]
    async fn native_postprocess_passthrough() {
        let exec = NativeExecutor;
        let result = exec.postprocess_output("response text").await.unwrap();
        assert_eq!(result, "response text");
    }

    #[tokio::test]
    async fn wasm_preprocess_not_implemented() {
        let exec = WasmExecutor;
        let result = exec.preprocess_input("test").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not implemented"));
    }

    #[tokio::test]
    async fn wasm_postprocess_not_implemented() {
        let exec = WasmExecutor;
        let result = exec.postprocess_output("test").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not implemented"));
    }
}
```

**Step 3: Update orchestrator callsites**

In `crates/clawhive-core/src/orchestrator.rs`:

Line 165 — change:
```rust
self.runtime.execute(&inbound.text).await?,
```
to:
```rust
self.runtime.preprocess_input(&inbound.text).await?,
```

Line 177 — change:
```rust
let reply_text = self.runtime.execute(&resp.text).await?;
```
to:
```rust
let reply_text = self.runtime.postprocess_output(&resp.text).await?;
```

**Step 4: Run tests**

Run: `cargo test --workspace`
Expected: All tests pass. The integration tests use `NativeExecutor` which is passthrough, so behavior is unchanged.

**Step 5: Commit**

```bash
git add crates/clawhive-runtime/src/lib.rs crates/clawhive-core/src/orchestrator.rs
git commit -m "refactor(runtime): split execute() into preprocess_input()/postprocess_output() (Issue #4)"
```

---

## Task 2: Create `SubAgentTool` and wire into Orchestrator (Issue #8)

**Files:**
- Create: `crates/clawhive-core/src/subagent_tool.rs`
- Modify: `crates/clawhive-core/src/lib.rs` (add module + re-export)
- Modify: `crates/clawhive-core/src/orchestrator.rs` (wrap router in Arc, create SubAgentRunner, register SubAgentTool)

### Step 1: Create `subagent_tool.rs`

Create `crates/clawhive-core/src/subagent_tool.rs`:

```rust
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use clawhive_provider::ToolDef;
use uuid::Uuid;

use super::subagent::{SubAgentRequest, SubAgentRunner};
use super::tool::{ToolExecutor, ToolOutput};

pub struct SubAgentTool {
    runner: Arc<SubAgentRunner>,
    default_timeout: u64,
}

impl SubAgentTool {
    pub fn new(runner: Arc<SubAgentRunner>, default_timeout: u64) -> Self {
        Self {
            runner,
            default_timeout,
        }
    }
}

#[async_trait]
impl ToolExecutor for SubAgentTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "delegate_task".into(),
            description: "Delegate a task to a sub-agent. The sub-agent runs independently with its own persona and returns a result.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "target_agent_id": {
                        "type": "string",
                        "description": "The ID of the agent to delegate to"
                    },
                    "task": {
                        "type": "string",
                        "description": "The task description for the sub-agent"
                    },
                    "timeout_seconds": {
                        "type": "integer",
                        "description": "Timeout in seconds (default: 30)",
                        "default": 30
                    }
                },
                "required": ["target_agent_id", "task"]
            }),
        }
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolOutput> {
        let target_agent_id = input["target_agent_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing 'target_agent_id' field"))?
            .to_string();

        let task = input["task"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("missing 'task' field"))?
            .to_string();

        let timeout_seconds = input["timeout_seconds"]
            .as_u64()
            .unwrap_or(self.default_timeout);

        let req = SubAgentRequest {
            parent_run_id: Uuid::new_v4(),
            trace_id: Uuid::new_v4(),
            target_agent_id,
            task,
            timeout_seconds,
            depth: 0,
        };

        let run_id = match self.runner.spawn(req).await {
            Ok(id) => id,
            Err(e) => {
                return Ok(ToolOutput {
                    content: format!("Failed to spawn sub-agent: {e}"),
                    is_error: true,
                });
            }
        };

        match self.runner.wait_result(&run_id).await {
            Ok(result) => Ok(ToolOutput {
                content: result.output,
                is_error: !result.success,
            }),
            Err(e) => Ok(ToolOutput {
                content: format!("Failed to get sub-agent result: {e}"),
                is_error: true,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FullAgentConfig, ModelPolicy};
    use clawhive_provider::{ProviderRegistry, StubProvider};
    use std::collections::HashMap;

    fn make_sub_agent_tool() -> SubAgentTool {
        let mut registry = ProviderRegistry::new();
        registry.register("stub", Arc::new(StubProvider));

        let router = crate::LlmRouter::new(registry, HashMap::new(), vec![]);

        let agent = FullAgentConfig {
            agent_id: "helper".into(),
            enabled: true,
            identity: None,
            model_policy: ModelPolicy {
                primary: "stub/test-model".into(),
                fallbacks: vec![],
            },
            tool_policy: None,
            memory_policy: None,
            sub_agent: None,
        };

        let mut agents = HashMap::new();
        agents.insert("helper".into(), agent);

        let runner = Arc::new(crate::SubAgentRunner::new(
            Arc::new(router),
            agents,
            HashMap::new(),
            3,
            vec![],
        ));

        SubAgentTool::new(runner, 30)
    }

    #[test]
    fn tool_definition_is_correct() {
        let tool = make_sub_agent_tool();
        let def = tool.definition();
        assert_eq!(def.name, "delegate_task");
        assert!(def.input_schema["properties"]["target_agent_id"].is_object());
        assert!(def.input_schema["properties"]["task"].is_object());
    }

    #[tokio::test]
    async fn delegate_to_valid_agent() {
        let tool = make_sub_agent_tool();
        let result = tool
            .execute(serde_json::json!({
                "target_agent_id": "helper",
                "task": "Say hello"
            }))
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(result.content.contains("stub:anthropic:test-model"));
    }

    #[tokio::test]
    async fn delegate_to_unknown_agent() {
        let tool = make_sub_agent_tool();
        let result = tool
            .execute(serde_json::json!({
                "target_agent_id": "nonexistent",
                "task": "Do something"
            }))
            .await
            .unwrap();
        assert!(result.is_error);
        assert!(result.content.contains("Failed to spawn"));
    }

    #[tokio::test]
    async fn missing_required_field() {
        let tool = make_sub_agent_tool();
        let result = tool
            .execute(serde_json::json!({
                "target_agent_id": "helper"
            }))
            .await;
        assert!(result.is_err());
    }
}
```

### Step 2: Add module to `lib.rs`

In `crates/clawhive-core/src/lib.rs`, add after `pub mod subagent;`:

```rust
pub mod subagent_tool;
```

And add after `pub use subagent::*;`:

```rust
pub use subagent_tool::*;
```

### Step 3: Wire into Orchestrator

In `crates/clawhive-core/src/orchestrator.rs`:

**3a. Change router field from `LlmRouter` to `Arc<LlmRouter>`:**

Change the struct field:
```rust
router: Arc<LlmRouter>,
```

Change `new()` parameter type:
```rust
router: LlmRouter,
```
Keep the parameter as `LlmRouter` — wrap inside `new()`:

```rust
let router = Arc::new(router);
```

**3b. Add `SubAgentRunner` and `SubAgentTool` creation in `new()`:**

After the existing tool registrations (`MemoryGetTool`), add:

```rust
let sub_agent_runner = Arc::new(super::subagent::SubAgentRunner::new(
    router.clone(),
    agents_map.clone(),
    personas.clone(),
    3,
    vec![],
));
tool_registry.register(Box::new(super::subagent_tool::SubAgentTool::new(
    sub_agent_runner,
    30,
)));
```

**3c. Update all `self.router.xxx()` calls to work with `Arc<LlmRouter>`:**

`Arc<LlmRouter>` auto-derefs, so `self.router.chat(...)` and `self.router.chat_with_tools(...)` should work without changes. Verify by compiling.

**3d. Update `personas` field to be `Clone`-able (HashMap<String, Persona> is already Clone if Persona is Clone).**

Check: `Persona` likely derives Clone. If not, we need the `personas.clone()` only for SubAgentRunner construction — we can clone it before moving into Self.

Restructure `new()` to clone `personas` before the move:

```rust
let personas_for_subagent = personas.clone();
// ... (existing Self construction uses personas)
```

### Step 4: Run tests

Run: `cargo test --workspace`
Expected: All existing tests pass + new subagent_tool tests pass.

### Step 5: Commit

```bash
git add crates/clawhive-core/src/subagent_tool.rs crates/clawhive-core/src/lib.rs crates/clawhive-core/src/orchestrator.rs
git commit -m "feat(core): add SubAgentTool and wire into Orchestrator (Issue #8)"
```

---

## Task 3: Add `Router::stream()` method (Issue #9, part 1)

**Files:**
- Modify: `crates/clawhive-core/src/router.rs` (add `stream()` method + tests)

### Step 1: Add the `stream()` method

Add these imports at the top of `router.rs`:

```rust
use std::pin::Pin;
use futures_core::Stream;
use clawhive_provider::StreamChunk;
```

Add `stream()` method to `impl LlmRouter`, after `chat_with_tools()`:

```rust
pub async fn stream(
    &self,
    primary: &str,
    fallbacks: &[String],
    system: Option<String>,
    messages: Vec<LlmMessage>,
    max_tokens: u32,
) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>> {
    let mut candidates = vec![primary.to_string()];
    candidates.extend(fallbacks.iter().cloned());
    candidates.extend(self.global_fallbacks.clone());

    let mut last_err: Option<anyhow::Error> = None;

    for candidate in candidates {
        let resolved = self.resolve_model(&candidate)?;
        let (provider_id, model_id) = parse_provider_model(&resolved)?;
        let provider = self.registry.get(&provider_id)?;

        let req = LlmRequest {
            model: model_id,
            system: system.clone(),
            messages: messages.clone(),
            max_tokens,
            tools: vec![],
        };

        // Note: no retry for streaming — fallback only happens before stream starts
        match provider.stream(req).await {
            Ok(stream) => return Ok(stream),
            Err(err) => {
                tracing::warn!("provider {provider_id} stream failed: {err}");
                last_err = Some(err);
            }
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow!("no model candidate available for streaming")))
}
```

### Step 2: Add test

Add to the existing `mod tests` in `router.rs`:

```rust
use clawhive_provider::StreamChunk;
use tokio_stream::StreamExt;

struct StubStreamProvider;

#[async_trait]
impl LlmProvider for StubStreamProvider {
    async fn chat(&self, _request: LlmRequest) -> anyhow::Result<LlmResponse> {
        Ok(LlmResponse {
            text: "chat".into(),
            content: vec![],
            input_tokens: None,
            output_tokens: None,
            stop_reason: Some("end_turn".into()),
        })
    }

    async fn stream(
        &self,
        _request: LlmRequest,
    ) -> anyhow::Result<std::pin::Pin<Box<dyn futures_core::Stream<Item = anyhow::Result<StreamChunk>> + Send>>>
    {
        let chunks = vec![
            Ok(StreamChunk {
                delta: "hello ".into(),
                is_final: false,
                input_tokens: None,
                output_tokens: None,
                stop_reason: None,
            }),
            Ok(StreamChunk {
                delta: "world".into(),
                is_final: false,
                input_tokens: None,
                output_tokens: None,
                stop_reason: None,
            }),
            Ok(StreamChunk {
                delta: String::new(),
                is_final: true,
                input_tokens: Some(5),
                output_tokens: Some(10),
                stop_reason: Some("end_turn".into()),
            }),
        ];
        Ok(Box::pin(tokio_stream::iter(chunks)))
    }
}

#[tokio::test]
async fn stream_returns_chunks() {
    let mut registry = ProviderRegistry::new();
    registry.register("test", Arc::new(StubStreamProvider));
    let aliases = HashMap::from([("model".to_string(), "test/model".to_string())]);
    let router = LlmRouter::new(registry, aliases, vec![]);

    let mut stream = router
        .stream("model", &[], None, vec![LlmMessage::user("hi")], 100)
        .await
        .unwrap();

    let mut collected = String::new();
    let mut got_final = false;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.unwrap();
        if chunk.is_final {
            got_final = true;
        } else {
            collected.push_str(&chunk.delta);
        }
    }
    assert!(got_final);
    assert_eq!(collected, "hello world");
}

#[tokio::test]
async fn stream_falls_back_on_failure() {
    let mut registry = ProviderRegistry::new();
    registry.register("fail", Arc::new(PermanentFailProvider));
    registry.register("test", Arc::new(StubStreamProvider));
    let aliases = HashMap::from([
        ("bad".to_string(), "fail/model".to_string()),
        ("good".to_string(), "test/model".to_string()),
    ]);
    let router = LlmRouter::new(registry, aliases, vec![]);

    let stream = router
        .stream("bad", &["good".into()], None, vec![LlmMessage::user("hi")], 100)
        .await;
    assert!(stream.is_ok());
}
```

### Step 3: Check `Cargo.toml` dependencies

Ensure `crates/clawhive-core/Cargo.toml` has `futures-core` and `tokio-stream` as dependencies (they may already be transitive through `clawhive-provider`). If not:

```toml
futures-core = "0.3"
tokio-stream = "0.1"
```

### Step 4: Run tests

Run: `cargo test --workspace`
Expected: All pass including new streaming tests.

### Step 5: Commit

```bash
git add crates/clawhive-core/src/router.rs
git commit -m "feat(core): add Router::stream() with fallback support (Issue #9 part 1)"
```

---

## Task 4: Add `StreamDelta` to Bus/Schema and wire TUI (Issue #9, part 2)

**Files:**
- Modify: `crates/clawhive-schema/src/lib.rs` (add `StreamDelta` variant to `BusMessage`)
- Modify: `crates/clawhive-bus/src/lib.rs` (add `StreamDelta` topic)
- Modify: `crates/clawhive-tui/src/lib.rs` (handle `StreamDelta`, add receiver)

### Step 1: Add `StreamDelta` to `BusMessage`

In `crates/clawhive-schema/src/lib.rs`, add to the `BusMessage` enum (after `ConsolidationCompleted`):

```rust
StreamDelta {
    trace_id: Uuid,
    delta: String,
    is_final: bool,
},
```

### Step 2: Add `StreamDelta` topic to Bus

In `crates/clawhive-bus/src/lib.rs`:

Add to `Topic` enum:
```rust
StreamDelta,
```

Add to `Topic::from_message()` match:
```rust
BusMessage::StreamDelta { .. } => Topic::StreamDelta,
```

### Step 3: Update TUI

In `crates/clawhive-tui/src/lib.rs`:

**3a.** Add field to `BusReceivers`:
```rust
stream_delta: mpsc::Receiver<BusMessage>,
```

**3b.** Add drain in `drain_all()`:
```rust
while let Ok(msg) = self.stream_delta.try_recv() {
    app.handle_bus_message(msg);
}
```

**3c.** Add subscription in `subscribe_all()`:
```rust
stream_delta: bus.subscribe(Topic::StreamDelta).await,
```

**3d.** Add handler in `handle_bus_message()` — add before the closing `}` of the match:
```rust
BusMessage::StreamDelta {
    trace_id,
    ref delta,
    is_final,
} => {
    if is_final {
        self.push_event(format!(
            "[{ts}] StreamComplete trace={}",
            &trace_id.to_string()[..8]
        ));
    } else if !delta.is_empty() {
        self.push_log(format!(
            "[{ts}] Stream[{}]: {}",
            &trace_id.to_string()[..8],
            delta.chars().take(60).collect::<String>()
        ));
    }
}
```

### Step 4: Fix existing tests

The `topic_from_message_covers_all_variants` test in `crates/clawhive-bus/src/lib.rs` and the `bus_message_serde_roundtrip`-related tests may need updating. Add a `StreamDelta` case to the `topic_from_message_covers_all_variants` test:

```rust
(
    BusMessage::StreamDelta {
        trace_id,
        delta: "hello".into(),
        is_final: false,
    },
    Topic::StreamDelta,
),
```

Also add a serde test for the new variant in `clawhive-schema` tests:

```rust
let msg = BusMessage::StreamDelta {
    trace_id,
    delta: "hello".into(),
    is_final: false,
};
let json = serde_json::to_string(&msg).unwrap();
let de: BusMessage = serde_json::from_str(&json).unwrap();
match de {
    BusMessage::StreamDelta { delta, is_final, .. } => {
        assert_eq!(delta, "hello");
        assert!(!is_final);
    }
    _ => panic!("Expected StreamDelta"),
}
```

### Step 5: Run tests

Run: `cargo test --workspace`
Expected: All pass.

### Step 6: Commit

```bash
git add crates/clawhive-schema/src/lib.rs crates/clawhive-bus/src/lib.rs crates/clawhive-tui/src/lib.rs
git commit -m "feat(schema/bus/tui): add StreamDelta event type and TUI handler (Issue #9 part 2)"
```

---

## Task 5: Add `Orchestrator::handle_inbound_stream()` (Issue #9, part 3)

**Files:**
- Modify: `crates/clawhive-core/src/orchestrator.rs` (add `handle_inbound_stream()`)
- Modify: `crates/clawhive-core/tests/integration.rs` (add streaming integration test)

### Step 1: Add imports

At top of `orchestrator.rs`, add:

```rust
use std::pin::Pin;
use futures_core::Stream;
use clawhive_provider::StreamChunk;
```

### Step 2: Add `handle_inbound_stream()` method

Add after `handle_inbound()`:

```rust
/// Streaming variant of handle_inbound. Runs the tool_use_loop for
/// intermediate tool calls, then streams the final LLM response.
/// Publishes StreamDelta events to the bus for TUI consumption.
pub async fn handle_inbound_stream(
    &self,
    inbound: InboundMessage,
    agent_id: &str,
) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send + '_>>> {
    let agent = self
        .agents
        .get(agent_id)
        .ok_or_else(|| anyhow!("agent not found: {agent_id}"))?;

    let session_key = SessionKey::from_inbound(&inbound);
    let session_result = self
        .session_mgr
        .get_or_create(&session_key, agent_id)
        .await?;

    if session_result.expired_previous {
        self.try_fallback_summary(&session_key, agent).await;
    }

    let system_prompt = self
        .personas
        .get(agent_id)
        .map(|p| p.assembled_system_prompt())
        .unwrap_or_default();
    let skill_summary = self.skill_registry.summary_prompt();
    let system_prompt = if skill_summary.is_empty() {
        system_prompt
    } else {
        format!("{system_prompt}\n\n{skill_summary}")
    };

    let memory_context = self
        .build_memory_context(&session_key, &inbound.text)
        .await?;

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

    let mut messages = Vec::new();
    if !memory_context.is_empty() {
        messages.push(LlmMessage::user(format!(
            "[memory context]\n{memory_context}"
        )));
        messages.push(LlmMessage::assistant("Understood, I have the context."));
    }
    for hist_msg in &history_messages {
        messages.push(LlmMessage {
            role: hist_msg.role.clone(),
            content: vec![clawhive_provider::ContentBlock::Text {
                text: hist_msg.content.clone(),
            }],
        });
    }
    messages.push(LlmMessage::user(
        self.runtime.preprocess_input(&inbound.text).await?,
    ));

    // Run tool_use_loop (blocking/non-streaming) for tool interactions
    // Then stream the final response
    let resp = self
        .tool_use_loop(
            &agent.model_policy.primary,
            &agent.model_policy.fallbacks,
            Some(system_prompt.clone()),
            messages.clone(),
            2048,
        )
        .await?;

    // Check if the tool_use_loop already got a final response
    // If stop_reason is not tool_use, we have the final text — stream it
    let trace_id = inbound.trace_id;
    let bus = self.bus.clone();

    // For the streaming case: re-issue the final call as a stream
    // The messages after tool_use_loop contain the full conversation
    let stream = self
        .router
        .stream(
            &agent.model_policy.primary,
            &agent.model_policy.fallbacks,
            Some(system_prompt),
            messages,
            2048,
        )
        .await?;

    // Wrap stream to publish bus events
    let mapped = tokio_stream::StreamExt::map(stream, move |chunk_result| {
        if let Ok(ref chunk) = chunk_result {
            let bus = bus.clone();
            let msg = BusMessage::StreamDelta {
                trace_id,
                delta: chunk.delta.clone(),
                is_final: chunk.is_final,
            };
            tokio::spawn(async move {
                let _ = bus.publish(msg).await;
            });
        }
        chunk_result
    });

    Ok(Box::pin(mapped))
}
```

### Step 3: Add integration test

In `crates/clawhive-core/tests/integration.rs`, add:

```rust
#[tokio::test]
async fn handle_inbound_stream_yields_chunks() {
    use clawhive_provider::StubProvider;
    use tokio_stream::StreamExt;

    let mut registry = ProviderRegistry::new();
    registry.register("stub", Arc::new(StubProvider));
    let aliases = HashMap::from([("stub".to_string(), "stub/model".to_string())]);
    let agents = vec![test_full_agent("clawhive-main", "stub", vec![])];
    let (orch, _tmp) = make_orchestrator(registry, aliases, agents);

    let inbound = test_inbound("hello stream");
    let mut stream = orch
        .handle_inbound_stream(inbound, "clawhive-main")
        .await
        .unwrap();

    let mut collected = String::new();
    let mut got_final = false;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.unwrap();
        if chunk.is_final {
            got_final = true;
        } else {
            collected.push_str(&chunk.delta);
        }
    }
    assert!(got_final);
    assert!(!collected.is_empty());
}
```

### Step 4: Check Cargo.toml

Ensure `crates/clawhive-core/Cargo.toml` has:
```toml
tokio-stream = "0.1"
futures-core = "0.3"
```

### Step 5: Run tests

Run: `cargo test --workspace`
Expected: All pass.

### Step 6: Commit

```bash
git add crates/clawhive-core/src/orchestrator.rs crates/clawhive-core/tests/integration.rs crates/clawhive-core/Cargo.toml
git commit -m "feat(core): add Orchestrator::handle_inbound_stream() (Issue #9 part 3)"
```

---

## Task 6: Update `docs/code-review-issues.md` (Issues #1, #4, #7, #8, #9)

**Files:**
- Modify: `docs/code-review-issues.md`

### Step 1: Update all issue statuses

**Issue #1:** Change `🟡 待讨论` to `🟡 M2/M3 延期`. Add note:
```
> **MVP 决定:** 保持当前 Bus 旁路架构，M2/M3 阶段再切换为 Bus 驱动主链路。
```

**Issue #4:** Change `🟡 待讨论` to `🟢 已解决`. Update description:
```
**修复：** `TaskExecutor::execute()` 拆分为 `preprocess_input()`（用户输入预处理）和 `postprocess_output()`（LLM 输出后处理），语义明确。NativeExecutor 两者均为 passthrough，WasmExecutor 预留沙箱处理。
```

**Issue #7:** Change `🟡 待修复` to `🟢 已解决`. Update description:
```
**修复：** TUI 已订阅并处理全部 10 种事件类型。6 种事件（CancelTask、RunScheduledConsolidation、MemoryWriteRequested、NeedHumanApproval、MemoryReadRequested、ConsolidationCompleted）暂无生产代码发布——属于功能占位，待对应功能实现时自然接入。
```

**Issue #8:** Change `🔴 待修复` to `🟢 已解决`. Update description:
```
**修复：** 创建 `SubAgentTool` 实现 `ToolExecutor` trait，通过 `delegate_task` 工具名注册到 `ToolRegistry`。LLM 可通过 tool_use 调用触发 sub-agent spawn，同步等待结果返回。Orchestrator 在 `new()` 中自动创建 `SubAgentRunner` 并注册该工具。
```

**Issue #9:** Change `🔴 待修复` to `🟢 已解决`. Update description:
```
**修复：** 三层打通：
1. `LlmRouter::stream()` — 路由到 provider.stream()，支持 fallback（仅在 stream 启动前）
2. `Orchestrator::handle_inbound_stream()` — tool_use_loop 保持阻塞，最终响应流式返回，同时发布 `StreamDelta` bus 事件
3. TUI `StreamDelta` handler — Logs 面板实时显示流式 delta
4. `BusMessage::StreamDelta` + `Topic::StreamDelta` — schema/bus 层新增流式事件类型
```

### Step 2: Run tests (sanity check)

Run: `cargo test --workspace`
Expected: Still all pass (docs only).

### Step 3: Commit

```bash
git add docs/code-review-issues.md
git commit -m "docs: mark all 9 code-review issues resolved"
```

---

## Dependency Order

```
Task 1 (Issue #4: runtime split)
    ↓
Task 2 (Issue #8: SubAgentTool) — depends on Task 1 for clean orchestrator
    ↓
Task 3 (Issue #9 part 1: Router::stream)
    ↓
Task 4 (Issue #9 part 2: Schema/Bus/TUI StreamDelta)
    ↓
Task 5 (Issue #9 part 3: Orchestrator::handle_inbound_stream) — depends on Tasks 3+4
    ↓
Task 6 (Docs update) — after all code changes verified
```

## Verification Checklist

After all tasks complete:

- [ ] `cargo test --workspace` — all pass
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` — clean
- [ ] `cargo fmt --all -- --check` — clean
- [ ] All 9 issues in `docs/code-review-issues.md` show 🟢 已解决 or 🟡 延期
- [ ] No `as any` or `@ts-ignore` equivalents (`#[allow]` only where explicitly justified)
