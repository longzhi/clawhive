# NanoCrab Web Admin Interface - Design Document

> **Status**: Research Phase (NOT implementing yet)  
> **Created**: 2026-02-14  
> **Author**: dragon

---

## Overview

Design a professional, responsive web admin interface for NanoCrab, supporting both mobile and PC.

## Manageable Entities

| Entity | Config Location | Current CLI Ops | Web Admin Ops |
|--------|----------------|-----------------|---------------|
| **Agent** | `config/agents.d/*.yaml` + `prompts/` | list, show, enable, disable | CRUD, persona editing, model/tool policy, enable/disable |
| **Session** | `sessions/*.jsonl` + SQLite | reset | List, view history, reset, delete, search |
| **Memory** | `memory/*.md` + `MEMORY.md` + SQLite index | consolidate | Browse, search, edit MEMORY.md, view daily files, re-index |
| **Skill** | `skills/*.md` | list, show | List, view, edit |
| **Provider** | `config/providers.d/*.yaml` | N/A | View status, test connectivity, model list |
| **Routing** | `config/routing.yaml` | N/A | View/edit channel-to-agent bindings |
| **System** | EventBus | TUI dashboard | Real-time monitoring, metrics, logs |

## Architecture Decision

### Current State
- **No HTTP API exists** - clawhive is CLI/TUI/Telegram only
- Need to build `clawhive-server` (axum) as API layer
- Configuration is YAML-file-driven, memory is Markdown-file-driven

### Recommended Backend: Axum
- Modern Rust HTTP framework, Tokio-native
- Tower middleware ecosystem
- Type-safe extractors
- Built-in SSE support for real-time streaming

### Recommended Frontend: Next.js + shadcn/ui + Tailwind CSS
- shadcn/ui: Copy-paste component model, full control, professional look
- Tailwind CSS: Mobile-first responsive design
- TanStack Query: Server state management
- Recharts: Dashboard charts
- Zustand: Client state management

### Real-time Strategy: SSE primary + WebSocket for chat
- SSE for: Dashboard metrics, agent logs, event stream
- WebSocket for: Interactive chat with agents (if added)

## Proposed Feature Modules

### 1. Dashboard (Home)
- System status overview cards (agents active, sessions today, memory usage)
- Real-time event stream (EventBus subscriber via SSE)
- Recent sessions timeline
- Error/warning alerts

### 2. Agents Management
- Agent list with status badges (enabled/disabled)
- Agent detail view: identity, model policy, tool policy, memory policy
- Persona editor (system.md, style.md, safety.md) with Markdown preview
- Enable/disable toggle
- Model priority configuration (primary + fallbacks)
- Tool allowlist management

### 3. Sessions Explorer
- Session list with filters (by agent, date, user, channel)
- Chat-style session viewer (render JSONL as conversation)
- Session search (full-text)
- Reset/delete operations
- Export as JSON/CSV

### 4. Memory Browser
- MEMORY.md viewer/editor with Markdown rendering
- Daily files browser (calendar view + file list)
- Hybrid search (vector + FTS5) via existing index
- Manual consolidation trigger
- Re-index button

### 5. Skills Manager
- Skill list with frontmatter metadata
- Skill detail view with Markdown rendering
- Edit skill content (live preview)

### 6. Providers & Routing
- Provider list with status (configured/missing API key)
- Model availability per provider
- Routing rules visualization (channel -> agent mapping)
- Edit routing bindings

### 7. Settings
- `main.yaml` configuration editor
- Channel management (Telegram/Discord connectors)
- Runtime settings (max_concurrent, feature flags)
- Embedding configuration

### 8. Chat (Optional / Future)
- Web-based chat with any agent
- WebSocket streaming responses
- Agent switcher
- Markdown rendered responses

## UI/UX Design Principles

### Layout
- **Desktop**: Fixed sidebar (240px) + content area
- **Mobile**: Offcanvas drawer sidebar, single column content
- Three-layer vertical: Top bar + Content + (optional status bar)

### Visual Style
- Dark mode by default (with toggle)
- Clean, information-dense but not cluttered (reference: Langfuse)
- Color coding: Success(green), Error(red), Warning(amber), Info(blue)
- Monospace fonts for code/config/logs

### Responsive Strategy
- Mobile-first with Tailwind breakpoints
- Cards: 1 col (mobile) -> 2 col (tablet) -> 3-4 col (desktop)
- Tables: Card view on mobile, full table on desktop
- Navigation: Drawer on mobile, fixed sidebar on desktop

## Technical Stack Summary

```
Backend:  axum + tower-http + tokio + serde
Frontend: Next.js 15 + React 19 + TypeScript
UI:       shadcn/ui + Radix UI + Tailwind CSS 4
Charts:   Recharts
State:    TanStack Query + Zustand
Icons:    Lucide React
Forms:    React Hook Form + Zod validation
Realtime: EventSource (SSE) + tokio-tungstenite (WebSocket)
```

## API Design Sketch

```
GET    /api/agents              - List all agents
GET    /api/agents/:id          - Get agent detail
PUT    /api/agents/:id          - Update agent config
POST   /api/agents/:id/toggle   - Enable/disable agent

GET    /api/sessions             - List sessions (paginated)
GET    /api/sessions/:id         - Get session messages
DELETE /api/sessions/:id         - Delete session
POST   /api/sessions/:id/reset   - Reset session

GET    /api/memory/long-term     - Read MEMORY.md
PUT    /api/memory/long-term     - Update MEMORY.md
GET    /api/memory/daily         - List daily files
GET    /api/memory/daily/:date   - Read specific daily file
GET    /api/memory/search?q=     - Hybrid search
POST   /api/memory/consolidate   - Trigger consolidation
POST   /api/memory/reindex       - Rebuild SQLite index

GET    /api/skills               - List skills
GET    /api/skills/:name         - Get skill detail

GET    /api/providers            - List providers
GET    /api/routing              - Get routing config
PUT    /api/routing              - Update routing config

GET    /api/events/stream        - SSE event stream (real-time)
GET    /api/metrics              - System metrics snapshot
```

## Reference Projects
- [Langfuse](https://github.com/langfuse/langfuse) - Next.js + Prisma, best AI admin UX reference
- [shadcn-admin](https://github.com/satnaing/shadcn-admin) - Ready-made admin template
- [n8n](https://github.com/n8n-io/n8n) - Workflow editor UI patterns
- [OpenAI Codex TUI](https://github.com/openai/codex) - Rust streaming patterns

## Prerequisites (before implementation)
1. Extract `bootstrap()` to `clawhive-core` (shared with devtui plan)
2. Add `axum`, `tower-http`, `tokio-tungstenite` to workspace dependencies
3. Create `crates/clawhive-server/` crate
4. Design API authentication strategy (API key? JWT? Local-only?)
