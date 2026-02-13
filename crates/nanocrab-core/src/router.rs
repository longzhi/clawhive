use std::collections::HashMap;
use std::pin::Pin;

use anyhow::{anyhow, Result};
use futures_core::Stream;
use nanocrab_provider::{LlmMessage, LlmRequest, LlmResponse, ProviderRegistry, StreamChunk};
use tokio::time;

const MAX_RETRIES: usize = 2;
const BASE_BACKOFF_MS: u64 = 1000;

pub struct LlmRouter {
    registry: ProviderRegistry,
    aliases: HashMap<String, String>,
    global_fallbacks: Vec<String>,
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

        let mut last_err: Option<anyhow::Error> = None;

        for candidate in candidates {
            let resolved = self.resolve_model(&candidate)?;
            let (provider_id, model_id) = parse_provider_model(&resolved)?;
            let provider = self.registry.get(&provider_id)?;

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
                    Ok(resp) => return Ok(resp),
                    Err(err) => {
                        let err_str = err.to_string();
                        let is_retryable = err_str.contains("[retryable]");

                        if is_retryable && attempts < MAX_RETRIES {
                            attempts += 1;
                            let backoff = BASE_BACKOFF_MS * (1 << (attempts - 1));
                            tracing::warn!(
                                "provider {provider_id} retryable error (attempt {attempts}/{MAX_RETRIES}), backing off {backoff}ms: {err_str}"
                            );
                            time::sleep(time::Duration::from_millis(backoff)).await;
                            continue;
                        }

                        tracing::warn!(
                            "provider {provider_id} failed (retryable={is_retryable}, attempts={attempts}): {err_str}"
                        );
                        last_err = Some(err);
                        break;
                    }
                }
            }
        }

        Err(last_err.unwrap_or_else(|| anyhow!("no model candidate available")))
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

        let mut last_err: Option<anyhow::Error> = None;

        for candidate in candidates {
            let resolved = self.resolve_model(&candidate)?;
            let (provider_id, model_id) = parse_provider_model(&resolved)?;
            let provider = self.registry.get(&provider_id)?;

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
                    Ok(resp) => return Ok(resp),
                    Err(err) => {
                        let err_str = err.to_string();
                        let is_retryable = err_str.contains("[retryable]");

                        if is_retryable && attempts < MAX_RETRIES {
                            attempts += 1;
                            let backoff = BASE_BACKOFF_MS * (1 << (attempts - 1));
                            tracing::warn!(
                                "provider {provider_id} retryable error (attempt {attempts}/{MAX_RETRIES}), backing off {backoff}ms: {err_str}"
                            );
                            time::sleep(time::Duration::from_millis(backoff)).await;
                            continue;
                        }

                        tracing::warn!(
                            "provider {provider_id} failed (retryable={is_retryable}, attempts={attempts}): {err_str}"
                        );
                        last_err = Some(err);
                        break;
                    }
                }
            }
        }

        Err(last_err.unwrap_or_else(|| anyhow!("no model candidate available")))
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

        let mut last_err: Option<anyhow::Error> = None;

        for candidate in candidates {
            let resolved = self.resolve_model(&candidate)?;
            let (provider_id, model_id) = parse_provider_model(&resolved)?;
            let provider = self.registry.get(&provider_id)?;

            let req = LlmRequest {
                model: model_id,
                system: system.clone(),
                messages: messages.clone(),
                max_tokens,
                tools: vec![],
            };

            match provider.stream(req).await {
                Ok(stream) => return Ok(stream),
                Err(err) => {
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
    use nanocrab_provider::{
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
