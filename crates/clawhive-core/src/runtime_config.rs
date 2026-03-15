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
use clawhive_bus::BusPublisher;
use clawhive_memory::embedding::{
    EmbeddingProvider, GeminiEmbeddingProvider, OllamaEmbeddingProvider, OpenAiEmbeddingProvider,
    StubEmbeddingProvider,
};
use clawhive_memory::file_store::MemoryFileStore;
use clawhive_memory::search_index::SearchIndex;
use clawhive_memory::MemoryStore;
use clawhive_provider::{
    custom, minimax, moonshot, qianfan, qwen, register_builtin_providers, volcengine, zhipu,
    AnthropicProvider, AzureOpenAiProvider, LlmProvider, LlmRequest, LlmResponse,
    OpenAiChatGptProvider, OpenAiProvider, ProviderRegistry, StreamChunk,
};
use futures_core::Stream;

use crate::config::{ClawhiveConfig, ProviderConfig};
use crate::config_view::ConfigView;
use crate::orchestrator::build_tool_registry;
use crate::persona::{load_persona_from_workspace, Persona};
use crate::router::LlmRouter;
use crate::workspace::Workspace;
use crate::ApprovalRegistry;

fn build_http_client(timeout_secs: u64) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .build()
        .unwrap_or_default()
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
                tracing::warn!(profile = %profile_name, "Failed to load OpenAI auth profile: {err}");
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
                    tracing::warn!(profile = %profile_name, "Failed to refresh OpenAI OAuth token before request: {err}");
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
                    tracing::warn!(profile = %profile_name, "Failed to refresh OpenAI OAuth token before request: {err}");
                    profile
                }
            },
            _ => {
                return Err(anyhow!(
                    "openai-chatgpt: auth profile '{profile_name}' is not OpenAI OAuth"
                ))
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
        let _ = token_manager
            .refresh_if_needed(&http, &profile_name, &refresh_config)
            .await;
    }
}

pub async fn build_router_from_config(config: &ClawhiveConfig) -> LlmRouter {
    let token_manager = match TokenManager::new() {
        Ok(manager) => {
            refresh_required_openai_oauth_profiles(config, &manager).await;
            Some(manager)
        }
        Err(_) => None,
    };
    let auth_store = token_manager
        .as_ref()
        .and_then(|manager| manager.load_store().ok());
    let active_profile = auth_store.as_ref().and_then(|store| {
        store
            .active_profile
            .as_ref()
            .and_then(|name| store.profiles.get(name).cloned())
    });
    let anthropic_profile = active_profile.as_ref().and_then(|profile| match profile {
        AuthProfile::AnthropicSession { .. } => Some(profile.clone()),
        AuthProfile::ApiKey { provider_id, .. } if provider_id == "anthropic" => {
            Some(profile.clone())
        }
        _ => None,
    });

    let mut registry = ProviderRegistry::new();
    for provider_config in &config.providers {
        if !provider_config.enabled {
            continue;
        }
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
                    .filter(|key| !key.is_empty())
                    .unwrap_or_default();
                if !api_key.is_empty() {
                    registry.register(
                        "anthropic",
                        Arc::new(AnthropicProvider::new_with_auth(
                            api_key,
                            provider_config.api_base.clone(),
                            anthropic_profile.clone(),
                        )),
                    );
                }
            }
            "openai" => {
                let api_key = provider_config
                    .api_key
                    .clone()
                    .filter(|key| !key.is_empty())
                    .unwrap_or_default();
                let oauth_profile = named_profile.clone().or_else(|| {
                    active_profile.as_ref().and_then(|profile| match profile {
                        AuthProfile::OpenAiOAuth { .. } => Some(profile.clone()),
                        _ => None,
                    })
                });
                if !api_key.is_empty() {
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
                }
            }
            "openai-chatgpt" => {
                let oauth_profile = named_profile.clone().or_else(|| {
                    active_profile.as_ref().and_then(|profile| match profile {
                        AuthProfile::OpenAiOAuth { .. } => Some(profile.clone()),
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
                }
            }
            "azure-openai" => {
                if let Some(api_key) = provider_config
                    .api_key
                    .clone()
                    .filter(|key| !key.is_empty())
                {
                    registry.register(
                        "azure-openai",
                        Arc::new(AzureOpenAiProvider::new(
                            api_key,
                            provider_config.api_base.clone(),
                        )),
                    );
                }
            }
            "qwen" => {
                if let Some(api_key) = provider_config
                    .api_key
                    .clone()
                    .filter(|key| !key.is_empty())
                {
                    registry.register("qwen", Arc::new(qwen(api_key)));
                }
            }
            "moonshot" => {
                if let Some(api_key) = provider_config
                    .api_key
                    .clone()
                    .filter(|key| !key.is_empty())
                {
                    registry.register("moonshot", Arc::new(moonshot(api_key)));
                }
            }
            "zhipu" => {
                if let Some(api_key) = provider_config
                    .api_key
                    .clone()
                    .filter(|key| !key.is_empty())
                {
                    registry.register("zhipu", Arc::new(zhipu(api_key)));
                }
            }
            "minimax" => {
                if let Some(api_key) = provider_config
                    .api_key
                    .clone()
                    .filter(|key| !key.is_empty())
                {
                    registry.register("minimax", Arc::new(minimax(api_key)));
                }
            }
            "volcengine" => {
                if let Some(api_key) = provider_config
                    .api_key
                    .clone()
                    .filter(|key| !key.is_empty())
                {
                    registry.register("volcengine", Arc::new(volcengine(api_key)));
                }
            }
            "qianfan" => {
                if let Some(api_key) = provider_config
                    .api_key
                    .clone()
                    .filter(|key| !key.is_empty())
                {
                    registry.register("qianfan", Arc::new(qianfan(api_key)));
                }
            }
            _ => {
                if provider_config.provider_type.as_deref() == Some("custom") {
                    let api_key = provider_config
                        .api_key
                        .clone()
                        .filter(|key| !key.is_empty())
                        .unwrap_or_default();
                    registry.register(
                        &provider_config.provider_id,
                        Arc::new(custom(api_key, provider_config.api_base.clone())),
                    );
                }
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
    aliases
        .entry("sonnet".to_string())
        .or_insert_with(|| "anthropic/claude-sonnet-4-6".to_string());
    aliases
        .entry("haiku".to_string())
        .or_insert_with(|| "anthropic/claude-haiku-4-5".to_string());
    aliases
        .entry("opus".to_string())
        .or_insert_with(|| "anthropic/claude-opus-4-6".to_string());
    aliases
        .entry("gpt".to_string())
        .or_insert_with(|| "openai/gpt-5.3-codex".to_string());
    aliases
        .entry("chatgpt".to_string())
        .or_insert_with(|| "openai-chatgpt/gpt-5.3-codex".to_string());

    LlmRouter::new(registry, aliases, vec![])
}

pub async fn build_embedding_provider(config: &ClawhiveConfig) -> Arc<dyn EmbeddingProvider> {
    let embedding_config = &config.main.embedding;
    if !embedding_config.enabled {
        return Arc::new(StubEmbeddingProvider::new(8));
    }
    match embedding_config.provider.as_str() {
        "ollama" => {
            let provider = OllamaEmbeddingProvider::with_model(
                embedding_config.model.clone(),
                embedding_config.dimensions,
            )
            .with_base_url(embedding_config.base_url.clone());
            if provider.is_available().await {
                return Arc::new(provider);
            }
        }
        "auto" | "" => {
            let ollama = OllamaEmbeddingProvider::new();
            if ollama.is_available().await {
                return Arc::new(ollama);
            }
        }
        "gemini" | "google" => {
            if !embedding_config.api_key.is_empty() {
                return Arc::new(
                    GeminiEmbeddingProvider::with_model(
                        embedding_config.api_key.clone(),
                        embedding_config.model.clone(),
                        embedding_config.dimensions,
                    )
                    .with_base_url(embedding_config.base_url.clone()),
                );
            }
        }
        _ => {}
    }
    if !embedding_config.api_key.is_empty() {
        return Arc::new(
            OpenAiEmbeddingProvider::with_model(
                embedding_config.api_key.clone(),
                embedding_config.model.clone(),
                embedding_config.dimensions,
            )
            .with_base_url(embedding_config.base_url.clone()),
        );
    }
    Arc::new(StubEmbeddingProvider::new(8))
}

pub async fn build_personas_from_config(
    root: &Path,
    config: &ClawhiveConfig,
) -> HashMap<String, Persona> {
    let mut personas = HashMap::new();
    for agent_config in &config.agents {
        let identity = agent_config.identity.as_ref();
        let name = identity
            .map(|value| value.name.as_str())
            .unwrap_or(&agent_config.agent_id);
        let emoji = identity.and_then(|value| value.emoji.as_deref());
        let workspace = Workspace::resolve(
            root,
            &agent_config.agent_id,
            agent_config.workspace.as_deref(),
        );
        let _ = workspace.init_with_defaults().await;
        if let Ok(persona) =
            load_persona_from_workspace(workspace.root(), &agent_config.agent_id, name, emoji)
        {
            personas.insert(agent_config.agent_id.clone(), persona);
        }
    }
    personas
}

pub async fn build_config_view(
    config: &ClawhiveConfig,
    generation: u64,
    root: &Path,
    memory: &Arc<MemoryStore>,
    approval_registry: &Option<Arc<ApprovalRegistry>>,
    publisher: &BusPublisher,
    schedule_manager: Arc<clawhive_scheduler::ScheduleManager>,
) -> ConfigView {
    let router = build_router_from_config(config).await;
    let personas = build_personas_from_config(root, config).await;
    let embedding_provider = build_embedding_provider(config).await;
    let file_store = MemoryFileStore::new(root);
    let search_index = SearchIndex::new(memory.db());
    let brave_api_key = config
        .main
        .tools
        .web_search
        .as_ref()
        .filter(|cfg| cfg.enabled)
        .and_then(|cfg| cfg.api_key.clone())
        .filter(|key| !key.is_empty());
    let router_arc = Arc::new(router.clone());
    let tool_registry = build_tool_registry(
        &file_store,
        &search_index,
        &embedding_provider,
        root,
        root,
        approval_registry,
        publisher,
        schedule_manager,
        brave_api_key,
        &router_arc,
        &config.agents,
        &personas,
    );
    ConfigView::new(
        generation,
        config.agents.clone(),
        personas,
        config.routing.clone(),
        router,
        tool_registry,
        embedding_provider,
    )
}
