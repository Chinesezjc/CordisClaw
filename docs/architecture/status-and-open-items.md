# 架构与计划完成度

## 1. 判定口径

- 本文基于当前仓库现状整理，最近更新：2026-07-17。
- 历史规划蓝图已经吸收进 [design-blueprint.md](./design-blueprint.md)，因此本文结论来自三类证据的交叉比对：
  - 设计蓝图：[design-blueprint.md](./design-blueprint.md)
  - 架构文档：[system-overview.md](./system-overview.md)、[contracts-and-loading.md](./contracts-and-loading.md)、[runtime-semantics.md](./runtime-semantics.md)、[maintenance-guide.md](./maintenance-guide.md)
  - 运行时代码与测试：`crates/cordis-runtime/src/*`、`crates/cordis-runtime/tests/*`

## 2. 状态总表

| 主题 | 状态 | 结论 |
|---|---|---|
| Stage A-E 架构冻结 | 已完成 | 插件契约、ABI 契约、Loader、Artifact、Context/Security 都已有实现与文档归纳 |
| Resolver / Loader 主链路 | 已完成 | 发现、解析、拓扑加载、预算、哈希、指纹、required/optional 传播都已落地 |
| 文档契约、Graph/Doc helper、tooling | 已完成 | docs 回写、artifact index 刷新、注册图导出都可运行 |
| Execution engine | 部分完成 | 语义与库实现已完成，`execute` / `serve execute` 入口可用；仍缺更真实的数据面验证 |
| 自迭代（Agent Loop） | 已完成基础 | 固定 9 阶段 Petri Net 已替换为 open-ended agent loop；agent 可读代码、写文件、跑编译测试并验证 |
| 交互式 Agent 对话 | 已完成 | 流式输出、18 个工具（含 run_plugin_test、append_file）、readline 编辑、Ctrl+C draft 安全、`/捷径`、工具调用结果日志 |
| Agent 错误恢复 | 已完成 | JSON 解析失败反馈到 agent 自修复、LLM 请求失败 notify 到群、所有 HTTP 错误重试、unknown-tool guard（strike=1 立即停止）、tool_calls 链诊断日志 |
| 优雅退出 + data/ 持久化 | 已完成 | SIGTERM→SIGINT 转发优雅退出、`data/memory/shutdown.json` 退出快照、`data/` 沙箱扩展（agent 可访问 workspace 根级目录） |
| QQ 插件消息解析 | 已完成 | JSON 卡片透传、合并转发 ID 提取、回复 msg_id 提示 |
| Agent 安全加固 | 已完成 | secret 不可见（token 不进入 prompt）、移除 `run_command`、敏感路径黑名单、工具防火墙（`agent_accessible`）、按群隔离 session、`build_plugins` 白名单命令、工具失败自动告警 |
| Service 生命周期 | 部分完成 | `Service` trait + `ServiceRegistry` + `NodeType::Task` 已实现；plugin load 时自动 start 尚未接入 |
| 插件封装形态蓝图 | 部分完成 | `dylib` + JSON artifact + process 已落地；`cdylib` / `WASM` 未实现 |
| 更真实的运行入口与服务化边界 | 部分完成 | `RuntimeHost`、`serve` REPL、agent chat、shell console 可用；尚未稳定化为外部服务边界 |
| YAML 配置入口 | 已完成 | runtime / kernel / llm_api / plugins 配置模型完整 |

## 3. 已完成

### 3.1 Stage A-E 已经落地到可运行原型

[system-overview.md](./system-overview.md) 明确把当前实现归纳为 Stage A-E：

- Stage A：插件工程发现与元数据契约
- Stage B：运行时 ABI 契约与指纹一致性
- Stage C：`discover -> resolve -> instantiate` 的 loader 架构
- Stage D：预构建工件索引与哈希校验
- Stage E：上下文注入、作用域与授权链路

同时文档也说明，在这五段之上，仓库已经额外实现了执行引擎原型、图可视化、tooling 和 Agent 自迭代。

### 3.2 插件发现、契约校验、加载主链路已完成

[contracts-and-loading.md](./contracts-and-loading.md) 和代码实现对应关系已经比较稳定：

- [plugin/package.rs](../../crates/cordis-runtime/src/plugin/package.rs) 负责从顶层 workspace members 起步，递归解析 `package.metadata.cordis.children`，并做路径、crate name、docs、循环、越界等 fail-fast 校验。
- [plugin/loader.rs](../../crates/cordis-runtime/src/plugin/loader.rs) 负责预算校验、artifact index 读取、ABI 指纹比对、哈希校验、实例化、注册、required/optional 故障传播。
- 当前 loader 只消费预构建工件索引，不做运行时编译，也不做跨类型 fallback。

### 3.3 文档驱动注册、图导出、插件调用与工具链已完成

- `docs/agent/interfaces.json` 作为运行时输入，参与节点注册、文档查询和图导出。
- `DocRegistry` 已提供稳定的 route-style 查询约定。
- `GraphRegistry` 已能导出"已注册节点图"和"已注册 net"的 JSON/HTML。
- CLI 已暴露 `invoke`、`graph-html`、`net-html`、`sync-plugin-docs`、`refresh-artifact-index`、`auto-update`、`prepare-artifacts` 等入口。

### 3.4 自迭代已从固定管道升级为 Agent Loop

原 9 阶段 Petri Net 自迭代内核（`kernel/loop.rs`）和独立 LLM 规划器（`kernel/planner.rs`，~7000 行）已被删除，替换为：

- [host.rs](../../crates/cordis-runtime/src/host.rs)：`iterate_plugins()` — 顺序过程调用 + agent loop + 固定 finalization
- [agent.rs](../../crates/cordis-runtime/src/agent.rs)：`AgentSession::respond()` — 统一的 tool-calling loop（最多 96 轮）
- [kernel/plugin_iteration.rs](../../crates/cordis-runtime/src/kernel/plugin_iteration.rs)：策略验证、回滚日志持久化、canary 回放
- 回退安全网：panicky guard + 增量 journal + draft patch + workspace 恢复

Agent 现在可以自主完成：读代码 → 理解结构 → 写/改文件 → cargo build → cargo test → 验证结果。

### 3.5 交互式 Serve REPL 已完成

- 三种模式：命令模式 (`>`)、Agent 对话 (`>>`)、Shell console (`$`)
- 流式 LLM 输出（reasoning + content 实时显示）
- Readline 编辑（上下历史、左右光标、Ctrl+A/E）
- Ctrl+C 自动存 draft + revert
- `/捷径` 直接调插件，绕过 LLM
- Agent 会话超时/错误时自动存 draft patch 并回退工作区

### 3.6 Service 生命周期基础已完成

- `NodeType` 枚举（Task/Router/Gate/Terminal）在 SDK 中已定义
- `Service` trait（start/stop）在 context 中已实现
- `ServiceRegistry` 支持按 plugin_path 子树启停
- `RuntimeHost::start_service()` 公开 API
- TODO: 插件加载时自动遍历 Task 节点并 start

### 3.7 插件样例已扩展

- `expr` — 递归下降四则运算 + 取模 (`%`) + 幂 (`^`)，6 个运算符子插件
- `shell` — 命令 catalog 分发（Nonebot console 模式）
- `qq` — OneBot v11 QQ 适配器（configure/send/status/call/JSON卡片透传/转发/回复）
- `gacha` — 原神抽卡模拟器（角色/武器池、软硬保底、50/50、data/ 持久化）
- `root` / `root/child` — scaffold 占位

## 4. 部分完成

### 4.1 Execution engine 有正式入口，数据面验证不足

[runtime-semantics.md](./runtime-semantics.md) 明确写到：

- CPN Net、Router、Actor、Scheduler、`execute_net()` 这些执行语义已经作为库实现完成。
- `execute` CLI 与 `serve execute` 控制面命令可用，返回 `execution_id`、顺序、结果与 metrics。

尚未完成的是把 execution engine 接到更真实的数据流与业务图。

#### 4.1.1 执行引擎内部的已知缺口

以下模块/字段已实现但未接入生产路径，或语义退化：

| 缺口 | 位置 | 详情 |
|---|---|---|
| **`gate.rs` gate policy 未接入主循环** | `execution/gate.rs` | `GatePolicy` 定义了 5 种策略 + `GateDecision`（含 branch cancellation），但 `ExecutionTransitionKind::Gate` 仅做 pass-through；真正的 gate 决策尚未接入令牌就绪检查。 |
| **`ChangeMemory` 未接入生产循环** | `kernel/memory.rs` | 固定容量的变更历史，有完整实现但仅在测试中验证，未被自迭代闭环持久化消费。 |
| **`NodeType` 默认值影响** | SDK `lib.rs` | `NodeType::default()` 为 `Router`，意味着所有未声明 `node_type` 的现有节点将经过 Router overlay（begin_subgraph/commit/rollback）。当前 Router overlay 是透明的，对现有节点无实际影响，但语义上不同于原来的 Task 路径。 |

> 2026-06-05 更新：以下缺口已在本轮闭合——
> - **`NodeType` 贯通管道**：`RegisteredNode` / `RegisteredNetNode` / `ExecutionTransitionSpec` 均携带 `node_type` 字段；`build_execution_net` 使用声明类型而非启发式。
> - **`ExecutionTransitionKind::Gate`** 已添加；`NodeType` 到 `ExecutionTransitionKind` 的映射覆盖全部 4 种类型。
> - **`ArcSpec.required` 强制执行**：`is_transition_ready()` 在检查 join policy 前验证 required 弧。
> - **`KeyedPair` / `KeyedGroup` 语义修复**：`KeyedPair` 要求恰好 2 个输入 place；`KeyedGroup` 要求所有输入 place 非空。
> - **`Terminal` 结束语义**：Terminal 变迁触发后清空 ready 队列，将未完成变迁标记为 Cancelled。
> - **`ActorExecutor` 已移除**（死代码清理）。

### 4.2 Service 生命周期基本接入插件加载

`Service` trait + `ServiceRegistry` + `NodeType::Task` 已实现。
- ✅ 插件加载时自动检测 Task 节点（`NodeRegistry::task_node_fqns()`）并输出诊断日志
- ✅ 插件 reload 时自动停止被移除/变更插件的 services（`reload_internal()` 中调用 `stop_plugin_services()`）
- ⬜ plugin boot 时自动 `start_service()` 仍需外部 service factory 注册机制；当前可通过 `host.start_service()` 手动启动。

### 4.3 插件封装形态只落地了蓝图的一部分

[design-blueprint.md](./design-blueprint.md) 里的蓝图：`rlib` / `dylib` / `cdylib` / `WASM` / `external process`。

当前落地：
- `dylib` — 内部受控路径，已完成
- JSON artifact + process — 已落地
- `cdylib` / `WASM` / 更完整的 runtime adapter 生态 — 未实现（TODO）

### 4.4 工作流 API：SDK 端已定义，运行时适配层缺失

[async-workflow-api.md](./async-workflow-api.md) 和 `cordis-plugin-sdk/src/workflow.rs` 定义了完整的异步工作流类型系统：

- `WorkflowRuntime` trait — 工作流执行的抽象入口
- `CallSpec` / `JoinSpec` / `RaceSpec` — 调用、汇合、竞速原语
- `EventSpec` — 事件总线等待（无运行时事件总线）
- `SleepSpec` — 定时等待
- `AskUserSpec` — 人工审批（无运行时支持）
- `WaitFuture` — 异步等待句柄

以上类型在 SDK 中有单元测试，但**运行时 `WorkflowRuntime` 适配层完全不存在**。文档明确写"后续在 runtime 侧实现 WorkflowRuntime 适配层，桥接到 execute_net、Router 和 Context 系统"。

### 4.5 金丝雀发布：单一回放已实现，流量分层缺失

当前已有：
- `CanaryReport` / `CanaryVerdict` 类型
- `run_plugin_canary()` — 基于 invocation sample 的单一调用重放
- promote/rollback 判定逻辑
- 回退安全网

尚未完成：
- **流量拆分**（x% 流量走 candidate snapshot）
- **自动晋升**（连续 N 次 canary pass → auto promote）
- **统计信息收集**（延迟分布、错误率对比）
- **真实环境验证**（当前仅重放历史样本，非实时流量）

## 5. 明确未完成（TODO）

### 5.1 Service auto-start on plugin load

- [x] Plugin load 时遍历 `docs.nodes`，对 `node_type: Task` 的节点输出诊断日志（`NodeRegistry::task_node_fqns()`）
- [x] Plugin reload 时调用 `stop_plugin_services()` 停止被移除/变更插件的 services
- [ ] Plugin boot 时自动 `start_service()` 需要 service factory 注册机制（当前手动调用 `host.start_service()`）

### 5.2 执行引擎缺口闭合（2026-06-05 大部分已闭合）

- [x] **`gate.rs` 接入执行路径** — `ExecutionTransitionKind::Gate` 变体已添加；gate 节点在触发时 pass-through（Success）
- [x] **`ArcSpec.required` 强制执行** — `is_transition_ready()` 在 join policy 前验证 required 弧
- [x] **`KeyedPair` / `KeyedGroup` 真正的键控匹配** — `KeyedPair` 要求恰好 2 个输入；`KeyedGroup` 要求所有输入
- [x] **`NodeType::Gate` 映射到执行模型** — `build_execution_net` 使用声明的 `node_type` 设置 `ExecutionTransitionKind`
- [x] **`Terminal` 节点实现结束语义** — Terminal 变迁后停止引擎，标记未完成变迁为 Cancelled
- [x] **`ActorExecutor` 已移除** — 死代码清理完成

### 5.2.1 Verifier 安全硬化（2026-07-17 已闭合）

- [x] **命令注入**（P0-1）— `bash -lc` 被换成 `shell_words::split` + `Command::new(argv[0])`；`;`、``` ` ```、`$(...)`、重定向等 shell 元字符不再被解释为语法。`kernel/verifier.rs::run_shell_command`。
- [x] **命令超时**（P0-1）— 每条 verifier 命令有 10 分钟默认 wall-clock 超时；到点 kill + wait。防止恶意 `sleep infinity` 或死锁 `cargo test` 卡住 iteration 管道。
- [x] **无命令等于 rubber-stamp**（P0-2）— 全部 stage 均 `Skipped` 时 `tests_passed = false` 且 `quality_score = 0`。至少要有一个 stage 真的 execute 才可能 Pass。
- [x] **Verify/Promote TOCTOU**（P0-3）— `VerificationReport.source_tree_hash` 记录 verify 时的 sha256；`finalize_plugin_iteration` 在 `promote_candidate` 前重算并比对，不匹配则强制 rollback。
- [x] **Plugin verifier 目标错位**（P0-4）— `verify_plugin_iteration` 传入 `VerifyOptions::candidate_invoker`，`plugin:` 命令走 staged candidate snapshot 而非 live registry。

### 5.2.5 Web/SSRF/Session 硬化（2026-07-17 已闭合）

- [x] **Web / Vision SSRF**（P0-22）— 抽出 `ip_is_forbidden` + `check_url_safety`，对每个 URL 做 scheme + host + DNS 双重检查；覆盖 loopback / RFC1918 / CGNAT (100.64/10) / link-local (169.254/16, 含 AWS metadata) / IPv6 ULA / IPv6 link-local / IPv4-mapped IPv6 / 0.0.0.0/8。web 用自定义 `reqwest::redirect::Policy` 每一跳都重新校验；vision 关闭 redirect (`redirects(0)`) 保守 fail。
- [x] **QQ webhook X-Signature 校验**（P0-23）— `/onebot/event` 现在验证 OneBot v11 `X-Signature: sha1=<hex>` HMAC-SHA1；access_token 已配置时必过，缺 token 才 fall-back 警告接受。用 `subtle::ConstantTimeEq` 常量时间比较避免 timing side channel。
- [x] **QQ token 落盘权限**（P0-24）— `save_runtime_config` 在 Unix 上用 `OpenOptions::mode(0o600)` 创建；不再有 world-readable 窗口。
- [x] **API key 不落盘**（P0-25）— `LlmApiConfig.api_key` 加 `#[serde(skip_serializing)]`；session 快照 / shutdown memory 都不再写出这个字段。`auto_save_session` 建目录后 chmod 0o700。
- [x] **`save_draft_and_revert` 保留 untracked**（P0-26）— 不再无条件 `git clean -fd -- plugins/`；untracked 文件被 `git mv` 到 `.cordis-drafts/untracked-<ts>-<reason>/` 保留；`git checkout` 只 revert tracked 修改。用户还没 add 的新文件不再被吞。
- [x] **Vision OCR temp file 冲突**（P0-27）— 文件名从 `cordis_ocr_<pid>.<ext>` 改为 `cordis_ocr_<pid>_<seq>_<nanos>.<ext>`；同进程并发 OCR 不再互相覆盖。

### 5.2.4 Kernel↔Plugin 边界收敛（2026-07-17 已闭合）

- [x] **`agent_run_command` 去 shell**（P0-17）— 从 `sh -c command` 改为 `shell_words::split` + `Command::new(argv[0]).args(...)`；`;`、``` ` ```、`$(...)`、重定向、管道 全部丧失特殊语义，Agent 工具面不再暴露 shell-injection 面。
- [x] **Git 插件 argv 硬化**（P0-18）— 新增 `is_safe_ref` / `is_safe_hash` 拒绝首字符 `-` 与非 `[A-Za-z0-9_./-]` 字符；`git_reset` / `git_rebase` 的 `target` / `onto`、`git_cherry_pick` 的 commit hash 全部经过该校验，`--exec=…` / `--pathspec-from-file=…` 等无法作为 flag 传入 git。`git_add` 在自定义 paths 分支加显式 `--` 分隔符，`-p` / `--interactive` 之类的 path 只能是 pathspec。`validate_commit_message` 删除误伤合法词的子串禁词，Message 是单个 argv 元素，git 从不解析其中的 `--force`。
- [x] **Filesystem / Git 白名单不再静默降级**（P0-19）— `canonicalize` 失败时不再退回未规范化路径。新增 `canonicalise_for_whitelist` / `validate_path_in_root` 的深度 ancestor 解析：路径不存在时 canonicalize 最深的存在祖先再拼 tail；两侧都失败则 fail-closed。`../../etc/shadow` 之类的构造再无绕过。
- [x] **Plugin iteration symlink 逃逸**（P0-20）— `PluginEditExecutor::execute` 在写入前调用新 helper `resolve_under_workspace`：canonicalize 结果必须仍在 workspace_root 下。plugins 内的 symlink 指向 `/etc/passwd` 之类无法被写入。
- [x] **Verifier shell 拼接**（P0-21）— 已随 A 批 P0-1 修复。`discover_rust_workspace_manifest` 输出的字符串被 `shell_words` 解析成 argv，空格 / `$` / `;` 无法被解释。

### 5.2.12 插件业务硬化（K 批，2026-07-17 已闭合）

- [x] **QQ system_notify 拒绝 bad group id**（P1-26）— `.parse().unwrap_or(0)` 换成 `parse::<i64>` + `id > 0` 校验；非法 id 跳过 + stderr 警告，不再静默发送到 group 0。
- [x] **QQ dedup FIFO**（P1-38）— `HashSet<String>` 换成 `VecDeque<String>`，`push_back` + `pop_front` 保证正确 FIFO 驱逐，"驱逐最旧" 不再是随机集合序。
- [x] **QQ 队列上限计数**（P1-39）— 新增 `MESSAGE_QUEUE_DROPPED: AtomicU64`，队列 >= 128 时递增；每 2^n 次到 stderr。之前静默丢弃。
- [x] **QQ WebSocket 死代码删除**（P1-40）— `start_ws_server` / `handle_ws_connection` (73 行) + `tungstenite` Cargo 依赖移除。
- [x] **Gacha node_id 白名单**（P1-43）— `GachaRequest` 加 `node_id` 字段；`gacha_status` 只允许 `cmd=status`，其他节点走原命令集。之前 agent 用 `gacha_status` 节点发 `cmd=reset` 就能清 pity。
- [x] **Expr parser 深度限制**（P1-45）— `Parser` 加 `depth` 计数器 + `MAX_PARSE_DEPTH=512` + 每个 parse_* 方法 `enter_scope`/`exit_scope` 包裹。`1^1^1^...` 与深嵌套括号不再栈溢出，返回 `TooDeep`。
- [x] **Expr factorial 上限**（P1-46）— 输入 > 170 直接返回 `FactorialOverflow`；`10000000000!` 之类的 CPU DoS 不再可行。
- [x] **Expr NonFinite 显式错**（P1-47）— `Pow` 结果非 finite 返回 `EvalError::NonFinite`；`10^500`、`(-1)^0.5` 不再触发 serde_json NaN 序列化崩溃。
- [x] **graph_registry 多 producer 确定性 + 显式诊断**（P1-50）— `candidates.sort()` 保证选择跨构建一致；诊断信息改为 `candidates=[...] chosen=X (sort-stable)` 便于 kernel status 与 HTML render 上报。

未做（在 K 剩余项，非紧急）:
- P1-41 shell cwd escape / P1-42 shell script quote / P1-44 gacha lock+atomic /
  P1-48 package.rs generated docs / P1-49 normalize_crate_name conflict /
  P1-51 cycle topo_level=0 / P1-52 main.rs sessions eviction /
  P1-53 main.rs message misrouting.

### 5.2.11 Agent 层硬化（J 批，2026-07-17 已闭合）

- [x] **`fs_search` 早停**（P1-27）— 新增 `walk_code_files_ctl` + `WalkControl::Stop`；`agent_search_code` 命中 40 后立即中止目录遍历，不再继续读文件 / 触发 IO。
- [x] **`agent_read_file` 加 byte cap**（P1-28）— 读盘前 `metadata().len()` 检查，超过 16 MiB 直接拒；即使 sandbox 允许 `/dev/zero` 之外的大文件也不会 OOM。
- [x] **`agent_replace_in_file` 拒绝多重匹配**（P1-29）— 命中数 !=1 报错（0 = pattern not found；>1 = 需要更多上下文）。旧行为 `replacen(..., 1)` 会静默改错位置。
- [x] **`agent_rename_file` 备份 destination**（P1-30）— rename 前若 `new_path` 已存在，先把它的字节备份进 rollback；`revert_changes` 现在能还原被覆盖的目标。
- [x] **`compact_history` 重算 estimated_tokens**（P1-32）— compact 后重新从 `history` 汇总，不再"只增不减"。之前 400K→800K 触发一次 compact 后每次请求都触发，且 compact early-exit no-op，导致 guard 永久失效。
- [x] **`compact_history` UTF-8 char/byte 一致**（P1-33）— `content.len() > 500`（byte）改成 `content.chars().count() > 500`（char），中文/多字节文本的 `…` 标记不再一致触发。返回值改为 `(old_len, new_len)` 供 tool 使用。
- [x] **`send_chat_request` 4xx 不重试**（P1-35）— 400-499（除 408 / 429）直接返回 `LlmRequestFailed`，不再对坏 API key / bad model / 权限拒绝重试 3 次。
- [ ] **Session self-lookup 冲突**（P1-25）— 需要把 session 存 `Arc<Mutex<...>>` 共享，大重构，未做。当前 `agent_compact_context` 在 turn 内仍返回 AgentSessionNotFound。
- [ ] **P1-26 QQ group id .parse().unwrap_or(0)** — 待 K 批。
- [ ] **P1-31 96-turn overflow / P1-34 streaming reader / P1-36 unknown_tool double / P1-37 timeout 联动** — agent 层深修，未做。

### 5.2.10 Reload / 生命周期硬化（I 批，2026-07-17 已闭合）

- [x] **`reload_subtree` stop 走 FFI 加 `catch_unwind`**（P1-19）— 每次 Task stop 调用被 `panic::catch_unwind(AssertUnwindSafe(...))` 包裹；插件 stop 处理器 panic 不再跨 C ABI unwind（UB）也不再让整个 reload 崩。panic message 落到 stderr。
- [x] **Phase-1 pre-loaded dylib 生命周期澄清**（P1-20）— 加显式注释：`_dylib` 是 struct-owned by-value handle，drop 于 fn return（Phase 2 之后）；invoke 路径每次自开 `LoadedDylibApi::open`，registry 不缓存函数指针，无 dangling reference。原 concern 事实上不构成 bug，但注释固化契约。
- [x] **`reload_subtree` 新 snapshot_id**（P1-21）— `to_snapshot_id` 使用 `make_snapshot_dir_name()` 生成新 id；invocation trace 现在能区分 reload 前后。
- [x] **`retired_snapshots` 加上限**（P1-22）— 除 Weak-dead 清理外，硬上限 `MAX_RETIRED_SNAPSHOTS = 64`，超出时按 FIFO 丢弃最旧的 `staged_artifact_root` + 目录；长活 agent session pin snapshot 也只能 leak 有限前缀。
- [x] **`plugin_history` 加 eviction**（P1-23）— hard cap `MAX_PLUGIN_HISTORY = 1024`；每次 `push_front` 后 `pop_back` 到上限内。之前 unbounded VecDeque 长运行会 OOM。
- [x] **Session 若干次要问题**（P1-24）— (a) 新 `delete_session_snapshot(session_id)` API，让 completed/reset 的 session 从磁盘摘除；(b) `detect_crash_and_recover` 用 `session.kind()` 决定 `AgentSessionKind`（`plugin_iteration` 走 PluginIteration 而不是硬编码 RuntimeShell）；(c) auto-save 的 tmp 文件名从共用 `.<id>.json.tmp` 换成 `.<id>.json.tmp.<seq>`（atomic 计数器），并发 `agent_send` 同一 session 不再互踩。

### 5.2.9 原子写入 / rollback（H 批，2026-07-17 已闭合）

- [x] **`write_shutdown_memory` 原子写**（P1-15）— 新 helper `atomic_write_bytes`（tmp + sync_all + rename）；crash 中留 truncated JSON 的问题消失。
- [x] **`create_plugin` workspace 清单加 flock**（P1-16）— `plugins/Cargo.toml.create-lock` 上 fcntl LOCK_EX；两个并发 `create_plugin` 不再互相踩 members。写入本身走 `atomic_write_bytes`。
- [x] **`write_pretty_json` 通用 durable 写入**（P1-17）— `write_pretty_json`（docs interfaces.json / artifact index.json 的写盘入口）改成 tmp + sync_all + rename。`refresh_artifact_index` / `prepare_artifacts_locked` 转用它。lock file 也补了 `sync_all`。
- [x] **`kernel/auto_update.rs::execute` 全路径 rollback**（P1-18）— 逐 patch 应用改成 `apply_one` 闭包 + Err 分支主动 `self.rollback(&backups)`；任何一 patch 失败都会把已经写完的前几个 patch 全部还原，不再半途留下工作树脏。

### 5.2.8 Lock / 生命周期硬化（G 批，2026-07-17 已闭合）

- [x] **`record_plugin_iteration_outcome` 单锁段**（P1-9）— 拆成"每个 Mutex 单独作用域"，函数在任意瞬间最多持有一把锁；与 `select_issue_for_request` 等 `plugin_issues → plugin_history` 反向路径不再冲突。文件内 canonical lock order 也补了注释。
- [x] **`kernel::notify::register` 去重 + `unregister`**（P1-10）— 已注册的 `(plugin_path, node_id)` 重复 register 是 no-op；reload 时可用 `unregister` / `unregister_plugin` 移除。之前 push-only 让 reload 累积重复交付。
- [x] **`kernel::health::start_health_loop` 支持停止**（P1-11）— 返回 `HealthLoopHandle`（内含 `AtomicBool` + `JoinHandle`）；`Drop` 或 `stop()` 都会 set flag + join 线程。sleep 拆成 500ms 分片，shutdown 延迟 ≤ 500ms。runtime-only 分支目前用 `mem::forget` 保留原语义，等 shutdown-orchestration 换成显式 stop。
- [x] **`static mut AGENT_TRIGGER_TX` → `OnceLock<Sender>`**（P1-12）— 无 unsafe 的初始化-一次 + 并发安全 read。
- [x] **`libc::signal(SIGTERM)` → `sigaction`**（P1-19/12）— 用 POSIX `sigaction(2)` + `SA_RESTART` 装 SIGTERM→SIGINT forwarder；老 `signal(2)` 在多线程进程下语义不定，现在显式定义。
- [x] **`PluginRegistry` 8 处 `.expect(...)` → tolerant lock**（P1-13）— 全部换成 `.unwrap_or_else(|poison| poison.into_inner())`，与 `context/mod.rs` / `kernel/notify.rs` 保持一致。任何加载路径 panic 中毒后，registry 不再让每一个 reader 都 crash。
- [x] **`TASK_LIBRARIES` 已随 P0-16 修**（P1-14）— tolerant lock，不再 `unwrap`。

### 5.2.7 并行执行引擎硬化（F 批，2026-07-17 已闭合）

- [x] **`commit_session` CAS**（P1-1）— `session_version` 用 `compare_exchange` 单步领取版本；两个线程并发 commit 同一 expected_version 不再都通过检查后各自 `fetch_add`。冲突方走 `CommitConflict` 错误。
- [x] **`list_by_ns` 与 `lookup_slot_entry` 锁序统一**（P1-2）— 都改成 active → overlays → request → session → global；`list_by_ns` 先 clone overlay snapshot 再顺序拿其它 lock，两条路径反向锁序造成的双工死锁被消除。
- [x] **并行路径 Router skip 语义对称**（P1-3）— skip 计算移到 Router 判定之前；`skip: skip` 不再叠加 `router_run.is_none()`，Router 现在遵守单线程一致的 AllOf-skip 规则。
- [x] **并行 batch 保留 priority 排序**（P1-4）— `BTreeMap<CorrelationKey, ReadyItem>` 去重后先 `sort_by(cmp_ready)`（topo_level → priority → ids）再 `take(N)`；高优先级 transition 不再因 correlation-key 字典序被低优先级挤掉。
- [x] **并行 runner panic 不再毒化 executor**（P1-5）— `h.join()` 的 `Err(panic)` 被翻译成 `RuntimeError::Invariant`（附 panic message），本 batch 其它 job 的结果仍能被 merge。之前 `.unwrap()` 让整个 executor 崩掉。
- [x] **`KeyedPair` arity 校验**（P1-6）— `execute_net` 入口对所有 KeyedPair transition 校验 input arc 数是否恰好 2；1 或 ≥3 直接 `RuntimeError::Invariant`，不再"静默永不 fire"。
- [x] **Router Timeout 不再覆盖 Success**（P1-7）— `execute_router` 仅当 `raw_outcome != Success` 且超时时才把结果改成 Timeout；慢成功保留 Success 不再被回滚。
- [x] **`eval_first_success` 不再永远 Wait**（P1-8）— completion counter 改用 `outcomes.iter().all(...)` 直接从状态源判定，某个 upstream 已 terminal 但未入 `completion_order` 时也能正确 CompleteFailure。

### 5.2.6 Build / Load 硬化（第 2 批，2026-07-17 已闭合）

- [x] **`invoke_dylib` UAF**（P0-11）— `CatalogPlugin` 现在持有 `Arc<Mutex<Option<LoadedDylib>>>`。首次 invoke 时 dlopen 并缓存 `Library` + `api_ptr`；后续 invoke 直接复用。返回的 `PluginResponse.payload` 内存所属的 dylib 与 `CatalogPlugin` 同生命周期，不再存在 dlclose-after-return 的悬空 String 问题。
- [x] **ABI fingerprint 校验**（P0-12）— `ArtifactIndexEntry` 新加 `abi_fingerprint` 字段（可选，兼容老 index）；host 首次加载 dylib 时调用 `(api.abi_fingerprint)()` 并与 index 记录比对，不匹配返回 `PluginHostError::AbiFingerprintMismatch` 拒绝 invoke。之前该字段完全是死代码。
- [x] **Loader docs-drift TOCTOU**（P0-14）— 每处 tmp 文件从共用 `<file>.tmp` 换成 `<file>.cordis-tmp.<pid>-<seq>-<nanos>` (`unique_staging_path`)，两个并发 loader 不再互相 clobber。整个 auto-heal 段用 fcntl `flock(LOCK_EX)` 拿排他锁 (`<snapshot_root>/artifacts/index.json.heal-lock`) 保护，跨进程互斥。

### 5.2.3 Build / Load 硬化（第 1 批，2026-07-17 已闭合）

- [x] **平台原生 dylib 后缀**（P0-9）— `rebuild_plugin_workspace` 用 `std::env::consts::{DLL_PREFIX, DLL_SUFFIX}` 而非硬编码 `.so`，macOS/Windows 上的 iteration 现在能跑。同时 `.so` 覆盖走新的 `stage_then_rename_file` helper：tmp + sync_all + rename，避免 live-mmap SIGSEGV。
- [x] **`materialize_artifact_entry` 同款硬化**（P0-8 补丁）— 也走 `stage_then_rename_file`，同一模板 stage-swap；共同解决过去两条 rebuild 路径的行为不一致。
- [x] **`lock_pid_is_live` 跨平台**（P0-10）— 从 `Path::new("/proc").join(pid).exists()` 改为 `libc::kill(pid, 0)` + errno 判定 (`ESRCH` = dead, `EPERM` = alive)。macOS/BSD 上不再把所有活锁误判为死锁。
- [x] **Loader sha256 校验**（P0-13）— `Loader::load_with_staging_root` 现在对每个 artifact 调用 `sha256_file` 并与 index 记录比对，不匹配就走 `PluginUnavailableReason::HashMismatch`（此前始终不校验，`HashMismatch` variant 是死代码）。
- [x] **`stage_file` 原子化**（P0-15）— 从 `remove + hard_link/copy` 双步改为 `stage <target>.cordis-staging.<pid>` + `rename`，两个并发 loader 不再互相 clobber 目标文件。
- [x] **`TASK_LIBRARIES` 从 `Vec` 换 `HashMap<plugin_path, ...>`**（P0-16）— 重复 invoke 同一 Task 节点不再累积 dylib mapping；reload 路径新增 `unregister_task_library` 让旧 `.so` 能被 OS 卸载。`.lock().unwrap()` 也换成 tolerant lock，poison 不再让 invoke 热路径直接 crash。

### 5.2.2 Rollback / Journal 硬化（2026-07-17 已闭合）

- [x] **写入前先备份**（P0-5）— `PluginEditExecutor::execute` 现在先把 `AppliedEditBackup` 推入 rollback，再调用 `atomic_write`；写入失败时本函数内立即 `rollback.rollback()` 并把错误上抛，工作树无残留。
- [x] **Rollback journal 持久化**（P0-6）— `persist_journal` 走 `<path>.cordis-tmp` + `sync_all` + rename；`RuntimeHost::boot` 在初始 snapshot 构建后自动调 `restore_plugin_iteration_workspace` 扫描 `plugin-iteration-edit-journal.json`，重放尚未清理的 rollback。
- [x] **Restore 幂等**（P0-7）— journal 头新增 `rollback_generation_id`；成功 restore 后写 sibling `.applied` marker（同样 atomic）。下次 boot 若 journal 与 marker 的 generation id 一致，跳过重放，避免二次 rollback 已恢复的源码。
- [x] **Artifact 参与 rollback**（P0-8）— `iterate_plugins` 在 `rebuild_plugin_workspace` 之前把即将被覆盖的 `artifacts/{plugin}.so` 备份进 rollback，并 re-persist journal。发生 rollback 时源码与编译产物一并回退，避免"source 回退但新 `.so` 留在盘上"的行为漂移。

### 5.3 工作流运行时适配层

- [ ] 实现 `WorkflowRuntime` trait，桥接到 `execute_net`、Router、Context 系统
- [ ] 实现 `EventSpec` 所需的运行时事件总线
- [ ] 实现 `AskUserSpec` 的人工审批回调机制

### 5.4 真实 canary 发布（流量分层 + 自动晋升）

当前已有：
- `run_plugin_canary()` — 基于 invocation sample 回放的单次 canary 检查
- promote/rollback 判定
- 回退安全网

尚未完成（TODO）：
- 流量分层（x% 流量走 candidate）
- 自动晋升（连续 N 次 canary pass → auto promote）
- 真实环境验证

### 5.5 Agent 工具面扩展

> 2026-06-05 更新：Web 搜索/抓取 + Git 操作已从 Kernel 拆为独立插件
> (`fixtures/plugins/web/` 和 `fixtures/plugins/git/`)，符合 Kernel 提供机制、Plugin 提供能力的原则。

当前 agent 有 15 个工具（read/write/search/run/revert/runtime ops），Kernel 自省 6 个，其余能力通过插件提供：
- [x] Web fetch / search — `web` 插件 (`web_search` 支持 Brave + Bing 双后端自动切换, `web_fetch` 节点)
- [x] Git 操作 — `git` 插件 (`git_diff`, `git_log`, `git_status`, `git_commit` 节点)
- [ ] 多文件 diff 预览（改动前展示将要修改的内容）

### 5.6 更多插件封装形态

- [ ] `cdylib` — 跨版本稳定 ABI
- [ ] `WASM` — 第三方插件沙盒
- [ ] `external process` — 不可信插件隔离

### 5.7 服务边界稳定化

- [ ] `DocRegistry` 升级为 HTTP/dedicated 服务
- [ ] `GraphRegistry` net 推导规则增强
- [ ] Agent 对话的 HTTP/WebSocket 远程接入

### 5.8 QQ adapter 接入真实 NoneBot 协议

- [ ] WebSocket 反向连接（当前仅 HTTP client）
- [ ] 事件订阅（当前仅主动调用）
- [ ] 作为 Service 常驻运行（`NodeType::Task`）

## 6. 建议优先级

1. **闭合执行引擎缺口** — `gate.rs` 接入或清理、`ArcSpec.required` 强制执行、`KeyedPair`/`KeyedGroup` 真正的键控匹配。这些是设计文档承诺但未生效的核心语义。
2. 把 Service auto-start 接入 plugin load，让 Task 节点能随插件自动启停。
3. 扩展 Agent 工具面（web fetch、git），提升自主能力。
4. 实现工作流运行时适配层，让 SDK 端已就绪的类型系统在运行时可用。
5. 补更多插件样例与契约测试，验证 loader 边界。
6. 在契约稳定后考虑扩展到 `cdylib` / `WASM`。

## 7. 当前最准确的总体判断

截至 2026-06-04：

- 架构主干和契约已经做完了。
- 自迭代已从固定管道升级为 Agent Loop，具备真实的读-写-构建-测试能力。
- 交互式 serve REPL 已完备（流式对话、readline、draft 安全）。
- Service 生命周期基础已完成，待接入 plugin load。
- **执行引擎"库完备，语义有缺口"** — `gate.rs`（248 行）、`scheduler.rs` 的 `run_deterministic`、`ActorExecutor` 是已实现但未接入的死代码；`ArcSpec.required`、`KeyedPair`/`KeyedGroup`、`NodeType::Gate`、`Terminal` 结束语义是已定义但未生效的语义。
- **工作流 SDK 已就绪，运行时适配层缺失** — `WorkflowRuntime` trait 与 6 种原语在 SDK 中有定义和测试，但运行时桥接层尚未实现。
- **金丝雀发布只有单次回放，无流量分层和自动晋升。**
- 未完成的主要是：执行语义闭合、产品化、服务化、更多插件形态和金丝雀闭环。
