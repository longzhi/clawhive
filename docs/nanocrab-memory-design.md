# nanocrab 记忆系统设计方案

> 日期：2026-02-13  
> 状态：设计确认  
> 核心思路：SQLite 结构化存储 + Markdown 可读层，双轨双向同步

---

## 1. 设计哲学

**双轨制：机器用 SQLite 高效查询，人用 Markdown 透明审计，双向同步保持一致。**

既保留 nanocrab 的结构化优势（concepts/置信度/证据链），又补上 OpenClaw 的透明可控优势。不选择纯 Markdown 路线（丢失结构化能力），也不放弃人可读性（纯数据库不透明）。

```
┌─────────────────────────────────────────────┐
│              nanocrab 记忆系统                │
│                                              │
│  ┌──────────────┐    ┌───────────────────┐  │
│  │  Markdown 层  │◄──►│   SQLite 层       │  │
│  │  （人可读写）  │同步 │  （机器可查询）    │  │
│  │              │    │                    │  │
│  │ MEMORY.md    │    │ episodes 表        │  │
│  │ memory/      │    │ concepts 表        │  │
│  │  YYYY-MM-DD  │    │ links 表           │  │
│  │ entities/    │    │ vec0 (sqlite-vec)  │  │
│  │              │    │ FTS5 (BM25)        │  │
│  └──────────────┘    └───────────────────┘  │
│         │                     │              │
│         ▼                     ▼              │
│  人直接编辑 md          Orchestrator 自动     │
│  → 触发反向同步          记录 + 检索          │
│    更新 SQLite           Consolidator 整合    │
└─────────────────────────────────────────────┘
```

---

## 2. 四层架构

### 2.1 第一层：Episodes（海马体）

**职责：** 自动记录每条对话消息，快写入，保细节。

**数据模型：**

| 字段 | 类型 | 说明 |
|------|------|------|
| id | UUID | 主键 |
| ts | DateTime | 时间戳 |
| session_id | String | 所属 session |
| speaker | String | 发言者（user / agent_id） |
| text | String | 消息文本 |
| embedding | Vec\<f32\> | 向量（sqlite-vec 索引） |
| tags | JSON | 标签 |
| importance | f32 | 重要性 0-1 |
| context_hash | String? | 上下文哈希 |
| source_ref | String? | 来源引用 |

**索引：**
- sqlite-vec `vec0` 虚拟表（向量相似度搜索）
- FTS5 全文索引（BM25 关键词搜索）
- 时间索引（`ts` 列）

**写入策略：** 全自动，每条用户消息和 agent 回复都记录（现有行为）。

**特点：** 这是 OpenClaw 和 memsearch 都没有的——它们依赖 LLM 主动写文件，可能遗漏。nanocrab 自动记录保证不丢任何信息。

### 2.2 第二层：Concepts（皮质）

**职责：** 结构化稳定知识，慢整合，保高置信。这是 nanocrab 的**核心差异化**。

**数据模型（现有，保留）：**

| 字段 | 类型 | 说明 |
|------|------|------|
| id | UUID | 主键 |
| concept_type | Enum | Fact / Preference / Rule / Entity / TaskState |
| key | String | 知识标识符（唯一） |
| value | String | 知识内容 |
| confidence | f32 | 置信度 0-1 |
| evidence | JSON | 证据引用列表 |
| first_seen | DateTime | 首次发现时间 |
| last_verified | DateTime | 最后验证时间 |
| status | Enum | Active / Stale / Conflicted |

**增强：**
- 加 `embedding` 列 + sqlite-vec 向量索引（支持语义查询 concepts）
- 加 FTS5 全文索引
- 冲突检测：新 concept 与已有 concept 语义矛盾时自动标记 `Conflicted`

**证据链（links 表，现有，保留）：**

| 字段 | 类型 | 说明 |
|------|------|------|
| id | UUID | 主键 |
| episode_id | UUID | 关联 episode |
| concept_id | UUID | 关联 concept |
| relation | Enum | Supports / Contradicts / Updates |
| created_at | DateTime | 创建时间 |

### 2.3 第三层：Markdown 可读层（新增）

**目录结构：**

```
workspace/memory/
├── MEMORY.md              ← 从 Active concepts 生成 / 人可编辑
├── 2026-02-13.md          ← 从当日 episodes 生成 / 人可编辑
├── 2026-02-12.md
└── entities/
    ├── nanocrab.md         ← 从 Entity 类型 concepts 生成
    └── peter.md
```

**双向同步机制：**

#### 正向同步（SQLite → Markdown，自动）

Consolidator 完成后自动导出：
- 当日 episodes → `memory/YYYY-MM-DD.md`（按时间排列，标注 speaker）
- Active concepts → `MEMORY.md`（按 concept_type 分组，标注 confidence）
- Entity 类型 concepts → `entities/<name>.md`

```markdown
# MEMORY.md（自动生成示例）

## Facts
- [0.95] db.choice: SQLite + sqlite-vec 作为本地存储方案
- [0.90] lang.primary: Rust

## Preferences
- [0.85] output.style: 简洁优先，代码示例辅助

## Rules
- [0.90] safety.no_rm: 使用 trash 替代 rm
```

#### 反向同步（Markdown → SQLite，监听）

Watch `memory/` 文件夹变化（debounce 1.5s）：
- 解析 Markdown 变更内容
- 对应更新 concepts（人编辑 = confidence 1.0，最高优先级）
- 重建向量索引
- 新增内容自动创建 concept 候选

### 2.4 第四层：Context Window 管理（新增）

**Session 历史持久化：**
- 每条消息写入 JSONL 文件（`sessions/<session_id>.jsonl`）或复用 episodes 表
- 对话时加载最近 N 条历史（可配置 `session.history_window`）

**Auto-Compaction（借鉴 OpenClaw）：**

```
Session 对话持续进行
  │
  │  估算 token 数接近 context window 上限
  ▼
Memory Flush（静默 agentic turn）
  │  提醒 Orchestrator 将重要信息写入 concepts
  ▼
Auto-Compaction
  │  调 LLM 总结旧对话为摘要
  │  保留：摘要 + 近期消息 + concepts
  │  删除：旧的完整消息
  ▼
继续对话（摘要 + 近期消息 + 记忆召回）
```

---

## 3. 检索策略

### 3.1 三级并行检索

```
用户发消息
  │
  ▼  并行检索三个来源
  │
  ├── 1. Concepts 精确查询
  │   → type/key/status 过滤
  │   → 返回结构化知识 + confidence
  │
  ├── 2. Episodes 混合搜索
  │   → 向量相似度（sqlite-vec）70%
  │   → BM25 关键词（FTS5）30%
  │   → 时间窗口过滤（默认 7 天）
  │
  └── 3. Concepts 向量搜索
      → embedding 语义匹配
      → 返回语义相关的 concepts
  │
  ▼  融合重排
  confidence × relevance × recency 加权
```

### 3.2 混合搜索实现

```
candidatePool = maxResults × 4

// 向量搜索
vectorHits = sqlite_vec_search(query_embedding, candidatePool)

// BM25 搜索
bm25Hits = fts5_search(query_text, candidatePool)

// 融合
for each candidate in union(vectorHits, bm25Hits):
    textScore = 1.0 / (1.0 + max(0, bm25_rank))
    finalScore = 0.7 * vectorScore + 0.3 * textScore

// 返回 top-K
return sorted(candidates, by=finalScore, limit=maxResults)
```

### 3.3 Prompt 注入格式

```
[Memory Context]

## Known Facts (high confidence)
- db.choice: SQLite + sqlite-vec [confidence: 0.95]
- lang.primary: Rust [confidence: 0.90]

## Recent Conversation
- [02-13 09:10] user: 数据库方案确定了吗？
- [02-13 09:11] nanocrab-main: 是的，已确认使用 SQLite + sqlite-vec...

## Related Context
- [02-12] 讨论了 Milvus vs sqlite-vec 的选型...
```

---

## 4. Consolidation 流程（增强版）

```
每日低峰 Cron 触发（或 Heartbeat 触发）
  │
  ├── 1. 读取近 24h episodes（importance ≥ 0.6）
  │
  ├── 2. 调 LLM 提取 concept 候选（JSON）
  │
  ├── 3. 冲突检测
  │   └── 新 concept 与已有 concept 语义对比
  │       ├── 一致 → 更新 last_verified，提高 confidence
  │       ├── 矛盾 → 标记 Conflicted，保留双方 evidence
  │       └── 全新 → 创建新 concept
  │
  ├── 4. 建立 episode → concept links
  │
  ├── 5. 维护
  │   ├── 标记 >30 天未验证 concept 为 Stale
  │   └── 清理 >90 天低重要性 episodes
  │
  ├── 6. 导出 Markdown（正向同步）
  │   ├── concepts → MEMORY.md
  │   ├── 当日 episodes → memory/YYYY-MM-DD.md
  │   └── Entity concepts → entities/*.md
  │
  └── 7. 重建向量索引（增量）
```

---

## 5. 与竞品的差异定位

| 维度 | OpenClaw | memsearch | nanocrab |
|------|----------|-----------|----------|
| Source of truth | Markdown | Markdown | **SQLite + Markdown 双轨** |
| 结构化知识 | ❌ | ❌ | **✅ concepts + confidence + evidence** |
| 自动记录 | ❌ 依赖 LLM | ❌ 依赖 LLM | **✅ 每条消息自动** |
| 向量检索 | ✅ | ✅ (Milvus) | **✅ sqlite-vec** |
| BM25 | ✅ FTS5 | ✅ | **✅ FTS5** |
| 混合搜索 | ✅ 70/30 | ✅ 70/30 | **✅ 70/30** |
| 人可编辑 | ✅ | ✅ | **✅ 双向同步** |
| Compaction | ✅ | ✅ | **✅** |
| 知识冲突检测 | ❌ | ❌ | **✅ Conflicted 状态** |
| 置信度演化 | ❌ | ❌ | **✅** |
| 证据溯源 | ❌ | ❌ | **✅ links 表** |
| 外部依赖 | Node.js | Python + Milvus | **零依赖（Rust + SQLite）** |

---

## 6. 落地节奏

### MVP（当前阶段）

- [x] episodes 自动记录
- [x] concepts + links 数据模型
- [x] Consolidator 定时提取
- [ ] sqlite-vec 向量索引（episodes + concepts）
- [ ] FTS5 全文索引
- [ ] 混合搜索（向量 70% + BM25 30%）
- [ ] Session 历史加载（conversation history 注入）

### vNext 第一步

- [ ] Markdown 可读层导出（正向同步）
- [ ] Watch + 反向同步
- [ ] Auto-Compaction + Memory Flush
- [ ] Concepts 冲突检测

### vNext 第二步

- [ ] Entity 页面自动生成
- [ ] 跨 agent 记忆共享/隔离策略
- [ ] 记忆 API（CLI 查询/管理）
- [ ] TUI 记忆面板
