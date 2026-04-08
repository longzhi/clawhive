# Protea 自演化系统 — 完整架构与机制研究

## 项目概览

**Protea** 是一个自进化的人工生命系统，核心理念是：**程序是一个活的有机体 — 它可以自我重构、自我繁殖、自我演化**。

- **代码库**：https://github.com/Drlucaslu/protea
- **语言**：Python 3.11+
- **架构**：三环设计（Three-Ring Architecture）
- **作者**：Drlucaslu (Liang Lu / Lucas)
- **状态**：正在积极开发（280+ commits），支持多种 LLM 提供商

---

## 核心架构：三环设计

### **Ring 0 — Sentinel（哨兵/物理层）**

**职责**：不可变的物理监督层，纯 Python stdlib，零外部依赖

关键组件：
- **sentinel.py** — 主监督循环
- **heartbeat.py** — Ring 2 心跳监控（每 2 秒检查一次）
- **git_manager.py** — Git 快照 + 自动回滚
- **fitness.py** — 适应度评分 + 高原期检测
- **memory.py** — 三层内存系统（热/温/冷）+ 向量搜索
- **skill_store.py** — 结晶化的技能存储
- **output_queue.py** — 演化输出队列（用户反馈循环）
- **user_profile.py** — 用户兴趣建模 + 衰减
- **preference_store.py** — 结构化偏好存储
- **evolution_intent.py** — 意图分类 + 影响范围控制
- **resource_monitor.py** — CPU/内存/磁盘监控
- **sqlite_store.py** — SQLite 持久化基础

**运行机制**：
1. Ring 0 启动 Ring 2 作为子进程
2. Ring 2 每 2 秒写一次 `.heartbeat` 文件
3. Ring 0 检查心跳新鲜度（默认 600 秒超时）
4. **如果存活**：评分适应度 → 结晶化技能 → 演化代码 → 下一代
5. **如果失败**：回滚到最后良好提交 → 从回滚基础演化 → 重启

---

### **Ring 1 — Intelligence（智能/进化引擎）**

**职责**：LLM 驱动的演化引擎、任务执行、机器人、技能结晶、仪表板

关键组件：

#### 演化系统
- **reflector.py** — 反射引擎（Reflexion 架构 + MemEvolve 模式）
  - 事件驱动的自省系统
  - 根据任务指标生成改进提案
  - 多维度评分：证据强度、影响范围、风险等级、可逆性
- **prompts.py** — 内存策展提示模板
- **crystallizer.py** — 从存活代码中提取技能
- **auto_crystallizer.py** — 自动结晶化触发器

#### LLM 集成
- **llm_base.py** — LLM 客户端抽象 + 工厂 + 线程安全超时（90 秒硬超时）
- **llm_client.py** — Anthropic Claude 客户端
- **llm_openai.py** — OpenAI / DeepSeek / Qwen 客户端
- **config.py** — 多提供商 LLM 配置

支持的 LLM：
| 提供商 | 默认模型 | 配置 |
|--------|---------|------|
| Anthropic | claude-sonnet-4-5 | CLAUDE_API_KEY 环境变量 |
| OpenAI | gpt-4o | [ring1.llm] 配置 |
| DeepSeek | deepseek-chat | [ring1.llm] 配置 |
| Qwen | qwen3.5-plus | DASHSCOPE_API_KEY |

#### 任务与代理
- **task_executor.py** — P0 用户任务 + P1 自主任务
- **task_generator.py** — 自主任务生成
- **subagent.py** — 后台任务子代理
- **preference_extractor.py** — 隐式偏好信号提取
- **habit_detector.py** — 用户习惯模式检测
- **proactive_loop.py** — 主动建议引擎

#### 用户交互
- **telegram_bot.py** — Telegram 机器人（命令 + 自由文本）
- **telegram.py** — Telegram 通知器（单向）
- **matrix_bot.py** — Matrix 机器人（Client-Server API）
- **dashboard.py** — Web 仪表板（http://localhost:8899）
- **skill_portal.py** — 技能 Web UI
- **skill_runner.py** — 技能进程管理器
- **skill_sandbox.py** — 技能虚拟环境 + 依赖管理

#### 内存与持久化
- **memory_curator.py** — LLM 辅助内存策展
- **embeddings.py** — 嵌入提供商（OpenAI / 本地哈希）

#### 工具框架
- **tool_registry.py** — 工具调度框架
- **tools/** 目录：
  - **filesystem.py** — read_file, write_file, edit_file, list_dir
  - **shell.py** — exec（沙箱化 shell）
  - **web.py** — web_search, web_fetch
  - **message.py** — 向用户发送进度消息
  - **skill.py** — run_skill, view_skill, edit_skill
  - **spawn.py** — 后台任务生成
  - **schedule.py** — 计划任务管理
  - **send_file.py** — 文件发送到聊天
  - **report.py** — 报告生成
  - **progress_monitor.py** — 任务进度监控

---

### **Ring 2 — Evolvable Code（活的代码）**

**职责**：自演化的活代码，由 Ring 0 管理

- **ring2/main.py** — 唯一的活代码文件（自动演化，**永不手动提交**）

---

## 自演化机制详解

### 1. **适应度评分系统**（0.0 - 1.0）

六个加权分量：

| 分量 | 权重 | 描述 |
|------|------|------|
| 基础存活 | 0.50 | 运行时长是否达到 max_runtime_sec |
| 输出体量 | 0.10 | 有意义的非空行（上限 50 行） |
| 输出多样性 | 0.10 | 唯一行数 / 总行数 |
| 新颖性 | 0.05 | Jaccard 距离（vs 最近世代输出） |
| 结构化输出 | 0.10 | JSON、表格、键值对检测 |
| 功能加分 | 0.05 | 真实 I/O、HTTP、文件操作 |
| 错误惩罚 | -0.10 | 回溯/异常行数 |

**实现细节**（ring0/fitness.py）：
- 错误检测用正则表达式精确匹配真实 Python 异常，避免误报
- 新颖性通过令牌级指纹比较计算（Jaccard 相似度）
- 功能检测通过模式匹配识别 HTTP、文件操作等

### 2. **演化意图分类**（优先级排序）

每一次演化根据当前状态分类为四种意图之一：

1. **adapt** — 用户指令待处理（**最高优先级**）
   - 当有用户直接指令 `/direct <text>` 时触发

2. **repair** — 代码崩溃或有持久错误
   - 当 Ring 2 进程死亡或输出中检测到持久错误时触发

3. **explore** — 适应度分数高原期（无进展）
   - 当连续 N 代分数无改善时触发

4. **optimize** — 代码存活且工作，做增量改进
   - 正常稳定运行时的默认意图

**高原期检测**：
- 跟踪最近 N 代的分数
- 如果没有新高分，标记为高原期
- 触发 LLM 探索新方向

**令牌节省机制**：
- 当分数高原且无用户指令时，**跳过 LLM 演化调用**，节省成本

### 3. **增量演化 vs 完全重写**

**影响范围控制**（Blast Radius Gate）：
- 完全重写定义为：>70% 行数变化
- 当代码运行良好且意图为 `optimize` 时，**拒绝**完全重写
- 允许更大改动的情况：
  - 代码崩溃（`repair`）
  - 分数停滞（`explore`）
  - 用户指令待处理（`adapt`）

这确保了**渐进式演化**而非激进的重写，提高稳定性。

### 4. **闭合循环进化反馈**

演化存活后，系统通过 AST diff 检测新能力，并通过 Telegram 按钮反馈：

| 按钮 | 效果 |
|------|------|
| 👍 不错 | 提升基因适应度；标记为"已接受"— 后续演化保留此方向 |
| 📌 定期执行 | 从能力创建计划任务；提升基因适应度 |
| 👎 不要了 | 删除基因；标记为"已拒绝" — 后续演化避免此方向 |
| *(沉默)* | 24 小时后自动过期；温和衰减 |

**约束注入**：
- 已接受和已拒绝的能力被注入到 LLM 进化提示中
- 防止 LLM 重新演化已解决的问题
- 防止追求不需要的方向
- 速率限制：每天最多 5 条推送

### 5. **基因池（Gene Pool）**

演化代码模式通过本地**基因池**跨代保存。

**基因提取流程**：
1. Ring 2 存活后，其源代码通过 AST 分析
2. 提取紧凑的基因摘要（200-500 令牌）
3. 按适应度评分存储前 100 个基因到 SQLite

**基因注入机制**：
- 演化时，最佳 2-3 个基因摘要作为**继承模式**注入 LLM 提示
- 任务执行时，按**语义相关性阈值**（min_semantic=1.0）过滤基因，避免注入无关模式
- 这使系统能够**记住和重用成功的代码模式**

---

## 内存系统

### 三层内存架构

| 层级 | 保留期 | 描述 |
|-----|--------|------|
| **热** | 最近 10 代 | 活跃记忆，完整保真度 |
| **温** | 10-30 代 | 按类型压缩，每组保留前 3 条 |
| **冷** | 30-100 代 | LLM 策展（保留/摘要/丢弃） |
| **遗忘** | >100 代 | 若重要性 < 0.3 则删除 |

### 可选语义搜索

- 当配置嵌入提供商时，记忆用 256 维向量存储
- 检索使用混合评分：40% 关键字 + 60% 余弦相似度

### 记忆策展 AI

LLM 驱动的记忆策展决策：
- **keep** — 唯一见解、用户偏好、重要教训、重复模式
- **summarize** — 有价值但冗长 → 1-2 句话
- **discard** — 冗余、过时、琐碎、被较新记忆取代
- **extract_rule** — 2+ 条相关条目形成模式 → 提取为可复用规则
- **conflict** — 两条条目包含矛盾信息 → 标记冲突

---

## 反射系统（Reflexion Architecture）

**ring1/reflector.py** 实现了 NeurIPS 2023 的 Reflexion 架构：

**核心循环**：
1. **任务执行** → 生成器阶段
2. **反思** → Reflector 分析任务指标
3. **记忆存储** → 证据存储到插曲记忆
4. **未来任务增强** → 反思提案用于改进下一轮

**反思提案三个维度**：

```json
{
  "category": "memory_cleanup | config_tune | task_pattern",
  "description": "What to change and why",
  "confidence": 0.0-1.0,
  "dimensions": {
    "evidence_strength": 0.35,
    "impact_scope": 0.25,
    "risk_level": 0.25,
    "reversibility": 0.15
  },
  "action": { /* 类别特定执行规范 */ },
  "evidence": [ /* 具体观察 */ ]
}
```

**空闲反思**：
- 系统空闲时触发深度审查
- 检查内存健康、模式分析、令牌趋势、配置调优

**连续控制机制**：
- 跟踪连续空回复、连续拒绝、连续超时
- 动态调整冷却倍数（最高 4 倍）

---

## 关键配置（config/config.toml）

### Ring 0 配置
```toml
[ring0]
ring2_path = "ring2"
db_path = "data/fitness.db"
heartbeat_interval_sec = 2
heartbeat_timeout_sec = 600
max_cpu_percent = 80
max_memory_percent = 80
max_disk_percent = 90

[ring0.reflection]
idle_threshold_sec = 7200
auto_confidence = 0.8
cooldown_sec = 1800
min_tasks_before_reflect = 3
```

### Ring 1 配置
```toml
[ring1.llm]
provider = "anthropic"  # or "openai", "deepseek", "qwen"
api_key_env = "CLAUDE_API_KEY"
model = "claude-sonnet-4-5"
max_tokens = 8192

[ring1.dashboard]
enabled = true
host = "127.0.0.1"
port = 8899

[ring1.embeddings]
provider = "openai"
model = "text-embedding-3-small"
dimensions = 256
```

---

## Telegram 命令

系统通过 Telegram 提供完整的用户交互接口：

| 命令 | 描述 |
|------|------|
| `/status` | 状态面板（代数、运行时间、执行器健康） |
| `/history` | 最近 10 代 |
| `/top` | 前 5 高适应度 |
| `/code` | 查看当前 Ring 2 源代码 |
| `/pause` / `/resume` | 暂停/恢复演化 |
| `/kill` | 重启 Ring 2 |
| `/direct <text>` | 设置演化指令 |
| `/tasks` | 任务队列 + 最近历史 |
| `/memory` | 最近记忆 |
| `/forget` | 清空所有记忆 |
| `/skills` | 列出结晶化技能 |
| `/skill <name>` | 查看技能详情 |
| `/run <name>` | 启动技能进程 |
| `/stop` | 停止运行技能 |
| `/running` | 运行中的技能状态 |
| `/background` | 后台子代理任务 |
| `/files` | 列出上传文件 |
| `/find <prefix>` | 按名称搜索文件 |
| *自由文本* | 提交为 P0 任务 |

---

## Web 仪表板（http://localhost:8899）

| 页面 | 内容 |
|------|------|
| **概览** | 统计卡 + SVG 适应度图表 |
| **内存** | 可浏览表格（按层级/类型过滤） |
| **技能** | 卡网格（使用计数、标签） |
| **模板** | 已发布和可发现的任务模板 |
| **意图** | 演化意图垂直时间线 |
| **配置** | 类别柱状图 + 交互统计 |

所有页面都有 JSON API 对应接口（`/api/memory`, `/api/skills` 等）

---

## 任务模板共享（protea-hub）

验证的计划任务（run_count >= 2）自动共享为**任务模板**（通过 protea-hub）。

每个模板是参数化的"意图+触发器+执行"三元组（自然语言）。其他 Protea 实例可根据用户配置文件发现和安装相关模板。

---

## 项目结构快照

```
protea/
├── ring0/                  # 哨兵/物理层
│   ├── sentinel.py         # 主监督循环
│   ├── fitness.py          # 适应度评分
│   ├── heartbeat.py        # 心跳监控
│   ├── git_manager.py      # Git 快照/回滚
│   ├── memory.py           # 三层内存 + 向量搜索
│   ├── skill_store.py      # 技能存储
│   ├── gene_pool.py        # 基因池（已演化）
│   ├── task_store.py       # 任务持久化
│   ├── user_profile.py     # 用户建模
│   └── ...
│
├── ring1/                  # 智能/演化引擎
│   ├── reflector.py        # Reflexion 反射系统
│   ├── prompts.py          # 提示模板
│   ├── llm_base.py         # LLM 抽象
│   ├── task_executor.py    # 任务执行
│   ├── telegram_bot.py     # Telegram 机器人
│   ├── dashboard.py        # Web 仪表板
│   ├── skill_*.py          # 技能管理
│   ├── tools/              # 工具实现
│   │   ├── filesystem.py
│   │   ├── shell.py
│   │   ├── web.py
│   │   └── ...
│   └── ...
│
├── ring2/                  # 活的代码
│   └── main.py             # 唯一的活文件（自动演化）
│
├── config/
│   ├── config.toml         # 主配置
│   └── soul.md             # 用户档案
│
└── tests/                  # 1900+ 测试
```

---

## 与 Clawhive 的对比与启示

### Protea 的独特设计要点

1. **三环分离** — 业务逻辑（Ring 2）完全与监督/演化分离（Ring 0/1）
2. **不可变基础** — Ring 0 使用纯 stdlib，无外部依赖
3. **闭合反馈循环** — Telegram 用户反馈直接影响基因池和演化方向
4. **多层防护** — 影响范围控制（Blast Radius Gate）防止激进重写
5. **记忆策展** — LLM 主动管理内存生命周期，而非堆积
6. **基因继承** — AST 提取的代码模式跨代保留和复用
7. **反射系统** — NeurIPS 2023 论文实现，提供自省能力

### 对 Clawhive 自演化功能的建议

1. **意图分类** — 采用类似的四阶段意图框架（adapt → repair → explore → optimize）
2. **适应度量化** — 不仅跟踪成功/失败，而是多维度打分（生存、多样性、新颖性等）
3. **基因提取** — 在 Rust 中用 AST 分析提取存活代码模式
4. **用户反馈闭合** — 通过技能中心（Skills）的反馈机制指导演化
5. **冷却机制** — 避免过度频繁的演化调用，节省 LLM 成本
6. **记忆三层** — 实施热/温/冷记忆分层，而非线性堆积

---

## 关键文件快速参考

| 文件 | 行数 | 核心职责 |
|------|------|---------|
| ring0/sentinel.py | 1026 | 主监督循环 + 进程管理 |
| ring0/fitness.py | 558 | 适应度评分 + 高原期检测 |
| ring1/reflector.py | 663 | Reflexion 反射引擎 |
| ring1/llm_base.py | 500+ | LLM 多提供商抽象 |
| ring2/main.py | ∞ | 自演化活代码（生成） |

---

## 启动与安装

### 快速开始
```bash
# 远程安装（克隆、创建虚拟环境、配置、运行测试）
curl -sSL https://raw.githubusercontent.com/Drlucaslu/protea/main/setup.sh | bash
cd protea && .venv/bin/python run.py
```

### 后台运行（带自动看门狗重启）
```bash
bash run_with_nohup.sh
```

---

## 总结：自演化的三层闭合

**Protea** 实现的自演化通过三层反馈闭合：

1. **物理层闭合**（Ring 0 → Ring 2）
   - 心跳监控 → 存活检查 → 适应度评分 → 演化参数

2. **智能层闭合**（Ring 1 → Ring 2）
   - LLM 生成演化代码
   - 基因注入最佳模式
   - 反射提出改进建议

3. **用户层闭合**（用户 → Ring 1 → Ring 0）
   - Telegram 反馈（接受/拒绝/定期）
   - 约束注入 LLM 提示
   - 指令影响演化意图优先级

这三个闭合层形成了**完整的自演化系统**，既保证稳定性（监督），又实现创新（LLM 引导），还尊重用户意图（反馈驱动）。

