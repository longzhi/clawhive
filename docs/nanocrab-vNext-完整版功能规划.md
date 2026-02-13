# nanocrab vNext：完整版功能规划（非 MVP）

> 目的：沉淀 nanocrab 下一阶段（vNext/完整版）设计，不挤占 MVP 开发节奏。

---

## 1. 定位与范围

本文档属于 **vNext 设计**，明确不纳入当前 MVP 的交付范围。  
MVP 继续聚焦：主链路可用（Gateway→Core→Reply）、基础弱 ReAct、真实 Telegram 接线。

vNext 重点是：

1. Capability-based 安全执行
2. 任务/步骤/工具调用术语标准化
3. 细粒度权限授予与审计体系
4. Planner 与执行分层

---

## 2. 术语标准（建议作为项目统一语言）

- **Task**：用户目标（例：检查 Gmail 新邮件）
- **Run**：一次任务执行实例
- **Plan**：任务拆解后的步骤序列
- **Step**：单个执行步骤
- **Action**：步骤内动作类型（respond/tool_call/finish）
- **Tool Call**：具体工具调用
- **Invocation**：某次真实执行行为
- **Capability Grant**：为某次运行/步骤授予的权限集合
- **Trace**：跨模块可观测链路

建议代码命名同步：`Task`, `Run`, `Step`, `ToolCall`, `CapabilityGrant`。

---

## 3. 能力权限模型（Capability-based Execution）

## 3.1 核心原则

1. **默认零权限（deny by default）**
2. **按任务最小授权（least privilege）**
3. **权限随生命周期回收（ephemeral grants）**
4. **高风险能力必须审批（human-in-the-loop）**

## 3.2 授权粒度

1. **Task 级**：本次任务允许能力边界
2. **Step 级**：特定步骤临时提权
3. **Resource 级**：精确到目录/API scope/主机能力

## 3.3 执行分级

- **Safe**：自动执行
- **Guarded**：会话级或单次审批后执行
- **Unsafe**：每次调用都需确认

说明：
- Wasm 是强隔离底座，但不是唯一准入条件
- 非 Wasm 工具可进入 Guarded/Unsafe，不应完全禁止

---

## 4. WASM 沙箱与宿主工具代理

## 4.1 设计要点

- WASM 实例在任务启动时创建
- mount/capability 在实例创建时确定
- 任务结束后实例和权限上下文销毁

## 4.2 宿主能力访问建议

对于 Gmail、macOS 日历等系统能力：

- 不建议 WASM 直接访问系统 API
- 建议通过宿主工具代理（如 `gmail.read`, `calendar.add`）
- 由 Runtime 控制 capability + 审计日志

---

## 5. 执行架构分层（vNext）

1. **Planner**（可选）
   - 任务拆解、重规划、并行建议
2. **Orchestrator**
   - 运行状态机、步骤调度、策略决策
3. **Policy Engine**
   - 能力匹配、风险分级、审批触发
4. **Executor**
   - Wasm 执行或宿主代理调用
5. **Audit/Telemetry**
   - 全链路 trace 与权限审计

---

## 6. 审计与可观测（必须）

每次执行最少记录：

- `trace_id`, `task_id`, `run_id`, `step_id`
- `tool_call`
- `requested_capabilities`
- `granted_capabilities`
- `approval_required/approval_result`
- `start_at/end_at/status/error`

---

## 7. 与 MVP 的边界

### 7.1 MVP 保留

- 弱 ReAct
- 基础工具调用
- 基础路由与配置

### 7.2 vNext 再做

- 完整 capability policy engine
- 细粒度权限生命周期管理
- planner 插件化
- 结构化审计面板（TUI/Web）

---

## 8. 存储与向量检索（扩展）

> 基础的 SQLite + sqlite-vec 方案已纳入 MVP（用于 episodes 向量检索与结构化存储）。  
> vNext 在此基础上扩展更多应用场景。

### 8.1 vNext 扩展应用

在 MVP 已有的 episodes 语义召回基础上，vNext 进一步利用 sqlite-vec：

- **审计日志结构化存储与查询**（trace/run/step，配合 §6 审计需求）
- **工具描述 & capability 的语义检索**（embedding → sqlite-vec，支持动态工具发现）
- **跨 agent 知识共享**（共享 concepts 表的向量索引）
- **任务/步骤状态持久化与相似任务召回**

### 8.2 vNext 记忆增强

从 MVP 延后到 vNext 的记忆功能：

- **语义感知分块**：按 Markdown heading 切分长文本，超长段落退化为固定窗口切分；提升 embedding 质量
- **本地 Embedding 模型**：MVP 预留 `EmbeddingProvider` trait 接口，vNext 实现本地模型（如 `ort` + ONNX），消除对远程 API 的依赖
- **记忆压缩（Compaction）**：参考 OpenClaw 的 auto-compaction 设计，对超长 episode 序列做摘要压缩，保留关键信息同时减少 token 消耗
- **多模态记忆**：图片/文件的 embedding 索引与召回

---

## 9. 工具调用系统（Tool Calling）

### 9.1 设计目标

在 MVP 的基础工具调用之上，构建完整的、多 provider 兼容的原生工具调用系统。

核心原则：

1. **走 Provider 原生 Tool Use 协议**（非文本解析），结构化、类型安全
2. **统一抽象层**：Core/Orchestrator 只操作统一类型，不感知 provider 协议差异
3. **Skill 指导 + Tool 执行**：Skill 提供策略知识（何时用、怎么用），Tool 提供执行能力（参数契约 + 实际调用）
4. **与 Capability 模型集成**：工具调用受权限策略约束（§3）

### 9.2 统一 Tool 类型

放置于 `nanocrab-schema` 或 `nanocrab-provider/types.rs`：

```rust
/// 工具定义：注册到 ToolRegistry，传递给 LLM Provider
struct ToolDef {
    name: String,
    description: String,
    parameters: serde_json::Value,  // JSON Schema
}

/// 工具调用：LLM 返回的结构化调用请求
struct ToolCall {
    id: String,           // provider 生成的调用 ID
    name: String,         // 工具名
    input: serde_json::Value,  // 参数
}

/// 工具结果：执行后回传给 LLM
struct ToolResult {
    tool_call_id: String, // 对应 ToolCall.id
    output: String,       // 执行输出
    is_error: bool,       // 是否执行失败
}
```

### 9.3 消息模型扩展

现有 `LlmMessage.content` 从纯 `String` 扩展为 `Vec<ContentBlock>`：

```rust
enum ContentBlock {
    Text(String),
    ToolUse(ToolCall),
    ToolResult(ToolResult),
}

struct LlmMessage {
    role: String,
    content: Vec<ContentBlock>,
}

struct LlmRequest {
    model: String,
    system: Option<String>,
    messages: Vec<LlmMessage>,
    tools: Vec<ToolDef>,       // 新增：可用工具列表
    max_tokens: u32,
}

struct LlmResponse {
    content: Vec<ContentBlock>, // 扩展：可能包含 text + tool_use 混合
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
    stop_reason: Option<String>,
}
```

> **Breaking Change 注意**：`content: String` → `Vec<ContentBlock>` 需要适配所有现有代码。建议提供 `LlmMessage::text(role, content)` 便捷构造器保持向后兼容。

### 9.4 多 Provider 协议适配

各 provider 的 tool use 协议格式不同，但语义一致：

| Provider | 请求格式 | 响应格式 | tool_result 传递方式 |
|----------|----------|----------|---------------------|
| **Anthropic** | `tools` 数组 | `tool_use` content block | `tool_result` content block（同一 message） |
| **OpenAI / 兼容** | `tools` 数组 | `tool_calls` in assistant message | `role: "tool"` 独立 message |
| **Google Gemini** | `tools` + `functionDeclarations` | `functionCall` in parts | `functionResponse` in parts |
| **本地推理** | 多数兼容 OpenAI 格式 | 同 OpenAI | 同 OpenAI |

适配策略：

```
nanocrab-core（统一抽象）          nanocrab-provider（各 Adapter）
┌───────────────────────┐         ┌──────────────────────────┐
│ ToolDef               │         │ AnthropicAdapter         │
│ ToolCall              │────────▶│  → tools content blocks  │
│ ToolResult            │         ├──────────────────────────┤
│ ContentBlock          │◀────────│ OpenAIAdapter            │
│                       │         │  → tools + tool_calls    │
│                       │         ├──────────────────────────┤
│                       │         │ GeminiAdapter            │
│                       │         │  → functionDeclarations  │
│                       │         └──────────────────────────┘
└───────────────────────┘
```

每个 Adapter 实现两个转换：
1. **请求转换**：统一 `ToolDef` + `ContentBlock` → provider 特定 API 格式
2. **响应转换**：provider 特定响应 → 统一 `ContentBlock`

LlmProvider trait 扩展：

```rust
#[async_trait]
trait LlmProvider: Send + Sync {
    async fn chat(&self, request: LlmRequest) -> Result<LlmResponse>;
    fn supports_tools(&self) -> bool { false }  // 新增：能力声明
    // ...
}
```

### 9.5 工具注册与管理

```rust
struct ToolRegistry {
    tools: HashMap<String, RegisteredTool>,
}

struct RegisteredTool {
    def: ToolDef,                    // 工具定义（传给 LLM）
    executor: Arc<dyn ToolExecutor>, // 工具执行器
    risk_level: RiskLevel,           // Safe / Guarded / Unsafe（对接 §3 权限模型）
}

#[async_trait]
trait ToolExecutor: Send + Sync {
    async fn execute(&self, input: serde_json::Value) -> Result<String>;
}
```

工具来源：
- **内置工具**：`shell_exec`、`file_read`、`file_write`、`web_fetch` 等
- **Skill 声明的工具**：Skill 的 frontmatter 中可声明 `requires.tools`
- **动态注册**：通过配置或 API 动态添加（vNext 后期）

按 agent 过滤：
- `ToolPolicyConfig.allow` 白名单控制每个 agent 可见的工具集
- 未在白名单中的工具不传给 LLM

### 9.6 工具调用执行循环

替代现有的 `weak_react_loop`，实现完整的 tool use 循环：

```
1. 组装 messages + tools → 调 LLM
2. 解析 LlmResponse.content:
   ├── 只有 Text → 直接返回（无工具调用）
   └── 包含 ToolUse →
       a. 对每个 ToolCall:
          ├── 权限检查（Capability Policy）
          ├── Safe → 直接执行
          ├── Guarded → 检查是否已审批
          └── Unsafe → 触发 NeedHumanApproval，等待确认
       b. 执行工具，收集 ToolResult
       c. 将 assistant message (含 ToolUse) + ToolResult 追加到 messages
       d. 回到步骤 1（继续调 LLM）
3. 循环上限：max_tool_rounds（防止无限循环）
4. 重复检测：连续相同工具调用 → 中断
```

### 9.7 Tool 与 Skill 的协作关系

```
Skill（知识层）              Tool（执行层）
┌─────────────────┐         ┌─────────────────┐
│ SKILL.md        │         │ ToolDef          │
│ - 何时使用      │         │ - name           │
│ - 操作步骤      │  指导   │ - parameters     │
│ - 注意事项      │────────▶│ - executor       │
│ - requires.tools│         │                  │
└─────────────────┘         └─────────────────┘
       │                           │
       ▼                           ▼
  注入 system prompt          传入 LLM tools 参数
  （LLM 读取理解）            （LLM 结构化调用）
```

- **Skill 不执行任何操作**：只作为 prompt 知识注入，帮助 LLM 理解何时/如何使用工具
- **Tool 不包含策略**：只定义参数契约和执行逻辑
- **LLM 是决策者**：根据 Skill 知识 + Tool 定义，自主决定调用时机和参数

### 9.8 与 Capability 模型的集成

工具调用是 Capability 权限模型（§3）最直接的应用场景：

- 每个 `RegisteredTool` 带 `risk_level`
- 工具执行前经过 Policy Engine 检查
- `CapabilityGrant` 记录授权详情，写入审计日志（§6）
- 高风险工具（如 `shell_exec`）默认 Unsafe，每次需确认

### 9.9 MVP vs vNext 边界

| 能力 | MVP | vNext |
|------|-----|-------|
| Provider 原生 tool_use（Anthropic） | ✅ 首先实现 | — |
| 统一 ToolDef / ToolCall / ToolResult 类型 | ✅ | — |
| ContentBlock 消息模型 | ✅ | — |
| 基础内置工具（shell_exec, file_read） | ✅ | — |
| 工具执行循环（替代 weak_react_loop） | ✅ | — |
| 多 Provider 适配（OpenAI/Gemini） | — | ✅ |
| ToolRegistry 动态注册 | — | ✅ |
| Capability 权限集成 | — | ✅ |
| 工具语义检索（sqlite-vec） | — | ✅ |
| Skill.requires.tools 自动关联 | — | ✅ |

---

## 10. Sub-Agent 演进设计

### 10.1 三阶段演进路线

#### 阶段 1：独立 LLM 调用（MVP — 当前骨架已有）

Sub-Agent 本质是一次独立的 LLM 调用，有自己的 agent 配置（模型、persona），但只做单轮对话。

```
Parent Agent
  │  spawn(task, agent_id)
  ▼
SubAgentRunner → tokio::spawn
  │  独立 messages + system prompt
  │  router.chat() → 单轮 LLM 调用
  ▼
SubAgentResult { output: String }
  ▼  合并回 parent 上下文
```

**当前状态：** `SubAgentRunner` 骨架已实现（spawn/cancel/wait_result/result_merge），但未接入 Orchestrator，缺少触发机制。

**MVP 补全项：**
- 将 SubAgentRunner 集成到 Orchestrator
- 定义触发条件（LLM 显式请求 or 规则触发）
- 适合场景：简单子任务（翻译、摘要、格式转换）

#### 阶段 2：完整 Agent 实例（vNext 第一步）

Sub-Agent 是具备完整能力的 agent 实例，能调工具、多轮推理、访问记忆。

```
Parent Agent (Orchestrator)
  │  spawn(task, agent_id)
  ▼
Sub-Agent（独立完整 Agent）
  ├── 自己的 session
  ├── 自己的记忆（可读 parent episodes，写入独立分区）
  ├── 自己的工具集（parent 的子集，最小权限）
  ├── 自己的 tool use / ReAct 循环
  └── 可再 spawn sub-sub-agent（受 max_depth 限制）
  ▼
最终结果返回 Parent
```

**关键设计：**
- `spawn()` 创建迷你 Orchestrator 实例，复用核心逻辑（tool use 循环、记忆访问）
- 记忆隔离策略：sub-agent 可读 parent 的 episodes，但写入独立分区（`session_id` 区分）
- 工具集默认比 parent 更收敛（`sub_agent.allowed_tools` 白名单）
- 必须带 `parent_run_id` + `trace_id`，便于审计链路追踪
- 适合场景：复杂独立任务（代码分析、研究报告、多步骤排障）

#### 阶段 3：Agent-as-Tool（vNext 第二步）

Sub-Agent 被包装为 Tool，Parent Agent 通过正常 tool_use 机制自主触发，无需显式编排。

```
Parent Agent
  │  LLM 返回 tool_use: { name: "research_agent", input: { topic: "..." } }
  ▼
ToolExecutor 识别 agent-type tool
  │  创建 Sub-Agent（阶段 2 的完整实例）
  │  独立运行直到完成
  ▼
tool_result: { output: "研究报告..." }
  ▼  回传 LLM 继续生成
```

**关键设计：**
- 在 ToolRegistry 注册 agent-type tools（`RegisteredTool` 的 executor 是 `AgentToolExecutor`）
- ToolDef 中描述 agent 的能力，让 parent LLM 理解何时应委托
- 对 parent agent 完全透明——它只是调用了一个 tool，不知道背后是另一个 agent
- 与 Capability 模型集成：spawn sub-agent 本身视为一个需要权限的操作
- 适合场景：LLM 自主决定何时委托，无需外部编排逻辑

### 10.2 三阶段对比

| | 阶段 1：独立 LLM 调用 | 阶段 2：完整 Agent 实例 | 阶段 3：Agent-as-Tool |
|---|---|---|---|
| 时间线 | MVP | vNext 第一步 | vNext 第二步 |
| 能力 | 单轮问答 | 多轮 + 工具 + 记忆 | 多轮 + 工具 + 记忆 |
| 触发方式 | 代码显式 spawn | 代码显式 spawn | LLM 自主调用 tool |
| 工具访问 | ❌ | ✅（parent 子集） | ✅（parent 子集） |
| 记忆访问 | ❌ | ✅（读共享/写隔离） | ✅（读共享/写隔离） |
| 递归 spawn | ❌ | ✅（受 max_depth 限制） | ✅ |
| 对 parent 透明 | ❌（需显式编排） | ❌（需显式编排） | ✅（就是一个 tool） |

---

## 11. 建议落地节奏

1. 在 `docs/` 保持本文件为 vNext 设计基线
2. MVP 完成后，先实现 `CapabilityGrant` 数据结构 + 审计日志
3. 多 Provider 工具调用适配（OpenAI/Gemini）+ ToolRegistry 动态注册
4. Sub-Agent 阶段 2（完整 Agent 实例）+ 记忆隔离
5. 引入 Policy Engine（Safe/Guarded/Unsafe）+ 工具权限集成
6. Sub-Agent 阶段 3（Agent-as-Tool）
7. 最后接 Planner 与高级执行策略

---

## 12. 结论

nanocrab 在 vNext 采用“Capability-based, per-task least-privilege”模型，将显著提升：

- 安全性（默认最小授权）
- 可控性（审批与策略分层）
- 可解释性（审计可追溯）
- 可扩展性（WASM 与宿主代理共存）

该模型建议作为 nanocrab 完整版核心能力之一。