use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use clawhive_bus::EventBus;
use serde_json::Value;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::Gateway;

type BotTask = Pin<Box<dyn Future<Output = Result<()>> + Send + 'static>>;
pub type BotFactory =
    Arc<dyn Fn(&str, &Value, Arc<Gateway>, Arc<EventBus>) -> Result<BotTask> + Send + Sync>;

pub struct ChannelSupervisor {
    bots: HashMap<String, BotEntry>,
    gateway: Arc<Gateway>,
    bus: Arc<EventBus>,
    global_token: CancellationToken,
    bot_factory: BotFactory,
}

struct BotEntry {
    token: CancellationToken,
    handle: JoinHandle<()>,
    channel_type: String,
    config_hash: u64,
}

impl ChannelSupervisor {
    pub fn new(gateway: Arc<Gateway>, bus: Arc<EventBus>) -> Self {
        let bot_factory: BotFactory = Arc::new(|channel_type, _, _, _| {
            Err(anyhow!(
                "no bot factory configured for channel type '{channel_type}'"
            ))
        });

        Self {
            bots: HashMap::new(),
            gateway,
            bus,
            global_token: CancellationToken::new(),
            bot_factory,
        }
    }

    pub fn with_bot_factory(mut self, bot_factory: BotFactory) -> Self {
        self.bot_factory = bot_factory;
        self
    }

    pub fn start(
        &mut self,
        connector_id: String,
        channel_type: &str,
        connector_config: Value,
    ) -> Result<()> {
        if self.bots.contains_key(&connector_id) {
            return Err(anyhow!("connector '{}' is already running", connector_id));
        }

        let gateway = Arc::clone(&self.gateway);
        let bus = Arc::clone(&self.bus);
        let bot_factory = Arc::clone(&self.bot_factory);
        let channel_type = channel_type.to_string();
        let task_token = self.global_token.child_token();
        let task_token_for_task = task_token.clone();
        let connector_id_for_task = connector_id.clone();
        let connector_config_for_task = connector_config.clone();
        let channel_type_for_task = channel_type.clone();
        let handle = tokio::spawn(async move {
            let mut backoff = Duration::from_secs(1);

            loop {
                let bot_task = match bot_factory(
                    &channel_type_for_task,
                    &connector_config_for_task,
                    Arc::clone(&gateway),
                    Arc::clone(&bus),
                ) {
                    Ok(bot_task) => bot_task,
                    Err(error) => {
                        tracing::error!(
                            connector_id = %connector_id_for_task,
                            channel_type = %channel_type_for_task,
                            error = %error,
                            "failed to create bot task"
                        );

                        tokio::select! {
                            _ = task_token_for_task.cancelled() => break,
                            _ = tokio::time::sleep(backoff) => {}
                        }

                        backoff = (backoff * 2).min(Duration::from_secs(300));
                        continue;
                    }
                };

                tokio::select! {
                    _ = task_token_for_task.cancelled() => break,
                    result = bot_task => {
                        match result {
                            Ok(()) => {
                                tracing::info!(
                                    connector_id = %connector_id_for_task,
                                    channel_type = %channel_type_for_task,
                                    "bot exited normally"
                                );
                                break;
                            }
                            Err(error) => {
                                tracing::error!(
                                    connector_id = %connector_id_for_task,
                                    channel_type = %channel_type_for_task,
                                    backoff_secs = backoff.as_secs(),
                                    error = %error,
                                    "bot crashed; scheduling restart"
                                );

                                tokio::select! {
                                    _ = task_token_for_task.cancelled() => break,
                                    _ = tokio::time::sleep(backoff) => {}
                                }

                                backoff = (backoff * 2).min(Duration::from_secs(300));
                            }
                        }
                    }
                }
            }

            tracing::info!(
                connector_id = %connector_id_for_task,
                channel_type = %channel_type_for_task,
                "bot supervision loop stopped"
            );
        });

        self.bots.insert(
            connector_id,
            BotEntry {
                token: task_token,
                handle,
                channel_type,
                config_hash: config_hash(&connector_config),
            },
        );

        Ok(())
    }

    pub async fn stop(&mut self, connector_id: &str) {
        let Some(entry) = self.bots.remove(connector_id) else {
            return;
        };

        let channel_type = entry.channel_type.clone();
        let abort_handle = entry.handle.abort_handle();
        entry.token.cancel();

        match tokio::time::timeout(Duration::from_secs(30), entry.handle).await {
            Ok(Ok(())) => {
                tracing::info!(connector_id = %connector_id, channel_type = %channel_type, "bot stopped");
            }
            Ok(Err(error)) if error.is_cancelled() => {
                tracing::info!(connector_id = %connector_id, channel_type = %channel_type, "bot cancelled during shutdown");
            }
            Ok(Err(error)) => {
                tracing::warn!(
                    connector_id = %connector_id,
                    channel_type = %channel_type,
                    error = %error,
                    "bot task ended with join error during shutdown"
                );
            }
            Err(_) => {
                tracing::warn!(
                    connector_id = %connector_id,
                    channel_type = %channel_type,
                    "bot did not stop within 30s, aborting"
                );
                abort_handle.abort();
            }
        }
    }

    pub async fn restart(
        &mut self,
        connector_id: String,
        channel_type: &str,
        connector_config: Value,
    ) -> Result<()> {
        self.stop(&connector_id).await;
        self.start(connector_id, channel_type, connector_config)
    }

    pub fn running_connectors(&self) -> HashMap<String, u64> {
        self.bots
            .iter()
            .map(|(connector_id, entry)| (connector_id.clone(), entry.config_hash))
            .collect()
    }

    pub async fn shutdown_all(&mut self) {
        self.global_token.cancel();

        let connector_ids: Vec<String> = self.bots.keys().cloned().collect();
        for connector_id in connector_ids {
            self.stop(&connector_id).await;
        }
    }
}

pub fn config_hash_value(config: &Value) -> u64 {
    config_hash(config)
}

fn config_hash(config: &Value) -> u64 {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    config.to_string().hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use clawhive_bus::EventBus;
    use clawhive_core::*;
    use clawhive_memory::embedding::{EmbeddingProvider, StubEmbeddingProvider};
    use clawhive_memory::MemoryStore;
    use clawhive_provider::{register_builtin_providers, ProviderRegistry};
    use clawhive_runtime::NativeExecutor;
    use clawhive_scheduler::ScheduleManager;
    use serde_json::json;

    use super::*;
    use crate::{Gateway, RateLimitConfig, RateLimiter};

    async fn make_gateway() -> (Arc<Gateway>, Arc<EventBus>, tempfile::TempDir) {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut registry = ProviderRegistry::new();
        register_builtin_providers(&mut registry);
        let aliases = HashMap::from([(
            "sonnet".to_string(),
            "anthropic/claude-sonnet-4-5".to_string(),
        )]);
        let router = LlmRouter::new(registry, aliases, vec![]);
        let memory = Arc::new(MemoryStore::open_in_memory().unwrap());
        let bus = Arc::new(EventBus::new(16));
        let publisher = bus.publisher();
        let schedule_manager = Arc::new(
            ScheduleManager::new(
                clawhive_scheduler::SqliteStore::open(&tmp.path().join("data/scheduler.db"))
                    .unwrap(),
                Arc::new(EventBus::new(16)),
            )
            .await
            .unwrap(),
        );
        let embedding_provider: Arc<dyn EmbeddingProvider> =
            Arc::new(StubEmbeddingProvider::new(8));
        let agents = vec![FullAgentConfig {
            agent_id: "clawhive-main".into(),
            enabled: true,
            security: SecurityMode::default(),
            identity: None,
            model_policy: ModelPolicy {
                primary: "sonnet".into(),
                fallbacks: vec![],
                thinking_level: None,
                context_window: None,
                compaction_model: None,
            },
            tool_policy: None,
            memory_policy: None,
            sub_agent: None,
            workspace: None,
            heartbeat: None,
            exec_security: None,
            sandbox: None,
            max_response_tokens: None,
            max_iterations: None,
            turn_timeout_secs: None,
            typing_ttl_secs: None,
            progress_delay_secs: None,
        }];
        let personas = HashMap::new();
        let routing = RoutingConfig {
            default_agent_id: "clawhive-main".to_string(),
            bindings: vec![],
        };
        let orchestrator = Arc::new(
            OrchestratorBuilder::new(
                ConfigView::new(
                    0,
                    agents,
                    personas,
                    routing.clone(),
                    router,
                    ToolRegistry::new(),
                    embedding_provider,
                ),
                publisher.clone(),
                memory,
                Arc::new(NativeExecutor),
                tmp.path().to_path_buf(),
                schedule_manager,
            )
            .build(),
        );
        let rate_limiter = RateLimiter::new(RateLimitConfig::default());

        (
            Arc::new(Gateway::new(orchestrator, publisher, rate_limiter, None)),
            bus,
            tmp,
        )
    }

    #[tokio::test]
    async fn start_tracks_running_connector_hash() {
        let (gateway, bus, _tmp) = make_gateway().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let bot_factory: BotFactory = {
            let calls = Arc::clone(&calls);
            Arc::new(move |_, _, _, _| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(Box::pin(async {
                    std::future::pending::<()>().await;
                    Ok(())
                }))
            })
        };

        let mut supervisor = ChannelSupervisor::new(gateway, bus).with_bot_factory(bot_factory);
        let connector_config = json!({"token": "abc"});

        supervisor
            .start("tg_main".to_string(), "telegram", connector_config.clone())
            .unwrap();
        tokio::task::yield_now().await;

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            supervisor.running_connectors().get("tg_main").copied(),
            Some(config_hash(&connector_config))
        );
    }

    #[tokio::test]
    async fn stop_removes_connector_from_running_map() {
        let (gateway, bus, _tmp) = make_gateway().await;
        let bot_factory: BotFactory = Arc::new(|_, _, _, _| {
            Ok(Box::pin(async {
                std::future::pending::<()>().await;
                Ok(())
            }))
        });
        let mut supervisor = ChannelSupervisor::new(gateway, bus).with_bot_factory(bot_factory);

        supervisor
            .start("tg_main".to_string(), "telegram", json!({"token": "abc"}))
            .unwrap();
        supervisor.stop("tg_main").await;

        assert!(supervisor.running_connectors().is_empty());
    }

    #[tokio::test]
    async fn restart_replaces_existing_config_hash() {
        let (gateway, bus, _tmp) = make_gateway().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let bot_factory: BotFactory = {
            let calls = Arc::clone(&calls);
            Arc::new(move |_, _, _, _| {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(Box::pin(async {
                    std::future::pending::<()>().await;
                    Ok(())
                }))
            })
        };
        let mut supervisor = ChannelSupervisor::new(gateway, bus).with_bot_factory(bot_factory);
        let initial_config = json!({"token": "abc"});
        let updated_config = json!({"token": "def"});

        supervisor
            .start("tg_main".to_string(), "telegram", initial_config)
            .unwrap();
        tokio::task::yield_now().await;
        supervisor
            .restart("tg_main".to_string(), "telegram", updated_config.clone())
            .await
            .unwrap();
        tokio::task::yield_now().await;

        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            supervisor.running_connectors().get("tg_main").copied(),
            Some(config_hash(&updated_config))
        );
    }
}
