# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Hook system for agent lifecycle (`hooks.rs`)
  - `before_model_resolve`: Override model selection
  - `before_prompt_build`: Inject context into prompts
  - `before_tool_call` / `after_tool_call`: Intercept tool execution
  - `before_compaction` / `after_compaction`: Observe compaction
  - `message_received` / `message_sending` / `message_sent`: Message lifecycle
- Block streaming for progressive output (`streaming.rs`)
  - Configurable chunking with min/max chars
  - Code fence tracking
  - Coalescer for merging chunks
- Session concurrency control (`session_lock.rs`)
  - Per-session mutex to prevent race conditions
  - Global concurrency limit (default: 10)
- Context management and auto-compaction (`context.rs`)
  - Token estimation
  - Automatic compaction when approaching context limits
  - Tool result pruning
- Parallel tool execution using `futures::future::join_all`
- `/new` slash command for session reset
- `install.sh` for easy installation

### Changed
- Memory context now injected into system prompt instead of fake dialogue
- Removed Episode recording (JSONL sessions are sufficient)

### Fixed
- NO_REPLY / HEARTBEAT_OK filtering from output

## [0.1.0] - 2025-02-26

### Added
- Initial public release
- Multi-agent orchestration with per-agent personas
- Three-layer memory system: Session JSONL → Daily files → MEMORY.md
- Hybrid search: sqlite-vec vector similarity + FTS5 BM25
- Hippocampus consolidation for memory synthesis
- Telegram and Discord channel adapters
- Weak ReAct reasoning loop with repeat guard
- Sub-agent spawning with depth limits and timeout
- Skill system with SKILL.md frontmatter
- Token-bucket rate limiting per user
- LLM provider abstraction with retry + exponential backoff
- Real-time TUI dashboard
- YAML-driven configuration

### Security
- Tool allowlist model in agent config
- Gateway rate limiting
- Bounded sub-agent execution
- Note: WASM sandbox is planned but not yet implemented

[Unreleased]: https://github.com/longzhi/nanocrab/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/longzhi/nanocrab/releases/tag/v0.1.0
