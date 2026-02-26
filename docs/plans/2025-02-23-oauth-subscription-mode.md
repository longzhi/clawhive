# OAuth Subscription Mode Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Enable users to use OpenAI and Anthropic subscription-based tokens instead of API keys via OAuth and setup-token flows in a single-tenant clawhive instance.

**Architecture:** Introduce an `AuthProfile` system that handles both legacy API keys and new OAuth/Session tokens. Implement a `TokenManager` for persistence, a local callback server for PKCE flows, and update existing providers to inject the correct headers based on the active profile.

**Tech Stack:** Rust, reqwest, serde_json, tokio, axum (for callback), clap (for CLI).

---

### Task 1: Auth Profile Data Structures

**Files:**
- Create: `crates/clawhive-auth/src/lib.rs`
- Create: `crates/clawhive-auth/src/profile.rs`

**Step 1: Define AuthProfile and Token models**

```rust
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(tag = "type")]
pub enum AuthProfile {
    ApiKey {
        provider_id: String,
        api_key: String,
    },
    OpenAiOAuth {
        access_token: String,
        refresh_token: String,
        expires_at: i64,
    },
    AnthropicSession {
        session_token: String,
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AuthStore {
    pub active_profile: Option<String>,
    pub profiles: HashMap<String, AuthProfile>,
}
```

**Step 2: Add serialization tests**

Verify that `AuthStore` correctly serializes to and from the `auth-profiles.json` format.

**Step 3: Commit**

```bash
git add crates/clawhive-auth
git commit -m "feat(auth): add AuthProfile and AuthStore data structures"
```

---

### Task 2: Token Storage Manager

**Files:**
- Create: `crates/clawhive-auth/src/manager.rs`

**Step 1: Implement TokenManager**

Create a `TokenManager` that handles loading/saving from `~/.config/clawhive/auth-profiles.json`. Implement `get_active_profile()` and `save_profile()`.

**Step 2: Write tests for file I/O**

Ensure the manager creates the directory if it doesn't exist and handles invalid JSON gracefully.

**Step 3: Commit**

```bash
git add crates/clawhive-auth/src/manager.rs
git commit -m "feat(auth): implement TokenManager for profile persistence"
```

---

### Task 3: Local OAuth Callback Server

**Files:**
- Create: `crates/clawhive-auth/src/oauth/server.rs`

**Step 1: Implement Axum server for callback**

Set up a temporary HTTP server on `127.0.0.1:1455` with a route `GET /auth/callback` to capture the `code` and `state` parameters from OpenAI.

**Step 2: Add shutdown mechanism**

The server should shut down automatically after receiving a valid callback or a timeout.

**Step 3: Commit**

```bash
git add crates/clawhive-auth/src/oauth/server.rs
git commit -m "feat(auth): add local OAuth callback server"
```

---

### Task 4: OpenAI PKCE OAuth Flow

**Files:**
- Create: `crates/clawhive-auth/src/oauth/openai.rs`

**Step 1: Implement PKCE generation**

Generate `code_verifier` and `code_challenge` using S256 as per RFC 7636.

**Step 2: Implement authorization URL and token exchange**

1. Build authorize URL: `https://auth.openai.com/oauth/authorize`.
2. Open browser for user.
3. Exchange `code` for tokens at `https://auth.openai.com/oauth/token`.

**Step 3: Commit**

```bash
git add crates/clawhive-auth/src/oauth/openai.rs
git commit -m "feat(auth): implement OpenAI PKCE OAuth flow"
```

---

### Task 5: Anthropic Setup-Token Flow

**Files:**
- Create: `crates/clawhive-auth/src/oauth/anthropic.rs`

**Step 1: Implement setup-token capture**

Provide a CLI prompt for users to paste their Claude Code setup-token. 

**Step 2: Validate and store**

Optionally verify the token against Anthropic's health endpoint before saving to `AnthropicSession` profile.

**Step 3: Commit**

```bash
git add crates/clawhive-auth/src/oauth/anthropic.rs
git commit -m "feat(auth): implement Anthropic setup-token flow"
```

---

### Task 6: Token Auto-Refresh Logic

**Files:**
- Modify: `crates/clawhive-auth/src/manager.rs`

**Step 1: Implement refresh_if_needed()**

Check `expires_at` for OpenAI OAuth profiles. If expired (or expiring in < 5 mins), use `refresh_token` to get new tokens.

**Step 2: Implement file locking**

Use `fd-lock` or similar to prevent race conditions during refresh if multiple clawhive processes are running.

**Step 3: Commit**

```bash
git add crates/clawhive-auth/src/manager.rs
git commit -m "feat(auth): add token auto-refresh with file locking"
```

---

### Task 7: CLI Auth Commands

**Files:**
- Create: `crates/clawhive-cli/src/commands/auth.rs`

**Step 1: Implement `auth login`**

Add subcommands for `openai` and `anthropic`. Trigger the respective flows from Task 4 and 5.

**Step 2: Implement `auth status` and `logout`**

Display current active profile and provide a way to clear stored credentials.

**Step 3: Commit**

```bash
git add crates/clawhive-cli/src/commands/auth.rs
git commit -m "feat(cli): add auth login, status, and logout commands"
```

---

### Task 8: Provider Integration

**Files:**
- Modify: `crates/clawhive-provider/src/openai.rs`
- Modify: `crates/clawhive-provider/src/anthropic.rs`

**Step 1: Inject AuthProfile into providers**

Update provider constructors to accept an optional `AuthProfile`.

**Step 2: Update header logic**

- For OpenAI OAuth: use `Authorization: Bearer {access_token}`.
- For Anthropic Session: use appropriate session headers (check OpenClaw reference for specific header names).

**Step 3: Commit**

```bash
git add crates/clawhive-provider
git commit -m "feat(provider): adapt providers to use OAuth/Session tokens"
```

---

### Task 9: Web UI Auth Status

**Files:**
- Modify: `frontend/src/pages/Providers.tsx` (assuming path)
- Create: `crates/clawhive-api/src/routes/auth.rs`

**Step 1: Add API endpoints for auth status**

Create backend routes to expose `active_profile` info to the frontend.

**Step 2: Update UI**

Show a "Login" button for OpenAI/Anthropic that directs the user to perform CLI login or opens the local OAuth flow. Show "Connected" status for active profiles.

**Step 3: Commit**

```bash
git add frontend/ crates/clawhive-api
git commit -m "feat(ui): display auth status on provider page"
```
