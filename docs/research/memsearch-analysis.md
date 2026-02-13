# memsearch 调研分析

> 来源：2026-02-13 调研  
> 推文：https://x.com/yinmin1987/status/2021835061801496604  
> 项目：https://github.com/zilliztech/memsearch  
> 作者：Zilliz 团队（Milvus 的公司）  
> 协议：MIT

---

## 1. 项目定位

把 OpenClaw 的记忆系统核心设计抽离出来，做成独立 Python 库。让任何 Agent 框架都能用 OpenClaw 同款记忆力，不必依赖整个 OpenClaw 生态。

## 2. 核心理念

**Markdown 是 source of truth，向量数据库只是派生索引。**

```
~/your-project/
└── memory/
    ├── MEMORY.md              # 长期记忆（人工策展）
    ├── 2026-02-09.md          # 每日日志
    └── 2026-02-08.md
```

- 透明：Markdown 明文，打开就知道 AI 记了什么
- 可编辑：vim 改完自动重建索引
- 可移植：复制文件夹就能迁移
- 人机共创：AI 写日志细节，人定长期原则

## 3. 四大工作流程

### Watch（监听）
- 监听 memory 文件夹下 Markdown 变化
- 1500ms 去抖后自动触发重新索引

### Index（索引）
- **分块**：按 heading/段落切分，语义边界清晰
- **去重**：SHA-256 哈希，重复内容只 embed 一次（省 20%+ 成本）
- **Chunk ID**：`hash(source_path:start_line:end_line:content_hash:model_version)`，换 embedding 模型自动识别过期索引

### Search（混合搜索）
- **向量搜索 70%**：语义匹配（"Redis 缓存" ≈ "Redis L1 cache"）
- **BM25 关键词 30%**：精确匹配（错误码、函数名、ID）
- 渐进式披露：Top-K 返回摘要（200 字），需要时 expand 查看完整内容

### Compact（压缩）
- 调 LLM 总结历史记忆为精简摘要
- 删除/归档原始文件
- 可手动触发或定时自动执行

## 4. 技术选型

- **向量数据库**：Milvus（Lite/Server/Zilliz Cloud，一行配置切换）
- **Embedding**：OpenAI、Google、Voyage、Ollama、本地模型
- **搜索**：向量 + BM25 混合（与 OpenClaw 一致的 70/30 配比）

## 5. 使用方式

### Python API
```python
from memsearch import MemSearch
ms = MemSearch(paths=["./memory/"])
memories = await ms.search("Redis caching", top_k=3)
```

### CLI
```bash
memsearch index ./docs/
memsearch search "Redis caching"
memsearch watch ./docs/
memsearch compact
```

## 6. vs 其他方案对比

| 维度 | memsearch | Mem0 / Zep | OpenClaw 内置 |
|------|-----------|------------|---------------|
| 记忆存储 | Markdown 文件 | 向量数据库 | Markdown 文件 |
| 透明度 | 高（明文可读） | 低（JSON/向量） | 高 |
| 可编辑 | vim 改完自动索引 | 需要 API 调用 | 同 memsearch |
| 向量数据库 | Milvus（可选） | 核心依赖 | sqlite-vec |
| 独立性 | ✅ 独立库 | ✅ 独立库 | ❌ 需要整个 OpenClaw |
| 混合搜索 | ✅ 向量 + BM25 | 部分支持 | ✅ 向量 + BM25 |
| 人机共创 | ✅ | ❌ | ✅ |

---

## 7. OpenClaw 记忆系统向量搜索实现细节

虽然 OpenClaw 以 Markdown 为 source of truth，但在上面建了完整的向量检索层：

### 索引链路
```
Markdown 文件
  ▼  监听变化（debounce 1.5s）
分块（~400 token/块，80 token 重叠）
  ▼
Embedding（多 provider）
  ├── OpenAI text-embedding-3-small（远程，默认）
  ├── Gemini embedding-001（远程）
  ├── Voyage（远程）
  └── 本地 GGUF 模型（node-llama-cpp，离线可用）
  ▼
SQLite 存储（~/.openclaw/memory/<agentId>.sqlite）
  ├── vec0 虚拟表（sqlite-vec 加速向量搜索）
  ├── FTS5 全文索引（BM25 关键词搜索）
  └── Embedding 缓存（SHA 去重）
```

### 混合搜索
```
finalScore = 0.7 × vectorScore + 0.3 × textScore(BM25)
```

- BM25 得分转换：`textScore = 1 / (1 + max(0, bm25Rank))`
- 候选池：两侧各取 `maxResults × candidateMultiplier` 个候选
- Union by chunk id，加权排序

### Fallback 机制
- sqlite-vec 不可用 → JS 进程内 cosine similarity
- FTS5 不可用 → 纯向量搜索
- 远程 embedding 失败 → 本地模型 fallback

### 额外能力
- 实验性 QMD 后端（BM25 + 向量 + reranking，本地 sidecar）
- Session transcript 索引（可选，opt-in）
- Embedding 批量索引（OpenAI/Gemini Batch API，省钱快速）
- 额外路径索引（`memorySearch.extraPaths`）

---

## 8. BM25 算法简述

**BM25（Best Matching 25）** 是经典关键词搜索算法，基于词频统计衡量文档与查询的相关度。

核心公式考虑：
- **词频（TF）**：关键词在文档中出现的次数（有饱和度，不是线性增长）
- **逆文档频率（IDF）**：关键词的稀有度（越少见越有区分度）
- **文档长度归一化**：长文档不会因为词多而占优

### 向量搜索 vs BM25 互补关系

| 场景 | 向量搜索 | BM25 |
|------|---------|------|
| 语义匹配（意思相同但用词不同） | ✅ 强 | ❌ 弱 |
| 精确匹配（错误码、函数名、ID） | ❌ 弱 | ✅ 强 |
| 同义词/改写 | ✅ 强 | ❌ 弱 |
| 长尾关键词 | ❌ 弱 | ✅ 强 |

在 SQLite 中通过 **FTS5 扩展**实现 BM25，创建全文索引即可使用，零额外依赖。

---

## 9. 对 nanocrab 的启示

### 可直接借鉴
1. **混合搜索 70/30 配比**：向量 + BM25，经 OpenClaw 和 memsearch 验证有效
2. **分块策略**：按 heading/段落切分，~400 token/块，80 token 重叠
3. **SHA-256 去重**：避免重复 embedding，降低成本
4. **Chunk ID 设计**：包含 model_version，换模型自动失效旧索引
5. **Watch + 自动重建索引**：文件变更 → debounce → 重新索引

### nanocrab 的差异化优势
1. **结构化知识**：concepts 表（类型/置信度/状态/证据链）— OpenClaw 和 memsearch 都没有
2. **自动记录**：每条消息自动写入 episodes，不依赖 LLM 主动性
3. **巩固机制**：Consolidator 从 episodes 自动提取 concepts，有证据链关联
4. **零外部依赖**：sqlite-vec 嵌入式，不需要 Milvus/外部数据库

### 建议演进方向
1. **补上 FTS5 + BM25**：在 SQLite 中建全文索引（零额外依赖）
2. **实现 sqlite-vec 向量检索**：替代当前的 `LIKE '%query%'`
3. **混合搜索**：向量 + BM25 加权融合
4. **可选的 Markdown 可读层**：从 SQLite 导出 Markdown 视图供人审计
5. **保持结构化优势**：concepts + confidence + evidence 是差异化竞争力
