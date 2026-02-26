# clawhive MVP Technical Specification (v0.1)

> Goal: Deliver a working first version as quickly as possible without sacrificing extensibility.
> Core Principles: **Pi's minimalist kernel + Nanobot's clear layering + clawhive's WASM and dual-layer memory design**.

---

## 1. MVP Scope and Boundaries

### 0.1 Key Conclusions Confirmed This Round (Frozen)

1. **Multi-Agent: Must have for MVP**
2. **Sub-Agent: Must have for MVP (minimal viable)**
3. **Persona belongs to Core/Agent management, not Gateway**
4. **MVP does not introduce workspace concept** (can be optional capability later)
5. **Configuration uses YAML** (no database as config source)
6. **Support multi-Bot / multi-account / multi-conversation-space** (`channel_type + connector_id + conversation_scope`)


### 1.1 Must Have This Phase

1. Single channel integration (Telegram)
2. Gateway boundary layer (ingress/auth/rate-limiting/protocol conversion)
3. Core/Orchestrator (session routing, policy decisions, execution orchestration)
4. Lightweight message bus (in-process Event Bus)
5. Runtime execution layer (reserve WASM execution interface, basic executor first)
6. Memory system MVP (Markdown + SQLite search index)
7. Basic scheduled consolidation (Cron daily off-peak execution)
8. CLI support (command-line management and one-off operations)
9. TUI support (developer real-time observation and debugging)

### 1.2 Not in MVP

1. Multi-node/distributed bus
2. Complex knowledge graph engine
3. External vector database (use SQLite + sqlite-vec instead)
4. Multi-channel simultaneous production
5. Agent autonomous task discovery

---

## 2. Overall Architecture (Responsibility Division)

### 2.0 Multi-Bot / Multi-Account / Multi-Conversation-Space (MVP Must Support)

Support "multiple Bot instances of the same channel type" from v0.1 to avoid later refactoring of session, memory, and routing rules.

Unified identifier triple:

- `channel_type`: telegram / discord / ...
- `connector_id`: specific Bot instance under the same channel type (e.g., `tg_main` / `tg_ops` / `dc_dev`)
- `conversation_scope`: specific conversation space
  - Telegram: `chat_id (+ topic_id)`
  - Discord: `guild_id + channel_id (+ thread_id)`

MVP Hard Constraints:

1. Inbound/outbound schema must include `connector_id`
2. Gateway performs auth and routing by `connector_id`
3. Session key must include `connector_id + conversation_scope`
4. Memory partition key must include `connector_id`


### 2.1 Module Roles

- **Gateway (Boundary Layer)**
  - Only responsible for: ingress, signature verification, rate limiting, protocol conversion, delivery
  - Not responsible for: session semantics, memory decisions, tool orchestration, persona assembly

- **Core / Orchestrator (Central Hub)**
  - Responsible for: Session routing, context assembly, policy selection, execution orchestration, memory control, persona assembly
  - The only "cognitive decision entry point" in the entire system

- **Bus (Event Layer - Sidecar Broadcast)**
  - **Position: Sidecar event broadcast, not main path driver**
  - Main path (message processing → LLM call → reply) maintains synchronous direct calls, Bus not involved
  - Bus responsible for non-blocking side effect broadcasts: audit logs, TUI real-time panels, metrics, alert notifications
  - Pattern: fire-and-forget, each listener consumes independently, failures don't affect main path
  - MVP uses in-memory async queue (`tokio::sync::mpsc`)
  - Interface design reserves backpressure semantics for future switch to NATS/Redis Stream
  - **vNext Extension**: Multi-channel outbound distribution can go through Bus (Gateway publish ReplyReady → various ChannelDrivers subscribe)

- **Runtime (Execution Layer)**
  - Receives tasks from Core and executes them
  - MVP reserves WASM interface for future smooth transition to WASM runtime

- **Memory (Memory Layer)**
  - Responsible for storage and retrieval
  - Policy in Core, storage in Memory module

### 2.2 Why Gateway Doesn't Control Memory

Memory belongs to "cognitive layer" capabilities (when to read, what to write, how to handle conflicts), must be decided by Core.
Gateway only handles I/O boundary processing, avoiding protocol details polluting business decisions.

---

## 3. Command / Event Schema (MVP Minimum Set)

> Freeze semantic protocol first, then choose transport implementation.
> **Note**: Commands/Events below don't necessarily go through Bus. Main path uses direct function calls, Bus only broadcasts sidecar events (see §12).
> Schema's value is in unifying DTO structure, not determining transport method.

All Inbound/Outbound DTOs must include at minimum:

- `channel_type`
- `connector_id`
- `conversation_scope`
- `user_scope`
- `trace_id`

### 3.1 Gateway -> Core Commands

1. `HandleIncomingMessage`
2. `CancelTask`
3. `RunScheduledConsolidation`

### 3.2 Core -> Gateway Events

1. `MessageAccepted`
2. `ReplyReady`
3. `TaskFailed`
4. `NeedHumanApproval` (reserved)

### 3.3 Core Internal Events (Optional)

1. `MemoryReadRequested`
2. `MemoryWriteRequested`
3. `ConsolidationCompleted`

---

## 4. Session Routing Rules (MVP)

`session_key` generation priority:

1. Explicit thread/session id (if provided by channel)
2. Private chat: `channel_type + connector_id + chat_id + user_id`
3. Group chat: `channel_type + connector_id + group_id + user_id (+ thread_id/topic_id)`

Recommended additions:

- Session TTL (e.g., 30 minute cooldown)
- Manual reset
- trace_id full-chain propagation

---

## 5. Memory System MVP

> Adopts OpenClaw memory model: Markdown files are the memory, SQLite is only a search index.
> Detailed design in `docs/clawhive-memory-design.md`.

### 5.1 Core Concepts

- **MEMORY.md**: Long-term memory (curated wisdom), both LLM and humans can directly edit
- **memory/YYYY-MM-DD.md**: Daily records (raw log), written anytime
- **SQLite Search Layer**: sqlite-vec + FTS5, pure index, doesn't hold authoritative data

```
Markdown files (source of truth)
  │  index
  ▼
SQLite (chunks table + sqlite-vec + FTS5)
  │  search
  ▼
Prompt injection (top-K chunks)
```

### 5.2 Write Strategy

- LLM actively writes to Markdown files during conversation (via tool or system prompt guidance)
- Humans can directly edit Markdown
- Fallback: if LLM didn't write anything when session ends → auto-generate summary to daily file
- **Don't automatically record every message** (avoid noise)

### 5.3 Search Strategy (Hybrid Search)

Parallel two-path search, merge and rerank:

- **sqlite-vec vector search**: semantic similarity recall (weight 0.7)
- **FTS5 full-text search**: BM25 keyword matching (weight 0.3)
- Default parameters: maxResults=6, minScore=0.35, candidateMultiplier=4
- Chunk size ~400 tokens, overlap ~80 tokens

> Zero external dependencies: SQLite built-in FTS5 + sqlite-vec extension.

### 5.4 Session History

- Each message persisted to `sessions/<session_id>.jsonl`
- Load recent N history when conversation starts
- Separated from memory files—this is raw conversation record, not memory

### 5.5 Not in MVP (vNext)

- Auto-Compaction + Memory Flush (context compression)
- Hippocampus scheduled task (extract from daily files to MEMORY.md)
- Semantic-aware chunking (heading-based splitting)
- Local Embedding model + auto fallback (MVP uses OpenAI API, reserves `EmbeddingProvider` trait)

---

## 6. Multi-Agent / Sub-Agent Design (MVP)

### 6.1 Multi-Agent (Required)

- `agent_id` is a first-class field, must be included in:
  - Command/Event schema
  - session_key generation
  - memory partition key
- Core provides `AgentRegistry`:
  - Parse available agent list
  - Select target agent based on routing rules
  - Load agent's model/tools/memory/persona policies

### 6.2 Sub-Agent (Required, Minimal Viable)

MVP supports:

- `spawn(task, agent_id?)`
- `cancel(run_id)`
- `timeout(run_id, ttl)`
- `result_merge(parent_session)`

Constraints:

- By default, sub-agents cannot recursively spawn again (prevent runaway)
- Sub-agent toolset defaults to more restricted than main agent (least privilege)
- Sub-agent must carry `parent_run_id` and `trace_id` for audit

### 6.3 Persona (Reference OpenClaw but workspace-less version)

OpenClaw's practice is "structured identity + text-based behavior rules".
clawhive MVP retains this concept but doesn't depend on workspace filesystem.

- **IdentityProfile (structured)**: `name/emoji/avatar/public_label`
- **BehaviorProfile (text-based)**: `system/style/safety` prompt templates

Persona loading and assembly done in Core, Gateway doesn't participate.

## 7. Configuration Management (YAML, not Database)

### 7.1 Decision Conclusion

- MVP config format: **YAML**
- Don't use database as config source
- Parse at runtime and map to strongly-typed structs

### 7.2 Recommended Directory Structure

- `config/main.yaml` (global)
- `config/agents.d/*.yaml` (one per Agent)
- `config/routing.yaml` (channel/connector -> agent binding)
- `prompts/<agent_id>/system.md`
- `prompts/<agent_id>/style.md`
- `prompts/<agent_id>/safety.md`

### 7.3 Configuration Validation

- Schema validation at startup (required fields, reference existence, duplicate ids)
- Validation failure blocks startup (fail fast)

## 8. LLM Provider Design (MVP)

### 8.1 Goals

- MVP first supports **Anthropic**
- Architecture supports extensibility from day one (OpenAI/OpenRouter/local inference can be plugged in later)

### 8.2 Core Abstraction

Uses `Provider Registry + Adapter`:

- `LlmProvider` (unified trait)
  - `chat(request) -> response`
  - `stream(request) -> stream` (optional, MVP can stub)
  - `health()` (optional)
- `ProviderRegistry`
  - Register/construct specific provider by `provider_id`
  - Core depends only on trait, not specific SDK

### 8.3 Model Resolution and Fallback

- `agent.model_policy.primary` specifies primary model
- `agent.model_policy.fallbacks` specifies fallback model chain
- Fallback triggers suggested: 429 / timeout / transient 5xx

### 8.4 Configuration Strategy (YAML)

Recommended directory:

- `config/providers.d/anthropic.yaml`
- `config/providers.d/*.yaml`

Keys don't go in agent config files, prefer environment variables/secret injection.

### 8.5 MVP Scope

- Implement `anthropic` adapter (can stub first)
- Reserve registry and provider trait
- Code structure as independent `clawhive-provider` crate

## 9. Skill System (MVP)

### 9.1 Goals

- Provide extensible capability description layer in MVP (not hardcoded in Core)
- Compatible with future plugin evolution

### 9.2 Technical Approach (Lightweight Version)

- Skill directory structure: `skills/<skill_name>/SKILL.md`
- `SKILL.md` uses frontmatter (`name/description/metadata`)
- Skill Loader responsible for:
  - Loading and merging (priority: workspace > user > builtin)
  - Requirements gating (`requires.bins` / `requires.env`)
  - Generate Skills index summary (for model low-cost awareness)

### 9.3 Prompt Injection Strategy

- By default only inject Skills summary (name/description/location)
- Agent reads corresponding `SKILL.md` body on demand when needed
- Avoid full injection causing context bloat

### 9.4 Relationship with Tool Schema

- Tool Schema: defines "how to call tool" (parameter contract)
- Skill: defines "when to call tool / how to complete task" (strategy and experience)
- MVP retains both, separate responsibilities

## 10. Project Structure (For Future Open Source Splitting)

Recommended Rust workspace (monorepo):

- `clawhive-gateway`: ingress layer
- `clawhive-core`: orchestrator + session + policy
- `clawhive-schema`: command/event DTOs (stable boundary)
- `clawhive-bus`: bus abstraction and in-memory implementation
- `clawhive-memory`: Markdown read/write + SQLite search index (chunks/FTS5/sqlite-vec)
- `clawhive-runtime`: executor interface (WASM adapter reserved)
- `clawhive-channels-telegram`: first channel driver
- `clawhive-sdk`: future plugin/third-party integration

### 10.1 Dependency Rules (Must Follow)

1. Cross-module communication only through `clawhive-schema`
2. `gateway` cannot directly depend on `memory` storage implementation
3. `core` depends on traits, not specific infrastructure implementations
4. Channel modules don't contain business decision code

---

## 11. CLI / TUI Support (MVP)

### 11.1 CLI (Required)

For one-off command operations:

- Start/stop service (gateway start/stop/restart)
- Configuration validation and loading
- Agent management (list/add/enable/disable)
- Task triggering and troubleshooting commands

### 11.2 TUI (Required, Developer-focused)

TUI has two responsibilities: **Real-time observation panel** + **Local Chat entry**.

#### Observation Panel

- Active Sessions panel
- Event Bus queue panel (inbound/outbound/backlog)
- Runs/Sub-Agent panel (status, duration, retry failures)
- Logs/Trace panel (filter by trace_id)

#### Local Chat Entry (Claude Code-like interaction experience)

TUI as local interaction channel, directly calls Orchestrator (bypasses Gateway), provides streaming conversation experience:

```
┌─ clawhive TUI ──────────────────────────────┐
│                                              │
│  You: Analyze the project architecture       │
│                                              │
│  clawhive-main: Let me check the structure...█  ← streaming character output
│                                              │
│  [tool: shell_exec("find crates -name...")] │  ← real-time tool_use display
│  [result: 10 crates found]                   │
│                                              │
│  The project uses Rust workspace with 10     │  ← continue streaming
│  crates:                                     │
│                                              │
├──────────────────────────────────────────────┤
│ > _                                          │
└──────────────────────────────────────────────┘
```

**Architecture Position:** TUI is parallel to TelegramBot, another channel entry, but uses in-process direct calls:

```
clawhive process
  ├── TelegramBot ──▶ Gateway ──▶ Orchestrator  (remote channel, full path)
  └── TUI ──▶ Orchestrator (streaming interface) (local channel, direct call)
```

TUI doesn't need to go through Gateway (no rate limiting/routing/auth needed for local use).

**Streaming + Tool Calling Alternating Execution Loop:**

```
loop {
    // 1. Streaming LLM call
    let stream = orchestrator.handle_inbound_stream(messages).await;

    // 2. Render each chunk to terminal in real-time
    for chunk in stream {
        tui.render_delta(chunk.delta);
    }

    // 3. Check for tool_use
    if has_tool_use(&response) {
        let tool_results = execute_tools(tool_calls).await;
        tui.render_tool_results(&tool_results);

        // Add tool_result to messages, continue next round (still streaming)
        messages.extend(tool_use_and_result_messages);
        continue;
    }

    break;  // No tool_use, end
}
```

**Streaming Pipeline to Connect:**

| Layer | Current Status | Need to Add |
|---|---|---|
| Provider `stream()` | ✅ Implemented (Anthropic SSE parsing complete) | - |
| LlmRouter `stream()` | ❌ Only has `chat()` | Add `stream()` method, route to provider.stream() |
| Orchestrator | ❌ Only sync `handle_inbound()` | Add `handle_inbound_stream()` returning `Stream<StreamChunk>` |
| TUI Chat panel | ❌ Not implemented | Consume stream, render per chunk + tool use alternating display |

> **Note**: Streaming for remote channels like Telegram (send_message + edit_message) is UX optimization, not in MVP scope. MVP streaming output focuses on TUI local Chat scenario.

Suggested implementation: `ratatui + crossterm`.

## 12. Execution Path (MVP)

### 12.1 Main Path (Synchronous Direct Calls)

```
TelegramBot
  │  teloxide long polling receives message
  │  construct InboundMessage
  │
  ▼  Arc<Gateway>.handle_inbound()     ← in-process function call
Gateway
  │  rate limit (TokenBucket) + resolve_agent
  │
  ▼  Arc<Orchestrator>.handle_inbound() ← in-process function call
Orchestrator
  │  Session management → Persona + Skill assembly
  │  → Memory recall → construct messages
  │
  ▼  HTTP POST → Anthropic API         ← only external network call
  │  ← LLM response
  │
  │  write memory files → construct OutboundMessage
  ▼  return Ok(OutboundMessage)         ← function return value
Gateway
  ▼  return Ok(outbound)                ← function return value
TelegramBot
  ▼  bot.send_message() → Telegram API  ← HTTP send reply
```

Design principle: main path is synchronous causal relationship (user sends message → must wait for LLM reply → send back), keep direct calls simplest and most reliable.

### 12.2 Sidecar Events (Bus Broadcast)

During main path execution, broadcast non-blocking events via Bus:

```
Main Path Node          ──publish──▶  Bus  ──▶  Consumer
Gateway                 MessageAccepted       TUI panel, audit log, metrics
Orchestrator            ReplyReady            TUI panel, audit log
Orchestrator            TaskFailed            TUI panel, alert system
Orchestrator            ToolExecuted (vNext)  audit log, TUI
Memory                  EpisodeWritten(vNext) TUI, stats
```

Sidecar events are fire-and-forget, consumer failures don't affect main path.

---

## 13. First Version Milestones (Recommended)

### M1 (Can Run Through)

- Telegram inbound/outbound
- Core basic routing
- Session persistence

### M2 (Usable)

- Markdown memory file read/write
- SQLite index build + hybrid search
- Memory injection before response

### M3 (Can Evolve)

- Daily consolidation task
- Conflict marking and simple forgetting
- Runtime WASM adapter skeleton

---

## 14. Conclusion

clawhive MVP recommends:

- **Architecture**: Gateway + Core/Orchestrator + Bus + Memory + Runtime
- **Memory**: Markdown files (source of truth) + SQLite search index
- **Engineering Strategy**: Implement light first, stabilize protocol first, protect boundaries first

This approach enables rapid delivery of the first version while ensuring modules can be independently open-sourced and extended later.
