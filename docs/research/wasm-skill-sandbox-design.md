# clawhive WASM Skill Sandbox Design

> Goal: Execute Agent Skills (tool calls) in WasmEdge sandbox, achieving capability-based permission control.

---

## 1. Conclusion: Completely Feasible

WasmEdge + WASI natively supports all four permission scenarios you need. Details below.

---

## 2. Implementation of Four Permission Types

### 2.1 Access Local Directories → WASI preopened directories (Native Support)

This is WASI's most mature capability. WASM modules **have no filesystem access by default**, must be explicitly granted by host when creating instance:

```rust
// Host side (clawhive-runtime)
let mut wasi_module = AsyncWasiModule::create(Some(args), Some(envs), Some(preopens))?;

// preopens specifies directory mappings visible inside WASM
// Example: "/workspace" inside WASM → "/Users/dragon/workspace/clawhive/skills/gmail/data" on host
let preopens = vec![
    ("/workspace", "/actual/host/path/to/skill/data"),
];
```

Inside the Skill, use standard Rust `std::fs` API to read/write files. When compiled to `wasm32-wasip1` target, underlying calls go through WASI fd, **can only access preopened directories**, out-of-bounds directly errors.

**Permission granularity:**
- Can be precise to directory level
- Can control read-only/read-write (WASI preview2 supports, preview1 via host proxy)
- Each Skill instance can have different directory mappings

### 2.2 Make HTTP Requests → WASI socket + Host Proxy (Two Approaches)

**Approach A: WASI socket (WasmEdge already supports)**

WasmEdge extended WASI socket support (based on wasi-socket proposal), Skills can directly initiate TCP/HTTP connections inside sandbox:

```rust
// Skill side (compiled to WASM)
// Using wasmedge_wasi_socket crate
use wasmedge_wasi_socket::TcpStream;

let mut stream = TcpStream::connect("api.example.com:443")?;
// ... HTTP request
```

⚠️ **Problem**: This allows Skills to connect to any address, not secure enough.

**Approach B: Host Proxy Host Function (Recommended)**

Don't give Skills direct socket permission, instead host exports an `http_request` host function:

```rust
// Host side — define host function
#[host_function]
fn host_http_request(
    caller: Caller,
    args: Vec<WasmValue>,
) -> Result<Vec<WasmValue>, HostFuncError> {
    // Read request parameters from WASM linear memory (URL, method, headers, body)
    let memory = caller.memory(0).unwrap();
    let request = deserialize_from_wasm_memory(&memory, &args);

    // ⭐ Host does permission check here
    if !policy.is_url_allowed(&request.url) {
        return Err(HostFuncError::User(403));
    }

    // Execute HTTP request
    let response = reqwest::blocking::Client::new()
        .request(request.method, &request.url)
        .headers(request.headers)
        .body(request.body)
        .send()?;

    // Write result back to WASM memory
    serialize_to_wasm_memory(&memory, &response);
    Ok(vec![WasmValue::from_i32(0)])
}
```

**Permission granularity:**
- URL whitelist/blacklist
- Rate limiting
- Request size limits
- Audit every request

### 2.3 Send Email → Host Proxy Host Function

Email is not a WASI standard capability, **must go through host function proxy**:

```rust
// Host-exported host function
#[host_function]
fn host_send_email(caller: Caller, args: Vec<WasmValue>) -> Result<Vec<WasmValue>, HostFuncError> {
    let memory = caller.memory(0).unwrap();
    let email = deserialize_email_from_wasm(&memory, &args);

    // ⭐ Permission check
    if !policy.can_send_email() {
        return Err(HostFuncError::User(403));
    }
    if !policy.is_recipient_allowed(&email.to) {
        return Err(HostFuncError::User(403));
    }

    // Call actual email service
    mailer.send(email)?;

    Ok(vec![WasmValue::from_i32(0)])
}
```

### 2.4 Access Browser → Host Proxy Host Function

Similarly, browser operations go through host function proxy:

```rust
#[host_function]
fn host_browser_navigate(caller: Caller, args: Vec<WasmValue>) -> ...) {
    // Permission check → call headless browser → return result
}

#[host_function]
fn host_browser_screenshot(caller: Caller, args: Vec<WasmValue>) -> ...) { ... }

#[host_function]
fn host_browser_eval_js(caller: Caller, args: Vec<WasmValue>) -> ...) { ... }
```

---

## 3. Overall Architecture

```
┌─────────────────────────────────────────────────────┐
│                    clawhive Core                     │
│                   (Orchestrator)                     │
│                                                     │
│   LLM returns tool_use: { name: "gmail_send", ... } │
│         │                                           │
│         ▼                                           │
│   ┌─────────────────────┐                           │
│   │   ToolRegistry      │                           │
│   │   find gmail skill  │                           │
│   └─────────┬───────────┘                           │
│             │                                       │
│             ▼                                       │
│   ┌─────────────────────────────────────────────┐   │
│   │         WasmSkillRunner                     │   │
│   │                                             │   │
│   │  1. Read Skill's capability declaration     │   │
│   │  2. Build CapabilityPolicy                  │   │
│   │  3. Create WasmEdge VM instance             │   │
│   │  4. Register host functions (filtered by    │   │
│   │     permissions)                            │   │
│   │  5. Configure WASI preopens (filtered by    │   │
│   │     permissions)                            │   │
│   │  6. Load Skill's .wasm file                 │   │
│   │  7. Call Skill exported function            │   │
│   │  8. Get result, destroy VM instance         │   │
│   └─────────────────────────────────────────────┘   │
│                                                     │
│   Host Functions (registered to WasmEdge VM):       │
│   ┌─────────────┬──────────────┬──────────────┐    │
│   │ http_request│ send_email   │ browser_*    │    │
│   │(URL whitelist)│(recipient  │(domain       │    │
│   │             │ check)       │ restriction) │    │
│   └─────────────┴──────────────┴──────────────┘    │
│                                                     │
│   WASI Capabilities (configured at VM creation):   │
│   ┌─────────────┬──────────────┬──────────────┐    │
│   │ preopened   │ env vars     │ args         │    │
│   │ directories │              │              │    │
│   └─────────────┴──────────────┴──────────────┘    │
└─────────────────────────────────────────────────────┘
```

---

## 4. Skill Packaging Format

Each Skill is a directory:

```
skills/
  gmail-sender/
    SKILL.md            # Metadata + capability description (for LLM)
    skill.wasm          # Compiled WASM binary
    manifest.yaml       # Permission declaration (for host)
    data/               # Skill private data directory
```

### manifest.yaml (Permission Declaration)

```yaml
name: gmail-sender
version: "0.1.0"
description: "Send emails via Gmail API"

# Build info
wasm:
  file: skill.wasm
  export: handle       # Entry function name

# Permission declaration — host decides which host functions to register based on this
capabilities:
  http:
    enabled: true
    allowed_hosts:
      - "gmail.googleapis.com"
      - "oauth2.googleapis.com"
    max_request_size: "1MB"
    rate_limit: "10/min"

  filesystem:
    enabled: true
    paths:
      - guest: "/data"
        host: "./data"       # Relative to skill directory
        mode: "rw"
      - guest: "/tmp"
        host: "$TEMP/gmail-sender"
        mode: "rw"

  email:
    enabled: false           # This skill calls Gmail API directly via http, doesn't need host email proxy

  browser:
    enabled: false

  network_socket:
    enabled: false           # No direct socket permission, only through http host function

# Resource limits
limits:
  max_memory: "64MB"         # WASM linear memory limit
  max_execution_time: "30s"  # Timeout forced termination
  max_fuel: 1000000          # Instruction count limit (optional)
```

### How Host Enforces Permissions

```rust
fn create_skill_vm(manifest: &SkillManifest) -> Result<Vm> {
    let mut vm = Vm::new(...);

    // 1. Configure WASI preopened dirs according to manifest
    if manifest.capabilities.filesystem.enabled {
        for path in &manifest.capabilities.filesystem.paths {
            wasi_module.add_preopen(path.guest, path.host, path.mode)?;
        }
    }

    // 2. Register host functions according to manifest
    if manifest.capabilities.http.enabled {
        let policy = HttpPolicy::from(&manifest.capabilities.http);
        vm.register_host_func("env", "http_request",
            make_http_handler(policy))?;
    }

    if manifest.capabilities.email.enabled {
        vm.register_host_func("env", "send_email",
            make_email_handler(...))?;
    }

    if manifest.capabilities.browser.enabled {
        vm.register_host_func("env", "browser_navigate",
            make_browser_handler(...))?;
    }

    // 3. Undeclared capabilities → don't register corresponding host function → WASM call directly traps

    // 4. Load WASM
    let module = Module::from_file(None, &manifest.wasm.file)?;
    vm.register_module(None, module)?;

    Ok(vm)
}
```

---

## 5. Skill Development Side (Guest Code)

Skills are written in Rust, compiled to `wasm32-wasip1` target. Host provides an SDK crate:

```rust
// clawhive-skill-sdk (used by Skill developers)

/// Make HTTP request (calls host function)
pub fn http_request(req: HttpRequest) -> Result<HttpResponse> {
    // Internally calls host-exported host function via extern "C"
    // Parameters passed through WASM shared memory
    unsafe {
        let req_bytes = serde_json::to_vec(&req)?;
        let ptr = allocate(req_bytes.len());
        std::ptr::copy(req_bytes.as_ptr(), ptr, req_bytes.len());

        let result_ptr = host_http_request(ptr as i32, req_bytes.len() as i32);
        // ... read result
    }
}

/// Read/write files (directly use std::fs, goes through WASI)
pub use std::fs;

// Host function declarations
extern "C" {
    fn host_http_request(ptr: i32, len: i32) -> i32;
    fn host_send_email(ptr: i32, len: i32) -> i32;
    fn host_browser_navigate(ptr: i32, len: i32) -> i32;
}
```

Code written by Skill developers:

```rust
// skills/gmail-sender/src/lib.rs
use clawhive_skill_sdk::{http_request, HttpRequest, SkillInput, SkillOutput};

#[no_mangle]
pub extern "C" fn handle(input_ptr: i32, input_len: i32) -> i32 {
    let input: SkillInput = read_input(input_ptr, input_len);

    // Call Gmail API (through host-proxied http_request)
    let resp = http_request(HttpRequest {
        url: "https://gmail.googleapis.com/gmail/v1/users/me/messages/send".into(),
        method: "POST".into(),
        headers: vec![("Authorization", &input.params["token"])],
        body: Some(build_email_body(&input.params)),
    }).unwrap();

    write_output(SkillOutput {
        success: resp.status == 200,
        result: resp.body,
    })
}
```

Build:
```bash
cargo build --target wasm32-wasip1 --release
cp target/wasm32-wasip1/release/gmail_sender.wasm ../skill.wasm
```

---

## 6. Data Transfer Protocol (Host ↔ WASM)

WASM functions only support basic numeric types (i32/i64/f32/f64), complex data needs to be passed through **shared linear memory**:

```
Host calling Skill:
1. Serialize SkillInput to JSON bytes
2. Call WASM's allocate(size) to get memory pointer
3. Write bytes to WASM linear memory
4. Call WASM's handle(ptr, len)
5. Read SkillOutput from returned pointer

Skill calling Host Function:
1. Skill serializes request to JSON bytes → writes to its linear memory
2. Call host_http_request(ptr, len)
3. Host function reads request from WASM memory → executes → writes result back to WASM memory
4. Skill reads result from memory
```

Serialization format recommendation: JSON (simple, debuggable) or MessagePack (more compact).

> **Alternative**: WasmEdge supports `wasmedge-bindgen`, can directly pass String and other high-level types, saves manual memory management. But less flexible.

---

## 7. Security Guarantees Summary

| Threat | Defense Mechanism |
|--------|-------------------|
| Skill reads arbitrary host files | WASI preopened dirs restriction, can only access declared directories |
| Skill makes arbitrary network requests | No socket permission, only through host function proxy + URL whitelist |
| Skill sends malicious emails | Recipient/rate check in host function |
| Skill infinite loop/DoS | max_execution_time + max_fuel instruction count |
| Skill consumes too much memory | WASM linear memory limit (max_memory) |
| Skill accesses host process memory | WASM sandbox naturally isolated, cannot access host memory |
| Malicious Skill privilege escalation | Only host functions declared in manifest and approved by host are registered |

---

## 8. Implementation Roadmap Suggestions

### Phase 1: Minimum Viable (1-2 weeks)
- Integrate WasmEdge Rust SDK into `clawhive-runtime`
- Implement `WasmExecutor`: load .wasm + call exported function + return result
- WASI preopened dirs support (filesystem permissions)
- One hello-world Skill runs end-to-end

### Phase 2: Host Function Proxy (1-2 weeks)
- Implement `host_http_request` host function
- Implement `clawhive-skill-sdk` crate (guest side SDK)
- URL whitelist policy
- manifest.yaml parsing + register host functions according to declarations

### Phase 3: Complete Capabilities (2-3 weeks)
- Email, browser, and other host functions
- Resource limits (timeout, memory, fuel)
- Audit logging
- Skill build toolchain (`clawhive skill build/test/publish`)

### Phase 4: Ecosystem (Ongoing)
- Skill marketplace
- Version management + signature verification
- Hot reload (update Skills without restart)

---

## 9. WasmEdge-Specific Advantages

- **Linux Foundation project**, long-term maintenance guaranteed
- **Mature Rust SDK** (`wasmedge-sdk` 0.16.1, supports bundled static linking)
- **macOS aarch64 support** (runs directly in your dev environment)
- **async-wasi**: Async WASI support, fits clawhive's tokio async architecture
- **WASI-NN plugin**: Future Skills can do local inference inside sandbox
- **AOT compilation**: Performance can approach native

---

## 10. Integration with Existing Architecture

Current `clawhive-runtime`'s `TaskExecutor` trait needs extension:

```rust
// Current
#[async_trait]
pub trait TaskExecutor: Send + Sync {
    async fn execute(&self, input: &str) -> Result<String>;
}

// Extended to
#[async_trait]
pub trait SkillExecutor: Send + Sync {
    async fn execute(&self, skill_id: &str, input: SkillInput, policy: CapabilityPolicy) -> Result<SkillOutput>;
}

pub struct WasmSkillExecutor {
    skill_registry: HashMap<String, LoadedSkill>,  // skill_id → (Module, Manifest)
}
```

Perfectly aligns with the `ToolExecutor` trait and Capability model in the vNext document.
