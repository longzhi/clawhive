# clawhive Memory System Design

> Date: 2026-02-13  
> Status: Design Confirmed (v2, adopting OpenClaw memory model)  
> Core Philosophy: Markdown files are memory, SQLite is only a search index

---

## 1. Design Philosophy

**Markdown is source of truth, SQLite is search engine.**

Adopts the memory model validated by OpenClaw: LLM directly reads and writes Markdown files like a human, without introducing additional structured database layers. SQLite + sqlite-vec + FTS5 purely serves as a search acceleration layer, doesn't hold authoritative data.

**Dependency Versions:**
- `rusqlite` = 0.32 (features: `bundled`, `vtab`)
- `sqlite-vec` = 0.1.6 (crates.io stable version, statically linked, zero runtime dependencies)

### 1.1 Three-Layer Memory Architecture

Drawing from neuroscience memory models, clawhive's memory system has three layers:

```
┌──────────────────────────────────────────────────┐
│                clawhive Memory System             │
│                                                   │
│  ① Session JSONL (Sensory Memory / Working Memory)│
│     sessions/<id>.jsonl                           │
│     ↓ Raw conversation flow, fully preserved     │
│                                                   │
│  ② Daily Files (Short-term Memory)               │
│     memory/YYYY-MM-DD.md                          │
│     ↓ LLM active recording + fallback summary    │
│                                                   │
│  ③ MEMORY.md (Long-term Memory)                  │
│     ↓ Hippocampus consolidation                  │
│                                                   │
│  🔍 SQLite Search Layer (Read-only Index)        │
│     sqlite-vec + FTS5 + chunks table             │
└──────────────────────────────────────────────────┘
```

**Hippocampus**: Periodically extracts knowledge from short-term memory (daily files), consolidates into long-term memory (MEMORY.md)—consistent with how the hippocampus in neuroscience consolidates short-term memory into long-term memory.

**Why we abandoned dual-track system:**
- Bidirectional sync added significant complexity (conflict resolution, watch, debounce), limited benefit
- Concepts/Links graph structure was over-engineered—LLM reading Markdown directly is more natural than querying structured databases
- OpenClaw's practice proves: Markdown + vector indexing is already good enough

---

## 2. Memory File Structure

### 2.1 Directory Layout

```
workspace/
├── MEMORY.md              ← Long-term memory (curated wisdom)
└── memory/
    ├── 2026-02-13.md      ← Daily record (raw log)
    ├── 2026-02-12.md
    └── ...
```

### 2.2 MEMORY.md (Long-term Memory)

**Nature:** Refined core knowledge, similar to human long-term memory.

**Content Example:**
```markdown
# MEMORY.md

## User Preferences
- Development language: Rust, prefers minimal design
- Documentation style: Chinese primary, technical terms in English
- Toolchain: neovim + wezterm + zellij

## Project Decisions
- Database: SQLite + sqlite-vec, zero external dependencies
- Architecture: Single-process monolith, Bus only for sidecar broadcast
- Memory system: Markdown as source of truth

## Important Facts
- clawhive repository: /Users/dragon/Workspace/clawhive/
- Obsidian vault syncs to GitHub
```

**Write Timing:**
1. Memory flush before compaction (when context nearly full, LLM writes important info)
2. Hippocampus scheduled task (extracts essence from recent daily files)
3. LLM actively writes during conversation (when discovering important information)

**Rules:**
- LLM directly reads/writes, no format constraints
- Humans can also manually edit
- Periodically clean outdated information

### 2.3 memory/YYYY-MM-DD.md (Daily Records)

**Nature:** Raw log for the day, doesn't need refinement.

**Content Example:**
```markdown
# 2026-02-13

## clawhive Architecture Review
- Completed message flow review, confirmed main path is Gateway→Orchestrator direct call
- Bus positioned as sidecar broadcast, not on main path
- Memory system changed from dual-track to OpenClaw mode (Markdown as source of truth)

## Design Decisions
- Abandoned episodes/concepts/links graph model
- Adopted Markdown + sqlite-vec/FTS5 search
```

**Write Timing:** Anytime. LLM records content worth preserving during conversation.

**Fallback Mechanism:** If LLM hasn't written anything when a session ends, trigger fallback summary—use LLM to generate a summary of the entire conversation, write to that day's daily file.

---

## 3. Search System

### 3.1 Index Building

When Markdown files change (full scan at startup + incremental watch at runtime), build SQLite search index.

> Schema below aligns with OpenClaw.

**Chunking Parameters (aligned with OpenClaw defaults):**
- chunk target size: ~400 tokens
- chunk overlap: ~80 tokens
- long paragraphs fall back to fixed window splitting

#### SQLite Schema

**Table 1: `meta` (Index Metadata)**

| Field | Type | Description |
|-------|------|-------------|
| key | TEXT (PK) | Metadata key |
| value | TEXT | Metadata value (e.g., embedding provider/model fingerprint, used to determine if reindex needed) |

**Table 2: `files` (Indexed File Snapshot)**

| Field | Type | Description |
|-------|------|-------------|
| path | TEXT (PK) | File path |
| source | TEXT | Source category (`memory` / `sessions`) |
| hash | TEXT | File content hash (for incremental update check) |
| mtime | INTEGER | File modification time |
| size | INTEGER | File size |

**Table 3: `chunks` (Chunks + Embedding)**

| Field | Type | Description |
|-------|------|-------------|
| id | TEXT (PK) | Chunk unique identifier |
| path | TEXT | Source file path |
| source | TEXT | Source category (`memory`) |
| start_line | INTEGER | Starting line number in file |
| end_line | INTEGER | Ending line number in file |
| hash | TEXT | Chunk content hash (for incremental check) |
| model | TEXT | Model identifier that generated embedding |
| text | TEXT | Chunk original text |
| embedding | TEXT | Vector (JSON serialized) |
| updated_at | INTEGER | Last index time |

**Table 4: `embedding_cache` (Cross-model Embedding Cache)**

| Field | Type | Description |
|-------|------|-------------|
| provider | TEXT | Embedding provider |
| model | TEXT | Embedding model |
| provider_key | TEXT | Provider identifier |
| hash | TEXT | Content hash |
| embedding | TEXT | Vector (JSON serialized) |
| dims | INTEGER | Vector dimensions |
| updated_at | INTEGER | Cache time |
| | | **PK: (provider, model, provider_key, hash)** |

**FTS5 Virtual Table: `chunks_fts`**

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

#### Incremental Update Strategy

1. Scan memory files at startup, compare hash/mtime with `files` table
2. Only re-chunk + re-embed files with changed hash
3. **Reindex Trigger**: When embedding provider/model fingerprint in `meta` table changes, rebuild entire index
4. **Runtime Watch**: Monitor `memory/` directory file changes (debounce), incremental update

### 3.2 Hybrid Search

```
User sends message / Orchestrator needs memory
  │
  ▼  Parallel two-path search
  │
  ├── Vector Search (sqlite-vec)
  │   query_embedding vs chunk embeddings
  │   → cosine similarity
  │
  └── Full-text Search (FTS5 BM25)
      query_text vs chunk content
      → BM25 rank score
  │
  ▼  Merge and Rerank
  │
  finalScore = vectorScore × 0.7 + bm25Score × 0.3
  │
  ▼  Return top-K chunks
```

**Fusion Details (aligned with OpenClaw defaults):**
- `vectorWeight`: 0.7 (cosine similarity, normalized to 0-1)
- `textWeight`: 0.3 (BM25 rank score, normalized)
- `candidateMultiplier`: 4 (candidatePool = maxResults × 4, fetch more then rank)
- `maxResults`: 6
- `minScore`: 0.35 (results below this score discarded)
- Sum of two weights normalized to 1.0 (configurable ratio adjustment)

> OpenClaw doesn't use recency weight, vector + BM25 is sufficient. Recency weighting can be a clawhive extension if needed.

### 3.3 LLM Memory Tools (Aligned with OpenClaw)

LLM can actively search and read memory through tool calling, not just relying on passive injection:

**`memory_search`**: Semantic search memory

```json
{
  "name": "memory_search",
  "parameters": {
    "query": "string — search keywords or semantic description",
    "maxResults": "number — max results (default 6)",
    "minScore": "number — minimum score threshold (default 0.35)"
  },
  "returns": [
    {
      "snippet": "matching chunk text (truncated ~700 chars)",
      "path": "source file path",
      "startLine": 10,
      "endLine": 25,
      "score": 0.82
    }
  ]
}
```

**`memory_get`**: Read specified segment of memory file

```json
{
  "name": "memory_get",
  "parameters": {
    "path": "string — file path (MEMORY.md or memory/*.md)",
    "from": "number — starting line number (optional)",
    "lines": "number — number of lines to read (optional)"
  },
  "returns": "string — file content segment"
}
```

**Usage Flow:**
1. LLM feels need to recall something → call `memory_search`
2. Found relevant chunks but needs more context → call `memory_get` to read full paragraph
3. Combine memory context to answer user

> Passive injection (auto-search and inject into prompt each conversation) is retained as baseline. LLM tools provide active search capability, complementing each other.

### 3.4 Prompt Injection (Passive)

Retrieved chunks automatically injected into LLM prompt's system message:

```
[Memory Context]

From MEMORY.md:
- Development language: Rust, prefers minimal design
- Database: SQLite + sqlite-vec, zero external dependencies

From memory/2026-02-13.md:
- Memory system changed from dual-track to OpenClaw mode
- Bus positioned as sidecar broadcast

From memory/2026-02-12.md:
- Discussed Milvus vs sqlite-vec selection, confirmed sqlite-vec
```

---

## 4. Hippocampus

> **Hippocampus** is the core process of clawhive's memory system—periodically consolidates short-term memory (daily files + session JSONL) into long-term memory (MEMORY.md), like how the brain's hippocampus consolidates daily experiences during sleep.

```
Scheduled Cron trigger (daily off-peak / configurable)
  │
  ├── 1. Read recent daily files (e.g., last 7 days)
  │      └── Optional: scan session JSONL to extract highlights
  │
  ├── 2. Read current MEMORY.md
  │
  ├── 3. Call LLM ("Hippocampus"):
  │      prompt includes old and new content, requirements:
  │      - Extract knowledge worth long-term retention
  │      - When conflicts found, new content takes precedence
  │      - Delete outdated information
  │      - Output updated MEMORY.md
  │
  ├── 4. Write MEMORY.md
  │
  └── 5. Rebuild index (incremental)
```

**Design Points:**
- LLM sees both old and new content simultaneously, naturally handles conflicts, no separate conflict detection needed
- Hippocampus is "organizing" not "extracting"—like how the brain reviews day's experiences during sleep
- Frequency configurable (default once daily)
- Hippocampus can also read session JSONL to supplement content missed by daily files (optional, vNext)

---

## 5. Auto-Compaction (Context Compression)

When session conversation token count approaches context window limit:

```
Conversation continues, tokens approaching limit
  │
  ▼  Memory Flush
  │  Orchestrator reminds LLM:
  │  "context about to compress, please write important info to MEMORY.md or daily file"
  │  LLM executes write (agentic turn, silent to user)
  │
  ▼  Compaction
  │  Call LLM to compress old conversation into summary
  │  Retain: summary + recent messages + memory recall results
  │  Discard: old complete messages
  │
  ▼  Continue conversation
```

---

## 6. Session JSONL (Sensory Memory / Working Memory)

> Reference OpenClaw Session design: one JSONL file per session, append-only, complete conversation flow record.

### 6.1 File Layout

```
workspace/sessions/
├── <session_id>.jsonl      ← one file per session
├── <session_id>.jsonl
└── ...
```

### 6.2 JSONL Line Types

One JSON object per line, `type` field distinguishes types:

```jsonl
{"type":"session","version":1,"id":"<uuid>","timestamp":"...","agent_id":"main"}
{"type":"message","id":"<id>","timestamp":"...","message":{"role":"user","content":"Hello"}}
{"type":"message","id":"<id>","timestamp":"...","message":{"role":"assistant","content":"Hello!"}}
{"type":"tool_call","id":"<id>","timestamp":"...","tool":"search","input":{...}}
{"type":"tool_result","id":"<id>","timestamp":"...","tool":"search","output":{...}}
{"type":"compaction","id":"<id>","timestamp":"...","summary":"...","dropped_before":"<msg_id>"}
{"type":"model_change","id":"<id>","timestamp":"...","model":"claude-sonnet-4-5"}
```

**Core Line Types:**

| type | Description |
|------|-------------|
| `session` | First line of file, records session metadata (version, agent_id, created_at) |
| `message` | User or agent message (role: user / assistant / system) |
| `tool_call` | Tool call request |
| `tool_result` | Tool call result |
| `compaction` | Context compression event (records summary + discard position) |
| `model_change` | Model switch |

### 6.3 Uses

1. **Conversation Recovery**: Load recent N message lines when session starts, recover context
2. **Audit Trail**: Complete original record, even if compaction compressed context, JSONL retains everything
3. **Fallback Summary Source**: If LLM didn't write memory when session ends, generate summary from JSONL
4. **Hippocampus Data Source**: Hippocampus reads JSONL to extract highlights (supplements daily files)

### 6.4 Relationship with Memory Files

```
Session JSONL (raw conversation flow, auto-written, not deleted)
      │
      ├──→ LLM actively writes to daily file (selective recording)
      │
      └──→ Fallback summary writes to daily file (when session ends)
              │
              └──→ Hippocampus consolidates into MEMORY.md
```

- **JSONL ≠ Memory**: JSONL is raw record, memory files are filtered/refined
- **JSONL Not Indexed**: Not indexed to SQLite search layer (avoid noise)
- **JSONL Configurable Retention**: Default 30 days, archive or delete after expiry

### 6.5 Append-only Writing

- Each message appended to JSONL in real-time, existing lines not modified
- Compaction doesn't delete JSONL content, only appends a `compaction` type record
- This guarantees complete audit chain

---

## 7. Embedding Strategy (Aligned with OpenClaw)

### 7.1 Provider Architecture

`EmbeddingProvider` trait, default `auto` mode, auto-selects based on availability:

| Provider | Default Model | Description |
|----------|---------------|-------------|
| `openai` | `text-embedding-3-small` | OpenAI API |
| `gemini` | `gemini-embedding-001` | Gemini API |
| `voyage` | `voyage-4-large` | Voyage AI |
| `local` | `embeddinggemma-300M` (GGUF) | Local inference, zero API dependency |
| `auto` | Auto-select | Has remote API key → use remote; none → fallback local |

### 7.2 Implementation Timeline

**MVP:**
- Implement `EmbeddingProvider` trait + `openai` provider (most universal)
- `auto` mode: use if OpenAI API key detected, otherwise error prompting configuration

**vNext First Step:**
- Add `gemini` + `voyage` provider
- Add `local` provider (see Rust solution selection below)
- `auto` complete fallback chain: remote → local

### 7.3 Local Embedding: Rust Solution Selection

> OpenClaw (Node.js) uses GGUF model + llama.cpp binding. Rust ecosystem is different, requires independent selection.

| Solution | Model Format | Advantages | Disadvantages |
|----------|-------------|------------|---------------|
| **`ort` (ONNX Runtime)** ⭐ | ONNX | Industrial-grade stable, cross-platform, rich model ecosystem | Needs ONNX Runtime C++ library |
| `candle` | safetensors | Pure Rust, zero C++ dependency | Newer, limited embedding model support |
| `llama-cpp-rs` | GGUF | Can reuse OpenClaw's GGUF models | Mainly for LLM, embedding support inferior to ort |

**Recommendation: `ort` + ONNX format**

Recommended models (by scenario):
- **General English**: `all-MiniLM-L6-v2` (384 dim, 22M params, fast)
- **Multilingual**: `bge-small-zh-v1.5` or `multilingual-e5-small` (Chinese-English mixed scenarios)
- **High Quality**: `bge-large-en-v1.5` (1024 dim, more accurate but slower)

**vNext Second Step:**
- Embedding cache (`embedding_cache` table, avoid duplicate API calls)
- Batch API support (reduce costs for large-scale indexing)

---

## 8. Implementation Timeline

### MVP

- [ ] MEMORY.md + memory/YYYY-MM-DD.md file read/write
- [ ] LLM active memory writing (tool/system prompt guidance)
- [ ] Fallback summary (session ends without writing → auto-summarize)
- [ ] SQLite index layer (chunks table + sqlite-vec 0.1.6 + FTS5)
- [ ] Hybrid search (vector 70% + BM25 30%)
- [ ] Search results passive injection into prompt
- [ ] `memory_search` + `memory_get` LLM tools (active search)
- [ ] Session history loading (conversation history)

### vNext First Step

- [ ] Auto-Compaction + Memory Flush
- [ ] Hippocampus scheduled task
- [ ] Semantic-aware chunking (heading split + long paragraph fallback)
- [ ] Index incremental update (watch file changes)

### vNext Second Step

- [ ] Local Embedding model
- [ ] Cross-agent memory isolation strategy
- [ ] Memory CLI (query/manage/debug)
- [ ] TUI memory panel
