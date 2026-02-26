# Clawhive Web Admin - UI Design Specification

> **Status**: Design Complete  
> **Created**: 2026-02-14  
> **Version**: 1.0

---

## 1. Design Philosophy

**关键词**: Professional, Clean, Functional  
**参考风格**: Vercel Dashboard / Linear / Stripe Dashboard  
**避免**: AI 化视觉（渐变、发光、科技感字体）、过度复杂的布局

### Core Principles
- **内容优先**: 信息密度适中，不浪费空间，不堆砌装饰
- **一致性**: 所有页面使用统一的组件和交互模式
- **可操作性**: 每个页面都有明确的主要操作（CTA）
- **响应式**: 移动端完全可用，不是简单的缩放

---

## 2. Tech Stack

```
Frontend:
  Framework:    Next.js 15 (App Router) + React 19 + TypeScript
  UI Library:   shadcn/ui (Radix primitives + Tailwind)
  Styling:      Tailwind CSS 4
  State:        TanStack Query (server state) + Zustand (client state)
  Forms:        React Hook Form + Zod validation
  Charts:       Recharts
  Icons:        Lucide React
  Realtime:     EventSource (SSE)

Backend:
  Framework:    axum 0.8 + tower-http
  Serialization: serde + serde_json
  Realtime:     axum SSE (axum::response::sse)
  Auth:         API Key header (X-API-Key), local-only mode optional
  CORS:         tower-http CorsLayer
```

---

## 3. Overall Layout

### 3.1 Desktop Layout (≥1024px)

```
┌──────────────────────────────────────────────────────────┐
│ ┌──────────┐ ┌──────────────────────────────────────────┐│
│ │           │ │  Top Bar                        [🔔] [👤]││
│ │  Sidebar  │ ├──────────────────────────────────────────┤│
│ │           │ │                                          ││
│ │  🐝 Logo  │ │                                          ││
│ │           │ │            Content Area                  ││
│ │  Dashboard│ │                                          ││
│ │  Agents   │ │         (module-specific content)        ││
│ │  Sessions │ │                                          ││
│ │  Channels │ │                                          ││
│ │  Providers│ │                                          ││
│ │  Routing  │ │                                          ││
│ │           │ │                                          ││
│ │           │ ├──────────────────────────────────────────┤│
│ │  ──────── │ │  Status: Connected  |  v0.3.0           ││
│ │  Settings │ └──────────────────────────────────────────┘│
│ └──────────┘                                              │
└──────────────────────────────────────────────────────────┘
```

- **Sidebar**: 固定 220px，深色背景（zinc-900），白色文字
- **Top Bar**: 高度 56px，显示当前页面标题 + 面包屑
- **Content Area**: 白色/浅灰背景，内边距 24px
- **Status Bar**: 固定底部，显示连接状态和版本号

### 3.2 Mobile Layout (<768px)

```
┌──────────────────────────┐
│ [☰]  Clawhive    [🔔][👤]│  ← Top Bar (sticky)
├──────────────────────────┤
│                          │
│    Content Area          │
│    (full width)          │
│    (single column)       │
│                          │
│                          │
├──────────────────────────┤
│ 📊  🤖  💬  📡  ⚙️      │  ← Bottom Tab Bar
└──────────────────────────┘

[☰] tap → slide-in drawer:
┌────────────────┐
│ 🐝 Clawhive    │
│                │
│ Dashboard      │
│ Agents         │
│ Sessions       │
│ Channels       │
│ Providers      │
│ Routing        │
│                │
│ Settings       │
└────────────────┘
```

- **Top Bar**: Sticky，汉堡菜单 + Logo + 操作按钮
- **Bottom Tab Bar**: 5 个主要入口快速切换（Dashboard/Agents/Sessions/Channels/Settings）
- **Drawer**: 完整导航，左滑弹出

### 3.3 Tablet Layout (768px - 1023px)

- Sidebar 折叠为图标模式（60px 宽，只显示图标）
- 悬停展开完整 sidebar
- Content area 自适应宽度

---

## 4. Design System

### 4.1 Color Palette

```
Background:
  page:       #FAFAFA (zinc-50)      -- 主背景
  card:       #FFFFFF                 -- 卡片背景
  sidebar:    #18181B (zinc-900)      -- 侧边栏
  topbar:     #FFFFFF                 -- 顶栏
  
Text:
  primary:    #09090B (zinc-950)      -- 正文
  secondary:  #71717A (zinc-500)      -- 次要文字
  muted:      #A1A1AA (zinc-400)      -- 占位/禁用

Accent:
  brand:      #F97316 (orange-500)    -- 品牌色（🐝 hive gold）
  brand-soft: #FFF7ED (orange-50)     -- 品牌浅底

Status:
  success:    #22C55E (green-500)     -- 在线/启用/完成
  error:      #EF4444 (red-500)       -- 错误/失败
  warning:    #F59E0B (amber-500)     -- 警告
  info:       #3B82F6 (blue-500)      -- 信息

Border:
  default:    #E4E4E7 (zinc-200)
  focus:      #F97316 (orange-500)

Dark Mode (future):
  page:       #09090B
  card:       #18181B
  text:       #FAFAFA
```

### 4.2 Typography

```
Font Family:
  sans:       Inter, system-ui, -apple-system, sans-serif
  mono:       JetBrains Mono, Menlo, Consolas, monospace

Sizes:
  xs:         12px / 1rem    -- badges, captions
  sm:         14px / 1.25    -- table cells, secondary text
  base:       16px / 1.5     -- body text
  lg:         18px / 1.75    -- section headers
  xl:         20px / 1.75    -- page titles
  2xl:        24px / 2       -- major headings

Weight:
  normal:     400            -- body
  medium:     500            -- labels, table headers
  semibold:   600            -- headings, buttons
  bold:       700            -- page title, emphasis
```

### 4.3 Spacing & Radius

```
Spacing (rem-based):
  xs:   4px     -- tight padding (badges)
  sm:   8px     -- compact spacing
  md:   16px    -- default padding
  lg:   24px    -- section spacing
  xl:   32px    -- page padding
  2xl:  48px    -- major sections

Border Radius:
  sm:   4px     -- buttons, badges
  md:   8px     -- cards, inputs
  lg:   12px    -- modals, large cards
  full: 9999px  -- avatars, pills
```

### 4.4 Core Components

| Component | Usage | Style |
|-----------|-------|-------|
| **Card** | Container for content groups | White bg, 1px border zinc-200, radius-md, shadow-sm |
| **Table** | Data lists (agents, sessions) | Striped rows, sticky header, hover highlight |
| **Badge** | Status indicators | Pill shape, colored bg + text (green/red/amber/blue) |
| **Button Primary** | Main actions | Orange-500 bg, white text, radius-sm |
| **Button Secondary** | Secondary actions | White bg, zinc-200 border, zinc-700 text |
| **Button Ghost** | Tertiary/icon | Transparent, zinc-500 text, hover: zinc-100 bg |
| **Input** | Form fields | White bg, zinc-200 border, radius-md, focus: orange ring |
| **Select** | Dropdowns | Same as Input, with chevron indicator |
| **Toggle** | On/off switches | Green when on, zinc-200 when off |
| **Tabs** | View switching | Underline style, orange active indicator |
| **Toast** | Notifications | Bottom-right, auto-dismiss, colored left border |

---

## 5. Module Designs

### 5.1 Dashboard

**Purpose**: Real-time system overview, equivalent to TUI's 4-panel view

```
Desktop:
┌──────────────────────────────────────────────────────┐
│  Dashboard                                           │
├──────────┬──────────┬──────────┬─────────────────────┤
│ Active   │ Sessions │ Messages │ Errors              │
│ Agents   │ Today    │ /hr      │ Today               │
│   2/3    │   47     │   156    │   3                  │
│  ↑12%    │  ↑8%     │  ↑23%    │  ↓50%               │
├──────────┴──────────┴──────────┴─────────────────────┤
│                                                      │
│  ┌─ Event Stream ──────────────────────────────────┐ │
│  │ 08:42:15  MessageAccepted  trace=a3f2..  ✓      │ │
│  │ 08:42:14  HandleIncoming   user:123 → main      │ │
│  │ 08:42:10  MemoryWrite      session=s:1  [0.8]   │ │
│  │ 08:42:08  ReplyReady       trace=b7e1..  2.3s   │ │
│  │ 08:42:05  StreamDelta      trace=b7e1..         │ │
│  │ ...                                    [Pause]  │ │
│  └─────────────────────────────────────────────────┘ │
│                                                      │
│  ┌─ Agent Status ───────┐ ┌─ Recent Sessions ─────┐ │
│  │ 🐝 clawhive-main  🟢 │ │ user:123  08:42  main │ │
│  │ 🛠️ clawhive-builder 🟢│ │ user:456  08:35  main │ │
│  │                      │ │ user:789  08:20  bldr  │ │
│  └──────────────────────┘ └────────────────────────┘ │
└──────────────────────────────────────────────────────┘

Mobile:
┌──────────────────────────┐
│ Dashboard                │
├──────────┬───────────────┤
│ Agents   │ Sessions      │
│  2/3  🟢 │  47 today     │
├──────────┼───────────────┤
│ Messages │ Errors        │
│  156/hr  │  3 today  ⚠️  │
├──────────┴───────────────┤
│ Event Stream             │
│ 08:42 MessageAccepted ✓  │
│ 08:42 HandleIncoming     │
│ 08:41 MemoryWrite        │
│ ...              [More]  │
└──────────────────────────┘
```

**Components**:
- **Metric Cards** (4x): Number + trend arrow + percentage
- **Event Stream**: SSE-powered real-time log, auto-scroll, filterable
  - Each event: timestamp + type badge + key info
  - Pause/resume button
  - Filter by event type (dropdown)
- **Agent Status**: Small list showing name + emoji + status dot
- **Recent Sessions**: Compact list, click to jump to Session detail

**SSE Integration**:
```
GET /api/events/stream
Content-Type: text/event-stream

data: {"type":"MessageAccepted","trace_id":"a3f2...","ts":"08:42:15"}
data: {"type":"ReplyReady","trace_id":"b7e1...","duration_ms":2300}
```

---

### 5.2 Agent Management

**Purpose**: View and configure all agents

```
Desktop - List View:
┌──────────────────────────────────────────────────────┐
│  Agents                                  [+ New Agent]│
├──────────────────────────────────────────────────────┤
│  ┌────────────────────────────────────────────────┐  │
│  │ Search agents...                    [All ▼]    │  │
│  └────────────────────────────────────────────────┘  │
│                                                      │
│  ┌──────────────────────────────────────────────────┐│
│  │ Agent          │ Model      │ Tools  │ Status    ││
│  ├────────────────┼────────────┼────────┼───────────┤│
│  │ 🐝 clawhive-main│ sonnet    │ 4      │ 🟢 Active ││
│  │ 🛠️ ncb-builder │ sonnet    │ 4      │ 🟢 Active ││
│  └──────────────────────────────────────────────────┘│
└──────────────────────────────────────────────────────┘

Desktop - Detail View (click agent row):
┌──────────────────────────────────────────────────────┐
│  ← Agents / clawhive-main              [Save] [Disable]│
├──────────────────────────────────────────────────────┤
│                                                      │
│  [Identity] [Model] [Tools] [Memory]    ← Tab nav    │
│                                                      │
│  Identity Tab:                                       │
│  ┌──────────────────────────────────────────────────┐│
│  │ Agent ID        clawhive-main                    ││
│  │ Display Name    [clawhive          ]             ││
│  │ Emoji           [🐝                ]             ││
│  │ Status          🟢 Enabled    [Toggle]           ││
│  └──────────────────────────────────────────────────┘│
│                                                      │
│  Model Tab:                                          │
│  ┌──────────────────────────────────────────────────┐│
│  │ Primary Model   [sonnet           ▼]            ││
│  │ Fallbacks       [haiku] [+ Add]                  ││
│  └──────────────────────────────────────────────────┘│
│                                                      │
│  Tools Tab:                                          │
│  ┌──────────────────────────────────────────────────┐│
│  │ ☑ read    ☑ write    ☑ edit    ☑ exec           ││
│  │ ☐ search  ☐ spawn                               ││
│  └──────────────────────────────────────────────────┘│
│                                                      │
│  Memory Tab:                                         │
│  ┌──────────────────────────────────────────────────┐│
│  │ Mode            [hippocampus_cortex ▼]           ││
│  │ Write Scope     [conservative       ▼]           ││
│  └──────────────────────────────────────────────────┘│
└──────────────────────────────────────────────────────┘

Mobile - List:
┌──────────────────────────┐
│ Agents           [+ New] │
├──────────────────────────┤
│ [Search...]              │
├──────────────────────────┤
│ ┌──────────────────────┐ │
│ │ 🐝 clawhive-main     │ │
│ │ sonnet | 4 tools  🟢 │ │
│ └──────────────────────┘ │
│ ┌──────────────────────┐ │
│ │ 🛠️ clawhive-builder  │ │
│ │ sonnet | 4 tools  🟢 │ │
│ └──────────────────────┘ │
└──────────────────────────┘
```

**Components**:
- **Agent Table/Cards**: Desktop = table, Mobile = cards
- **Detail Tabs**: Identity / Model / Tools / Memory 
- **Toggle Switch**: Enable/disable agent
- **Multi-select Chips**: Fallback models, tool allowlist
- **Save Button**: Only appears when changes are made (dirty state)

---

### 5.3 Session Explorer

**Purpose**: Browse and inspect conversation sessions

```
Desktop:
┌──────────────────────────────────────────────────────┐
│  Sessions                                            │
├──────────────────────────────────────────────────────┤
│  [Search sessions...]  [All Agents ▼]  [Today ▼]    │
│                                                      │
│  ┌─ Session List ──────────┬─ Session Detail ───────┐│
│  │                         │                        ││
│  │ ● user:123 → main      │  Session: tg:main:123  ││
│  │   08:42 • 12 messages   │  Agent: clawhive-main  ││
│  │                         │  Started: 08:30:15     ││
│  │   user:456 → main      │  Messages: 12          ││
│  │   08:35 • 8 messages    │                        ││
│  │                         │  ┌─────────────────┐   ││
│  │   user:789 → builder   │  │ 👤 Hello!        │   ││
│  │   08:20 • 3 messages    │  │                 │   ││
│  │                         │  │ 🐝 Hi there!    │   ││
│  │                         │  │ How can I help? │   ││
│  │                         │  │                 │   ││
│  │                         │  │ 👤 What tools   │   ││
│  │                         │  │ do you have?    │   ││
│  │                         │  │                 │   ││
│  │                         │  │ 🐝 I have read, │   ││
│  │                         │  │ write, edit...  │   ││
│  │                         │  └─────────────────┘   ││
│  │                         │                        ││
│  │                         │  [Reset] [Delete]      ││
│  └─────────────────────────┴────────────────────────┘│
└──────────────────────────────────────────────────────┘

Mobile:
┌──────────────────────────┐
│ Sessions                 │
├──────────────────────────┤
│ [Search...]              │
│ [Agent ▼] [Date ▼]      │
├──────────────────────────┤
│ ┌──────────────────────┐ │
│ │ user:123 → main      │ │
│ │ 08:42 • 12 msgs   →  │ │
│ └──────────────────────┘ │
│ ┌──────────────────────┐ │
│ │ user:456 → main      │ │
│ │ 08:35 • 8 msgs    →  │ │
│ └──────────────────────┘ │
│                          │
│ (tap → full screen detail│
│  with chat view)         │
└──────────────────────────┘
```

**Components**:
- **Master-Detail Layout**: Desktop = side-by-side, Mobile = drill-down
- **Session List**: Filterable by agent, date range, user
- **Chat Viewer**: Renders JSONL as conversation bubbles
  - User messages: right-aligned, blue bg
  - Agent messages: left-aligned, gray bg
  - System messages: centered, muted text
- **Actions**: Reset session, Delete session

---

### 5.4 Channel Configuration

**Purpose**: Configure Telegram/Discord connectors

```
Desktop:
┌──────────────────────────────────────────────────────┐
│  Channels                                [+ Add Channel]│
├──────────────────────────────────────────────────────┤
│                                                      │
│  ┌─ Telegram ──────────────────────────────────────┐ │
│  │                                        [Enabled]│ │
│  │  Connectors:                                    │ │
│  │  ┌────────────────────────────────────────────┐ │ │
│  │  │ Connector ID    tg_main                    │ │ │
│  │  │ Bot Token       ${TELEGRAM_BOT_TOKEN}      │ │ │
│  │  │ Status          🟢 Token configured        │ │ │
│  │  │                               [Edit] [🗑️]  │ │ │
│  │  └────────────────────────────────────────────┘ │ │
│  │  [+ Add Connector]                              │ │
│  └─────────────────────────────────────────────────┘ │
│                                                      │
│  ┌─ Discord ───────────────────────────────────────┐ │
│  │                                       [Disabled]│ │
│  │  Connectors:                                    │ │
│  │  ┌────────────────────────────────────────────┐ │ │
│  │  │ Connector ID    dc_main                    │ │ │
│  │  │ Bot Token       ${DISCORD_BOT_TOKEN}       │ │ │
│  │  │ Status          ⚠️ Token not set            │ │ │
│  │  │                               [Edit] [🗑️]  │ │ │
│  │  └────────────────────────────────────────────┘ │ │
│  └─────────────────────────────────────────────────┘ │
│                                                      │
│                                      [Save Changes]  │
└──────────────────────────────────────────────────────┘
```

**Components**:
- **Channel Cards**: One card per channel type (Telegram, Discord)
- **Enable/Disable Toggle**: Per channel
- **Connector List**: Nested within each channel
- **Token Display**: Masked by default, env var reference shown
- **Status Indicator**: Green (configured), Amber (missing token), Red (error)
- **Save**: Only enabled when changes exist

---

### 5.5 LLM Provider Configuration

**Purpose**: Configure AI model providers

```
Desktop:
┌──────────────────────────────────────────────────────┐
│  Providers                              [+ Add Provider]│
├──────────────────────────────────────────────────────┤
│                                                      │
│  ┌─────────────────────────┐ ┌─────────────────────┐ │
│  │ Anthropic         🟢    │ │ OpenAI         🟢    │ │
│  │                         │ │                     │ │
│  │ API Base:               │ │ API Base:           │ │
│  │ api.anthropic.com       │ │ api.openai.com/v1   │ │
│  │                         │ │                     │ │
│  │ API Key:                │ │ API Key:            │ │
│  │ ANTHROPIC_API_KEY  ✓    │ │ OPENAI_API_KEY  ✓   │ │
│  │                         │ │                     │ │
│  │ Models:                 │ │ Models:             │ │
│  │  • claude-sonnet-4-5    │ │  • gpt-4o           │ │
│  │  • claude-opus-4-6      │ │  • gpt-4o-mini      │ │
│  │                         │ │                     │ │
│  │        [Edit] [Test]    │ │        [Edit] [Test]│ │
│  └─────────────────────────┘ └─────────────────────┘ │
│                                                      │
└──────────────────────────────────────────────────────┘

Edit Modal:
┌──────────────────────────────────┐
│  Edit Provider: Anthropic    [×] │
├──────────────────────────────────┤
│                                  │
│  Provider ID   [anthropic     ]  │
│  Enabled       [Toggle: ON    ]  │
│  API Base      [https://api...]  │
│  API Key Env   [ANTHROPIC_A...]  │
│                                  │
│  Models:                         │
│  [claude-sonnet-4-5        ] [×] │
│  [claude-opus-4-6          ] [×] │
│  [+ Add model                  ] │
│                                  │
│          [Cancel]  [Save]        │
└──────────────────────────────────┘
```

**Components**:
- **Provider Cards**: Grid layout, one per provider
- **API Key Status**: Check mark if env var is set, warning if not
- **Model List**: Tags/chips showing available models
- **Test Button**: Fires a lightweight API call to verify connectivity
- **Edit Modal**: Form dialog for editing provider details

---

### 5.6 Routing Configuration

**Purpose**: Map channels to agents

```
Desktop:
┌──────────────────────────────────────────────────────┐
│  Routing                                 [+ Add Rule] │
├──────────────────────────────────────────────────────┤
│                                                      │
│  Default Agent: [clawhive-main ▼]                    │
│                                                      │
│  ┌──────────────────────────────────────────────────┐│
│  │ #  │ Channel  │ Connector│ Match        │ Agent  ││
│  ├────┼──────────┼──────────┼──────────────┼────────┤│
│  │ 1  │ telegram │ tg_main  │ kind: dm     │ main   ││
│  │ 2  │ telegram │ tg_main  │ mention:     │ builder││
│  │    │          │          │  @builder    │        ││
│  │ 3  │ discord  │ dc_main  │ kind: dm     │ main   ││
│  │ 4  │ discord  │ dc_main  │ kind: mention│ main   ││
│  └──────────────────────────────────────────────────┘│
│                                                      │
│  Visual:                                             │
│  ┌──────────┐     ┌──────────┐     ┌──────────┐     │
│  │ Telegram │────→│  Router  │────→│ 🐝 main  │     │
│  │ tg_main  │     │          │     │ 🛠️ builder│     │
│  ├──────────┤     │  dm→main │     └──────────┘     │
│  │ Discord  │────→│  @→bldr  │                       │
│  │ dc_main  │     └──────────┘                       │
│  └──────────┘                                        │
│                                        [Save Changes]│
└──────────────────────────────────────────────────────┘

Mobile:
┌──────────────────────────┐
│ Routing      [+ Add Rule]│
├──────────────────────────┤
│ Default: clawhive-main ▼ │
├──────────────────────────┤
│ ┌──────────────────────┐ │
│ │ telegram/tg_main     │ │
│ │ dm → clawhive-main   │ │
│ │               [Edit] │ │
│ └──────────────────────┘ │
│ ┌──────────────────────┐ │
│ │ telegram/tg_main     │ │
│ │ @builder → builder   │ │
│ │               [Edit] │ │
│ └──────────────────────┘ │
│ ...                      │
└──────────────────────────┘
```

**Components**:
- **Default Agent Select**: Dropdown at top
- **Rules Table**: Desktop = table, Mobile = cards
- **Visual Diagram**: Simple flow visualization (optional, desktop only)
  - Channel → Router → Agent mapping
  - Color-coded connections
- **Edit Rule Modal**: Channel type, connector, match criteria, target agent
- **Drag-to-reorder**: Rules are priority-ordered

---

## 6. Navigation Structure

### Sidebar Items (Desktop)

```
🐝 Clawhive                    ← Logo + app name
─────────────────
📊 Dashboard                   ← Main monitoring view
🤖 Agents                      ← Agent management
💬 Sessions                    ← Session explorer
─────────────────
📡 Channels                    ← Channel configuration  
🧠 Providers                   ← LLM provider config
🔀 Routing                     ← Message routing rules
─────────────────
⚙️ Settings                    ← General settings (future)
```

### Bottom Tab Bar (Mobile)

```
📊       🤖       💬       📡       ⚙️
Dash    Agents   Sessions  Config   More

(Config = Channels + Providers + Routing combined)
(More = Settings)
```

---

## 7. Interaction Patterns

### 7.1 Forms
- **Inline Edit**: Toggle switches, simple fields update immediately
- **Modal Edit**: Complex forms (provider edit, routing rule) use dialog
- **Save Confirmation**: Toast notification "Changes saved" (green, bottom-right)
- **Unsaved Warning**: Orange banner if navigating away with unsaved changes

### 7.2 Real-time Updates
- **Event Stream**: SSE auto-scroll with pause button
- **Reconnection**: Auto-reconnect with backoff, show "Reconnecting..." banner
- **Stale Data**: TanStack Query auto-refetch on window focus

### 7.3 Error Handling
- **API Errors**: Toast notification with error message (red)
- **Network Errors**: Top banner "Connection lost. Retrying..."
- **Validation Errors**: Inline field errors below inputs

### 7.4 Loading States
- **Initial Load**: Skeleton screens (shimmer effect)
- **Actions**: Button disabled + spinner during API calls
- **Event Stream**: Pulsing dot indicator when connected

### 7.5 Empty States
- **No Agents**: Illustration + "No agents configured. Create your first agent."
- **No Sessions**: "No sessions yet. Start a conversation via Telegram or CLI."
- **No Events**: "Waiting for events... Make sure the bot is running."

---

## 8. API Endpoints (Backend)

```
# Dashboard
GET    /api/metrics                    → { agents_active, sessions_today, messages_per_hour, errors_today }
GET    /api/events/stream              → SSE stream of BusMessage events

# Agents
GET    /api/agents                     → [{ agent_id, enabled, identity, model_policy }]
GET    /api/agents/:id                 → { full agent config }
PUT    /api/agents/:id                 → Update agent config (writes YAML)
POST   /api/agents/:id/toggle          → Toggle enabled/disabled

# Sessions
GET    /api/sessions?agent=&from=&to=  → [{ session_key, agent, message_count, last_active }]
GET    /api/sessions/:key/messages     → [{ role, text, timestamp }]
POST   /api/sessions/:key/reset        → Reset session
DELETE /api/sessions/:key              → Delete session data

# Channels
GET    /api/channels                   → { telegram: { enabled, connectors }, discord: { ... } }
PUT    /api/channels                   → Update channel config (writes main.yaml)

# Providers
GET    /api/providers                  → [{ provider_id, enabled, api_base, models, key_status }]
PUT    /api/providers/:id              → Update provider config (writes YAML)
POST   /api/providers/:id/test         → Test API connectivity → { ok: bool, latency_ms }

# Routing
GET    /api/routing                    → { default_agent_id, bindings: [...] }
PUT    /api/routing                    → Update routing rules (writes YAML)
```

---

## 9. Responsive Breakpoints

| Breakpoint | Width | Layout Changes |
|------------|-------|---------------|
| **Mobile** | <640px | Single column, bottom tabs, drawer nav |
| **Mobile+** | 640-767px | Wider cards, still single column |
| **Tablet** | 768-1023px | Icon sidebar (60px), 2-column grids |
| **Desktop** | 1024-1279px | Full sidebar (220px), standard layout |
| **Wide** | ≥1280px | Full sidebar, wider content, 3-4 column grids |

---

## 10. File Structure (Frontend)

```
web/
├── package.json
├── next.config.ts
├── tailwind.config.ts
├── tsconfig.json
├── components.json              ← shadcn config
├── public/
│   └── favicon.ico
├── src/
│   ├── app/
│   │   ├── layout.tsx           ← Root layout (sidebar + topbar)
│   │   ├── page.tsx             ← Dashboard
│   │   ├── agents/
│   │   │   ├── page.tsx         ← Agent list
│   │   │   └── [id]/page.tsx    ← Agent detail
│   │   ├── sessions/
│   │   │   ├── page.tsx         ← Session list
│   │   │   └── [key]/page.tsx   ← Session detail
│   │   ├── channels/page.tsx    ← Channel config
│   │   ├── providers/page.tsx   ← Provider config
│   │   └── routing/page.tsx     ← Routing config
│   ├── components/
│   │   ├── ui/                  ← shadcn components
│   │   ├── layout/
│   │   │   ├── sidebar.tsx
│   │   │   ├── topbar.tsx
│   │   │   ├── mobile-nav.tsx
│   │   │   └── status-bar.tsx
│   │   ├── dashboard/
│   │   │   ├── metric-card.tsx
│   │   │   ├── event-stream.tsx
│   │   │   └── agent-status.tsx
│   │   ├── agents/
│   │   │   ├── agent-table.tsx
│   │   │   ├── agent-card.tsx
│   │   │   └── agent-form.tsx
│   │   ├── sessions/
│   │   │   ├── session-list.tsx
│   │   │   └── chat-viewer.tsx
│   │   ├── channels/
│   │   │   └── channel-card.tsx
│   │   ├── providers/
│   │   │   ├── provider-card.tsx
│   │   │   └── provider-form.tsx
│   │   └── routing/
│   │       ├── routing-table.tsx
│   │       └── rule-form.tsx
│   ├── lib/
│   │   ├── api.ts               ← API client (fetch wrapper)
│   │   ├── sse.ts               ← SSE client hook
│   │   └── utils.ts
│   ├── hooks/
│   │   ├── use-agents.ts        ← TanStack Query hooks
│   │   ├── use-sessions.ts
│   │   ├── use-channels.ts
│   │   ├── use-providers.ts
│   │   ├── use-routing.ts
│   │   └── use-event-stream.ts
│   └── types/
│       └── index.ts             ← TypeScript types matching Rust schema
└── components.json
```

---

## 11. Implementation Priority

| Phase | Deliverable | Effort |
|-------|------------|--------|
| **Phase 1** | Backend: clawhive-server crate with axum + CORS + SSE | High |
| **Phase 2** | Frontend scaffold: Next.js + shadcn + layout (sidebar/topbar) | Medium |
| **Phase 3** | Dashboard module (metrics + event stream) | Medium |
| **Phase 4** | Agent management (list + detail + edit) | Medium |
| **Phase 5** | Channels + Providers + Routing config | Medium |
| **Phase 6** | Session explorer (list + chat viewer) | Medium |
| **Phase 7** | Polish: loading states, error handling, mobile optimization | Low |
