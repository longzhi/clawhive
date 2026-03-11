use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use async_trait::async_trait;

use clawhive_auth::{
    manager::OpenAiRefreshConfig,
    oauth::{OpenAiOAuthConfig, OPENAI_OAUTH_CLIENT_ID},
    AuthProfile, TokenManager,
};
use clawhive_bus::EventBus;
use clawhive_core::*;
use clawhive_gateway::{Gateway, RateLimitConfig, RateLimiter};
use clawhive_memory::embedding::{
    EmbeddingProvider, GeminiEmbeddingProvider, OllamaEmbeddingProvider, OpenAiEmbeddingProvider,
    StubEmbeddingProvider,
};
use clawhive_memory::search_index::SearchIndex;
use clawhive_memory::MemoryStore;
use clawhive_provider::{
    minimax, moonshot, qianfan, qwen, register_builtin_providers, volcengine, zhipu,
    AnthropicProvider, AzureOpenAiProvider, LlmProvider, LlmRequest, LlmResponse,
    OpenAiChatGptProvider, OpenAiProvider, ProviderRegistry, StreamChunk,
};
use clawhive_runtime::NativeExecutor;
use clawhive_scheduler::{ScheduleManager, ScheduleType, SqliteStore, WaitTaskManager};
use futures_core::Stream;

pub(crate) fn toggle_agent(
    agents_dir: &std::path::Path,
    agent_id: &str,
    enabled: bool,
) -> Result<()> {
    let path = agents_dir.join(format!("{agent_id}.yaml"));
    if !path.exists() {
        anyhow::bail!("agent config not found: {}", path.display());
    }
    let content = std::fs::read_to_string(&path)?;
    let mut doc: serde_yaml::Value = serde_yaml::from_str(&content)?;
    if let serde_yaml::Value::Mapping(ref mut map) = doc {
        map.insert(
            serde_yaml::Value::String("enabled".into()),
            serde_yaml::Value::Bool(enabled),
        );
    }
    let output = serde_yaml::to_string(&doc)?;
    std::fs::write(&path, output)?;
    Ok(())
}

pub(crate) fn format_schedule_type(schedule: &ScheduleType) -> String {
    match schedule {
        ScheduleType::Cron { expr, tz } => format!("cron({expr} @ {tz})"),
        ScheduleType::At { at } => format!("at({at})"),
        ScheduleType::Every {
            interval_ms,
            anchor_ms,
        } => match anchor_ms {
            Some(anchor) => format!("every({interval_ms}ms, anchor={anchor})"),
            None => format!("every({interval_ms}ms)"),
        },
    }
}

pub(crate) fn resolve_security_override(
    security: Option<SecurityMode>,
    no_security: bool,
) -> Option<SecurityMode> {
    if no_security {
        Some(SecurityMode::Off)
    } else {
        security
    }
}

#[derive(Debug, Clone)]
enum OpenAiProfileTarget {
    Named(String),
    Active,
}

impl OpenAiProfileTarget {
    fn from_provider_config(provider_config: &ProviderConfig) -> Self {
        provider_config
            .auth_profile
            .clone()
            .map(Self::Named)
            .unwrap_or(Self::Active)
    }

    fn resolve_profile_name(&self, token_manager: &TokenManager) -> Result<Option<String>> {
        match self {
            Self::Named(name) => Ok(Some(name.clone())),
            Self::Active => Ok(token_manager.load_store()?.active_profile),
        }
    }
}

fn default_openai_refresh_config() -> OpenAiRefreshConfig {
    let oauth_config = OpenAiOAuthConfig::default_with_client(OPENAI_OAUTH_CLIENT_ID);
    OpenAiRefreshConfig {
        token_endpoint: oauth_config.token_endpoint,
        client_id: oauth_config.client_id,
    }
}

fn build_http_client(timeout_secs: u64) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .build()
        .unwrap_or_default()
}

#[derive(Clone)]
struct RefreshingOpenAiProvider {
    client: reqwest::Client,
    token_manager: TokenManager,
    profile_target: OpenAiProfileTarget,
    api_key: String,
    api_base: String,
    refresh_config: OpenAiRefreshConfig,
}

impl RefreshingOpenAiProvider {
    fn new(
        token_manager: TokenManager,
        profile_target: OpenAiProfileTarget,
        api_key: String,
        api_base: String,
    ) -> Self {
        Self {
            client: build_http_client(60),
            token_manager,
            profile_target,
            api_key,
            api_base,
            refresh_config: default_openai_refresh_config(),
        }
    }

    async fn load_auth_profile(&self) -> Option<AuthProfile> {
        let profile_name = match self
            .profile_target
            .resolve_profile_name(&self.token_manager)
        {
            Ok(profile_name) => profile_name,
            Err(err) => {
                tracing::warn!("Failed to resolve OpenAI auth profile: {err}");
                return None;
            }
        }?;

        let profile = match self.token_manager.get_profile(&profile_name) {
            Ok(Some(profile)) => profile,
            Ok(None) => return None,
            Err(err) => {
                tracing::warn!(
                    profile = %profile_name,
                    "Failed to load OpenAI auth profile: {err}"
                );
                return None;
            }
        };

        match profile {
            AuthProfile::OpenAiOAuth { .. } => match self
                .token_manager
                .refresh_if_needed(&self.client, &profile_name, &self.refresh_config)
                .await
            {
                Ok(Some(profile)) => Some(profile),
                Ok(None) => Some(profile),
                Err(err) => {
                    tracing::warn!(
                        profile = %profile_name,
                        "Failed to refresh OpenAI OAuth token before request: {err}"
                    );
                    Some(profile)
                }
            },
            AuthProfile::ApiKey {
                ref provider_id, ..
            } if provider_id == "openai" => Some(profile),
            _ => None,
        }
    }

    async fn build_inner(&self) -> OpenAiProvider {
        OpenAiProvider::with_client(
            self.client.clone(),
            self.api_key.clone(),
            self.api_base.clone(),
            self.load_auth_profile().await,
        )
    }
}

#[async_trait]
impl LlmProvider for RefreshingOpenAiProvider {
    async fn chat(&self, request: LlmRequest) -> Result<LlmResponse> {
        self.build_inner().await.chat(request).await
    }

    async fn stream(
        &self,
        request: LlmRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>> {
        self.build_inner().await.stream(request).await
    }

    async fn health(&self) -> Result<()> {
        self.build_inner().await.health().await
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        self.build_inner().await.list_models().await
    }
}

#[derive(Clone)]
struct RefreshingOpenAiChatGptProvider {
    client: reqwest::Client,
    token_manager: TokenManager,
    profile_target: OpenAiProfileTarget,
    api_base: String,
    refresh_config: OpenAiRefreshConfig,
}

impl RefreshingOpenAiChatGptProvider {
    fn new(
        token_manager: TokenManager,
        profile_target: OpenAiProfileTarget,
        api_base: String,
    ) -> Self {
        Self {
            client: build_http_client(120),
            token_manager,
            profile_target,
            api_base,
            refresh_config: default_openai_refresh_config(),
        }
    }

    async fn load_oauth_profile(&self) -> Result<(String, Option<String>)> {
        let profile_name = self
            .profile_target
            .resolve_profile_name(&self.token_manager)?
            .ok_or_else(|| anyhow!("openai-chatgpt: no OAuth profile found"))?;

        let profile = self
            .token_manager
            .get_profile(&profile_name)?
            .ok_or_else(|| anyhow!("openai-chatgpt: auth profile '{profile_name}' not found"))?;

        let profile = match profile {
            AuthProfile::OpenAiOAuth { .. } => match self
                .token_manager
                .refresh_if_needed(&self.client, &profile_name, &self.refresh_config)
                .await
            {
                Ok(Some(profile)) => profile,
                Ok(None) => profile,
                Err(err) => {
                    tracing::warn!(
                        profile = %profile_name,
                        "Failed to refresh OpenAI OAuth token before request: {err}"
                    );
                    profile
                }
            },
            _ => {
                return Err(anyhow!(
                    "openai-chatgpt: auth profile '{profile_name}' is not OpenAI OAuth"
                ));
            }
        };

        match profile {
            AuthProfile::OpenAiOAuth {
                access_token,
                chatgpt_account_id,
                ..
            } => Ok((access_token, chatgpt_account_id)),
            _ => Err(anyhow!(
                "openai-chatgpt: auth profile '{profile_name}' is not OpenAI OAuth"
            )),
        }
    }

    async fn build_inner(&self) -> Result<OpenAiChatGptProvider> {
        let (access_token, chatgpt_account_id) = self.load_oauth_profile().await?;
        Ok(OpenAiChatGptProvider::with_client(
            self.client.clone(),
            access_token,
            chatgpt_account_id,
            self.api_base.clone(),
        ))
    }
}

#[async_trait]
impl LlmProvider for RefreshingOpenAiChatGptProvider {
    async fn chat(&self, request: LlmRequest) -> Result<LlmResponse> {
        self.build_inner().await?.chat(request).await
    }

    async fn stream(
        &self,
        request: LlmRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>> {
        self.build_inner().await?.stream(request).await
    }

    async fn health(&self) -> Result<()> {
        self.build_inner().await?.health().await
    }

    async fn list_models(&self) -> Result<Vec<String>> {
        self.build_inner().await?.list_models().await
    }
}

#[allow(clippy::type_complexity)]
pub(crate) async fn bootstrap(
    root: &Path,
    security_override: Option<SecurityMode>,
) -> Result<(
    Arc<EventBus>,
    Arc<MemoryStore>,
    Arc<Gateway>,
    ClawhiveConfig,
    Arc<ScheduleManager>,
    Arc<WaitTaskManager>,
    Arc<ApprovalRegistry>,
)> {
    let mut config = load_config(&root.join("config"))?;

    if let Some(mode) = security_override {
        for agent in &mut config.agents {
            agent.security = mode.clone();
        }
        if mode == SecurityMode::Off {
            tracing::warn!(
                "⚠️  Security disabled via --no-security flag. All security checks are OFF."
            );
            eprintln!(
                "⚠️  WARNING: Security disabled. All security checks (HardBaseline, approval, sandbox restrictions) are OFF."
            );
        }
    }

    let db_path = root.join("data/clawhive.db");
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let memory = Arc::new(MemoryStore::open(
        db_path.to_str().unwrap_or("data/clawhive.db"),
    )?);

    let router = build_router_from_config(&config).await;

    // Initialize peer registry by scanning workspaces
    let workspaces_root = root.join("workspaces");
    let peer_registry = match PeerRegistry::scan_workspaces(&workspaces_root) {
        Ok(registry) => {
            tracing::info!("Discovered {} peer agents", registry.len());
            registry
        }
        Err(e) => {
            tracing::warn!("Failed to scan workspaces for peers: {e}");
            PeerRegistry::new()
        }
    };

    // Load personas from workspace directories (OpenClaw-style)
    let mut personas = HashMap::new();
    for agent_config in &config.agents {
        let identity = agent_config.identity.as_ref();
        let name = identity
            .map(|i| i.name.as_str())
            .unwrap_or(&agent_config.agent_id);
        let emoji = identity.and_then(|i| i.emoji.as_deref());

        // Resolve workspace path and ensure prompt templates exist
        let workspace = Workspace::resolve(
            root,
            &agent_config.agent_id,
            agent_config.workspace.as_deref(),
        );
        if let Err(e) = workspace.init_with_defaults().await {
            tracing::warn!(
                "Failed to init workspace for {}: {e}",
                agent_config.agent_id
            );
        }

        match load_persona_from_workspace(workspace.root(), &agent_config.agent_id, name, emoji) {
            Ok(mut persona) => {
                // Inject peers context for multi-agent collaboration
                let peers_md = peer_registry.format_peers_md(&agent_config.agent_id);
                if !peers_md.is_empty() {
                    persona.peers_context = peers_md;
                }
                personas.insert(agent_config.agent_id.clone(), persona);
            }
            Err(e) => {
                tracing::warn!("Failed to load persona for {}: {e}", agent_config.agent_id);
            }
        }
    }

    let bus = Arc::new(EventBus::new(256));
    let publisher = bus.publisher();
    let new_path = root.join("data/runtime_allowlist.json");
    let old_path = root.join("data/exec_allowlist.json");
    if !new_path.exists() && old_path.exists() {
        if let Err(e) = std::fs::rename(&old_path, &new_path) {
            tracing::warn!("Failed to migrate exec_allowlist.json to runtime_allowlist.json: {e}");
        } else {
            tracing::info!("Migrated exec_allowlist.json -> runtime_allowlist.json");
        }
    }
    let approval_registry = Arc::new(ApprovalRegistry::with_persistence(new_path));
    let schedule_manager = Arc::new(ScheduleManager::new(
        &root.join("config/schedules.d"),
        &root.join("data/schedules"),
        Arc::clone(&bus),
    )?);

    // Initialize SQLite store for wait tasks
    let scheduler_db_path = root.join("data/scheduler.db");
    let sqlite_store = Arc::new(SqliteStore::open(&scheduler_db_path)?);
    let wait_task_manager = Arc::new(WaitTaskManager::new(
        Arc::clone(&sqlite_store),
        Arc::clone(&bus),
    ));
    let session_mgr = SessionManager::new(memory.clone(), 1800);
    let skill_registry = SkillRegistry::load_from_dir(&root.join("skills")).unwrap_or_else(|e| {
        tracing::warn!("Failed to load skills: {e}");
        SkillRegistry::new()
    });
    let workspace_dir = root.to_path_buf();
    let file_store = clawhive_memory::file_store::MemoryFileStore::new(&workspace_dir);
    let session_writer = clawhive_memory::SessionWriter::new(&workspace_dir);
    let session_reader = clawhive_memory::SessionReader::new(&workspace_dir);
    let search_index = SearchIndex::new(memory.db());
    let embedding_provider = build_embedding_provider(&config).await;

    let brave_api_key = config
        .main
        .tools
        .web_search
        .as_ref()
        .filter(|ws| ws.enabled)
        .and_then(|ws| ws.api_key.clone())
        .filter(|k| !k.is_empty());

    let orchestrator = Arc::new(Orchestrator::new(
        router,
        config.agents.clone(),
        personas,
        session_mgr,
        skill_registry,
        memory.clone(),
        publisher.clone(),
        Some(approval_registry.clone()),
        Arc::new(NativeExecutor),
        file_store,
        session_writer,
        session_reader,
        search_index,
        embedding_provider,
        workspace_dir.clone(),
        brave_api_key,
        Some(root.to_path_buf()),
        Arc::clone(&schedule_manager),
    ));

    let rate_limiter = RateLimiter::new(RateLimitConfig::default());
    let gateway = Arc::new(Gateway::new(
        orchestrator,
        config.routing.clone(),
        publisher,
        rate_limiter,
        Some(approval_registry.clone()),
    ));

    Ok((
        bus,
        memory,
        gateway,
        config,
        schedule_manager,
        wait_task_manager,
        approval_registry,
    ))
}

fn collect_openai_oauth_refresh_targets(
    config: &ClawhiveConfig,
    active_profile_name: Option<&str>,
) -> HashSet<String> {
    let mut targets = HashSet::new();

    for provider_config in &config.providers {
        if !provider_config.enabled {
            continue;
        }

        if !matches!(
            provider_config.provider_id.as_str(),
            "openai" | "openai-chatgpt"
        ) {
            continue;
        }

        if let Some(profile_name) = provider_config
            .auth_profile
            .as_deref()
            .or(active_profile_name)
        {
            targets.insert(profile_name.to_string());
        }
    }

    targets
}

async fn refresh_required_openai_oauth_profiles(
    config: &ClawhiveConfig,
    token_manager: &TokenManager,
) {
    let store = match token_manager.load_store() {
        Ok(store) => store,
        Err(err) => {
            tracing::warn!("Failed to load auth profiles before OAuth refresh: {err}");
            return;
        }
    };

    let targets = collect_openai_oauth_refresh_targets(config, store.active_profile.as_deref());
    if targets.is_empty() {
        return;
    }

    let refresh_config = default_openai_refresh_config();
    let http = build_http_client(60);

    for profile_name in targets {
        if !matches!(
            store.profiles.get(&profile_name),
            Some(AuthProfile::OpenAiOAuth { .. })
        ) {
            continue;
        }

        match token_manager
            .refresh_if_needed(&http, &profile_name, &refresh_config)
            .await
        {
            Ok(Some(_)) => {
                tracing::info!(profile = %profile_name, "Refreshed OpenAI OAuth token");
            }
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(
                    profile = %profile_name,
                    "Failed to refresh OpenAI OAuth token: {err}"
                );
            }
        }
    }
}

pub(crate) async fn build_router_from_config(config: &ClawhiveConfig) -> LlmRouter {
    let token_manager = match TokenManager::new() {
        Ok(manager) => {
            refresh_required_openai_oauth_profiles(config, &manager).await;
            Some(manager)
        }
        Err(err) => {
            tracing::warn!("Failed to initialize auth token manager: {err}");
            None
        }
    };
    let auth_store = token_manager
        .as_ref()
        .and_then(|manager| match manager.load_store() {
            Ok(store) => Some(store),
            Err(err) => {
                tracing::warn!("Failed to load auth profiles: {err}");
                None
            }
        });
    let active_profile = auth_store.as_ref().and_then(|store| {
        store
            .active_profile
            .as_ref()
            .and_then(|name| store.profiles.get(name).cloned())
    });

    let anthropic_profile = active_profile.as_ref().and_then(|p| match p {
        AuthProfile::AnthropicSession { .. } => Some(p.clone()),
        AuthProfile::ApiKey { provider_id, .. } if provider_id == "anthropic" => Some(p.clone()),
        _ => None,
    });

    let mut registry = ProviderRegistry::new();
    for provider_config in &config.providers {
        if !provider_config.enabled {
            continue;
        }

        // Resolve OAuth profile: named auth_profile takes priority, then fallback to active_profile
        let named_profile = provider_config.auth_profile.as_ref().and_then(|name| {
            auth_store
                .as_ref()
                .and_then(|store| store.profiles.get(name).cloned())
        });
        let profile_target = OpenAiProfileTarget::from_provider_config(provider_config);

        match provider_config.provider_id.as_str() {
            "anthropic" => {
                let api_key = provider_config
                    .api_key
                    .clone()
                    .filter(|k| !k.is_empty())
                    .unwrap_or_default();
                if !api_key.is_empty() {
                    let provider = Arc::new(AnthropicProvider::new_with_auth(
                        api_key,
                        provider_config.api_base.clone(),
                        anthropic_profile.clone(),
                    ));
                    registry.register("anthropic", provider);
                } else {
                    tracing::warn!("Anthropic API key not set, using stub provider");
                    register_builtin_providers(&mut registry);
                }
            }
            "openai" => {
                let api_key = provider_config
                    .api_key
                    .clone()
                    .filter(|k| !k.is_empty())
                    .unwrap_or_default();

                // Resolve the effective OAuth profile for this provider
                let oauth_profile = named_profile.clone().or_else(|| {
                    active_profile.as_ref().and_then(|p| match p {
                        AuthProfile::OpenAiOAuth { .. } => Some(p.clone()),
                        _ => None,
                    })
                });

                if !api_key.is_empty() {
                    // Standard API key path — use chat/completions
                    let provider: Arc<dyn LlmProvider> =
                        if matches!(oauth_profile, Some(AuthProfile::OpenAiOAuth { .. })) {
                            match token_manager.clone() {
                                Some(token_manager) => Arc::new(RefreshingOpenAiProvider::new(
                                    token_manager,
                                    profile_target.clone(),
                                    api_key.clone(),
                                    provider_config.api_base.clone(),
                                )),
                                None => Arc::new(OpenAiProvider::new_with_auth(
                                    api_key.clone(),
                                    provider_config.api_base.clone(),
                                    oauth_profile,
                                )),
                            }
                        } else {
                            Arc::new(OpenAiProvider::new_with_auth(
                                api_key.clone(),
                                provider_config.api_base.clone(),
                                oauth_profile,
                            ))
                        };
                    registry.register("openai", provider);
                } else if let Some(AuthProfile::OpenAiOAuth {
                    access_token,
                    chatgpt_account_id,
                    ..
                }) = &oauth_profile
                {
                    // Backward compat: openai config with no api_key but has OAuth → ChatGPT provider
                    let provider: Arc<dyn LlmProvider> = match token_manager.clone() {
                        Some(token_manager) => Arc::new(RefreshingOpenAiChatGptProvider::new(
                            token_manager,
                            profile_target.clone(),
                            provider_config.api_base.clone(),
                        )),
                        None => Arc::new(OpenAiChatGptProvider::new(
                            access_token.clone(),
                            chatgpt_account_id.clone(),
                            provider_config.api_base.clone(),
                        )),
                    };
                    registry.register("openai", provider);
                    tracing::info!(
                        "OpenAI registered via ChatGPT OAuth (account: {:?})",
                        chatgpt_account_id
                    );
                } else {
                    tracing::warn!("OpenAI: no API key and no OAuth profile, skipping");
                }
            }
            "openai-chatgpt" => {
                // Dedicated ChatGPT OAuth provider
                let oauth_profile = named_profile.clone().or_else(|| {
                    active_profile.as_ref().and_then(|p| match p {
                        AuthProfile::OpenAiOAuth { .. } => Some(p.clone()),
                        _ => None,
                    })
                });

                if let Some(AuthProfile::OpenAiOAuth {
                    access_token,
                    chatgpt_account_id,
                    ..
                }) = &oauth_profile
                {
                    let provider: Arc<dyn LlmProvider> = match token_manager.clone() {
                        Some(token_manager) => Arc::new(RefreshingOpenAiChatGptProvider::new(
                            token_manager,
                            profile_target.clone(),
                            provider_config.api_base.clone(),
                        )),
                        None => Arc::new(OpenAiChatGptProvider::new(
                            access_token.clone(),
                            chatgpt_account_id.clone(),
                            provider_config.api_base.clone(),
                        )),
                    };
                    registry.register("openai-chatgpt", provider);
                    tracing::info!(
                        "openai-chatgpt registered via OAuth (account: {:?})",
                        chatgpt_account_id
                    );
                } else {
                    tracing::warn!("openai-chatgpt: no OAuth profile found, skipping");
                }
            }
            "azure-openai" => {
                let api_key = provider_config.api_key.clone().filter(|k| !k.is_empty());
                if let Some(api_key) = api_key {
                    let provider = Arc::new(AzureOpenAiProvider::new(
                        api_key,
                        provider_config.api_base.clone(),
                    ));
                    registry.register("azure-openai", provider);
                } else {
                    tracing::warn!("Azure OpenAI: no API key set, skipping");
                }
            }
            "qwen" => {
                let api_key = provider_config.api_key.clone().filter(|k| !k.is_empty());
                if let Some(api_key) = api_key {
                    let provider = Arc::new(qwen(api_key));
                    registry.register("qwen", provider);
                } else {
                    tracing::warn!("Qwen: no API key set, skipping");
                }
            }
            "moonshot" => {
                let api_key = provider_config.api_key.clone().filter(|k| !k.is_empty());
                if let Some(api_key) = api_key {
                    let provider = Arc::new(moonshot(api_key));
                    registry.register("moonshot", provider);
                } else {
                    tracing::warn!("Moonshot: no API key set, skipping");
                }
            }
            "zhipu" => {
                let api_key = provider_config.api_key.clone().filter(|k| !k.is_empty());
                if let Some(api_key) = api_key {
                    let provider = Arc::new(zhipu(api_key));
                    registry.register("zhipu", provider);
                } else {
                    tracing::warn!("Zhipu: no API key set, skipping");
                }
            }
            "minimax" => {
                let api_key = provider_config.api_key.clone().filter(|k| !k.is_empty());
                if let Some(api_key) = api_key {
                    let provider = Arc::new(minimax(api_key));
                    registry.register("minimax", provider);
                } else {
                    tracing::warn!("MiniMax: no API key set, skipping");
                }
            }
            "volcengine" => {
                let api_key = provider_config.api_key.clone().filter(|k| !k.is_empty());
                if let Some(api_key) = api_key {
                    let provider = Arc::new(volcengine(api_key));
                    registry.register("volcengine", provider);
                } else {
                    tracing::warn!("Volcengine: no API key set, skipping");
                }
            }
            "qianfan" => {
                let api_key = provider_config.api_key.clone().filter(|k| !k.is_empty());
                if let Some(api_key) = api_key {
                    let provider = Arc::new(qianfan(api_key));
                    registry.register("qianfan", provider);
                } else {
                    tracing::warn!("Qianfan: no API key set, skipping");
                }
            }
            _ => {
                tracing::warn!("Unknown provider: {}", provider_config.provider_id);
            }
        }
    }

    if registry.get("anthropic").is_err() {
        register_builtin_providers(&mut registry);
    }

    let mut aliases = HashMap::new();
    for provider_config in &config.providers {
        if !provider_config.enabled {
            continue;
        }
        for model in &provider_config.models {
            aliases.insert(
                model.clone(),
                format!("{}/{}", provider_config.provider_id, model),
            );
        }
    }
    // Anthropic model aliases: short names → latest models
    aliases
        .entry("sonnet".to_string())
        .or_insert_with(|| "anthropic/claude-sonnet-4-6".to_string());
    aliases
        .entry("haiku".to_string())
        .or_insert_with(|| "anthropic/claude-haiku-4-5".to_string());
    aliases
        .entry("opus".to_string())
        .or_insert_with(|| "anthropic/claude-opus-4-6".to_string());

    // Anthropic model aliases: bare model IDs (without provider prefix) → fully qualified
    for model_id in &[
        "claude-opus-4-6",
        "claude-sonnet-4-6",
        "claude-haiku-4-5",
        "claude-haiku-4-5-20251001",
        "claude-sonnet-4-5",
        "claude-sonnet-4-5-20250929",
        "claude-opus-4-5",
        "claude-opus-4-5-20251101",
        "claude-opus-4-1",
        "claude-opus-4-1-20250805",
        "claude-sonnet-4-0",
        "claude-sonnet-4-20250514",
        "claude-opus-4-0",
        "claude-opus-4-20250514",
        "claude-3-haiku-20240307",
    ] {
        aliases
            .entry(model_id.to_string())
            .or_insert_with(|| format!("anthropic/{model_id}"));
    }
    // Use gpt-5.3-codex for ChatGPT OAuth compatibility (Codex Responses API)
    // gpt-4o-mini and other non-Codex models are not supported via ChatGPT OAuth
    aliases
        .entry("gpt".to_string())
        .or_insert_with(|| "openai/gpt-5.3-codex".to_string());
    aliases
        .entry("chatgpt".to_string())
        .or_insert_with(|| "openai-chatgpt/gpt-5.3-codex".to_string());

    let mut global_fallbacks = Vec::new();
    if registry.get("openai").is_ok() {
        global_fallbacks.push("gpt".to_string());
    }
    if registry.get("openai-chatgpt").is_ok() {
        global_fallbacks.push("chatgpt".to_string());
    }

    LlmRouter::new(registry, aliases, global_fallbacks)
}

pub(crate) async fn build_embedding_provider(
    config: &ClawhiveConfig,
) -> Arc<dyn EmbeddingProvider> {
    let embedding_config = &config.main.embedding;

    // If explicitly disabled, use stub
    if !embedding_config.enabled {
        tracing::info!("Embedding disabled, using stub provider");
        return Arc::new(StubEmbeddingProvider::new(8));
    }

    // Priority: ollama > openai (explicit key) > openai (reuse provider key) > stub
    match embedding_config.provider.as_str() {
        "ollama" => {
            let provider = OllamaEmbeddingProvider::with_model(
                embedding_config.model.clone(),
                embedding_config.dimensions,
            )
            .with_base_url(embedding_config.base_url.clone());

            if provider.is_available().await {
                tracing::info!(
                    "Ollama embedding provider initialized (model: {}, dimensions: {})",
                    embedding_config.model,
                    embedding_config.dimensions
                );
                return Arc::new(provider);
            }
            tracing::warn!("Ollama not available, falling back");
        }
        "auto" | "" => {
            // Try Ollama first (free, local)
            let ollama = OllamaEmbeddingProvider::new();
            if ollama.is_available().await {
                tracing::info!(
                    "Auto-detected Ollama, using embedding model: {}",
                    ollama.model_id()
                );
                return Arc::new(ollama);
            }
            tracing::debug!("Ollama not available for auto-detection");
        }
        "openai" => {} // Fall through to OpenAI logic below
        "gemini" | "google" => {
            let api_key = embedding_config.api_key.clone();
            if !api_key.is_empty() {
                let provider = GeminiEmbeddingProvider::with_model(
                    api_key,
                    embedding_config.model.clone(),
                    embedding_config.dimensions,
                )
                .with_base_url(embedding_config.base_url.clone());

                tracing::info!(
                    "Gemini embedding provider initialized (model: {}, dimensions: {})",
                    embedding_config.model,
                    embedding_config.dimensions
                );
                return Arc::new(provider);
            }
            tracing::warn!("Gemini embedding API key not set, falling back");
        }
        other => {
            tracing::warn!("Unknown embedding provider '{}', falling back", other);
        }
    }

    // Try explicit embedding API key first
    let api_key = embedding_config.api_key.clone();
    if !api_key.is_empty() {
        let provider = OpenAiEmbeddingProvider::with_model(
            api_key,
            embedding_config.model.clone(),
            embedding_config.dimensions,
        )
        .with_base_url(embedding_config.base_url.clone());

        tracing::info!(
            "OpenAI embedding provider initialized (model: {}, dimensions: {})",
            embedding_config.model,
            embedding_config.dimensions
        );
        return Arc::new(provider);
    }

    // Try to reuse API key from configured LLM providers
    // Priority: OpenAI > Gemini (both support embeddings)
    let mut gemini_key: Option<String> = None;

    for p in &config.providers {
        if !p.enabled {
            continue;
        }
        if let Some(ref key) = p.api_key {
            if key.is_empty() {
                continue;
            }

            // OpenAI (direct API only)
            if p.api_base.contains("openai.com") {
                let provider = OpenAiEmbeddingProvider::with_model(
                    key.clone(),
                    "text-embedding-3-small".to_string(),
                    1536,
                )
                .with_base_url(p.api_base.clone());

                tracing::info!("Reusing OpenAI API key for embeddings (text-embedding-3-small)");
                return Arc::new(provider);
            }

            // Gemini / Google
            if p.provider_id == "gemini"
                || p.provider_id == "google"
                || p.api_base.contains("generativelanguage.googleapis.com")
                || p.api_base.contains("google")
            {
                gemini_key = Some(key.clone());
            }
        }
    }

    // Also check env var for Gemini
    if gemini_key.is_none() {
        if let Ok(key) = std::env::var("GEMINI_API_KEY") {
            if !key.is_empty() {
                gemini_key = Some(key);
            }
        }
    }

    if let Some(key) = gemini_key {
        tracing::info!("Using Gemini API key for embeddings (gemini-embedding-001)");
        return Arc::new(GeminiEmbeddingProvider::new(key));
    }

    // No embedding provider available — stub will be used
    // BM25 keyword search will handle memory_search as fallback
    tracing::warn!("No embedding provider available, memory_search will use keyword matching only");
    Arc::new(StubEmbeddingProvider::new(8))
}

#[cfg(test)]
mod tests {
    use super::collect_openai_oauth_refresh_targets;
    use clawhive_core::{ClawhiveConfig, MainConfig, ProviderConfig, RoutingConfig};

    #[test]
    fn collect_openai_oauth_refresh_targets_uses_named_or_active_profiles() {
        let config = ClawhiveConfig {
            main: MainConfig::default(),
            routing: RoutingConfig {
                default_agent_id: String::new(),
                bindings: Vec::new(),
            },
            providers: vec![
                ProviderConfig {
                    provider_id: "openai".to_string(),
                    enabled: true,
                    api_base: "https://api.openai.com/v1".to_string(),
                    api_key: None,
                    auth_profile: Some("named-openai".to_string()),
                    models: Vec::new(),
                },
                ProviderConfig {
                    provider_id: "openai-chatgpt".to_string(),
                    enabled: true,
                    api_base: "https://chatgpt.com/backend-api/codex".to_string(),
                    api_key: None,
                    auth_profile: None,
                    models: Vec::new(),
                },
                ProviderConfig {
                    provider_id: "anthropic".to_string(),
                    enabled: true,
                    api_base: "https://api.anthropic.com".to_string(),
                    api_key: None,
                    auth_profile: Some("anthropic-session".to_string()),
                    models: Vec::new(),
                },
                ProviderConfig {
                    provider_id: "openai".to_string(),
                    enabled: false,
                    api_base: "https://api.openai.com/v1".to_string(),
                    api_key: None,
                    auth_profile: Some("disabled-openai".to_string()),
                    models: Vec::new(),
                },
            ],
            agents: Vec::new(),
        };

        let targets = collect_openai_oauth_refresh_targets(&config, Some("active-openai"));

        assert_eq!(targets.len(), 2);
        assert!(targets.contains("named-openai"));
        assert!(targets.contains("active-openai"));
    }
}
