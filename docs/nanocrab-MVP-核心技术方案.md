# nanocrab MVP 核心技术方案（v0.1）

> 目标：在不牺牲可扩展性的前提下，尽快交付可运行的第一版。  
> 核心原则：**Pi 的极简内核 + Nanobot 的清晰分层 + nanocrab 的 WASM 与双层记忆设计**。

---

## 1. MVP 范围与边界

### 0.1 本轮已确认的关键结论（冻结）

1. **多 Agent：MVP 必做**
2. **Sub-Agent：MVP 必做（最小可用）**
3. **人设（Persona）归 Core/Agent 管理，不归 Gateway**
4. **MVP 不引入 workspace 概念**（后续可作为可选能力）
5. **配置采用 YAML**（不使用数据库作为配置源）
6. **支持多 Bot / 多账号 / 多会话空间**（`channel_type + connector_id + conversation_scope`）


### 1.1 本期要做（Must Have）

1. 单通道接入（Telegram）
2. Gateway 边界层（接入/鉴权/限流/协议转换）
3. Core/Orchestrator（会话路由、策略决策、执行编排）
4. 轻量消息总线（进程内 Event Bus）
5. Runtime 执行层（预留 WASM 执行接口，先可用基础执行器）
6. 记忆系统 MVP（海马体 episodes + 皮质 concepts）
7. 基础定时巩固（Cron 每日低峰执行）
8. CLI 支持（命令式管理与一次性操作）
9. TUI 支持（开发者实时观测与调试）

### 1.2 本期不做（Not in MVP）

1. 多节点/分布式总线
2. 复杂知识图谱引擎
3. 外部向量数据库（使用 SQLite + sqlite-vec 替代）
4. 多通道同时生产化
5. Agent 自主找任务

---

## 2. 总体架构（职责切分）

### 2.0 多 Bot / 多账号 / 多会话空间（MVP 必须支持）

从 v0.1 开始即支持“同一通道类型的多实例 Bot”，避免后续 session、memory 与路由规则重构。

统一标识三元组：

- `channel_type`：telegram / discord / ...
- `connector_id`：同类型通道下的具体 Bot 实例（如 `tg_main` / `tg_ops` / `dc_dev`）
- `conversation_scope`：具体会话空间
  - Telegram：`chat_id (+ topic_id)`
  - Discord：`guild_id + channel_id (+ thread_id)`

MVP 强约束：

1. 入站/出站 schema 必带 `connector_id`
2. Gateway 按 `connector_id` 做鉴权与路由
3. Session key 必须包含 `connector_id + conversation_scope`
4. Memory 分区键必须包含 `connector_id`


### 2.1 模块角色

- **Gateway（边界层）**
  - 只负责：接入、验签、限流、协议转换、投递
  - 不负责：会话语义、记忆决策、工具编排、人设组装

- **Core / Orchestrator（中枢）**
  - 负责：Session 路由、上下文组装、策略选择、执行编排、记忆控制、人设装配
  - 是全系统唯一“认知决策入口”

- **Bus（事件层 — 旁路广播）**
  - **定位：旁路事件广播，非主链路驱动**
  - 主链路（消息处理→LLM 调用→回复）保持同步直接调用，Bus 不参与
  - Bus 负责非阻塞副作用的广播：审计日志、TUI 实时面板、metrics 统计、告警通知
  - 模式：fire-and-forget，各 listener 独立消费，失败不影响主链路
  - MVP 使用 in-memory async queue（`tokio::sync::mpsc`）
  - 接口设计预留 backpressure 语义，方便后续切换 NATS/Redis Stream
  - **vNext 扩展**：多通道出站分发可走 Bus（Gateway publish ReplyReady → 各 ChannelDriver 订阅）

- **Runtime（执行层）**
  - 接收 Core 下发任务并执行
  - MVP 预留 WASM 接口，后续平滑切换为 WASM 运行时

- **Memory（记忆层）**
  - 负责存储与检索
  - 策略在 Core，存储在 Memory 模块

### 2.2 为什么 Gateway 不控制 Memory

Memory 属于“认知层”能力（什么时候读、写什么、冲突如何处理），必须由 Core 决定。  
Gateway 只做 I/O 边界处理，避免协议细节污染业务决策。

---

## 3. Command / Event Schema（MVP 最小集）

> 先冻结语义协议，再选择传输实现。  
> **注意**：以下 Command/Event 不代表走 Bus 传递。主链路通过直接函数调用，Bus 仅广播旁路事件（见 §12）。  
> Schema 的价值在于统一 DTO 结构，而非决定传输方式。

所有 Inbound/Outbound DTO 的基础字段至少包含：

- `channel_type`
- `connector_id`
- `conversation_scope`
- `user_scope`
- `trace_id`

### 3.1 Gateway -> Core Commands

1. `HandleIncomingMessage`
2. `CancelTask`
3. `RunScheduledConsolidation`

### 3.2 Core -> Gateway Events

1. `MessageAccepted`
2. `ReplyReady`
3. `TaskFailed`
4. `NeedHumanApproval`（可预留）

### 3.3 Core 内部事件（可选）

1. `MemoryReadRequested`
2. `MemoryWriteRequested`
3. `ConsolidationCompleted`

---

## 4. Session 路由规则（MVP）

`session_key` 生成优先级：

1. 显式 thread/session id（若通道提供）
2. 私聊：`channel_type + connector_id + chat_id + user_id`
3. 群聊：`channel_type + connector_id + group_id + user_id (+ thread_id/topic_id)`

建议附加：

- Session TTL（例如 30 分钟冷却）
- 手动 reset
- trace_id 全链路透传

---

## 5. 记忆系统 MVP（海马体 + 皮质）

## 5.1 设计目标

借鉴“海马体快编码 + 皮质慢整合”：

- **海马体（Hippocampus）**：快速记录近期经历，保细节
- **皮质（Cortex）**：沉淀稳定知识，保低噪声高置信

### 5.2 数据模型（SQLite + sqlite-vec）

MVP 存储层采用 SQLite 搭配 [sqlite-vec](https://github.com/asg017/sqlite-vec) 扩展：

- SQLite 负责结构化数据存储（episodes/concepts/links/session 状态）
- sqlite-vec 提供向量相似度检索能力（cosine/L2/inner product），用于 episodes 语义召回
- 零额外依赖，单文件部署，不引入外部向量数据库

#### 表 1：`episodes`（海马体）

- `id`
- `ts`
- `session_id`
- `speaker`
- `text`
- `embedding`（向量，通过 sqlite-vec 索引，用于语义检索）
- `tags`（JSON）
- `importance`（0-1）
- `context_hash`
- `source_ref`

#### 表 2：`concepts`（皮质）

- `id`
- `type`（fact/preference/rule/entity/task_state）
- `key`
- `value`
- `confidence`
- `evidence`
- `first_seen`
- `last_verified`
- `status`（active/stale/conflicted）

#### 表 3：`links`（证据链，简化版）

- `id`
- `episode_id`
- `concept_id`
- `relation`（supports/contradicts/updates）
- `created_at`

### 5.3 写入策略（MVP 保守）

长期记忆（concepts）仅在以下场景写入：

1. 用户明确“记住这个”
2. 稳定偏好（语言、输出风格、工具偏好）
3. 稳定事实（路径、命名约定、架构决策）
4. 任务状态（todo / blocked / done）

其余内容只写 `episodes`。

### 5.4 检索策略（MVP）

1. 先查海马体：近期窗口（如 7 天）+ sqlite-vec 向量语义检索（基于 query embedding 召回相关 episodes）
2. 再查皮质概念层
3. 融合重排（语义相关度 + 置信度 + 新近性 + importance）

> 向量检索相比纯时间窗口，能在 episodes 量大时精准召回语义相关内容，显著提升记忆注入质量。

### 5.5 巩固流程（Consolidation，定时 Cron）

> 产出与人手写内容隔离：consolidation 产出的 concepts 标记 `source = "consolidation"`，与用户明确要求记住的内容区分。

1. 读取近 24h 高价值 episodes
2. LLM 同时看到新 episodes + 已有 concepts，自然处理冲突（不需要独立冲突检测系统）
3. 生成 concept 候选
4. 与已有 concept 比对并更新 confidence/status
5. 建立 episode->concept links
6. 执行遗忘策略（episodes TTL，concept stale 标注）

### 5.6 检索增强（FTS5 + sqlite-vec）

MVP 同时启用两种检索并融合：

- **FTS5 全文检索**：关键词精确匹配，对人名、代码符号等 exact-match 场景效果好
- **sqlite-vec 向量检索**：语义相似度召回，对模糊/同义表达场景效果好
- **融合策略**：两路结果取并集，按 (语义相关度 × 0.5 + FTS5 BM25 分数 × 0.3 + 新近性 × 0.2) 加权重排

> 与 OpenClaw 的 hybrid BM25+vector 方案思路一致，但实现更轻量（SQLite 内置 FTS5 + sqlite-vec 扩展，零外部依赖）。

### 5.7 设计备忘（对比 OpenClaw 后的决策）

| 设计点 | nanocrab 选择 | 理由 |
|---|---|---|
| 自动记录每条消息 | ❌ 不做 | 会制造噪声；episodes 由 LLM 判断是否值得写入 |
| 结构化元数据约束 | ❌ 不约束写入格式 | 改为索引/检索阶段自动推断元数据 |
| 独立冲突检测系统 | ❌ 不做 | Consolidation LLM 同时看新旧内容，自然处理 |
| FTS5 + sqlite-vec | ✅ MVP 必做 | 零依赖，hybrid 检索显著优于单一方式 |
| Consolidation 定时任务 | ✅ MVP 必做 | 产出与人手写内容隔离 |
| 语义感知分块 | vNext | 按 heading 切分 + 超长段落退化处理 |
| 本地 Embedding 模型 | vNext | 预留接口，MVP 用远程 API |

---

## 6. 多 Agent / Sub-Agent 设计（MVP）

### 6.1 多 Agent（必须）

- `agent_id` 为一等字段，必须进入：
  - Command/Event schema
  - session_key 生成
  - memory 分区键
- Core 内提供 `AgentRegistry`：
  - 解析可用 agent 列表
  - 基于 routing 规则选择目标 agent
  - 加载该 agent 的 model/tools/memory/persona 策略

### 6.2 Sub-Agent（必须，最小可用）

MVP 支持：

- `spawn(task, agent_id?)`
- `cancel(run_id)`
- `timeout(run_id, ttl)`
- `result_merge(parent_session)`

约束：

- 默认禁止子代理递归再 spawn（避免失控）
- 子代理工具集默认比主代理更收敛（最小权限）
- 子代理必须带 `parent_run_id` 与 `trace_id`，便于审计

### 6.3 Persona（参考 OpenClaw 但做无 workspace 版）

OpenClaw 的实践是“结构化 identity + 文本化行为规则”。  
nanocrab MVP 保留此思想，但不依赖 workspace 文件系统。

- **IdentityProfile（结构化）**：`name/emoji/avatar/public_label`
- **BehaviorProfile（文本化）**：`system/style/safety` 提示模板

人设加载与组装在 Core 内完成，Gateway 不参与。

## 7. 配置管理（YAML，非数据库）

### 7.1 选择结论

- MVP 配置格式：**YAML**
- 不使用数据库作为配置源
- 运行时解析后映射为强类型结构体

### 7.2 建议目录结构

- `config/main.yaml`（全局）
- `config/agents.d/*.yaml`（每个 Agent 一份）
- `config/routing.yaml`（channel/connector -> agent 绑定）
- `prompts/<agent_id>/system.md`
- `prompts/<agent_id>/style.md`
- `prompts/<agent_id>/safety.md`

### 7.3 配置校验

- 启动时做 schema 校验（必填字段、引用存在、重复 id）
- 校验失败即阻止启动（fail fast）

## 8. LLM Provider 设计（MVP）

### 8.1 目标

- MVP 首发支持 **Anthropic**
- 架构上从第一天支持可扩展（OpenAI/OpenRouter/本地推理后续可插）

### 8.2 核心抽象

采用 `Provider Registry + Adapter`：

- `LlmProvider`（统一 trait）
  - `chat(request) -> response`
  - `stream(request) -> stream`（可选，MVP 可先占位）
  - `health()`（可选）
- `ProviderRegistry`
  - 根据 `provider_id` 注册/构造具体 provider
  - Core 只依赖 trait，不依赖具体 SDK

### 8.3 模型解析与回退

- `agent.model_policy.primary` 指定主模型
- `agent.model_policy.fallbacks` 指定回退模型链
- 回退触发建议：429 / timeout / transient 5xx

### 8.4 配置策略（YAML）

建议目录：

- `config/providers.d/anthropic.yaml`
- `config/providers.d/*.yaml`

密钥不落 agent 配置文件，优先从环境变量/secret 注入。

### 8.5 MVP 范围

- 实现 `anthropic` adapter（可先 stub）
- 预留 registry 与 provider trait
- 代码结构独立为 `nanocrab-provider` crate

## 9. Skill 系统（MVP）

### 9.1 目标

- 在 MVP 阶段提供可扩展能力描述层（不是硬编码在 Core）
- 兼容后续插件化演进

### 9.2 技术方案（轻量版）

- Skill 目录结构：`skills/<skill_name>/SKILL.md`
- `SKILL.md` 采用 frontmatter（`name/description/metadata`）
- Skill Loader 负责：
  - 加载与合并（优先级：workspace > user > builtin）
  - requirements 门控（`requires.bins` / `requires.env`）
  - 生成 Skills 索引摘要（供模型低成本感知）

### 9.3 Prompt 注入策略

- 默认只注入 Skills 摘要（name/description/location）
- 需要时由 agent 按需读取对应 `SKILL.md` 正文
- 避免全量注入导致上下文膨胀

### 9.4 与 Tool Schema 关系

- Tool Schema：定义“如何调用工具”（参数契约）
- Skill：定义“何时调用工具/怎么完成任务”（策略与经验）
- MVP 同时保留二者，职责分离

## 10. 项目结构（面向后续开源拆分）

建议使用 Rust workspace（monorepo）：

- `crab-gateway`：接入层
- `crab-core`：orchestrator + session + policy
- `crab-schema`：command/event DTO（稳定边界）
- `crab-bus`：总线抽象与 in-memory 实现
- `crab-memory`：episodes/concepts/links 存取
- `crab-runtime`：执行器接口（WASM adapter 预留）
- `crab-channels-telegram`：首个通道驱动
- `crab-sdk`：后续插件/第三方接入

### 6.1 依赖规则（必须遵守）

1. 跨模块通信只走 `crab-schema`
2. `gateway` 不能直接依赖 `memory` 存储实现
3. `core` 依赖 trait，不依赖具体基础设施实现
4. 通道模块不包含业务决策代码

---

## 11. CLI / TUI 支持（MVP）

### 9.1 CLI（必须）

用于一次性命令操作：

- 启停服务（gateway start/stop/restart）
- 配置校验与加载
- agent 管理（list/add/enable/disable）
- 任务触发与排障命令

### 9.2 TUI（必须，开发者向）

TUI 有两个职责：**实时观测面板** + **本地 Chat 入口**。

#### 观测面板

- Active Sessions 面板
- Event Bus 队列面板（inbound/outbound/backlog）
- Runs/Sub-Agent 面板（状态、耗时、失败重试）
- Logs/Trace 面板（按 trace_id 过滤）

#### 本地 Chat 入口（类 Claude Code 交互体验）

TUI 作为本地交互通道，直接调用 Orchestrator（不经过 Gateway），提供流式对话体验：

```
┌─ nanocrab TUI ──────────────────────────────┐
│                                              │
│  You: 分析一下项目架构                      │
│                                              │
│  nanocrab-main: 我来看看项目结构...█        │  ← 流式逐字输出
│                                              │
│  [tool: shell_exec("find crates -name...")] │  ← tool_use 实时展示
│  [result: 10 crates found]                   │
│                                              │
│  项目采用 Rust workspace，包含 10 个 crate：│  ← 继续流式输出
│                                              │
├──────────────────────────────────────────────┤
│ > _                                          │
└──────────────────────────────────────────────┘
```

**架构位置：** TUI 与 TelegramBot 并列，是另一个通道入口，但走进程内直接调用：

```
nanocrab 进程
  ├── TelegramBot ──▶ Gateway ──▶ Orchestrator  （远程通道，走完整链路）
  └── TUI ──▶ Orchestrator（流式接口）           （本地通道，直接调用）
```

TUI 不需要经过 Gateway（本地使用无需限流/路由/鉴权）。

**流式 + 工具调用交替执行循环：**

```
loop {
    // 1. 流式调 LLM
    let stream = orchestrator.handle_inbound_stream(messages).await;
    
    // 2. 逐 chunk 实时渲染到终端
    for chunk in stream {
        tui.render_delta(chunk.delta);
    }
    
    // 3. 检查是否有 tool_use
    if has_tool_use(&response) {
        let tool_results = execute_tools(tool_calls).await;
        tui.render_tool_results(&tool_results);
        
        // 把 tool_result 加入 messages，继续下一轮（还是流式）
        messages.extend(tool_use_and_result_messages);
        continue;
    }
    
    break;  // 没有 tool_use，结束
}
```

**需要打通的流式链路：**

| 层 | 当前状态 | 需要补的 |
|---|---|---|
| Provider `stream()` | ✅ 已实现（Anthropic SSE 解析完整） | — |
| LlmRouter `stream()` | ❌ 只有 `chat()` | 加 `stream()` 方法，路由到 provider.stream() |
| Orchestrator | ❌ 只有同步 `handle_inbound()` | 加 `handle_inbound_stream()` 返回 `Stream<StreamChunk>` |
| TUI Chat 面板 | ❌ 未实现 | 消费 stream，逐 chunk 渲染 + tool use 交替展示 |

> **注意**：Telegram 等远程通道的流式（send_message + edit_message）属于体验优化，不在 MVP 范围。MVP 流式输出聚焦 TUI 本地 Chat 场景。

建议实现：`ratatui + crossterm`。

## 12. 执行链路（MVP）

### 12.1 主链路（同步直接调用）

```
TelegramBot
  │  teloxide long polling 收到消息
  │  构造 InboundMessage
  │
  ▼  Arc<Gateway>.handle_inbound()     ← 进程内函数调用
Gateway
  │  限流(TokenBucket) + resolve_agent
  │
  ▼  Arc<Orchestrator>.handle_inbound() ← 进程内函数调用
Orchestrator
  │  Session 管理 → Persona + Skill 组装
  │  → Memory 召回 → 构造 messages
  │
  ▼  HTTP POST → Anthropic API         ← 唯一的外部网络调用
  │  ← LLM 回复
  │
  │  记录 episodes → 构造 OutboundMessage
  ▼  return Ok(OutboundMessage)         ← 函数返回值
Gateway
  ▼  return Ok(outbound)                ← 函数返回值
TelegramBot
  ▼  bot.send_message() → Telegram API  ← HTTP 发送回复
```

设计原则：主链路是同步因果关系（用户发消息 → 必须等 LLM 回复 → 发回去），保持直接调用最简单可靠。

### 12.2 旁路事件（Bus 广播）

主链路执行过程中，通过 Bus 广播非阻塞事件：

```
主链路节点          ──publish──▶  Bus  ──▶  消费者
Gateway            MessageAccepted       TUI 面板、审计日志、metrics
Orchestrator       ReplyReady            TUI 面板、审计日志
Orchestrator       TaskFailed            TUI 面板、告警系统
Orchestrator       ToolExecuted (vNext)  审计日志、TUI
Memory             EpisodeWritten(vNext) TUI、统计
```

旁路事件 fire-and-forget，消费者失败不影响主链路。

---

## 13. 第一版里程碑（建议）

### M1（可跑通）

- Telegram 入站/出站
- Core 基础路由
- Session 持久化

### M2（可用）

- episodes 写入 + 检索
- concepts 手动/规则写入
- 回答前记忆注入

### M3（可演进）

- 每日巩固任务
- 冲突标注与简单遗忘
- runtime wasm adapter 骨架

---

## 14. 结论

nanocrab MVP 推荐采用：

- **架构层面**：Gateway + Core/Orchestrator + Bus + Memory + Runtime
- **记忆层面**：海马体（快写入）+ 皮质（慢整合）
- **工程策略**：先轻实现，先稳协议，先保边界

这套方案既能快速落地第一版，也保证后续模块可独立开源与扩展。
