# Contributing to nanocrab

Thank you for your interest in contributing to nanocrab! This document provides guidelines and information for contributors.

## Table of Contents

- [Code of Conduct](#code-of-conduct)
- [Getting Started](#getting-started)
- [Development Setup](#development-setup)
- [Making Changes](#making-changes)
- [Commit Convention](#commit-convention)
- [Pull Request Process](#pull-request-process)
- [Testing](#testing)
- [Code Style](#code-style)
- [Documentation](#documentation)

## Code of Conduct

Be respectful, inclusive, and constructive. We're all here to build something useful together.

## Getting Started

1. Fork the repository
2. Clone your fork: `git clone https://github.com/YOUR_USERNAME/nanocrab.git`
3. Add upstream remote: `git remote add upstream https://github.com/longzhi/nanocrab.git`
4. Create a branch: `git checkout -b feature/your-feature-name`

## Development Setup

### Prerequisites

- Rust 1.75+ (install via [rustup](https://rustup.rs/))
- Git

### Build

```bash
# Build all crates
cargo build --workspace

# Run tests
cargo test --workspace

# Run clippy
cargo clippy --workspace --all-targets -- -D warnings

# Format code
cargo fmt --all
```

### Running Locally

```bash
# Validate configuration
cargo run -- validate

# Local chat mode (no Telegram needed)
export ANTHROPIC_API_KEY=your-key
cargo run -- chat --agent nanocrab-main

# Start with TUI
cargo run -- start --tui
```

## Making Changes

1. **Check existing issues** - See if someone is already working on it
2. **Create an issue first** - For significant changes, discuss before coding
3. **Keep changes focused** - One feature or fix per PR
4. **Write tests** - Add tests for new functionality
5. **Update documentation** - Keep docs in sync with code

## Commit Convention

We use [Conventional Commits](https://www.conventionalcommits.org/):

```
<type>(<scope>): <description>

[optional body]

[optional footer]
```

### Types

| Type | Description |
|------|-------------|
| `feat` | New feature |
| `fix` | Bug fix |
| `docs` | Documentation only |
| `style` | Formatting, no code change |
| `refactor` | Code change that neither fixes a bug nor adds a feature |
| `perf` | Performance improvement |
| `test` | Adding or updating tests |
| `chore` | Maintenance tasks |

### Examples

```
feat(core): add session concurrency control
fix(memory): prevent duplicate indexing of markdown files
docs(readme): add installation instructions for Linux ARM64
refactor(router): extract failover logic into separate module
test(hooks): add integration tests for hook registry
```

## Pull Request Process

1. **Update your branch** with the latest upstream changes
   ```bash
   git fetch upstream
   git rebase upstream/main
   ```

2. **Ensure all checks pass**
   ```bash
   cargo test --workspace
   cargo clippy --workspace --all-targets -- -D warnings
   cargo fmt --all --check
   ```

3. **Create the PR** with a clear title and description

4. **Fill out the PR template** completely

5. **Respond to review feedback** promptly

### PR Title Format

Use the same convention as commits:
```
feat(core): add hook system for agent lifecycle
```

## Testing

### Running Tests

```bash
# All tests
cargo test --workspace

# Specific crate
cargo test -p nanocrab-core

# Specific test
cargo test -p nanocrab-core session_lock

# With output
cargo test --workspace -- --nocapture
```

### Writing Tests

- Place unit tests in the same file as the code (`#[cfg(test)]` module)
- Place integration tests in `tests/` directory
- Use descriptive test names: `test_session_lock_prevents_concurrent_access`
- Test both success and error cases

## Code Style

### Rust Guidelines

- Follow [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/)
- Use `rustfmt` defaults (no custom config)
- Fix all `clippy` warnings
- Prefer explicit types over `impl Trait` in public APIs
- Document public items with `///` doc comments

### Naming

- Crates: `nanocrab-{name}` (lowercase, hyphenated)
- Modules: `snake_case`
- Types: `PascalCase`
- Functions/methods: `snake_case`
- Constants: `SCREAMING_SNAKE_CASE`

### Error Handling

- Use `anyhow::Result` for application code
- Use `thiserror` for library error types
- Provide context with `.context()` or `.with_context()`

## Documentation

- Update README.md for user-facing changes
- Add doc comments (`///`) for public APIs
- Include examples in doc comments when helpful
- Update CHANGELOG.md for notable changes

### Doc Comment Example

```rust
/// Acquires a lock for the given session.
///
/// This ensures only one request can modify a session at a time,
/// preventing race conditions in message ordering.
///
/// # Arguments
///
/// * `session_key` - Unique identifier for the session
///
/// # Returns
///
/// A guard that releases the lock when dropped.
///
/// # Example
///
/// ```rust
/// let guard = lock_manager.acquire("session-123").await;
/// // ... do work ...
/// // lock released when guard drops
/// ```
pub async fn acquire(&self, session_key: &str) -> SessionLockGuard {
    // ...
}
```

## Questions?

- Open a [Discussion](https://github.com/longzhi/nanocrab/discussions)
- Check existing [Issues](https://github.com/longzhi/nanocrab/issues)

Thank you for contributing! 🦀
