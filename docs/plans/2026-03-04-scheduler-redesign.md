# Scheduler Redesign Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Replace ClawhHive scheduler's flat task+session_mode model with a typed TaskPayload system (SystemEvent/AgentTurn/DirectDeliver), add robust delivery pipeline, stuck-run recovery, and source identity preservation.

**Architecture:** The scheduler crate defines payload types and delivery config. The schema crate carries them over the EventBus. The gateway crate dispatches three execution paths based on payload kind. The schedule_tool captures source identity from ToolContext at job creation time.

**Tech Stack:** Rust, serde, tokio, SQLite (rusqlite), EventBus pub/sub, chrono

**Reference:** Design doc at `obsidian-vault/Projects/ClawhHive Scheduler 改造方案.md` (commit `2e9bcad`)

---

## Task 1: Add `source_user_scope` to ToolContext

Propagate `user_scope` from `InboundMessage` through the orchestrator into `ToolContext` so tools can capture the original user identity.

**Files:**
- Modify: `crates/clawhive-core/src/tool.rs`
- Modify: `crates/clawhive-core/src/orchestrator.rs`

**Step 1: Write failing test**

In `crates/clawhive-core/src/tool.rs`, add a test in the existing `mod tests` block:

```rust
#[test]
fn context_with_source_includes_user_scope() {
    let ctx = ToolContext::builtin()
        .with_source("telegram".into(), "tg_main".into(), "chat:123".into())
        .with_source_user_scope("user:456".into());
    assert_eq!(ctx.source_user_scope(), Some("user:456"));
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p clawhive-core context_with_source_includes_user_scope`
Expected: FAIL — `with_source_user_scope` and `source_user_scope()` don't exist

**Step 3: Add `source_user_scope` field to ToolContext**

In `crates/clawhive-core/src/tool.rs`:

1. Add field to `ToolContext` struct (after `source_conversation_scope`):
```rust
    /// Source user scope (e.g., "user:456") for preserving session key identity
    source_user_scope: Option<String>,
```

2. Initialize to `None` in ALL constructors (`builtin()`, `builtin_with_security()`, `builtin_with_security_and_private_overrides()`, `external()`, `external_with_security()`, `external_with_security_and_private_overrides()`).

3. Add builder method (after `with_session_key`):
```rust
    /// Set the source user scope.
    pub fn with_source_user_scope(mut self, user_scope: String) -> Self {
        self.source_user_scope = Some(user_scope);
        self
    }
```

4. Add accessor (after `source_conversation_scope()`):
```rust
    /// Get the source user scope.
    pub fn source_user_scope(&self) -> Option<&str> {
        self.source_user_scope.as_deref()
    }
```

**Step 4: Run test to verify it passes**

Run: `cargo test -p clawhive-core context_with_source_includes_user_scope`
Expected: PASS

**Step 5: Propagate user_scope in orchestrator**

In `crates/clawhive-core/src/orchestrator.rs`, find `source_info` construction (~line 910):

```rust
let source_info = Some((
    inbound.channel_type.clone(),
    inbound.connector_id.clone(),
    inbound.conversation_scope.clone(),
));
```

Change the tuple to include `user_scope`. This requires updating the `source_info` type from `Option<(String, String, String)>` to `Option<(String, String, String, String)>` throughout:

1. Update ALL `source_info` construction sites (grep for `source_info = Some(`) to include `inbound.user_scope.clone()` as the 4th element.

2. Update the `tool_use_loop` signature parameter type:
```rust
source_info: Option<(String, String, String, String)>, // (channel_type, connector_id, conversation_scope, user_scope)
```

3. Update the destructuring inside `tool_use_loop` where `with_source` is called (~line 1323):
```rust
let ctx = if let Some((ref ch, ref co, ref cv, ref us)) = source_info {
    ctx.with_source(ch.clone(), co.clone(), cv.clone())
       .with_source_user_scope(us.clone())
} else {
    ctx
};
```

**Step 6: Run all tests**

Run: `cargo test -p clawhive-core`
Expected: PASS (all existing tests should still pass)

**Step 7: Commit**

```bash
git add crates/clawhive-core/src/tool.rs crates/clawhive-core/src/orchestrator.rs
git commit -m "feat(core): add source_user_scope to ToolContext for session key identity preservation"
```

---

## Task 2: Add `TaskPayload` enum to scheduler config

Replace the flat `task: String` + `session_mode: SessionMode` with a typed `TaskPayload` enum, with backward compatibility.

**Files:**
- Modify: `crates/clawhive-scheduler/src/config.rs`

**Step 1: Write failing test**

In `crates/clawhive-scheduler/src/config.rs`, add tests module at the bottom:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_payload_serde_system_event() {
        let payload = TaskPayload::SystemEvent { text: "hello".into() };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("system_event"));
        let back: TaskPayload = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, TaskPayload::SystemEvent { text } if text == "hello"));
    }

    #[test]
    fn task_payload_serde_agent_turn() {
        let payload = TaskPayload::AgentTurn {
            message: "do task".into(),
            model: Some("anthropic/claude-opus-4".into()),
            thinking: None,
            timeout_seconds: 120,
            light_context: false,
        };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("agent_turn"));
        let back: TaskPayload = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, TaskPayload::AgentTurn { message, .. } if message == "do task"));
    }

    #[test]
    fn task_payload_serde_direct_deliver() {
        let payload = TaskPayload::DirectDeliver { text: "reminder".into() };
        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("direct_deliver"));
        let back: TaskPayload = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, TaskPayload::DirectDeliver { text } if text == "reminder"));
    }

    #[test]
    fn resolve_payload_prefers_explicit() {
        let payload = TaskPayload::DirectDeliver { text: "hi".into() };
        let result = resolve_payload(Some("old task".into()), Some(payload)).unwrap();
        assert!(matches!(result, TaskPayload::DirectDeliver { .. }));
    }

    #[test]
    fn resolve_payload_falls_back_to_task() {
        let result = resolve_payload(Some("old task".into()), None).unwrap();
        match result {
            TaskPayload::AgentTurn { message, timeout_seconds, .. } => {
                assert_eq!(message, "old task");
                assert_eq!(timeout_seconds, 300);
            }
            _ => panic!("expected AgentTurn"),
        }
    }

    #[test]
    fn resolve_payload_errors_when_both_none() {
        let result = resolve_payload(None, None);
        assert!(result.is_err());
    }

    #[test]
    fn delivery_config_serde_with_user_scope() {
        let config = DeliveryConfig {
            source_user_scope: Some("user:456".into()),
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("user:456"));
        let back: DeliveryConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.source_user_scope.as_deref(), Some("user:456"));
    }
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p clawhive-scheduler`
Expected: FAIL — `TaskPayload`, `resolve_payload`, `source_user_scope` field don't exist

**Step 3: Add TaskPayload enum**

In `crates/clawhive-scheduler/src/config.rs`, add after the `SessionMode` enum:

```rust
fn default_payload_timeout() -> u64 {
    300
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(tag = "kind")]
pub enum TaskPayload {
    /// Inject into the source channel's session, reusing the original conversation context.
    /// Agent processes it on next heartbeat or wake.
    #[serde(rename = "system_event")]
    SystemEvent {
        text: String,
    },
    /// Create an isolated session and run a full agent turn.
    #[serde(rename = "agent_turn")]
    AgentTurn {
        message: String,
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        thinking: Option<String>,
        #[serde(default = "default_payload_timeout")]
        timeout_seconds: u64,
        #[serde(default)]
        light_context: bool,
    },
    /// Deliver text directly without going through the agent. For simple reminders.
    #[serde(rename = "direct_deliver")]
    DirectDeliver {
        text: String,
    },
}

/// Resolve payload from either explicit payload or legacy task field.
pub fn resolve_payload(
    task: Option<String>,
    payload: Option<TaskPayload>,
) -> Result<TaskPayload, anyhow::Error> {
    if let Some(p) = payload {
        return Ok(p);
    }
    match task {
        Some(t) => Ok(TaskPayload::AgentTurn {
            message: t,
            model: None,
            thinking: None,
            timeout_seconds: 300,
            light_context: false,
        }),
        None => Err(anyhow::anyhow!("either task or payload must be provided")),
    }
}
```

**Step 4: Add `source_user_scope` to DeliveryConfig**

In `DeliveryConfig` struct, add after `source_conversation_scope`:

```rust
    /// Source user scope for preserving session key identity in SystemEvent execution
    #[serde(default)]
    pub source_user_scope: Option<String>,
```

Update `Default` impl for `DeliveryConfig` to include `source_user_scope: None`.

**Step 5: Run tests to verify they pass**

Run: `cargo test -p clawhive-scheduler`
Expected: PASS

**Step 6: Commit**

```bash
git add crates/clawhive-scheduler/src/config.rs
git commit -m "feat(scheduler): add TaskPayload enum and source_user_scope to DeliveryConfig"
```

---

## Task 3: Add `DeliveryStatus` to scheduler state

Track delivery outcomes in `ScheduleState`.

**Files:**
- Modify: `crates/clawhive-scheduler/src/state.rs`

**Step 1: Write failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_status_serde_roundtrip() {
        let state = ScheduleState {
            schedule_id: "test".into(),
            next_run_at_ms: None,
            running_at_ms: None,
            last_run_at_ms: None,
            last_run_status: None,
            last_error: None,
            last_duration_ms: None,
            consecutive_errors: 0,
            last_delivery_status: Some(DeliveryStatus::Delivered),
            last_delivery_error: None,
        };
        let json = serde_json::to_string(&state).unwrap();
        let back: ScheduleState = serde_json::from_str(&json).unwrap();
        assert_eq!(back.last_delivery_status, Some(DeliveryStatus::Delivered));
    }
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p clawhive-scheduler delivery_status_serde_roundtrip`
Expected: FAIL — fields and enum don't exist

**Step 3: Add DeliveryStatus and fields**

In `crates/clawhive-scheduler/src/state.rs`:

1. Add `DeliveryStatus` enum after `RunStatus`:
```rust
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub enum DeliveryStatus {
    #[serde(rename = "delivered")]
    Delivered,
    #[serde(rename = "not_delivered")]
    NotDelivered,
    #[serde(rename = "not_requested")]
    NotRequested,
}
```

2. Add fields to `ScheduleState` (with `#[serde(default)]` for backward compat):
```rust
    #[serde(default)]
    pub last_delivery_status: Option<DeliveryStatus>,
    #[serde(default)]
    pub last_delivery_error: Option<String>,
```

3. Update `ScheduleState::new()` to initialize them to `None`.

**Step 4: Run test to verify it passes**

Run: `cargo test -p clawhive-scheduler delivery_status_serde_roundtrip`
Expected: PASS

**Step 5: Commit**

```bash
git add crates/clawhive-scheduler/src/state.rs
git commit -m "feat(scheduler): add DeliveryStatus tracking to ScheduleState"
```

---

## Task 4: Stuck run detection and startup cleanup

Prevent schedules from getting permanently stuck when the process crashes mid-execution.

**Files:**
- Modify: `crates/clawhive-scheduler/src/manager.rs`

**Step 1: Write failing test**

Add to existing `mod tests` in `manager.rs`:

```rust
#[tokio::test]
async fn stuck_running_marker_is_cleared_on_startup() {
    let (config_dir, data_dir) = test_dirs();
    let bus = Arc::new(clawhive_bus::EventBus::new(16));

    // Create a config file with a stuck schedule
    let config = ScheduleConfig {
        schedule_id: "stuck-job".to_string(),
        enabled: true,
        name: "Stuck Job".to_string(),
        description: None,
        schedule: ScheduleType::Every { interval_ms: 60_000, anchor_ms: None },
        agent_id: "clawhive-main".to_string(),
        session_mode: SessionMode::Isolated,
        task: "stuck task".to_string(),
        timeout_seconds: 300,
        delete_after_run: false,
        delivery: DeliveryConfig::default(),
    };
    std::fs::write(
        config_dir.join("stuck-job.yaml"),
        serde_yaml::to_string(&config).unwrap(),
    ).unwrap();

    // Create persisted state with running_at_ms set (simulating crash)
    let mut states = HashMap::new();
    states.insert("stuck-job".to_string(), ScheduleState {
        schedule_id: "stuck-job".to_string(),
        next_run_at_ms: Some(Utc::now().timestamp_millis() + 60_000),
        running_at_ms: Some(Utc::now().timestamp_millis() - 10_000),
        last_run_at_ms: None,
        last_run_status: None,
        last_error: None,
        last_duration_ms: None,
        consecutive_errors: 0,
    });
    let state_json = serde_json::to_string_pretty(&states).unwrap();
    std::fs::create_dir_all(&data_dir).unwrap();
    std::fs::write(data_dir.join("state.json"), state_json).unwrap();

    // ScheduleManager::new should clear the stuck running marker
    let manager = ScheduleManager::new(&config_dir, &data_dir, bus).unwrap();
    let list = manager.list().await;
    assert_eq!(list.len(), 1);
    assert!(list[0].state.running_at_ms.is_none(), "running_at_ms should be cleared on startup");
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p clawhive-scheduler stuck_running_marker_is_cleared_on_startup`
Expected: FAIL — `running_at_ms` is NOT cleared

**Step 3: Add startup cleanup in `ScheduleManager::new()`**

In `crates/clawhive-scheduler/src/manager.rs`, inside `ScheduleManager::new()`, after the `for config in configs` loop builds `entries`, add:

```rust
        // Clear stale running markers from process crash
        for entry in entries.values_mut() {
            if entry.state.running_at_ms.is_some() {
                tracing::warn!(
                    schedule_id = %entry.config.schedule_id,
                    "Clearing stale running marker on startup"
                );
                entry.state.running_at_ms = None;
            }
        }
```

**Step 4: Add stuck run detection in `check_and_trigger()`**

In `check_and_trigger()`, at the start of the `for entry in entries.values_mut()` loop, before the `if !entry.config.enabled` check, add:

```rust
            // Clear stuck running markers (>2 hours)
            const STUCK_RUN_MS: i64 = 2 * 60 * 60 * 1000;
            if let Some(running_at) = entry.state.running_at_ms {
                if now_ms - running_at > STUCK_RUN_MS {
                    tracing::warn!(
                        schedule_id = %entry.config.schedule_id,
                        running_at_ms = running_at,
                        "Clearing stuck running marker"
                    );
                    entry.state.running_at_ms = None;
                }
            }
```

**Step 5: Run test to verify it passes**

Run: `cargo test -p clawhive-scheduler stuck_running_marker_is_cleared_on_startup`
Expected: PASS

**Step 6: Run all scheduler tests**

Run: `cargo test -p clawhive-scheduler`
Expected: PASS

**Step 7: Commit**

```bash
git add crates/clawhive-scheduler/src/manager.rs
git commit -m "fix(scheduler): clear stuck running markers on startup and after 2h timeout"
```

---

## Task 5: Update schema BusMessage types

Replace flat fields in `ScheduledTaskTriggered` with structured `ScheduledTaskPayload` + `ScheduledDeliveryInfo`, and add `Webhook` delivery mode.

**Files:**
- Modify: `crates/clawhive-schema/src/lib.rs`

**Step 1: Write failing test**

Add to the existing `mod tests` in schema `lib.rs`:

```rust
#[test]
fn scheduled_task_payload_serde_roundtrip() {
    let payload = ScheduledTaskPayload::AgentTurn {
        message: "do task".into(),
        model: None,
        thinking: None,
        timeout_seconds: 300,
        light_context: false,
    };
    let json = serde_json::to_string(&payload).unwrap();
    let back: ScheduledTaskPayload = serde_json::from_str(&json).unwrap();
    assert!(matches!(back, ScheduledTaskPayload::AgentTurn { message, .. } if message == "do task"));
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p clawhive-schema scheduled_task_payload_serde_roundtrip`
Expected: FAIL — `ScheduledTaskPayload` doesn't exist

**Step 3: Add new types, update BusMessage**

In `crates/clawhive-schema/src/lib.rs`:

1. Replace `ScheduledSessionMode` with `ScheduledTaskPayload`:
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum ScheduledTaskPayload {
    #[serde(rename = "system_event")]
    SystemEvent { text: String },
    #[serde(rename = "agent_turn")]
    AgentTurn {
        message: String,
        model: Option<String>,
        thinking: Option<String>,
        timeout_seconds: u64,
        light_context: bool,
    },
    #[serde(rename = "direct_deliver")]
    DirectDeliver { text: String },
}
```

2. Add `ScheduledDeliveryInfo`:
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledDeliveryInfo {
    pub mode: ScheduledDeliveryMode,
    pub channel: Option<String>,
    pub connector_id: Option<String>,
    pub source_channel_type: Option<String>,
    pub source_connector_id: Option<String>,
    pub source_conversation_scope: Option<String>,
    pub source_user_scope: Option<String>,
    pub webhook_url: Option<String>,
}
```

3. Add `Webhook` variant to `ScheduledDeliveryMode`:
```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ScheduledDeliveryMode {
    #[serde(rename = "none")]
    None,
    #[serde(rename = "announce")]
    Announce,
    #[serde(rename = "webhook")]
    Webhook,
}
```

4. Update `BusMessage::ScheduledTaskTriggered` to use the new types:
```rust
    ScheduledTaskTriggered {
        schedule_id: String,
        agent_id: String,
        payload: ScheduledTaskPayload,
        delivery: ScheduledDeliveryInfo,
        triggered_at: DateTime<Utc>,
    },
```

5. Remove `ScheduledSessionMode` enum entirely (no longer used).

**Step 4: Fix all compilation errors**

The `ScheduledTaskTriggered` change will break:
- `crates/clawhive-scheduler/src/manager.rs` — `trigger_now()` and `check_and_trigger()` construct this message
- `crates/clawhive-gateway/src/lib.rs` — `spawn_scheduled_task_listener` destructures this message
- Any test files referencing old fields

Fix `manager.rs` first — update `trigger_now()` and `check_and_trigger()` to construct the new message format:

```rust
let msg = BusMessage::ScheduledTaskTriggered {
    schedule_id: entry.config.schedule_id.clone(),
    agent_id: entry.config.agent_id.clone(),
    payload: ScheduledTaskPayload::AgentTurn {
        message: entry.config.task.clone(),
        model: None,
        thinking: None,
        timeout_seconds: entry.config.timeout_seconds,
        light_context: false,
    },
    delivery: ScheduledDeliveryInfo {
        mode: match entry.config.delivery.mode {
            DeliveryMode::None => ScheduledDeliveryMode::None,
            DeliveryMode::Announce => ScheduledDeliveryMode::Announce,
        },
        channel: entry.config.delivery.channel.clone(),
        connector_id: entry.config.delivery.connector_id.clone(),
        source_channel_type: entry.config.delivery.source_channel_type.clone(),
        source_connector_id: entry.config.delivery.source_connector_id.clone(),
        source_conversation_scope: entry.config.delivery.source_conversation_scope.clone(),
        source_user_scope: entry.config.delivery.source_user_scope.clone(),
        webhook_url: None,
    },
    triggered_at: Utc::now(),
};
```

Update the `use` imports in `manager.rs` to use the new types instead of `ScheduledSessionMode`.

For `gateway/src/lib.rs`, temporarily update destructuring to match new shape (full rewrite happens in Task 6).

**Step 5: Run full build**

Run: `cargo build`
Expected: PASS (no compile errors)

**Step 6: Run all tests**

Run: `cargo test`
Expected: PASS

**Step 7: Commit**

```bash
git add crates/clawhive-schema/src/lib.rs crates/clawhive-scheduler/src/manager.rs crates/clawhive-gateway/src/lib.rs
git commit -m "feat(schema): replace flat ScheduledTaskTriggered fields with ScheduledTaskPayload + ScheduledDeliveryInfo"
```

---

## Task 6: Rewrite gateway three-way dispatch

Replace the current two-path logic in `spawn_scheduled_task_listener` with the three-way dispatch: SystemEvent → source session injection, AgentTurn → isolated session + timeout, DirectDeliver → immediate delivery.

**Files:**
- Modify: `crates/clawhive-gateway/src/lib.rs`

**Step 1: Write failing test for DirectDeliver path**

Add test in gateway's existing `mod tests`:

```rust
#[tokio::test]
async fn scheduled_direct_deliver_publishes_announce() {
    // Setup gateway with mock agent...
    // Publish ScheduledTaskTriggered with DirectDeliver payload
    // Assert DeliverAnnounce is published with "⏰ " prefix
    // Assert ScheduledTaskCompleted with Ok status
}
```

(The exact test setup depends on the existing gateway test infrastructure — follow the patterns in the existing tests like `handle_inbound_produces_valid_output`.)

**Step 2: Rewrite `spawn_scheduled_task_listener`**

Replace the entire function body. The new logic:

```rust
pub fn spawn_scheduled_task_listener(
    gateway: Arc<Gateway>,
    bus: Arc<EventBus>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut rx = bus.subscribe(Topic::ScheduledTaskTriggered).await;
        while let Some(msg) = rx.recv().await {
            let BusMessage::ScheduledTaskTriggered {
                schedule_id,
                agent_id: _,
                payload,
                delivery,
                triggered_at,
            } = msg
            else {
                continue;
            };

            match payload {
                // ─── Path 1: Direct Deliver ───
                ScheduledTaskPayload::DirectDeliver { text } => {
                    deliver_if_needed(&bus, &delivery, &format!("⏰ {}", text)).await;
                    let _ = bus.publish(BusMessage::ScheduledTaskCompleted {
                        schedule_id,
                        status: ScheduledRunStatus::Ok,
                        error: None,
                        started_at: triggered_at,
                        ended_at: chrono::Utc::now(),
                        response: Some(text),
                    }).await;
                }

                // ─── Path 2: SystemEvent ───
                ScheduledTaskPayload::SystemEvent { text } => {
                    let (ch_type, conn_id, conv_scope) = match (
                        &delivery.source_channel_type,
                        &delivery.source_connector_id,
                        &delivery.source_conversation_scope,
                    ) {
                        (Some(ct), Some(ci), Some(cs)) => (ct.clone(), ci.clone(), cs.clone()),
                        _ => {
                            // Fallback: deliver directly if source scope missing
                            tracing::warn!(schedule_id = %schedule_id, "SystemEvent missing source scope, falling back to DirectDeliver");
                            deliver_if_needed(&bus, &delivery, &format!("⏰ {}", text)).await;
                            let _ = bus.publish(BusMessage::ScheduledTaskCompleted {
                                schedule_id, status: ScheduledRunStatus::Ok, error: None,
                                started_at: triggered_at, ended_at: chrono::Utc::now(),
                                response: Some(text),
                            }).await;
                            continue;
                        }
                    };

                    let user_scope = delivery.source_user_scope.clone()
                        .unwrap_or_else(|| "user:scheduler".into());

                    let inbound = InboundMessage {
                        trace_id: Uuid::new_v4(),
                        channel_type: ch_type,
                        connector_id: conn_id,
                        conversation_scope: conv_scope,
                        user_scope,
                        text: format!("[Scheduled Reminder]\n{}", text),
                        at: triggered_at,
                        thread_id: None,
                        is_mention: false,
                        mention_target: None,
                        message_id: None,
                        attachments: vec![],
                        group_context: None,
                    };

                    match gateway.handle_inbound(inbound).await {
                        Ok(_outbound) => {
                            let _ = bus.publish(BusMessage::ScheduledTaskCompleted {
                                schedule_id, status: ScheduledRunStatus::Ok, error: None,
                                started_at: triggered_at, ended_at: chrono::Utc::now(),
                                response: None,
                            }).await;
                        }
                        Err(e) => {
                            let _ = bus.publish(BusMessage::ScheduledTaskCompleted {
                                schedule_id, status: ScheduledRunStatus::Error,
                                error: Some(e.to_string()),
                                started_at: triggered_at, ended_at: chrono::Utc::now(),
                                response: None,
                            }).await;
                        }
                    }
                }

                // ─── Path 3: Agent Turn (isolated) ───
                ScheduledTaskPayload::AgentTurn {
                    message, model: _, thinking: _, timeout_seconds, light_context: _,
                } => {
                    let conversation_scope = format!("schedule:{}:{}", schedule_id, Uuid::new_v4());

                    let inbound = InboundMessage {
                        trace_id: Uuid::new_v4(),
                        channel_type: "scheduler".into(),
                        connector_id: schedule_id.clone(),
                        conversation_scope,
                        user_scope: "user:scheduler".into(),
                        text: message,
                        at: triggered_at,
                        thread_id: None,
                        is_mention: false,
                        mention_target: None,
                        message_id: None,
                        attachments: vec![],
                        group_context: None,
                    };

                    let effective_timeout = timeout_seconds.max(30).min(3600);
                    let result = tokio::time::timeout(
                        std::time::Duration::from_secs(effective_timeout),
                        gateway.handle_inbound(inbound),
                    ).await;

                    match result {
                        Ok(Ok(outbound)) => {
                            deliver_if_needed(&bus, &delivery, &outbound.text).await;
                            let _ = bus.publish(BusMessage::ScheduledTaskCompleted {
                                schedule_id, status: ScheduledRunStatus::Ok, error: None,
                                started_at: triggered_at, ended_at: chrono::Utc::now(),
                                response: Some(outbound.text),
                            }).await;
                        }
                        Ok(Err(e)) => {
                            let _ = bus.publish(BusMessage::ScheduledTaskCompleted {
                                schedule_id, status: ScheduledRunStatus::Error,
                                error: Some(e.to_string()),
                                started_at: triggered_at, ended_at: chrono::Utc::now(),
                                response: None,
                            }).await;
                        }
                        Err(_) => {
                            let _ = bus.publish(BusMessage::ScheduledTaskCompleted {
                                schedule_id, status: ScheduledRunStatus::Error,
                                error: Some(format!("execution timed out after {}s", effective_timeout)),
                                started_at: triggered_at, ended_at: chrono::Utc::now(),
                                response: None,
                            }).await;
                        }
                    }
                }
            }
        }
    })
}
```

**Step 3: Add `deliver_if_needed` helper**

```rust
async fn deliver_if_needed(
    bus: &Arc<EventBus>,
    delivery: &ScheduledDeliveryInfo,
    text: &str,
) {
    match delivery.mode {
        ScheduledDeliveryMode::None => {}
        ScheduledDeliveryMode::Announce => {
            if let (Some(ch), Some(conn), Some(scope)) = (
                &delivery.source_channel_type,
                &delivery.source_connector_id,
                &delivery.source_conversation_scope,
            ) {
                let _ = bus.publish(BusMessage::DeliverAnnounce {
                    channel_type: ch.clone(),
                    connector_id: conn.clone(),
                    conversation_scope: scope.clone(),
                    text: text.to_string(),
                }).await;
            }
        }
        ScheduledDeliveryMode::Webhook => {
            // Webhook delivery will be implemented in a later task
            tracing::warn!("Webhook delivery not yet implemented");
        }
    }
}
```

**Step 4: Run all tests**

Run: `cargo test`
Expected: PASS

**Step 5: Commit**

```bash
git add crates/clawhive-gateway/src/lib.rs
git commit -m "feat(gateway): rewrite spawn_scheduled_task_listener with three-way dispatch"
```

---

## Task 7: Update schedule_tool for new payload + source_user_scope

Adapt the schedule tool to construct `TaskPayload` and capture `source_user_scope`.

**Files:**
- Modify: `crates/clawhive-core/src/schedule_tool.rs`

**Step 1: Write failing test**

Add to existing `mod tests` in `schedule_tool.rs`:

```rust
#[tokio::test]
async fn add_action_captures_source_user_scope() {
    let (manager, _bus, _tmp) = setup();
    let tool = ScheduleTool::new(manager.clone());
    let ctx = ToolContext::builtin()
        .with_source("discord".into(), "dc_main".into(), "guild:1:channel:2".into())
        .with_source_user_scope("user:789".into());

    let result = tool.execute(serde_json::json!({
        "action": "add",
        "job": {
            "name": "User scope test",
            "schedule": { "kind": "at", "at": "5m" },
            "task": "test task"
        }
    }), &ctx).await.unwrap();

    assert!(!result.is_error);
    let entries = manager.list().await;
    assert_eq!(entries[0].config.delivery.source_user_scope.as_deref(), Some("user:789"));
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p clawhive-core add_action_captures_source_user_scope`
Expected: FAIL

**Step 3: Update `into_config` to capture source_user_scope**

In `schedule_tool.rs`, in `ScheduleJobInput::into_config()`, update the `delivery: DeliveryConfig` construction:

```rust
            delivery: DeliveryConfig {
                mode: delivery_mode,
                channel: delivery.channel,
                connector_id: delivery.connector_id,
                source_channel_type: ctx.source_channel_type().map(String::from),
                source_connector_id: ctx.source_connector_id().map(String::from),
                source_conversation_scope: ctx.source_conversation_scope().map(String::from),
                source_user_scope: ctx.source_user_scope().map(String::from),
            },
```

**Step 4: Run test to verify it passes**

Run: `cargo test -p clawhive-core add_action_captures_source_user_scope`
Expected: PASS

**Step 5: Run all tests**

Run: `cargo test`
Expected: PASS

**Step 6: Commit**

```bash
git add crates/clawhive-core/src/schedule_tool.rs
git commit -m "feat(schedule-tool): capture source_user_scope from ToolContext in delivery config"
```

---

## Task 8: SQLite migration v3 for delivery status

Add columns for delivery status tracking.

**Files:**
- Modify: `crates/clawhive-scheduler/src/sqlite_store.rs`

**Step 1: Write failing test**

Add to existing `mod tests` in `sqlite_store.rs`:

```rust
#[tokio::test]
async fn test_schedule_state_with_delivery_fields() {
    let tmp = TempDir::new().unwrap();
    let store = SqliteStore::open(&tmp.path().join("test.db")).unwrap();

    let state = ScheduleState {
        schedule_id: "test-delivery".into(),
        next_run_at_ms: None,
        running_at_ms: None,
        last_run_at_ms: None,
        last_run_status: None,
        last_error: None,
        last_duration_ms: None,
        consecutive_errors: 0,
        last_delivery_status: Some(DeliveryStatus::Delivered),
        last_delivery_error: None,
    };

    store.save_schedule_state(&state).await.unwrap();
    let loaded = store.load_schedule_states().await.unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].last_delivery_status, Some(DeliveryStatus::Delivered));
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p clawhive-scheduler test_schedule_state_with_delivery_fields`
Expected: FAIL — columns don't exist in SQLite

**Step 3: Add migration v3 and update queries**

In `sqlite_store.rs`:

1. Add migration v3 to the `migrations` vec:
```rust
        (
            3,
            r#"
            ALTER TABLE schedule_states ADD COLUMN last_delivery_status TEXT;
            ALTER TABLE schedule_states ADD COLUMN last_delivery_error TEXT;
            "#,
        ),
```

2. Update `load_schedule_states` SELECT to include new columns:
```sql
SELECT schedule_id, next_run_at_ms, running_at_ms, last_run_at_ms,
       last_run_status, last_error, last_duration_ms, consecutive_errors,
       last_delivery_status, last_delivery_error
FROM schedule_states
```

3. Update the `query_map` closure to read new columns.

4. Update `save_schedule_state` INSERT to include new columns.

**Step 4: Run test to verify it passes**

Run: `cargo test -p clawhive-scheduler test_schedule_state_with_delivery_fields`
Expected: PASS

**Step 5: Run all tests**

Run: `cargo test -p clawhive-scheduler`
Expected: PASS

**Step 6: Commit**

```bash
git add crates/clawhive-scheduler/src/sqlite_store.rs
git commit -m "feat(scheduler): add SQLite migration v3 for delivery status tracking"
```

---

## Task 9: Full integration verification

Verify the entire system builds and all tests pass end-to-end.

**Step 1: Full build**

Run: `cargo build`
Expected: PASS, exit code 0

**Step 2: Full test suite**

Run: `cargo test`
Expected: PASS

**Step 3: Clippy**

Run: `cargo clippy -- -D warnings`
Expected: PASS (no warnings)

**Step 4: Verify no `as any` / `unwrap` / `panic!` in new code**

Grep for dangerous patterns in changed files:
```bash
grep -n 'panic!\|\.unwrap()' crates/clawhive-scheduler/src/config.rs crates/clawhive-scheduler/src/state.rs crates/clawhive-scheduler/src/manager.rs crates/clawhive-gateway/src/lib.rs crates/clawhive-core/src/tool.rs crates/clawhive-core/src/schedule_tool.rs
```

Review and fix any new instances (existing `unwrap()` in tests is OK).

**Step 5: Commit any fixes**

```bash
git add -A
git commit -m "chore: fix lint and integration issues from scheduler redesign"
```

---

## Implementation Order Summary

| Task | Description | Depends On |
|------|-------------|------------|
| 1 | `source_user_scope` in ToolContext | — |
| 2 | `TaskPayload` enum in scheduler config | — |
| 3 | `DeliveryStatus` in scheduler state | — |
| 4 | Stuck run detection + startup cleanup | — |
| 5 | Update schema BusMessage types | 2 |
| 6 | Gateway three-way dispatch | 5 |
| 7 | Update schedule_tool | 1, 2 |
| 8 | SQLite migration v3 | 3 |
| 9 | Full integration verification | ALL |

Tasks 1-4 are independent and can be done in parallel. Task 5 depends on Task 2. Task 6 depends on Task 5. Task 7 depends on Tasks 1+2. Task 8 depends on Task 3. Task 9 is the final verification.

## Not Included (separate PRs)

- **Phase 6: MessageTool** — orthogonal feature, needs separate security review
- **Phase 7: Webhook delivery logic** — type infrastructure added here, actual HTTP POST implementation deferred
- **Phase 8: Stagger, fallback models, run log pruning** — P2-P3 features
