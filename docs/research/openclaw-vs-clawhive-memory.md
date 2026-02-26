# OpenClaw vs clawhive Memory System Comparison

> Source: 2026-02-13 code review discussion

---

## Design Philosophy

| | OpenClaw | clawhive |
|---|---|---|
| **Core concept** | Markdown is memory, files are source of truth | Database is memory, SQLite is source of truth |
| **Storage medium** | Filesystem (.md) + derived index | SQLite tables (episodes + concepts + links) |
| **Who writes** | LLM writes files itself (agent actively calls read/write) | System auto-writes (Orchestrator auto-records every message) |
| **Auditability** | Humans can directly open Markdown to view, edit, git track | Requires SQL queries or specialized tools |
| **Metaphor** | Diary + note-taking system | Brain (hippocampus + cortex) |

## Memory Layers

### OpenClaw — Two-layer Files

```
~/.openclaw/workspace/
├── MEMORY.md              ← Long-term memory (human curated, only loaded in main session)
└── memory/
    ├── 2026-02-12.md      ← Daily log (append-only)
    └── 2026-02-13.md
```

- Write strategy: LLM autonomously decides what to write
- No structured schema, all free text
- MEMORY.md only loaded in main session (security isolation, doesn't leak to group chats)

### clawhive — Three-table Structure

```
SQLite DB
├── episodes    ← Hippocampus (every message auto-recorded)
├── concepts    ← Cortex (structured knowledge, has confidence/status)
└── links       ← Evidence chain (episode ↔ concept association)
```

- episodes fully auto-recorded
- concepts extracted periodically from episodes by Consolidator using LLM
- links record support/refute/update relationships

## Retrieval Capabilities

| | OpenClaw | clawhive |
|---|---|---|
| Vector search | ✅ Multi-provider embedding + sqlite-vec | ❌ Only `LIKE '%query%'` |
| BM25 keywords | ✅ FTS5 | ❌ None |
| Hybrid search | ✅ Vector 70% + BM25 30% | ❌ |
| Time query | ✅ | ✅ Filter by days |
| Entity query | ⚠️ Research phase | ✅ concepts table Entity type |
| Confidence query | ⚠️ Research phase | ✅ concepts has confidence |

## Consolidation/Integration Mechanism

### OpenClaw
- **Compaction**: When context window nearly full, summarize old conversation into summary
- **Memory Flush**: Before compaction, silently remind LLM to write persistent memory
- **Heartbeat maintenance**: Agent periodically self-organizes MEMORY.md
- Executor: LLM itself

### clawhive
- **Consolidator (Scheduled Cron)**:
  - Read high-value episodes from last 24h (importance ≥ 0.6)
  - Call LLM to extract concepts (JSON format)
  - Upsert concepts + establish links
  - Mark >30 day unverified concepts as Stale
  - Clean up >90 day low-importance episodes
- Executor: System code, LLM only responsible for extraction

## Context Window Management

| | OpenClaw | clawhive |
|---|---|---|
| Session history | ✅ JSONL persistence + auto-compaction | ❌ No session history |
| Compaction | ✅ Summarize old messages into summary | ❌ |
| Session Pruning | ✅ Trim old tool results | ❌ |

## Respective Advantages

### OpenClaw
1. Human readable and editable — Markdown extremely transparent
2. Mature retrieval — Hybrid search + sqlite-vec + multi embedding providers
3. Compaction — Elegantly handles long conversations
4. Memory Flush — Don't lose important info before compaction
5. Security isolation — MEMORY.md restricted to main session
6. Flexibility — LLM autonomously organizes memory structure

### clawhive
1. Structured knowledge — concepts have type/confidence/status/evidence chain
2. Auto-recording — Doesn't rely on LLM "remembering" to write
3. Confidence evolution — confidence + Stale/Conflicted
4. Evidence chain — Traceable knowledge sources
5. Automated consolidation — Scheduled extraction, doesn't rely on LLM initiative

## Recommendations for clawhive to Adopt

1. **Markdown readable layer**: Export Markdown view on top of SQLite
2. **Vector retrieval + FTS5**: Implement sqlite-vec + BM25 hybrid search
3. **Compaction**: Adopt auto-compaction + memory flush
4. **Maintain differentiation**: Structured concepts + confidence + evidence chain are unique advantages
