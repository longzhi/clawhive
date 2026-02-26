# clawhive

[![CI](https://github.com/longzhi/clawhive/actions/workflows/ci.yml/badge.svg)](https://github.com/longzhi/clawhive/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Rust](https://img.shields.io/badge/rust-1.92%2B-orange.svg)](https://www.rust-lang.org/)
[![GitHub release](https://img.shields.io/github/v/release/longzhi/clawhive?include_prereleases)](https://github.com/longzhi/clawhive/releases)

A Rust-native multi-agent framework focused on bounded runtime behavior, Markdown-native memory, and Telegram-first deployment.

## Overview

clawhive is a Rust-native multi-agent framework designed for a smaller operational footprint than broad "everything connector" platforms. It currently focuses on Telegram + Discord + CLI workflows, routes messages to configurable agents, and preserves persistent memory across conversations.

The memory system follows a "Markdown as source of truth" philosophy. Long-term knowledge lives in `MEMORY.md`, daily observations in `memory/YYYY-MM-DD.md` files, and raw conversation history in Session JSONL files. SQLite with sqlite-vec and FTS5 is used only as a search index layer, enabling hybrid vector + full-text retrieval.

Each agent has its own persona (system prompts), model policy (primary + fallback LLMs), memory policy, and optional tool policy config. Agents can spawn sub-agents with explicit depth and timeout bounds. A ReAct loop provides iterative reasoning with repeat guards.

## 🔐 Security First

clawhive implements a **two-layer security architecture** that provides defense-in-depth for AI agent tool execution:

### Hard Baseline (Always Enforced)

These security constraints are **non-negotiable** and apply to ALL tool executions, regardless of trust level:

| Protection | What It Blocks |
|------------|----------------|
| **SSRF Prevention** | Private networks (10.x, 172.16-31.x, 192.168.x), loopback, cloud metadata endpoints (169.254.169.254) |
| **Sensitive Path Protection** | Writes to `~/.ssh/`, `~/.gnupg/`, `~/.aws/`, `/etc/`, system directories |
| **Private Key Shield** | Reads of `~/.ssh/id_*`, `~/.gnupg/private-keys`, cloud credentials |
| **Dangerous Command Block** | `rm -rf /`, fork bombs, disk wipes, curl-pipe-to-shell patterns |
| **Resource Limits** | 30s timeout, 1MB output cap, 5 concurrent executions |

### Origin-Based Trust Model

Tools are classified by origin, determining their permission requirements:

| Origin | Trust Level | Permission Checks |
|--------|-------------|-------------------|
| **Builtin** | Trusted | Hard baseline only (no permission declarations needed) |
| **External** | Sandboxed | Must declare all permissions in SKILL.md frontmatter |

### Skill Permission Declaration

External skills must explicitly declare their required permissions in SKILL.md:

```yaml
---
name: weather-skill
description: Get weather forecasts
permissions:
  network:
    allow:
      - "api.openweathermap.org:443"
      - "api.weatherapi.com:443"
  fs:
    read:
      - "${WORKSPACE}/**"
    write:
      - "${WORKSPACE}/cache/**"
  exec:
    - curl
    - jq
  env:
    - WEATHER_API_KEY
---
```

**Any access outside declared permissions is denied at runtime.**

### Security Philosophy

1. **Deny by default** — External skills have no permissions unless explicitly declared
2. **Hard baseline is non-bypassable** — Even misconfigured permissions can't override it
3. **Honest documentation** — We only claim what's implemented, not roadmap intent
4. **Defense in depth** — Multiple layers prevent single-point failures

## Technical Differentiators (vs OpenClaw)

| Aspect | clawhive | OpenClaw |
|--------|----------|----------|
| **Runtime** | Pure Rust binary, embedded SQLite | Node.js runtime |
| **Security Model** | Two-layer policy (hard baseline + origin trust) | Tool allowlist |
| **Permission System** | Declarative SKILL.md permissions | Runtime policy |
| **Memory** | Markdown-native (MEMORY.md canonical) | Markdown + DB hybrid |
| **Integration Surface** | Focused (Telegram + Discord + CLI) | Broad connectors |
| **Dependency** | Single binary, no runtime deps | Node.js + npm |

### Key Architectural Choices

- **Rust workspace with embedded SQLite** (`rusqlite` + bundled): zero runtime dependencies in production
- **Markdown-first memory**: `MEMORY.md` and daily files are canonical; SQLite index is rebuildable
- **Permission-as-code**: Skills declare permissions in YAML frontmatter, enforced at runtime
- **Bounded execution**: Token-bucket rate limits, sub-agent recursion limits, timeouts

## Features

- Multi-agent orchestration with per-agent personas, model routing, and memory policy controls
- Three-layer memory system: Session JSONL (working memory) → Daily files (short-term) → MEMORY.md (long-term)
- Hybrid search: sqlite-vec vector similarity (70%) + FTS5 BM25 (30%) over memory chunks
- Hippocampus consolidation: periodic LLM-driven synthesis of daily observations into long-term memory
- Telegram channel adapter (multi-bot, multi-connector)
- ReAct reasoning loop with repeat guard
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
├── clawhive-cli/               # CLI binary (clap) — start, chat, validate, consolidate, agent/skill/session/task
├── clawhive-core/              # Orchestrator, session mgmt, config, persona, skill system, sub-agent, LLM router
├── clawhive-memory/            # Memory system — file store (MEMORY.md + daily), session JSONL, SQLite index, chunker, embedding
├── clawhive-gateway/           # Gateway with agent routing and per-user rate limiting
├── clawhive-bus/               # Topic-based in-process event bus (pub/sub)
├── clawhive-provider/          # LLM provider trait + Anthropic Claude adapter (streaming, retry)
├── clawhive-channels-telegram/ # Telegram adapter via teloxide
├── clawhive-schema/            # Shared DTOs (InboundMessage, OutboundMessage, BusMessage, SessionKey)
├── clawhive-runtime/           # Task executor abstraction
└── clawhive-tui/               # Real-time terminal dashboard (ratatui)

config/
├── main.yaml                   # App settings, channel configuration
├── agents.d/*.yaml             # Per-agent config (model policy, tools, memory, identity)
├── providers.d/*.yaml          # LLM provider settings (API keys, models)
└── routing.yaml                # Channel → agent routing bindings

prompts/<agent_id>/             # Per-agent persona prompts (system.md, style.md, safety.md)
skills/                         # Skill definitions (SKILL.md with frontmatter)
```

## Installation (End Users)

Download prebuilt binaries from [GitHub Releases](https://github.com/longzhi/clawhive/releases).

### macOS (Intel)

```bash
curl -LO https://github.com/longzhi/clawhive/releases/download/vX.Y.Z/clawhive-vX.Y.Z-x86_64-apple-darwin.tar.gz
tar -xzf clawhive-vX.Y.Z-x86_64-apple-darwin.tar.gz
chmod +x clawhive
sudo mv clawhive /usr/local/bin/
```

### macOS (Apple Silicon)

```bash
curl -LO https://github.com/longzhi/clawhive/releases/download/vX.Y.Z/clawhive-vX.Y.Z-aarch64-apple-darwin.tar.gz
tar -xzf clawhive-vX.Y.Z-aarch64-apple-darwin.tar.gz
chmod +x clawhive
sudo mv clawhive /usr/local/bin/
```

### Ubuntu (x86_64)

```bash
curl -LO https://github.com/longzhi/clawhive/releases/download/vX.Y.Z/clawhive-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz
tar -xzf clawhive-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz
chmod +x clawhive
sudo mv clawhive /usr/local/bin/
```

### Run

```bash
# Validate configuration
clawhive validate

# Chat mode (local REPL)
export ANTHROPIC_API_KEY=your-key
clawhive chat --agent clawhive-main

# Start Telegram bot
export TELEGRAM_BOT_TOKEN=your-token
export ANTHROPIC_API_KEY=your-key
clawhive start

# Start with TUI dashboard
clawhive start --tui
```

## Quick Start (Developers)

Prerequisites: Rust 1.75+

```bash
# Clone
git clone https://github.com/longzhi/clawhive.git
cd clawhive

# Build
cargo build --workspace

# Validate configuration
cargo run -- validate

# Chat mode (local REPL, no Telegram needed)
export ANTHROPIC_API_KEY=your-key
cargo run -- chat --agent clawhive-main

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

clawhive uses a three-layer memory architecture inspired by neuroscience:

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

This project is under active development. The memory architecture uses Markdown-native storage + hybrid retrieval.
