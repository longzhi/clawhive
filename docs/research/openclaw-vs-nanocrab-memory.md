# OpenClaw vs nanocrab 记忆系统对比

> 来源：2026-02-13 code review 讨论

---

## 设计哲学

| | OpenClaw | nanocrab |
|---|---|---|
| **核心理念** | Markdown 即记忆，文件是 source of truth | 数据库即记忆，SQLite 是 source of truth |
| **存储介质** | 文件系统（.md）+ 派生索引 | SQLite 表（episodes + concepts + links） |
| **谁来写** | LLM 自己写文件（agent 主动调用 read/write） | 系统自动写（Orchestrator 自动记录每条消息） |
| **可审计性** | 人可直接打开 Markdown 看、编辑、git track | 需要 SQL 查询或专门工具 |
| **隐喻** | 日记本 + 笔记系统 | 大脑（海马体 + 皮质） |

## 记忆层次

### OpenClaw — 两层文件

```
~/.openclaw/workspace/
├── MEMORY.md              ← 长期记忆（人工策展，只在主 session 加载）
└── memory/
    ├── 2026-02-12.md      ← 每日日志（append-only）
    └── 2026-02-13.md
```

- 写入策略：LLM 自主决定写什么
- 无结构化 schema，全是自由文本
- MEMORY.md 只在主 session 加载（安全隔离，不泄露到群聊）

### nanocrab — 三表结构

```
SQLite DB
├── episodes    ← 海马体（每条消息自动记录）
├── concepts    ← 皮质（结构化知识，有 confidence/status）
└── links       ← 证据链（episode ↔ concept 关联）
```

- episodes 全自动记录
- concepts 由 Consolidator 用 LLM 从 episodes 中定期提取
- links 记录支持/反驳/更新关系

## 检索能力

| | OpenClaw | nanocrab |
|---|---|---|
| 向量检索 | ✅ 多 provider embedding + sqlite-vec | ❌ 仅 `LIKE '%query%'` |
| BM25 关键词 | ✅ FTS5 | ❌ 无 |
| 混合搜索 | ✅ 向量 70% + BM25 30% | ❌ |
| 时间查询 | ✅ | ✅ 按天数过滤 |
| 实体查询 | ⚠️ 研究阶段 | ✅ concepts 表 Entity 类型 |
| 置信度查询 | ⚠️ 研究阶段 | ✅ concepts 有 confidence |

## 巩固/整合机制

### OpenClaw
- **Compaction**：context window 快满时总结旧对话为摘要
- **Memory Flush**：compaction 前静默提醒 LLM 写持久记忆
- **Heartbeat 维护**：agent 定期自主整理 MEMORY.md
- 执行者：LLM 自己

### nanocrab
- **Consolidator（定时 Cron）**：
  - 读近 24h 高价值 episodes（importance ≥ 0.6）
  - 调 LLM 提取 concepts（JSON 格式）
  - upsert concepts + 建立 links
  - 标记 >30 天未验证 concept 为 Stale
  - 清理 >90 天低重要性 episodes
- 执行者：系统代码，LLM 只负责提取

## Context Window 管理

| | OpenClaw | nanocrab |
|---|---|---|
| Session 历史 | ✅ JSONL 持久化 + auto-compaction | ❌ 无 session 历史 |
| Compaction | ✅ 总结旧消息为摘要 | ❌ |
| Session Pruning | ✅ 裁剪旧 tool results | ❌ |

## 各自优势

### OpenClaw
1. 人可读可编辑 — Markdown 透明度极高
2. 检索成熟 — 混合搜索 + sqlite-vec + 多 embedding provider
3. Compaction — 优雅处理长对话
4. Memory Flush — compaction 前不丢重要信息
5. 安全隔离 — MEMORY.md 主 session 限定
6. 灵活性 — LLM 自主组织记忆结构

### nanocrab
1. 结构化知识 — concepts 有类型/置信度/状态/证据链
2. 自动记录 — 不依赖 LLM "记得"要写
3. 置信度演化 — confidence + Stale/Conflicted
4. 证据链 — 可溯源知识来源
5. 巩固自动化 — 定时提取，不依赖 LLM 主动性

## 建议 nanocrab 借鉴

1. **Markdown 可读层**：SQLite 之上导出 Markdown 视图
2. **向量检索 + FTS5**：实现 sqlite-vec + BM25 混合搜索
3. **Compaction**：借鉴 auto-compaction + memory flush
4. **保持差异化**：结构化 concepts + 置信度 + 证据链是独有优势
