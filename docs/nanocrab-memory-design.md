# nanocrab 记忆系统设计方案

> 日期：2026-02-13  
> 状态：设计确认（v2，采纳 OpenClaw 记忆模型）  
> 核心思路：Markdown 文件即记忆，SQLite 仅作检索索引

---

## 1. 设计哲学

**Markdown 是 source of truth，SQLite 是搜索引擎。**

采用 OpenClaw 验证过的记忆模型：LLM 像人一样直接读写 Markdown 文件，不引入额外的结构化数据库层。SQLite + sqlite-vec + FTS5 纯粹作为检索加速层，不持有权威数据。

### 1.1 三层记忆架构

借鉴神经科学的记忆模型，nanocrab 的记忆体系有三个层次：

```
┌──────────────────────────────────────────────────┐
│                nanocrab 记忆系统                    │
│                                                    │
│  ① Session JSONL（感觉记忆 / 工作记忆）            │
│     sessions/<id>.jsonl                            │
│     ↓ 原始对话流，完整保留                          │
│                                                    │
│  ② Daily Files（短期记忆）                         │
│     memory/YYYY-MM-DD.md                           │
│     ↓ LLM 主动记录 + 兜底摘要                      │
│                                                    │
│  ③ MEMORY.md（长期记忆）                           │
│     ↓ 海马体整合                   │
│                                                    │
│  🔍 SQLite 检索层（只读索引）                       │
│     sqlite-vec + FTS5 + chunks 表                  │
└──────────────────────────────────────────────────┘
```

**海马体（Hippocampus）**：定期从短期记忆（daily files）中提炼知识，整合到长期记忆（MEMORY.md）——与神经科学中海马体将短期记忆巩固为长期记忆的过程一致。

**为什么放弃双轨制：**
- 双向同步增加了大量复杂度（conflict resolution、watch、debounce），收益有限
- Concepts/Links 图结构过度设计——LLM 直接读 Markdown 比查结构化数据库更自然
- OpenClaw 的实践证明：Markdown + 向量索引已经足够好

---

## 2. 记忆文件结构

### 2.1 目录布局

```
workspace/
├── MEMORY.md              ← 长期记忆（curated wisdom）
└── memory/
    ├── 2026-02-13.md      ← 每日记录（raw log）
    ├── 2026-02-12.md
    └── ...
```

### 2.2 MEMORY.md（长期记忆）

**性质：** 精炼后的核心知识，类似人的长期记忆。

**内容示例：**
```markdown
# MEMORY.md

## 用户偏好
- 开发语言：Rust，偏好极简设计
- 文档风格：中文为主，技术术语保留英文
- 工具链：neovim + wezterm + zellij

## 项目决策
- 数据库：SQLite + sqlite-vec，零外部依赖
- 架构：单进程 monolith，Bus 仅做 sidecar 广播
- 记忆系统：Markdown 为 source of truth

## 重要事实
- nanocrab 仓库：/Users/dragon/Workspace/nanocrab/
- Obsidian vault 会同步到 GitHub
```

**写入时机：**
1. Compaction 前的 memory flush（context 快满时，LLM 将重要信息写入）
2. 海马体定时任务（从近期 daily files 提炼精华）
3. LLM 在对话中主动写入（发现重要信息时）

**规则：**
- LLM 直接读写，无格式约束
- 人也可以手动编辑
- 定期清理过时信息

### 2.3 memory/YYYY-MM-DD.md（每日记录）

**性质：** 当天的原始日志，不需要精炼。

**内容示例：**
```markdown
# 2026-02-13

## nanocrab 架构 Review
- 完成 message flow 审查，确认主路径是 Gateway→Orchestrator 直接调用
- Bus 定位为 sidecar 广播，不在主路径上
- 记忆系统从双轨制改为 OpenClaw 模式（Markdown 为 source of truth）

## 设计决策
- 放弃 episodes/concepts/links 图模型
- 采用 Markdown + sqlite-vec/FTS5 检索
```

**写入时机：** 随时。LLM 在对话过程中记录值得保留的内容。

**兜底机制：** 如果一个 session 结束时 LLM 没有写入任何内容，触发兜底摘要——用 LLM 对整段对话生成一条总结，写入当天的 daily file。

---

## 3. 检索系统

### 3.1 索引构建

Markdown 文件变化时（启动时全量 + 运行时 watch 增量），构建 SQLite 检索索引。

> 以下 schema 与 OpenClaw 对齐。

**分块参数（对齐 OpenClaw 默认值）：**
- chunk 目标大小：~400 tokens
- chunk 重叠：~80 tokens
- 超长段落退化为固定窗口切分

#### SQLite Schema

**表 1：`meta`（索引元数据）**

| 字段 | 类型 | 说明 |
|------|------|------|
| key | TEXT (PK) | 元数据键 |
| value | TEXT | 元数据值（如 embedding provider/model fingerprint，用于判断是否需要 reindex） |

**表 2：`files`（已索引文件快照）**

| 字段 | 类型 | 说明 |
|------|------|------|
| path | TEXT (PK) | 文件路径 |
| source | TEXT | 来源分类（`memory` / `sessions`） |
| hash | TEXT | 文件内容哈希（增量更新判断） |
| mtime | INTEGER | 文件修改时间 |
| size | INTEGER | 文件大小 |

**表 3：`chunks`（分块 + embedding）**

| 字段 | 类型 | 说明 |
|------|------|------|
| id | TEXT (PK) | chunk 唯一标识 |
| path | TEXT | 来源文件路径 |
| source | TEXT | 来源分类（`memory`） |
| start_line | INTEGER | 在文件中的起始行号 |
| end_line | INTEGER | 在文件中的结束行号 |
| hash | TEXT | chunk 内容哈希（增量判断） |
| model | TEXT | 生成 embedding 的模型标识 |
| text | TEXT | chunk 原文 |
| embedding | TEXT | 向量（JSON 序列化） |
| updated_at | INTEGER | 最后索引时间 |

**表 4：`embedding_cache`（跨模型 embedding 缓存）**

| 字段 | 类型 | 说明 |
|------|------|------|
| provider | TEXT | embedding provider |
| model | TEXT | embedding model |
| provider_key | TEXT | provider 标识 |
| hash | TEXT | 内容哈希 |
| embedding | TEXT | 向量（JSON 序列化） |
| dims | INTEGER | 向量维度 |
| updated_at | INTEGER | 缓存时间 |
| | | **PK: (provider, model, provider_key, hash)** |

**FTS5 虚拟表：`chunks_fts`**

```sql
CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
  text,
  id UNINDEXED,
  path UNINDEXED,
  source UNINDEXED,
  model UNINDEXED,
  start_line UNINDEXED,
  end_line UNINDEXED
);
```

#### 增量更新策略

1. 启动时扫描 memory 文件，对比 `files` 表的 hash/mtime
2. 仅 hash 变化的文件重新分块 + 重新 embedding
3. **Reindex 触发**：当 `meta` 表中的 embedding provider/model fingerprint 变化时，全量重建索引
4. **运行时 watch**：监听 `memory/` 目录文件变化（debounce），增量更新

### 3.2 混合检索（Hybrid Search）

```
用户发消息 / Orchestrator 需要记忆
  │
  ▼  并行两路检索
  │
  ├── 向量搜索（sqlite-vec）
  │   query_embedding vs chunk embeddings
  │   → cosine similarity
  │
  └── 全文搜索（FTS5 BM25）
      query_text vs chunk content
      → BM25 rank score
  │
  ▼  融合重排
  │
  finalScore = vectorScore × 0.7 + bm25Score × 0.3
  │
  ▼  返回 top-K chunks
```

**融合细节（对齐 OpenClaw 默认值）：**
- `vectorWeight`：0.7（cosine similarity，归一化到 0-1）
- `textWeight`：0.3（BM25 rank score，归一化）
- `candidateMultiplier`：4（candidatePool = maxResults × 4，先取多再排）
- `maxResults`：6
- `minScore`：0.35（低于此分数的结果丢弃）
- 两个权重之和归一化为 1.0（可配置调整比例）

> OpenClaw 不使用 recency 权重，依赖 vector + BM25 已足够。如需 recency 加权可作为 nanocrab 扩展。

### 3.3 Prompt 注入

检索到的 chunks 注入到 LLM prompt 的 system message 中：

```
[Memory Context]

From MEMORY.md:
- 开发语言：Rust，偏好极简设计
- 数据库：SQLite + sqlite-vec，零外部依赖

From memory/2026-02-13.md:
- 记忆系统从双轨制改为 OpenClaw 模式
- Bus 定位为 sidecar 广播

From memory/2026-02-12.md:
- 讨论了 Milvus vs sqlite-vec 选型，确认 sqlite-vec
```

---

## 4. 海马体（Hippocampus）

> **海马体（Hippocampus）** 是 nanocrab 记忆系统的核心进程——定期将短期记忆（daily files + session JSONL）整合为长期记忆（MEMORY.md），如同大脑中海马体在睡眠期间巩固当天经历。

```
定时 Cron 触发（每日低峰 / 可配置）
  │
  ├── 1. 读取近期 daily files（如近 7 天）
  │      └── 可选：扫描 session JSONL 提取 highlights
  │
  ├── 2. 读取当前 MEMORY.md
  │
  ├── 3. 调 LLM（"海马体"）：
  │      prompt 包含新旧内容，要求：
  │      - 提炼值得长期保留的知识
  │      - 发现矛盾时以新内容为准
  │      - 删除过时信息
  │      - 输出更新后的 MEMORY.md
  │
  ├── 4. 写入 MEMORY.md
  │
  └── 5. 重建索引（增量）
```

**设计要点：**
- LLM 同时看到新旧内容，自然处理冲突，不需要独立冲突检测
- 海马体是"整理"不是"提取"——像人睡觉时大脑复习当天经历一样
- 频率可配置（默认每日一次）
- 海马体也可以读 session JSONL 来补充 daily files 遗漏的内容（可选，vNext）

---

## 5. Auto-Compaction（上下文压缩）

当 session 对话 token 数接近 context window 上限时：

```
对话持续进行，token 逼近上限
  │
  ▼  Memory Flush
  │  Orchestrator 提醒 LLM：
  │  "context 即将压缩，请将重要信息写入 MEMORY.md 或 daily file"
  │  LLM 执行写入（agentic turn，对用户静默）
  │
  ▼  Compaction
  │  调 LLM 将旧对话压缩为摘要
  │  保留：摘要 + 近期消息 + 记忆召回结果
  │  丢弃：旧的完整消息
  │
  ▼  继续对话
```

---

## 6. Session JSONL（感觉记忆 / 工作记忆）

> 参考 OpenClaw Session 设计：每个 session 一个 JSONL 文件，append-only，完整记录对话流。

### 6.1 文件布局

```
workspace/sessions/
├── <session_id>.jsonl      ← 一个 session 一个文件
├── <session_id>.jsonl
└── ...
```

### 6.2 JSONL 行类型

每行一个 JSON 对象，`type` 字段区分类型：

```jsonl
{"type":"session","version":1,"id":"<uuid>","timestamp":"...","agent_id":"main"}
{"type":"message","id":"<id>","timestamp":"...","message":{"role":"user","content":"你好"}}
{"type":"message","id":"<id>","timestamp":"...","message":{"role":"assistant","content":"你好！"}}
{"type":"tool_call","id":"<id>","timestamp":"...","tool":"search","input":{...}}
{"type":"tool_result","id":"<id>","timestamp":"...","tool":"search","output":{...}}
{"type":"compaction","id":"<id>","timestamp":"...","summary":"...","dropped_before":"<msg_id>"}
{"type":"model_change","id":"<id>","timestamp":"...","model":"claude-sonnet-4-5"}
```

**核心行类型：**

| type | 说明 |
|------|------|
| `session` | 文件首行，记录 session 元数据（version, agent_id, created_at） |
| `message` | 用户或 agent 消息（role: user / assistant / system） |
| `tool_call` | 工具调用请求 |
| `tool_result` | 工具调用结果 |
| `compaction` | 上下文压缩事件（记录摘要 + 丢弃位置） |
| `model_change` | 模型切换 |

### 6.3 用途

1. **对话恢复**：session 开始时加载最近 N 条 message 行，恢复上下文
2. **审计追溯**：完整原始记录，即使 compaction 压缩了 context，JSONL 保留全部
3. **兜底摘要来源**：session 结束时如果 LLM 没写 memory，从 JSONL 生成摘要
4. **海马体数据源**：海马体读取 JSONL 提取 highlights（补充 daily files）

### 6.4 与记忆文件的关系

```
Session JSONL（原始对话流，自动写入，不删除）
      │
      ├──→ LLM 主动写入 daily file（选择性记录）
      │
      └──→ 兜底摘要写入 daily file（session 结束时）
              │
              └──→ 海马体整合到 MEMORY.md
```

- **JSONL ≠ 记忆**：JSONL 是原始记录，memory 文件是经过筛选/提炼的
- **JSONL 不参与检索**：不索引到 SQLite 检索层（避免噪声）
- **JSONL 可配置保留期**：默认 30 天，过期归档或删除

### 6.5 Append-only 写入

- 每条消息实时 append 到 JSONL，不修改已有行
- Compaction 不删除 JSONL 内容，只追加一条 `compaction` 类型记录
- 这保证了完整的审计链

---

## 7. Embedding 策略（对齐 OpenClaw）

### 7.1 Provider 架构

`EmbeddingProvider` trait，默认 `auto` 模式，按可用性自动选择：

| Provider | 默认模型 | 说明 |
|---|---|---|
| `openai` | `text-embedding-3-small` | OpenAI API |
| `gemini` | `gemini-embedding-001` | Gemini API |
| `voyage` | `voyage-4-large` | Voyage AI |
| `local` | `embeddinggemma-300M` (GGUF) | 本地推理，零 API 依赖 |
| `auto` | 自动选择 | 有远程 API key → 用远程；都没有 → fallback local |

### 7.2 落地节奏

**MVP：**
- 实现 `EmbeddingProvider` trait + `openai` provider（最通用）
- `auto` 模式：检测到 OpenAI API key 就用，否则报错提示配置

**vNext 第一步：**
- 加 `gemini` + `voyage` provider
- 加 `local` provider（`ort` + GGUF 模型，如 `embeddinggemma-300M`）
- `auto` 完整 fallback 链：远程 → local

**vNext 第二步：**
- Embedding 缓存（`embedding_cache` 表，避免重复调用 API）
- Batch API 支持（大量索引时降本）

---

## 8. 落地节奏

### MVP

- [ ] MEMORY.md + memory/YYYY-MM-DD.md 文件读写
- [ ] LLM 主动写入记忆（tool/system prompt 指导）
- [ ] 兜底摘要（session 结束时未写入 → 自动总结）
- [ ] SQLite 索引层（chunks 表 + sqlite-vec + FTS5）
- [ ] Hybrid search（向量 50% + BM25 30% + 新近性 20%）
- [ ] 检索结果注入 prompt
- [ ] Session 历史加载（conversation history）

### vNext 第一步

- [ ] Auto-Compaction + Memory Flush
- [ ] 海马体定时任务
- [ ] 语义感知分块（heading 切分 + 超长退化）
- [ ] 索引增量更新（watch 文件变化）

### vNext 第二步

- [ ] 本地 Embedding 模型
- [ ] 跨 agent 记忆隔离策略
- [ ] 记忆 CLI（查询/管理/调试）
- [ ] TUI 记忆面板
