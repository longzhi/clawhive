# nanocrab Code Review Issues

> 来源：2026-02-13 消息入口链路 review（Telegram → Agent）  
> 状态标记：🔴 待修复 | 🟡 待讨论 | 🟢 已解决

---

## Issue #1: Bus 是旁路，非主链路驱动

**状态：** 🟡 待讨论  
**模块：** `nanocrab-gateway`, `nanocrab-bus`  
**描述：**  
当前消息流是 TelegramBot → Gateway → Orchestrator 的直接同步调用链，Bus 仅用于旁路通知（`MessageAccepted` / `ReplyReady` / `TaskFailed`）。与 MVP 技术文档 §3 设计的「Command/Event 驱动」模式有差距。  
**影响：** 模块耦合度高于预期，后续接入新通道或做异步编排时需要重构调用方式。  
**建议：** MVP 阶段可接受，但应在 M2/M3 阶段将主链路切换为 Bus 驱动（Gateway publish Command → Core subscribe 处理 → publish Event → Gateway 回写）。

---

## Issue #2: 无消息队列缓冲，LLM 慢响应会积压

**状态：** 🟢 已解决  
**模块：** `nanocrab-channels-telegram`  
**描述：**  
`TelegramBot::run()` 的 endpoint closure 直接 await Gateway 返回。如果 LLM 响应慢（数秒甚至超时），teloxide dispatcher 的并发处理能力受限，可能导致消息积压或丢失。  
**影响：** 高并发场景下用户体验差，消息处理可能超时。  
**修复：** endpoint 中先发 `ChatAction::Typing`，然后 `tokio::spawn` gateway 调用，endpoint 立即返回。spawned task 完成后主动 `bot.send_message()` 发送回复。

---

## Issue #3: Session 不加载历史对话

**状态：** 🔴 待修复  
**模块：** `nanocrab-core/orchestrator.rs`  
**描述：**  
`Orchestrator::handle_inbound()` 中 `SessionManager::get_or_create()` 只管理 session 元数据（创建/续期/过期），没有将 session 内的历史对话消息加入 LLM 的 messages 列表。当前每次对话只有：
- 记忆召回的 episodes（作为 `[memory context]`）
- 当前用户输入

缺少 conversation history（最近 N 轮对话），导致 agent 无法进行连续多轮对话。  
**影响：** 用户体验：agent 没有短期对话记忆，每次都像新对话。  
**建议：**  
1. 在 `handle_inbound` 中从 `episodes` 表查询当前 session 最近 N 条记录（按 `session_id` + 时间排序）
2. 将历史对话作为 messages 注入到 LLM 请求中（在 memory context 之后、当前用户消息之前）
3. 可配置窗口大小（如 `session.history_window: 20`）

---

## Issue #4: Runtime `execute()` 语义不明确

**状态：** 🟡 待讨论  
**模块：** `nanocrab-core/orchestrator.rs`, `nanocrab-runtime`  
**描述：**  
`runtime.execute()` 在 `handle_inbound` 中被调用了两次：
1. 处理用户输入文本：`self.runtime.execute(&inbound.text)`
2. 处理 LLM 输出文本：`self.runtime.execute(&reply_text)`

从上下文看 `NativeExecutor` 可能是 pass-through（原样返回），但语义不清晰——用户输入为什么需要经过 runtime execute？LLM 输出又为什么需要？  
**影响：** 代码可读性差，后续维护者容易困惑。如果 execute 有副作用，可能产生非预期行为。  
**建议：**  
1. 明确 `TaskExecutor::execute()` 的职责文档
2. 如果是为后续 WASM 预留，考虑拆分为 `preprocess_input()` 和 `postprocess_output()` 两个语义明确的方法

---

## Issue #5: Weak ReAct 缺少 Prompt 指令

**状态：** 🔴 待修复  
**模块：** `nanocrab-core/orchestrator.rs`, `nanocrab-core/persona.rs`  
**描述：**  
`weak_react_loop()` 依赖 LLM 输出特定标记（`[think]`、`[action]`、`[finish]`）来驱动循环，但当前没有看到在 system prompt 中注入这些标记的使用说明。Persona 的 `assembled_system_prompt()` 和 Skill 的 `summary_prompt()` 中是否包含 ReAct 指令需要确认。  
**影响：** 如果 LLM 不知道这些标记的存在，永远不会输出 `[think]`/`[action]`，ReAct 循环实际上退化为单轮调用。  
**建议：**  
1. 在 system prompt 组装阶段注入 Weak ReAct 的行为指令模板
2. 或在 `Orchestrator` 中硬编码一段 ReAct instruction 拼接到 system prompt 末尾

---

## Issue #6: TelegramBot endpoint 阻塞 dispatcher

**状态：** 🟢 已解决（同 Issue #2）  
**模块：** `nanocrab-channels-telegram`  
**描述：**  
当前 TelegramBot 的 endpoint handler 直接 `await gateway.handle_inbound(inbound)`，LLM 响应期间（5-30 秒）阻塞 teloxide dispatcher。多用户并发时后续消息排队等待，严重时可能因 long polling 超时导致消息丢失。  
**影响：** 并发场景下用户体验差，消息处理可能超时或丢失。  
**修复：** 同 Issue #2。endpoint 发 `ChatAction::Typing` 后 `tokio::spawn` 异步处理，立即返回 dispatcher。

---

## Issue #7: Bus 事件无消费者

**状态：** 🟡 待修复  
**模块：** `nanocrab-bus`  
**描述：**  
Bus 当前发布了 `MessageAccepted`、`ReplyReady`、`TaskFailed` 等事件，但没有任何代码订阅和消费这些事件。Bus 处于"发了没人听"的状态。  
**影响：** Bus 占用代码但无实际作用，TUI 面板和审计日志也没有数据源。  
**建议：**  
1. MVP 阶段至少接入 TUI 面板消费 `MessageAccepted` / `ReplyReady` / `TaskFailed`
2. 接入审计日志 writer 消费关键事件写入 SQLite
3. Bus 定位已明确为旁路广播（见 MVP 文档 §2.1 / §12），不参与主链路

---

## Issue #8: SubAgentRunner 未接入 Orchestrator

**状态：** 🔴 待修复  
**模块：** `nanocrab-core/orchestrator.rs`, `nanocrab-core/subagent.rs`  
**描述：**  
`SubAgentRunner` 骨架已实现（spawn/cancel/wait_result/result_merge），但 Orchestrator 中没有任何代码使用它。Sub-Agent 能力处于"写了但没接上"的状态。  
**影响：** MVP 文档 §6 明确要求 Sub-Agent 为必做项，当前无法使用。  
**建议：**  
1. 在 Orchestrator 中持有 `SubAgentRunner` 实例
2. 定义触发条件：可先通过 LLM 文本标记（如 `[delegate: agent_id] task`）或未来通过 tool_use 触发
3. spawn 结果合并回 parent 上下文后继续生成

---

## Issue #9: 流式输出链路未打通（Provider 已实现，上层未接入）

**状态：** 🔴 待修复  
**模块：** `nanocrab-core/router.rs`, `nanocrab-core/orchestrator.rs`, `nanocrab-tui`  
**描述：**  
`AnthropicProvider::stream()` 和 `StreamChunk` 类型已完整实现（SSE 解析、三种事件类型），但上层链路完全未接入：
- `LlmRouter` 只有 `chat()` 没有 `stream()`
- `Orchestrator` 只有同步 `handle_inbound()` 没有流式接口
- TUI 没有 Chat 面板消费 stream

**影响：** TUI 作为本地 Chat 入口无法提供流式交互体验，与 Claude Code 类似的逐字输出无法实现。  
**建议：**  
1. `LlmRouter` 加 `stream()` 方法（路由到 provider.stream()，支持 fallback）
2. `Orchestrator` 加 `handle_inbound_stream()` 返回 `Stream<StreamChunk>`
3. TUI Chat 面板消费 stream + tool use 交替执行循环
4. Telegram 等远程通道的流式回复（send + edit）留作后续优化

---

## 后续 Review 计划

- [ ] 记忆系统存取细节（MemoryStore / retrieve_context / consolidation）
- [ ] Provider 实现（Anthropic adapter）
- [ ] Config 加载与校验链路
- [ ] Skill 系统加载与注入
- [ ] Sub-Agent spawn 与生命周期
