# Post-MVP Completion Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Complete all spec-defined features beyond the MVP milestone — skill system, consolidation scheduling, sub-agent lifecycle, CLI/TUI maturity, and gateway hardening.

**Architecture:** Extend existing crate modules. No new crates needed. Skill system goes in clawhive-core as a new module. Gateway rate limiting uses in-memory token bucket. TUI subscribes to real bus events via mpsc channel bridge.

**Tech Stack:** Rust 2021, tokio, ratatui, crossterm, serde_yaml (frontmatter parsing)

---

## Dependency Graph

```
Wave A (all parallel)           Wave B (depends on A)       Wave C
========================        =======================     ========
T1: Skill System ──────────────► T4: CLI Completion
T2: Consolidation Scheduler ───► T4: CLI Completion
T3: Sub-Agent cancel/merge       T5: TUI Real-time ────────► T7: E2E
T6: Gateway Rate Limiting
```

---

## Wave A: Independent Features (4 tasks, all parallel)

### Task T1: Skill System

**Crate:** `clawhive-core` (new module `skill.rs`)
**Estimated:** ~200 lines

**Files:**
- Create: `crates/clawhive-core/src/skill.rs`
- Modify: `crates/clawhive-core/src/lib.rs` (add `pub mod skill; pub use skill::*;`)
- Modify: `crates/clawhive-core/src/orchestrator.rs` (inject skill summary into system prompt)
- Create: `skills/example/SKILL.md` (sample skill for testing)

**Design:**

```rust
// crates/clawhive-core/src/skill.rs
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct SkillFrontmatter {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub requires: SkillRequirements,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SkillRequirements {
    #[serde(default)]
    pub bins: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub requires: SkillRequirements,
    pub content: String,
    pub path: PathBuf,
}

impl Skill {
    pub fn requirements_met(&self) -> bool {
        for bin in &self.requires.bins {
            if which::which(bin).is_err() {
                return false;
            }
        }
        for env_var in &self.requires.env {
            if std::env::var(env_var).is_err() {
                return false;
            }
        }
        true
    }
}

#[derive(Debug, Clone)]
pub struct SkillRegistry {
    skills: HashMap<String, Skill>,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self { skills: HashMap::new() }
    }

    pub fn load_from_dir(dir: &Path) -> Result<Self> {
        let mut registry = Self::new();
        if !dir.exists() {
            return Ok(registry);
        }
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let skill_dir = entry.path();
            if !skill_dir.is_dir() { continue; }
            let skill_md = skill_dir.join("SKILL.md");
            if !skill_md.exists() { continue; }
            match load_skill(&skill_md) {
                Ok(skill) => { registry.skills.insert(skill.name.clone(), skill); }
                Err(e) => { tracing::warn!("Failed to load skill from {}: {e}", skill_md.display()); }
            }
        }
        Ok(registry)
    }

    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name)
    }

    pub fn list(&self) -> Vec<&Skill> {
        self.skills.values().collect()
    }

    pub fn available(&self) -> Vec<&Skill> {
        self.skills.values().filter(|s| s.requirements_met()).collect()
    }

    /// Generate a compact summary for LLM prompt injection
    pub fn summary_prompt(&self) -> String {
        let available = self.available();
        if available.is_empty() {
            return String::new();
        }
        let mut lines = vec!["## Available Skills".to_string()];
        for skill in &available {
            lines.push(format!("- **{}**: {}", skill.name, skill.description));
        }
        lines.push("\nTo use a skill, ask to read the full SKILL.md for detailed instructions.".to_string());
        lines.join("\n")
    }
}

fn load_skill(path: &Path) -> Result<Skill> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let (frontmatter, content) = parse_frontmatter(&raw)?;
    Ok(Skill {
        name: frontmatter.name,
        description: frontmatter.description,
        requires: frontmatter.requires,
        content,
        path: path.to_path_buf(),
    })
}

fn parse_frontmatter(raw: &str) -> Result<(SkillFrontmatter, String)> {
    let trimmed = raw.trim_start();
    if !trimmed.starts_with("---") {
        anyhow::bail!("SKILL.md must start with YAML frontmatter (---)");
    }
    let after_first = &trimmed[3..];
    let end = after_first.find("---")
        .ok_or_else(|| anyhow::anyhow!("no closing --- for frontmatter"))?;
    let yaml_str = &after_first[..end];
    let content = after_first[end + 3..].trim().to_string();
    let fm: SkillFrontmatter = serde_yaml::from_str(yaml_str)
        .context("parsing skill frontmatter YAML")?;
    Ok((fm, content))
}
```

**Orchestrator integration** (in orchestrator.rs `handle_inbound`):
After persona system prompt assembly, append skill summary:
```rust
let skill_summary = self.skill_registry.summary_prompt();
let system_prompt = if skill_summary.is_empty() {
    system_prompt
} else {
    format!("{system_prompt}\n\n{skill_summary}")
};
```

Add `skill_registry: SkillRegistry` field to `Orchestrator`.

**Sample skill** (`skills/example/SKILL.md`):
```markdown
---
name: example
description: An example skill showing the SKILL.md format
requires:
  bins: []
  env: []
---

# Example Skill

This is a sample skill that demonstrates the SKILL.md format.
```

**Tests (5):**
1. `parse_frontmatter` extracts name/description/requires
2. `parse_frontmatter` rejects missing frontmatter
3. `SkillRegistry::load_from_dir` loads skills from directory
4. `SkillRegistry::summary_prompt` formats correctly
5. `Skill::requirements_met` checks bins and env vars

**Note:** Don't add `which` crate — use `std::process::Command` to check bin existence instead. Replace `which::which(bin).is_err()` with a simple PATH check:
```rust
fn bin_exists(name: &str) -> bool {
    std::process::Command::new("which")
        .arg(name)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
```

---

### Task T2: Consolidation Cron Scheduler

**Crate:** `clawhive-core` (extend `consolidation.rs`)
**Estimated:** ~60 lines

**Files:**
- Modify: `crates/clawhive-core/src/consolidation.rs` (add `ConsolidationScheduler`)
- Modify: `crates/clawhive-cli/src/main.rs` (start scheduler in `start_bot`, add `consolidate` subcommand)

**Design:**

```rust
// Add to consolidation.rs
pub struct ConsolidationScheduler {
    consolidator: Arc<Consolidator>,
    interval_hours: u64,
}

impl ConsolidationScheduler {
    pub fn new(consolidator: Arc<Consolidator>, interval_hours: u64) -> Self {
        Self { consolidator, interval_hours }
    }

    /// Spawn a background task that runs consolidation on interval
    pub fn start(self) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(
                tokio::time::Duration::from_secs(self.interval_hours * 3600)
            );
            interval.tick().await; // skip first immediate tick
            loop {
                interval.tick().await;
                tracing::info!("Running scheduled consolidation...");
                match self.consolidator.run_daily().await {
                    Ok(report) => {
                        tracing::info!(
                            "Consolidation complete: {} created, {} updated, {} processed, {} staled, {} purged",
                            report.concepts_created, report.concepts_updated,
                            report.episodes_processed, report.concepts_staled, report.episodes_purged
                        );
                    }
                    Err(e) => {
                        tracing::error!("Consolidation failed: {e}");
                    }
                }
            }
        })
    }

    /// Run once immediately (for CLI trigger)
    pub async fn run_once(&self) -> Result<ConsolidationReport> {
        self.consolidator.run_daily().await
    }
}
```

**CLI addition** — add `Consolidate` subcommand:
```rust
#[derive(Subcommand)]
enum Commands {
    // ... existing ...
    #[command(about = "Run memory consolidation manually")]
    Consolidate,
}
```

**In bootstrap**, create Consolidator and wire it. In `start_bot`, spawn the scheduler.

**Tests (2):**
1. `ConsolidationScheduler::new` constructs correctly
2. `run_once` delegates to `run_daily` (integration test with in-memory store)

---

### Task T3: Sub-Agent cancel + result_merge

**Crate:** `clawhive-core` (extend `subagent.rs`)
**Estimated:** ~100 lines

**Files:**
- Modify: `crates/clawhive-core/src/subagent.rs`

**Design:**

Add run tracking with `HashMap<Uuid, JoinHandle>` and `cancel` / `result_merge`:

```rust
use tokio::task::JoinHandle;
use tokio::sync::Mutex;

pub struct SubAgentRunner {
    router: Arc<LlmRouter>,
    agents: HashMap<String, FullAgentConfig>,
    personas: HashMap<String, Persona>,
    active_runs: Arc<Mutex<HashMap<Uuid, RunHandle>>>,
}

struct RunHandle {
    handle: JoinHandle<Result<SubAgentResult>>,
    parent_run_id: Uuid,
    trace_id: Uuid,
}

impl SubAgentRunner {
    // spawn now returns run_id immediately, stores JoinHandle
    pub async fn spawn(&self, req: SubAgentRequest) -> Result<Uuid> {
        // ... validate agent exists ...
        let run_id = Uuid::new_v4();
        let handle = tokio::spawn(async move { /* existing logic */ });
        self.active_runs.lock().await.insert(run_id, RunHandle { handle, parent_run_id: req.parent_run_id, trace_id: req.trace_id });
        Ok(run_id)
    }

    pub async fn cancel(&self, run_id: &Uuid) -> Result<bool> {
        if let Some(run) = self.active_runs.lock().await.remove(run_id) {
            run.handle.abort();
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub async fn wait_result(&self, run_id: &Uuid) -> Result<SubAgentResult> {
        let run = self.active_runs.lock().await.remove(run_id)
            .ok_or_else(|| anyhow::anyhow!("run not found: {run_id}"))?;
        match run.handle.await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(e)) => Ok(SubAgentResult { run_id: *run_id, output: e.to_string(), success: false }),
            Err(_) => Ok(SubAgentResult { run_id: *run_id, output: "task cancelled".into(), success: false }),
        }
    }

    pub async fn result_merge(&self, results: &[SubAgentResult]) -> String {
        // Simple merge: concatenate successful outputs
        results.iter()
            .filter(|r| r.success)
            .map(|r| r.output.as_str())
            .collect::<Vec<_>>()
            .join("\n\n---\n\n")
    }

    pub async fn active_count(&self) -> usize {
        self.active_runs.lock().await.len()
    }
}
```

**Tests (4):**
1. `spawn` returns run_id, `wait_result` gets result
2. `cancel` aborts running task
3. `result_merge` concatenates successful outputs
4. `active_count` tracks running tasks

---

### Task T6: Gateway Rate Limiting

**Crate:** `clawhive-gateway` (extend `lib.rs`)
**Estimated:** ~120 lines

**Files:**
- Modify: `crates/clawhive-gateway/src/lib.rs`
- Modify: `crates/clawhive-core/src/config.rs` (add rate limit config structs)

**Design — Token Bucket rate limiter:**

```rust
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;
use chrono::{DateTime, Utc};

#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    pub requests_per_minute: u32,
    pub burst: u32,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self { requests_per_minute: 30, burst: 10 }
    }
}

struct TokenBucket {
    tokens: f64,
    max_tokens: f64,
    refill_rate: f64, // tokens per second
    last_refill: DateTime<Utc>,
}

impl TokenBucket {
    fn new(config: &RateLimitConfig) -> Self {
        Self {
            tokens: config.burst as f64,
            max_tokens: config.burst as f64,
            refill_rate: config.requests_per_minute as f64 / 60.0,
            last_refill: Utc::now(),
        }
    }

    fn try_consume(&mut self) -> bool {
        let now = Utc::now();
        let elapsed = (now - self.last_refill).num_milliseconds() as f64 / 1000.0;
        self.tokens = (self.tokens + elapsed * self.refill_rate).min(self.max_tokens);
        self.last_refill = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

pub struct RateLimiter {
    buckets: Arc<Mutex<HashMap<String, TokenBucket>>>,
    config: RateLimitConfig,
}

impl RateLimiter {
    pub fn new(config: RateLimitConfig) -> Self {
        Self {
            buckets: Arc::new(Mutex::new(HashMap::new())),
            config,
        }
    }

    /// Check rate limit by user scope key. Returns true if allowed.
    pub async fn check(&self, key: &str) -> bool {
        let mut buckets = self.buckets.lock().await;
        let bucket = buckets.entry(key.to_string())
            .or_insert_with(|| TokenBucket::new(&self.config));
        bucket.try_consume()
    }
}
```

Integrate in `Gateway::handle_inbound`:
```rust
if !self.rate_limiter.check(&inbound.user_scope).await {
    return Err(anyhow!("rate limited"));
}
```

Add `rate_limiter: RateLimiter` field to Gateway, with optional config in main.yaml:
```yaml
# config/main.yaml addition
rate_limit:
  requests_per_minute: 30
  burst: 10
```

**Tests (4):**
1. `RateLimiter::check` allows within limit
2. `RateLimiter::check` blocks after burst exceeded
3. `RateLimiter::check` refills over time
4. Gateway returns rate limit error when exceeded

---

## Wave B: Integration Features (2 tasks)

### Task T4: CLI Completion

**Depends on:** T1 (skill system), T2 (consolidation scheduler)
**Crate:** `clawhive-cli`
**Estimated:** ~150 lines

**Files:**
- Modify: `crates/clawhive-cli/src/main.rs`

**New subcommands:**

```rust
#[derive(Subcommand)]
enum Commands {
    Start,
    Chat { #[arg(long, default_value = "clawhive-main")] agent: String },
    Validate,
    Consolidate,
    #[command(subcommand, about = "Agent management")]
    Agent(AgentCommands),
    #[command(subcommand, about = "Skill management")]
    Skill(SkillCommands),
}

#[derive(Subcommand)]
enum AgentCommands {
    #[command(about = "List all agents")]
    List,
    #[command(about = "Show agent details")]
    Show { agent_id: String },
}

#[derive(Subcommand)]
enum SkillCommands {
    #[command(about = "List available skills")]
    List,
    #[command(about = "Show skill details")]
    Show { skill_name: String },
}
```

**Implementation:**
- `agent list`: Load config, print table of agents with status
- `agent show <id>`: Print full agent config details
- `skill list`: Load SkillRegistry, print available/unavailable skills
- `skill show <name>`: Print full SKILL.md content
- `consolidate`: Run consolidation once via bootstrap + Consolidator

---

### Task T5: TUI Real-Time

**Depends on:** EventBus (already exists)
**Crate:** `clawhive-tui`
**Estimated:** ~350 lines

**Files:**
- Rewrite: `crates/clawhive-tui/src/main.rs`
- Modify: `crates/clawhive-tui/Cargo.toml` (add clawhive-core, clawhive-memory, clawhive-provider, clawhive-gateway deps)

**Design — 4-panel layout:**

```
┌──────────────────────┬──────────────────────┐
│  Bus Events          │  Active Sessions     │
│  (real-time stream)  │  (session list)      │
│                      │                      │
├──────────────────────┼──────────────────────┤
│  Agent Runs          │  Logs / Trace        │
│  (sub-agent status)  │  (filtered by trace) │
│                      │                      │
└──────────────────────┴──────────────────────┘
  [q] quit  [Tab] focus  [↑↓] scroll  clawhive TUI v0.2.0
```

- Uses tokio runtime (switch main to async)
- Subscribes to ALL bus topics via EventBus
- Receives events through mpsc channel
- Tab key switches focus between panels
- Up/Down scrolls focused panel
- Bus events panel shows last 200 events with timestamp
- Sessions panel reads from MemoryStore periodically
- Agent Runs panel is placeholder (shows sub-agent count)
- Logs panel shows trace_id filtered events

---

## Wave C: Verification

### Task T7: End-to-End Verification

**Depends on:** All previous tasks
**Estimated:** ~30 min manual testing + integration test

**Steps:**
1. `cargo build` — full workspace
2. `cargo test` — all tests pass
3. `clawhive validate --config-root .` — config validation
4. `clawhive agent list --config-root .` — lists agents
5. `clawhive skill list --config-root .` — lists skills
6. `clawhive chat --config-root .` — REPL responds (uses stub provider if no API key)
7. `clawhive consolidate --config-root .` — runs consolidation
8. Verify no warnings, clean diagnostics

---

## Execution Plan

| Wave | Tasks | Parallelism |
|------|-------|-------------|
| **A** | T1, T2, T3, T6 | 4-way parallel |
| **B** | T4, T5 | 2-way parallel |
| **C** | T7 | Sequential verification |

**Total: 7 tasks, ~1000 lines new Rust code, ~20 new tests**
