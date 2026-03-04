# Scheduler Redesign Phase 2 Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Implement the remaining scheduler redesign features: DeliveryConfig extensions (webhook_url, failure_destination, best_effort), Schedule Tool payload schema exposure, YAML config legacy migration, Webhook HTTP POST delivery, and MessageTool for cross-channel agent messaging.

**Architecture:** Task E extends the scheduler's `DeliveryConfig` and `DeliveryMode` with webhook support fields. Task C updates the schedule tool's JSON schema to expose the `payload` parameter to agents. Task D adds `migrate_legacy()` to auto-convert old `task+session_mode` configs to `payload`. Task A implements the webhook HTTP POST delivery in the gateway crate. Task B creates a new `MessageTool` that lets agents proactively send messages to any channel via the existing `BusPublisher`.

**Tech Stack:** Rust, serde, tokio, reqwest (for webhook HTTP POST), clawhive-bus BusPublisher, clawhive-schema BusMessage

---

## Task 1: Extend DeliveryConfig with webhook_url, failure_destination, best_effort

Add `webhook_url`, `failure_destination`, and `best_effort` fields to the scheduler's `DeliveryConfig`, and add `Webhook` variant to `DeliveryMode`.

**Files:**
- Modify: `crates/clawhive-scheduler/src/config.rs`

**Step 1: Write failing test**

In `crates/clawhive-scheduler/src/config.rs`, add to the existing `mod tests` block:

```rust
#[test]
fn delivery_config_serde_with_webhook() {
    let config = DeliveryConfig {
        mode: DeliveryMode::Webhook,
        webhook_url: Some("https://example.com/hook".into()),
        best_effort: true,
        failure_destination: Some(FailureDestination {
            channel: Some("discord".into()),
            connector_id: Some("dc_main".into()),
            conversation_scope: Some("guild:1:channel:2".into()),
        }),
        ..Default::default()
    };
    let json = serde_json::to_string(&config).unwrap();
    assert!(json.contains("webhook"));
    assert!(json.contains("https://example.com/hook"));
    assert!(json.contains("best_effort"));
    let back: DeliveryConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(back.mode, DeliveryMode::Webhook);
    assert_eq!(back.webhook_url.as_deref(), Some("https://example.com/hook"));
    assert!(back.best_effort);
    assert!(back.failure_destination.is_some());
}

#[test]
fn delivery_config_defaults_backward_compatible() {
    // Existing configs without new fields should still deserialize
    let json = r#"{"mode":"none"}"#;
    let config: DeliveryConfig = serde_json::from_str(json).unwrap();
    assert_eq!(config.mode, DeliveryMode::None);
    assert!(config.webhook_url.is_none());
    assert!(!config.best_effort);
    assert!(config.failure_destination.is_none());
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p clawhive-scheduler delivery_config_serde_with_webhook`
Expected: FAIL — `DeliveryMode::Webhook`, `webhook_url`, `best_effort`, `failure_destination`, `FailureDestination` don't exist

**Step 3: Add new types and fields**

In `crates/clawhive-scheduler/src/config.rs`:

1. Add `Webhook` variant to `DeliveryMode`:
```rust
#[derive(Debug, Serialize, Deserialize, Clone, Default, PartialEq, Eq)]
pub enum DeliveryMode {
    #[default]
    #[serde(rename = "none")]
    None,
    #[serde(rename = "announce")]
    Announce,
    #[serde(rename = "webhook")]
    Webhook,
}
```

2. Add `FailureDestination` struct (before `DeliveryConfig`):
```rust
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct FailureDestination {
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub connector_id: Option<String>,
    #[serde(default)]
    pub conversation_scope: Option<String>,
}
```

3. Add new fields to `DeliveryConfig`:
```rust
pub struct DeliveryConfig {
    // ... existing fields ...
    
    /// Webhook URL for webhook delivery mode
    #[serde(default)]
    pub webhook_url: Option<String>,
    /// Where to deliver failure notifications
    #[serde(default)]
    pub failure_destination: Option<FailureDestination>,
    /// Best-effort delivery: don't report delivery failure as error
    #[serde(default)]
    pub best_effort: bool,
}
```

4. Update `Default` impl for `DeliveryConfig` to include the new fields:
```rust
impl Default for DeliveryConfig {
    fn default() -> Self {
        Self {
            // ... existing defaults ...
            webhook_url: None,
            failure_destination: None,
            best_effort: false,
        }
    }
}
```

5. Update the `manager.rs` code that maps `DeliveryMode` to `ScheduledDeliveryMode` — add the `Webhook` arm:
```rust
DeliveryMode::Webhook => ScheduledDeliveryMode::Webhook,
```
Also include `webhook_url` in the `ScheduledDeliveryInfo` construction:
```rust
webhook_url: entry.config.delivery.webhook_url.clone(),
```

**Step 4: Run tests to verify they pass**

Run: `cargo test -p clawhive-scheduler`
Expected: PASS

**Step 5: Run full build**

Run: `cargo build`
Expected: PASS (no compile errors — verify manager.rs match arm is exhaustive)

**Step 6: Commit**

```bash
git add crates/clawhive-scheduler/src/config.rs crates/clawhive-scheduler/src/manager.rs
git commit -m "feat(scheduler): extend DeliveryConfig with webhook_url, failure_destination, best_effort"
```

---

## Task 2: Update Schedule Tool to expose payload parameter

Add `payload` field to the schedule tool's JSON schema and `ScheduleJobInput`, enabling agents to create jobs with typed payloads instead of just the legacy `task` field.

**Files:**
- Modify: `crates/clawhive-core/src/schedule_tool.rs`

**Step 1: Write failing test**

In `crates/clawhive-core/src/schedule_tool.rs`, add to existing `mod tests`:

```rust
#[tokio::test]
async fn add_action_with_payload_direct_deliver() {
    let (manager, _bus, _tmp) = setup();
    let tool = ScheduleTool::new(manager.clone());
    let ctx = ToolContext::builtin();

    let result = tool
        .execute(
            serde_json::json!({
                "action": "add",
                "job": {
                    "name": "Direct reminder",
                    "schedule": { "kind": "at", "at": "5m" },
                    "payload": {
                        "kind": "direct_deliver",
                        "text": "Time to eat!"
                    }
                }
            }),
            &ctx,
        )
        .await
        .unwrap();

    assert!(!result.is_error);
    let entries = manager.list().await;
    assert_eq!(entries.len(), 1);
    // When payload is provided, the config should still have a task field
    // (set from payload for backward compat) but the payload is the source of truth
}

#[tokio::test]
async fn add_action_with_payload_agent_turn() {
    let (manager, _bus, _tmp) = setup();
    let tool = ScheduleTool::new(manager.clone());
    let ctx = ToolContext::builtin();

    let result = tool
        .execute(
            serde_json::json!({
                "action": "add",
                "job": {
                    "name": "Agent task",
                    "schedule": { "kind": "cron", "expr": "0 9 * * *" },
                    "payload": {
                        "kind": "agent_turn",
                        "message": "Generate daily report",
                        "timeout_seconds": 600
                    }
                }
            }),
            &ctx,
        )
        .await
        .unwrap();

    assert!(!result.is_error);
    let entries = manager.list().await;
    assert_eq!(entries.len(), 1);
}

#[tokio::test]
async fn add_action_legacy_task_still_works() {
    // Ensure backward compat: providing only `task` still works
    let (manager, _bus, _tmp) = setup();
    let tool = ScheduleTool::new(manager.clone());
    let ctx = ToolContext::builtin();

    let result = tool
        .execute(
            serde_json::json!({
                "action": "add",
                "job": {
                    "name": "Legacy task",
                    "schedule": { "kind": "at", "at": "5m" },
                    "task": "Old style task"
                }
            }),
            &ctx,
        )
        .await
        .unwrap();

    assert!(!result.is_error);
    let entries = manager.list().await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].config.task, "Old style task");
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p clawhive-core add_action_with_payload_direct_deliver`
Expected: FAIL — `payload` field not in `ScheduleJobInput`

**Step 3: Update ScheduleJobInput and definition schema**

In `crates/clawhive-core/src/schedule_tool.rs`:

1. Add import for `TaskPayload` and `resolve_payload`:
```rust
use clawhive_scheduler::{
    DeliveryConfig, DeliveryMode, ScheduleConfig, ScheduleManager, ScheduleType, SessionMode,
    TaskPayload, resolve_payload,
};
```

2. Add `payload` field to `ScheduleJobInput` (after `task`):
```rust
#[derive(Debug, Deserialize)]
struct ScheduleJobInput {
    #[serde(default)]
    schedule_id: Option<String>,
    name: String,
    #[serde(default)]
    description: Option<String>,
    schedule: ScheduleType,
    #[serde(default)]
    task: Option<String>,  // ← Change from `String` to `Option<String>`
    #[serde(default)]
    payload: Option<TaskPayload>,  // ← NEW
    #[serde(default)]
    session_mode: Option<SessionMode>,
    #[serde(default)]
    agent_id: Option<String>,
    #[serde(default)]
    delete_after_run: Option<bool>,
    #[serde(default)]
    timeout_seconds: Option<u64>,
    #[serde(default)]
    context_messages: Option<usize>,
    #[serde(default)]
    delivery: Option<DeliveryInput>,
    #[serde(default)]
    webhook_url: Option<String>,  // ← NEW: convenience field for webhook delivery
}
```

3. Update `into_config()` to use `resolve_payload`:

The key change: `task` is now `Option<String>`. The function should resolve which task string to use based on payload vs legacy task field.

```rust
fn into_config(self, default_agent_id: &str, ctx: &ToolContext) -> Result<ScheduleConfig, anyhow::Error> {
    // Resolve payload from explicit payload or legacy task field
    let resolved = resolve_payload(self.task.clone(), self.payload)?;

    // Extract task text for backward compat (stored in config.task)
    let mut task = match &resolved {
        TaskPayload::AgentTurn { message, .. } => message.clone(),
        TaskPayload::SystemEvent { text } => text.clone(),
        TaskPayload::DirectDeliver { text } => text.clone(),
    };

    // Append context messages if requested
    if let Some(limit) = self.context_messages {
        if limit > 0 {
            let context = ctx
                .recent_messages(limit)
                .into_iter()
                .map(|message| format!("- {}: {}", message.role, message.content))
                .collect::<Vec<_>>()
                .join("\n");
            if !context.is_empty() {
                task = format!("{task}\n\nRecent context:\n{context}");
            }
        }
    }

    // ... rest of delivery/schedule logic stays the same ...
}
```

**IMPORTANT**: The return type changes from `ScheduleConfig` to `Result<ScheduleConfig, anyhow::Error>` because `resolve_payload` can fail. Update the callsite in the `"add"` action:
```rust
"add" => {
    let Some(job) = parsed.job else {
        return Ok(tool_error("job is required for add action"));
    };
    let config = match job.into_config(&self.default_agent_id, ctx) {
        Ok(c) => c,
        Err(e) => return Ok(tool_error(format!("Invalid job: {e}"))),
    };
    // ... rest unchanged
}
```

4. Update `definition()` JSON schema — add `payload` property to the `job` object:

In the `"job"` properties, add:
```json
"payload": {
    "type": "object",
    "description": "Typed task payload. Use instead of 'task' for typed control. Kinds: system_event (inject into source session), agent_turn (isolated agent execution), direct_deliver (simple text delivery).",
    "properties": {
        "kind": {
            "type": "string",
            "enum": ["system_event", "agent_turn", "direct_deliver"],
            "description": "Payload type"
        },
        "text": { "type": "string", "description": "For system_event and direct_deliver: the text content" },
        "message": { "type": "string", "description": "For agent_turn: the task message for the agent" },
        "model": { "type": "string", "description": "For agent_turn: model override (e.g. anthropic/claude-opus-4)" },
        "thinking": { "type": "string", "description": "For agent_turn: thinking/reasoning level" },
        "timeout_seconds": { "type": "number", "description": "For agent_turn: execution timeout in seconds (default 300)" }
    },
    "required": ["kind"]
}
```

Also update `"task"` description to note it's legacy:
```json
"task": {
    "type": "string",
    "description": "Legacy: task/reminder text. Prefer using 'payload' instead for typed control."
}
```

Change the `"required"` for `job` from `["name", "schedule", "task"]` to `["name", "schedule"]` — since either `task` or `payload` can be provided.

5. Add `webhook_url` to `DeliveryInput`:
```rust
#[derive(Debug, Deserialize)]
struct DeliveryInput {
    #[serde(default)]
    mode: Option<DeliveryMode>,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    connector_id: Option<String>,
    #[serde(default)]
    webhook_url: Option<String>,
}
```

Update `into_config` delivery construction to pass through `webhook_url`:
```rust
delivery: DeliveryConfig {
    mode: delivery_mode,
    channel: delivery.channel,
    connector_id: delivery.connector_id,
    source_channel_type: ctx.source_channel_type().map(String::from),
    source_connector_id: ctx.source_connector_id().map(String::from),
    source_conversation_scope: ctx.source_conversation_scope().map(String::from),
    source_user_scope: ctx.source_user_scope().map(String::from),
    webhook_url: delivery.webhook_url.or(self_webhook_url),
    failure_destination: None,
    best_effort: false,
},
```
(Where `self_webhook_url` comes from `self.webhook_url` on the `ScheduleJobInput`.)

**Step 4: Run tests to verify they pass**

Run: `cargo test -p clawhive-core add_action_with_payload`
Expected: PASS

**Step 5: Run all tests**

Run: `cargo test`
Expected: PASS

**Step 6: Commit**

```bash
git add crates/clawhive-core/src/schedule_tool.rs
git commit -m "feat(schedule-tool): expose payload parameter for typed task payloads"
```

---

## Task 3: YAML config legacy migration

Add `migrate_legacy()` method to `ScheduleConfig` that auto-converts old `task + session_mode` configs to the `payload` field. Add an `Optional<TaskPayload>` payload field to `ScheduleConfig`.

**Files:**
- Modify: `crates/clawhive-scheduler/src/config.rs`
- Modify: `crates/clawhive-scheduler/src/manager.rs` (call migrate_legacy after loading)

**Step 1: Write failing test**

In `crates/clawhive-scheduler/src/config.rs`, add to `mod tests`:

```rust
#[test]
fn migrate_legacy_isolated_becomes_agent_turn() {
    let mut config = ScheduleConfig {
        schedule_id: "test".into(),
        name: "Test".into(),
        task: "do stuff".into(),
        session_mode: SessionMode::Isolated,
        payload: None,
        ..Default::default()
    };
    config.migrate_legacy();
    let payload = config.payload.as_ref().expect("payload should be set");
    match payload {
        TaskPayload::AgentTurn { message, timeout_seconds, .. } => {
            assert_eq!(message, "do stuff");
            assert_eq!(*timeout_seconds, 300);
        }
        _ => panic!("expected AgentTurn"),
    }
}

#[test]
fn migrate_legacy_main_becomes_system_event() {
    let mut config = ScheduleConfig {
        schedule_id: "test".into(),
        name: "Test".into(),
        task: "remind me".into(),
        session_mode: SessionMode::Main,
        payload: None,
        ..Default::default()
    };
    config.migrate_legacy();
    let payload = config.payload.as_ref().expect("payload should be set");
    assert!(matches!(payload, TaskPayload::SystemEvent { text } if text == "remind me"));
}

#[test]
fn migrate_legacy_skips_if_payload_present() {
    let mut config = ScheduleConfig {
        schedule_id: "test".into(),
        name: "Test".into(),
        task: "old task".into(),
        payload: Some(TaskPayload::DirectDeliver { text: "new".into() }),
        ..Default::default()
    };
    config.migrate_legacy();
    assert!(matches!(config.payload.as_ref().unwrap(), TaskPayload::DirectDeliver { text } if text == "new"));
}

#[test]
fn migrate_legacy_skips_if_task_empty() {
    let mut config = ScheduleConfig {
        schedule_id: "test".into(),
        name: "Test".into(),
        task: String::new(),
        payload: None,
        ..Default::default()
    };
    config.migrate_legacy();
    assert!(config.payload.is_none());
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p clawhive-scheduler migrate_legacy`
Expected: FAIL — `payload` field doesn't exist on `ScheduleConfig`, `migrate_legacy` method doesn't exist

**Step 3: Add payload field and migrate_legacy**

In `crates/clawhive-scheduler/src/config.rs`:

1. Add `payload` field to `ScheduleConfig`:
```rust
pub struct ScheduleConfig {
    // ... existing fields ...
    
    /// Typed task payload. Takes precedence over legacy `task` field.
    #[serde(default)]
    pub payload: Option<TaskPayload>,
}
```

2. Update `Default` impl for `ScheduleConfig` to include `payload: None`.

3. Add `migrate_legacy` method to `ScheduleConfig`:
```rust
impl ScheduleConfig {
    /// Auto-convert legacy `task + session_mode` to `payload` if payload is not set.
    pub fn migrate_legacy(&mut self) {
        if self.payload.is_some() {
            return;
        }
        if self.task.is_empty() {
            return;
        }
        self.payload = Some(match self.session_mode {
            SessionMode::Main => TaskPayload::SystemEvent {
                text: self.task.clone(),
            },
            SessionMode::Isolated => TaskPayload::AgentTurn {
                message: self.task.clone(),
                model: None,
                thinking: None,
                timeout_seconds: self.timeout_seconds,
                light_context: false,
            },
        });
    }
}
```

4. In `crates/clawhive-scheduler/src/manager.rs`, in `ScheduleManager::new()`, after loading configs and before building entries, call `migrate_legacy()` on each config:
```rust
for config in configs.iter_mut() {
    config.migrate_legacy();
}
```

5. Also in `manager.rs`, update the `BusMessage::ScheduledTaskTriggered` construction in `trigger_now()` and `check_and_trigger()` to use `entry.config.payload` if available, falling back to the current `AgentTurn` construction from `entry.config.task`:
```rust
let payload = entry.config.payload.clone().unwrap_or_else(|| {
    ScheduledTaskPayload::AgentTurn {
        message: entry.config.task.clone(),
        model: None,
        thinking: None,
        timeout_seconds: entry.config.timeout_seconds,
        light_context: false,
    }
});
```
Convert from `TaskPayload` (scheduler) to `ScheduledTaskPayload` (schema) as needed — the enums mirror each other.

**Step 4: Run tests to verify they pass**

Run: `cargo test -p clawhive-scheduler migrate_legacy`
Expected: PASS

**Step 5: Run full build and all tests**

Run: `cargo build && cargo test`
Expected: PASS

**Step 6: Commit**

```bash
git add crates/clawhive-scheduler/src/config.rs crates/clawhive-scheduler/src/manager.rs
git commit -m "feat(scheduler): add payload field to ScheduleConfig with migrate_legacy() for backward compat"
```

---

## Task 4: Webhook HTTP POST delivery

Implement the actual webhook delivery logic: HTTP POST with retry, and integrate it into the gateway's `deliver_if_needed` function.

**Files:**
- Create: `crates/clawhive-gateway/src/webhook.rs`
- Modify: `crates/clawhive-gateway/src/lib.rs`
- Modify: `crates/clawhive-gateway/Cargo.toml`

**Step 1: Write failing test for webhook module**

Create `crates/clawhive-gateway/src/webhook.rs` with the test:

```rust
use std::time::Duration;

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use reqwest::Client;
use serde::Serialize;

const WEBHOOK_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_RETRIES: u32 = 2;
const RETRY_DELAY: Duration = Duration::from_secs(2);

#[derive(Debug, Serialize)]
pub struct WebhookPayload {
    pub schedule_id: String,
    pub status: String,
    pub response: Option<String>,
    pub error: Option<String>,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub duration_ms: u64,
}

pub async fn deliver_webhook(url: &str, payload: &WebhookPayload) -> Result<()> {
    todo!("implement webhook delivery")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webhook_payload_serializes_correctly() {
        let now = Utc::now();
        let payload = WebhookPayload {
            schedule_id: "test-job".into(),
            status: "ok".into(),
            response: Some("result text".into()),
            error: None,
            started_at: now,
            ended_at: now,
            duration_ms: 1500,
        };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("test-job"));
        assert!(json.contains("result text"));
        assert!(json.contains("1500"));
    }
}
```

**Step 2: Add reqwest dependency to gateway**

In `crates/clawhive-gateway/Cargo.toml`, add under `[dependencies]`:
```toml
reqwest.workspace = true
serde_json.workspace = true
```

In `crates/clawhive-gateway/src/lib.rs`, add module declaration:
```rust
pub mod webhook;
```

**Step 3: Run test to verify it compiles and test passes**

Run: `cargo test -p clawhive-gateway webhook_payload_serializes_correctly`
Expected: PASS (the serialization test doesn't call `deliver_webhook`)

**Step 4: Implement deliver_webhook**

Replace the `todo!()` in `webhook.rs`:

```rust
pub async fn deliver_webhook(url: &str, payload: &WebhookPayload) -> Result<()> {
    let client = Client::builder()
        .timeout(WEBHOOK_TIMEOUT)
        .build()?;

    let mut last_error = None;

    for attempt in 0..=MAX_RETRIES {
        if attempt > 0 {
            tokio::time::sleep(RETRY_DELAY * attempt).await;
        }

        match client
            .post(url)
            .header("Content-Type", "application/json")
            .header("User-Agent", "ClawhHive-Scheduler/1.0")
            .json(payload)
            .send()
            .await
        {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    return Ok(());
                }
                let body = resp.text().await.unwrap_or_default();
                if status.is_server_error() {
                    // 5xx → retryable
                    last_error = Some(anyhow!("webhook returned {}: {}", status, body));
                    continue;
                }
                // 4xx → not retryable
                return Err(anyhow!("webhook returned {}: {}", status, body));
            }
            Err(e) => {
                last_error = Some(anyhow!("webhook request failed: {e}"));
                continue;
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow!("webhook delivery failed after retries")))
}
```

**Step 5: Integrate into deliver_if_needed**

In `crates/clawhive-gateway/src/lib.rs`, update `deliver_if_needed` to replace the `tracing::warn!("Webhook delivery not yet implemented")` with actual webhook delivery:

```rust
ScheduledDeliveryMode::Webhook => {
    let Some(url) = &delivery.webhook_url else {
        tracing::warn!("Webhook delivery mode set but no webhook_url provided");
        return;
    };
    let now = chrono::Utc::now();
    let payload = webhook::WebhookPayload {
        schedule_id: "unknown".into(), // We don't have schedule_id in this scope
        status: "ok".into(),
        response: Some(text.to_string()),
        error: None,
        started_at: now,
        ended_at: now,
        duration_ms: 0,
    };
    if let Err(e) = webhook::deliver_webhook(url, &payload).await {
        tracing::warn!(url = %url, error = %e, "Webhook delivery failed");
    }
}
```

**NOTE**: The `deliver_if_needed` function signature is simple (`bus, delivery, text`) and doesn't carry `schedule_id` or timing info. For Phase 2 this is acceptable — the webhook payload will have `schedule_id: "unknown"` and timing set to current time. A future refinement can thread `schedule_id` and `started_at` through the call if needed. Alternatively, update the function signature — this is left to the implementer's judgment based on how invasive the change would be.

**Step 6: Run full build and tests**

Run: `cargo build && cargo test`
Expected: PASS

**Step 7: Commit**

```bash
git add crates/clawhive-gateway/src/webhook.rs crates/clawhive-gateway/src/lib.rs crates/clawhive-gateway/Cargo.toml
git commit -m "feat(gateway): implement webhook HTTP POST delivery with retry"
```

---

## Task 5: MessageTool for cross-channel agent messaging

Create a new tool that lets agents proactively send messages to any channel via `BusPublisher::publish(BusMessage::DeliverAnnounce { ... })`.

**Files:**
- Create: `crates/clawhive-core/src/message_tool.rs`
- Modify: `crates/clawhive-core/src/lib.rs` (add `pub mod message_tool;` and `pub use message_tool::*;`)
- Modify: `crates/clawhive-core/src/orchestrator.rs` (register MessageTool)

**Step 1: Write failing test**

Create `crates/clawhive-core/src/message_tool.rs` with the full implementation including tests:

```rust
use std::sync::Arc;

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use clawhive_bus::BusPublisher;
use clawhive_provider::ToolDef;
use clawhive_schema::BusMessage;
use serde::Deserialize;

use crate::tool::{ToolContext, ToolExecutor, ToolOutput};

pub const MESSAGE_TOOL_NAME: &str = "message";

pub struct MessageTool {
    bus: BusPublisher,
}

impl MessageTool {
    pub fn new(bus: BusPublisher) -> Self {
        Self { bus }
    }
}

#[derive(Debug, Deserialize)]
struct MessageInput {
    action: String,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    connector_id: Option<String>,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    message: Option<String>,
}

#[async_trait]
impl ToolExecutor for MessageTool {
    fn definition(&self) -> ToolDef {
        todo!()
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        todo!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clawhive_bus::EventBus;
    use clawhive_bus::Topic;

    #[tokio::test]
    async fn send_action_publishes_deliver_announce() {
        let bus = EventBus::new(16);
        let publisher = bus.publisher();
        let mut rx = bus.subscribe(Topic::DeliverAnnounce).await;

        let tool = MessageTool::new(publisher);
        let ctx = ToolContext::builtin();

        let result = tool
            .execute(
                serde_json::json!({
                    "action": "send",
                    "channel": "discord",
                    "connector_id": "dc_main",
                    "target": "guild:123:channel:456",
                    "message": "Hello from agent!"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!result.is_error);
        assert!(result.content.contains("discord"));

        let msg = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            rx.recv(),
        )
        .await
        .unwrap()
        .unwrap();

        match msg {
            BusMessage::DeliverAnnounce {
                channel_type,
                connector_id,
                conversation_scope,
                text,
            } => {
                assert_eq!(channel_type, "discord");
                assert_eq!(connector_id, "dc_main");
                assert_eq!(conversation_scope, "guild:123:channel:456");
                assert_eq!(text, "Hello from agent!");
            }
            _ => panic!("expected DeliverAnnounce"),
        }
    }

    #[tokio::test]
    async fn send_action_defaults_connector_id() {
        let bus = EventBus::new(16);
        let publisher = bus.publisher();
        let mut rx = bus.subscribe(Topic::DeliverAnnounce).await;

        let tool = MessageTool::new(publisher);
        let ctx = ToolContext::builtin();

        let result = tool
            .execute(
                serde_json::json!({
                    "action": "send",
                    "channel": "telegram",
                    "target": "chat:789",
                    "message": "Auto connector"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!result.is_error);

        let msg = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            rx.recv(),
        )
        .await
        .unwrap()
        .unwrap();

        match msg {
            BusMessage::DeliverAnnounce { connector_id, .. } => {
                assert_eq!(connector_id, "telegram_main");
            }
            _ => panic!("expected DeliverAnnounce"),
        }
    }

    #[tokio::test]
    async fn send_action_requires_channel() {
        let bus = EventBus::new(16);
        let publisher = bus.publisher();
        let tool = MessageTool::new(publisher);
        let ctx = ToolContext::builtin();

        let result = tool
            .execute(
                serde_json::json!({
                    "action": "send",
                    "target": "chat:789",
                    "message": "Missing channel"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("channel"));
    }

    #[tokio::test]
    async fn unknown_action_returns_error() {
        let bus = EventBus::new(16);
        let publisher = bus.publisher();
        let tool = MessageTool::new(publisher);
        let ctx = ToolContext::builtin();

        let result = tool
            .execute(
                serde_json::json!({
                    "action": "delete",
                    "channel": "discord",
                    "target": "chat:1",
                    "message": "nope"
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(result.is_error);
        assert!(result.content.contains("Unknown action"));
    }
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p clawhive-core send_action_publishes_deliver_announce`
Expected: FAIL — `todo!()` panics

**Step 3: Implement MessageTool**

Replace the `todo!()` in `definition()`:

```rust
fn definition(&self) -> ToolDef {
    ToolDef {
        name: MESSAGE_TOOL_NAME.to_string(),
        description: "Send messages to channels (Discord, Telegram, Slack, etc). \
            Use for proactive cross-channel messaging and notifications."
            .to_string(),
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["send"],
                    "description": "Action to perform"
                },
                "channel": {
                    "type": "string",
                    "description": "Channel type: discord, telegram, slack, whatsapp"
                },
                "connector_id": {
                    "type": "string",
                    "description": "Connector ID (defaults to {channel}_main if not specified)"
                },
                "target": {
                    "type": "string",
                    "description": "Target conversation scope (e.g. guild:123:channel:456, chat:789)"
                },
                "message": {
                    "type": "string",
                    "description": "Message text to send"
                }
            },
            "required": ["action", "channel", "target", "message"]
        }),
    }
}
```

Replace the `todo!()` in `execute()`:

```rust
async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
    let parsed: MessageInput = serde_json::from_value(input)
        .map_err(|e| anyhow!("invalid message tool input: {e}"))?;

    match parsed.action.as_str() {
        "send" => {
            let channel = parsed.channel
                .ok_or_else(|| anyhow!("channel is required"))?;
            let target = parsed.target
                .ok_or_else(|| anyhow!("target is required"))?;
            let message = parsed.message
                .ok_or_else(|| anyhow!("message is required"))?;

            let connector_id = parsed.connector_id
                .unwrap_or_else(|| format!("{}_main", channel));

            self.bus
                .publish(BusMessage::DeliverAnnounce {
                    channel_type: channel.clone(),
                    connector_id,
                    conversation_scope: target.clone(),
                    text: message,
                })
                .await
                .map_err(|e| anyhow!("failed to publish message: {e}"))?;

            Ok(ToolOutput {
                content: format!("Message sent to {channel}:{target}"),
                is_error: false,
            })
        }
        other => Ok(ToolOutput {
            content: format!("Unknown action: {other}"),
            is_error: true,
        }),
    }
}
```

**Step 4: Register module in lib.rs**

In `crates/clawhive-core/src/lib.rs`, add (in alphabetical order with existing modules):
```rust
pub mod message_tool;
```
And:
```rust
pub use message_tool::*;
```

**Step 5: Register MessageTool in orchestrator**

In `crates/clawhive-core/src/orchestrator.rs`, after the `ScheduleTool` registration (line ~171):
```rust
tool_registry.register(Box::new(ScheduleTool::new(schedule_manager)));
tool_registry.register(Box::new(crate::message_tool::MessageTool::new(bus_for_tools.clone())));
```

Note: `bus_for_tools` is already a `BusPublisher` clone created at line ~101. MessageTool takes `BusPublisher`, so this works directly. **No constructor signature change needed.**

**Step 6: Run tests to verify they pass**

Run: `cargo test -p clawhive-core send_action`
Expected: PASS

**Step 7: Run full build and all tests**

Run: `cargo build && cargo test`
Expected: PASS

**Step 8: Commit**

```bash
git add crates/clawhive-core/src/message_tool.rs crates/clawhive-core/src/lib.rs crates/clawhive-core/src/orchestrator.rs
git commit -m "feat(core): add MessageTool for cross-channel agent messaging"
```

---

## Task 6: Full integration verification

Verify the entire system builds, all tests pass, and clippy is clean.

**Step 1: Full build**

Run: `cargo build`
Expected: PASS, exit code 0

**Step 2: Full test suite**

Run: `cargo test`
Expected: PASS

**Step 3: Clippy**

Run: `cargo clippy -- -D warnings`
Expected: PASS (no warnings)

**Step 4: Verify no dangerous patterns in new code**

Grep for `panic!`, `unwrap()` in new/modified files (outside of tests):
```bash
grep -n 'panic!\|\.unwrap()' crates/clawhive-gateway/src/webhook.rs crates/clawhive-core/src/message_tool.rs | grep -v '#\[cfg(test)\]' | grep -v 'mod tests'
```

Review and fix any non-test instances.

**Step 5: Commit any fixes**

```bash
git add -A
git commit -m "chore: fix lint and integration issues from scheduler redesign phase 2"
```

---

## Implementation Order Summary

| Task | Description | Depends On |
|------|-------------|------------|
| 1 | DeliveryConfig extensions (webhook_url, failure_destination, best_effort) | — |
| 2 | Schedule Tool payload schema exposure | 1 (needs webhook_url in DeliveryInput/DeliveryConfig) |
| 3 | YAML config legacy migration (migrate_legacy) | 1 (needs payload field, which uses extended DeliveryConfig) |
| 4 | Webhook HTTP POST delivery | 1 (needs DeliveryMode::Webhook) |
| 5 | MessageTool for cross-channel messaging | — (independent) |
| 6 | Full integration verification | ALL |

Tasks 1 and 5 are independent and can run in parallel. Task 2 depends on Task 1. Task 3 depends on Task 1. Task 4 depends on Task 1. Task 6 is final verification.

**Recommended execution order:** 1 → (2, 3, 4 in parallel) → 5 → 6

Or with subagent-driven: 1 and 5 in parallel first, then 2/3/4 in sequence, then 6.

## Not Included (P2-P3 features)

- **Stagger/错峰**: schedule_id hash-based deterministic offset
- **Per-job fallback models**: `AgentTurn.fallbacks: Vec<String>`
- **Run log pruning**: SQLite `run_history` cleanup
- **Session cleanup**: Isolated session history pruning
- **Failure alerts**: Notify user after N consecutive failures
- **MessageTool security**: Permission checking (isolated session restrictions, allowlists, rate limiting) — marked for independent security review
