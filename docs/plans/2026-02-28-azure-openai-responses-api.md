# Azure OpenAI Responses API Support

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add `azure-openai` provider that uses Azure's Responses API (`/openai/v1/responses`) so users with Azure OpenAI deployments can use clawhive.

**Architecture:** Create `AzureOpenAiProvider` that reuses the existing Responses API request/response logic from `OpenAiChatGptProvider` (types, SSE parsing), but with Azure-specific URL construction and `api-key` header authentication. Register it in both `create_provider()` and `build_router_from_config()`.

**Tech Stack:** Rust, reqwest, async-trait, serde, tokio-stream

---

## Context

Azure OpenAI Responses API:
- **URL**: `{api_base}/responses` where api_base = `https://{RESOURCE}.openai.azure.com/openai/v1`
- **Auth**: `api-key: {key}` header (NOT `Authorization: Bearer`)
- **No** `openai-beta` header needed
- **Request body**: identical to OpenAI Responses API (same `ResponsesRequest` struct)
- **SSE stream**: identical event format

Example config the user will write:
```yaml
provider_id: azure-openai
enabled: true
api_base: https://my-resource.openai.azure.com/openai/v1
api_key: ${AZURE_API_KEY}
models:
  - gpt-4o
```

---

### Task 1: Make Responses API utilities reusable

**Files:**
- Modify: `crates/clawhive-provider/src/openai_chatgpt.rs`

**Step 1: Change visibility of shared functions and types**

Make these items `pub(crate)` so the new `AzureOpenAiProvider` can reuse them:

```rust
// Change from `fn` to `pub(crate) fn`:
pub(crate) fn parse_sse_stream(...)   // line ~255
pub(crate) fn format_api_error(...)   // line ~722

// FunctionCallBuilder is only used internally by parse_sse_stream, no change needed
```

Note: `ResponsesRequest`, `ResponsesInputItem`, `ResponsesInputContent`, `ResponsesTool`, `ResponsesStreamEvent`, `ResponsesOutputItem`, `ResponsesStreamEventResponse`, `ResponsesError`, `ResponsesUsage`, `ResponsesApiErrorEnvelope`, `ResponsesApiErrorBody` are already `pub(crate)`.

Note: `OpenAiChatGptProvider::to_responses_request()` is already `pub(crate)`.

**Step 2: Run existing tests to verify nothing breaks**

Run: `cargo test -p clawhive-provider --lib openai_chatgpt`
Expected: All existing tests PASS

**Step 3: Commit**

```bash
git add crates/clawhive-provider/src/openai_chatgpt.rs
git commit -m "refactor: make Responses API utilities pub(crate) for reuse"
```

---

### Task 2: Create AzureOpenAiProvider

**Files:**
- Create: `crates/clawhive-provider/src/azure_openai.rs`
- Modify: `crates/clawhive-provider/src/lib.rs`

**Step 1: Write failing test**

Add to end of new file `azure_openai.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolDef;

    #[test]
    fn azure_provider_constructs_correct_url() {
        let provider = AzureOpenAiProvider::new(
            "test-key",
            "https://myresource.openai.azure.com/openai/v1",
        );
        // URL should be api_base + /responses
        assert_eq!(
            provider.api_base,
            "https://myresource.openai.azure.com/openai/v1"
        );
    }

    #[test]
    fn azure_provider_trims_trailing_slash() {
        let provider = AzureOpenAiProvider::new(
            "test-key",
            "https://myresource.openai.azure.com/openai/v1/",
        );
        assert_eq!(
            provider.api_base,
            "https://myresource.openai.azure.com/openai/v1"
        );
    }

    #[tokio::test]
    async fn azure_provider_uses_api_key_header() {
        // Verify the provider builds requests with api-key header, not Bearer
        // We test this indirectly by checking the struct stores api_key correctly
        let provider = AzureOpenAiProvider::new("my-azure-key", "https://test.openai.azure.com/openai/v1");
        assert_eq!(provider.api_key, "my-azure-key");
    }

    #[test]
    fn azure_reuses_responses_request_format() {
        use crate::openai_chatgpt::OpenAiChatGptProvider;

        let request = LlmRequest {
            model: "gpt-4o".into(),
            system: Some("Be concise".into()),
            messages: vec![LlmMessage::user("Hello")],
            max_tokens: 128,
            tools: vec![ToolDef {
                name: "get_weather".into(),
                description: "Get weather".into(),
                input_schema: serde_json::json!({"type": "object", "properties": {"location": {"type": "string"}}}),
            }],
        };

        let payload = OpenAiChatGptProvider::to_responses_request(request, true);
        assert_eq!(payload.model, "gpt-4o");
        assert_eq!(payload.instructions.as_deref(), Some("Be concise"));
        assert!(payload.tools.is_some());
        assert!(payload.stream);
    }
}
```

**Step 2: Run test to verify it fails**

Run: `cargo test -p clawhive-provider --lib azure_openai`
Expected: FAIL — `AzureOpenAiProvider` not defined

**Step 3: Write implementation**

Create `crates/clawhive-provider/src/azure_openai.rs`:

```rust
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use reqwest::StatusCode;
use std::pin::Pin;
use futures_core::Stream;
use tokio_stream::StreamExt;

use crate::{ContentBlock, LlmProvider, LlmRequest, LlmResponse, StreamChunk};
use crate::openai_chatgpt::{
    OpenAiChatGptProvider, parse_sse_stream, format_api_error, ResponsesApiErrorEnvelope,
};

#[derive(Debug, Clone)]
pub struct AzureOpenAiProvider {
    client: reqwest::Client,
    pub(crate) api_key: String,
    pub(crate) api_base: String,
}

impl AzureOpenAiProvider {
    pub fn new(api_key: impl Into<String>, api_base: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .build()
                .unwrap_or_default(),
            api_key: api_key.into(),
            api_base: api_base.into().trim_end_matches('/').to_string(),
        }
    }
}

#[async_trait]
impl LlmProvider for AzureOpenAiProvider {
    async fn chat(&self, request: LlmRequest) -> Result<LlmResponse> {
        let url = format!("{}/responses", self.api_base);
        let payload = OpenAiChatGptProvider::to_responses_request(request, true);

        let resp = self
            .client
            .post(&url)
            .header("api-key", &self.api_key)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .json(&payload)
            .send()
            .await?;

        if resp.status() != StatusCode::OK {
            let status = resp.status();
            let text = resp.text().await?;
            let parsed = serde_json::from_str::<ResponsesApiErrorEnvelope>(&text).ok();
            return Err(format_api_error(status, &text, parsed));
        }

        // Collect SSE stream into full response (same logic as OpenAiChatGptProvider)
        let mut full_text = String::new();
        let mut content_blocks: Vec<ContentBlock> = Vec::new();
        let mut input_tokens = None;
        let mut output_tokens = None;
        let mut stop_reason = None;

        let mut stream = std::pin::pin!(parse_sse_stream(resp.bytes_stream()));
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(chunk) => {
                    full_text.push_str(&chunk.delta);
                    if chunk.input_tokens.is_some() {
                        input_tokens = chunk.input_tokens;
                    }
                    if chunk.output_tokens.is_some() {
                        output_tokens = chunk.output_tokens;
                    }
                    if chunk.stop_reason.is_some() {
                        stop_reason = chunk.stop_reason.clone();
                    }
                    for block in chunk.content_blocks {
                        content_blocks.push(block);
                    }
                }
                Err(e) => tracing::warn!("SSE chunk error in azure chat(): {e}"),
            }
        }

        if !full_text.is_empty()
            && !content_blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { .. }))
        {
            content_blocks.insert(0, ContentBlock::Text { text: full_text.clone() });
        }

        let final_stop_reason = if content_blocks
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolUse { .. }))
        {
            Some("tool_use".to_string())
        } else {
            stop_reason.or_else(|| Some("end_turn".to_string()))
        };

        Ok(LlmResponse {
            text: full_text,
            content: content_blocks,
            stop_reason: final_stop_reason,
            input_tokens,
            output_tokens,
        })
    }

    async fn stream(
        &self,
        request: LlmRequest,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamChunk>> + Send>>> {
        let url = format!("{}/responses", self.api_base);
        let payload = OpenAiChatGptProvider::to_responses_request(request, true);

        let resp = self
            .client
            .post(&url)
            .header("api-key", &self.api_key)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .json(&payload)
            .send()
            .await?;

        if resp.status() != StatusCode::OK {
            let status = resp.status();
            let text = resp.text().await?;
            let parsed = serde_json::from_str::<ResponsesApiErrorEnvelope>(&text).ok();
            return Err(format_api_error(status, &text, parsed));
        }

        Ok(Box::pin(parse_sse_stream(resp.bytes_stream())))
    }
}
```

**Step 4: Register module in lib.rs**

Add to `crates/clawhive-provider/src/lib.rs`:
- Add `pub mod azure_openai;` after other module declarations (line ~5 area)
- Add `pub use azure_openai::AzureOpenAiProvider;` in the exports area (line ~21 area)
- Update `create_provider()` AzureOpenAI branch (line ~127-136) to use `AzureOpenAiProvider` instead of `OpenAiProvider`

Change in `create_provider()`:
```rust
ProviderType::AzureOpenAI => {
    let key = config
        .api_key
        .as_ref()
        .ok_or_else(|| anyhow!("azure-openai requires api_key"))?;
    let base_url = config
        .base_url
        .as_ref()
        .ok_or_else(|| anyhow!("azure-openai requires base_url"))?;
    Arc::new(AzureOpenAiProvider::new(key.clone(), base_url.clone()))
}
```

**Step 5: Run tests**

Run: `cargo test -p clawhive-provider --lib azure_openai`
Expected: All tests PASS

**Step 6: Commit**

```bash
git add crates/clawhive-provider/src/azure_openai.rs crates/clawhive-provider/src/lib.rs
git commit -m "feat: add AzureOpenAiProvider using Responses API"
```

---

### Task 3: Register azure-openai in CLI router

**Files:**
- Modify: `crates/clawhive-cli/src/main.rs`

**Step 1: Add azure-openai match arm in build_router_from_config()**

In `build_router_from_config()` (line ~896), add a new match arm before the `_ =>` catch-all:

```rust
"azure-openai" => {
    let api_key = provider_config
        .api_key
        .clone()
        .filter(|k| !k.is_empty());
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
```

Also add the import at the top of main.rs:
```rust
use clawhive_provider::AzureOpenAiProvider;
```

**Step 2: Verify it compiles**

Run: `cargo build -p clawhive-cli`
Expected: Compiles successfully

**Step 3: Commit**

```bash
git add crates/clawhive-cli/src/main.rs
git commit -m "feat: register azure-openai provider in CLI router"
```

---

### Task 4: Add example config and verify full build

**Files:**
- Create: `config/providers.d/azure-openai.yaml.example`

**Step 1: Create example config**

```yaml
provider_id: azure-openai
enabled: false
api_base: https://YOUR-RESOURCE-NAME.openai.azure.com/openai/v1
api_key: ${AZURE_API_KEY}
models:
  - gpt-4o
```

**Step 2: Run full test suite**

Run: `cargo test`
Expected: All tests PASS

**Step 3: Commit**

```bash
git add config/providers.d/azure-openai.yaml.example
git commit -m "docs: add azure-openai example config"
```

---

## Summary of changes

| File | Action | Description |
|------|--------|-------------|
| `crates/clawhive-provider/src/openai_chatgpt.rs` | Modify | Make `parse_sse_stream`, `format_api_error` pub(crate) |
| `crates/clawhive-provider/src/azure_openai.rs` | Create | New `AzureOpenAiProvider` using Responses API |
| `crates/clawhive-provider/src/lib.rs` | Modify | Register module, export, update `create_provider()` |
| `crates/clawhive-cli/src/main.rs` | Modify | Add `"azure-openai"` match arm in router |
| `config/providers.d/azure-openai.yaml.example` | Create | Example config for users |

## User configuration

Users configure Azure OpenAI by adding to their `providers.d/`:

```yaml
provider_id: azure-openai
enabled: true
api_base: https://MY-RESOURCE.openai.azure.com/openai/v1
api_key: ${AZURE_API_KEY}
models:
  - gpt-4o
```

And reference models as `azure-openai/gpt-4o` in agent configs, or add model aliases.
