# OpenAI Provider + Discord Channel + Channels Crate Merge

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add OpenAI-compatible LLM provider (chat + streaming + tool calling) and Discord channel adapter, while merging all channel adapters into a single `clawhive-channels` crate with feature flags and a `ChannelBot` trait.

**Architecture:** Three parallel workstreams: (1) `clawhive-provider/src/openai.rs` implementing `LlmProvider` with OpenAI Chat Completions API format conversion, (2) `clawhive-channels` crate replacing `clawhive-channels-telegram` with feature-gated `telegram` and `discord` modules plus a shared `ChannelBot` trait, (3) CLI and config wiring to register both providers and start all enabled channel bots generically.

**Tech Stack:** reqwest + async-stream for OpenAI HTTP/SSE, serenity 0.12 for Discord gateway, teloxide 0.13 (existing) for Telegram.

---

## Task 1: Create OpenAI provider implementation

**Files:**
- Create: `crates/clawhive-provider/src/openai.rs`
- Modify: `crates/clawhive-provider/src/lib.rs` — add `pub mod openai; pub use openai::OpenAiProvider;`

**What to implement:**

`OpenAiProvider` struct with `client: reqwest::Client`, `api_key: String`, `api_base: String`.

Constructor: `new(api_key, api_base)`, `from_env(api_key_env, api_base)`.

`impl LlmProvider`:
- `chat()`: POST `{api_base}/chat/completions`, `Authorization: Bearer {api_key}`, convert `LlmRequest` → OpenAI format, parse response back to `LlmResponse`.
- `stream()`: Same endpoint with `stream: true` + `stream_options: {include_usage: true}`, parse SSE chunks.

**Key format conversions (all internal to openai.rs):**

Request mapping:
- `LlmRequest.system` → separate message `{role: "system", content: "..."}`
- `LlmRequest.tools` → `[{type: "function", function: {name, description, parameters: input_schema}}]`
- `ContentBlock::ToolUse` → assistant message with `tool_calls: [{id, type: "function", function: {name, arguments: JSON.stringify(input)}}]`
- `ContentBlock::ToolResult` → separate message `{role: "tool", tool_call_id, content}`

Response mapping:
- `choices[0].message.content` → `ContentBlock::Text`
- `choices[0].message.tool_calls` → `ContentBlock::ToolUse` (parse `arguments` string as JSON)
- `finish_reason: "tool_calls"` → `stop_reason: Some("tool_use")` (normalize to Anthropic convention used by orchestrator)
- `usage.prompt_tokens` → `input_tokens`, `usage.completion_tokens` → `output_tokens`

Error handling: same pattern as AnthropicProvider — classify by status code, mark 429/5xx as `[retryable]`.

SSE streaming: parse `data: {...}` lines, emit `StreamChunk` for `choices[0].delta.content`, final chunk when `finish_reason` present. Handle `data: [DONE]`.

**Tests (in same file #[cfg(test)] mod tests):**
- `to_api_request_maps_tools_and_messages` — verify request serialization
- `to_api_request_includes_system_as_first_message` — system prompt becomes system role message
- `api_response_deserialization_with_tool_calls` — parse response JSON with tool_calls
- `api_response_deserialization_text_only` — parse simple text response
- `format_api_error_retryable_for_429` — error classification
- `format_api_error_not_retryable_for_401` — auth errors not retryable
- `parse_sse_event_content_delta` — streaming text chunk
- `parse_sse_event_finish_with_usage` — final chunk with usage
- `to_api_messages_handles_tool_result` — ToolResult becomes role=tool message
- `from_env_missing_key_returns_error`

**Commit:** `feat(provider): add OpenAI-compatible provider with chat, streaming, and tool calling`

---

## Task 2: Create `clawhive-channels` crate with ChannelBot trait

**Files:**
- Create: `crates/clawhive-channels/Cargo.toml`
- Create: `crates/clawhive-channels/src/lib.rs` — ChannelBot trait + feature-gated re-exports
- Create: `crates/clawhive-channels/src/telegram.rs` — move from `clawhive-channels-telegram/src/lib.rs`
- Create: `crates/clawhive-channels/src/discord.rs` — new Discord adapter

**Cargo.toml:**
```toml
[package]
name = "clawhive-channels"
version.workspace = true
edition.workspace = true
license.workspace = true

[features]
default = ["telegram", "discord"]
telegram = ["dep:teloxide", "dep:log"]
discord = ["dep:serenity"]

[dependencies]
anyhow.workspace = true
tokio.workspace = true
serde.workspace = true
uuid.workspace = true
chrono.workspace = true
tracing.workspace = true
async-trait.workspace = true
clawhive-schema = { path = "../clawhive-schema" }
clawhive-gateway = { path = "../clawhive-gateway" }

teloxide = { workspace = true, optional = true }
log = { workspace = true, optional = true }
serenity = { workspace = true, optional = true }
```

**lib.rs:**
```rust
use std::sync::Arc;
use clawhive_gateway::Gateway;

#[async_trait::async_trait]
pub trait ChannelBot: Send {
    fn channel_type(&self) -> &str;
    fn connector_id(&self) -> &str;
    async fn run(self: Box<Self>) -> anyhow::Result<()>;
}

#[cfg(feature = "telegram")]
pub mod telegram;

#[cfg(feature = "discord")]
pub mod discord;
```

**telegram.rs:** Copy from `clawhive-channels-telegram/src/lib.rs`, add `impl ChannelBot for TelegramBot`.

**discord.rs:** New implementation:
- `DiscordAdapter` with `to_inbound(guild_id: Option<u64>, channel_id: u64, user_id: u64, text)` → `InboundMessage`
- `DiscordBot` with `new(token, connector_id, gateway)` + `impl ChannelBot`
- `DiscordHandler` implementing serenity's `EventHandler`:
  - `ready()` — log connected
  - `message()` — ignore bots, detect mentions via `msg.mentions`, send typing, tokio::spawn gateway call + reply
- Tests: `adapter_to_inbound_dm_sets_fields`, `adapter_to_inbound_guild_sets_fields`, `render_outbound_formats_correctly`

**Commit:** `feat(channels): create unified channels crate with ChannelBot trait, telegram, and discord`

---

## Task 3: Remove old `clawhive-channels-telegram` crate

**Files:**
- Delete: `crates/clawhive-channels-telegram/` (entire directory)
- Modify: `Cargo.toml` (workspace) — remove `clawhive-channels-telegram` from members, add `clawhive-channels`, add `serenity` to workspace.dependencies
- Modify: `crates/clawhive-cli/Cargo.toml` — replace `clawhive-channels-telegram` with `clawhive-channels`

**Commit:** `refactor: remove old clawhive-channels-telegram crate in favor of clawhive-channels`

---

## Task 4: Wire OpenAI provider + Discord channel into CLI

**Files:**
- Modify: `crates/clawhive-cli/src/main.rs`
  - Import `OpenAiProvider` from clawhive-provider
  - Import `ChannelBot` trait + `TelegramBot`, `DiscordBot` from clawhive-channels
  - Add `"openai"` arm in `build_router_from_config` match
  - Refactor `start_bot()` to collect `Vec<Box<dyn ChannelBot>>` and spawn all
- Modify: `crates/clawhive-provider/src/lib.rs` — add `pub use openai::OpenAiProvider;`
- Modify: `crates/clawhive-core/src/config.rs`:
  - Add `DiscordConnectorConfig`, `DiscordChannelConfig`
  - Add `discord: Option<DiscordChannelConfig>` to `ChannelsConfig`
  - Update `resolve_main_env` for discord connectors

**Config files:**
- Create: `config/providers.d/openai.yaml`
  ```yaml
  provider_id: openai
  enabled: false
  api_base: https://api.openai.com/v1
  api_key_env: OPENAI_API_KEY
  models:
    - gpt-4o
    - gpt-4o-mini
  ```
- Modify: `config/main.yaml` — add `discord: null` under channels
- Modify: `config/routing.yaml` — add discord routing bindings (commented out)

**Commit:** `feat(cli): wire OpenAI provider and Discord channel into startup`

---

## Task 5: Final verification

Run:
```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

All must pass. Fix any issues, then commit if needed.

---

## Execution Notes

- Tasks 1 and 2 are **independent** and can be dispatched in parallel
- Task 3 depends on Task 2 completing
- Task 4 depends on Tasks 1, 2, 3 all completing
- Task 5 is final verification

**Important normalization:** The orchestrator's `tool_use_loop` checks `stop_reason == "tool_use"`. OpenAI uses `finish_reason: "tool_calls"`. The OpenAI provider MUST normalize this: map `"tool_calls"` → `"tool_use"` in the response, and `"stop"` → `"end_turn"`, so the orchestrator works without changes.
