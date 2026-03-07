# clawhive

[![CI](https://github.com/longzhi/clawhive/actions/workflows/ci.yml/badge.svg)](https://github.com/longzhi/clawhive/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Rust](https://img.shields.io/badge/rust-1.92%2B-orange.svg)](https://www.rust-lang.org/)
[![GitHub release](https://img.shields.io/github/v/release/longzhi/clawhive?include_prereleases)](https://github.com/longzhi/clawhive/releases)

[English](README.md) | 中文

用 Rust 从零构建的开源 AI Agent 平台——一个二进制文件，部署到 Telegram、Discord、Slack、WhatsApp、iMessage 等多个渠道。

## 概述

clawhive 是 [OpenClaw](https://github.com/openclaw/openclaw) 的 Rust 原生替代方案。OpenClaw 基于 Node.js、43 万行代码；clawhive 编译为**单个静态二进制文件，零运行时依赖**——不需要 Node.js，不需要 npm，不需要 Docker。下载、配置、运行，就这么简单。

**为什么选择 clawhive？**

- **极致轻量** — 单文件约 14MB，运行时内存占用约 48MB。树莓派、VPS、Mac Mini 都能跑。没有 GC 停顿，内存可预测。
- **安全先行** — 从第一天就内置两层安全模型：不可绕过的硬基线拦截 SSRF、危险命令和敏感文件访问；第三方 Skill 必须显式声明权限，没有隐式授权。
- **有限执行** — Agent 有强制的 Token 预算、超时限制和子 Agent 递归深度限制。不会有无限循环，不会有天价账单。
- **Web + CLI 配置** — 浏览器向导或交互式 CLI，2 分钟内跑起你的第一个 Agent。

## 🔐 安全优先

clawhive 实现了**两层安全架构**，为 AI Agent 的工具执行提供纵深防御：

### 硬基线（始终强制）

以下安全约束**不可协商**，适用于所有工具执行，无论信任级别：

| 防护类型 | 拦截内容 |
|---------|---------|
| **SSRF 防护** | 私有网络（10.x、172.16-31.x、192.168.x）、回环地址、云元数据端点（169.254.169.254） |
| **敏感路径保护** | 写入 `~/.ssh/`、`~/.gnupg/`、`~/.aws/`、`/etc/` 等系统目录 |
| **私钥防护** | 读取 `~/.ssh/id_*`、`~/.gnupg/private-keys`、云凭证 |
| **危险命令拦截** | `rm -rf /`、fork bomb、磁盘擦除、curl 管道到 shell |
| **资源限制** | 30 秒超时、1MB 输出上限、5 个并发执行 |

### 基于来源的信任模型

工具按来源分类，决定其权限要求：

| 来源 | 信任级别 | 权限检查 |
|------|---------|---------|
| **内置** | 受信任 | 仅硬基线（无需权限声明） |
| **外部** | 沙箱化 | 必须在 SKILL.md frontmatter 中声明所有权限 |

### Skill 权限声明

外部 Skill 必须在 SKILL.md 中显式声明所需权限：

```yaml
---
name: weather-skill
description: Get weather forecasts
permissions:
  network:
    allow:
      - "api.openweathermap.org:443"
      - "api.weatherapi.com:443"
  fs:
    read:
      - "${WORKSPACE}/**"
    write:
      - "${WORKSPACE}/cache/**"
  exec:
    - curl
    - jq
  env:
    - WEATHER_API_KEY
---
```

**未声明的权限在运行时一律拒绝。**

### 安全理念

1. **默认拒绝** — 外部 Skill 没有声明就没有权限
2. **硬基线不可绕过** — 即使权限配置错误也无法覆盖
3. **诚实文档** — 只声明已实现的能力，不画饼
4. **纵深防御** — 多层防护防止单点失效

## 技术差异（对比 OpenClaw）

| 维度 | clawhive | OpenClaw |
|------|----------|----------|
| **运行时** | 纯 Rust 二进制 + 嵌入式 SQLite | Node.js |
| **安全模型** | 两层策略（硬基线 + 来源信任） | 工具白名单 |
| **权限系统** | 声明式 SKILL.md 权限 | 运行时策略 |
| **记忆** | Markdown 原生（MEMORY.md 为准） | Markdown 原生（MEMORY.md + memory/*.md） |
| **集成渠道** | 多渠道（Telegram、Discord、Slack、WhatsApp、iMessage、CLI） | 广泛连接器 |
| **依赖** | 单文件，零运行时依赖 | Node.js + npm |

### 关键架构选择

- **Rust workspace + 嵌入式 SQLite**（`rusqlite` + bundled）：生产环境零运行时依赖
- **Markdown 优先的记忆**：`MEMORY.md` 和每日文件为权威数据源；SQLite 索引可重建
- **权限即代码**：Skill 在 YAML frontmatter 中声明权限，运行时强制执行
- **有限执行**：Token 桶限流、子 Agent 递归限制、超时控制

## 功能特性

- 多 Agent 编排：每个 Agent 有独立的人设、模型路由和记忆策略
- 三层记忆系统：会话 JSONL（工作记忆）→ 每日文件（短期记忆）→ MEMORY.md（长期记忆）
- 混合搜索：sqlite-vec 向量相似度（70%）+ FTS5 BM25（30%）
- 海马体整合：LLM 定期将每日观察提炼为长期记忆
- 渠道适配：Telegram、Discord、Slack、WhatsApp、iMessage（多 Bot、多连接器）
- ReAct 推理循环 + 防空转保护
- 子 Agent 生成（深度限制 + 超时）
- Skill 系统（SKILL.md frontmatter + 提示注入）
- 按用户 Token 桶限流
- LLM 提供商抽象 + 重试 + 指数退避（Anthropic、OpenAI、Gemini、DeepSeek、Groq、Ollama、OpenRouter、Together、Fireworks，以及任何 OpenAI 兼容端点）
- 实时 TUI 仪表板（会话、事件、Agent 状态）
- YAML 驱动配置（Agent、提供商、路由）

## 架构

![clawhive 架构图](assets/architecture.png)

## 项目结构

```
crates/
├── clawhive-cli/        # CLI 入口（clap）— start、setup、chat、validate、agent/skill/session/schedule
├── clawhive-core/       # 编排器、会话管理、配置、人设、Skill 系统、子 Agent、LLM 路由
├── clawhive-memory/     # 记忆系统 — 文件存储（MEMORY.md + 每日文件）、会话 JSONL、SQLite 索引、分块、嵌入
├── clawhive-gateway/    # 网关：Agent 路由 + 按用户限流
├── clawhive-bus/        # 基于主题的进程内事件总线（发布/订阅）
├── clawhive-provider/   # LLM 提供商 trait + 多提供商适配器（流式、重试）
├── clawhive-channels/   # 渠道适配器（Telegram、Discord、Slack、WhatsApp、iMessage）
├── clawhive-auth/       # OAuth 和 API Key 认证
├── clawhive-scheduler/  # 基于 Cron 的任务调度
├── clawhive-server/     # HTTP API 服务器
├── clawhive-schema/     # 共享 DTO（InboundMessage、OutboundMessage、BusMessage、SessionKey）
├── clawhive-runtime/    # 任务执行器抽象
└── clawhive-tui/        # 实时终端仪表板（ratatui）

~/.clawhive/             # 安装 + 配置后创建
├── bin/                 # 二进制文件
├── skills/              # Skill 定义（SKILL.md + frontmatter）
├── config/              # 由 `clawhive setup` 生成
│   ├── main.yaml        # 应用设置、渠道配置
│   ├── agents.d/*.yaml  # 每个 Agent 的配置（模型策略、工具、记忆、身份）
│   ├── providers.d/*.yaml # LLM 提供商设置
│   └── routing.yaml     # 渠道 → Agent 路由绑定
├── workspaces/          # 每个 Agent 的工作空间（记忆、会话、提示）
├── data/                # SQLite 数据库
└── logs/                # 日志文件
```

## 安装

```bash
curl -fsSL https://raw.githubusercontent.com/longzhi/clawhive/main/install.sh | bash
```

自动检测操作系统和架构，下载最新版本，将二进制文件和 Skill 安装到 `~/.clawhive/`。

安装后重启终端或执行：

```bash
source ~/.zshrc  # 或 ~/.bashrc
```

也可以从 [GitHub Releases](https://github.com/longzhi/clawhive/releases) 手动下载。

### 配置

安装后，通过以下任一方式配置提供商、Agent 和渠道：

**方式 A：Web 配置向导** — 启动服务后在浏览器中打开向导：

```bash
clawhive start
# 在浏览器中打开 http://localhost:3000/setup
```

**方式 B：CLI 配置向导** — 运行交互式终端向导：

```bash
clawhive setup
```

### 运行

```bash
# 配置
clawhive setup
clawhive validate

# 聊天模式（本地 REPL）
clawhive chat

# 服务生命周期
clawhive start
clawhive start --daemon  # 别名：-d
clawhive restart
clawhive restart --daemon  # 别名：-d
clawhive stop

# 仪表板模式（可观测性 TUI）
clawhive dashboard
clawhive dashboard --port 3000

# 编码模式（开发者 TUI）
clawhive code
clawhive code --port 3000

# Agent / 会话
clawhive agent list
clawhive agent show clawhive-main
clawhive session reset <session_key>

# 定时任务
clawhive schedule list
clawhive schedule run <schedule_id>
clawhive task trigger clawhive-main "总结今天的工作"

# 认证
clawhive auth status
clawhive auth login openai
```

## 快速开始（开发者）

前置条件：Rust 1.92+

```bash
# 克隆并构建
git clone https://github.com/longzhi/clawhive.git
cd clawhive
cargo build --workspace

# 交互式配置（配置提供商、Agent、渠道）
cargo run -- setup

# 聊天模式（本地 REPL）
cargo run -- chat

# 启动所有已配置的渠道 Bot
cargo run -- start

# 以守护进程方式启动
cargo run -- start --daemon  # 别名：-d

# 重启 / 停止
cargo run -- restart
cargo run -- restart --daemon
cargo run -- stop

# 仪表板模式（可观测性 TUI）
cargo run -- dashboard
cargo run -- dashboard --port 3000

# 编码 Agent 模式（将本地 TUI 渠道接入运行中的网关）
cargo run -- code
cargo run -- code --port 3000
```

## 开发者工作流

推送前使用本地质量检查：

```bash
# 一次性：安装仓库管理的 git hooks
just install-hooks

# 运行所有 CI 等效检查
just check

# 发布流程：check -> push main -> 替换 tag 并推送
just release v0.1.0-alpha.15
```

如果不用 `just`，可以直接执行脚本：

```bash
bash scripts/install-git-hooks.sh
bash scripts/check.sh
bash scripts/release.sh v0.1.0-alpha.15
```

`just check` 会执行：

1. `cargo fmt --all -- --check`
2. `cargo clippy --workspace --all-targets -- -D warnings`
3. `cargo test --workspace`

## 配置说明

配置通过 `clawhive setup` 管理，交互式生成 YAML 文件到 `~/.clawhive/config/`：

- `main.yaml` — 应用名称、运行时设置、功能开关、渠道配置
- `agents.d/<agent_id>.yaml` — Agent 身份（名称、emoji）、模型策略（主模型 + 备用）、工具策略、记忆策略
- `providers.d/<provider>.yaml` — 提供商类型、API 地址、认证方式（API Key 或 OAuth）
- `routing.yaml` — 默认 Agent ID、渠道到 Agent 的路由绑定

支持的提供商：Anthropic、OpenAI、Gemini、DeepSeek、Groq、Ollama、OpenRouter、Together、Fireworks，以及任何 OpenAI 兼容端点。

## 记忆系统

clawhive 使用受神经科学启发的三层记忆架构：

1. **会话 JSONL**（`sessions/<id>.jsonl`）— 追加式对话日志，类型化条目（message、tool_call、tool_result、compaction、model_change）。用于会话恢复和审计追踪。
2. **每日文件**（`memory/YYYY-MM-DD.md`）— LLM 在对话中写入的每日观察。如果会话中 LLM 没有写入，会生成备用摘要。
3. **MEMORY.md** — 策展的长期知识。通过海马体整合（LLM 对近期每日文件的综合提炼）和 LLM 直接写入来更新。
4. **SQLite 搜索索引** — sqlite-vec 0.1.6 + FTS5。Markdown 文件分块（约 400 token，80 token 重叠），嵌入、索引。混合搜索：向量相似度 × 0.7 + BM25 × 0.3。

注意：JSONL 文件不参与索引（噪音太大）。只有 Markdown 记忆文件参与搜索。

## CLI 命令

| 命令 | 说明 |
|------|------|
| `setup` | 交互式配置向导 |
| `start [--tui] [--daemon]` | 启动所有已配置的渠道 Bot 和 HTTP API 服务器 |
| `stop` | 停止运行中的 clawhive 进程 |
| `restart` | 重启 clawhive（stop + start） |
| `chat [--agent <id>]` | 本地 REPL 测试 |
| `validate` | 验证 YAML 配置 |
| `consolidate` | 手动运行记忆整合 |
| `agent list\|show\|enable\|disable` | Agent 管理 |
| `skill list\|show\|analyze\|install` | Skill 管理 |
| `session reset <key>` | 重置会话 |
| `schedule list\|run\|enable\|disable\|history` | 定时任务管理 |
| `wait list` | 列出后台等待任务 |
| `task trigger <agent> <task>` | 向 Agent 发送一次性任务 |
| `auth login\|status` | OAuth 认证管理 |

## 开发

```bash
# 运行所有测试
cargo test --workspace

# 代码检查
cargo clippy --workspace --all-targets -- -D warnings

# 格式化
cargo fmt --all
```

## 技术栈

| 组件 | 技术 |
|------|------|
| 语言 | Rust（2021 edition） |
| LLM 提供商 | Anthropic、OpenAI、Gemini、DeepSeek、Groq、Ollama、OpenRouter、Together、Fireworks |
| 渠道 | Telegram、Discord、Slack、WhatsApp、iMessage、CLI |
| 数据库 | SQLite（rusqlite，bundled） |
| 向量搜索 | sqlite-vec |
| 全文搜索 | FTS5 |
| HTTP | reqwest |
| 异步 | tokio |
| TUI | ratatui + crossterm |
| CLI | clap 4 |

## 许可证

MIT

## 状态

本项目正在活跃开发中。记忆架构使用 Markdown 原生存储 + 混合检索。
