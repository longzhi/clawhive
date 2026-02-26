# ChatGPT Responses API Provider Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Replace the broken id_token→API key exchange with a ChatGPT Responses API provider that uses OAuth access_token directly, enabling ChatGPT Plus/Pro users to use OpenAI via `clawhive setup`.

**Architecture:** Create a new `OpenAiChatGptProvider` implementing `LlmProvider` that calls `chatgpt.com/backend-api/codex/responses` with OAuth access_token + chatgpt-account-id header. The existing `OpenAiProvider` remains for API key users. Provider selection happens at startup in `main.rs` based on whether the config has `auth_profile` (→ ChatGPT provider) or `api_key` (→ standard provider).

**Tech Stack:** Rust, reqwest, serde, tokio, async-stream (already in workspace)

---

### Task 1: Add `chatgpt_account_id` to AuthProfile and JWT extraction

**Files:**
- Modify: `crates/clawhive-auth/src/profile.rs` (lines 12-16, OpenAiOAuth variant)
- Modify: `crates/clawhive-auth/src/oauth/openai.rs` (add function + export)
- Modify: `crates/clawhive-auth/src/oauth/mod.rs` (add export)

**Step 1: Update `AuthProfile::OpenAiOAuth` to include `chatgpt_account_id`**

In `crates/clawhive-auth/src/profile.rs`, change the `OpenAiOAuth` variant:

```rust
OpenAiOAuth {
    access_token: String,
    refresh_token: String,
    expires_at: i64,
    #[serde(default)]
    chatgpt_account_id: Option<String>,
},
```

Update tests in the same file — the existing `auth_store_roundtrips_json` test creates an `OpenAiOAuth` — add `chatgpt_account_id: Some("acct_123".to_string())` to the test value.

**Step 2: Add `extract_chatgpt_account_id` function to `openai.rs`**

At the end of `crates/clawhive-auth/src/oauth/openai.rs` (before `#[cfg(test)]`), add:

```rust
/// Extract the ChatGPT account ID from an OAuth access_token JWT.
///
/// Decodes the JWT payload (no signature verification) and checks
/// multiple known claim keys for the account ID.
pub fn extract_chatgpt_account_id(access_token: &str) -> Option<String> {
    let payload = access_token.split('.').nth(1)?;
    // JWT uses URL-safe base64 without padding, but some tokens may have padding stripped
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&decoded).ok()?;

    // Try claim keys in precedence order (zeroclaw reference)
    for key in [
        "https://api.openai.com/auth",  // namespaced claims object
    ] {
        if let Some(auth_obj) = claims.get(key) {
            for sub_key in ["chatgpt_account_id", "account_id"] {
                if let Some(val) = auth_obj.get(sub_key).and_then(|v| v.as_str()) {
                    if !val.trim().is_empty() {
                        return Some(val.to_string());
                    }
                }
            }
        }
    }

    // Fallback: top-level keys
    for key in ["account_id", "accountId", "acct", "sub", "https://api.openai.com/account_id"] {
        if let Some(val) = claims.get(key).and_then(|v| v.as_str()) {
            if !val.trim().is_empty() {
                return Some(val.to_string());
            }
        }
    }

    None
}
```

**Step 3: Export the new function**

In `crates/clawhive-auth/src/oauth/mod.rs`, add `extract_chatgpt_account_id` to the `pub use openai::{ ... }` list.

**Step 4: Add tests for JWT extraction in `openai.rs`**

```rust
#[test]
fn extract_chatgpt_account_id_from_jwt() {
    // Build a fake JWT: header.payload.signature
    let claims = serde_json::json!({
        "https://api.openai.com/auth": {
            "chatgpt_account_id": "acct-test-123"
        },
        "sub": "user-456"
    });
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&claims).unwrap());
    let fake_jwt = format!("eyJhbGciOiJSUzI1NiJ9.{payload}.fakesig");
    assert_eq!(
        extract_chatgpt_account_id(&fake_jwt).as_deref(),
        Some("acct-test-123")
    );
}

#[test]
fn extract_chatgpt_account_id_fallback_to_sub() {
    let claims = serde_json::json!({"sub": "user-789"});
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&claims).unwrap());
    let fake_jwt = format!("eyJhbGciOiJSUzI1NiJ9.{payload}.fakesig");
    assert_eq!(
        extract_chatgpt_account_id(&fake_jwt).as_deref(),
        Some("user-789")
    );
}

#[test]
fn extract_chatgpt_account_id_returns_none_for_garbage() {
    assert!(extract_chatgpt_account_id("not-a-jwt").is_none());
    assert!(extract_chatgpt_account_id("a.b.c").is_none());
}
```

**Step 5: Fix all existing code that constructs `AuthProfile::OpenAiOAuth`**

Search for `AuthProfile::OpenAiOAuth {` across the workspace. Every construction site must add `chatgpt_account_id: None` (or the extracted value). Known locations:
- `crates/clawhive-cli/src/setup.rs` ~line 711
- `crates/clawhive-provider/src/openai.rs` test ~line 779
- `crates/clawhive-auth/src/profile.rs` test ~line 39

The `#[serde(default)]` on the field ensures old JSON profiles deserialize correctly.

**Step 6: Run tests**

```bash
cargo test --workspace
```

**Step 7: Commit**

```
feat(auth): add chatgpt_account_id to OpenAiOAuth profile and JWT extraction
```

---

### Task 2: Update setup flow — remove token exchange, store access_token + account_id

**Files:**
- Modify: `crates/clawhive-cli/src/setup.rs` (lines 681-742, `run_oauth_auth` function)
- Modify: `crates/clawhive-cli/src/setup.rs` (line 8, imports)
- Modify: `crates/clawhive-cli/src/setup.rs` (line 48, `api_base` for OpenAI OAuth)
- Modify: `crates/clawhive-cli/src/setup.rs` (line 783, `generate_provider_yaml` for OAuth)

**Step 1: Update the `run_oauth_auth` function for OpenAI**

Replace the id_token exchange logic (lines ~690-708) with:

```rust
ProviderId::OpenAi => {
    let client_id = "app_EMoamEEZ73f0CkXaXp7hrann";
    let config = OpenAiOAuthConfig::default_with_client(client_id);
    let http = reqwest::Client::new();
    let token = run_openai_pkce_flow(&http, &config).await?;

    let account_id = extract_chatgpt_account_id(&token.access_token);
    if let Some(ref id) = account_id {
        eprintln!("  ✓ ChatGPT account: {id}");
    } else {
        eprintln!("  ⚠ Could not extract chatgpt_account_id from token");
    }

    manager.save_profile(
        &profile_name,
        AuthProfile::OpenAiOAuth {
            access_token: token.access_token,
            refresh_token: token.refresh_token,
            expires_at: unix_timestamp()? + token.expires_in,
            chatgpt_account_id: account_id,
        },
    )?;
}
```

**Step 2: Update imports**

In `setup.rs` line 8, change:
```rust
use clawhive_auth::oauth::{exchange_id_token_for_api_key, profile_from_setup_token, run_openai_pkce_flow, validate_setup_token, OpenAiOAuthConfig};
```
to:
```rust
use clawhive_auth::oauth::{extract_chatgpt_account_id, profile_from_setup_token, run_openai_pkce_flow, validate_setup_token, OpenAiOAuthConfig};
```

**Step 3: Update `generate_provider_yaml` for OAuth**

The `api_base` for OAuth should use ChatGPT backend, not standard API. In `generate_provider_yaml` (~line 782), change the OAuth branch:

```rust
AuthChoice::OAuth { profile_name } => {
    let base = match provider {
        ProviderId::OpenAi => "https://chatgpt.com/backend-api/codex",
        _ => provider.api_base(),
    };
    format!(
        "provider_id: {provider}\nenabled: true\napi_base: {base}\nauth_profile: \"{profile}\"\nmodels:\n  - {model}\n",
        provider = provider.as_str(),
        base = base,
        profile = profile_name,
        model = provider.default_model(),
    )
}
```

**Step 4: Run tests**

```bash
cargo test --workspace
```

Fix any tests that assert on the old OAuth behavior (e.g., the `provider_yaml_uses_auth_profile_for_oauth` test around line 914 — update expected `api_base`).

**Step 5: Commit**

```
feat(setup): use access_token directly for OpenAI OAuth, skip broken token exchange
```

---

### Task 3: Create `OpenAiChatGptProvider` — Responses API implementation

**Files:**
- Create: `crates/clawhive-provider/src/openai_chatgpt.rs`
- Modify: `crates/clawhive-provider/src/lib.rs` (add module + export)

**Step 1: Create the new provider file**

Create `crates/clawhive-provider/src/openai_chatgpt.rs` with:

1. **Request structs** — `ResponsesRequest`, `ResponsesInput`, `ResponsesInputContent`, `ResponsesTextOptions`, `ResponsesReasoningOptions`
2. **Response/SSE structs** — event types for streaming
3. **`OpenAiChatGptProvider`** struct with `access_token`, `chatgpt_account_id`, `api_base`, `client`
4. **`LlmProvider` impl** — `chat()` and `stream()` methods
5. **Message conversion** — `LlmRequest` → `ResponsesRequest` (system prompt → `instructions`, messages → `input`)
6. **SSE parser** — handle `response.output_text.delta`, `response.output_text.done`, `response.completed`, error events

Provider struct:

```rust
#[derive(Debug, Clone)]
pub struct OpenAiChatGptProvider {
    client: reqwest::Client,
    access_token: String,
    chatgpt_account_id: Option<String>,
    api_base: String, // e.g. "https://chatgpt.com/backend-api/codex"
}
```

Key implementation details:
- Endpoint: `{api_base}/responses`
- Headers: `Authorization: Bearer {access_token}`, `OpenAI-Beta: responses=experimental`, `originator: clawhive`, `chatgpt-account-id: {id}`, `accept: text/event-stream`
- System prompt goes to `instructions` field (NOT in `input` array)
- User messages use `input_text` content type
- Assistant messages use `output_text` content type
- Tool calling: NOT supported initially (match zeroclaw — `tools` field omitted)
- Streaming: SSE with `response.output_text.delta` events containing `delta` field
- `store: false`, `stream: true` always
- Model normalization: strip `openai/` prefix if present

For the `chat()` method (non-streaming): send with `stream: false`, parse `ResponsesResponse` with `output_text` shortcut or `output[].content[].text`.

For the `stream()` method: parse SSE events, yield `StreamChunk` for each `response.output_text.delta`, final chunk on `response.completed` or `response.done`.

**Step 2: Add module and export**

In `crates/clawhive-provider/src/lib.rs`:
```rust
pub mod openai_chatgpt;
// ...
pub use openai_chatgpt::OpenAiChatGptProvider;
```

**Step 3: Write tests**

In the same file, write tests for:
- `to_responses_request` — verify message conversion (system → instructions, user/assistant → input with correct content types)
- SSE event parsing — mock events for `response.output_text.delta`, `response.completed`
- Non-streaming response parsing
- Error event handling

**Step 4: Run tests**

```bash
cargo test --workspace
```

**Step 5: Commit**

```
feat(provider): add OpenAiChatGptProvider for Responses API via ChatGPT backend
```

---

### Task 4: Wire up provider selection in main.rs

**Files:**
- Modify: `crates/clawhive-cli/src/main.rs` (~lines 614-629, OpenAI provider init)

**Step 1: Update OpenAI provider initialization**

Currently (lines ~614-629):
```rust
"openai" => {
    let api_key = provider_config.api_key.clone().filter(|k| !k.is_empty()).unwrap_or_default();
    if !api_key.is_empty() {
        let provider = Arc::new(OpenAiProvider::new_with_auth(api_key, provider_config.api_base.clone(), openai_profile.clone()));
        registry.register("openai", provider);
    } else {
        tracing::warn!("OpenAI API key not set, skipping");
    }
}
```

Change to:
```rust
"openai" => {
    let api_key = provider_config.api_key.clone().filter(|k| !k.is_empty()).unwrap_or_default();
    if !api_key.is_empty() {
        // Standard API key path — use chat/completions
        let provider = Arc::new(OpenAiProvider::new_with_auth(
            api_key,
            provider_config.api_base.clone(),
            openai_profile.clone(),
        ));
        registry.register("openai", provider);
    } else if let Some(AuthProfile::OpenAiOAuth { access_token, chatgpt_account_id, .. }) = &openai_profile {
        // OAuth path — use ChatGPT Responses API
        let provider = Arc::new(OpenAiChatGptProvider::new(
            access_token.clone(),
            chatgpt_account_id.clone(),
            provider_config.api_base.clone(),
        ));
        registry.register("openai", provider);
        tracing::info!("OpenAI registered via ChatGPT OAuth (account: {:?})", chatgpt_account_id);
    } else {
        tracing::warn!("OpenAI: no API key and no OAuth profile, skipping");
    }
}
```

Add `use clawhive_provider::OpenAiChatGptProvider;` to imports at top of main.rs.

**Step 2: Run tests and clippy**

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

**Step 3: Commit**

```
feat(start): select ChatGPT provider for OAuth users, standard provider for API key users
```

---

### Task 5: Clean up dead code and unused imports

**Files:**
- Modify: `crates/clawhive-auth/src/oauth/openai.rs` — `exchange_id_token_for_api_key` may become unused externally
- Modify: `crates/clawhive-auth/src/oauth/mod.rs` — update exports
- Modify: `crates/clawhive-cli/src/commands/auth.rs` — if it references token exchange

**Step 1: Check if `exchange_id_token_for_api_key` is still used anywhere**

If no callers remain, either:
- Remove the function and its test, OR
- Keep it but remove the `pub use` export (make it `pub(crate)` or just delete)

**Step 2: Remove diagnostic `eprintln!` statements from previous debugging**

Check `setup.rs` and `commands/auth.rs` for diagnostic `eprintln!` lines about "id_token → API key exchange" and remove them.

**Step 3: Run clippy + tests**

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

**Step 4: Commit**

```
chore: remove dead token-exchange code and diagnostic logging
```

---

### Task 6: Build, deploy, and end-to-end test

**Step 1: Cross-compile for macstudio**

```bash
cargo build --release --target aarch64-apple-darwin
```

**Step 2: Deploy to macstudio**

```bash
# Stop running process first
ssh macstudio 'kill $(cat ~/.clawhive/clawhive.pid 2>/dev/null) 2>/dev/null; sleep 1'
scp target/aarch64-apple-darwin/release/clawhive macstudio:~/.clawhive/clawhive
```

**Step 3: Re-run setup to add OpenAI via OAuth**

```bash
ssh macstudio '~/.clawhive/clawhive setup'
# Choose: Add Provider → OpenAI → OAuth Login
# Complete browser flow
# Verify: profile saved with chatgpt_account_id
```

**Step 4: Start the server and test**

```bash
ssh macstudio '~/.clawhive/clawhive start --port 3001 &'
# Send a test message through Telegram or API
# Verify OpenAI responses work
```

**Step 5: Commit all remaining changes if any**

```
deploy: ChatGPT Responses API provider live on macstudio
```
