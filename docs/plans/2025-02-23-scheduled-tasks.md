# Scheduled Tasks Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Implement a scheduled task system that triggers agent actions via the existing Gateway/Orchestrator flow. Tasks can be created three ways: YAML config files, CLI/Web UI, or by the AI agent itself during conversation (via a `schedule` tool).

**Architecture:**
- New `nanocrab-scheduler` crate: `ScheduleConfig`, `ScheduleManager`, schedule computation, state persistence.
- `ScheduleManager` loads configs from `config/schedules.d/`, manages tokio timers, publishes `ScheduledTaskTriggered` events to `EventBus`.
- `Gateway` subscribes to the event and injects `InboundMessage(channel_type: "scheduler")` into the standard flow — **Orchestrator needs zero modification**.
- A `schedule` tool is exposed to agents, allowing the AI to autonomously create/manage scheduled tasks during conversation (e.g. "remind me in 20 minutes"). Agent-created schedules take effect immediately (hot-add to running ScheduleManager) and persist to YAML for restart survival.

**Config example** (`config/schedules.d/daily-report.yaml`):
```yaml
schedule_id: daily-report
enabled: true
name: "Daily Morning Report"
schedule:
  kind: cron
  expr: "0 9 * * *"
  tz: "Asia/Shanghai"
agent_id: nanocrab-main
session_mode: isolated
task: "Summarize yesterday's key events and today's calendar."
timeout_seconds: 300
delete_after_run: false
delivery:
  mode: announce
  channel: telegram
  connector_id: main
```

**Tech Stack:** Rust (tokio, cron, serde, chrono), Next.js 16, React 19, shadcn/ui, TanStack Query 5.

---

### Task 1: Create `nanocrab-scheduler` crate and define schemas

**Files:**
- Create: `crates/nanocrab-scheduler/Cargo.toml`
- Create: `crates/nanocrab-scheduler/src/lib.rs`
- Create: `crates/nanocrab-scheduler/src/config.rs`
- Modify: `Cargo.toml` (workspace members)

**Step 1: Create the crate structure**

```toml
[package]
name = "nanocrab-scheduler"
version = "0.1.0"
edition = "2021"

[dependencies]
nanocrab-schema = { path = "../nanocrab-schema" }
serde = { version = "1.0", features = ["derive"] }
serde_yaml = "0.9"
chrono = { version = "0.4", features = ["serde"] }
cron = "0.12"
uuid = { version = "1.0", features = ["v4"] }
```

**Step 2: Define `ScheduleConfig`**

```rust
// crates/nanocrab-scheduler/src/config.rs

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ScheduleConfig {
    pub schedule_id: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    pub schedule: ScheduleType,
    pub agent_id: String,
    #[serde(default)]
    pub session_mode: SessionMode,     // "main" or "isolated"
    pub task: String,                  // The prompt/message to send to the agent
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,          // Default: 300
    #[serde(default)]
    pub delete_after_run: bool,        // One-time tasks: delete on success
    #[serde(default)]
    pub delivery: DeliveryConfig,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "kind")]
pub enum ScheduleType {
    #[serde(rename = "cron")]
    Cron {
        expr: String,                   // Standard cron: "0 9 * * *"
        #[serde(default = "default_tz")]
        tz: String,                     // Default: "UTC"
    },
    #[serde(rename = "at")]
    At {
        at: String,                     // ISO 8601 or relative: "2026-03-01T09:00:00Z", "20m", "2h"
    },
    #[serde(rename = "every")]
    Every {
        interval_ms: u64,               // Milliseconds between runs
        #[serde(default)]
        anchor_ms: Option<u64>,         // Optional anchor point for alignment
    },
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub enum SessionMode {
    #[default]
    #[serde(rename = "isolated")]
    Isolated,                           // Fresh session each run (clean, repeatable)
    #[serde(rename = "main")]
    Main,                               // Shared main session (memory-aware reminders)
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct DeliveryConfig {
    #[serde(default)]
    pub mode: DeliveryMode,             // "announce" | "none"
    pub channel: Option<String>,        // "telegram", "discord"
    pub connector_id: Option<String>,   // Which connector to deliver to
}

impl Default for DeliveryConfig {
    fn default() -> Self {
        Self { mode: DeliveryMode::None, channel: None, connector_id: None }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
pub enum DeliveryMode {
    #[default]
    #[serde(rename = "none")]
    None,
    #[serde(rename = "announce")]
    Announce,                           // Send result to channel
}
```

**Step 3: Define `ScheduleState` (runtime state, separate from config)**

```rust
// crates/nanocrab-scheduler/src/state.rs

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ScheduleState {
    pub schedule_id: String,
    pub next_run_at_ms: Option<i64>,
    pub running_at_ms: Option<i64>,
    pub last_run_at_ms: Option<i64>,
    pub last_run_status: Option<RunStatus>,   // "ok" | "error" | "skipped"
    pub last_error: Option<String>,
    pub last_duration_ms: Option<u64>,
    pub consecutive_errors: u32,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum RunStatus {
    #[serde(rename = "ok")]
    Ok,
    #[serde(rename = "error")]
    Error,
    #[serde(rename = "skipped")]
    Skipped,
}

/// Single run history entry, appended to JSONL
#[derive(Debug, Serialize, Deserialize)]
pub struct RunRecord {
    pub schedule_id: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub status: RunStatus,
    pub error: Option<String>,
    pub duration_ms: u64,
}
```

**Step 4: Commit**

```bash
git add crates/nanocrab-scheduler
git commit -m "feat(scheduler): initialize scheduler crate with config and state schemas"
```

---

### Task 2: Implement Schedule Computation Logic

**Files:**
- Create: `crates/nanocrab-scheduler/src/logic.rs`
- Test: `crates/nanocrab-scheduler/src/logic.rs`

**Step 1: Implement `compute_next_run_at_ms()`**

```rust
// crates/nanocrab-scheduler/src/compute.rs

use cron::Schedule as CronSchedule;
use chrono::{Utc, TimeZone};
use chrono_tz::Tz;

/// Compute the next run time in epoch milliseconds.
/// Returns None if no future run exists (e.g., one-time "at" already passed).
pub fn compute_next_run_at_ms(
    schedule: &ScheduleType,
    now_ms: i64,
) -> Result<Option<i64>> {
    match schedule {
        ScheduleType::Cron { expr, tz } => {
            let tz: Tz = tz.parse().map_err(|_| SchedulerError::InvalidTimezone(tz.clone()))?;
            let cron = CronSchedule::from_str(expr)?;
            let now_dt = tz.timestamp_millis_opt(now_ms).single()
                .ok_or(SchedulerError::InvalidTimestamp)?;
            let next = cron.after(&now_dt).next();
            Ok(next.map(|dt| dt.with_timezone(&Utc).timestamp_millis()))
        }
        ScheduleType::At { at } => {
            let at_ms = parse_absolute_or_relative_ms(at, now_ms)?;
            Ok(if at_ms > now_ms { Some(at_ms) } else { None })
        }
        ScheduleType::Every { interval_ms, anchor_ms } => {
            let interval = *interval_ms as i64;
            let anchor = anchor_ms.map(|a| a as i64).unwrap_or(now_ms);
            if now_ms < anchor {
                return Ok(Some(anchor));
            }
            let elapsed = now_ms - anchor;
            let steps = (elapsed + interval - 1) / interval;
            Ok(Some(anchor + steps * interval))
        }
    }
}

/// Parse "20m", "2h", "1d" relative to now_ms, or ISO 8601 absolute time.
fn parse_absolute_or_relative_ms(input: &str, now_ms: i64) -> Result<i64> {
    // Try relative first: "20m", "2h", "1d", "30s"
    if let Some(ms) = try_parse_relative_ms(input) {
        return Ok(now_ms + ms);
    }
    // Fall back to ISO 8601
    let dt = DateTime::parse_from_rfc3339(input)
        .or_else(|_| DateTime::parse_from_str(input, "%Y-%m-%dT%H:%M:%S%z"))?;
    Ok(dt.with_timezone(&Utc).timestamp_millis())
}

fn try_parse_relative_ms(input: &str) -> Option<i64> {
    let input = input.trim();
    let (num_str, unit) = input.split_at(input.len() - 1);
    let num: i64 = num_str.parse().ok()?;
    match unit {
        "s" => Some(num * 1_000),
        "m" => Some(num * 60_000),
        "h" => Some(num * 3_600_000),
        "d" => Some(num * 86_400_000),
        _ => None,
    }
}
```

**Step 2: Add `chrono-tz` dependency to Cargo.toml**

```toml
chrono-tz = "0.9"
```

**Step 3: Write tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cron_next_run() {
        // "every minute" should return next minute boundary
        let schedule = ScheduleType::Cron {
            expr: "* * * * *".into(),
            tz: "UTC".into(),
        };
        let now_ms = Utc::now().timestamp_millis();
        let next = compute_next_run_at_ms(&schedule, now_ms).unwrap().unwrap();
        assert!(next > now_ms);
        assert!(next - now_ms <= 60_000);
    }

    #[test]
    fn test_at_relative() {
        let schedule = ScheduleType::At { at: "20m".into() };
        let now_ms = 1_000_000;
        let next = compute_next_run_at_ms(&schedule, now_ms).unwrap().unwrap();
        assert_eq!(next, 1_000_000 + 20 * 60_000);
    }

    #[test]
    fn test_at_past_returns_none() {
        let schedule = ScheduleType::At { at: "2020-01-01T00:00:00Z".into() };
        let now_ms = Utc::now().timestamp_millis();
        assert!(compute_next_run_at_ms(&schedule, now_ms).unwrap().is_none());
    }

    #[test]
    fn test_every_with_anchor() {
        let schedule = ScheduleType::Every { interval_ms: 60_000, anchor_ms: Some(0) };
        let next = compute_next_run_at_ms(&schedule, 90_000).unwrap().unwrap();
        assert_eq!(next, 120_000);
    }
}
```

**Step 4: Commit**

```bash
git add crates/nanocrab-scheduler/src/compute.rs
git commit -m "feat(scheduler): implement schedule computation with cron, at, and every"
```

---

### Task 3: Build `ScheduleManager` and Timer Loop

**Files:**
- Create: `crates/nanocrab-scheduler/src/manager.rs`
- Modify: `crates/nanocrab-core/src/lib.rs` (to initialize manager)

**Step 1: Define `ScheduleEntry` (config + runtime state combined)**

```rust
// crates/nanocrab-scheduler/src/manager.rs

/// A schedule entry in the manager — config + live state.
pub struct ScheduleEntry {
    pub config: ScheduleConfig,
    pub state: ScheduleState,
}
```

**Step 2: Implement `ScheduleManager` with smart timer**

Instead of polling every second, compute the next wake time across all schedules and sleep until then (like OpenClaw's `armTimer`). Re-arm after each trigger or state change.

```rust
pub struct ScheduleManager {
    entries: Arc<RwLock<HashMap<String, ScheduleEntry>>>,
    bus: Arc<EventBus>,
    state_store: StateStore,       // persists to data/schedules/state.json
    history_store: HistoryStore,   // appends to data/schedules/runs/<id>.jsonl
}

const MAX_SLEEP_MS: u64 = 60_000; // Cap sleep at 60s to catch config changes

impl ScheduleManager {
    /// Load configs from schedules.d/ and restore persisted state.
    pub fn new(
        config_dir: &Path,
        data_dir: &Path,
        bus: Arc<EventBus>,
    ) -> Result<Self> {
        let configs: Vec<ScheduleConfig> = read_yaml_dir(config_dir)?;
        let persisted_states = StateStore::load(data_dir)?;

        let mut entries = HashMap::new();
        for config in configs {
            let state = persisted_states.get(&config.schedule_id)
                .cloned()
                .unwrap_or_else(|| ScheduleState::new(&config.schedule_id));
            entries.insert(config.schedule_id.clone(), ScheduleEntry { config, state });
        }

        Ok(Self {
            entries: Arc::new(RwLock::new(entries)),
            bus,
            state_store: StateStore::new(data_dir),
            history_store: HistoryStore::new(data_dir),
        })
    }

    /// Main loop — sleep until next due task, trigger, repeat.
    pub async fn run(&self) {
        loop {
            let sleep_ms = self.compute_sleep_ms().await;
            tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
            self.check_and_trigger().await;
        }
    }

    /// Find the soonest next_run_at across all enabled entries.
    async fn compute_sleep_ms(&self) -> u64 {
        let entries = self.entries.read().await;
        let now_ms = Utc::now().timestamp_millis();
        let mut soonest: Option<i64> = None;

        for entry in entries.values() {
            if !entry.config.enabled { continue; }
            if let Some(next) = entry.state.next_run_at_ms {
                soonest = Some(soonest.map_or(next, |s: i64| s.min(next)));
            }
        }

        match soonest {
            Some(next) => {
                let delay = (next - now_ms).max(0) as u64;
                delay.min(MAX_SLEEP_MS)
            }
            None => MAX_SLEEP_MS,
        }
    }

    /// Check all entries, trigger any that are due.
    async fn check_and_trigger(&self) {
        let now_ms = Utc::now().timestamp_millis();
        let mut entries = self.entries.write().await;

        for entry in entries.values_mut() {
            if !entry.config.enabled { continue; }
            if entry.state.running_at_ms.is_some() { continue; } // Already running

            let due = entry.state.next_run_at_ms
                .map(|next| next <= now_ms)
                .unwrap_or(false);

            if due {
                entry.state.running_at_ms = Some(now_ms);
                self.bus.publish(BusMessage::ScheduledTaskTriggered {
                    schedule_id: entry.config.schedule_id.clone(),
                    agent_id: entry.config.agent_id.clone(),
                    task: entry.config.task.clone(),
                    session_mode: entry.config.session_mode.clone(),
                    delivery: entry.config.delivery.clone(),
                    triggered_at: Utc::now(),
                }).await;
            }
        }

        self.state_store.persist(&entries).await.ok();
    }

    /// Hot-add a schedule at runtime (for agent tool use).
    /// Takes effect immediately without restart.
    pub async fn add_schedule(&self, config: ScheduleConfig) -> Result<()> {
        let now_ms = Utc::now().timestamp_millis();
        let next = compute_next_run_at_ms(&config.schedule, now_ms)?;
        let mut state = ScheduleState::new(&config.schedule_id);
        state.next_run_at_ms = next;

        // Persist to YAML
        let yaml = serde_yaml::to_string(&config)?;
        let path = self.config_dir.join(format!("{}.yaml", &config.schedule_id));
        tokio::fs::write(&path, yaml).await?;

        // Add to running manager
        let mut entries = self.entries.write().await;
        entries.insert(config.schedule_id.clone(), ScheduleEntry { config, state });

        Ok(())
    }

    /// Remove a schedule at runtime.
    pub async fn remove_schedule(&self, schedule_id: &str) -> Result<()> {
        let mut entries = self.entries.write().await;
        entries.remove(schedule_id);
        // Remove YAML file
        let path = self.config_dir.join(format!("{}.yaml", schedule_id));
        if path.exists() {
            tokio::fs::remove_file(&path).await?;
        }
        Ok(())
    }
}
```

**Step 3: Wire into startup** (`crates/nanocrab-cli/src/main.rs`)

Reference the existing `ConsolidationScheduler::start()` pattern:

```rust
let schedule_manager = ScheduleManager::new(
    &config_root.join("schedules.d"),
    &data_dir.join("schedules"),
    bus.clone(),
)?;
let _schedule_handle = tokio::spawn(async move {
    schedule_manager.run().await;
});
```

**Step 4: Commit**

```bash
git add crates/nanocrab-scheduler/src/manager.rs crates/nanocrab-cli/src/main.rs
git commit -m "feat(scheduler): implement ScheduleManager with smart timer and hot-add"
```

---

### Task 4: EventBus Integration and Gateway Handler

**Files:**
- Modify: `crates/nanocrab-bus/src/lib.rs`
- Modify: `crates/nanocrab-gateway/src/lib.rs`

**Step 1: Add topic and message variant to `nanocrab-bus`**

```rust
// crates/nanocrab-bus/src/lib.rs

pub enum Topic {
    // ... existing topics
    ScheduledTaskTriggered,
    ScheduledTaskCompleted,
}

pub enum BusMessage {
    // ... existing variants
    ScheduledTaskTriggered {
        schedule_id: String,
        agent_id: String,
        task: String,
        session_mode: SessionMode,
        delivery: DeliveryConfig,
        triggered_at: DateTime<Utc>,
    },
    ScheduledTaskCompleted {
        schedule_id: String,
        status: RunStatus,
        error: Option<String>,
        started_at: DateTime<Utc>,
        ended_at: DateTime<Utc>,
        response: Option<String>,  // Agent's response (for delivery)
    },
}
```

**Step 2: Subscribe in Gateway and construct `InboundMessage`**

```rust
// crates/nanocrab-gateway/src/lib.rs

/// Called during Gateway startup — subscribe to schedule events.
async fn listen_for_scheduled_tasks(gateway: Arc<Gateway>, bus: Arc<EventBus>) {
    let mut rx = bus.subscribe(Topic::ScheduledTaskTriggered).await;
    while let Some(msg) = rx.recv().await {
        if let BusMessage::ScheduledTaskTriggered {
            schedule_id, agent_id, task, session_mode, delivery, triggered_at,
        } = msg {
            // Construct conversation_scope based on session_mode:
            // - Isolated: unique per run → "schedule:{schedule_id}:{uuid}"
            // - Main:     shared per schedule → "schedule:{schedule_id}"
            let conversation_scope = match session_mode {
                SessionMode::Isolated => format!("schedule:{}:{}", schedule_id, Uuid::new_v4()),
                SessionMode::Main => format!("schedule:{}", schedule_id),
            };

            let inbound = InboundMessage {
                trace_id: Uuid::new_v4(),
                channel_type: "scheduler".into(),
                connector_id: schedule_id.clone(),
                conversation_scope,
                user_scope: "user:scheduler".into(),
                text: task,
                at: triggered_at,
            };

            match gateway.handle_inbound(inbound).await {
                Ok(outbound) => {
                    // Deliver result if configured
                    if matches!(delivery.mode, DeliveryMode::Announce) {
                        gateway.deliver_to_channel(&delivery, &outbound.text).await.ok();
                    }
                    bus.publish(BusMessage::ScheduledTaskCompleted {
                        schedule_id,
                        status: RunStatus::Ok,
                        error: None,
                        started_at: triggered_at,
                        ended_at: Utc::now(),
                        response: Some(outbound.text),
                    }).await;
                }
                Err(e) => {
                    bus.publish(BusMessage::ScheduledTaskCompleted {
                        schedule_id,
                        status: RunStatus::Error,
                        error: Some(e.to_string()),
                        started_at: triggered_at,
                        ended_at: Utc::now(),
                        response: None,
                    }).await;
                }
            }
        }
    }
}
```

**Step 3: ScheduleManager subscribes to `ScheduledTaskCompleted` to update state**

The manager listens for completion events to record results, update `consecutive_errors`, apply backoff, compute next run, and handle `delete_after_run`.

**Step 4: Commit**

```bash
git add crates/nanocrab-bus/src/lib.rs crates/nanocrab-gateway/src/lib.rs crates/nanocrab-scheduler/src/manager.rs
git commit -m "feat(scheduler): EventBus integration with Gateway trigger and completion loop"
```

---

### Task 5: State Persistence and Run History

**Files:**
- Create: `crates/nanocrab-scheduler/src/state.rs`
- Modify: `crates/nanocrab-scheduler/src/manager.rs`

**Step 1: Implement `StateStore` for `data/schedules/state.json`**

Persists all `ScheduleState` entries as a JSON object keyed by `schedule_id`. Loaded at startup, written after every state change.

```rust
// crates/nanocrab-scheduler/src/persistence.rs

pub struct StateStore {
    path: PathBuf,  // data/schedules/state.json
}

impl StateStore {
    pub fn load(&self) -> Result<HashMap<String, ScheduleState>> {
        if !self.path.exists() {
            return Ok(HashMap::new());
        }
        let content = std::fs::read_to_string(&self.path)?;
        Ok(serde_json::from_str(&content)?)
    }

    pub async fn persist(&self, entries: &HashMap<String, ScheduleEntry>) -> Result<()> {
        let states: HashMap<&str, &ScheduleState> = entries.iter()
            .map(|(k, v)| (k.as_str(), &v.state))
            .collect();
        let json = serde_json::to_string_pretty(&states)?;
        tokio::fs::write(&self.path, json).await?;
        Ok(())
    }
}
```

**Step 2: Implement `HistoryStore` for JSONL run logs**

Each schedule gets its own JSONL file: `data/schedules/runs/<schedule_id>.jsonl`. One `RunRecord` per line, append-only.

```rust
pub struct HistoryStore {
    dir: PathBuf,  // data/schedules/runs/
}

impl HistoryStore {
    pub async fn append(&self, record: &RunRecord) -> Result<()> {
        let path = self.dir.join(format!("{}.jsonl", record.schedule_id));
        let mut line = serde_json::to_string(record)?;
        line.push('\n');

        let mut file = OpenOptions::new()
            .create(true).append(true).open(&path).await?;
        file.write_all(line.as_bytes()).await?;
        Ok(())
    }

    pub async fn recent(&self, schedule_id: &str, limit: usize) -> Result<Vec<RunRecord>> {
        let path = self.dir.join(format!("{}.jsonl", schedule_id));
        if !path.exists() {
            return Ok(vec![]);
        }
        // Read last N lines (for display in CLI/Web UI)
        let content = tokio::fs::read_to_string(&path).await?;
        let records: Vec<RunRecord> = content.lines().rev().take(limit)
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect();
        Ok(records)
    }
}
```

**Step 3: Commit**

```bash
git add crates/nanocrab-scheduler/src/persistence.rs
git commit -m "feat(scheduler): add state persistence and JSONL run history"
```

---

### Task 6: Error Handling and Backoff Logic

**Files:**
- Modify: `crates/nanocrab-scheduler/src/manager.rs`

**Step 1: Implement exponential backoff schedule (5 levels)**

```rust
// crates/nanocrab-scheduler/src/backoff.rs

const ERROR_BACKOFF_MS: &[u64] = &[
    30_000,       // 1st error  →  30s
    60_000,       // 2nd error  →   1 min
    5 * 60_000,   // 3rd error  →   5 min
    15 * 60_000,  // 4th error  →  15 min
    60 * 60_000,  // 5th+ error →  60 min
];

pub fn error_backoff_ms(consecutive_errors: u32) -> u64 {
    let idx = (consecutive_errors.saturating_sub(1) as usize)
        .min(ERROR_BACKOFF_MS.len() - 1);
    ERROR_BACKOFF_MS[idx]
}
```

**Step 2: Implement `apply_job_result()` — the core state transition logic**

Called when `ScheduledTaskCompleted` is received. Handles all state transitions:

```rust
/// Apply the result of a completed run to the schedule entry.
/// Returns true if the entry should be deleted (one-time + delete_after_run + success).
pub fn apply_job_result(entry: &mut ScheduleEntry, result: &CompletedResult) -> bool {
    let state = &mut entry.state;
    let config = &entry.config;

    state.running_at_ms = None;
    state.last_run_at_ms = Some(result.started_at_ms);
    state.last_run_status = Some(result.status.clone());
    state.last_duration_ms = Some(result.duration_ms);
    state.last_error = result.error.clone();

    // Update consecutive error count
    match result.status {
        RunStatus::Ok => { state.consecutive_errors = 0; }
        RunStatus::Error => { state.consecutive_errors += 1; }
        RunStatus::Skipped => { /* no change */ }
    }

    // Check for one-time task deletion
    let should_delete = matches!(config.schedule, ScheduleType::At { .. })
        && config.delete_after_run
        && matches!(result.status, RunStatus::Ok);

    if should_delete {
        return true;
    }

    // Compute next run
    if matches!(config.schedule, ScheduleType::At { .. }) {
        // One-time: disable after execution (don't delete)
        entry.config.enabled = false;
        state.next_run_at_ms = None;
    } else if matches!(result.status, RunStatus::Error) && entry.config.enabled {
        // Apply backoff: next_run = max(normal_next, now + backoff)
        let backoff = error_backoff_ms(state.consecutive_errors);
        let normal_next = compute_next_run_at_ms(&config.schedule, result.ended_at_ms).ok().flatten();
        let backoff_next = result.ended_at_ms + backoff as i64;
        state.next_run_at_ms = Some(
            normal_next.map_or(backoff_next, |n| n.max(backoff_next))
        );
    } else if entry.config.enabled {
        state.next_run_at_ms = compute_next_run_at_ms(&config.schedule, result.ended_at_ms)
            .ok().flatten();
    }

    // Auto-disable after too many consecutive schedule computation errors
    // (3 consecutive errors where next_run could not be computed)
    if state.next_run_at_ms.is_none() && state.consecutive_errors >= 3 {
        tracing::warn!(schedule_id = %config.schedule_id, "Auto-disabling after 3 consecutive errors");
        entry.config.enabled = false;
    }

    false
}
```

**Step 3: Write tests**

```rust
#[test]
fn test_backoff_escalation() {
    assert_eq!(error_backoff_ms(1), 30_000);
    assert_eq!(error_backoff_ms(2), 60_000);
    assert_eq!(error_backoff_ms(5), 60 * 60_000);
    assert_eq!(error_backoff_ms(100), 60 * 60_000); // Caps at 60min
}

#[test]
fn test_one_time_task_deleted_on_success() {
    let mut entry = make_at_entry("20m", true); // delete_after_run: true
    let result = CompletedResult { status: RunStatus::Ok, .. };
    assert!(apply_job_result(&mut entry, &result));
}

#[test]
fn test_consecutive_errors_trigger_disable() {
    let mut entry = make_cron_entry();
    entry.state.consecutive_errors = 2;
    let result = CompletedResult { status: RunStatus::Error, .. };
    apply_job_result(&mut entry, &result);
    // consecutive_errors is now 3, and if next_run is None → disabled
}
```

**Step 4: Commit**

```bash
git add crates/nanocrab-scheduler/src/backoff.rs crates/nanocrab-scheduler/src/manager.rs
git commit -m "feat(scheduler): implement 5-level exponential backoff and auto-disable"
```

---

### Task 7: Schedule Agent Tool (AI self-service)

> This task enables the AI agent to autonomously create/manage scheduled tasks during conversation.
> When a user says "remind me in 20 minutes to check the oven", the agent calls this tool.

**Files:**
- Create: `crates/nanocrab-core/src/schedule_tool.rs`
- Modify: `crates/nanocrab-core/src/orchestrator.rs` (register tool)

**Step 1: Define tool schema**

The tool exposes CRUD operations to the LLM via function calling:

```rust
// crates/nanocrab-core/src/schedule_tool.rs

use nanocrab_scheduler::{ScheduleConfig, ScheduleType, SessionMode, DeliveryConfig, ScheduleManager};

pub const SCHEDULE_TOOL_NAME: &str = "schedule";

pub const SCHEDULE_TOOL_DESCRIPTION: &str = r#"Manage scheduled tasks — create reminders, recurring tasks, or one-time actions.

ACTIONS:
- list: List all scheduled tasks with their status
- add: Create a new scheduled task
- update: Update an existing task (requires schedule_id + fields to change)
- remove: Delete a task (requires schedule_id)
- run: Trigger a task immediately (requires schedule_id)

SCHEDULE TYPES:
- at: One-time. Supports relative ("20m", "2h") or absolute ("2026-03-01T09:00:00Z")
- every: Fixed interval in milliseconds
- cron: Standard cron expression with timezone

EXAMPLES:
- Reminder in 20 min: { "action": "add", "job": { "name": "Check oven", "schedule": { "kind": "at", "at": "20m" }, "task": "Remind user to check the oven", "session_mode": "main", "delete_after_run": true } }
- Daily 9am report: { "action": "add", "job": { "name": "Morning report", "schedule": { "kind": "cron", "expr": "0 9 * * *", "tz": "Asia/Shanghai" }, "task": "Summarize inbox and calendar", "session_mode": "isolated" } }
"#;

pub fn schedule_tool_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "action": {
                "type": "string",
                "enum": ["list", "add", "update", "remove", "run"]
            },
            "job": {
                "type": "object",
                "description": "Required for 'add'. The schedule job definition.",
                "properties": {
                    "name": { "type": "string" },
                    "schedule": {
                        "type": "object",
                        "properties": {
                            "kind": { "type": "string", "enum": ["at", "every", "cron"] },
                            "at": { "type": "string", "description": "For 'at': relative ('20m') or ISO 8601" },
                            "expr": { "type": "string", "description": "For 'cron': cron expression" },
                            "tz": { "type": "string", "description": "For 'cron': timezone (default UTC)" },
                            "interval_ms": { "type": "integer", "description": "For 'every': ms between runs" }
                        }
                    },
                    "task": { "type": "string", "description": "The prompt/message for the agent" },
                    "session_mode": { "type": "string", "enum": ["main", "isolated"], "default": "isolated" },
                    "agent_id": { "type": "string", "description": "Target agent (defaults to current)" },
                    "delete_after_run": { "type": "boolean", "default": false },
                    "context_messages": { "type": "integer", "description": "Include N recent messages as context in the task payload" },
                    "delivery": {
                        "type": "object",
                        "properties": {
                            "mode": { "type": "string", "enum": ["announce", "none"] },
                            "channel": { "type": "string" },
                            "connector_id": { "type": "string" }
                        }
                    }
                }
            },
            "schedule_id": { "type": "string", "description": "Required for update/remove/run" },
            "patch": { "type": "object", "description": "For 'update': fields to change" }
        },
        "required": ["action"]
    })
}
```

**Step 2: Implement tool execution**

```rust
pub struct ScheduleTool {
    manager: Arc<ScheduleManager>,
}

#[async_trait]
impl ToolExecutor for ScheduleTool {
    async fn execute(
        &self,
        input: &serde_json::Value,
        ctx: &ToolContext,
    ) -> Result<String> {
        let action = input["action"].as_str().unwrap_or("list");

        match action {
            "list" => {
                let entries = self.manager.list().await;
                let summary: Vec<_> = entries.iter().map(|e| {
                    serde_json::json!({
                        "schedule_id": e.config.schedule_id,
                        "name": e.config.name,
                        "enabled": e.config.enabled,
                        "next_run": e.state.next_run_at_ms,
                        "last_status": e.state.last_run_status,
                        "consecutive_errors": e.state.consecutive_errors,
                    })
                }).collect();
                Ok(serde_json::to_string_pretty(&summary)?)
            }
            "add" => {
                let job_input = &input["job"];
                let mut config = parse_schedule_config(job_input)?;

                // Generate schedule_id if not provided
                if config.schedule_id.is_empty() {
                    config.schedule_id = slug_from_name(&config.name);
                }

                // Context injection: append recent messages to task payload
                if let Some(n) = input["job"]["context_messages"].as_u64() {
                    if n > 0 {
                        // ctx provides access to recent conversation history
                        let recent = ctx.recent_messages(n as usize);
                        let context_text: String = recent.iter()
                            .map(|m| format!("- {}: {}", m.role, m.content))
                            .collect::<Vec<_>>()
                            .join("\n");
                        config.task = format!("{}\n\nRecent context:\n{}", config.task, context_text);
                    }
                }

                // Hot-add: takes effect immediately + persists to YAML
                self.manager.add_schedule(config.clone()).await?;

                Ok(format!("Created schedule '{}' (id: {}). Next run: {:?}",
                    config.name, config.schedule_id,
                    self.manager.get_next_run(&config.schedule_id).await))
            }
            "remove" => {
                let id = input["schedule_id"].as_str()
                    .ok_or_else(|| anyhow!("schedule_id required for remove"))?;
                self.manager.remove_schedule(id).await?;
                Ok(format!("Removed schedule '{}'", id))
            }
            "run" => {
                let id = input["schedule_id"].as_str()
                    .ok_or_else(|| anyhow!("schedule_id required for run"))?;
                self.manager.trigger_now(id).await?;
                Ok(format!("Triggered immediate run of '{}'", id))
            }
            "update" => {
                let id = input["schedule_id"].as_str()
                    .ok_or_else(|| anyhow!("schedule_id required for update"))?;
                let patch = &input["patch"];
                self.manager.update_schedule(id, patch).await?;
                Ok(format!("Updated schedule '{}'", id))
            }
            _ => Err(anyhow!("Unknown action: {}", action)),
        }
    }
}
```

**Step 3: Register tool in orchestrator**

```rust
// In orchestrator tool registration (crates/nanocrab-core/src/orchestrator.rs)
// Add alongside existing tools (file_tools, web_fetch, shell_tool, etc.)

let schedule_tool = ScheduleTool { manager: schedule_manager.clone() };
tools.insert(SCHEDULE_TOOL_NAME.to_string(), Box::new(schedule_tool));
```

**Step 4: Commit**

```bash
git add crates/nanocrab-core/src/schedule_tool.rs crates/nanocrab-core/src/orchestrator.rs
git commit -m "feat(scheduler): expose schedule tool to agents for autonomous task management"
```

---

### Task 8: CLI Management Commands

**Files:**
- Modify: `crates/nanocrab-cli/src/main.rs`

**Step 1: Add `schedule` subcommand group**

```rust
#[derive(Subcommand)]
enum Commands {
    // ... existing commands
    /// Manage scheduled tasks
    #[command(subcommand)]
    Schedule(ScheduleCommands),
}

#[derive(Subcommand)]
enum ScheduleCommands {
    /// List all scheduled tasks with status
    List,
    /// Trigger a scheduled task immediately
    Run { schedule_id: String },
    /// Enable a disabled schedule
    Enable { schedule_id: String },
    /// Disable a schedule (without deleting)
    Disable { schedule_id: String },
    /// Show recent run history for a schedule
    History {
        schedule_id: String,
        #[arg(long, default_value = "10")]
        limit: usize,
    },
}
```

**Step 2: Implement handlers**

```rust
ScheduleCommands::List => {
    let configs: Vec<ScheduleConfig> = read_yaml_dir(&config_root.join("schedules.d"))?;
    let states = StateStore::load(&data_dir.join("schedules"))?;

    println!("{:<20} {:<8} {:<25} {:<20} {:<8}",
        "ID", "Enabled", "Schedule", "Next Run", "Errors");
    println!("{}", "-".repeat(85));

    for config in &configs {
        let state = states.get(&config.schedule_id);
        let next_run = state
            .and_then(|s| s.next_run_at_ms)
            .map(|ms| Utc.timestamp_millis_opt(ms).unwrap().to_rfc3339())
            .unwrap_or_else(|| "—".into());
        let errors = state.map(|s| s.consecutive_errors).unwrap_or(0);

        println!("{:<20} {:<8} {:<25} {:<20} {:<8}",
            config.schedule_id,
            if config.enabled { "yes" } else { "no" },
            format_schedule_type(&config.schedule),
            next_run,
            errors,
        );
    }
}

ScheduleCommands::History { schedule_id, limit } => {
    let store = HistoryStore::new(&data_dir.join("schedules"));
    let records = store.recent(&schedule_id, limit).await?;

    for record in records {
        println!("{} | {:>6}ms | {:?} | {}",
            record.started_at.to_rfc3339(),
            record.duration_ms,
            record.status,
            record.error.as_deref().unwrap_or("—"),
        );
    }
}
```

**Step 3: Commit**

```bash
git add crates/nanocrab-cli/src/main.rs
git commit -m "feat(cli): add schedule list/run/enable/disable/history commands"
```

---

### Task 9: Backend API Endpoints

**Files:**
- Create: `crates/nanocrab-server/src/routes/schedules.rs`
- Modify: `crates/nanocrab-server/src/routes/mod.rs` (register routes)

**Step 1: Define API response types**

```rust
// crates/nanocrab-server/src/routes/schedules.rs

#[derive(Serialize)]
pub struct ScheduleListItem {
    pub schedule_id: String,
    pub name: String,
    pub description: Option<String>,
    pub enabled: bool,
    pub schedule: ScheduleType,
    pub agent_id: String,
    pub session_mode: SessionMode,
    pub next_run_at: Option<String>,       // ISO 8601
    pub last_run_status: Option<RunStatus>,
    pub last_run_at: Option<String>,
    pub consecutive_errors: u32,
}

#[derive(Serialize)]
pub struct ScheduleRunHistoryItem {
    pub started_at: String,
    pub ended_at: String,
    pub status: RunStatus,
    pub error: Option<String>,
    pub duration_ms: u64,
}
```

**Step 2: Implement endpoints**

```rust
// GET /api/schedules — list all schedules with merged state
async fn list_schedules(State(state): State<AppState>) -> Json<Vec<ScheduleListItem>> { ... }

// POST /api/schedules/:id/run — trigger immediate run
async fn run_schedule(
    State(state): State<AppState>,
    Path(schedule_id): Path<String>,
) -> Result<StatusCode, StatusCode> { ... }

// PATCH /api/schedules/:id — toggle enabled/disabled
async fn toggle_schedule(
    State(state): State<AppState>,
    Path(schedule_id): Path<String>,
    Json(body): Json<ToggleBody>,
) -> Result<StatusCode, StatusCode> { ... }

// GET /api/schedules/:id/history — recent runs
async fn schedule_history(
    State(state): State<AppState>,
    Path(schedule_id): Path<String>,
    Query(params): Query<HistoryParams>,
) -> Json<Vec<ScheduleRunHistoryItem>> { ... }
```

**Step 3: Register routes**

```rust
// crates/nanocrab-server/src/routes/mod.rs
pub fn api_routes(state: AppState) -> Router {
    Router::new()
        // ... existing routes
        .route("/api/schedules", get(schedules::list_schedules))
        .route("/api/schedules/:id/run", post(schedules::run_schedule))
        .route("/api/schedules/:id", patch(schedules::toggle_schedule))
        .route("/api/schedules/:id/history", get(schedules::schedule_history))
        .with_state(state)
}
```

**Step 4: Commit**

```bash
git add crates/nanocrab-server/src/routes/schedules.rs crates/nanocrab-server/src/routes/mod.rs
git commit -m "feat(api): add schedule management REST endpoints"
```

---

### Task 10: Frontend Schedules Page

**Files:**
- Modify: `web/src/hooks/use-api.ts` (add schedule hooks)
- Create: `web/src/app/schedules/page.tsx`

> Note: Frontend path is `web/src/`, not `apps/web/`. Follow existing patterns in `web/src/app/channels/page.tsx` and `web/src/app/providers/page.tsx`.

**Step 1: Add API hooks**

```typescript
// web/src/hooks/use-api.ts (add to existing file)

export interface ScheduleListItem {
  schedule_id: string;
  name: string;
  description?: string;
  enabled: boolean;
  schedule: { kind: string; expr?: string; tz?: string; at?: string; interval_ms?: number };
  agent_id: string;
  session_mode: "main" | "isolated";
  next_run_at: string | null;
  last_run_status: "ok" | "error" | "skipped" | null;
  last_run_at: string | null;
  consecutive_errors: number;
}

export function useSchedules() {
  return useQuery<ScheduleListItem[]>({
    queryKey: ['schedules'],
    queryFn: () => fetch('/api/schedules').then(r => r.json()),
    refetchInterval: 10_000,  // Poll every 10s for status updates
  });
}

export function useRunSchedule() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: (scheduleId: string) =>
      fetch(`/api/schedules/${scheduleId}/run`, { method: 'POST' }),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ['schedules'] }),
  });
}

export function useToggleSchedule() {
  const queryClient = useQueryClient();
  return useMutation({
    mutationFn: ({ id, enabled }: { id: string; enabled: boolean }) =>
      fetch(`/api/schedules/${id}`, {
        method: 'PATCH',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ enabled }),
      }),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: ['schedules'] }),
  });
}
```

**Step 2: Create schedules page**

```tsx
// web/src/app/schedules/page.tsx

"use client";

import { useSchedules, useRunSchedule, useToggleSchedule } from "@/hooks/use-api";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Switch } from "@/components/ui/switch";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { Play, Clock, AlertTriangle } from "lucide-react";
import { formatDistanceToNow } from "date-fns";

export default function SchedulesPage() {
  const { data: schedules, isLoading } = useSchedules();
  const runMutation = useRunSchedule();
  const toggleMutation = useToggleSchedule();

  // ... render schedule cards with:
  // - Toggle switch (enabled/disabled)
  // - Schedule description (cron expr / "at" time / interval)
  // - Next run countdown
  // - Last run status badge (green ok / red error / gray skipped)
  // - Consecutive errors warning
  // - "Run Now" button
  // Follow the card-based layout pattern from channels/page.tsx
}
```

**Step 3: Add navigation link**

Add "Schedules" to the sidebar/nav in the existing layout component.

**Step 4: Commit**

```bash
git add web/src/hooks/use-api.ts web/src/app/schedules/page.tsx
git commit -m "feat(web): add schedules management page with status and controls"
```

---

### Task 11: Integration Tests

**Files:**
- Create: `crates/nanocrab-scheduler/tests/integration.rs`

**Step 1: Test schedule computation**

```rust
#[test]
fn test_cron_schedule_config_loads() {
    let yaml = r#"
schedule_id: test-daily
enabled: true
name: "Test Daily"
schedule:
  kind: cron
  expr: "0 9 * * *"
  tz: "Asia/Shanghai"
agent_id: nanocrab-main
session_mode: isolated
task: "Test task"
"#;
    let config: ScheduleConfig = serde_yaml::from_str(yaml).unwrap();
    assert_eq!(config.schedule_id, "test-daily");
    assert!(config.enabled);
    assert!(matches!(config.delivery.mode, DeliveryMode::None)); // default
}
```

**Step 2: Test ScheduleManager trigger flow**

```rust
#[tokio::test]
async fn test_schedule_triggers_bus_event() {
    let bus = Arc::new(EventBus::new());
    let mut rx = bus.subscribe(Topic::ScheduledTaskTriggered).await;

    // Create a schedule due "now"
    let config = ScheduleConfig {
        schedule_id: "test-immediate".into(),
        enabled: true,
        name: "Immediate Test".into(),
        schedule: ScheduleType::At { at: "1s".into() },
        agent_id: "test-agent".into(),
        session_mode: SessionMode::Isolated,
        task: "Hello from test".into(),
        ..Default::default()
    };

    let manager = ScheduleManager::from_configs(vec![config], &temp_dir, bus.clone()).unwrap();
    tokio::spawn(async move { manager.run().await });

    // Should receive trigger within 2 seconds
    let msg = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await;
    assert!(msg.is_ok());
    if let Some(BusMessage::ScheduledTaskTriggered { schedule_id, .. }) = msg.unwrap() {
        assert_eq!(schedule_id, "test-immediate");
    }
}
```

**Step 3: Test backoff and state persistence**

```rust
#[test]
fn test_error_backoff_persists_state() {
    let mut entry = make_cron_entry();
    let result = CompletedResult {
        status: RunStatus::Error,
        error: Some("timeout".into()),
        ..make_result()
    };

    apply_job_result(&mut entry, &result);
    assert_eq!(entry.state.consecutive_errors, 1);
    assert!(entry.state.next_run_at_ms.is_some());
    // Next run should be at least 30s from ended_at
    let min_next = result.ended_at_ms + 30_000;
    assert!(entry.state.next_run_at_ms.unwrap() >= min_next);
}
```

**Step 4: Test schedule tool add + hot-load**

```rust
#[tokio::test]
async fn test_schedule_tool_add_hot_loads() {
    let manager = setup_test_manager().await;
    let tool = ScheduleTool { manager: manager.clone() };

    let input = serde_json::json!({
        "action": "add",
        "job": {
            "name": "Test reminder",
            "schedule": { "kind": "at", "at": "5m" },
            "task": "Check something",
            "session_mode": "main",
            "delete_after_run": true
        }
    });

    let result = tool.execute(&input, &ToolContext::default()).await.unwrap();
    assert!(result.contains("Created schedule"));

    // Verify it's in the manager
    let entries = manager.list().await;
    assert!(entries.iter().any(|e| e.config.name == "Test reminder"));

    // Verify YAML file was written
    assert!(temp_dir.join("schedules.d/test-reminder.yaml").exists());
}
```

**Step 5: Run all tests**

```bash
cargo test -p nanocrab-scheduler
```

**Step 6: Commit**

```bash
git add crates/nanocrab-scheduler/tests/
git commit -m "test(scheduler): add integration tests for config, trigger, backoff, and agent tool"
```
