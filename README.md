# nanocrab

A Rust-based multi-agent AI bot framework with dual-layer memory, event-driven architecture, and WASM runtime support.

## Features

- Multi-agent orchestration with per-agent personas, model policies, and routing
- Dual-layer memory system (hippocampus episodes + cortex concepts) with SQLite
- Automated memory consolidation (daily cron-driven concept extraction)
- Event-driven architecture with topic-based in-process message bus
- Telegram channel adapter (multi-bot, multi-account support)
- Token-bucket rate limiting per user
- Weak ReAct loop for iterative reasoning
- Sub-agent spawning with timeout, depth limits, and minimum-privilege tools
- Skill system (SKILL.md frontmatter + prompt injection)
- LLM provider abstraction with retry + exponential backoff
- Real-time TUI dashboard (sessions, events, agent runs, logs)
- CLI for management (start, chat, validate, consolidate, agent/skill/session/task)

## Architecture

```
Telegram -> Gateway -> EventBus -> Orchestrator -> LLM Provider
                          |              |
                       Memory      Sub-Agents
                     (SQLite)      (async spawn)
```

Modules:

- **Gateway**: Ingress, auth, rate limiting, protocol conversion, routing
- **Core/Orchestrator**: Session routing, context assembly, WeakReAct loop, memory control, persona
- **Bus**: Topic-based in-process pub/sub (EventBus)
- **Memory**: SQLite-backed episodes + concepts + links with consolidation
- **Runtime**: Task execution layer (native + WASM skeleton)
- **Provider**: LLM abstraction (Anthropic adapter with streaming, retry, error classification)
- **Schema**: Shared DTOs (InboundMessage, OutboundMessage, BusMessage, SessionKey)

## Project Structure

Crates:

```
crates/
  nanocrab-bus/          # Topic-based event bus
  nanocrab-channels-telegram/  # Telegram adapter via teloxide
  nanocrab-cli/          # CLI binary (clap)
  nanocrab-core/         # Orchestrator, session, config, persona, skill, sub-agent, router
  nanocrab-gateway/      # Gateway with routing and rate limiting
  nanocrab-memory/       # SQLite memory store (episodes, concepts, links)
  nanocrab-provider/     # LLM provider trait + Anthropic implementation
  nanocrab-runtime/      # Task executor (native + WASM skeleton)
  nanocrab-schema/       # Shared message types and DTOs
  nanocrab-tui/          # Real-time TUI dashboard (ratatui)
config/                  # YAML configuration files
prompts/                 # Per-agent persona prompts (system.md, style.md, safety.md)
skills/                  # Skill definitions (SKILL.md with frontmatter)
```

## Quick Start

Prerequisites: Rust 1.75+, SQLite

```bash
# Clone
git clone https://github.com/longzhi/nanocrab.git
cd nanocrab

# Build
cargo build --workspace

# Validate configuration
cargo run -- validate

# Start with TUI
export ANTHROPIC_API_KEY=your-key
cargo run -- start --tui

# Chat mode (REPL)
cargo run -- chat --agent nanocrab-main
```

## Configuration

Layout:

- `config/main.yaml` - Global settings, channel configs
- `config/agents.d/*.yaml` - Per-agent configuration (model policy, tools, memory)
- `config/routing.yaml` - Channel/connector to agent bindings
- `config/providers.d/*.yaml` - LLM provider settings
- `prompts/<agent_id>/` - Persona files (system.md, style.md, safety.md)

## CLI Commands

| Command | Description |
|---------|-------------|
| `start` | Start the bot (add `--tui` for dashboard) |
| `chat` | Interactive REPL with an agent |
| `validate` | Validate configuration |
| `consolidate` | Run memory consolidation |
| `agent list` | List configured agents |
| `agent show <id>` | Show agent details |
| `agent enable/disable <id>` | Enable or disable an agent |
| `skill list` | List available skills |
| `skill show <name>` | Show skill details |
| `session reset <key>` | Reset a session |
| `task trigger <id>` | Manually trigger a task |

## Development

```bash
# Run tests
cargo test --workspace

# Clippy
cargo clippy --workspace --all-targets -- -D warnings

# Format
cargo fmt --all
```

## License

MIT
