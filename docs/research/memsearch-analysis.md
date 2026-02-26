# memsearch Research Analysis

> Source: 2026-02-13 research  
> Tweet: https://x.com/yinmin1987/status/2021835061801496604  
> Project: https://github.com/zilliztech/memsearch  
> Author: Zilliz team (the company behind Milvus)  
> License: MIT

---

## 1. Project Positioning

Extracts OpenClaw's core memory system design into a standalone Python library. Allows any Agent framework to use OpenClaw's memory capabilities without depending on the entire OpenClaw ecosystem.

## 2. Core Philosophy

**Markdown is source of truth, vector database is just a derived index.**

```
~/your-project/
└── memory/
    ├── MEMORY.md              # Long-term memory (human curated)
    ├── 2026-02-09.md          # Daily log
    └── 2026-02-08.md
```

- Transparent: Markdown plaintext, open it to see what AI recorded
- Editable: Edit with vim, auto-rebuild index
- Portable: Copy folder to migrate
- Human-AI co-creation: AI writes daily details, humans define long-term principles

## 3. Four Main Workflows

### Watch
- Monitor memory folder for Markdown changes
- 1500ms debounce before auto-triggering re-index

### Index
- **Chunking**: Split by heading/paragraph, clear semantic boundaries
- **Deduplication**: SHA-256 hash, duplicate content only embedded once (saves 20%+ cost)
- **Chunk ID**: `hash(source_path:start_line:end_line:content_hash:model_version)`, changing embedding model auto-detects stale indexes

### Search (Hybrid)
- **Vector search 70%**: Semantic matching ("Redis caching" ≈ "Redis L1 cache")
- **BM25 keywords 30%**: Exact matching (error codes, function names, IDs)
- Progressive disclosure: Top-K returns summary (200 chars), expand for full content when needed

### Compact
- Call LLM to summarize historical memory into concise summary
- Delete/archive original files
- Can be manually triggered or scheduled auto-execution

## 4. Technology Stack

- **Vector database**: Milvus (Lite/Server/Zilliz Cloud, one-line config switch)
- **Embedding**: OpenAI, Google, Voyage, Ollama, local models
- **Search**: Vector + BM25 hybrid (same 70/30 ratio as OpenClaw)

## 5. Usage

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

## 6. Comparison with Other Solutions

| Dimension | memsearch | Mem0 / Zep | OpenClaw built-in |
|-----------|-----------|------------|-------------------|
| Memory storage | Markdown files | Vector database | Markdown files |
| Transparency | High (plaintext readable) | Low (JSON/vectors) | High |
| Editable | vim edit → auto-index | Requires API calls | Same as memsearch |
| Vector database | Milvus (optional) | Core dependency | sqlite-vec |
| Independence | ✅ Standalone library | ✅ Standalone library | ❌ Needs full OpenClaw |
| Hybrid search | ✅ Vector + BM25 | Partial support | ✅ Vector + BM25 |
| Human-AI co-creation | ✅ | ❌ | ✅ |

---

## 7. OpenClaw Memory System Vector Search Implementation Details

Although OpenClaw uses Markdown as source of truth, it built a complete vector retrieval layer on top:

### Indexing Pipeline
```
Markdown files
  ▼  Watch for changes (debounce 1.5s)
Chunking (~400 token/chunk, 80 token overlap)
  ▼
Embedding (multi-provider)
  ├── OpenAI text-embedding-3-small (remote, default)
  ├── Gemini embedding-001 (remote)
  ├── Voyage (remote)
  └── Local GGUF model (node-llama-cpp, offline available)
  ▼
SQLite storage (~/.openclaw/memory/<agentId>.sqlite)
  ├── vec0 virtual table (sqlite-vec accelerated vector search)
  ├── FTS5 full-text index (BM25 keyword search)
  └── Embedding cache (SHA dedup)
```

### Hybrid Search
```
finalScore = 0.7 × vectorScore + 0.3 × textScore(BM25)
```

- BM25 score conversion: `textScore = 1 / (1 + max(0, bm25Rank))`
- Candidate pool: Take `maxResults × candidateMultiplier` candidates from each side
- Union by chunk id, weighted sort

### Fallback Mechanism
- sqlite-vec unavailable → JS in-process cosine similarity
- FTS5 unavailable → pure vector search
- Remote embedding failed → local model fallback

### Additional Capabilities
- Experimental QMD backend (BM25 + vector + reranking, local sidecar)
- Session transcript indexing (optional, opt-in)
- Embedding batch indexing (OpenAI/Gemini Batch API, cheaper and faster)
- Additional path indexing (`memorySearch.extraPaths`)

---

## 8. BM25 Algorithm Brief

**BM25 (Best Matching 25)** is a classic keyword search algorithm that measures document-query relevance based on term frequency statistics.

Core formula considers:
- **Term Frequency (TF)**: How often keyword appears in document (saturates, not linear growth)
- **Inverse Document Frequency (IDF)**: Keyword rarity (rarer = more discriminative)
- **Document length normalization**: Long documents don't win just because they have more words

### Vector Search vs BM25 Complementary Relationship

| Scenario | Vector Search | BM25 |
|----------|--------------|------|
| Semantic matching (same meaning, different wording) | ✅ Strong | ❌ Weak |
| Exact matching (error codes, function names, IDs) | ❌ Weak | ✅ Strong |
| Synonyms/paraphrases | ✅ Strong | ❌ Weak |
| Long-tail keywords | ❌ Weak | ✅ Strong |

In SQLite, BM25 is implemented via **FTS5 extension** - create full-text index and use it, zero additional dependencies.

---

## 9. Implications for clawhive

### Can Directly Adopt
1. **Hybrid search 70/30 ratio**: Vector + BM25, validated by OpenClaw and memsearch
2. **Chunking strategy**: Split by heading/paragraph, ~400 token/chunk, 80 token overlap
3. **SHA-256 dedup**: Avoid duplicate embedding, reduce costs
4. **Chunk ID design**: Include model_version, changing model auto-invalidates old indexes
5. **Watch + auto-rebuild index**: File change → debounce → re-index

### clawhive's Differentiation Advantages
1. **Structured knowledge**: concepts table (type/confidence/status/evidence chain) — neither OpenClaw nor memsearch has this
2. **Auto-recording**: Every message auto-written to episodes, doesn't rely on LLM initiative
3. **Consolidation mechanism**: Consolidator auto-extracts concepts from episodes, with evidence chain association
4. **Zero external dependencies**: sqlite-vec embedded, no need for Milvus/external database

### Suggested Evolution Direction
1. **Add FTS5 + BM25**: Build full-text index in SQLite (zero additional dependencies)
2. **Implement sqlite-vec vector retrieval**: Replace current `LIKE '%query%'`
3. **Hybrid search**: Vector + BM25 weighted fusion
4. **Optional Markdown readable layer**: Export Markdown view from SQLite for human audit
5. **Maintain structural advantage**: concepts + confidence + evidence is differentiated competitiveness
