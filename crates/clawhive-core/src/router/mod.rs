mod cooldown;
mod failover;

pub use cooldown::{CooldownStore, ProviderCooldownStats};
pub use failover::{
    classify_failover_reason, get_cooldown_duration, is_failover_error, FailoverReason,
};

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use anyhow::{anyhow, Result};
use clawhive_provider::{LlmMessage, LlmRequest, LlmResponse, ProviderRegistry, StreamChunk};
use futures_core::Stream;
use tokio::time;

const MAX_RETRIES: usize = 2;
const BASE_BACKOFF_MS: u64 = 1000;

#[derive(Clone)]
pub struct LlmRouter {
    registry: ProviderRegistry,
    aliases: HashMap<String, String>,
    global_fallbacks: Vec<String>,
    cooldowns: Arc<RwLock<CooldownStore>>,
}

impl LlmRouter {
    pub fn new(
        registry: ProviderRegistry,
        aliases: HashMap<String, String>,
        global_fallbacks: Vec<String>,
    ) -> Self {
        Self {
            registry,
            aliases,
            global_fallbacks,
            cooldowns: Arc::new(RwLock::new(CooldownStore::new())),
        }
    }

    /// Check if a provider is registered.
    pub fn has_provider(&self, provider_id: &str) -> bool {
        self.registry.get(provider_id).is_ok()
    }

    /// List all registered provider IDs.
    pub fn provider_ids(&self) -> Vec<String> {
        self.registry.list().into_iter().map(String::from).collect()
    }

    /// Check if a provider is currently in cooldown
    fn is_provider_in_cooldown(&self, provider_id: &str) -> bool {
        self.cooldowns
            .read()
            .ok()
            .map(|store| store.get_stats(provider_id).is_in_cooldown())
            .unwrap_or(false)
    }

    /// Record a provider failure and set cooldown
    fn record_provider_failure(&self, provider_id: &str, reason: FailoverReason) {
        let duration = get_cooldown_duration(reason);
        if let Ok(mut store) = self.cooldowns.write() {
            store.set_cooldown(provider_id, duration, reason.as_str());
        }
    }

    /// Clear cooldown for a provider (on successful request)
    fn clear_provider_cooldown(&self, provider_id: &str) {
        if let Ok(mut store) = self.cooldowns.write() {
            store.clear_cooldown(provider_id);
        }
    }

    pub async fn chat(
        &self,
        primary: &str,
        fallbacks: &[String],
        system: Option<String>,
        messages: Vec<LlmMessage>,
        max_tokens: u32,
    ) -> Result<LlmResponse> {
        let mut candidates = vec![primary.to_string()];
        candidates.extend(fallbacks.iter().cloned());
        candidates.extend(self.global_fallbacks.clone());

        // Deduplicate candidates while preserving order
        let mut seen = std::collections::HashSet::new();
        candidates.retain(|c| seen.insert(c.clone()));

        let mut last_err: Option<anyhow::Error> = None;
        let mut tried_providers: Vec<String> = Vec::new();

        for (idx, candidate) in candidates.iter().enumerate() {
            let resolved = match self.resolve_model(candidate) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("failed to resolve model {candidate}: {e}");
                    continue;
                }
            };

            let (provider_id, model_id) = match parse_provider_model(&resolved) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("invalid model format {resolved}: {e}");
                    continue;
                }
            };

            // Skip providers in cooldown (but log it)
            if self.is_provider_in_cooldown(&provider_id) {
                let remaining = self
                    .cooldowns
                    .read()
                    .ok()
                    .and_then(|s| s.get_stats(&provider_id).remaining_cooldown())
                    .map(|d| format!("{:.0}s", d.as_secs_f64()))
                    .unwrap_or_else(|| "?".to_string());
                tracing::info!(
                    "skipping provider {provider_id} (in cooldown for {remaining}), trying next"
                );
                continue;
            }

            let provider = match self.registry.get(&provider_id) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("provider {provider_id} not available: {e}");
                    continue;
                }
            };

            tried_providers.push(format!("{}/{}", provider_id, model_id));

            let mut attempts = 0;
            loop {
                let req = LlmRequest {
                    model: model_id.clone(),
                    system: system.clone(),
                    messages: messages.clone(),
                    max_tokens,
                    tools: vec![],
                    thinking_level: None,
                };

                match provider.chat(req).await {
                    Ok(resp) => {
                        // Success! Clear any cooldown for this provider
                        self.clear_provider_cooldown(&provider_id);

                        if idx > 0 {
                            tracing::info!(
                                "fallback_triggered=true, from={}, to={}/{}, attempt={}",
                                primary,
                                provider_id,
                                model_id,
                                idx + 1
                            );
                        }
                        return Ok(resp);
                    }
                    Err(err) => {
                        let is_retryable = err.is_retryable();
                        let err_str = err.to_string();
                        let failover_reason = classify_failover_reason(&err_str);

                        // Retry within same provider if retryable and within limits
                        if is_retryable && attempts < MAX_RETRIES {
                            attempts += 1;
                            let backoff = BASE_BACKOFF_MS * (1 << (attempts - 1));
                            tracing::warn!(
                                "provider {provider_id} retryable error (attempt {attempts}/{MAX_RETRIES}), backing off {backoff}ms: {err_str}"
                            );
                            time::sleep(time::Duration::from_millis(backoff)).await;
                            continue;
                        }

                        // Record failure and potentially set cooldown
                        if let Some(reason) = failover_reason {
                            self.record_provider_failure(&provider_id, reason);
                            tracing::warn!(
                                "provider {provider_id} failed (reason={}, retryable={}, attempts={}): {err_str}",
                                reason.as_str(),
                                is_retryable,
                                attempts
                            );
                        } else {
                            tracing::warn!(
                                "provider {provider_id} failed (retryable={}, attempts={}): {err_str}",
                                is_retryable,
                                attempts
                            );
                        }

                        last_err = Some(err.into());
                        break; // Try next candidate
                    }
                }
            }
        }

        // All candidates exhausted
        let tried = tried_providers.join(" -> ");
        Err(last_err.unwrap_or_else(|| {
            anyhow!("all model candidates failed or in cooldown (tried: {tried})")
        }))
    }

    pub async fn reply(&self, agent: &super::AgentConfig, user_text: &str) -> Result<String> {
        let messages = vec![LlmMessage::user(user_text)];
        let resp = self
            .chat(
                &agent.model_policy.primary,
                &agent.model_policy.fallbacks,
                Some(format!("agent_id={}", agent.agent_id)),
                messages,
                2048,
            )
            .await?;
        Ok(resp.text)
    }

    pub async fn chat_with_tools(
        &self,
        primary: &str,
        fallbacks: &[String],
        request: LlmRequest,
    ) -> Result<LlmResponse> {
        let mut candidates = vec![primary.to_string()];
        candidates.extend(fallbacks.iter().cloned());
        candidates.extend(self.global_fallbacks.clone());

        // Deduplicate candidates while preserving order
        let mut seen = std::collections::HashSet::new();
        candidates.retain(|c| seen.insert(c.clone()));

        let mut last_err: Option<anyhow::Error> = None;
        let mut tried_providers: Vec<String> = Vec::new();

        for (idx, candidate) in candidates.iter().enumerate() {
            let resolved = match self.resolve_model(candidate) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("failed to resolve model {candidate}: {e}");
                    continue;
                }
            };

            let (provider_id, model_id) = match parse_provider_model(&resolved) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("invalid model format {resolved}: {e}");
                    continue;
                }
            };

            // Skip providers in cooldown
            if self.is_provider_in_cooldown(&provider_id) {
                let remaining = self
                    .cooldowns
                    .read()
                    .ok()
                    .and_then(|s| s.get_stats(&provider_id).remaining_cooldown())
                    .map(|d| format!("{:.0}s", d.as_secs_f64()))
                    .unwrap_or_else(|| "?".to_string());
                tracing::info!(
                    "skipping provider {provider_id} (in cooldown for {remaining}), trying next"
                );
                continue;
            }

            let provider = match self.registry.get(&provider_id) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("provider {provider_id} not available: {e}");
                    continue;
                }
            };

            tried_providers.push(format!("{}/{}", provider_id, model_id));

            let mut attempts = 0;
            loop {
                let req = LlmRequest {
                    model: model_id.clone(),
                    system: request.system.clone(),
                    messages: request.messages.clone(),
                    max_tokens: request.max_tokens,
                    tools: request.tools.clone(),
                    thinking_level: request.thinking_level,
                };

                match provider.chat(req).await {
                    Ok(resp) => {
                        self.clear_provider_cooldown(&provider_id);

                        if idx > 0 {
                            tracing::info!(
                                "fallback_triggered=true (with_tools), from={}, to={}/{}, attempt={}",
                                primary,
                                provider_id,
                                model_id,
                                idx + 1
                            );
                        }
                        return Ok(resp);
                    }
                    Err(err) => {
                        let is_retryable = err.is_retryable();
                        let err_str = err.to_string();
                        let failover_reason = classify_failover_reason(&err_str);

                        if is_retryable && attempts < MAX_RETRIES {
                            attempts += 1;
                            let backoff = BASE_BACKOFF_MS * (1 << (attempts - 1));
                            tracing::warn!(
                                "provider {provider_id} retryable error (attempt {attempts}/{MAX_RETRIES}), backing off {backoff}ms: {err_str}"
                            );
                            time::sleep(time::Duration::from_millis(backoff)).await;
                            continue;
                        }

                        if let Some(reason) = failover_reason {
                            self.record_provider_failure(&provider_id, reason);
                            tracing::warn!(
                                "provider {provider_id} failed (reason={}, retryable={}, attempts={}): {err_str}",
                                reason.as_str(),
                                is_retryable,
                                attempts
                            );
                        } else {
                            tracing::warn!(
                                "provider {provider_id} failed (retryable={}, attempts={}): {err_str}",
                                is_retryable,
                                attempts
                            );
                        }

                        last_err = Some(err.into());
                        break;
                    }
                }
            }
        }

        let tried = tried_providers.join(" -> ");
        Err(last_err.unwrap_or_else(|| {
            anyhow!("all model candidates failed or in cooldown (tried: {tried})")
        }))
    }

    pub async fn stream(
        &self,
        primary: &str,
        fallbacks: &[String],
        system: Option<String>,
        messages: Vec<LlmMessage>,
        max_tokens: u32,
        thinking_level: Option<clawhive_provider::ThinkingLevel>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>> {
        let mut candidates = vec![primary.to_string()];
        candidates.extend(fallbacks.iter().cloned());
        candidates.extend(self.global_fallbacks.clone());

        // Deduplicate
        let mut seen = std::collections::HashSet::new();
        candidates.retain(|c| seen.insert(c.clone()));

        let mut last_err: Option<anyhow::Error> = None;

        for (idx, candidate) in candidates.iter().enumerate() {
            let resolved = match self.resolve_model(candidate) {
                Ok(r) => r,
                Err(_) => continue,
            };

            let (provider_id, model_id) = match parse_provider_model(&resolved) {
                Ok(p) => p,
                Err(_) => continue,
            };

            // Skip providers in cooldown
            if self.is_provider_in_cooldown(&provider_id) {
                tracing::info!("skipping provider {provider_id} (in cooldown), trying next");
                continue;
            }

            let provider = match self.registry.get(&provider_id) {
                Ok(p) => p,
                Err(_) => continue,
            };

            let req = LlmRequest {
                model: model_id.clone(),
                system: system.clone(),
                messages: messages.clone(),
                max_tokens,
                tools: vec![],
                thinking_level,
            };

            match provider.stream(req).await {
                Ok(stream) => {
                    self.clear_provider_cooldown(&provider_id);
                    if idx > 0 {
                        tracing::info!(
                            "fallback_triggered=true (stream), from={}, to={}/{}",
                            primary,
                            provider_id,
                            model_id
                        );
                    }
                    return Ok(stream);
                }
                Err(err) => {
                    let err_str = err.to_string();
                    if let Some(reason) = classify_failover_reason(&err_str) {
                        self.record_provider_failure(&provider_id, reason);
                    }
                    tracing::warn!("provider {provider_id} stream failed: {err}");
                    last_err = Some(err.into());
                }
            }
        }

        Err(last_err.unwrap_or_else(|| anyhow!("no model candidate available for streaming")))
    }

    fn resolve_model(&self, raw: &str) -> Result<String> {
        if raw.contains('/') {
            return Ok(raw.to_string());
        }
        self.aliases
            .get(raw)
            .cloned()
            .ok_or_else(|| anyhow!("unknown model alias: {raw}"))
    }
}

fn parse_provider_model(input: &str) -> Result<(String, String)> {
    let mut parts = input.splitn(2, '/');
    let provider = parts
        .next()
        .ok_or_else(|| anyhow!("invalid model format: {input}"))?;
    let model = parts
        .next()
        .ok_or_else(|| anyhow!("invalid model format: {input}"))?;
    Ok((provider.to_string(), model.to_string()))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use async_trait::async_trait;
    use clawhive_provider::{
        LlmMessage, LlmProvider, LlmRequest, LlmResponse, ProviderError, ProviderRegistry,
        StreamChunk,
    };
    use tokio_stream::StreamExt;

    use super::LlmRouter;

    struct RetryableFailProvider {
        call_count: AtomicUsize,
        fail_times: usize,
    }

    #[async_trait]
    impl LlmProvider for RetryableFailProvider {
        async fn chat(&self, _request: LlmRequest) -> Result<LlmResponse, ProviderError> {
            let count = self.call_count.fetch_add(1, Ordering::SeqCst);
            if count < self.fail_times {
                return Err(ProviderError::RateLimited { retry_after_ms: 0 });
            }
            Ok(LlmResponse {
                text: format!("ok after {} retries", count),
                content: vec![],
                input_tokens: None,
                output_tokens: None,
                stop_reason: Some("end_turn".into()),
            })
        }
    }

    struct PermanentFailProvider;

    #[async_trait]
    impl LlmProvider for PermanentFailProvider {
        async fn chat(&self, _request: LlmRequest) -> Result<LlmResponse, ProviderError> {
            Err(ProviderError::AuthFailed(
                "anthropic api error (401): unauthorized".into(),
            ))
        }
    }

    struct StubStreamProvider;

    #[async_trait]
    impl LlmProvider for StubStreamProvider {
        async fn chat(&self, _request: LlmRequest) -> Result<LlmResponse, ProviderError> {
            Ok(LlmResponse {
                text: "chat".into(),
                content: vec![],
                input_tokens: None,
                output_tokens: None,
                stop_reason: Some("end_turn".into()),
            })
        }

        async fn stream(
            &self,
            _request: LlmRequest,
        ) -> Result<
            std::pin::Pin<Box<dyn futures_core::Stream<Item = anyhow::Result<StreamChunk>> + Send>>,
            ProviderError,
        > {
            let chunks = vec![
                Ok(StreamChunk {
                    delta: "hello ".into(),
                    is_final: false,
                    input_tokens: None,
                    output_tokens: None,
                    stop_reason: None,
                    content_blocks: vec![],
                }),
                Ok(StreamChunk {
                    delta: "world".into(),
                    is_final: false,
                    input_tokens: None,
                    output_tokens: None,
                    stop_reason: None,
                    content_blocks: vec![],
                }),
                Ok(StreamChunk {
                    delta: String::new(),
                    is_final: true,
                    input_tokens: Some(5),
                    output_tokens: Some(10),
                    stop_reason: Some("end_turn".into()),
                    content_blocks: vec![],
                }),
            ];
            Ok(Box::pin(tokio_stream::iter(chunks)))
        }
    }

    struct SuccessProvider;

    #[async_trait]
    impl LlmProvider for SuccessProvider {
        async fn chat(&self, _request: LlmRequest) -> Result<LlmResponse, ProviderError> {
            Ok(LlmResponse {
                text: "success from fallback".into(),
                content: vec![],
                input_tokens: None,
                output_tokens: None,
                stop_reason: Some("end_turn".into()),
            })
        }
    }

    #[tokio::test]
    async fn retries_on_retryable_error() {
        let provider = Arc::new(RetryableFailProvider {
            call_count: AtomicUsize::new(0),
            fail_times: 2,
        });
        let mut registry = ProviderRegistry::new();
        registry.register("test", provider.clone());
        let aliases = HashMap::from([("model".to_string(), "test/model".to_string())]);
        let router = LlmRouter::new(registry, aliases, vec![]);

        let resp = router
            .chat("model", &[], None, vec![LlmMessage::user("hi")], 100)
            .await
            .unwrap();
        assert!(resp.text.contains("ok after 2 retries"));
        assert_eq!(provider.call_count.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn no_retry_on_non_retryable_error() {
        let mut registry = ProviderRegistry::new();
        registry.register("test", Arc::new(PermanentFailProvider));
        let aliases = HashMap::from([("model".to_string(), "test/model".to_string())]);
        let router = LlmRouter::new(registry, aliases, vec![]);

        let result = router
            .chat("model", &[], None, vec![LlmMessage::user("hi")], 100)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("401"));
    }

    #[tokio::test]
    async fn fallback_on_failure() {
        let mut registry = ProviderRegistry::new();
        registry.register("fail", Arc::new(PermanentFailProvider));
        registry.register("success", Arc::new(SuccessProvider));
        let aliases = HashMap::from([
            ("bad".to_string(), "fail/model".to_string()),
            ("good".to_string(), "success/model".to_string()),
        ]);
        let router = LlmRouter::new(registry, aliases, vec![]);

        let resp = router
            .chat(
                "bad",
                &["good".into()],
                None,
                vec![LlmMessage::user("hi")],
                100,
            )
            .await
            .unwrap();
        assert!(resp.text.contains("success from fallback"));
    }

    #[tokio::test]
    async fn global_fallback_used() {
        let mut registry = ProviderRegistry::new();
        registry.register("fail", Arc::new(PermanentFailProvider));
        registry.register("global", Arc::new(SuccessProvider));
        let aliases = HashMap::from([
            ("bad".to_string(), "fail/model".to_string()),
            ("global_model".to_string(), "global/model".to_string()),
        ]);
        let router = LlmRouter::new(registry, aliases, vec!["global_model".into()]);

        let resp = router
            .chat("bad", &[], None, vec![LlmMessage::user("hi")], 100)
            .await
            .unwrap();
        assert!(resp.text.contains("success from fallback"));
    }

    #[tokio::test]
    async fn stream_returns_chunks() {
        let mut registry = ProviderRegistry::new();
        registry.register("test", Arc::new(StubStreamProvider));
        let aliases = HashMap::from([("model".to_string(), "test/model".to_string())]);
        let router = LlmRouter::new(registry, aliases, vec![]);

        let mut stream = router
            .stream("model", &[], None, vec![LlmMessage::user("hi")], 100, None)
            .await
            .unwrap();

        let mut collected = String::new();
        let mut got_final = false;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.unwrap();
            if chunk.is_final {
                got_final = true;
            } else {
                collected.push_str(&chunk.delta);
            }
        }
        assert!(got_final);
        assert_eq!(collected, "hello world");
    }

    #[tokio::test]
    async fn stream_falls_back_on_failure() {
        let mut registry = ProviderRegistry::new();
        registry.register("fail", Arc::new(PermanentFailProvider));
        registry.register("test", Arc::new(StubStreamProvider));
        let aliases = HashMap::from([
            ("bad".to_string(), "fail/model".to_string()),
            ("good".to_string(), "test/model".to_string()),
        ]);
        let router = LlmRouter::new(registry, aliases, vec![]);

        let stream = router
            .stream(
                "bad",
                &["good".into()],
                None,
                vec![LlmMessage::user("hi")],
                100,
                None,
            )
            .await;
        assert!(stream.is_ok());
    }
}
