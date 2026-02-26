use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use futures_core::Stream;
use clawhive_provider::{LlmMessage, LlmRequest, LlmResponse, ProviderRegistry, StreamChunk};
use tokio::time;

const MAX_RETRIES: usize = 2;
const BASE_BACKOFF_MS: u64 = 1000;
const DEFAULT_COOLDOWN_SECS: u64 = 60;
const BILLING_COOLDOWN_SECS: u64 = 300; // 5 minutes for billing errors

/// Tracks cooldown state for providers (similar to OpenClaw's auth profile stats)
#[derive(Debug, Clone, Default)]
pub struct ProviderCooldownStats {
    pub cooldown_until: Option<Instant>,
    pub failure_count: u32,
    pub last_failure_reason: Option<String>,
}

impl ProviderCooldownStats {
    pub fn is_in_cooldown(&self) -> bool {
        self.cooldown_until
            .map(|until| Instant::now() < until)
            .unwrap_or(false)
    }

    pub fn remaining_cooldown(&self) -> Option<Duration> {
        self.cooldown_until.and_then(|until| {
            let now = Instant::now();
            if now < until {
                Some(until - now)
            } else {
                None
            }
        })
    }

    pub fn set_cooldown(&mut self, duration: Duration, reason: &str) {
        self.cooldown_until = Some(Instant::now() + duration);
        self.failure_count += 1;
        self.last_failure_reason = Some(reason.to_string());
    }

    pub fn clear_cooldown(&mut self) {
        self.cooldown_until = None;
    }
}

/// Cooldown store for all providers
#[derive(Debug, Clone, Default)]
pub struct CooldownStore {
    stats: HashMap<String, ProviderCooldownStats>,
}

impl CooldownStore {
    pub fn new() -> Self {
        Self {
            stats: HashMap::new(),
        }
    }

    pub fn get_stats(&self, provider_id: &str) -> ProviderCooldownStats {
        self.stats.get(provider_id).cloned().unwrap_or_default()
    }

    pub fn set_cooldown(&mut self, provider_id: &str, duration: Duration, reason: &str) {
        let stats = self.stats.entry(provider_id.to_string()).or_default();
        stats.set_cooldown(duration, reason);
    }

    pub fn clear_cooldown(&mut self, provider_id: &str) {
        if let Some(stats) = self.stats.get_mut(provider_id) {
            stats.clear_cooldown();
        }
    }

    pub fn clear_expired_cooldowns(&mut self) {
        let now = Instant::now();
        for stats in self.stats.values_mut() {
            if let Some(until) = stats.cooldown_until {
                if now >= until {
                    stats.cooldown_until = None;
                }
            }
        }
    }
}

/// Error classification for failover decisions (following OpenClaw patterns)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailoverReason {
    RateLimit,       // 429
    Billing,         // insufficient credits / billing error
    Timeout,         // request timed out
    ServerError,     // 5xx
    ContextOverflow, // context too long
    AuthError,       // 401/403
    Unknown,         // other errors
}

impl FailoverReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::RateLimit => "rate_limit",
            Self::Billing => "billing",
            Self::Timeout => "timeout",
            Self::ServerError => "server_error",
            Self::ContextOverflow => "context_overflow",
            Self::AuthError => "auth_error",
            Self::Unknown => "unknown",
        }
    }
}

/// Classify an error to determine if/how to failover
fn classify_failover_reason(err_str: &str) -> Option<FailoverReason> {
    let lower = err_str.to_lowercase();

    // Rate limit (429)
    if lower.contains("429") || lower.contains("rate limit") || lower.contains("rate_limit") {
        return Some(FailoverReason::RateLimit);
    }

    // Billing errors
    if lower.contains("insufficient")
        || lower.contains("billing")
        || lower.contains("credits")
        || lower.contains("quota")
        || lower.contains("payment")
    {
        return Some(FailoverReason::Billing);
    }

    // Timeout
    if lower.contains("timeout") || lower.contains("timed out") || lower.contains("deadline") {
        return Some(FailoverReason::Timeout);
    }

    // Server errors (5xx)
    if lower.contains("500")
        || lower.contains("502")
        || lower.contains("503")
        || lower.contains("504")
        || lower.contains("internal server error")
        || lower.contains("service unavailable")
        || lower.contains("bad gateway")
    {
        return Some(FailoverReason::ServerError);
    }

    // Context overflow
    if lower.contains("context")
        && (lower.contains("overflow")
            || lower.contains("too long")
            || lower.contains("too large")
            || lower.contains("exceed"))
    {
        return Some(FailoverReason::ContextOverflow);
    }

    // Auth errors (401/403)
    if lower.contains("401")
        || lower.contains("403")
        || lower.contains("unauthorized")
        || lower.contains("forbidden")
        || lower.contains("authentication")
    {
        return Some(FailoverReason::AuthError);
    }

    // Check for [retryable] marker - if present but not caught above, it's still failover-eligible
    if lower.contains("[retryable]") {
        return Some(FailoverReason::Unknown);
    }

    None
}

/// Check if an error should trigger failover
#[allow(dead_code)]
fn is_failover_error(err_str: &str) -> bool {
    classify_failover_reason(err_str).is_some()
}

/// Get cooldown duration based on error type
fn get_cooldown_duration(reason: FailoverReason) -> Duration {
    match reason {
        FailoverReason::Billing => Duration::from_secs(BILLING_COOLDOWN_SECS),
        FailoverReason::RateLimit => Duration::from_secs(DEFAULT_COOLDOWN_SECS),
        FailoverReason::Timeout => Duration::from_secs(30),
        FailoverReason::ServerError => Duration::from_secs(30),
        FailoverReason::ContextOverflow => Duration::from_secs(0), // no cooldown, just skip
        FailoverReason::AuthError => Duration::from_secs(3600),    // 1 hour for auth issues
        FailoverReason::Unknown => Duration::from_secs(DEFAULT_COOLDOWN_SECS),
    }
}

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
                        let err_str = err.to_string();
                        let is_retryable = err_str.contains("[retryable]");
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

                        last_err = Some(err);
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
                        let err_str = err.to_string();
                        let is_retryable = err_str.contains("[retryable]");
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

                        last_err = Some(err);
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
                    last_err = Some(err);
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
        LlmMessage, LlmProvider, LlmRequest, LlmResponse, ProviderRegistry, StreamChunk,
    };
    use tokio_stream::StreamExt;

    use super::LlmRouter;

    struct RetryableFailProvider {
        call_count: AtomicUsize,
        fail_times: usize,
    }

    #[async_trait]
    impl LlmProvider for RetryableFailProvider {
        async fn chat(&self, _request: LlmRequest) -> anyhow::Result<LlmResponse> {
            let count = self.call_count.fetch_add(1, Ordering::SeqCst);
            if count < self.fail_times {
                anyhow::bail!("anthropic api error (429) [retryable]: rate limited")
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
        async fn chat(&self, _request: LlmRequest) -> anyhow::Result<LlmResponse> {
            anyhow::bail!("anthropic api error (401): unauthorized")
        }
    }

    struct StubStreamProvider;

    #[async_trait]
    impl LlmProvider for StubStreamProvider {
        async fn chat(&self, _request: LlmRequest) -> anyhow::Result<LlmResponse> {
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
        ) -> anyhow::Result<
            std::pin::Pin<Box<dyn futures_core::Stream<Item = anyhow::Result<StreamChunk>> + Send>>,
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
        async fn chat(&self, _request: LlmRequest) -> anyhow::Result<LlmResponse> {
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
            .stream("model", &[], None, vec![LlmMessage::user("hi")], 100)
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
            )
            .await;
        assert!(stream.is_ok());
    }
}
