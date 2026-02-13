# nanocrab

A Rust-based multi-agent AI bot framework with Markdown-native memory, hybrid search, and Telegram integration.

## Overview

nanocrab is a multi-agent AI framework written in Rust. It connects to Telegram (with more channels planned), routes messages to configurable AI agents, and maintains persistent memory across conversations.

The memory system follows a "Markdown as source of truth" philosophy — inspired by the OpenClaw project. Long-term knowledge lives in `MEMORY.md`, daily observations in `memory/YYYY-MM-DD.md` files, and raw conversation history in Session JSONL files. SQLite with sqlite-vec and FTS5 serves purely as a search index layer, enabling hybrid vector + full-text retrieval.

Each agent has its own persona (system prompts), model policy (primary + fallback LLMs), tool permissions, and memory access. Agents can spawn sub-agents for delegated tasks. A Weak ReAct loop provides iterative reasoning capabilities.

## Features

- Multi-agent orchestration with per-agent personas, model routing, and tool policies
- Three-layer memory system: Session JSONL (working memory) → Daily files (short-term) → MEMORY.md (long-term)
- Hybrid search: sqlite-vec vector similarity (70%) + FTS5 BM25 (30%) over memory chunks
- Hippocampus consolidation: periodic LLM-driven synthesis of daily observations into long-term memory
- Telegram channel adapter (multi-bot, multi-connector)
- Weak ReAct reasoning loop with repeat guard
- Sub-agent spawning with depth limits and timeout
- Skill system (SKILL.md with frontmatter + prompt injection)
- Token-bucket rate limiting per user
- LLM provider abstraction with retry + exponential backoff (Anthropic Claude supported)
- Real-time TUI dashboard (sessions, events, agent status)
- YAML-driven configuration (agents, providers, routing)

## Architecture

```
Telegram ──→ Gateway ──→ Orchestrator ──→ LLM Provider (Anthropic)
               │              │
          Rate Limiter    ┌───┴───┐
               │          │       │
            Routing    Memory   Sub-Agents
               │       │   │
            EventBus   │   Session JSONL
                       │
                 ┌─────┴──────┐
                 │  File Store │
                 │  MEMORY.md  │
                 │  daily/*.md │
                 └─────┬──────┘
                       │
                 ┌─────┴──────┐
                 │ SQLite Index│
                 │ sqlite-vec  │
                 │ FTS5        │
                 └─────────────┘
```

## Project Structure

```
crates/
├── nanocrab-cli/               # CLI binary (clap) — start, chat, validate, consolidate, agent/skill/session/task
├── nanocrab-core/              # Orchestrator, session mgmt, config, persona, skill system, sub-agent, LLM router
├── nanocrab-memory/            # Memory system — file store (MEMORY.md + daily), session JSONL, SQLite index, chunker, embedding
├── nanocrab-gateway/           # Gateway with agent routing and per-user rate limiting
├── nanocrab-bus/               # Topic-based in-process event bus (pub/sub)
├── nanocrab-provider/          # LLM provider trait + Anthropic Claude adapter (streaming, retry)
├── nanocrab-channels-telegram/ # Telegram adapter via teloxide
├── nanocrab-schema/            # Shared DTOs (InboundMessage, OutboundMessage, BusMessage, SessionKey)
├── nanocrab-runtime/           # Task executor abstraction (native + WASM skeleton)
└── nanocrab-tui/               # Real-time terminal dashboard (ratatui)

config/
├── main.yaml                   # App settings, channel configuration
├── agents.d/*.yaml             # Per-agent config (model policy, tools, memory, identity)
├── providers.d/*.yaml          # LLM provider settings (API keys, models)
└── routing.yaml                # Channel → agent routing bindings

prompts/<agent_id>/             # Per-agent persona prompts (system.md, style.md, safety.md)
skills/                         # Skill definitions (SKILL.md with frontmatter)
```

## Quick Start

Prerequisites: Rust 1.75+

```bash
# Clone
git clone https://github.com/longzhi/nanocrab.git
cd nanocrab

# Build
cargo build --workspace

# Validate configuration
cargo run -- validate

# Chat mode (local REPL, no Telegram needed)
export ANTHROPIC_API_KEY=your-key
cargo run -- chat --agent nanocrab-main

# Start Telegram bot
export TELEGRAM_BOT_TOKEN=your-token
export ANTHROPIC_API_KEY=your-key
cargo run -- start

# Start with TUI dashboard
cargo run -- start --tui
```

## Configuration

- `config/main.yaml` — app name, runtime settings, feature flags, channel config (Telegram connectors with `${ENV_VAR}` token resolution)
- `config/agents.d/<agent_id>.yaml` — agent identity (name, emoji), model policy (primary + fallbacks), tool policy, memory policy, sub-agent settings
- `config/providers.d/<provider>.yaml` — provider ID, API base URL, API key env var name, available models
- `config/routing.yaml` — default agent ID, channel-to-agent bindings (match by kind: dm/mention/group, optional pattern)

Model aliases: `sonnet` → `claude-sonnet-4-5`, `haiku` → `claude-3-5-haiku-latest`, `opus` → `claude-opus-4-6`

## Memory System

nanocrab uses a three-layer memory architecture inspired by neuroscience:

1. **Session JSONL** (`sessions/<id>.jsonl`) — append-only conversation log, typed entries (message, tool_call, tool_result, compaction, model_change). Used for session recovery and audit trail.
2. **Daily Files** (`memory/YYYY-MM-DD.md`) — daily observations written by LLM during conversations. Fallback summary generated if LLM didn't write anything in a session.
3. **MEMORY.md** — curated long-term knowledge. Updated by hippocampus consolidation (LLM synthesis of recent daily files) and direct LLM writes.
4. **SQLite Search Index** — sqlite-vec 0.1.6 + FTS5. Markdown files chunked (~400 tokens, 80 token overlap), embedded, indexed. Hybrid search: vector similarity × 0.7 + BM25 × 0.3.

Note: JSONL files are NOT indexed (too noisy). Only Markdown memory files participate in search.

## CLI Commands

| Command | Description |
|---------|-------------|
| `start [--tui]` | Start the Telegram bot (optionally with TUI dashboard) |
| `chat --agent <id>` | Local REPL for testing |
| `validate` | Validate YAML configuration |
| `consolidate` | Run memory consolidation manually |
| `agent list` | List configured agents |
| `agent show <id>` | Show agent details |
| `agent enable <id>` | Enable an agent |
| `agent disable <id>` | Disable an agent |
| `skill list` | List available skills |
| `skill show <name>` | Show skill details |
| `session reset <key>` | Reset a session |
| `task trigger <agent> <task>` | Send a one-off task to an agent |

## Development

```bash
# Run all tests
cargo test --workspace

# Lint
cargo clippy --workspace --all-targets -- -D warnings

# Format
cargo fmt --all
```

## Tech Stack

| Component | Technology |
|-----------|-----------|
| Language | Rust (2021 edition) |
| LLM | Anthropic Claude (Sonnet / Haiku / Opus) |
| Database | SQLite (rusqlite 0.32, bundled) |
| Vector Search | sqlite-vec 0.1.6 |
| Full-Text Search | FTS5 |
| Telegram | teloxide 0.13 |
| HTTP | reqwest 0.12 |
| Async | tokio |
| TUI | ratatui 0.29 + crossterm 0.28 |
| CLI | clap 4 |

## License

MIT

## Status

This project is under active development. The memory system is being rewritten from a structured episodes/concepts model to a Markdown-native model with hybrid search.
