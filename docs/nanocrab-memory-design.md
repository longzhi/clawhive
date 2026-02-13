# nanocrab 记忆系统设计方案

> 日期：2026-02-13  
> 状态：设计确认（v2，采纳 OpenClaw 记忆模型）  
> 核心思路：Markdown 文件即记忆，SQLite 仅作检索索引

---

## 1. 设计哲学

**Markdown 是 source of truth，SQLite 是搜索引擎。**

直接采用 OpenClaw 验证过的记忆模型：LLM 像人一样直接读写 Markdown 文件，不引入额外的结构化数据库层。SQLite + sqlite-vec + FTS5 纯粹作为检索加速层，不持有权威数据。

**为什么放弃双轨制：**
- 双向同步增加了大量复杂度（conflict resolution、watch、debounce），收益有限
- Concepts/Links 图结构过度设计——LLM 直接读 Markdown 比查结构化数据库更自然
- OpenClaw 的实践证明：Markdown + 向量索引已经足够好

```
┌─────────────────────────────────────┐
│        nanocrab 记忆系统             │
│                                     │
│  Source of Truth:                   │
│  ┌───────────────────────────┐     │
│  │  Markdown 文件             │     │
│  │  ├── MEMORY.md（长期记忆）  │     │
│  │  └── memory/               │     │
│  │      ├── 2026-02-13.md     │     │
│  │      └── 2026-02-12.md     │     │
│  └───────────────────────────┘     │
│           │ 索引                    │
│           ▼                         │
│  ┌───────────────────────────┐     │
│  │  SQLite 检索层（只读索引）  │     │
│  │  ├── sqlite-vec（向量）    │     │
│  │  ├── FTS5（全文）          │     │
│  │  └── chunks 表（分块元数据）│     │
│  └───────────────────────────┘     │
└─────────────────────────────────────┘
```

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
2. Consolidation 定时任务（从近期 daily files 提炼精华）
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

Markdown 文件变化时（启动时全量 + 运行时增量），构建 SQLite 检索索引：

```
Markdown 文件
  │
  ├── 按 heading 分块（## 级别切分）
  │   └── 超长段落退化为固定窗口（~512 tokens）
  │
  ├── 每个 chunk 生成 embedding → sqlite-vec
  │
  └── 每个 chunk 文本 → FTS5 全文索引
```

**chunks 表：**

| 字段 | 类型 | 说明 |
|------|------|------|
| id | INTEGER | 主键 |
| file_path | TEXT | 来源文件路径 |
| heading | TEXT | 所属 heading |
| content | TEXT | chunk 文本 |
| embedding | BLOB | 向量（sqlite-vec） |
| char_offset | INTEGER | 在文件中的字符偏移 |
| updated_at | INTEGER | 最后索引时间 |

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
  finalScore = vectorScore × 0.5
             + bm25Score × 0.3
             + recencyScore × 0.2
  │
  ▼  返回 top-K chunks
```

**融合细节：**
- `vectorScore`：cosine similarity，归一化到 0-1
- `bm25Score`：`1.0 / (1.0 + max(0, bm25_rank))`，归一化
- `recencyScore`：基于文件日期衰减（MEMORY.md 永远为 1.0，daily files 按天数衰减）
- candidatePool = maxResults × 4（先取多再排）

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

## 4. Consolidation（定期整理）

> 产出与人手写内容混合在同一文件中，但 commit message 标注来源。

```
定时 Cron 触发（每日低峰 / 可配置）
  │
  ├── 1. 读取近期 daily files（如近 7 天）
  │
  ├── 2. 读取当前 MEMORY.md
  │
  ├── 3. 调 LLM：
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
- Consolidation 是"整理"不是"提取"——像人复习笔记一样
- 频率可配置（默认每日一次）

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

## 6. Session 历史

每条消息持久化到 JSONL 文件（`sessions/<session_id>.jsonl`），用于：
- 对话开始时加载最近 N 条历史
- Compaction 后保留完整原始记录（可审计）
- 不同于 memory 文件——这是原始对话记录，不是记忆

---

## 7. Embedding 策略

### MVP
- 远程 API（OpenAI `text-embedding-3-small` 或同类）
- 预留 `EmbeddingProvider` trait 接口

### vNext
- 本地模型（`ort` + ONNX，如 `all-MiniLM-L6-v2`）
- 消除对远程 API 的依赖

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
- [ ] Consolidation 定时任务
- [ ] 语义感知分块（heading 切分 + 超长退化）
- [ ] 索引增量更新（watch 文件变化）

### vNext 第二步

- [ ] 本地 Embedding 模型
- [ ] 跨 agent 记忆隔离策略
- [ ] 记忆 CLI（查询/管理/调试）
- [ ] TUI 记忆面板
