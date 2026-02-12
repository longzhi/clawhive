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

---

## 9. 建议落地节奏

1. 在 `docs/` 保持本文件为 vNext 设计基线
2. MVP 完成后，先实现 `CapabilityGrant` 数据结构 + 审计日志
3. 再引入 Policy Engine（Safe/Guarded/Unsafe）
4. 最后接 Planner 与高级执行策略

---

## 10. 结论

nanocrab 在 vNext 采用“Capability-based, per-task least-privilege”模型，将显著提升：

- 安全性（默认最小授权）
- 可控性（审批与策略分层）
- 可解释性（审计可追溯）
- 可扩展性（WASM 与宿主代理共存）

该模型建议作为 nanocrab 完整版核心能力之一。