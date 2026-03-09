# Model Metadata & Discovery Enhancement Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Enrich provider presets with model metadata (context_window, max_output_tokens, reasoning, vision) and enable API-based model discovery for all OpenAI-compatible providers.

**Architecture:** Replace `&[&str]` model lists in `ProviderPreset` with `&[ModelPresetInfo]` containing rich metadata. Make `list_models()` functional for all OpenAI-compat providers (they already use `OpenAiProvider` which has the implementation). Add model filtering to exclude non-chat models. Wire metadata through to `ContextConfig` so the orchestrator adapts to each model's actual context window.

**Tech Stack:** Rust (clawhive-schema, clawhive-provider, clawhive-core, clawhive-server), React/TypeScript (web/)

---

## Background

Currently:
- `ProviderPreset.models` is `&[&str]` — just model ID strings, no metadata
- `ContextConfig` hardcodes `128_000` tokens for all models (a 128k model and an 8k model get the same treatment)
- `list_models()` only works for Anthropic and OpenAI providers; OpenAI-compat providers (DeepSeek, Qwen, Groq, etc.) return empty `vec![]` despite using `OpenAiProvider` which has the implementation
- API-fetched models include embeddings, moderation, and legacy models — no filtering
- `ModelPolicy` has no `max_tokens` or `context_window` — the orchestrator can't adapt

## Key files

- `crates/clawhive-schema/src/provider_presets.rs` — `ProviderPreset`, `PROVIDER_PRESETS`
- `crates/clawhive-provider/src/lib.rs` — `LlmProvider` trait, `create_provider()`, `ProviderType`
- `crates/clawhive-provider/src/types.rs` — `LlmRequest`, `LlmResponse`
- `crates/clawhive-provider/src/openai.rs` — `OpenAiProvider`, `list_models()`
- `crates/clawhive-provider/src/anthropic.rs` — `AnthropicProvider`, `list_models()`
- `crates/clawhive-provider/src/openai_compat.rs` — factory functions (deepseek, qwen, ollama, etc.)
- `crates/clawhive-core/src/lib.rs` — `ModelPolicy`, `AgentConfig`
- `crates/clawhive-core/src/context.rs` — `ContextConfig`, `ContextConfig::for_model()`
- `crates/clawhive-core/src/orchestrator.rs` — uses `ContextConfig::default()` at line ~206
- `crates/clawhive-server/src/routes/setup.rs` — `list_models_handler`, `ListModelsResponse`
- `web/src/pages/Setup.tsx` — model selection UI, "Fetch from API" button
- `web/src/hooks/use-api.ts` — `useListModels` hook

---

### Task 1: Add `ModelPresetInfo` struct to clawhive-schema

**Files:**
- Modify: `crates/clawhive-schema/src/provider_presets.rs`

**Step 1: Add the `ModelPresetInfo` struct**

Add above `ProviderPreset`:

```rust
/// Metadata for a known model within a provider preset.
#[derive(Debug, Clone, Serialize)]
pub struct ModelPresetInfo {
    pub id: &'static str,
    /// Context window size in tokens.
    pub context_window: u32,
    /// Maximum output tokens the model can generate.
    pub max_output_tokens: u32,
    /// Whether this is a reasoning/thinking model (o3, deepseek-reasoner, etc.)
    pub reasoning: bool,
    /// Whether the model supports image/vision input.
    pub vision: bool,
}
```

**Step 2: Change `ProviderPreset.models` type**

```rust
pub struct ProviderPreset {
    pub id: &'static str,
    pub name: &'static str,
    pub api_base: &'static str,
    pub needs_key: bool,
    pub needs_base_url: bool,
    pub default_model: &'static str,
    pub models: &'static [ModelPresetInfo],  // was: &'static [&'static str]
}
```

**Step 3: Update `provider_models_for_id` to use new type**

```rust
pub fn provider_models_for_id(provider_id: &str) -> Vec<String> {
    match preset_by_id(provider_id) {
        Some(p) => p
            .models
            .iter()
            .map(|m| format!("{}/{}", provider_id, m.id))
            .collect(),
        None => vec![],
    }
}
```

**Step 4: Add a lookup helper**

```rust
/// Look up model metadata by provider id and model id.
pub fn model_info(provider_id: &str, model_id: &str) -> Option<&'static ModelPresetInfo> {
    preset_by_id(provider_id)
        .and_then(|p| p.models.iter().find(|m| m.id == model_id))
}
```

**Step 5: Commit**

```
feat(schema): add ModelPresetInfo with context_window, reasoning, vision metadata
```

---

### Task 2: Populate model metadata for all 15 providers

**Files:**
- Modify: `crates/clawhive-schema/src/provider_presets.rs`

**Step 1: Replace all `PROVIDER_PRESETS` entries**

Convert every `models: &[&str]` to `models: &[ModelPresetInfo]`. Reference data:

| Provider | Model | context_window | max_output_tokens | reasoning | vision |
|----------|-------|---------------|-------------------|-----------|--------|
| **anthropic** | claude-opus-4-6 | 200000 | 32768 | false | true |
| | claude-sonnet-4-6 | 200000 | 16384 | false | true |
| | claude-opus-4-5 | 200000 | 32768 | false | true |
| | claude-sonnet-4-5 | 200000 | 16384 | false | true |
| | claude-haiku-4-5 | 200000 | 8192 | false | true |
| **openai** | gpt-5.2 | 200000 | 16384 | false | true |
| | gpt-5.2-pro | 200000 | 32768 | false | true |
| | gpt-5 | 128000 | 16384 | false | true |
| | gpt-5-pro | 128000 | 32768 | false | true |
| | gpt-5-mini | 128000 | 16384 | false | true |
| | o3-pro | 200000 | 100000 | true | true |
| **azure-openai** | gpt-5.3-codex | 200000 | 16384 | false | false |
| | gpt-5.2 | 200000 | 16384 | false | true |
| | gpt-5.2-codex | 200000 | 16384 | false | false |
| | gpt-5.1-codex-max | 200000 | 32768 | false | false |
| | o3-pro | 200000 | 100000 | true | true |
| **gemini** | gemini-2.5-pro | 1000000 | 65536 | false | true |
| | gemini-2.5-flash | 1000000 | 65536 | false | true |
| | gemini-2.0-flash | 1000000 | 8192 | false | true |
| **deepseek** | deepseek-chat | 65536 | 8192 | false | false |
| | deepseek-reasoner | 65536 | 8192 | true | false |
| **groq** | llama-3.3-70b-versatile | 128000 | 32768 | false | false |
| | llama-3.1-8b-instant | 128000 | 8192 | false | false |
| **ollama** | llama3.2 | 128000 | 8192 | false | false |
| | qwen2.5-coder | 32768 | 8192 | false | false |
| | mistral | 32768 | 8192 | false | false |
| **openrouter** | openai/gpt-5.3-codex | 200000 | 16384 | false | false |
| | anthropic/claude-opus-4-6 | 200000 | 32768 | false | true |
| | google/gemini-2.5-pro | 1000000 | 65536 | false | true |
| | openai/gpt-5.2 | 200000 | 16384 | false | true |
| **together** | meta-llama/Llama-3.3-70B-Instruct-Turbo | 128000 | 8192 | false | false |
| | meta-llama/Llama-4-Scout-17B-16E-Instruct | 512000 | 8192 | false | true |
| **fireworks** | .../llama-v3p3-70b-instruct | 128000 | 8192 | false | false |
| | .../llama4-scout-instruct-basic | 128000 | 8192 | false | true |
| **qwen** | qwen-max | 32768 | 8192 | false | false |
| | qwen-plus | 131072 | 8192 | false | false |
| | qwen-turbo | 131072 | 8192 | false | false |
| | qwen-long | 1000000 | 8192 | false | false |
| **moonshot** | moonshot-v1-128k | 128000 | 8192 | false | false |
| | moonshot-v1-32k | 32768 | 8192 | false | false |
| | moonshot-v1-8k | 8192 | 4096 | false | false |
| **zhipu** | glm-4-plus | 128000 | 4096 | false | true |
| | glm-4-flash | 128000 | 4096 | false | true |
| | glm-4-long | 1000000 | 4096 | false | false |
| | glm-4 | 128000 | 4096 | false | true |
| **minimax** | MiniMax-Text-01 | 1000000 | 8192 | false | false |
| | abab6.5s-chat | 245760 | 8192 | false | false |
| **volcengine** | doubao-pro-128k | 128000 | 4096 | false | false |
| | doubao-pro-32k | 32768 | 4096 | false | false |
| | doubao-lite-128k | 128000 | 4096 | false | false |
| **qianfan** | ernie-4.0-8k | 8192 | 4096 | false | false |
| | ernie-4.0-turbo-8k | 8192 | 4096 | false | false |
| | ernie-3.5-8k | 8192 | 4096 | false | false |

Example entry:

```rust
ProviderPreset {
    id: "moonshot",
    name: "Moonshot AI",
    api_base: "https://api.moonshot.cn/v1",
    needs_key: true,
    needs_base_url: false,
    default_model: "moonshot-v1-128k",
    models: &[
        ModelPresetInfo { id: "moonshot-v1-128k", context_window: 128000, max_output_tokens: 8192, reasoning: false, vision: false },
        ModelPresetInfo { id: "moonshot-v1-32k", context_window: 32768, max_output_tokens: 8192, reasoning: false, vision: false },
        ModelPresetInfo { id: "moonshot-v1-8k", context_window: 8192, max_output_tokens: 4096, reasoning: false, vision: false },
    ],
},
```

**Step 2: Fix any compilation errors from callers**

Search workspace for `.models` access on `ProviderPreset` and update if they index by string. Key callers:
- `provider_models_for_id()` — already updated in Task 1
- `web/src/pages/Setup.tsx` — reads `preset.models` from JSON; needs frontend update (Task 6)
- `crates/clawhive-cli/src/setup.rs` — reads `preset.models` for CLI display

**Step 3: Run `cargo clippy --workspace --all-targets -- -D warnings`**

**Step 4: Commit**

```
feat(schema): populate model metadata for all 15 providers
```

---

### Task 3: Update CLI setup to use ModelPresetInfo

**Files:**
- Modify: `crates/clawhive-cli/src/setup.rs`

**Step 1: Update model selection display in CLI**

Find where `preset.models` is used in setup.rs for model selection. Change from displaying `&str` to displaying `ModelPresetInfo.id`. Add context_window info in the display:

```rust
// In model selection, show: "gpt-5.2 (200k ctx, vision)"
let model_labels: Vec<String> = preset.models.iter().map(|m| {
    let ctx = if m.context_window >= 1_000_000 {
        format!("{}M", m.context_window / 1_000_000)
    } else {
        format!("{}k", m.context_window / 1000)
    };
    let mut tags = vec![format!("{ctx} ctx")];
    if m.reasoning { tags.push("reasoning".into()); }
    if m.vision { tags.push("vision".into()); }
    format!("{} ({})", m.id, tags.join(", "))
}).collect();
```

**Step 2: Run `cargo test -p clawhive-cli`**

**Step 3: Commit**

```
feat(cli): show model metadata in setup model selection
```

---

### Task 4: Update server list-models endpoint to return metadata

**Files:**
- Modify: `crates/clawhive-server/src/routes/setup.rs`

**Step 1: Add `ModelInfoResponse` struct**

```rust
#[derive(Serialize)]
struct ModelInfoResponse {
    id: String,
    context_window: Option<u32>,
    max_output_tokens: Option<u32>,
    reasoning: bool,
    vision: bool,
}

#[derive(Serialize)]
struct ListModelsResponse {
    models: Vec<ModelInfoResponse>,
}
```

**Step 2: Update `list_models_handler` to merge API results with preset metadata**

```rust
async fn list_models_handler(
    Json(req): Json<ListModelsRequest>,
) -> Result<Json<ListModelsResponse>, axum::http::StatusCode> {
    // ... existing provider creation code ...

    let api_models = provider.list_models().await.unwrap_or_default();
    let provider_id = &req.provider_type;

    let models: Vec<ModelInfoResponse> = if api_models.is_empty() {
        // Fallback to static presets
        clawhive_schema::provider_presets::preset_by_id(provider_id)
            .map(|p| p.models.iter().map(|m| ModelInfoResponse {
                id: m.id.to_string(),
                context_window: Some(m.context_window),
                max_output_tokens: Some(m.max_output_tokens),
                reasoning: m.reasoning,
                vision: m.vision,
            }).collect())
            .unwrap_or_default()
    } else {
        // Merge API results with preset metadata
        api_models.into_iter()
            .filter(|id| !is_non_chat_model(id))
            .map(|id| {
                let preset_info = clawhive_schema::provider_presets::model_info(provider_id, &id);
                ModelInfoResponse {
                    context_window: preset_info.map(|p| p.context_window),
                    max_output_tokens: preset_info.map(|p| p.max_output_tokens),
                    reasoning: preset_info.map(|p| p.reasoning).unwrap_or(false),
                    vision: preset_info.map(|p| p.vision).unwrap_or(false),
                    id,
                }
            })
            .collect()
    };

    Ok(Json(ListModelsResponse { models }))
}
```

**Step 3: Add `is_non_chat_model` filter function**

```rust
/// Filter out non-chat models (embeddings, moderation, tts, etc.)
fn is_non_chat_model(model_id: &str) -> bool {
    let id = model_id.to_lowercase();
    id.contains("embed")
        || id.contains("moderation")
        || id.contains("tts")
        || id.contains("whisper")
        || id.contains("dall-e")
        || id.contains("davinci")
        || id.contains("babbage")
}
```

**Step 4: Run `cargo clippy -p clawhive-server -- -D warnings`**

**Step 5: Commit**

```
feat(server): return model metadata from list-models endpoint and filter non-chat models
```

---

### Task 5: Enable list_models() for all OpenAI-compat providers

**Files:**
- Modify: `crates/clawhive-provider/src/openai_compat.rs` — no code change needed (already returns `OpenAiProvider`)
- Verify: `crates/clawhive-provider/src/lib.rs` — `create_provider()` routes to correct type

**Step 1: Verify OpenAI-compat providers already support list_models**

All functions in `openai_compat.rs` return `OpenAiProvider` which has `list_models()` implemented. Verify by checking `create_provider()` in `lib.rs` — all OpenAI-compat `ProviderType` variants (DeepSeek, Groq, Qwen, Moonshot, etc.) should create their provider via the compat functions, which return `OpenAiProvider`.

Read `crates/clawhive-provider/src/lib.rs` lines 160+ to confirm all compat types call the right constructor.

**Step 2: Write a test to verify**

Add to `crates/clawhive-provider/src/openai_compat.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::LlmProvider;

    #[test]
    fn compat_providers_are_openai_provider() {
        // All compat providers should be OpenAiProvider instances
        // which means they inherit list_models()
        let provider = deepseek("test-key");
        // Verify it implements LlmProvider (compile-time check)
        let _: Box<dyn LlmProvider> = Box::new(provider);
    }
}
```

**Step 3: Commit**

```
test(provider): verify openai-compat providers support list_models
```

---

### Task 6: Update Web UI to show model metadata

**Files:**
- Modify: `web/src/hooks/use-api.ts`
- Modify: `web/src/pages/Setup.tsx`

**Step 1: Update `ListModelsResponse` type in use-api.ts**

```typescript
interface ModelInfo {
  id: string;
  context_window?: number;
  max_output_tokens?: number;
  reasoning: boolean;
  vision: boolean;
}

// Update useListModels to return ModelInfo[]
```

**Step 2: Update Setup.tsx model dropdown to show metadata**

In the model selection section, display context window and capability badges:

```tsx
// Model option label: "gpt-5.2 · 200k ctx · vision"
const formatModelLabel = (model: ModelInfo) => {
  const parts = [model.id];
  if (model.context_window) {
    const ctx = model.context_window >= 1_000_000
      ? `${(model.context_window / 1_000_000).toFixed(0)}M`
      : `${Math.round(model.context_window / 1000)}k`;
    parts.push(`${ctx} ctx`);
  }
  if (model.reasoning) parts.push('reasoning');
  if (model.vision) parts.push('vision');
  return parts.join(' · ');
};
```

**Step 3: Update preset model display**

When showing preset models (before API fetch), read from the new `ModelPresetInfo` structure. The backend `GET /api/setup/provider-presets` already serializes `models` — update frontend to read `models[].id` instead of `models[]` (string).

**Step 4: Run `bun run build` in web/**

**Step 5: Commit**

```
feat(web): display model metadata in setup model selector
```

---

### Task 7: Wire model metadata to ContextConfig

**Files:**
- Modify: `crates/clawhive-core/src/orchestrator.rs`
- Modify: `crates/clawhive-core/src/lib.rs` (ModelPolicy)

**Step 1: Add optional context_window to ModelPolicy**

In `crates/clawhive-core/src/lib.rs`:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPolicy {
    pub primary: String,
    #[serde(default)]
    pub fallbacks: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<ThinkingLevel>,
    /// Override context window (auto-resolved from model presets if not set)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
}
```

**Step 2: Resolve context_window from preset in orchestrator**

In `crates/clawhive-core/src/orchestrator.rs`, where `ContextConfig::default()` is used (~line 206), resolve from model metadata:

```rust
let context_config = {
    // Try to extract provider/model from "provider/model" format
    let context_window = agent_config.model_policy.context_window.or_else(|| {
        let primary = &agent_config.model_policy.primary;
        let parts: Vec<&str> = primary.splitn(2, '/').collect();
        if parts.len() == 2 {
            clawhive_schema::provider_presets::model_info(parts[0], parts[1])
                .map(|m| m.context_window)
        } else {
            None
        }
    });
    match context_window {
        Some(cw) => super::context::ContextConfig::for_model(cw as usize),
        None => super::context::ContextConfig::default(),
    }
};
```

**Step 3: Update all ModelPolicy constructions to add `context_window: None`**

Search workspace for `ModelPolicy {` and add `context_window: None` to each. Key locations:
- `crates/clawhive-core/src/config.rs`
- `crates/clawhive-core/src/subagent.rs`
- `crates/clawhive-core/src/subagent_tool.rs`
- `crates/clawhive-core/tests/*.rs`
- `crates/clawhive-gateway/src/lib.rs`

**Step 4: Write test**

```rust
#[test]
fn context_config_resolves_from_model_preset() {
    let info = clawhive_schema::provider_presets::model_info("moonshot", "moonshot-v1-8k");
    assert!(info.is_some());
    let info = info.unwrap();
    assert_eq!(info.context_window, 8192);
    let config = super::context::ContextConfig::for_model(info.context_window as usize);
    assert_eq!(config.max_tokens, 8192);
}
```

**Step 5: Run `cargo test --workspace` and `cargo clippy --workspace --all-targets -- -D warnings`**

**Step 6: Commit**

```
feat(core): resolve ContextConfig from model preset metadata
```

---

### Task 8: Final verification

**Step 1: Run full CI check**

```bash
just check
# Equivalent to:
#   cargo fmt --all -- --check
#   cargo clippy --workspace --all-targets -- -D warnings
#   cargo test --workspace
```

**Step 2: Run web build**

```bash
cd web && bun run build
```

**Step 3: Manual smoke test (optional)**

```bash
cargo run -- setup
# Verify model selection shows metadata
# Verify "Fetch from API" works for OpenAI-compat providers
```

**Step 4: Commit any remaining fixes**

```
chore: final cleanup for model metadata enhancement
```
