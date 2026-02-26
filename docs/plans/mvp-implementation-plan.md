Task continued and completed in 6m 28s.

---

The user confirmed all my suggestions and wants the full implementation plan. Let me create a comprehensive parallel task graph plan with waves, dependencies, and specific implementation details. I'll save it to `docs/plans/` as the writing-plans skill suggests.

Let me organize this into waves based on dependency ordering:

**Wave 0 (Foundation)**: No dependencies on each other
- T1: Config system (parse all YAML files)
- T2: Schema expansion (more event types, session key)
- T3: Memory crate (SQLite setup, tables, CRUD)
- T4: Real Anthropic provider (reqwest HTTP calls)

**Wave 1 (Integration)**: Depends on Wave 0


- T5: EventBus rewrite (topic-based routing) - depends on T2
- T6: Session management - depends on T2, T3
- T7: Persona loading - depends on T1

**Wave 2 (Core rewiring)**: Depends on Wave 1
- T8: Orchestrator rewrite (memory injection, persona, session, bus integration) - depends on T1, T3, T5, T6, T7
- T9: Gateway rewrite (bus-driven, routing.yaml) - depends on T1, T5, T8

**Wave 3 (Channels & CLI)**: Depends on Wave 2
- T10: Real Telegram bot (teloxide) - depends on T9
- T11: CLI (start + chat commands) - depends on T1, T8, T9, T10

**Wave 4 (M2 - Memory features)**: Depends on Wave 3
- T12: Episode write on every message - depends on T3, T8
- T13: Episode search + concept query - depends on T3
- T14: Memory injection before LLM call - depends on T8, T13

**Wave 5 (M3 - Evolution)**: Depends on Wave 4
- T15: Daily consolidation cron - depends on T3, T4
- T16: Conflict/forgetting - depends on T15
- T17: Runtime WASM skeleton - standalone
- T18: Sub-agent basic - depends on T8
- T19: TUI minimal - standalone

Now I'm reorganizing the dependency structure more carefully. I see that Wave 0 should be completely parallel with no dependencies: config system, schema expansion for new Event variants and SessionKey, real Anthropic provider using reqwest, and SQLite memory foundation with tables and CRUD operations. Then Wave 1 builds on that foundation with EventBus rewrite needing the new Event types, session manager needing both SessionKey and memory layer, and persona loader needing config paths. Wave 2 brings it together with the orchestrator rewrite depending on all the Wave 0 and Wave 1 pieces.

Continuing through the remaining waves, the gateway rewrite depends on config, EventBus, and orchestrator, then endpoints follow with Telegram bot and CLI. The memory features in Wave 4 handle episode recording, search and retrieval, and injection. Wave 5 covers the evolution features like consolidation, conflict detection, the WASM skeleton, sub-agents, and a minimal TUI. Let me reconsider this structure...

Actually, I need to think through the config system more carefully. There's no dedicated config crate right now, so I need to decide where config types and parsing should live—either create a new `clawhive-config` crate, add them to the schema crate as shared types, or put them in core since that's what orchestrates everything. A dedicated config crate is cleanest architecturally, but it requires workspace changes. Putting config in schema makes sense since it's already the shared types layer, though core as the orchestrator also has merit.

The real constraint is that both the CLI and telegram crate need config access, and the CLI already depends on core, so I can have telegram depend on core too. I think the right split is storing config structs in schema and putting the config loading logic in a module within core—that keeps the shared types separate from the loading mechanism.

Now I need to flesh out the actual plan with concrete details: specific file paths, the code I'll write, test cases, commands to run, and commit messages. I should also think about how to structure this for potential task delegation, breaking it into categories based on what skills or focus areas each piece requires.

Let me organize this as a comprehensive implementation plan that I can present in full, structured by implementation waves with clear deliverables for each phase. Now I'm mapping out Wave 3 with the remaining evolution tasks, then diving into the config system implementation. I'm looking at the YAML structure for the main config file to understand how app settings, runtime parameters, features, and channel connectors are organized.

Defining the routing and provider configurations...

Now I'm thinking through the Rust struct definitions needed to deserialize these YAML configs. For the provider setup, I need to integrate `reqwest` to make actual HTTP calls to the Anthropic API instead of using mock implementations. The `ProviderRegistry` should store configured client instances rather than just factories, so each provider has its connection details and authentication ready to go. For the event bus, I'm starting to outline how messages flow through the system, but I need to figure out the exact architecture there. Keep mpsc internally but add topic-based subscription where the bus manages multiple channels and routes events to subscribers by topic. For sessions, I'll generate a key from the 5-tuple and store it in SQLite with TTL management. Memory uses SQLite with three tables for episodes, concepts, and links, wrapping CRUD operations in spawn_blocking with a connection pool. The orchestrator accepts memory, session, persona, and bus, then on inbound messages generates the session key, loads or creates the session, queries memory, assembles context, calls the LLM, writes the episode, and publishes events through the bus. The gateway parses routing rules to match incoming messages to agents, and integrates with the bus by subscribing to ReplyReady events and publishing HandleIncomingMessage. For Telegram, I'm using teloxide with polling to create InboundMessage objects and send them through the gateway, then listening for ReplyReady events to send responses back via the bot.

Now I'm laying out the complete implementation plan with specific crates, files, dependencies, and concrete deliverables for each component.
我正在使用 **writing-plans** skill 来创建完整的实现计划。

Dragon，以下是 clawhive MVP 完整的并行任务图实现计划。

---

# clawhive MVP Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** 将 clawhive 从 ~600 行半成品推进为可工作的端到端多 Agent AI 框架（M1 可跑通 + M2 可用 + M3 可演进）

**Architecture:** Bus-centric event-driven architecture. Telegram -> Gateway -> Bus(Command) -> Core/Orchestrator -> Memory/Provider -> Bus(Event) -> Gateway -> Telegram. 所有模块通过 EventBus topic routing 解耦。Memory 使用 SQLite 双层（episodes + concepts + links）。

**Tech Stack:** Rust 2021, tokio, teloxide, rusqlite (bundled), reqwest, serde_yaml, ratatui, clap

---

## Dependency Graph (ASCII)

```
Wave 0 (Foundation)          Wave 1 (Infrastructure)     Wave 2 (Core)        Wave 3 (Endpoints)
========================     =======================     ================     ==================
T1: Config System ──────────► T7: Persona Loader ──────►                     
                              T6: Session Manager ──────► T8: Orchestrator ──► T9: Gateway ──► T10: Telegram Bot
T2: Schema Expansion ──────► T5: EventBus Rewrite ──────►     Rewrite           Rewrite        T11: CLI Commands
T3: Anthropic Provider ─────────────────────────────────►
T4: Memory SQLite ──────────► T6: Session Manager ──────►

Wave 4 (M2 Memory)          Wave 5 (M3 Evolution)
========================     =======================
T12: Episode Recording       T15: Daily Consolidation
T13: Memory Search           T16: Conflict & Forgetting
T14: Memory Injection        T17: Runtime WASM Skeleton
                             T18: Sub-Agent Basic
                             T19: TUI Minimal
```

## Precise Dependency Matrix

| Task | Depends On | Milestone |
|------|-----------|-----------|
| T1: Config System | - | M1 |
| T2: Schema Expansion | - | M1 |
| T3: Anthropic Provider | - | M1 |
| T4: Memory SQLite | - | M2 |
| T5: EventBus Rewrite | T2 | M1 |
| T6: Session Manager | T2, T4 | M1 |
| T7: Persona Loader | T1 | M1 |
| T8: Orchestrator Rewrite | T1, T3, T5, T6, T7 | M1 |
| T9: Gateway Rewrite | T1, T5, T8 | M1 |
| T10: Telegram Bot | T9 | M1 |
| T11: CLI Commands | T1, T9, T10 | M1 |
| T12: Episode Recording | T4, T8 | M2 |
| T13: Memory Search | T4 | M2 |
| T14: Memory Injection | T8, T13 | M2 |
| T15: Daily Consolidation | T3, T4, T14 | M3 |
| T16: Conflict & Forgetting | T15 | M3 |
| T17: Runtime WASM Skeleton | - | M3 |
| T18: Sub-Agent Basic | T8 | M3 |
| T19: TUI Minimal | T5 | M3 |

---

## Wave 0: Foundation (4 tasks, all parallel)

### Task T1: Config System

**Crate:** `clawhive-core` (new module `config.rs`)
**Delegation:** `category='feature', skills=['rust']`
**Estimated:** ~200 lines

**Files:**
- Create: `crates/clawhive-core/src/config.rs`
- Modify: `crates/clawhive-core/src/lib.rs` (add `pub mod config;`)
- Modify: `crates/clawhive-core/Cargo.toml` (add `serde_yaml` dep)
- Test: `crates/clawhive-core/src/config.rs` (inline `#[cfg(test)]`)

**Config Structs:**

```rust
// crates/clawhive-core/src/config.rs
use std::path::{Path, PathBuf};
use anyhow::{Context, Result};
use serde::Deserialize;

// ── main.yaml ──
#[derive(Debug, Clone, Deserialize)]
pub struct MainConfig {
    pub app: AppConfig,
    pub runtime: RuntimeConfig,
    pub features: FeaturesConfig,
    pub channels: ChannelsConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub name: String,
    pub env: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RuntimeConfig {
    pub max_concurrent: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FeaturesConfig {
    pub multi_agent: bool,
    pub sub_agent: bool,
    pub tui: bool,
    pub cli: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChannelsConfig {
    pub telegram: Option<TelegramChannelConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramChannelConfig {
    pub enabled: bool,
    pub connectors: Vec<TelegramConnectorConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TelegramConnectorConfig {
    pub connector_id: String,
    pub token: String, // raw "${TELEGRAM_BOT_TOKEN}" — resolved at runtime
}

// ── routing.yaml ──
#[derive(Debug, Clone, Deserialize)]
pub struct RoutingConfig {
    pub default_agent_id: String,
    pub bindings: Vec<RoutingBinding>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RoutingBinding {
    pub channel_type: String,
    pub connector_id: String,
    #[serde(rename = "match")]
    pub match_rule: MatchRule,
    pub agent_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MatchRule {
    pub kind: String, // "dm", "mention", "group"
    pub pattern: Option<String>,
}

// ── providers.d/*.yaml ──
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderConfig {
    pub provider_id: String,
    pub enabled: bool,
    pub api_base: String,
    pub api_key_env: String,
    pub models: Vec<String>,
}

// ── agents.d/*.yaml (扩展现有 AgentConfig) ──
#[derive(Debug, Clone, Deserialize)]
pub struct FullAgentConfig {
    pub agent_id: String,
    pub enabled: bool,
    pub identity: Option<IdentityConfig>,
    pub model_policy: super::ModelPolicy,
    pub tool_policy: Option<ToolPolicyConfig>,
    pub memory_policy: Option<MemoryPolicyConfig>,
    pub sub_agent: Option<SubAgentPolicyConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IdentityConfig {
    pub name: String,
    pub emoji: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolPolicyConfig {
    pub allow: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MemoryPolicyConfig {
    pub mode: String,
    pub write_scope: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SubAgentPolicyConfig {
    pub allow_spawn: bool,
}

// ── 聚合根 ──
#[derive(Debug, Clone)]
pub struct ClawhiveConfig {
    pub main: MainConfig,
    pub routing: RoutingConfig,
    pub providers: Vec<ProviderConfig>,
    pub agents: Vec<FullAgentConfig>,
}

// ── 加载逻辑 ──
pub fn resolve_env_var(raw: &str) -> String {
    if raw.starts_with("${") && raw.ends_with('}') {
        let var_name = &raw[2..raw.len() - 1];
        std::env::var(var_name).unwrap_or_default()
    } else {
        raw.to_string()
    }
}

pub fn load_config(root: &Path) -> Result<ClawhiveConfig> {
    let main: MainConfig = load_yaml(&root.join("config/main.yaml"))?;
    let routing: RoutingConfig = load_yaml(&root.join("config/routing.yaml"))?;

    let providers = load_dir_yamls::<ProviderConfig>(&root.join("config/providers.d"))?;
    let agents = load_dir_yamls::<FullAgentConfig>(&root.join("config/agents.d"))?;

    validate_config(&main, &routing, &providers, &agents)?;

    Ok(ClawhiveConfig { main, routing, providers, agents })
}

fn load_yaml<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    serde_yaml::from_str(&text)
        .with_context(|| format!("failed to parse {}", path.display()))
}

fn load_dir_yamls<T: serde::de::DeserializeOwned>(dir: &Path) -> Result<Vec<T>> {
    let mut results = Vec::new();
    if dir.is_dir() {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().map_or(false, |e| e == "yaml" || e == "yml") {
                results.push(load_yaml(&path)?);
            }
        }
    }
    Ok(results)
}

fn validate_config(
    _main: &MainConfig,
    routing: &RoutingConfig,
    _providers: &[ProviderConfig],
    agents: &[FullAgentConfig],
) -> Result<()> {
    // 1. routing 引用的 agent_id 必须存在
    let agent_ids: std::collections::HashSet<_> =
        agents.iter().map(|a| a.agent_id.as_str()).collect();
    for binding in &routing.bindings {
        anyhow::ensure!(
            agent_ids.contains(binding.agent_id.as_str()),
            "routing references unknown agent_id: {}",
            binding.agent_id
        );
    }
    anyhow::ensure!(
        agent_ids.contains(routing.default_agent_id.as_str()),
        "default_agent_id '{}' not found in agents",
        routing.default_agent_id
    );
    // 2. 无重复 agent_id
    anyhow::ensure!(
        agent_ids.len() == agents.len(),
        "duplicate agent_id detected"
    );
    Ok(())
}
```

**Tests (5 cases):**
1. `load_config` 从 fixture YAML 加载成功
2. `validate_config` 检测 routing 引用不存在的 agent_id 时报错
3. `validate_config` 检测重复 agent_id 报错
4. `resolve_env_var` 正常替换环境变量
5. `resolve_env_var` 非变量格式原样返回

**Deliverables:**
- [x] `ClawhiveConfig` 聚合所有 YAML
- [x] `load_config(root)` 一次加载全部
- [x] `validate_config` fail-fast 校验
- [x] `resolve_env_var` 环境变量替换
- [x] 5 个单元测试

---

### Task T2: Schema Expansion

**Crate:** `clawhive-schema`
**Delegation:** `category='quick', skills=['rust']`
**Estimated:** ~120 lines

**Files:**
- Modify: `crates/clawhive-schema/src/lib.rs`

**Changes:**

```rust
// ── 扩展 Event enum (Bus Commands + Events) ──
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BusMessage {
    // Gateway -> Core commands
    HandleIncomingMessage {
        inbound: InboundMessage,
        resolved_agent_id: String,
    },
    CancelTask {
        trace_id: Uuid,
    },
    RunScheduledConsolidation,

    // Core -> Gateway events
    MessageAccepted {
        trace_id: Uuid,
    },
    ReplyReady {
        outbound: OutboundMessage,
    },
    TaskFailed {
        trace_id: Uuid,
        error: String,
    },

    // Core internal
    MemoryWriteRequested {
        session_key: String,
        speaker: String,
        text: String,
        importance: f32,
    },
}

// ── Session Key ──
#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionKey(pub String);

impl SessionKey {
    /// 私聊: channel_type:connector_id:chat_id:user_id
    /// 群聊: channel_type:connector_id:group_id:user_id:thread_id
    pub fn from_inbound(msg: &InboundMessage) -> Self {
        Self(format!(
            "{}:{}:{}:{}",
            msg.channel_type, msg.connector_id,
            msg.conversation_scope, msg.user_scope
        ))
    }
}

// ── InboundMessage 增加 optional 字段 ──
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundMessage {
    pub trace_id: Uuid,
    pub channel_type: String,
    pub connector_id: String,
    pub conversation_scope: String,
    pub user_scope: String,
    pub text: String,
    pub at: DateTime<Utc>,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub is_mention: bool,
    #[serde(default)]
    pub mention_target: Option<String>,
}
```

**Tests:**
1. `SessionKey::from_inbound` 正确拼接
2. `BusMessage` 各 variant 能正确 serde roundtrip
3. `InboundMessage` 新增字段 backward compatible（`thread_id` default None）

**Deliverables:**
- [x] `BusMessage` enum 覆盖 spec 的所有 Command/Event
- [x] `SessionKey` 生成逻辑
- [x] `InboundMessage` 扩展字段
- [x] 3 个单元测试

---

### Task T3: Real Anthropic Provider

**Crate:** `clawhive-provider`
**Delegation:** `category='feature', skills=['rust', 'http-api']`
**Estimated:** ~250 lines

**Files:**
- Modify: `crates/clawhive-provider/src/lib.rs` (拆分为多文件)
- Create: `crates/clawhive-provider/src/anthropic.rs`
- Create: `crates/clawhive-provider/src/types.rs`
- Modify: `crates/clawhive-provider/Cargo.toml`
- Modify: `Cargo.toml` (workspace deps 添加 reqwest, serde_json)

**Cargo.toml changes:**

```toml
# Workspace Cargo.toml additions
reqwest = { version = "0.12", features = ["json"] }
serde_json = "1"

# crates/clawhive-provider/Cargo.toml additions
reqwest.workspace = true
serde_json.workspace = true
```

**Core Changes:**

```rust
// crates/clawhive-provider/src/types.rs
// LlmRequest 扩展为支持多轮对话
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmMessage {
    pub role: String, // "user" | "assistant"
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmRequest {
    pub model: String,
    pub system: Option<String>,
    pub messages: Vec<LlmMessage>,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
}
fn default_max_tokens() -> u32 { 2048 }

// 向后兼容：单轮 user 消息的便捷构造
impl LlmRequest {
    pub fn simple(model: String, system: Option<String>, user: String) -> Self {
        Self {
            model,
            system,
            messages: vec![LlmMessage { role: "user".into(), content: user }],
            max_tokens: default_max_tokens(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmResponse {
    pub text: String,
    pub input_tokens: Option<u32>,
    pub output_tokens: Option<u32>,
    pub stop_reason: Option<String>,
}
```

```rust
// crates/clawhive-provider/src/anthropic.rs
use reqwest::Client;
use serde::{Deserialize, Serialize};
use anyhow::{Result, bail};
use async_trait::async_trait;

pub struct AnthropicProvider {
    client: Client,
    api_key: String,
    api_base: String,
}

impl AnthropicProvider {
    pub fn new(api_key: String, api_base: String) -> Self {
        Self {
            client: Client::new(),
            api_key,
            api_base,
        }
    }

    pub fn from_env(api_base: String) -> Result<Self> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .map_err(|_| anyhow::anyhow!("ANTHROPIC_API_KEY not set"))?;
        Ok(Self::new(api_key, api_base))
    }
}

// ── Anthropic API wire types ──
#[derive(Serialize)]
struct ApiRequest {
    model: String,
    max_tokens: u32,
    messages: Vec<ApiMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
}

#[derive(Serialize)]
struct ApiMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct ApiResponse {
    content: Vec<ContentBlock>,
    stop_reason: Option<String>,
    usage: Option<ApiUsage>,
}

#[derive(Deserialize)]
struct ContentBlock {
    text: String,
}

#[derive(Deserialize)]
struct ApiUsage {
    input_tokens: u32,
    output_tokens: u32,
}

#[derive(Deserialize)]
struct ApiError {
    error: ApiErrorDetail,
}

#[derive(Deserialize)]
struct ApiErrorDetail {
    message: String,
    #[serde(rename = "type")]
    error_type: String,
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    async fn chat(&self, request: LlmRequest) -> Result<LlmResponse> {
        let api_req = ApiRequest {
            model: request.model,
            max_tokens: request.max_tokens,
            system: request.system,
            messages: request.messages.iter().map(|m| ApiMessage {
                role: m.role.clone(),
                content: m.content.clone(),
            }).collect(),
        };

        let resp = self.client
            .post(format!("{}/v1/messages", self.api_base))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&api_req)
            .send()
            .await?;

        let status = resp.status();
        if !status.is_success() {
            let err: ApiError = resp.json().await.unwrap_or_else(|_| ApiError {
                error: ApiErrorDetail {
                    message: format!("HTTP {status}"),
                    error_type: "http_error".into(),
                },
            });
            bail!("anthropic api error ({}): {}", err.error.error_type, err.error.message);
        }

        let body: ApiResponse = resp.json().await?;
        let text = body.content.into_iter()
            .map(|b| b.text)
            .collect::<Vec<_>>()
            .join("");

        Ok(LlmResponse {
            text,
            input_tokens: body.usage.as_ref().map(|u| u.input_tokens),
            output_tokens: body.usage.as_ref().map(|u| u.output_tokens),
            stop_reason: body.stop_reason,
        })
    }
}
```

**ProviderRegistry 改造：**

```rust
// 改为存储实例而非 factory（支持持有 reqwest::Client）
pub struct ProviderRegistry {
    providers: HashMap<String, Arc<dyn LlmProvider>>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self { providers: HashMap::new() }
    }

    pub fn register(&mut self, id: impl Into<String>, provider: Arc<dyn LlmProvider>) {
        self.providers.insert(id.into(), provider);
    }

    pub fn get(&self, id: &str) -> Result<Arc<dyn LlmProvider>> {
        self.providers.get(id).cloned()
            .ok_or_else(|| anyhow!("provider not found: {id}"))
    }
}
```

**注意：** `LlmRequest` 接口变了（`user: String` → `messages: Vec<LlmMessage>`），需要更新 `clawhive-core` 中的 `LlmRouter::reply` 调用方式。这在 T8 (Orchestrator Rewrite) 中一并处理。为避免 Wave 0 内编译失败，T3 需保留一个临时的 `LlmRequest::simple()` 兼容方法 + 更新现有测试。

**Tests:**
1. `AnthropicProvider::new` 构造正确
2. `ApiRequest` serde 序列化匹配 Anthropic 格式
3. `ApiResponse` 反序列化正确解析 text + usage
4. `ApiError` 反序列化正确
5. `ProviderRegistry` get 已注册 provider 成功
6. `ProviderRegistry` get 未注册 provider 报错
7. 集成测试（`#[ignore]`）：真实调用 Anthropic API（需要 env var）

**Deliverables:**
- [x] 真实 HTTP 调用 Anthropic Messages API
- [x] 错误处理（HTTP 状态码 + API error body）
- [x] `LlmRequest` 支持多轮对话
- [x] `LlmResponse` 包含 token usage
- [x] `ProviderRegistry` 改为实例存储（支持 `reqwest::Client` 复用）
- [x] 7 个测试（6 unit + 1 integration #[ignore]）

---

### Task T4: Memory SQLite Foundation

**Crate:** `clawhive-memory`
**Delegation:** `category='feature', skills=['rust', 'sqlite']`
**Estimated:** ~350 lines

**Files:**
- Rewrite: `crates/clawhive-memory/src/lib.rs`
- Create: `crates/clawhive-memory/src/store.rs`
- Create: `crates/clawhive-memory/src/models.rs`
- Create: `crates/clawhive-memory/src/migrations.rs`
- Modify: `crates/clawhive-memory/Cargo.toml`
- Modify: `Cargo.toml` (workspace deps 添加 rusqlite)

**Cargo.toml changes:**

```toml
# Workspace Cargo.toml
rusqlite = { version = "0.32", features = ["bundled"] }

# crates/clawhive-memory/Cargo.toml
[dependencies]
rusqlite.workspace = true
tokio.workspace = true
# ...existing...
```

**Models:**

```rust
// crates/clawhive-memory/src/models.rs
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Episode {
    pub id: Uuid,
    pub ts: DateTime<Utc>,
    pub session_id: String,
    pub speaker: String, // "user" | "assistant" | agent_id
    pub text: String,
    pub tags: Vec<String>, // stored as JSON
    pub importance: f32, // 0.0 - 1.0
    pub context_hash: Option<String>,
    pub source_ref: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Concept {
    pub id: Uuid,
    pub concept_type: ConceptType,
    pub key: String,
    pub value: String,
    pub confidence: f32,
    pub evidence: Vec<String>, // stored as JSON
    pub first_seen: DateTime<Utc>,
    pub last_verified: DateTime<Utc>,
    pub status: ConceptStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ConceptType {
    Fact,
    Preference,
    Rule,
    Entity,
    TaskState,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ConceptStatus {
    Active,
    Stale,
    Conflicted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Link {
    pub id: Uuid,
    pub episode_id: Uuid,
    pub concept_id: Uuid,
    pub relation: LinkRelation,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum LinkRelation {
    Supports,
    Contradicts,
    Updates,
}
```

**Migrations:**

```rust
// crates/clawhive-memory/src/migrations.rs
use rusqlite::Connection;
use anyhow::Result;

const MIGRATIONS: &[&str] = &[
    // V0: episodes
    "CREATE TABLE IF NOT EXISTS episodes (
        id TEXT PRIMARY KEY,
        ts TEXT NOT NULL,
        session_id TEXT NOT NULL,
        speaker TEXT NOT NULL,
        text TEXT NOT NULL,
        tags TEXT NOT NULL DEFAULT '[]',
        importance REAL NOT NULL DEFAULT 0.5,
        context_hash TEXT,
        source_ref TEXT
    );
    CREATE INDEX IF NOT EXISTS idx_episodes_session ON episodes(session_id);
    CREATE INDEX IF NOT EXISTS idx_episodes_ts ON episodes(ts);",

    // V1: concepts
    "CREATE TABLE IF NOT EXISTS concepts (
        id TEXT PRIMARY KEY,
        concept_type TEXT NOT NULL,
        key TEXT NOT NULL,
        value TEXT NOT NULL,
        confidence REAL NOT NULL DEFAULT 0.5,
        evidence TEXT NOT NULL DEFAULT '[]',
        first_seen TEXT NOT NULL,
        last_verified TEXT NOT NULL,
        status TEXT NOT NULL DEFAULT 'Active'
    );
    CREATE INDEX IF NOT EXISTS idx_concepts_key ON concepts(key);
    CREATE INDEX IF NOT EXISTS idx_concepts_type ON concepts(concept_type);",

    // V2: links
    "CREATE TABLE IF NOT EXISTS links (
        id TEXT PRIMARY KEY,
        episode_id TEXT NOT NULL REFERENCES episodes(id),
        concept_id TEXT NOT NULL REFERENCES concepts(id),
        relation TEXT NOT NULL,
        created_at TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_links_episode ON links(episode_id);
    CREATE INDEX IF NOT EXISTS idx_links_concept ON links(concept_id);",

    // V3: sessions
    "CREATE TABLE IF NOT EXISTS sessions (
        session_key TEXT PRIMARY KEY,
        agent_id TEXT NOT NULL,
        created_at TEXT NOT NULL,
        last_active TEXT NOT NULL,
        ttl_seconds INTEGER NOT NULL DEFAULT 1800,
        metadata TEXT NOT NULL DEFAULT '{}'
    );",
];

pub fn run_migrations(conn: &Connection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS __schema_version (
            id INTEGER PRIMARY KEY CHECK (id = 0),
            version INTEGER NOT NULL DEFAULT 0
        )",
        [],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO __schema_version (id, version) VALUES (0, 0)",
        [],
    )?;

    let current: usize = conn.query_row(
        "SELECT version FROM __schema_version WHERE id = 0",
        [],
        |row| row.get(0),
    )?;

    for (idx, sql) in MIGRATIONS.iter().enumerate() {
        if idx >= current {
            conn.execute_batch(sql)?;
        }
    }

    conn.execute(
        "UPDATE __schema_version SET version = ?1 WHERE id = 0",
        [MIGRATIONS.len()],
    )?;

    Ok(())
}
```

**Store (async wrapper):**

```rust
// crates/clawhive-memory/src/store.rs
use std::sync::Arc;
use rusqlite::Connection;
use tokio::sync::Mutex;
use tokio::task;
use anyhow::Result;
use uuid::Uuid;
use chrono::{Utc, Duration};
use super::models::*;
use super::migrations;

/// Thread-safe async wrapper around SQLite.
/// SQLite is single-writer, so we use a Mutex<Connection>.
pub struct MemoryStore {
    conn: Arc<Mutex<Connection>>,
}

impl MemoryStore {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        migrations::run_migrations(&conn)?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys=ON;")?;
        migrations::run_migrations(&conn)?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    // ── Episodes CRUD ──

    pub async fn insert_episode(&self, ep: &Episode) -> Result<()> {
        let conn = self.conn.clone();
        let ep = ep.clone();
        task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "INSERT INTO episodes (id, ts, session_id, speaker, text, tags, importance, context_hash, source_ref)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![
                    ep.id.to_string(), ep.ts.to_rfc3339(), ep.session_id,
                    ep.speaker, ep.text, serde_json::to_string(&ep.tags)?,
                    ep.importance, ep.context_hash, ep.source_ref,
                ],
            )?;
            Ok(())
        }).await?
    }

    pub async fn recent_episodes(&self, session_id: &str, limit: usize) -> Result<Vec<Episode>> {
        let conn = self.conn.clone();
        let session_id = session_id.to_string();
        task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn.prepare(
                "SELECT id, ts, session_id, speaker, text, tags, importance, context_hash, source_ref
                 FROM episodes WHERE session_id = ?1 ORDER BY ts DESC LIMIT ?2"
            )?;
            let rows = stmt.query_map(rusqlite::params![session_id, limit], |row| {
                Ok(Episode {
                    id: row.get::<_, String>(0)?.parse().unwrap(),
                    ts: chrono::DateTime::parse_from_rfc3339(&row.get::<_, String>(1)?)
                        .unwrap().with_timezone(&Utc),
                    session_id: row.get(2)?,
                    speaker: row.get(3)?,
                    text: row.get(4)?,
                    tags: serde_json::from_str(&row.get::<_, String>(5)?).unwrap_or_default(),
                    importance: row.get(6)?,
                    context_hash: row.get(7)?,
                    source_ref: row.get(8)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        }).await?
    }

    pub async fn search_episodes(&self, query: &str, days: i64, limit: usize) -> Result<Vec<Episode>> {
        let conn = self.conn.clone();
        let query = format!("%{}%", query);
        let since = (Utc::now() - Duration::days(days)).to_rfc3339();
        task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn.prepare(
                "SELECT id, ts, session_id, speaker, text, tags, importance, context_hash, source_ref
                 FROM episodes WHERE text LIKE ?1 AND ts > ?2 ORDER BY importance DESC, ts DESC LIMIT ?3"
            )?;
            let rows = stmt.query_map(rusqlite::params![query, since, limit], |row| {
                Ok(Episode {
                    id: row.get::<_, String>(0)?.parse().unwrap(),
                    ts: chrono::DateTime::parse_from_rfc3339(&row.get::<_, String>(1)?)
                        .unwrap().with_timezone(&Utc),
                    session_id: row.get(2)?,
                    speaker: row.get(3)?,
                    text: row.get(4)?,
                    tags: serde_json::from_str(&row.get::<_, String>(5)?).unwrap_or_default(),
                    importance: row.get(6)?,
                    context_hash: row.get(7)?,
                    source_ref: row.get(8)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        }).await?
    }

    // ── Concepts CRUD ──

    pub async fn upsert_concept(&self, concept: &Concept) -> Result<()> {
        let conn = self.conn.clone();
        let c = concept.clone();
        task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "INSERT INTO concepts (id, concept_type, key, value, confidence, evidence, first_seen, last_verified, status)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                 ON CONFLICT(id) DO UPDATE SET
                    value = excluded.value,
                    confidence = excluded.confidence,
                    evidence = excluded.evidence,
                    last_verified = excluded.last_verified,
                    status = excluded.status",
                rusqlite::params![
                    c.id.to_string(), format!("{:?}", c.concept_type), c.key, c.value,
                    c.confidence, serde_json::to_string(&c.evidence)?,
                    c.first_seen.to_rfc3339(), c.last_verified.to_rfc3339(),
                    format!("{:?}", c.status),
                ],
            )?;
            Ok(())
        }).await?
    }

    pub async fn get_concepts_by_type(&self, ctype: ConceptType) -> Result<Vec<Concept>> {
        // similar pattern as episodes...
        let conn = self.conn.clone();
        task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let type_str = format!("{:?}", ctype);
            let mut stmt = conn.prepare(
                "SELECT id, concept_type, key, value, confidence, evidence, first_seen, last_verified, status
                 FROM concepts WHERE concept_type = ?1 AND status = 'Active'"
            )?;
            let rows = stmt.query_map([type_str], Self::row_to_concept)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        }).await?
    }

    pub async fn get_active_concepts(&self) -> Result<Vec<Concept>> {
        let conn = self.conn.clone();
        task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            let mut stmt = conn.prepare(
                "SELECT id, concept_type, key, value, confidence, evidence, first_seen, last_verified, status
                 FROM concepts WHERE status = 'Active' ORDER BY confidence DESC"
            )?;
            let rows = stmt.query_map([], Self::row_to_concept)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        }).await?
    }

    // ── Links CRUD ──

    pub async fn insert_link(&self, link: &Link) -> Result<()> {
        let conn = self.conn.clone();
        let l = link.clone();
        task::spawn_blocking(move || {
            let conn = conn.blocking_lock();
            conn.execute(
                "INSERT INTO links (id, episode_id, concept_id, relation, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    l.id.to_string(), l.episode_id.to_string(), l.concept_id.to_string(),
                    format!("{:?}", l.relation), l.created_at.to_rfc3339(),
                ],
            )?;
            Ok(())
        }).await?
    }

    // ── private helpers ──
    fn row_to_concept(row: &rusqlite::Row) -> rusqlite::Result<Concept> {
        // parse from row... (omitted for brevity, same pattern as episodes)
        todo!("implement row->Concept mapping")
    }
}
```

**Tests (8 cases):**
1. `open_in_memory` 成功执行 migrations
2. `insert_episode` + `recent_episodes` roundtrip
3. `recent_episodes` 按时间倒序，limit 生效
4. `search_episodes` LIKE 查询命中
5. `search_episodes` 时间窗口过滤
6. `upsert_concept` 插入新 concept
7. `upsert_concept` 更新已有 concept
8. `insert_link` 正确关联 episode 和 concept

**Deliverables:**
- [x] SQLite 自动 migration（4 tables）
- [x] `MemoryStore` async wrapper（Mutex<Connection> + spawn_blocking）
- [x] Episodes: insert, recent, search
- [x] Concepts: upsert, get_by_type, get_active
- [x] Links: insert
- [x] 8 个单元测试

---

## Wave 1: Infrastructure (3 tasks, depends on Wave 0)

### Task T5: EventBus Rewrite (Topic-Based Routing)

**Depends on:** T2 (BusMessage enum)
**Crate:** `clawhive-bus`
**Delegation:** `category='feature', skills=['rust', 'tokio']`
**Estimated:** ~150 lines

**Files:**
- Rewrite: `crates/clawhive-bus/src/lib.rs`
- Modify: `crates/clawhive-bus/Cargo.toml` (add `clawhive-schema` if not present — already present)

**Design:** Bus 内部持有多个 topic channel。每个 subscriber 注册时声明感兴趣的 topic（BusMessage variant 的 discriminant）。publish 时，Bus 根据 message variant 路由到对应 subscriber。

```rust
// crates/clawhive-bus/src/lib.rs
use std::collections::HashMap;
use std::sync::Arc;
use anyhow::Result;
use clawhive_schema::BusMessage;
use tokio::sync::{mpsc, RwLock};

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub enum Topic {
    HandleIncomingMessage,
    CancelTask,
    RunScheduledConsolidation,
    MessageAccepted,
    ReplyReady,
    TaskFailed,
    MemoryWriteRequested,
}

impl Topic {
    pub fn from_message(msg: &BusMessage) -> Self {
        match msg {
            BusMessage::HandleIncomingMessage { .. } => Topic::HandleIncomingMessage,
            BusMessage::CancelTask { .. } => Topic::CancelTask,
            BusMessage::RunScheduledConsolidation => Topic::RunScheduledConsolidation,
            BusMessage::MessageAccepted { .. } => Topic::MessageAccepted,
            BusMessage::ReplyReady { .. } => Topic::ReplyReady,
            BusMessage::TaskFailed { .. } => Topic::TaskFailed,
            BusMessage::MemoryWriteRequested { .. } => Topic::MemoryWriteRequested,
        }
    }
}

type Subscriber = mpsc::Sender<BusMessage>;

pub struct EventBus {
    subscribers: Arc<RwLock<HashMap<Topic, Vec<Subscriber>>>>,
    capacity: usize,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        Self {
            subscribers: Arc::new(RwLock::new(HashMap::new())),
            capacity,
        }
    }

    /// 订阅某个 topic，返回 receiver
    pub async fn subscribe(&self, topic: Topic) -> mpsc::Receiver<BusMessage> {
        let (tx, rx) = mpsc::channel(self.capacity);
        let mut subs = self.subscribers.write().await;
        subs.entry(topic).or_default().push(tx);
        rx
    }

    /// 发布消息，自动路由到对应 topic 的所有 subscriber
    pub async fn publish(&self, msg: BusMessage) -> Result<()> {
        let topic = Topic::from_message(&msg);
        let subs = self.subscribers.read().await;
        if let Some(subscribers) = subs.get(&topic) {
            for tx in subscribers {
                // 非阻塞发送，如果 subscriber 满了就 drop
                let _ = tx.try_send(msg.clone());
            }
        }
        Ok(())
    }

    /// 获取一个 publisher handle（可 clone 到多处）
    pub fn publisher(&self) -> BusPublisher {
        BusPublisher {
            subscribers: self.subscribers.clone(),
        }
    }
}

#[derive(Clone)]
pub struct BusPublisher {
    subscribers: Arc<RwLock<HashMap<Topic, Vec<Subscriber>>>>,
}

impl BusPublisher {
    pub async fn publish(&self, msg: BusMessage) -> Result<()> {
        let topic = Topic::from_message(&msg);
        let subs = self.subscribers.read().await;
        if let Some(subscribers) = subs.get(&topic) {
            for tx in subscribers {
                let _ = tx.try_send(msg.clone());
            }
        }
        Ok(())
    }
}
```

**Tests:**
1. publish 到无 subscriber 的 topic 不报错
2. subscribe + publish 单 subscriber 收到消息
3. 同一 topic 多个 subscriber 都收到消息
4. 不同 topic 的 subscriber 不会串扰
5. `BusPublisher` clone 后在另一个 task 中 publish 成功

**Deliverables:**
- [x] Topic-based routing EventBus
- [x] `subscribe(topic)` → `Receiver<BusMessage>`
- [x] `publish(msg)` 自动路由
- [x] `BusPublisher` 可 clone handle
- [x] 5 个单元测试

---

### Task T6: Session Manager

**Depends on:** T2 (SessionKey), T4 (MemoryStore with sessions table)
**Crate:** `clawhive-core` (new module `session.rs`)
**Delegation:** `category='feature', skills=['rust']`
**Estimated:** ~120 lines

**Files:**
- Create: `crates/clawhive-core/src/session.rs`
- Modify: `crates/clawhive-core/src/lib.rs` (add `pub mod session;`)
- Modify: `crates/clawhive-core/Cargo.toml` (add `clawhive-memory` dep)

```rust
// crates/clawhive-core/src/session.rs
use anyhow::Result;
use chrono::{DateTime, Utc, Duration};
use clawhive_memory::MemoryStore;
use clawhive_schema::SessionKey;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub session_key: SessionKey,
    pub agent_id: String,
    pub created_at: DateTime<Utc>,
    pub last_active: DateTime<Utc>,
    pub ttl_seconds: i64,
}

impl Session {
    pub fn is_expired(&self) -> bool {
        Utc::now() - self.last_active > Duration::seconds(self.ttl_seconds)
    }

    pub fn touch(&mut self) {
        self.last_active = Utc::now();
    }
}

pub struct SessionManager {
    store: std::sync::Arc<MemoryStore>,
    default_ttl: i64,
}

impl SessionManager {
    pub fn new(store: std::sync::Arc<MemoryStore>, default_ttl: i64) -> Self {
        Self { store, default_ttl }
    }

    /// 获取或创建 session。如果已过期，创建新的。
    pub async fn get_or_create(&self, key: &SessionKey, agent_id: &str) -> Result<Session> {
        if let Some(mut session) = self.store.get_session(&key.0).await? {
            if session.is_expired() {
                let new_session = self.create_new(key, agent_id);
                self.store.upsert_session(&new_session).await?;
                return Ok(new_session);
            }
            session.touch();
            self.store.upsert_session(&session).await?;
            Ok(session)
        } else {
            let session = self.create_new(key, agent_id);
            self.store.upsert_session(&session).await?;
            Ok(session)
        }
    }

    fn create_new(&self, key: &SessionKey, agent_id: &str) -> Session {
        let now = Utc::now();
        Session {
            session_key: key.clone(),
            agent_id: agent_id.to_string(),
            created_at: now,
            last_active: now,
            ttl_seconds: self.default_ttl,
        }
    }
}
```

**注意：** 此 task 需要在 `MemoryStore` (T4) 中添加 `get_session` / `upsert_session` 方法。这属于 T4 范围内的 sessions table CRUD。

**Tests:**
1. `get_or_create` 新 session 创建成功
2. `get_or_create` 已有未过期 session 复用
3. `get_or_create` 过期 session 重建
4. `Session::is_expired` 判断正确

**Deliverables:**
- [x] `Session` struct 带 TTL
- [x] `SessionManager::get_or_create` 带自动过期
- [x] SQLite 持久化
- [x] 4 个单元测试

---

### Task T7: Persona Loader

**Depends on:** T1 (Config system for agent identity)
**Crate:** `clawhive-core` (new module `persona.rs`)
**Delegation:** `category='quick', skills=['rust']`
**Estimated:** ~80 lines

**Files:**
- Create: `crates/clawhive-core/src/persona.rs`
- Modify: `crates/clawhive-core/src/lib.rs`

```rust
// crates/clawhive-core/src/persona.rs
use std::path::Path;
use anyhow::{Result, Context};

#[derive(Debug, Clone)]
pub struct Persona {
    pub agent_id: String,
    pub name: String,
    pub emoji: Option<String>,
    pub system_prompt: String,
    pub style_prompt: String,
    pub safety_prompt: String,
}

impl Persona {
    pub fn assembled_system_prompt(&self) -> String {
        let mut parts = Vec::new();
        parts.push(self.system_prompt.clone());
        if !self.style_prompt.is_empty() {
            parts.push(format!("\n## Style\n{}", self.style_prompt));
        }
        if !self.safety_prompt.is_empty() {
            parts.push(format!("\n## Safety\n{}", self.safety_prompt));
        }
        parts.join("\n")
    }
}

pub fn load_persona(
    prompts_root: &Path,
    agent_id: &str,
    name: &str,
    emoji: Option<&str>,
) -> Result<Persona> {
    let dir = prompts_root.join(agent_id);

    let system_prompt = read_optional_md(&dir.join("system.md"))
        .with_context(|| format!("loading persona for {agent_id}"))?
        .unwrap_or_default();
    let style_prompt = read_optional_md(&dir.join("style.md"))?.unwrap_or_default();
    let safety_prompt = read_optional_md(&dir.join("safety.md"))?.unwrap_or_default();

    Ok(Persona {
        agent_id: agent_id.to_string(),
        name: name.to_string(),
        emoji: emoji.map(|s| s.to_string()),
        system_prompt,
        style_prompt,
        safety_prompt,
    })
}

fn read_optional_md(path: &Path) -> Result<Option<String>> {
    if path.exists() {
        Ok(Some(std::fs::read_to_string(path)?))
    } else {
        Ok(None)
    }
}
```

**Tests:**
1. `load_persona` 读取已有 prompts 目录
2. `load_persona` 缺少某个 .md 文件时 fallback 空字符串
3. `assembled_system_prompt` 正确拼装三段

**Deliverables:**
- [x] `Persona` struct
- [x] `load_persona(prompts_root, agent_id, ...)` 从文件系统加载
- [x] `assembled_system_prompt()` 拼合 system + style + safety
- [x] 3 个单元测试

---

## Wave 2: Core Integration (2 tasks)

### Task T8: Orchestrator Rewrite

**Depends on:** T1, T3, T5, T6, T7
**Crate:** `clawhive-core`
**Delegation:** `category='feature', skills=['rust', 'tokio']`
**Estimated:** ~300 lines

**Files:**
- Rewrite: `crates/clawhive-core/src/lib.rs` (拆分为 `lib.rs` + `orchestrator.rs` + `router.rs`)
- Create: `crates/clawhive-core/src/orchestrator.rs`
- Create: `crates/clawhive-core/src/router.rs`
- Modify: `crates/clawhive-core/Cargo.toml` (add clawhive-bus, clawhive-memory)

**`lib.rs` 变为 barrel export：**

```rust
// crates/clawhive-core/src/lib.rs
pub mod config;
pub mod orchestrator;
pub mod persona;
pub mod router;
pub mod session;

pub use config::*;
pub use orchestrator::*;
pub use router::*;
pub use session::*;
pub use persona::*;
```

**LlmRouter 更新（适配新 LlmRequest）：**

```rust
// crates/clawhive-core/src/router.rs
use std::collections::HashMap;
use std::sync::Arc;
use anyhow::{anyhow, Result};
use clawhive_provider::{LlmProvider, LlmRequest, LlmResponse, LlmMessage, ProviderRegistry};

pub struct LlmRouter {
    registry: ProviderRegistry,
    aliases: HashMap<String, String>,
    global_fallbacks: Vec<String>,
}

impl LlmRouter {
    pub fn new(
        registry: ProviderRegistry,
        aliases: HashMap<String, String>,
        global_fallbacks: Vec<String>,
    ) -> Self {
        Self { registry, aliases, global_fallbacks }
    }

    pub async fn chat(
        &self,
        primary: &str,
        fallbacks: &[String],
        system: Option<String>,
        messages: Vec<LlmMessage>,
        max_tokens: u32,
    ) -> Result<LlmResponse> {
        let mut candidates = vec![primary.to_string()];
        candidates.extend(fallbacks.iter().cloned());
        candidates.extend(self.global_fallbacks.clone());

        let mut last_err: Option<anyhow::Error> = None;

        for candidate in candidates {
            let resolved = self.resolve_model(&candidate)?;
            let (provider_id, model_id) = parse_provider_model(&resolved)?;
            let provider = self.registry.get(&provider_id)?;

            let req = LlmRequest {
                model: model_id,
                system: system.clone(),
                messages: messages.clone(),
                max_tokens,
            };

            match provider.chat(req).await {
                Ok(resp) => return Ok(resp),
                Err(err) => {
                    tracing::warn!("provider {provider_id} failed: {err}");
                    last_err = Some(err);
                }
            }
        }

        Err(last_err.unwrap_or_else(|| anyhow!("no model candidate available")))
    }

    fn resolve_model(&self, raw: &str) -> Result<String> {
        if raw.contains('/') { return Ok(raw.to_string()); }
        self.aliases.get(raw).cloned()
            .ok_or_else(|| anyhow!("unknown model alias: {raw}"))
    }
}

fn parse_provider_model(input: &str) -> Result<(String, String)> {
    let mut parts = input.splitn(2, '/');
    let provider = parts.next().ok_or_else(|| anyhow!("invalid: {input}"))?;
    let model = parts.next().ok_or_else(|| anyhow!("invalid: {input}"))?;
    Ok((provider.to_string(), model.to_string()))
}
```

**Orchestrator 重写（核心调度）：**

```rust
// crates/clawhive-core/src/orchestrator.rs
use std::collections::HashMap;
use std::sync::Arc;
use anyhow::{anyhow, Result};
use clawhive_bus::{BusPublisher, Topic};
use clawhive_memory::MemoryStore;
use clawhive_provider::LlmMessage;
use clawhive_schema::*;
use super::config::{FullAgentConfig, ClawhiveConfig};
use super::persona::Persona;
use super::router::LlmRouter;
use super::session::{Session, SessionManager};

pub struct Orchestrator {
    router: LlmRouter,
    agents: HashMap<String, FullAgentConfig>,
    personas: HashMap<String, Persona>,
    session_mgr: SessionManager,
    memory: Arc<MemoryStore>,
    bus: BusPublisher,
    react_max_steps: usize,
    react_repeat_guard: usize,
}

impl Orchestrator {
    pub fn new(
        router: LlmRouter,
        agents: Vec<FullAgentConfig>,
        personas: HashMap<String, Persona>,
        session_mgr: SessionManager,
        memory: Arc<MemoryStore>,
        bus: BusPublisher,
    ) -> Self {
        let agents_map = agents.into_iter().map(|a| (a.agent_id.clone(), a)).collect();
        Self {
            router,
            agents: agents_map,
            personas,
            session_mgr,
            memory,
            bus,
            react_max_steps: 4,
            react_repeat_guard: 2,
        }
    }

    pub async fn handle_inbound(
        &self,
        inbound: InboundMessage,
        agent_id: &str,
    ) -> Result<OutboundMessage> {
        let agent = self.agents.get(agent_id)
            .ok_or_else(|| anyhow!("agent not found: {agent_id}"))?;

        // 1. Session
        let session_key = SessionKey::from_inbound(&inbound);
        let _session = self.session_mgr.get_or_create(&session_key, agent_id).await?;

        // 2. Persona system prompt
        let system_prompt = self.personas.get(agent_id)
            .map(|p| p.assembled_system_prompt())
            .unwrap_or_default();

        // 3. Memory context (M2 会充实此处, M1 先空)
        let memory_context = self.build_memory_context(&session_key).await?;

        // 4. Assemble messages
        let mut messages = Vec::new();
        if !memory_context.is_empty() {
            messages.push(LlmMessage {
                role: "user".into(),
                content: format!("[memory context]\n{memory_context}"),
            });
            messages.push(LlmMessage {
                role: "assistant".into(),
                content: "我已了解上下文，请继续。".into(),
            });
        }
        messages.push(LlmMessage {
            role: "user".into(),
            content: inbound.text.clone(),
        });

        // 5. WeakReAct loop
        let policy = &agent.model_policy;
        let reply_text = self.weak_react_loop(
            &policy.primary,
            &policy.fallbacks,
            Some(system_prompt),
            messages,
        ).await?;

        // 6. Build outbound
        let outbound = OutboundMessage {
            trace_id: inbound.trace_id,
            channel_type: inbound.channel_type.clone(),
            connector_id: inbound.connector_id.clone(),
            conversation_scope: inbound.conversation_scope.clone(),
            text: reply_text,
            at: chrono::Utc::now(),
        };

        // 7. Publish ReplyReady event
        let _ = self.bus.publish(BusMessage::ReplyReady {
            outbound: outbound.clone(),
        }).await;

        Ok(outbound)
    }

    async fn weak_react_loop(
        &self,
        primary: &str,
        fallbacks: &[String],
        system: Option<String>,
        initial_messages: Vec<LlmMessage>,
    ) -> Result<String> {
        let mut messages = initial_messages;
        let mut repeated = 0usize;
        let mut last_reply = String::new();

        for _step in 0..self.react_max_steps {
            let resp = self.router.chat(primary, fallbacks, system.clone(), messages.clone(), 2048).await?;
            let reply = resp.text;

            if reply == last_reply {
                repeated += 1;
                if repeated >= self.react_repeat_guard {
                    return Ok(format!("{reply}\n[weak-react: stopped, repeated)"));
                }
            } else {
                repeated = 0;
            }

            if reply.contains("[finish]") {
                return Ok(reply.replace("[finish]", "").trim().to_string());
            }

            last_reply = reply.clone();
            messages.push(LlmMessage { role: "assistant".into(), content: reply });
        }

        Ok(last_reply)
    }

    async fn build_memory_context(&self, _session_key: &SessionKey) -> Result<String> {
        // M1: 返回空。M2 (T14) 填充此方法
        Ok(String::new())
    }
}
```

**Tests (update existing + new):**
1. 更新现有 5 个 core tests 适配新 API
2. `Orchestrator::handle_inbound` 端到端（mock provider + in-memory store）
3. `weak_react_loop` repeat guard 触发
4. `weak_react_loop` finish tag 检测
5. Session 创建 + memory context 为空时的正常流程

**Deliverables:**
- [x] `Orchestrator` 集成 session、persona、memory、bus
- [x] `LlmRouter` 适配新多轮 `LlmRequest`
- [x] WeakReAct loop 保留
- [x] Bus publish `ReplyReady` event
- [x] 更新所有现有测试 + 5 个新测试

---

### Task T9: Gateway Rewrite

**Depends on:** T1, T5, T8
**Crate:** `clawhive-gateway`
**Delegation:** `category='feature', skills=['rust', 'tokio']`
**Estimated:** ~150 lines

**Files:**
- Rewrite: `crates/clawhive-gateway/src/lib.rs`
- Modify: `crates/clawhive-gateway/Cargo.toml` (add clawhive-bus)

```rust
// crates/clawhive-gateway/src/lib.rs
use std::sync::Arc;
use anyhow::{anyhow, Result};
use clawhive_bus::{BusPublisher, EventBus, Topic};
use clawhive_core::{Orchestrator, RoutingBinding, RoutingConfig};
use clawhive_schema::*;

pub struct Gateway {
    orchestrator: Arc<Orchestrator>,
    routing: RoutingConfig,
    bus: BusPublisher,
}

impl Gateway {
    pub fn new(
        orchestrator: Arc<Orchestrator>,
        routing: RoutingConfig,
        bus: BusPublisher,
    ) -> Self {
        Self { orchestrator, routing, bus }
    }

    /// 根据 routing.yaml 规则匹配 agent_id
    pub fn resolve_agent(&self, inbound: &InboundMessage) -> String {
        for binding in &self.routing.bindings {
            if binding.channel_type == inbound.channel_type
                && binding.connector_id == inbound.connector_id
            {
                match binding.match_rule.kind.as_str() {
                    "dm" if !inbound.conversation_scope.contains("group") => {
                        return binding.agent_id.clone();
                    }
                    "mention" if inbound.is_mention => {
                        if let Some(pattern) = &binding.match_rule.pattern {
                            if inbound.mention_target.as_deref() == Some(pattern.as_str()) {
                                return binding.agent_id.clone();
                            }
                        }
                    }
                    "group" => {
                        return binding.agent_id.clone();
                    }
                    _ => {}
                }
            }
        }
        self.routing.default_agent_id.clone()
    }

    /// 处理入站消息（完整流程）
    pub async fn handle_inbound(&self, inbound: InboundMessage) -> Result<OutboundMessage> {
        let agent_id = self.resolve_agent(&inbound);

        // Publish acceptance
        let _ = self.bus.publish(BusMessage::MessageAccepted {
            trace_id: inbound.trace_id,
        }).await;

        // Delegate to orchestrator
        match self.orchestrator.handle_inbound(inbound.clone(), &agent_id).await {
            Ok(outbound) => Ok(outbound),
            Err(err) => {
                let _ = self.bus.publish(BusMessage::TaskFailed {
                    trace_id: inbound.trace_id,
                    error: err.to_string(),
                }).await;
                Err(err)
            }
        }
    }
}
```

**Tests:**
1. `resolve_agent` DM 匹配正确
2. `resolve_agent` mention 匹配正确
3. `resolve_agent` fallback 到 default_agent_id
4. `handle_inbound` 端到端（mock orchestrator via bus events）

**Deliverables:**
- [x] `Gateway` 集成 routing.yaml 规则匹配
- [x] `resolve_agent` 基于 channel/connector/kind/mention 路由
- [x] Bus event 发布（MessageAccepted, TaskFailed）
- [x] 4 个测试

---

## Wave 3: Endpoints (2 tasks)

### Task T10: Real Telegram Bot

**Depends on:** T9
**Crate:** `clawhive-channels-telegram`
**Delegation:** `category='feature', skills=['rust', 'teloxide']`
**Estimated:** ~200 lines

**Files:**
- Modify: `crates/clawhive-channels-telegram/src/lib.rs` (保留 TelegramAdapter, 新增 TelegramBot)
- Modify: `crates/clawhive-channels-telegram/Cargo.toml`
- Modify: `Cargo.toml` (workspace deps: teloxide, log)

**Cargo.toml:**

```toml
# Workspace
teloxide = { version = "0.13", features = ["macros"] }
log = "0.4"

# crates/clawhive-channels-telegram/Cargo.toml
teloxide.workspace = true
log.workspace = true
tokio.workspace = true
clawhive-gateway = { path = "../clawhive-gateway" }
```

```rust
// 在 lib.rs 中新增
use std::sync::Arc;
use teloxide::prelude::*;
use clawhive_gateway::Gateway;

pub struct TelegramBot {
    token: String,
    connector_id: String,
    gateway: Arc<Gateway>,
}

impl TelegramBot {
    pub fn new(token: String, connector_id: String, gateway: Arc<Gateway>) -> Self {
        Self { token, connector_id, gateway }
    }

    pub async fn run(self) -> anyhow::Result<()> {
        let bot = Bot::new(&self.token);
        let adapter = Arc::new(TelegramAdapter::new(&self.connector_id));
        let gateway = self.gateway;

        let handler = Update::filter_message()
            .filter_map(|msg: Message| msg.text().map(|t| t.to_string()))
            .endpoint(move |bot: Bot, msg: Message, text: String| {
                let adapter = adapter.clone();
                let gateway = gateway.clone();
                async move {
                    let chat_id = msg.chat.id.0;
                    let user_id = msg.from.as_ref().map(|u| u.id.0 as i64).unwrap_or(0);

                    // 检测是否是 mention
                    let (is_mention, mention_target) = detect_mention(&msg);

                    let mut inbound = adapter.to_inbound(chat_id, user_id, &text);
                    inbound.is_mention = is_mention;
                    inbound.mention_target = mention_target;

                    // thread_id for topic groups
                    inbound.thread_id = msg.thread_id.map(|t| t.to_string());

                    match gateway.handle_inbound(inbound).await {
                        Ok(outbound) => {
                            bot.send_message(msg.chat.id, &outbound.text).await?;
                        }
                        Err(err) => {
                            tracing::error!("gateway error: {err}");
                            bot.send_message(msg.chat.id, "内部错误，请稍后再试").await?;
                        }
                    }
                    Ok::<(), teloxide::RequestError>(())
                }
            });

        Dispatcher::builder(bot, handler)
            .enable_ctrlc_handler()
            .build()
            .dispatch()
            .await;

        Ok(())
    }
}

fn detect_mention(msg: &Message) -> (bool, Option<String>) {
    if let Some(entities) = msg.entities() {
        for entity in entities {
            if entity.kind == teloxide::types::MessageEntityKind::Mention {
                let text = msg.text().unwrap_or("");
                let mention = &text[entity.offset..entity.offset + entity.length];
                return (true, Some(mention.to_string()));
            }
        }
    }
    (false, None)
}
```

**Tests:**
1. `detect_mention` 解析 @mention 正确
2. `TelegramAdapter::to_inbound` 新字段设置正确（保留现有行为）
3. 集成测试 `#[ignore]`：需要真实 token

**Deliverables:**
- [x] `TelegramBot::run()` — teloxide polling loop
- [x] 文本消息处理 + 回复
- [x] Mention 检测
- [x] Thread/topic_id 支持
- [x] 错误回复 fallback
- [x] 2 个 unit test + 1 个 integration test (#[ignore])

---

### Task T11: CLI Commands

**Depends on:** T1, T9, T10
**Crate:** `clawhive-cli`
**Delegation:** `category='feature', skills=['rust', 'clap']`
**Estimated:** ~200 lines

**Files:**
- Rewrite: `crates/clawhive-cli/src/main.rs`
- Modify: `crates/clawhive-cli/Cargo.toml`
- Modify: `Cargo.toml` (workspace deps: clap)

**Cargo.toml:**

```toml
# Workspace
clap = { version = "4", features = ["derive"] }

# crates/clawhive-cli/Cargo.toml
clap.workspace = true
clawhive-bus = { path = "../clawhive-bus" }
clawhive-memory = { path = "../clawhive-memory" }
```

```rust
// crates/clawhive-cli/src/main.rs
use std::path::PathBuf;
use std::sync::Arc;
use std::collections::HashMap;
use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "clawhive", version, about = "clawhive AI agent framework")]
struct Cli {
    #[arg(long, default_value = ".")]
    config_root: PathBuf,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// 启动 Telegram bot polling
    Start,
    /// 本地 REPL 测试（不需要 Telegram）
    Chat {
        #[arg(long, default_value = "clawhive-main")]
        agent: String,
    },
    /// 校验配置文件
    Validate,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    match cli.command {
        Commands::Validate => {
            let config = clawhive_core::load_config(&cli.config_root)?;
            println!("Config valid. {} agents, {} providers, {} routing bindings.",
                config.agents.len(), config.providers.len(), config.routing.bindings.len());
        }
        Commands::Start => {
            // 1. Load config
            // 2. Build MemoryStore, ProviderRegistry, EventBus
            // 3. Build Orchestrator, Gateway
            // 4. Start TelegramBot::run()
            start_bot(&cli.config_root).await?;
        }
        Commands::Chat { agent } => {
            // REPL loop: stdin -> Gateway -> stdout
            run_repl(&cli.config_root, &agent).await?;
        }
    }

    Ok(())
}

async fn start_bot(root: &PathBuf) -> Result<()> {
    let config = clawhive_core::load_config(root)?;
    let (bus, memory, orchestrator, gateway) = bootstrap(root, &config)?;

    // 找到 telegram connector
    let tg_config = config.main.channels.telegram
        .as_ref().ok_or_else(|| anyhow::anyhow!("telegram not configured"))?;

    for connector in &tg_config.connectors {
        let token = clawhive_core::resolve_env_var(&connector.token);
        let bot = clawhive_channels_telegram::TelegramBot::new(
            token, connector.connector_id.clone(), gateway.clone(),
        );
        // For MVP: run first connector (single bot)
        bot.run().await?;
        break;
    }

    Ok(())
}

async fn run_repl(root: &PathBuf, agent_id: &str) -> Result<()> {
    let config = clawhive_core::load_config(root)?;
    let (bus, memory, orchestrator, gateway) = bootstrap(root, &config)?;

    println!("clawhive REPL (agent: {agent_id}). Type 'quit' to exit.");

    let adapter = clawhive_channels_telegram::TelegramAdapter::new("repl");
    let stdin = std::io::stdin();
    loop {
        print!("> ");
        let mut input = String::new();
        stdin.read_line(&mut input)?;
        let input = input.trim();
        if input == "quit" || input == "exit" { break; }
        if input.is_empty() { continue; }

        let inbound = clawhive_schema::InboundMessage {
            trace_id: uuid::Uuid::new_v4(),
            channel_type: "repl".into(),
            connector_id: "repl".into(),
            conversation_scope: "repl:0".into(),
            user_scope: "user:local".into(),
            text: input.to_string(),
            at: chrono::Utc::now(),
            thread_id: None,
            is_mention: false,
            mention_target: None,
        };

        match gateway.handle_inbound(inbound).await {
            Ok(out) => println!("{}", out.text),
            Err(err) => eprintln!("Error: {err}"),
        }
    }

    Ok(())
}

// bootstrap 函数构建完整的依赖图
fn bootstrap(
    root: &PathBuf,
    config: &clawhive_core::ClawhiveConfig,
) -> Result<(
    clawhive_bus::EventBus,
    Arc<clawhive_memory::MemoryStore>,
    Arc<clawhive_core::Orchestrator>,
    Arc<clawhive_gateway::Gateway>,
)> {
    // ... 构建 memory, providers, personas, bus, session_mgr, router, orchestrator, gateway
    todo!("assemble dependency graph — 具体接线代码在实现时填充")
}
```

**Tests:**
1. `validate` 子命令在有效配置上通过
2. `validate` 子命令在无效配置上报错

**Deliverables:**
- [x] `clawhive start` — 启动 Telegram polling
- [x] `clawhive chat --agent <id>` — 本地 REPL
- [x] `clawhive validate` — 配置校验
- [x] `bootstrap()` — 完整依赖图组装
- [x] 2 个测试

---

## Wave 4: M2 Memory Features (3 tasks)

### Task T12: Episode Recording

**Depends on:** T4, T8
**Crate:** `clawhive-core` (modify orchestrator)
**Delegation:** `category='quick', skills=['rust']`
**Estimated:** ~60 lines

**Changes:** 在 `Orchestrator::handle_inbound` 末尾添加 episode 写入：

```rust
// 在 handle_inbound 的 outbound 构建之后
// 写入用户消息 episode
let user_ep = Episode {
    id: Uuid::new_v4(),
    ts: inbound.at,
    session_id: session_key.0.clone(),
    speaker: "user".into(),
    text: inbound.text.clone(),
    tags: vec![],
    importance: 0.5,
    context_hash: None,
    source_ref: None,
};
self.memory.insert_episode(&user_ep).await?;

// 写入 assistant 回复 episode
let asst_ep = Episode {
    id: Uuid::new_v4(),
    ts: outbound.at,
    session_id: session_key.0.clone(),
    speaker: agent_id.to_string(),
    text: outbound.text.clone(),
    tags: vec![],
    importance: 0.5,
    context_hash: None,
    source_ref: None,
};
self.memory.insert_episode(&asst_ep).await?;
```

**Tests:**
1. `handle_inbound` 后 `recent_episodes` 返回 2 条（user + assistant）
2. episode 的 session_id 匹配 SessionKey

**Deliverables:**
- [x] 每次对话自动写入 user + assistant episode
- [x] 2 个测试

---

### Task T13: Memory Search & Retrieval

**Depends on:** T4
**Crate:** `clawhive-memory` (扩展 MemoryStore)
**Delegation:** `category='quick', skills=['rust']`
**Estimated:** ~80 lines

**新增方法到 `MemoryStore`：**

```rust
/// 综合检索：recent episodes + active concepts，按相关度排序
pub async fn retrieve_context(
    &self,
    session_id: &str,
    query: &str,
    episode_limit: usize,
    concept_limit: usize,
) -> Result<MemoryContext> {
    let recent = self.recent_episodes(session_id, episode_limit).await?;
    let searched = if !query.is_empty() {
        self.search_episodes(query, 7, episode_limit).await?
    } else {
        vec![]
    };
    let concepts = self.get_active_concepts().await?;

    Ok(MemoryContext {
        recent_episodes: recent,
        relevant_episodes: searched,
        active_concepts: concepts.into_iter().take(concept_limit).collect(),
    })
}

#[derive(Debug, Clone)]
pub struct MemoryContext {
    pub recent_episodes: Vec<Episode>,
    pub relevant_episodes: Vec<Episode>,
    pub active_concepts: Vec<Concept>,
}

impl MemoryContext {
    pub fn to_prompt_text(&self) -> String {
        let mut parts = Vec::new();

        if !self.recent_episodes.is_empty() {
            parts.push("## Recent Conversation".to_string());
            for ep in &self.recent_episodes {
                parts.push(format!("[{}] {}: {}", ep.ts.format("%m-%d %H:%M"), ep.speaker, ep.text));
            }
        }

        if !self.active_concepts.is_empty() {
            parts.push("\n## Known Facts".to_string());
            for c in &self.active_concepts {
                parts.push(format!("- [{:?}] {}: {} (confidence: {:.1})", c.concept_type, c.key, c.value, c.confidence));
            }
        }

        parts.join("\n")
    }
}
```

**Tests:**
1. `retrieve_context` 空数据库返回空
2. `retrieve_context` 有 episodes 时正确返回
3. `to_prompt_text` 格式化正确

**Deliverables:**
- [x] `MemoryStore::retrieve_context` 综合检索
- [x] `MemoryContext::to_prompt_text` 格式化为 prompt 可用文本
- [x] 3 个测试

---

### Task T14: Memory Injection into LLM Context

**Depends on:** T8, T13
**Crate:** `clawhive-core`
**Delegation:** `category='quick', skills=['rust']`
**Estimated:** ~30 lines

**Changes:** 填充 `Orchestrator::build_memory_context`：

```rust
async fn build_memory_context(&self, session_key: &SessionKey) -> Result<String> {
    let context = self.memory.retrieve_context(
        &session_key.0,
        "", // M2: 用 inbound.text 作为 query
        10, // recent episode limit
        20, // concept limit
    ).await?;

    let text = context.to_prompt_text();
    if text.is_empty() {
        Ok(String::new())
    } else {
        Ok(text)
    }
}
```

改为接受 `query` 参数：

```rust
async fn build_memory_context(&self, session_key: &SessionKey, query: &str) -> Result<String> {
    let context = self.memory.retrieve_context(&session_key.0, query, 10, 20).await?;
    Ok(context.to_prompt_text())
}
```

并更新 `handle_inbound` 调用处传入 `&inbound.text`。

**Tests:**
1. 有 episode 数据时，memory context 注入到 LLM messages 中
2. 无数据时，messages 不包含 memory context 前缀

**Deliverables:**
- [x] 回答前自动注入记忆上下文
- [x] 2 个测试

---

## Wave 5: M3 Evolution (5 tasks, partially parallel)

### Task T15: Daily Consolidation

**Depends on:** T3, T4, T14
**Crate:** `clawhive-core` (new module `consolidation.rs`)
**Delegation:** `category='feature', skills=['rust', 'tokio']`
**Estimated:** ~200 lines

```rust
// crates/clawhive-core/src/consolidation.rs
use std::sync::Arc;
use anyhow::Result;
use chrono::Utc;
use clawhive_memory::{MemoryStore, models::*};
use clawhive_provider::LlmMessage;
use super::router::LlmRouter;

pub struct Consolidator {
    memory: Arc<MemoryStore>,
    router: Arc<LlmRouter>,
    model_primary: String,
    model_fallbacks: Vec<String>,
}

impl Consolidator {
    pub async fn run_daily(&self) -> Result<ConsolidationReport> {
        // 1. 读取近 24h 高 importance episodes
        let episodes = self.memory.search_episodes("", 1, 100).await?;
        let high_value: Vec<_> = episodes.into_iter()
            .filter(|e| e.importance >= 0.6)
            .collect();

        if high_value.is_empty() {
            return Ok(ConsolidationReport { concepts_created: 0, concepts_updated: 0, episodes_processed: 0 });
        }

        // 2. 用 LLM 提取 concept 候选
        let candidates = self.extract_concepts(&high_value).await?;

        // 3. Upsert concepts + create links
        let mut created = 0;
        let mut updated = 0;
        for (concept, source_episode_id) in candidates {
            // 检查是否已有同 key 的 concept
            let existing = self.memory.find_concept_by_key(&concept.key).await?;
            if existing.is_some() {
                updated += 1;
            } else {
                created += 1;
            }
            self.memory.upsert_concept(&concept).await?;

            let link = Link {
                id: uuid::Uuid::new_v4(),
                episode_id: source_episode_id,
                concept_id: concept.id,
                relation: LinkRelation::Supports,
                created_at: Utc::now(),
            };
            self.memory.insert_link(&link).await?;
        }

        Ok(ConsolidationReport {
            concepts_created: created,
            concepts_updated: updated,
            episodes_processed: high_value.len(),
        })
    }

    async fn extract_concepts(&self, episodes: &[Episode]) -> Result<Vec<(Concept, uuid::Uuid)>> {
        // 构造 prompt 让 LLM 提取结构化 concepts
        let episodes_text = episodes.iter()
            .map(|e| format!("[{}] {}: {}", e.ts.format("%m-%d"), e.speaker, e.text))
            .collect::<Vec<_>>()
            .join("\n");

        let system = "你是一个知识提取器。从以下对话中提取稳定事实、偏好、规则。\
            输出 JSON 数组，每项格式：{\"type\":\"fact|preference|rule\", \"key\":\"简短标识\", \"value\":\"描述\", \"confidence\":0.0-1.0, \"source_index\":0}\
            只提取高置信度的稳定知识，忽略临时性内容。";

        let messages = vec![LlmMessage {
            role: "user".into(),
            content: episodes_text,
        }];

        let resp = self.router.chat(&self.model_primary, &self.model_fallbacks, Some(system.into()), messages, 2048).await?;

        // 解析 JSON 响应 → Vec<Concept>
        parse_concept_candidates(&resp.text, episodes)
    }
}

#[derive(Debug)]
pub struct ConsolidationReport {
    pub concepts_created: usize,
    pub concepts_updated: usize,
    pub episodes_processed: usize,
}

fn parse_concept_candidates(llm_output: &str, episodes: &[Episode]) -> Result<Vec<(Concept, uuid::Uuid)>> {
    // 解析 LLM JSON 输出，容错处理
    // ... 具体实现
    todo!()
}
```

**MemoryStore 需新增：** `find_concept_by_key(key: &str) -> Result<Option<Concept>>`

**Tests:**
1. `run_daily` 无高价值 episode 时跳过
2. `run_daily` 有 episode 时生成 concept（mock LLM response）
3. `parse_concept_candidates` 正确解析 JSON
4. `parse_concept_candidates` 容错畸形 JSON

**Deliverables:**
- [x] `Consolidator::run_daily` 完整巩固流程
- [x] LLM 提取 concept 候选
- [x] 自动创建 episode→concept links
- [x] 4 个测试

---

### Task T16: Conflict Detection & Forgetting

**Depends on:** T15
**Crate:** `clawhive-memory` + `clawhive-core`
**Delegation:** `category='quick', skills=['rust']`
**Estimated:** ~100 lines

```rust
// 在 MemoryStore 中新增
pub async fn mark_stale_concepts(&self, days_inactive: i64) -> Result<usize> {
    let conn = self.conn.clone();
    let cutoff = (Utc::now() - Duration::days(days_inactive)).to_rfc3339();
    task::spawn_blocking(move || {
        let conn = conn.blocking_lock();
        let affected = conn.execute(
            "UPDATE concepts SET status = 'Stale'
             WHERE status = 'Active' AND last_verified < ?1",
            [cutoff],
        )?;
        Ok(affected)
    }).await?
}

pub async fn mark_conflicted(&self, concept_id: &Uuid) -> Result<()> {
    // UPDATE concepts SET status = 'Conflicted' WHERE id = ?1
    todo!()
}

pub async fn purge_old_episodes(&self, days: i64) -> Result<usize> {
    // DELETE FROM episodes WHERE ts < cutoff AND importance < 0.3
    todo!()
}
```

**在 Consolidator::run_daily 末尾添加：**

```rust
// 4. 遗忘策略
let staled = self.memory.mark_stale_concepts(30).await?;
let purged = self.memory.purge_old_episodes(90).await?;
tracing::info!("consolidation: {staled} concepts staled, {purged} episodes purged");
```

**Tests:**
1. `mark_stale_concepts` 正确标注超期 concept
2. `purge_old_episodes` 只删低 importance 的旧 episode
3. 不误删高 importance episode

**Deliverables:**
- [x] concept stale 标注（30 天未验证）
- [x] episode 低价值清理（90 天 + importance < 0.3）
- [x] 3 个测试

---

### Task T17: Runtime WASM Skeleton

**Depends on:** 无
**Crate:** `clawhive-runtime`
**Delegation:** `category='quick', skills=['rust']`
**Estimated:** ~60 lines

```rust
// crates/clawhive-runtime/src/lib.rs
use anyhow::Result;
use async_trait::async_trait;

#[async_trait]
pub trait TaskExecutor: Send + Sync {
    async fn execute(&self, input: &str) -> Result<String>;
}

pub struct NativeExecutor;

#[async_trait]
impl TaskExecutor for NativeExecutor {
    async fn execute(&self, input: &str) -> Result<String> {
        // MVP: 直接返回 input (pass-through)
        Ok(input.to_string())
    }
}

pub struct WasmExecutor;

#[async_trait]
impl TaskExecutor for WasmExecutor {
    async fn execute(&self, _input: &str) -> Result<String> {
        anyhow::bail!("WASM executor not implemented yet")
    }
}
```

**Modify Cargo.toml:**

```toml
[dependencies]
async-trait.workspace = true
anyhow.workspace = true
tracing.workspace = true
```

**Tests:**
1. `NativeExecutor::execute` pass-through
2. `WasmExecutor::execute` 返回 error

**Deliverables:**
- [x] `TaskExecutor` trait
- [x] `NativeExecutor` pass-through 实现
- [x] `WasmExecutor` placeholder
- [x] 2 个测试

---

### Task T18: Sub-Agent Basic

**Depends on:** T8
**Crate:** `clawhive-core` (new module `subagent.rs`)
**Delegation:** `category='feature', skills=['rust', 'tokio']`
**Estimated:** ~120 lines

```rust
// crates/clawhive-core/src/subagent.rs
use std::sync::Arc;
use anyhow::Result;
use tokio::time::{timeout, Duration};
use uuid::Uuid;
use clawhive_provider::LlmMessage;
use super::router::LlmRouter;
use super::config::FullAgentConfig;
use super::persona::Persona;

pub struct SubAgentRequest {
    pub parent_run_id: Uuid,
    pub trace_id: Uuid,
    pub target_agent_id: String,
    pub task: String,
    pub timeout_seconds: u64,
}

pub struct SubAgentResult {
    pub run_id: Uuid,
    pub output: String,
    pub success: bool,
}

pub struct SubAgentRunner {
    router: Arc<LlmRouter>,
    agents: std::collections::HashMap<String, FullAgentConfig>,
    personas: std::collections::HashMap<String, Persona>,
}

impl SubAgentRunner {
    pub async fn spawn(&self, req: SubAgentRequest) -> Result<SubAgentResult> {
        let agent = self.agents.get(&req.target_agent_id)
            .ok_or_else(|| anyhow::anyhow!("sub-agent not found: {}", req.target_agent_id))?;

        // 检查 allow_spawn
        if let Some(sa) = &agent.sub_agent {
            if !sa.allow_spawn {
                // sub-agent 不允许递归 spawn — 这里是检查目标 agent 的策略
                // MVP: 允许被 spawn，但目标 agent 自己不能再 spawn
            }
        }

        let system = self.personas.get(&req.target_agent_id)
            .map(|p| p.assembled_system_prompt())
            .unwrap_or_default();

        let messages = vec![LlmMessage {
            role: "user".into(),
            content: req.task,
        }];

        let result = timeout(
            Duration::from_secs(req.timeout_seconds),
            self.router.chat(
                &agent.model_policy.primary,
                &agent.model_policy.fallbacks,
                Some(system),
                messages,
                2048,
            ),
        ).await;

        match result {
            Ok(Ok(resp)) => Ok(SubAgentResult {
                run_id: Uuid::new_v4(),
                output: resp.text,
                success: true,
            }),
            Ok(Err(err)) => Ok(SubAgentResult {
                run_id: Uuid::new_v4(),
                output: err.to_string(),
                success: false,
            }),
            Err(_) => Ok(SubAgentResult {
                run_id: Uuid::new_v4(),
                output: "sub-agent timeout".into(),
                success: false,
            }),
        }
    }
}
```

**Tests:**
1. `spawn` 成功返回 LLM 结果（mock provider）
2. `spawn` timeout 返回 timeout 错误
3. `spawn` 未知 agent_id 报错

**Deliverables:**
- [x] `SubAgentRunner::spawn` 带 timeout
- [x] `SubAgentRequest` / `SubAgentResult` 结构
- [x] 3 个测试

---

### Task T19: TUI Minimal

**Depends on:** T5 (EventBus for observation)
**Crate:** `clawhive-tui`
**Delegation:** `category='feature', skills=['rust', 'ratatui']`
**Estimated:** ~200 lines

**Cargo.toml:**

```toml
# Workspace
ratatui = "0.29"
crossterm = "0.28"

# crates/clawhive-tui/Cargo.toml
ratatui.workspace = true
crossterm.workspace = true
clawhive-bus = { path = "../clawhive-bus" }
clawhive-schema = { path = "../clawhive-schema" }
```

**最小实现：** 两栏布局 — 左栏 Event Bus 日志，右栏 Active Sessions。

```rust
// crates/clawhive-tui/src/main.rs
// 使用 ratatui 基础模板:
// - 左半 Panel: 最近 50 条 BusMessage（subscribe All topics）
// - 右半 Panel: placeholder "Sessions" 面板
// - 'q' 退出
// - 每 100ms 刷新
```

具体 ratatui 代码较长，实现时参照 ratatui examples/minimal。

**Tests:** 无（TUI 为视觉组件，手动测试）

**Deliverables:**
- [x] 最小 ratatui 双栏面板
- [x] Bus event 实时日志流
- [x] 'q' 退出

---

## Implementation Schedule Summary

| Wave | Tasks | Parallelism | M |
|------|-------|-------------|---|
| **0** | T1, T2, T3, T4 | 4-way parallel | — |
| **1** | T5, T6, T7 | 3-way parallel | — |
| **2** | T8, T9 | sequential (T8→T9) | M1 |
| **3** | T10, T11 | sequential (T10→T11) | **M1 Done** |
| **4** | T12, T13, T14 | T12∥T13, then T14 | **M2 Done** |
| **5** | T15, T16, T17, T18, T19 | T15→T16; T17∥T18∥T19 | **M3 Done** |

**Total: 19 tasks, ~2600 lines of new Rust code, ~60+ tests**

## New Workspace Dependencies Summary

```toml
# Add to [workspace.dependencies]
reqwest = { version = "0.12", features = ["json"] }
serde_json = "1"
rusqlite = { version = "0.32", features = ["bundled"] }
teloxide = { version = "0.13", features = ["macros"] }
clap = { version = "4", features = ["derive"] }
log = "0.4"
ratatui = "0.29"
crossterm = "0.28"
tracing-subscriber = "0.3"
```

---

Dragon，计划已完成。两种执行方式可选：

**1. Subagent-Driven (当前 session)** — 我逐 task 派遣 subagent 执行，每个 task 完成后做 code review，快速迭代

**2. Parallel Session (新 session)** — 打开新 session 进入 worktree，使用 executing-plans skill 批量执行

选哪种？

<task_metadata>
session_id: ses_3ae17545cffeg55KTMWPGLtRHB
</task_metadata>