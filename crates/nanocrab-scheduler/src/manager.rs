use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;
use nanocrab_bus::EventBus;
use tokio::sync::RwLock;
use tokio::time::Duration;

use crate::{compute_next_run_at_ms, ScheduleConfig, ScheduleState};

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
        loop {
            let sleep_ms = self.compute_sleep_ms().await;
            tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
            self.check_and_trigger().await;
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
            }
        }

        let _ = self.bus.publisher();
        let _ = &self.history_store;
        let _ = self.state_store.persist(&entries).await;
    }
}

#[derive(Debug, Clone)]
pub struct ScheduleStateView {
    pub config: ScheduleConfig,
    pub state: ScheduleState,
}

struct StateStore {
    _path: PathBuf,
}

impl StateStore {
    fn new(data_dir: &Path) -> Self {
        Self {
            _path: data_dir.join("state.json"),
        }
    }

    fn load(&self) -> Result<HashMap<String, ScheduleState>> {
        Ok(HashMap::new())
    }

    async fn persist(&self, _entries: &HashMap<String, ScheduleEntry>) -> Result<()> {
        Ok(())
    }
}

struct HistoryStore {
    _dir: PathBuf,
}

impl HistoryStore {
    fn new(data_dir: &Path) -> Self {
        Self {
            _dir: data_dir.join("runs"),
        }
    }
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
