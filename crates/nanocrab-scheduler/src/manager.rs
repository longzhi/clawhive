use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use nanocrab_bus::EventBus;
use nanocrab_schema::{
    BusMessage, ScheduledDeliveryMode, ScheduledRunStatus, ScheduledSessionMode,
};
use tokio::sync::RwLock;
use tokio::time::Duration;

use crate::{
    compute_next_run_at_ms, error_backoff_ms, DeliveryMode, HistoryStore, RunRecord, RunStatus,
    ScheduleConfig, ScheduleState, ScheduleType, SessionMode, StateStore,
};

const MAX_SLEEP_MS: u64 = 60_000;

pub struct ScheduleEntry {
    pub config: ScheduleConfig,
    pub state: ScheduleState,
}

pub struct ScheduleManager {
    entries: Arc<RwLock<HashMap<String, ScheduleEntry>>>,
    bus: Arc<EventBus>,
    config_dir: PathBuf,
    state_store: StateStore,
    history_store: HistoryStore,
}

#[derive(Debug, Clone)]
pub struct CompletedResult {
    pub status: RunStatus,
    pub error: Option<String>,
    pub started_at_ms: i64,
    pub ended_at_ms: i64,
    pub duration_ms: u64,
}

pub fn apply_job_result(entry: &mut ScheduleEntry, result: &CompletedResult) -> bool {
    let state = &mut entry.state;

    state.running_at_ms = None;
    state.last_run_at_ms = Some(result.started_at_ms);
    state.last_run_status = Some(result.status.clone());
    state.last_duration_ms = Some(result.duration_ms);
    state.last_error = result.error.clone();

    match result.status {
        RunStatus::Ok => state.consecutive_errors = 0,
        RunStatus::Error => {
            state.consecutive_errors = state.consecutive_errors.saturating_add(1);
        }
        RunStatus::Skipped => {}
    }

    let should_delete = matches!(entry.config.schedule, ScheduleType::At { .. })
        && entry.config.delete_after_run
        && matches!(result.status, RunStatus::Ok);

    if should_delete {
        return true;
    }

    if matches!(entry.config.schedule, ScheduleType::At { .. }) {
        entry.config.enabled = false;
        state.next_run_at_ms = None;
    } else if entry.config.enabled {
        if matches!(result.status, RunStatus::Error) {
            let backoff = error_backoff_ms(state.consecutive_errors) as i64;
            let normal_next = compute_next_run_at_ms(&entry.config.schedule, result.ended_at_ms)
                .ok()
                .flatten();
            let backoff_next = result.ended_at_ms + backoff;
            state.next_run_at_ms = Some(normal_next.map_or(backoff_next, |n| n.max(backoff_next)));
        } else {
            state.next_run_at_ms = compute_next_run_at_ms(&entry.config.schedule, result.ended_at_ms)
                .ok()
                .flatten();
        }
    } else {
        state.next_run_at_ms = None;
    }

    if state.next_run_at_ms.is_none() && state.consecutive_errors >= 3 {
        tracing::warn!(
            schedule_id = %entry.config.schedule_id,
            "Auto-disabling schedule after 3 consecutive errors"
        );
        entry.config.enabled = false;
    }

    false
}

impl ScheduleManager {
    pub fn new(config_dir: &Path, data_dir: &Path, bus: Arc<EventBus>) -> Result<Self> {
        let configs: Vec<ScheduleConfig> = read_yaml_dir(config_dir)?;
        let persisted_states = StateStore::new(data_dir).load()?;

        let mut entries = HashMap::new();
        let now_ms = Utc::now().timestamp_millis();

        for config in configs {
            let mut state = persisted_states
                .get(&config.schedule_id)
                .cloned()
                .unwrap_or_else(|| ScheduleState::new(&config.schedule_id));
            state.next_run_at_ms = if config.enabled {
                compute_next_run_at_ms(&config.schedule, now_ms)?
            } else {
                None
            };
            entries.insert(config.schedule_id.clone(), ScheduleEntry { config, state });
        }

        Ok(Self {
            entries: Arc::new(RwLock::new(entries)),
            bus,
            config_dir: config_dir.to_path_buf(),
            state_store: StateStore::new(data_dir),
            history_store: HistoryStore::new(data_dir),
        })
    }

    pub async fn run(&self) {
        let mut completion_rx = self
            .bus
            .subscribe(nanocrab_bus::Topic::ScheduledTaskCompleted)
            .await;

        loop {
            let sleep_ms = self.compute_sleep_ms().await;
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(sleep_ms)) => {
                    self.check_and_trigger().await;
                }
                maybe_msg = completion_rx.recv() => {
                    if let Some(BusMessage::ScheduledTaskCompleted { schedule_id, status, error, started_at, ended_at, .. }) = maybe_msg {
                        self.apply_completion(&schedule_id, status, error, started_at.timestamp_millis(), ended_at.timestamp_millis()).await;
                    }
                }
            }
        }
    }

    pub async fn list(&self) -> Vec<ScheduleStateView> {
        let entries = self.entries.read().await;
        entries
            .values()
            .map(|entry| ScheduleStateView {
                config: entry.config.clone(),
                state: entry.state.clone(),
            })
            .collect()
    }

    pub async fn get_next_run(&self, schedule_id: &str) -> Option<i64> {
        let entries = self.entries.read().await;
        entries.get(schedule_id).and_then(|entry| entry.state.next_run_at_ms)
    }

    pub async fn add_schedule(&self, config: ScheduleConfig) -> Result<()> {
        let now_ms = Utc::now().timestamp_millis();
        let next = if config.enabled {
            compute_next_run_at_ms(&config.schedule, now_ms)?
        } else {
            None
        };
        let mut state = ScheduleState::new(&config.schedule_id);
        state.next_run_at_ms = next;

        let yaml = serde_yaml::to_string(&config)?;
        tokio::fs::create_dir_all(&self.config_dir).await?;
        let path = self.config_dir.join(format!("{}.yaml", &config.schedule_id));
        tokio::fs::write(&path, yaml).await?;

        let mut entries = self.entries.write().await;
        entries.insert(config.schedule_id.clone(), ScheduleEntry { config, state });

        self.state_store.persist(&entries).await?;
        Ok(())
    }

    pub async fn remove_schedule(&self, schedule_id: &str) -> Result<()> {
        let mut entries = self.entries.write().await;
        entries.remove(schedule_id);

        let path = self.config_dir.join(format!("{}.yaml", schedule_id));
        if path.exists() {
            tokio::fs::remove_file(&path).await?;
        }

        self.state_store.persist(&entries).await?;
        Ok(())
    }

    async fn compute_sleep_ms(&self) -> u64 {
        let entries = self.entries.read().await;
        let now_ms = Utc::now().timestamp_millis();
        let soonest = entries
            .values()
            .filter(|entry| entry.config.enabled)
            .filter_map(|entry| entry.state.next_run_at_ms)
            .min();

        match soonest {
            Some(next) => ((next - now_ms).max(0) as u64).min(MAX_SLEEP_MS),
            None => MAX_SLEEP_MS,
        }
    }

    async fn check_and_trigger(&self) {
        let now_ms = Utc::now().timestamp_millis();
        let mut entries = self.entries.write().await;

        for entry in entries.values_mut() {
            if !entry.config.enabled || entry.state.running_at_ms.is_some() {
                continue;
            }

            let due = entry
                .state
                .next_run_at_ms
                .map(|next| next <= now_ms)
                .unwrap_or(false);

            if due {
                entry.state.running_at_ms = Some(now_ms);

                let _ = self
                    .bus
                    .publish(BusMessage::ScheduledTaskTriggered {
                        schedule_id: entry.config.schedule_id.clone(),
                        agent_id: entry.config.agent_id.clone(),
                        task: entry.config.task.clone(),
                        session_mode: match entry.config.session_mode {
                            SessionMode::Isolated => ScheduledSessionMode::Isolated,
                            SessionMode::Main => ScheduledSessionMode::Main,
                        },
                        delivery_mode: match entry.config.delivery.mode {
                            DeliveryMode::None => ScheduledDeliveryMode::None,
                            DeliveryMode::Announce => ScheduledDeliveryMode::Announce,
                        },
                        delivery_channel: entry.config.delivery.channel.clone(),
                        delivery_connector_id: entry.config.delivery.connector_id.clone(),
                        triggered_at: Utc::now(),
                    })
                    .await;
            }
        }

        let _ = self.bus.publisher();
        let _ = &self.history_store;
        let _ = self.state_store.persist(&entries).await;
    }

    async fn apply_completion(
        &self,
        schedule_id: &str,
        status: ScheduledRunStatus,
        error: Option<String>,
        started_at_ms: i64,
        ended_at_ms: i64,
    ) {
        let mut entries = self.entries.write().await;
        let Some(entry) = entries.get_mut(schedule_id) else {
            return;
        };

        let run_status = match status {
            ScheduledRunStatus::Ok => RunStatus::Ok,
            ScheduledRunStatus::Error => RunStatus::Error,
            ScheduledRunStatus::Skipped => RunStatus::Skipped,
        };
        let duration_ms = (ended_at_ms - started_at_ms).max(0) as u64;

        let should_delete = apply_job_result(
            entry,
            &CompletedResult {
                status: run_status.clone(),
                error,
                started_at_ms,
                ended_at_ms,
                duration_ms,
            },
        );

        if let (Some(started_at), Some(ended_at)) = (
            Utc.timestamp_millis_opt(started_at_ms).single(),
            Utc.timestamp_millis_opt(ended_at_ms).single(),
        ) {
            let _ = self
                .history_store
                .append(&RunRecord {
                    schedule_id: schedule_id.to_string(),
                    started_at,
                    ended_at,
                    status: run_status,
                    error: entry.state.last_error.clone(),
                    duration_ms,
                })
                .await;
        }

        if should_delete {
            entries.remove(schedule_id);
            let path = self.config_dir.join(format!("{}.yaml", schedule_id));
            if path.exists() {
                let _ = tokio::fs::remove_file(path).await;
            }
        }

        let _ = self.state_store.persist(&entries).await;
    }
}

#[derive(Debug, Clone)]
pub struct ScheduleStateView {
    pub config: ScheduleConfig,
    pub state: ScheduleState,
}

fn read_yaml_dir<T>(dir: &Path) -> Result<Vec<T>>
where
    T: for<'de> serde::Deserialize<'de>,
{
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut paths = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("failed to read {}", dir.display()))? {
        let entry = entry.with_context(|| format!("failed to read {}", dir.display()))?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) == Some("yaml") {
            paths.push(path);
        }
    }
    paths.sort();

    let mut items = Vec::with_capacity(paths.len());
    for path in paths {
        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let item = serde_yaml::from_str(&content)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        items.push(item);
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use chrono::Utc;
    use nanocrab_schema::ScheduledRunStatus;
    use uuid::Uuid;

    use super::{ScheduleEntry, ScheduleManager};
    use crate::{
        DeliveryConfig, HistoryStore, ScheduleConfig, ScheduleState, ScheduleType, SessionMode,
        StateStore,
    };

    fn test_dirs() -> (std::path::PathBuf, std::path::PathBuf) {
        let base = std::env::temp_dir().join(format!("nanocrab-scheduler-{}", Uuid::new_v4()));
        let config_dir = base.join("config");
        let data_dir = base.join("data");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::create_dir_all(&data_dir).unwrap();
        (config_dir, data_dir)
    }

    fn make_manager(entry: ScheduleEntry) -> ScheduleManager {
        let (config_dir, data_dir) = test_dirs();
        let mut entries = HashMap::new();
        entries.insert(entry.config.schedule_id.clone(), entry);

        ScheduleManager {
            entries: Arc::new(tokio::sync::RwLock::new(entries)),
            bus: Arc::new(nanocrab_bus::EventBus::new(16)),
            config_dir,
            state_store: StateStore::new(&data_dir),
            history_store: HistoryStore::new(&data_dir),
        }
    }

    fn make_entry(
        schedule_id: &str,
        schedule: ScheduleType,
        delete_after_run: bool,
        next_run_at_ms: Option<i64>,
    ) -> ScheduleEntry {
        ScheduleEntry {
            config: ScheduleConfig {
                schedule_id: schedule_id.to_string(),
                enabled: true,
                name: schedule_id.to_string(),
                description: None,
                schedule,
                agent_id: "nanocrab-main".to_string(),
                session_mode: SessionMode::Isolated,
                task: "test task".to_string(),
                timeout_seconds: 300,
                delete_after_run,
                delivery: DeliveryConfig::default(),
            },
            state: ScheduleState {
                schedule_id: schedule_id.to_string(),
                next_run_at_ms,
                running_at_ms: Some(Utc::now().timestamp_millis() - 1_000),
                last_run_at_ms: None,
                last_run_status: None,
                last_error: None,
                last_duration_ms: None,
                consecutive_errors: 0,
            },
        }
    }

    #[tokio::test]
    async fn one_time_delete_after_run_success_removes_entry() {
        let entry = make_entry(
            "one-shot",
            ScheduleType::At {
                at: "2099-01-01T00:00:00Z".to_string(),
            },
            true,
            Some(Utc::now().timestamp_millis() + 10_000),
        );
        let manager = make_manager(entry);
        let started = Utc::now().timestamp_millis() - 2_000;
        let ended = Utc::now().timestamp_millis();

        manager
            .apply_completion("one-shot", ScheduledRunStatus::Ok, None, started, ended)
            .await;

        let entries = manager.list().await;
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn error_completion_applies_exponential_backoff_floor() {
        let ended = Utc::now().timestamp_millis();
        let entry = make_entry(
            "retry-every-second",
            ScheduleType::Every {
                interval_ms: 1_000,
                anchor_ms: Some(0),
            },
            false,
            Some(ended + 1_000),
        );
        let manager = make_manager(entry);
        let started = ended - 1_500;

        manager
            .apply_completion(
                "retry-every-second",
                ScheduledRunStatus::Error,
                Some("network timeout".to_string()),
                started,
                ended,
            )
            .await;

        let entry = manager
            .list()
            .await
            .into_iter()
            .find(|item| item.config.schedule_id == "retry-every-second")
            .unwrap();

        assert_eq!(entry.state.consecutive_errors, 1);
        assert!(entry.state.next_run_at_ms.unwrap() >= ended + 30_000);
    }

    #[tokio::test]
    async fn three_consecutive_errors_without_next_run_auto_disables_schedule() {
        let mut entry = make_entry(
            "invalid-one-shot",
            ScheduleType::At {
                at: "2020-01-01T00:00:00Z".to_string(),
            },
            false,
            Some(Utc::now().timestamp_millis() + 1_000),
        );
        entry.state.consecutive_errors = 2;
        let manager = make_manager(entry);
        let started = Utc::now().timestamp_millis() - 1_000;
        let ended = Utc::now().timestamp_millis();

        manager
            .apply_completion(
                "invalid-one-shot",
                ScheduledRunStatus::Error,
                Some("execution failed".to_string()),
                started,
                ended,
            )
            .await;

        let entry = manager
            .list()
            .await
            .into_iter()
            .find(|item| item.config.schedule_id == "invalid-one-shot")
            .unwrap();

        assert_eq!(entry.state.consecutive_errors, 3);
        assert!(!entry.config.enabled);
    }
}
