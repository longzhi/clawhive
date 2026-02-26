# clawhive Code Review Issues

> Source: 2026-02-13 message entry path review (Telegram → Agent)  
> Status markers: 🔴 To Fix | 🟡 To Discuss | 🟢 Resolved

---

## Issue #1: Bus is Sidecar, Not Main Path Driver

**Status:** 🟡 Deferred to M2/M3  
**Modules:** `clawhive-gateway`, `clawhive-bus`  
**Description:**  
Current message flow is direct synchronous call chain from TelegramBot → Gateway → Orchestrator. Bus is only used for sidecar notifications (`MessageAccepted` / `ReplyReady` / `TaskFailed`). This differs from the "Command/Event driven" pattern designed in MVP technical document §3.  
**Impact:** Module coupling is higher than expected; adding new channels or async orchestration will require refactoring call patterns.  
**Recommendation:** Acceptable for MVP phase, but should switch main path to Bus-driven in M2/M3 phase (Gateway publish Command → Core subscribe and handle → publish Event → Gateway write back).

> **MVP Decision:** Keep current Bus sidecar architecture, switch to Bus-driven main path in M2/M3 phase.

---

## Issue #2: No Message Queue Buffer, Slow LLM Response Causes Backlog

**Status:** 🟢 Resolved  
**Module:** `clawhive-channels-telegram`  
**Description:**  
`TelegramBot::run()`'s endpoint closure directly awaits Gateway return. If LLM response is slow (seconds or even timeout), teloxide dispatcher's concurrent processing capacity is limited, potentially causing message backlog or loss.  
**Impact:** Poor user experience in high-concurrency scenarios, message processing may timeout.  
**Fix:** In endpoint, first send `ChatAction::Typing`, then `tokio::spawn` the gateway call, endpoint returns immediately. Spawned task actively calls `bot.send_message()` to send reply when complete.

---

## Issue #3: Session Doesn't Load Conversation History

**Status:** 🟢 Resolved  
**Module:** `clawhive-core/orchestrator.rs`  
**Description:**  
In `Orchestrator::handle_inbound()`, `SessionManager::get_or_create()` only manages session metadata (create/renew/expire), doesn't add session's historical conversation messages to LLM's messages list. Current conversation only has:
- Memory recall episodes (as `[memory context]`)
- Current user input

Missing conversation history (recent N turns), causing agent unable to conduct continuous multi-turn dialogue.  
**Impact:** User experience: agent has no short-term conversation memory, every interaction is like a new conversation.  
**Fix:** `handle_inbound` loads recent 10 conversation history messages via `SessionReader::load_recent_messages()`, injected into LLM messages list after memory context and before current user message. Session JSONL serves as history source.

---

## Issue #4: Runtime `execute()` Semantics Unclear

**Status:** 🟢 Resolved  
**Modules:** `clawhive-core/orchestrator.rs`, `clawhive-runtime`  
**Description:**  
`runtime.execute()` is called twice in `handle_inbound`:
1. Processing user input text: `self.runtime.execute(&inbound.text)`
2. Processing LLM output text: `self.runtime.execute(&reply_text)`

From context, `NativeExecutor` might be pass-through (returns as-is), but semantics are unclear—why does user input need to go through runtime execute? And why LLM output?  
**Impact:** Poor code readability, future maintainers easily confused. If execute has side effects, may produce unexpected behavior.  
**Fix:** `TaskExecutor::execute()` split into `preprocess_input()` (user input preprocessing) and `postprocess_output()` (LLM output postprocessing), clear semantics. NativeExecutor both are passthrough, WasmExecutor reserves sandbox processing.

---

## Issue #5: Weak ReAct Missing Prompt Instructions

**Status:** 🟢 Resolved  
**Modules:** `clawhive-core/orchestrator.rs`, `clawhive-core/persona.rs`  
**Description:**  
`weak_react_loop()` relies on LLM outputting specific markers (`[think]`, `[action]`, `[finish]`) to drive the loop, but currently no system prompt injection of usage instructions for these markers is visible. Whether Persona's `assembled_system_prompt()` and Skill's `summary_prompt()` include ReAct instructions needs confirmation.  
**Impact:** If LLM doesn't know these markers exist, will never output `[think]`/`[action]`, ReAct loop actually degrades to single-turn call.  
**Fix:** `tool_use_loop` replaces `weak_react_loop` as main loop. Drives multi-turn tool calling through Anthropic native tool calling API (`tool_use` stop_reason + `tool_result` messages), no longer relies on text markers. `ToolRegistry` registers `memory_search` and `memory_get` tools, definitions passed to API via JSON Schema.

---

## Issue #6: TelegramBot Endpoint Blocks Dispatcher

**Status:** 🟢 Resolved (same as Issue #2)  
**Module:** `clawhive-channels-telegram`  
**Description:**  
Current TelegramBot endpoint handler directly `await gateway.handle_inbound(inbound)`, blocking teloxide dispatcher during LLM response period (5-30 seconds). With multiple concurrent users, subsequent messages queue waiting; in severe cases may lose messages due to long polling timeout.  
**Impact:** Poor user experience in concurrent scenarios, message processing may timeout or be lost.  
**Fix:** Same as Issue #2. Endpoint sends `ChatAction::Typing` then `tokio::spawn` async processing, immediately returns to dispatcher.

---

## Issue #7: Bus Events Have No Consumers

**Status:** 🟢 Resolved  
**Module:** `clawhive-bus`  
**Description:**  
Bus currently publishes `MessageAccepted`, `ReplyReady`, `TaskFailed` and other events, but no code subscribes to and consumes these events. Bus is in "publishing but nobody listening" state.  
**Impact:** Bus occupies code but has no actual function, TUI panel and audit log also have no data source.  
**Fix:** TUI now subscribes and handles all 10 event types. 6 events (CancelTask, RunScheduledConsolidation, MemoryWriteRequested, NeedHumanApproval, MemoryReadRequested, ConsolidationCompleted) have no production code publishing yet—these are feature placeholders, will naturally integrate when corresponding features are implemented.

---

## Issue #8: SubAgentRunner Not Integrated with Orchestrator

**Status:** 🟢 Resolved  
**Modules:** `clawhive-core/orchestrator.rs`, `clawhive-core/subagent.rs`  
**Description:**  
`SubAgentRunner` skeleton is implemented (spawn/cancel/wait_result/result_merge), but no code in Orchestrator uses it. Sub-Agent capability is in "written but not connected" state.  
**Impact:** MVP document §6 explicitly requires Sub-Agent as must-have, currently unusable.  
**Fix:** Created `SubAgentTool` implementing `ToolExecutor` trait, registered to `ToolRegistry` with `delegate_task` tool name. LLM can trigger sub-agent spawn via tool_use call, synchronously waits for result return. Orchestrator automatically creates `SubAgentRunner` and registers this tool in `new()`.

---

## Issue #9: Streaming Output Path Not Connected (Provider Implemented, Upper Layers Not Integrated)

**Status:** 🟢 Resolved  
**Modules:** `clawhive-core/router.rs`, `clawhive-core/orchestrator.rs`, `clawhive-tui`  
**Description:**  
`AnthropicProvider::stream()` and `StreamChunk` type fully implemented (SSE parsing, three event types), but upper layer path completely not integrated:
- `LlmRouter` only has `chat()`, no `stream()`
- `Orchestrator` only has sync `handle_inbound()`, no streaming interface
- TUI has no Chat panel consuming stream

**Impact:** TUI as local Chat entry cannot provide streaming interaction experience, character-by-character output similar to Claude Code cannot be achieved.  
**Fix:** Three layers connected:
1. `LlmRouter::stream()` — routes to provider.stream(), supports fallback (only before stream starts)
2. `Orchestrator::handle_inbound_stream()` — tool_use_loop remains blocking, final response streams back, simultaneously publishes `StreamDelta` bus events
3. TUI `StreamDelta` handler — Logs panel displays streaming delta in real-time
4. `BusMessage::StreamDelta` + `Topic::StreamDelta` — schema/bus layer adds streaming event type

---

## Future Review Plan

- [ ] Memory system storage details (MemoryStore / retrieve_context / consolidation)
- [ ] Provider implementation (Anthropic adapter)
- [ ] Config loading and validation path
- [ ] Skill system loading and injection
- [ ] Sub-Agent spawn and lifecycle
