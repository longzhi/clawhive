use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use anyhow::Result;
use arc_swap::ArcSwap;
use clawhive_bus::BusPublisher;
use clawhive_core::{
    build_config_view, load_config, validate_config, ApprovalRegistry, ClawhiveConfig, ConfigDiff,
    ConfigView, Orchestrator,
};
use clawhive_memory::MemoryStore;

use crate::supervisor::ChannelSupervisor;

pub struct ReloadCoordinator {
    current_config: ArcSwap<ClawhiveConfig>,
    orchestrator: Arc<Orchestrator>,
    supervisor: Arc<tokio::sync::Mutex<ChannelSupervisor>>,
    apply_lock: tokio::sync::Mutex<()>,
    generation: AtomicU64,
    root: PathBuf,
    memory: Arc<MemoryStore>,
    publisher: BusPublisher,
    schedule_manager: Arc<clawhive_scheduler::ScheduleManager>,
    approval_registry: Option<Arc<ApprovalRegistry>>,
    loaded_hashes: std::sync::RwLock<std::collections::HashMap<String, u64>>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum ChannelChangeResult {
    Started { connector_id: String },
    Stopped { connector_id: String },
    Restarted { connector_id: String },
    Failed { connector_id: String, error: String },
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ReloadOutcome {
    pub generation: u64,
    pub config_view_applied: bool,
    pub channel_results: Vec<ChannelChangeResult>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ConfigStatus {
    pub running_generation: u64,
    pub has_pending_changes: bool,
    pub changed_files: Vec<String>,
}

impl ReloadOutcome {
    pub fn no_changes(generation: u64) -> Self {
        Self {
            generation,
            config_view_applied: false,
            channel_results: Vec::new(),
            warnings: Vec::new(),
        }
    }
}

impl ReloadCoordinator {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        initial_config: ClawhiveConfig,
        orchestrator: Arc<Orchestrator>,
        supervisor: Arc<tokio::sync::Mutex<ChannelSupervisor>>,
        root: PathBuf,
        memory: Arc<MemoryStore>,
        publisher: BusPublisher,
        schedule_manager: Arc<clawhive_scheduler::ScheduleManager>,
        approval_registry: Option<Arc<ApprovalRegistry>>,
    ) -> Self {
        let initial_generation = orchestrator.config_view().generation;

        let loaded_hashes = disk_file_hashes(&root.join("config"));

        Self {
            current_config: ArcSwap::from_pointee(initial_config),
            orchestrator,
            supervisor,
            apply_lock: tokio::sync::Mutex::new(()),
            generation: AtomicU64::new(initial_generation),
            root,
            memory,
            publisher,
            schedule_manager,
            approval_registry,
            loaded_hashes: std::sync::RwLock::new(loaded_hashes),
        }
    }

    pub async fn reload(&self) -> Result<ReloadOutcome> {
        let _guard = self.apply_lock.lock().await;

        let new_cfg = load_config(&self.root.join("config"))?;
        validate_config(&new_cfg)?;

        let old_cfg = self.current_config.load_full();
        let diff = ConfigDiff::between(old_cfg.as_ref(), &new_cfg);
        let current_gen = self.generation.load(Ordering::SeqCst);

        if diff.is_empty() {
            return Ok(ReloadOutcome::no_changes(current_gen));
        }

        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;

        let warnings = diff
            .requires_restart
            .iter()
            .map(|item| format!("{item} changed - requires restart"))
            .collect();

        let new_view = self.build_config_view(&new_cfg, generation).await?;

        if !diff.agents_added.is_empty() {
            self.orchestrator
                .ensure_workspaces_for(&new_cfg, &diff.agents_added);
        }

        self.orchestrator.apply_config_view(new_view);
        self.current_config.store(Arc::new(new_cfg.clone()));
        *self.loaded_hashes.write().unwrap() = disk_file_hashes(&self.root.join("config"));

        let channel_results = self.reconcile_channels(&new_cfg).await;

        tracing::info!(
            generation,
            agents_added = diff.agents_added.len(),
            agents_removed = diff.agents_removed.len(),
            agents_changed = diff.agents_changed.len(),
            routing_changed = diff.routing_changed,
            providers_changed = diff.providers_changed,
            "config reloaded"
        );

        Ok(ReloadOutcome {
            generation,
            config_view_applied: true,
            channel_results,
            warnings,
        })
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Compare on-disk config files against the running config snapshot.
    /// Returns which files differ (if any).
    pub fn config_status(&self) -> ConfigStatus {
        let generation = self.generation.load(Ordering::SeqCst);
        let config_dir = self.root.join("config");

        let running_hashes = self.loaded_hashes.read().unwrap();
        let disk_hashes = disk_file_hashes(&config_dir);

        let mut changed_files = Vec::new();
        let all_keys: BTreeSet<&String> = running_hashes.keys().chain(disk_hashes.keys()).collect();
        for key in all_keys {
            let running = running_hashes.get(key.as_str());
            let disk = disk_hashes.get(key.as_str());
            if running != disk {
                changed_files.push(key.to_string());
            }
        }

        ConfigStatus {
            running_generation: generation,
            has_pending_changes: !changed_files.is_empty(),
            changed_files,
        }
    }

    async fn build_config_view(
        &self,
        config: &ClawhiveConfig,
        generation: u64,
    ) -> Result<ConfigView> {
        Ok(build_config_view(
            config,
            generation,
            &self.root,
            &self.memory,
            &self.approval_registry,
            &self.publisher,
            Arc::clone(&self.schedule_manager),
        )
        .await)
    }

    async fn reconcile_channels(&self, new_cfg: &ClawhiveConfig) -> Vec<ChannelChangeResult> {
        let mut sup = self.supervisor.lock().await;
        let running = sup.running_connectors();
        let desired = extract_connectors(new_cfg);

        let mut results = Vec::new();

        for (id, (channel_type, config)) in &desired {
            if !running.contains_key(id.as_str()) {
                match sup.start(id.clone(), channel_type, config.clone()) {
                    Ok(()) => results.push(ChannelChangeResult::Started {
                        connector_id: id.clone(),
                    }),
                    Err(e) => results.push(ChannelChangeResult::Failed {
                        connector_id: id.clone(),
                        error: e.to_string(),
                    }),
                }
            }
        }

        let to_stop: Vec<String> = running
            .keys()
            .filter(|id| !desired.contains_key(id.as_str()))
            .cloned()
            .collect();
        for id in to_stop {
            sup.stop(&id).await;
            results.push(ChannelChangeResult::Stopped { connector_id: id });
        }

        for (id, (channel_type, config)) in &desired {
            if let Some(&old_hash) = running.get(id.as_str()) {
                let new_hash = crate::supervisor::config_hash_value(config);
                if old_hash != new_hash {
                    match sup.restart(id.clone(), channel_type, config.clone()).await {
                        Ok(()) => results.push(ChannelChangeResult::Restarted {
                            connector_id: id.clone(),
                        }),
                        Err(e) => results.push(ChannelChangeResult::Failed {
                            connector_id: id.clone(),
                            error: e.to_string(),
                        }),
                    }
                }
            }
        }

        results
    }
}

fn extract_connectors(
    config: &ClawhiveConfig,
) -> std::collections::HashMap<String, (String, serde_json::Value)> {
    let mut map = std::collections::HashMap::new();
    let channels = &config.main.channels;

    if let Some(tg) = &channels.telegram {
        if tg.enabled {
            for c in &tg.connectors {
                if let Ok(v) = serde_json::to_value(c) {
                    map.insert(c.connector_id.clone(), ("telegram".into(), v));
                }
            }
        }
    }
    if let Some(dc) = &channels.discord {
        if dc.enabled {
            for c in &dc.connectors {
                if let Ok(v) = serde_json::to_value(c) {
                    map.insert(c.connector_id.clone(), ("discord".into(), v));
                }
            }
        }
    }
    if let Some(fs) = &channels.feishu {
        if fs.enabled {
            for c in &fs.connectors {
                if let Ok(v) = serde_json::to_value(c) {
                    map.insert(c.connector_id.clone(), ("feishu".into(), v));
                }
            }
        }
    }
    if let Some(dt) = &channels.dingtalk {
        if dt.enabled {
            for c in &dt.connectors {
                if let Ok(v) = serde_json::to_value(c) {
                    map.insert(c.connector_id.clone(), ("dingtalk".into(), v));
                }
            }
        }
    }
    if let Some(wc) = &channels.wecom {
        if wc.enabled {
            for c in &wc.connectors {
                if let Ok(v) = serde_json::to_value(c) {
                    map.insert(c.connector_id.clone(), ("wecom".into(), v));
                }
            }
        }
    }
    map
}

fn disk_file_hashes(config_dir: &Path) -> std::collections::HashMap<String, u64> {
    use std::hash::{Hash, Hasher};
    let mut result = std::collections::HashMap::new();
    for subdir in &["", "agents.d", "providers.d"] {
        let dir = config_dir.join(subdir);
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("yaml") {
                if let Ok(content) = std::fs::read_to_string(&path) {
                    let rel = if subdir.is_empty() {
                        path.file_name().unwrap().to_string_lossy().to_string()
                    } else {
                        format!("{}/{}", subdir, path.file_name().unwrap().to_string_lossy())
                    };
                    let mut hasher = std::collections::hash_map::DefaultHasher::new();
                    content.hash(&mut hasher);
                    result.insert(rel, hasher.finish());
                }
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use clawhive_bus::EventBus;
    use clawhive_core::{
        build_config_view, load_config, ApprovalRegistry, ClawhiveConfig, OrchestratorBuilder,
    };
    use clawhive_memory::MemoryStore;
    use clawhive_runtime::NativeExecutor;
    use clawhive_scheduler::{ScheduleManager, SqliteStore};

    use super::{ChannelChangeResult, ReloadCoordinator};
    use crate::{Gateway, RateLimitConfig, RateLimiter};

    fn write_config(root: &std::path::Path, model: &str, log_level: &str) {
        std::fs::create_dir_all(root.join("config/agents.d")).unwrap();
        std::fs::create_dir_all(root.join("config/providers.d")).unwrap();
        std::fs::create_dir_all(root.join("data")).unwrap();
        std::fs::write(
            root.join("config/main.yaml"),
            format!(
                "app:\n  name: clawhive\nruntime:\n  max_concurrent: 4\nfeatures:\n  multi_agent: true\n  sub_agent: true\n  tui: false\n  cli: true\nchannels:\n  telegram: null\n  discord: null\nlog_level: {log_level}\n"
            ),
        )
        .unwrap();
        std::fs::write(
            root.join("config/routing.yaml"),
            "default_agent_id: agent-a\nbindings: []\n",
        )
        .unwrap();
        std::fs::write(
            root.join("config/providers.d/openai.yaml"),
            "provider_id: openai\nenabled: true\napi_base: https://api.openai.com/v1\napi_key: sk-test\nmodels:\n  - gpt-4o\n",
        )
        .unwrap();
        std::fs::write(
            root.join("config/agents.d/agent-a.yaml"),
            format!(
                "agent_id: agent-a\nenabled: true\nmodel_policy:\n  primary: {model}\n  fallbacks: []\n"
            ),
        )
        .unwrap();
    }

    async fn make_coordinator(
        root: &std::path::Path,
    ) -> (ReloadCoordinator, Arc<clawhive_core::Orchestrator>) {
        let config: ClawhiveConfig = load_config(&root.join("config")).unwrap();
        let bus = Arc::new(EventBus::new(16));
        let publisher = bus.publisher();
        let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
        let schedule_manager = Arc::new(
            ScheduleManager::new(
                SqliteStore::open(&root.join("data/scheduler.db")).unwrap(),
                Arc::clone(&bus),
            )
            .await
            .unwrap(),
        );
        let approval_registry = Some(Arc::new(ApprovalRegistry::with_persistence(
            root.join("data/runtime_allowlist.json"),
        )));
        let config_view = build_config_view(
            &config,
            0,
            root,
            &memory,
            &approval_registry,
            &publisher,
            Arc::clone(&schedule_manager),
        )
        .await;
        let orchestrator = Arc::new(
            OrchestratorBuilder::new(
                config_view,
                publisher.clone(),
                Arc::clone(&memory),
                Arc::new(NativeExecutor),
                root.to_path_buf(),
                Arc::clone(&schedule_manager),
            )
            .approval_registry(approval_registry.clone().unwrap())
            .project_root(root.to_path_buf())
            .build(),
        );
        let gateway = Arc::new(Gateway::new(
            Arc::clone(&orchestrator),
            publisher.clone(),
            RateLimiter::new(RateLimitConfig::default()),
            approval_registry.clone(),
        ));
        let supervisor = Arc::new(tokio::sync::Mutex::new(
            crate::supervisor::ChannelSupervisor::new(gateway, bus),
        ));

        (
            ReloadCoordinator::new(
                config,
                Arc::clone(&orchestrator),
                supervisor,
                root.to_path_buf(),
                memory,
                publisher,
                schedule_manager,
                approval_registry,
            ),
            orchestrator,
        )
    }

    #[tokio::test]
    async fn reload_reports_no_changes_when_config_matches_disk() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), "gpt-4o", "info");
        let (coordinator, _orchestrator) = make_coordinator(tmp.path()).await;

        let outcome = coordinator.reload().await.unwrap();

        assert_eq!(outcome.generation, 0);
        assert!(!outcome.config_view_applied);
        assert!(outcome.channel_results.is_empty());
        assert!(outcome.warnings.is_empty());
    }

    #[tokio::test]
    async fn reload_swaps_config_view_and_collects_warnings() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), "gpt-4o", "info");
        let (coordinator, orchestrator) = make_coordinator(tmp.path()).await;
        write_config(tmp.path(), "gpt-4.1", "debug");

        let outcome = coordinator.reload().await.unwrap();
        let current = orchestrator.config_view();

        assert_eq!(outcome.generation, 1);
        assert!(outcome.config_view_applied);
        assert_eq!(outcome.channel_results, Vec::<ChannelChangeResult>::new());
        assert!(outcome
            .warnings
            .iter()
            .any(|item| item.contains("requires restart")));
        assert_eq!(current.generation, 1);
        assert_eq!(
            current.agent("agent-a").unwrap().model_policy.primary,
            "gpt-4.1"
        );
    }
}
