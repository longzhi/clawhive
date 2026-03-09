# AGENTS.md

This file provides guidance to AI coding agents when working with code in this repository.

## Project Overview

Clawhive is a Rust-native single-binary (~14MB) multi-agent AI platform for deploying agents across messaging channels (Telegram, Discord, Slack, WhatsApp, iMessage). 13-crate workspace, edition 2021, Rust **1.92.0+**, version `0.1.0-alpha.*`.

## Build / Test / Lint

```bash
# One-time setup (git hooks: fmt+clippy on commit, full check on push)
just install-hooks

# Full CI-equivalent quality gate (run before every PR)
just check
# Equivalent to:
#   1. cargo fmt --all -- --check
#   2. cargo clippy --workspace --all-targets -- -D warnings
#   3. cargo test --workspace

# Individual commands
just fmt                # cargo fmt --all (auto-format)
just fmt-check          # cargo fmt --all -- --check
just clippy             # cargo clippy --workspace --all-targets -- -D warnings
just test               # cargo test --workspace
cargo build --release   # release binary

# Run a single test (by name substring)
cargo test -p clawhive-core -- policy::tests::check_exec -v

# Run all tests in one crate
cargo test -p clawhive-core

# Run tests matching a pattern
cargo test -p clawhive-scheduler -- integration

# Frontend (in web/ directory, use bun not npm)
cd web && bun install && bun run build
cd web && bun run dev   # dev server with proxy to localhost:3001

# Deploy to Mac Studio (pushes dev, builds remotely, restarts daemon)
./deploy.sh

# Version release (tags on main, not dev)
just release patch  # or minor/major
```

CI runs 4 parallel jobs on `ubuntu-latest`: check, test, clippy, fmt. `RUSTFLAGS=-Dwarnings` is set globally in CI — **all warnings are errors**.

## Workspace Structure

```
crates/
├── clawhive-cli/        # CLI binary (clap) — the only bin crate
├── clawhive-core/       # Orchestrator, tools, policy, skills, config — most logic lives here
├── clawhive-memory/     # Memory system (file store, JSONL sessions, SQLite index, embedding)
├── clawhive-gateway/    # Gateway, agent routing, rate limiting, scheduled task listener
├── clawhive-bus/        # In-process event bus (pub/sub)
├── clawhive-provider/   # LLM provider trait + multi-provider adapters
├── clawhive-channels/   # Channel adapters (Telegram, Discord, Slack, WhatsApp, iMessage)
├── clawhive-auth/       # OAuth and API key auth
├── clawhive-scheduler/  # Cron-based task scheduling
├── clawhive-server/     # HTTP API server (axum) + embedded SPA
├── clawhive-schema/     # Shared DTOs (InboundMessage, OutboundMessage, BusMessage)
├── clawhive-runtime/    # Task executor abstraction
└── clawhive-tui/        # Terminal dashboard (ratatui)
```

Dependency flow (top → bottom):

```
clawhive-cli
  ├─ clawhive-tui
  ├─ clawhive-server
  ├─ clawhive-gateway
  │    ├─ clawhive-channels
  │    └─ clawhive-core
  │         ├─ clawhive-provider
  │         ├─ clawhive-memory
  │         ├─ clawhive-scheduler
  │         └─ clawhive-auth
  ├─ clawhive-bus
  ├─ clawhive-runtime
  └─ clawhive-schema
```

### Key Source Files

- `clawhive-core/src/orchestrator.rs` — ReAct reasoning loop, tool execution, sub-agent spawning
- `clawhive-core/src/persona.rs` — Agent identity construction and system prompt
- `clawhive-core/src/skill.rs` — Skill loading, permission checking from SKILL.md
- `clawhive-core/src/shell_tool.rs` — Command execution with access gate
- `clawhive-core/src/access_gate.rs` — Two-layer security (hard baseline + origin-based trust)
- `clawhive-memory/src/store.rs` — SQLite + sqlite-vec hybrid search (70% vector + 30% FTS5)
- `clawhive-gateway/src/lib.rs` — Message routing and rate limiting

### Web Frontend (`web/`)

React 19 + Vite 6 + TailwindCSS 4 + TypeScript. State management via Zustand. API calls via TanStack Query. Path alias `@/` → `web/src/`. Dev proxy: `/api` → `localhost:3001`.

### Runtime Data Layout

```
~/.clawhive/
├── config/
│   ├── main.yaml              # App config, runtime, features, channels
│   ├── agents.d/*.yaml        # Agent definitions (identity, model, tools, memory)
│   ├── providers.d/*.yaml     # LLM provider credentials
│   └── routing.yaml           # Channel → agent bindings
├── workspaces/<agent_id>/     # Per-agent storage
│   ├── memory/MEMORY.md       # Long-term memory
│   ├── memory/YYYY-MM-DD.md   # Daily short-term memory
│   └── sessions/*.jsonl       # Session logs (append-only)
├── data/                      # SQLite databases
├── logs/                      # Log files
└── bin/                       # Installed binary
```

### Memory System

Three tiers: (1) Session JSONL for working memory, (2) Daily Markdown for short-term, (3) MEMORY.md for long-term (consolidated by "hippocampus" LLM synthesis). SQLite indexes chunks with sqlite-vec embeddings and FTS5 full-text search.

### Channel Adapters

Feature-gated in `clawhive-channels`: `telegram`, `discord`, `feishu`, `dingtalk`, and `wecom` enabled by default; `slack`, `whatsapp`, and `imessage` are optional features. Each implements the `ChannelBot` trait.

## Security Architecture

Two-layer security model. Know this before touching tool code:

1. **HardBaseline** (`policy.rs`) — Non-bypassable. Blocks SSRF, private keys, dangerous commands. Cannot be configured away.
2. **Origin-based Policy** (`policy.rs`) — `ToolOrigin::Builtin` (trusted) vs `ToolOrigin::External` (sandboxed by skill permissions).

Additional exec layers in `shell_tool.rs`:
- **ExecSecurityConfig** — Agent-level command allowlist (`exec_security.allowlist` in agent YAML).
- **Network approval** — Domain-level ask/allow/deny for outbound network in commands.
- **OS Sandbox** — `corral_core` process isolation for `execute_command`.

Skills declare permissions in `SKILL.md` YAML frontmatter (`permissions.exec`, `.fs`, `.network`, `.env`).

## Code Style

### Imports

Order: std → external crates → workspace crates → `super::`/`crate::` locals. One blank line between groups.

```rust
use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use clawhive_bus::EventBus;
use clawhive_schema::*;
use tokio::sync::Mutex;

use super::config::{ExecSecurityConfig, SecurityMode};
use super::tool::{ToolContext, ToolExecutor, ToolOutput};
```

### Error Handling

- **`anyhow::Result`** for application-level errors (orchestrator, tools, CLI).
- **`thiserror`** for library-level errors that cross crate boundaries.
- **Never** use `.unwrap()` in non-test code. Use `?`, `.context("reason")`, or explicit error handling.
- Match on `Result` and log with `tracing::warn!` before returning errors where appropriate.

### Structs and Config

- Derive order: `Debug, Clone, Serialize, Deserialize` (consistent across codebase).
- Use `#[serde(default)]` for optional fields with defaults. Use `#[serde(default = "fn_name")]` for non-trivial defaults.
- Config structs go in `config.rs`. Tool implementations each get their own file (`shell_tool.rs`, `file_tools.rs`, etc.).

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SomeConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}
```

### Naming

- Crate names: `clawhive-{name}` (kebab-case)
- Modules: `snake_case` (one file per major component)
- Structs/Enums: `PascalCase`
- Functions/methods: `snake_case`
- Constants: `SCREAMING_SNAKE_CASE`

### Logging / Tracing

Use `tracing` crate, not `log` or `println!`. Structured fields, not string interpolation:

```rust
tracing::info!(
    agent_id = %self.agent_id,
    command = %command_preview,
    timeout_secs = timeout_secs,
    "executing command in sandbox"
);
```

Use `target` for audit logs: `target: "clawhive::audit::network"`.

### Tests

- **Inline tests** in `#[cfg(test)] mod tests { }` at bottom of each file (primary pattern).
- **Integration tests** in `crates/{crate}/tests/` when needed.
- Use `tempfile::tempdir()` for filesystem tests. Use `wiremock` for HTTP mocking.
- Test function names describe behavior: `fn exec_security_deny_blocks_all_commands()`.

### Async

- Tokio runtime (`tokio = { features = ["full"] }`).
- Use `async_trait` for async trait methods.
- Use `Arc<T>` for shared state across tasks. `Arc<RwLock<T>>` for mutable shared state.

## Key Patterns

- **Tool implementations**: Implement `ToolExecutor` trait (`tool.rs`). Return `ToolOutput { content, is_error }`.
- **Bus events**: Publish via `bus.publish(BusMessage::SomeEvent { ... })`. Subscribe via `bus.subscribe(Topic::SomeEvent)`.
- **Config loading**: YAML files in `~/.clawhive/config/`. Parsed by `config.rs::load_config()`. Env vars resolved via `${VAR}` syntax.
- **Session keys**: `SessionKey::from_inbound(&msg)` derives from `(channel_type, connector_id, conversation_scope)`.

## Don'ts

- **No `unsafe`** without explicit justification.
- **No `.unwrap()`** outside of tests.
- **No `println!`** — use `tracing::*` macros.
- **No suppressing clippy** with `#[allow(...)]` without a comment explaining why.
- **No new dependencies** without checking if workspace already provides an equivalent.

## Git Workflow

- `main` is the only long-lived branch — no develop/release branches
- Small changes: commit and push directly to `main`
- Large changes: create a `feature/*` branch, then merge to `main`
- Release: tag on `main` (e.g. `v0.1.0`), CI auto-builds binaries and creates GitHub Release
- Bug fixes: fix on `main`, tag a patch release (e.g. `v0.1.1`)
- Workspace version in root `Cargo.toml` under `[workspace.package]`
