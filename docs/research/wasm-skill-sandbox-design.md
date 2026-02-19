# nanocrab WASM Skill 沙箱方案设计

> 目标：将 Agent 的 Skill（工具调用）放入 WasmEdge 沙箱执行，实现 capability-based 权限控制。

---

## 1. 结论：完全可行

WasmEdge + WASI 天然支持你要的四种权限场景。下面逐一说明。

---

## 2. 四种权限的实现方式

### 2.1 访问本地目录 → WASI preopened directories（原生支持）

这是 WASI 最成熟的能力。WASM 模块默认**没有任何文件系统访问权限**，必须由宿主在创建实例时显式授予：

```rust
// 宿主侧（nanocrab-runtime）
let mut wasi_module = AsyncWasiModule::create(Some(args), Some(envs), Some(preopens))?;

// preopens 指定 WASM 内可见的目录映射
// 例如：WASM 内的 "/workspace" → 宿主的 "/Users/dragon/workspace/nanocrab/skills/gmail/data"
let preopens = vec![
    ("/workspace", "/actual/host/path/to/skill/data"),
];
```

Skill 内部用标准 Rust `std::fs` API 读写文件，编译到 `wasm32-wasip1` target 后，底层自动走 WASI fd 调用，**只能访问 preopened 的目录**，越界直接报错。

**权限粒度：**
- 可以精确到目录级别
- 可以控制只读/读写（WASI preview2 支持，preview1 通过宿主代理实现）
- 每个 Skill 实例可以有不同的目录映射

### 2.2 发起 HTTP 请求 → WASI socket + 宿主代理（两种方案）

**方案 A：WASI socket（WasmEdge 已支持）**

WasmEdge 扩展了 WASI socket 支持（基于 wasi-socket 提案），Skill 可以在沙箱内直接发起 TCP/HTTP 连接：

```rust
// Skill 侧（编译为 WASM）
// 使用 wasmedge_wasi_socket crate
use wasmedge_wasi_socket::TcpStream;

let mut stream = TcpStream::connect("api.example.com:443")?;
// ... HTTP request
```

⚠️ **问题**：这样 Skill 可以连接任意地址，不够安全。

**方案 B：宿主代理 Host Function（推荐）**

不给 Skill 直接的 socket 权限，而是由宿主导出一个 `http_request` host function：

```rust
// 宿主侧 —— 定义 host function
#[host_function]
fn host_http_request(
    caller: Caller,
    args: Vec<WasmValue>,
) -> Result<Vec<WasmValue>, HostFuncError> {
    // 从 WASM 线性内存中读取请求参数（URL, method, headers, body）
    let memory = caller.memory(0).unwrap();
    let request = deserialize_from_wasm_memory(&memory, &args);

    // ⭐ 宿主在这里做权限检查
    if !policy.is_url_allowed(&request.url) {
        return Err(HostFuncError::User(403));
    }

    // 执行 HTTP 请求
    let response = reqwest::blocking::Client::new()
        .request(request.method, &request.url)
        .headers(request.headers)
        .body(request.body)
        .send()?;

    // 将结果写回 WASM 内存
    serialize_to_wasm_memory(&memory, &response);
    Ok(vec![WasmValue::from_i32(0)])
}
```

**权限粒度：**
- URL 白名单/黑名单
- 限制请求频率
- 限制请求大小
- 审计每一个请求

### 2.3 发送邮件 → 宿主代理 Host Function

邮件不是 WASI 标准能力，**必须通过 host function 代理**：

```rust
// 宿主导出的 host function
#[host_function]
fn host_send_email(caller: Caller, args: Vec<WasmValue>) -> Result<Vec<WasmValue>, HostFuncError> {
    let memory = caller.memory(0).unwrap();
    let email = deserialize_email_from_wasm(&memory, &args);

    // ⭐ 权限检查
    if !policy.can_send_email() {
        return Err(HostFuncError::User(403));
    }
    if !policy.is_recipient_allowed(&email.to) {
        return Err(HostFuncError::User(403));
    }

    // 调用实际邮件服务
    mailer.send(email)?;

    Ok(vec![WasmValue::from_i32(0)])
}
```

### 2.4 访问浏览器 → 宿主代理 Host Function

同理，浏览器操作通过 host function 代理：

```rust
#[host_function]
fn host_browser_navigate(caller: Caller, args: Vec<WasmValue>) -> ...) {
    // 权限检查 → 调用 headless browser → 返回结果
}

#[host_function]
fn host_browser_screenshot(caller: Caller, args: Vec<WasmValue>) -> ...) { ... }

#[host_function]
fn host_browser_eval_js(caller: Caller, args: Vec<WasmValue>) -> ...) { ... }
```

---

## 3. 整体架构

```
┌─────────────────────────────────────────────────────┐
│                    nanocrab Core                     │
│                   (Orchestrator)                     │
│                                                     │
│   LLM 返回 tool_use: { name: "gmail_send", ... }   │
│         │                                           │
│         ▼                                           │
│   ┌─────────────────────┐                           │
│   │   ToolRegistry      │                           │
│   │   找到 gmail skill  │                           │
│   └─────────┬───────────┘                           │
│             │                                       │
│             ▼                                       │
│   ┌─────────────────────────────────────────────┐   │
│   │         WasmSkillRunner                     │   │
│   │                                             │   │
│   │  1. 读取 Skill 的 capability 声明           │   │
│   │  2. 构建 CapabilityPolicy                   │   │
│   │  3. 创建 WasmEdge VM 实例                   │   │
│   │  4. 注册 host functions（按权限过滤）       │   │
│   │  5. 配置 WASI preopens（按权限过滤）        │   │
│   │  6. 加载 Skill 的 .wasm 文件                │   │
│   │  7. 调用 Skill 导出函数                     │   │
│   │  8. 获取结果，销毁 VM 实例                  │   │
│   └─────────────────────────────────────────────┘   │
│                                                     │
│   宿主 Host Functions（注册到 WasmEdge VM）：       │
│   ┌─────────────┬──────────────┬──────────────┐    │
│   │ http_request│ send_email   │ browser_*    │    │
│   │ (带URL白名单)│(带收件人检查)│(带域名限制)  │    │
│   └─────────────┴──────────────┴──────────────┘    │
│                                                     │
│   WASI 能力（创建 VM 时配置）：                     │
│   ┌─────────────┬──────────────┬──────────────┐    │
│   │ preopened   │ env vars     │ args         │    │
│   │ directories │              │              │    │
│   └─────────────┴──────────────┴──────────────┘    │
└─────────────────────────────────────────────────────┘
```

---

## 4. Skill 打包格式

每个 Skill 是一个目录：

```
skills/
  gmail-sender/
    SKILL.md            # 元数据 + 能力描述（给 LLM 看）
    skill.wasm          # 编译好的 WASM 二进制
    manifest.yaml       # 权限声明（给宿主看）
    data/               # Skill 私有数据目录
```

### manifest.yaml（权限声明）

```yaml
name: gmail-sender
version: "0.1.0"
description: "Send emails via Gmail API"

# 编译信息
wasm:
  file: skill.wasm
  export: handle       # 入口函数名

# 权限声明 —— 宿主据此决定注册哪些 host functions
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
        host: "./data"       # 相对于 skill 目录
        mode: "rw"
      - guest: "/tmp"
        host: "$TEMP/gmail-sender"
        mode: "rw"

  email:
    enabled: false           # 这个 skill 用 http 直接调 Gmail API，不需要宿主邮件代理

  browser:
    enabled: false

  network_socket:
    enabled: false           # 不给直接 socket 权限，只走 http host function

# 资源限制
limits:
  max_memory: "64MB"         # WASM 线性内存上限
  max_execution_time: "30s"  # 超时强制终止
  max_fuel: 1000000          # 指令计数限制（可选）
```

### 宿主如何执行权限

```rust
fn create_skill_vm(manifest: &SkillManifest) -> Result<Vm> {
    let mut vm = Vm::new(...);

    // 1. 按 manifest 配置 WASI preopened dirs
    if manifest.capabilities.filesystem.enabled {
        for path in &manifest.capabilities.filesystem.paths {
            wasi_module.add_preopen(path.guest, path.host, path.mode)?;
        }
    }

    // 2. 按 manifest 注册 host functions
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

    // 3. 没有声明的能力 → 不注册对应 host function → WASM 调用时直接 trap

    // 4. 加载 WASM
    let module = Module::from_file(None, &manifest.wasm.file)?;
    vm.register_module(None, module)?;

    Ok(vm)
}
```

---

## 5. Skill 开发侧（guest 代码）

Skill 用 Rust 编写，编译到 `wasm32-wasip1` target。宿主提供一个 SDK crate：

```rust
// nanocrab-skill-sdk（Skill 开发者使用）

/// 发起 HTTP 请求（调用宿主 host function）
pub fn http_request(req: HttpRequest) -> Result<HttpResponse> {
    // 内部通过 extern "C" 调用宿主导出的 host function
    // 参数通过 WASM 共享内存传递
    unsafe {
        let req_bytes = serde_json::to_vec(&req)?;
        let ptr = allocate(req_bytes.len());
        std::ptr::copy(req_bytes.as_ptr(), ptr, req_bytes.len());

        let result_ptr = host_http_request(ptr as i32, req_bytes.len() as i32);
        // ... 读取结果
    }
}

/// 读写文件（直接用 std::fs，走 WASI）
pub use std::fs;

// 宿主 host function 声明
extern "C" {
    fn host_http_request(ptr: i32, len: i32) -> i32;
    fn host_send_email(ptr: i32, len: i32) -> i32;
    fn host_browser_navigate(ptr: i32, len: i32) -> i32;
}
```

Skill 开发者写的代码：

```rust
// skills/gmail-sender/src/lib.rs
use nanocrab_skill_sdk::{http_request, HttpRequest, SkillInput, SkillOutput};

#[no_mangle]
pub extern "C" fn handle(input_ptr: i32, input_len: i32) -> i32 {
    let input: SkillInput = read_input(input_ptr, input_len);

    // 调用 Gmail API（通过宿主代理的 http_request）
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

编译：
```bash
cargo build --target wasm32-wasip1 --release
cp target/wasm32-wasip1/release/gmail_sender.wasm ../skill.wasm
```

---

## 6. 数据传递协议（宿主 ↔ WASM）

WASM 函数只支持基本数值类型（i32/i64/f32/f64），复杂数据需要通过**共享线性内存**传递：

```
宿主调用 Skill：
1. 将 SkillInput 序列化为 JSON bytes
2. 调用 WASM 的 allocate(size) 获取内存指针
3. 将 bytes 写入 WASM 线性内存
4. 调用 WASM 的 handle(ptr, len)
5. 从返回的指针读取 SkillOutput

Skill 调用 Host Function：
1. Skill 将请求序列化为 JSON bytes → 写入自己的线性内存
2. 调用 host_http_request(ptr, len)
3. Host function 从 WASM 内存读取请求 → 执行 → 将结果写回 WASM 内存
4. Skill 从内存读取结果
```

序列化格式建议用 JSON（简单、可调试）或 MessagePack（更紧凑）。

> **替代方案**：WasmEdge 支持 `wasmedge-bindgen`，可以直接传递 String 等高级类型，省去手动内存管理。但灵活度稍差。

---

## 7. 安全保障总结

| 威胁 | 防御机制 |
|------|---------|
| Skill 读取宿主任意文件 | WASI preopened dirs 限制，只能访问声明的目录 |
| Skill 发起任意网络请求 | 不给 socket 权限，只通过 host function 代理 + URL 白名单 |
| Skill 发送恶意邮件 | host function 中做收件人/频率检查 |
| Skill 无限循环/DoS | max_execution_time + max_fuel 指令计数 |
| Skill 消耗过多内存 | WASM 线性内存上限（max_memory） |
| Skill 访问宿主进程内存 | WASM 沙箱天然隔离，不可能访问宿主内存 |
| 恶意 Skill 提权 | 只有 manifest 声明且宿主批准的 host function 被注册 |

---

## 8. 实现路线建议

### Phase 1：最小可跑（1-2 周）
- WasmEdge Rust SDK 集成到 `nanocrab-runtime`
- 实现 `WasmExecutor`：加载 .wasm + 调用导出函数 + 返回结果
- WASI preopened dirs 支持（文件系统权限）
- 一个 hello-world Skill 端到端跑通

### Phase 2：Host Function 代理（1-2 周）
- 实现 `host_http_request` host function
- 实现 `nanocrab-skill-sdk` crate（guest 侧 SDK）
- URL 白名单策略
- manifest.yaml 解析 + 按声明注册 host functions

### Phase 3：完整能力（2-3 周）
- 邮件、浏览器等 host function
- 资源限制（timeout, memory, fuel）
- 审计日志
- Skill 构建工具链（`nanocrab skill build/test/publish`）

### Phase 4：生态（持续）
- Skill marketplace
- 版本管理 + 签名验证
- 热加载（不重启更新 Skill）

---

## 9. WasmEdge 特有优势

- **Linux 基金会项目**，长期维护有保障
- **Rust SDK 成熟**（`wasmedge-sdk` 0.16.1，支持 bundled 静态链接）
- **macOS aarch64 支持**（你的开发环境可以直接跑）
- **async-wasi**：异步 WASI 支持，适合 nanocrab 的 tokio 异步架构
- **WASI-NN 插件**：未来 Skill 可以在沙箱内做本地推理
- **AOT 编译**：性能可以接近原生

---

## 10. 与现有架构的对接

当前 `nanocrab-runtime` 的 `TaskExecutor` trait 需要扩展：

```rust
// 现有
#[async_trait]
pub trait TaskExecutor: Send + Sync {
    async fn execute(&self, input: &str) -> Result<String>;
}

// 扩展为
#[async_trait]
pub trait SkillExecutor: Send + Sync {
    async fn execute(&self, skill_id: &str, input: SkillInput, policy: CapabilityPolicy) -> Result<SkillOutput>;
}

pub struct WasmSkillExecutor {
    skill_registry: HashMap<String, LoadedSkill>,  // skill_id → (Module, Manifest)
}
```

与 vNext 文档中的 `ToolExecutor` trait 和 Capability 模型完全契合。
