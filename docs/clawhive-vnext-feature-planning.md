# clawhive vNext: Full Feature Planning (Non-MVP)

> Purpose: Document clawhive's next phase (vNext/full version) design without disrupting MVP development pace.

---

## 1. Positioning and Scope

This document belongs to **vNext design**, explicitly not included in current MVP delivery scope.
MVP continues to focus on: main path usability (Gateway→Core→Reply), basic weak ReAct, real Telegram integration.

vNext focuses on:

1. Capability-based secure execution
2. Task/step/tool call terminology standardization
3. Fine-grained permission granting and audit system
4. Planner and execution layering

---

## 2. Terminology Standard (Recommended as Project Unified Language)

- **Task**: User goal (e.g., check Gmail for new emails)
- **Run**: One task execution instance
- **Plan**: Step sequence after task decomposition
- **Step**: Single execution step
- **Action**: Action type within step (respond/tool_call/finish)
- **Tool Call**: Specific tool invocation
- **Invocation**: Actual execution behavior
- **Capability Grant**: Permission set granted for a specific run/step
- **Trace**: Cross-module observable chain

Recommended code naming: `Task`, `Run`, `Step`, `ToolCall`, `CapabilityGrant`.

---

## 3. Capability Permission Model (Capability-based Execution)

## 3.1 Core Principles

1. **Default zero permissions (deny by default)**
2. **Per-task minimal authorization (least privilege)**
3. **Permissions reclaimed with lifecycle (ephemeral grants)**
4. **High-risk capabilities require approval (human-in-the-loop)**

## 3.2 Authorization Granularity

1. **Task level**: Capability boundary allowed for this task
2. **Step level**: Temporary elevation for specific step
3. **Resource level**: Precise to directory/API scope/host capability

## 3.3 Execution Classification

- **Safe**: Auto-execute
- **Guarded**: Execute after session-level or one-time approval
- **Unsafe**: Requires confirmation for each invocation

Note:
- Wasm is a strong isolation foundation, but not the only admission condition
- Non-Wasm tools can enter Guarded/Unsafe, shouldn't be completely prohibited

---

## 4. WASM Sandbox and Host Tool Proxy

## 4.1 Design Points

- WASM instance created at task startup
- mount/capability determined at instance creation
- Instance and permission context destroyed after task ends

## 4.2 Host Capability Access Recommendations

For system capabilities like Gmail, macOS Calendar:

- Don't recommend WASM directly accessing system APIs
- Recommend host tool proxy (e.g., `gmail.read`, `calendar.add`)
- Runtime controls capability + audit logging

---

## 5. Execution Architecture Layers (vNext)

1. **Planner** (Optional)
   - Task decomposition, re-planning, parallel suggestions
2. **Orchestrator**
   - Run state machine, step scheduling, policy decisions
3. **Policy Engine**
   - Capability matching, risk classification, approval triggering
4. **Executor**
   - Wasm execution or host proxy calls
5. **Audit/Telemetry**
   - Full-chain trace and permission auditing

---

## 6. Audit and Observability (Required)

Minimum recording per execution:

- `trace_id`, `task_id`, `run_id`, `step_id`
- `tool_call`
- `requested_capabilities`
- `granted_capabilities`
- `approval_required/approval_result`
- `start_at/end_at/status/error`

---

## 7. Boundary with MVP

### 7.1 MVP Retains

- Weak ReAct
- Basic tool calling
- Basic routing and configuration

### 7.2 vNext Will Do

- Complete capability policy engine
- Fine-grained permission lifecycle management
- Planner plugin support
- Structured audit panel (TUI/Web)

---

## 8. Storage and Vector Retrieval (Extension)

> Basic SQLite + sqlite-vec solution already included in MVP (for episodes vector retrieval and structured storage).
> vNext extends more application scenarios on this foundation.

### 8.1 vNext Extended Applications

Building on MVP's episodes semantic recall, vNext further leverages sqlite-vec:

- **Audit log structured storage and query** (trace/run/step, supporting §6 audit requirements)
- **Tool description & capability semantic retrieval** (embedding → sqlite-vec, supports dynamic tool discovery)
- **Cross-agent knowledge sharing** (vector index of shared concepts table)
- **Task/step state persistence and similar task recall**

### 8.2 vNext Memory Enhancements

Memory features deferred from MVP to vNext:

- **Semantic-aware chunking**: Split long text by Markdown headings, long paragraphs fall back to fixed window splitting; improves embedding quality
- **Local Embedding model**: MVP reserves `EmbeddingProvider` trait interface, vNext implements local model (e.g., `ort` + ONNX), eliminates remote API dependency
- **Memory compression (Compaction)**: Reference OpenClaw's auto-compaction design, summarize and compress long episode sequences, preserve key info while reducing token consumption
- **Multi-modal memory**: Image/file embedding indexing and recall

---

## 9. Tool Calling System

### 9.1 Design Goals

Build a complete, multi-provider compatible native tool calling system on top of MVP's basic tool calling.

Core principles:

1. **Use Provider native Tool Use protocol** (not text parsing), structured, type-safe
2. **Unified abstraction layer**: Core/Orchestrator only operates on unified types, unaware of provider protocol differences
3. **Skill guidance + Tool execution**: Skill provides strategy knowledge (when to use, how to use), Tool provides execution capability (parameter contract + actual call)
4. **Integration with Capability model**: Tool calls constrained by permission policies (§3)

### 9.2 Unified Tool Types

Placed in `clawhive-schema` or `clawhive-provider/types.rs`:

```rust
/// Tool definition: registered to ToolRegistry, passed to LLM Provider
struct ToolDef {
    name: String,
    description: String,
    parameters: serde_json::Value,  // JSON Schema
}

/// Tool call: structured call request returned by LLM
struct ToolCall {
    id: String,           // call ID generated by provider
    name: String,         // tool name
    input: serde_json::Value,  // parameters
}

/// Tool result: passed back to LLM after execution
struct ToolResult {
    tool_call_id: String, // corresponds to ToolCall.id
    output: String,       // execution output
    is_error: bool,       // whether execution failed
}
```

### 9.3 Message Model Extension

Existing `LlmMessage.content` extends from pure `String` to `Vec<ContentBlock>`:

```rust
enum ContentBlock {
    Text(String),
    ToolUse(ToolCall),
    ToolResult(ToolResult),
}

struct LlmMessage {
    role: String,
    content: Vec<ContentBlock>,
}

struct LlmRequest {
    model: String,
    system: Option<String>,
    messages: Vec<LlmMessage>,
    tools: Vec<ToolDef>,       // new: available tools list
    max_tokens: u32,
}

struct LlmResponse {
    content: Vec<ContentBlock>, // extended: may contain text + tool_use mixed
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
    stop_reason: Option<String>,
}
```

> **Breaking Change Note**: `content: String` → `Vec<ContentBlock>` requires adapting all existing code. Recommend providing `LlmMessage::text(role, content)` convenience constructor for backward compatibility.

### 9.4 Multi-Provider Protocol Adaptation

Each provider's tool use protocol format differs, but semantics are consistent:

| Provider | Request Format | Response Format | tool_result Passing Method |
|----------|----------------|-----------------|---------------------------|
| **Anthropic** | `tools` array | `tool_use` content block | `tool_result` content block (same message) |
| **OpenAI / Compatible** | `tools` array | `tool_calls` in assistant message | `role: "tool"` separate message |
| **Google Gemini** | `tools` + `functionDeclarations` | `functionCall` in parts | `functionResponse` in parts |
| **Local Inference** | Mostly OpenAI compatible | Same as OpenAI | Same as OpenAI |

Adaptation strategy:

```
clawhive-core (unified abstraction)          clawhive-provider (each Adapter)
┌───────────────────────┐                    ┌──────────────────────────┐
│ ToolDef               │                    │ AnthropicAdapter         │
│ ToolCall              │────────────────────▶│  → tools content blocks  │
│ ToolResult            │                    ├──────────────────────────┤
│ ContentBlock          │◀───────────────────│ OpenAIAdapter            │
│                       │                    │  → tools + tool_calls    │
│                       │                    ├──────────────────────────┤
│                       │                    │ GeminiAdapter            │
│                       │                    │  → functionDeclarations  │
│                       │                    └──────────────────────────┘
└───────────────────────┘
```

Each Adapter implements two conversions:
1. **Request conversion**: unified `ToolDef` + `ContentBlock` → provider-specific API format
2. **Response conversion**: provider-specific response → unified `ContentBlock`

LlmProvider trait extension:

```rust
#[async_trait]
trait LlmProvider: Send + Sync {
    async fn chat(&self, request: LlmRequest) -> Result<LlmResponse>;
    fn supports_tools(&self) -> bool { false }  // new: capability declaration
    // ...
}
```

### 9.5 Tool Registration and Management

```rust
struct ToolRegistry {
    tools: HashMap<String, RegisteredTool>,
}

struct RegisteredTool {
    def: ToolDef,                    // tool definition (passed to LLM)
    executor: Arc<dyn ToolExecutor>, // tool executor
    risk_level: RiskLevel,           // Safe / Guarded / Unsafe (connects to §3 permission model)
}

#[async_trait]
trait ToolExecutor: Send + Sync {
    async fn execute(&self, input: serde_json::Value) -> Result<String>;
}
```

Tool sources:
- **Built-in tools**: `shell_exec`, `file_read`, `file_write`, `web_fetch`, etc.
- **Skill-declared tools**: Skill frontmatter can declare `requires.tools`
- **Dynamic registration**: Add via config or API dynamically (vNext later)

Per-agent filtering:
- `ToolPolicyConfig.allow` whitelist controls tool set visible to each agent
- Tools not in whitelist aren't passed to LLM

### 9.6 Tool Calling Execution Loop

Replaces existing `weak_react_loop`, implements complete tool use loop:

```
1. Assemble messages + tools → call LLM
2. Parse LlmResponse.content:
   ├── Only Text → return directly (no tool calls)
   └── Contains ToolUse →
       a. For each ToolCall:
          ├── Permission check (Capability Policy)
          ├── Safe → execute directly
          ├── Guarded → check if already approved
          └── Unsafe → trigger NeedHumanApproval, await confirmation
       b. Execute tools, collect ToolResults
       c. Append assistant message (with ToolUse) + ToolResult to messages
       d. Go back to step 1 (continue calling LLM)
3. Loop limit: max_tool_rounds (prevent infinite loops)
4. Repeat detection: consecutive identical tool calls → interrupt
```

### 9.7 Tool and Skill Collaboration

```
Skill (Knowledge Layer)              Tool (Execution Layer)
┌─────────────────┐                  ┌─────────────────┐
│ SKILL.md        │                  │ ToolDef          │
│ - when to use   │                  │ - name           │
│ - operation     │   guides         │ - parameters     │
│   steps         │─────────────────▶│ - executor       │
│ - precautions   │                  │                  │
│ - requires.tools│                  │                  │
└─────────────────┘                  └─────────────────┘
       │                                    │
       ▼                                    ▼
  inject into system prompt          pass into LLM tools parameter
  (LLM reads and understands)        (LLM makes structured calls)
```

- **Skill doesn't execute anything**: Only serves as prompt knowledge injection, helps LLM understand when/how to use tools
- **Tool doesn't contain strategy**: Only defines parameter contract and execution logic
- **LLM is the decision maker**: Based on Skill knowledge + Tool definition, autonomously decides invocation timing and parameters

### 9.8 Integration with Capability Model

Tool calling is the most direct application scenario of Capability permission model (§3):

- Each `RegisteredTool` has `risk_level`
- Policy Engine checks before tool execution
- `CapabilityGrant` records authorization details, written to audit log (§6)
- High-risk tools (like `shell_exec`) default to Unsafe, require confirmation each time

### 9.9 MVP vs vNext Boundary

| Capability | MVP | vNext |
|------------|-----|-------|
| Provider native tool_use (Anthropic) | ✅ Implement first | — |
| Unified ToolDef / ToolCall / ToolResult types | ✅ | — |
| ContentBlock message model | ✅ | — |
| Basic built-in tools (shell_exec, file_read) | ✅ | — |
| Tool execution loop (replace weak_react_loop) | ✅ | — |
| Multi-Provider adaptation (OpenAI/Gemini) | — | ✅ |
| ToolRegistry dynamic registration | — | ✅ |
| Capability permission integration | — | ✅ |
| Tool semantic retrieval (sqlite-vec) | — | ✅ |
| Skill.requires.tools auto-association | — | ✅ |

---

## 10. Sub-Agent Evolution Design

### 10.1 Three-Phase Evolution Roadmap

#### Phase 1: Independent LLM Call (MVP — Current Skeleton Exists)

Sub-Agent is essentially an independent LLM call, has its own agent config (model, persona), but only does single-turn dialogue.

```
Parent Agent
  │  spawn(task, agent_id)
  ▼
SubAgentRunner → tokio::spawn
  │  independent messages + system prompt
  │  router.chat() → single-turn LLM call
  ▼
SubAgentResult { output: String }
  ▼  merge back to parent context
```

**Current Status:** `SubAgentRunner` skeleton implemented (spawn/cancel/wait_result/result_merge), but not integrated into Orchestrator, lacks triggering mechanism.

**MVP Completion Items:**
- Integrate SubAgentRunner into Orchestrator
- Define trigger conditions (LLM explicit request or rule-based trigger)
- Suitable scenarios: Simple subtasks (translation, summary, format conversion)

#### Phase 2: Complete Agent Instance (vNext First Step)

Sub-Agent is a fully capable agent instance, can call tools, multi-turn reasoning, access memory.

```
Parent Agent (Orchestrator)
  │  spawn(task, agent_id)
  ▼
Sub-Agent (Independent Complete Agent)
  ├── own session
  ├── own memory (can read parent episodes, write to isolated partition)
  ├── own toolset (subset of parent, least privilege)
  ├── own tool use / ReAct loop
  └── can spawn sub-sub-agent (limited by max_depth)
  ▼
Final result returned to Parent
```

**Key Design:**
- `spawn()` creates mini Orchestrator instance, reuses core logic (tool use loop, memory access)
- Memory isolation strategy: sub-agent can read parent's episodes, but writes to isolated partition (`session_id` differentiated)
- Toolset defaults to more restricted than parent (`sub_agent.allowed_tools` whitelist)
- Must carry `parent_run_id` + `trace_id` for audit trail tracing
- Suitable scenarios: Complex independent tasks (code analysis, research reports, multi-step troubleshooting)

#### Phase 3: Agent-as-Tool (vNext Second Step)

Sub-Agent wrapped as Tool, Parent Agent autonomously triggers via normal tool_use mechanism, no explicit orchestration needed.

```
Parent Agent
  │  LLM returns tool_use: { name: "research_agent", input: { topic: "..." } }
  ▼
ToolExecutor recognizes agent-type tool
  │  creates Sub-Agent (Phase 2 complete instance)
  │  runs independently until completion
  ▼
tool_result: { output: "Research report..." }
  ▼  passed back to LLM to continue generation
```

**Key Design:**
- Register agent-type tools in ToolRegistry (`RegisteredTool`'s executor is `AgentToolExecutor`)
- ToolDef describes agent's capabilities, lets parent LLM understand when to delegate
- Completely transparent to parent agent—it just called a tool, unaware another agent is behind it
- Integration with Capability model: spawning sub-agent itself is an operation requiring permission
- Suitable scenarios: LLM autonomously decides when to delegate, no external orchestration logic needed

### 10.2 Three-Phase Comparison

| | Phase 1: Independent LLM Call | Phase 2: Complete Agent Instance | Phase 3: Agent-as-Tool |
|---|---|---|---|
| Timeline | MVP | vNext First Step | vNext Second Step |
| Capability | Single-turn Q&A | Multi-turn + Tools + Memory | Multi-turn + Tools + Memory |
| Trigger Method | Code explicit spawn | Code explicit spawn | LLM autonomous tool call |
| Tool Access | ❌ | ✅ (parent subset) | ✅ (parent subset) |
| Memory Access | ❌ | ✅ (read shared/write isolated) | ✅ (read shared/write isolated) |
| Recursive Spawn | ❌ | ✅ (limited by max_depth) | ✅ |
| Transparent to Parent | ❌ (needs explicit orchestration) | ❌ (needs explicit orchestration) | ✅ (just a tool) |

---

## 11. Recommended Implementation Pace

1. Keep this file in `docs/` as vNext design baseline
2. After MVP completion, first implement `CapabilityGrant` data structure + audit logging
3. Multi-Provider tool calling adaptation (OpenAI/Gemini) + ToolRegistry dynamic registration
4. Sub-Agent Phase 2 (complete Agent instance) + memory isolation
5. Introduce Policy Engine (Safe/Guarded/Unsafe) + tool permission integration
6. Sub-Agent Phase 3 (Agent-as-Tool)
7. Finally connect Planner and advanced execution strategies

---

## 12. Conclusion

clawhive's vNext adoption of "Capability-based, per-task least-privilege" model will significantly improve:

- Security (default minimal authorization)
- Controllability (approval and policy layering)
- Explainability (audit traceability)
- Extensibility (WASM and host proxy coexistence)

This model is recommended as one of clawhive's full version core capabilities.
