# 架构与计划完成度

## 1. 判定口径

- 本文基于当前仓库现状整理，最近更新：2026-07-20。
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

### 5.2.24 REPL / serve 收尾正确性（E 批任务 C，2026-07-20）

批处理式 `cat cmds | cordis-runtime serve fixtures` 时，`kernel iterate-plugins` 的最终结果 JSON 会被后面排队的 `quit` 打断（5.2.22 E2E 二轮复验里 run log 停在第二次 reload 的 `[snapshot] detected`，`final_verdict` 缺失）。改动仅限 [crates/cordis-runtime/src/main.rs](../../crates/cordis-runtime/src/main.rs)：

- **根因**：serve 主循环里，每条命令的 stdout flush 只发生在 `handle_serve_command` 内部末尾（`io::stdout().flush()`）与主循环的 `Err` 分支；而 `quit`/`exit` 走 `Ok(false) => return Ok(false)`，在到达那个 flush **之前**就 `break` 出循环。批处理模式下 stdout 连到 pipe（全缓冲，非行缓冲），`iterate-plugins` 那一坨大 JSON 还躺在用户态缓冲区里没落地，`quit` 一 break、`run_serve` 返回、进程退出，缓冲区内容随之丢失 → 输出截断。（并非 stdin 并发消费，也非内部 reload 调 `process::exit`——排查过 `iterate_plugins` 全链路：rebuild 子进程用 `Stdio::null()` 接 stdin，两次 reload 与 promote/rollback 都不碰 `process::exit`。）
- **修法**：把主循环尾部改成"先算 `keep_going`，**无条件 `io::stdout().flush()?`，再决定是否 break**"。这样退出路径（`Ok(false)`）与正常路径、错误路径统一都在读下一条 stdin 前 flush 完；命令处理中途不允许 `process::exit`（现状本就没有）。
- **顺带**：新增 `emit_plugin_iteration_result()`，`kernel iterate-plugins` 与 `kernel plan-apply` 两个入口共用。先打印一行人类可读摘要 `iterate-plugins: final_verdict=... changed_paths=[...] blocked_reason=...`，完整 JSON 仍在其后**最后一行**，`tail -1 | jq` 照旧可解析。

**验证**：
- 快速失败路径（policy-blocked 的 `create_file` 到非法子树路径，`iterate_plugins` 立即返回 `rolled_back`，不触碰 rebuild/DeepSeek）：`printf '<req>\nquit\n' | serve fixtures 2>/dev/null | tail -1 | jq -r .final_verdict` → `rolled_back`，`jq 'keys|length'` → 19（结果结构体全字段齐全，无截断）；摘要行 `grep` 命中。
- 慢路径（manual edit_plan：给 `time` 插件新建一个 trivial 通过测试，走完 rebuild + 两次 reload + verify + canary + promote）跑通一次：`final_verdict=promoted`，last line 10769 bytes 完整 JSON。
- `cargo build -p cordis-runtime` 零新增 warning（bin 仍是既有 3 条：SIGTERM 函数指针 cast、`session_id` unused、闭包 `mut`）；`cargo test -p cordis-runtime --lib` 89 passed / 0 failed。
### 5.2.25 QQ 对话链路端到端验证（E 批任务 D，2026-07-20）

首次对 **QQ 消息进来 → agent 处理 → 回复发出去** 的完整链路做端到端验证（此前只有 P0-23 签名单测，整条链从未拉通跑过）。产品面，runtime 侧只读不改。

**链路图**（token 消耗点已标注）：

```
① OneBot 客户端 / curl
      │  POST /onebot/event  (body + X-Signature: sha1=<hmac-sha1(body, access_token)>)
      ▼
② qq_serve HTTP 事件循环  run_event_loop  (lib.rs:491)
      │  verify_onebot_signature 校验签名 (lib.rs:1038)：配了 token → 必校验，失败 401
      │  先回 200 {"status":"ok"}，再解析事件
      ▼
③ handle_onebot_event  (lib.rs:575)
      │  post_type != "message" → 丢弃
      │  黑名单(block_groups) > 灰度白名单(allow_groups) 过滤
      │  extract_message_info 抽取文本 / reply / CQ code
      │  按 message_id 去重 (RECENT_MESSAGE_IDS, FIFO≤200)
      ▼
④ MESSAGE_QUEUE  (VecDeque, 上限 128；满则计数丢弃 MESSAGE_QUEUE_DROPPED)
      ▼
⑤ start_agent_poller 轮询线程  (lib.rs:839, 每 5s 排空)
      │  should_process 过滤（≤2 字符 / 斜杠命令跳过）
      │  拼 prompt: "[QQ group from <gid> (user <uid>)]: <text>"
      │  cordis_plugin_sdk::agent_trigger(prompt)  → dlsym _cordis_agent_trigger
      ▼
⑥ runtime: _cordis_agent_trigger  (main.rs:46) → AGENT_TRIGGER_TX.send
      ▼
⑦ runtime inbox 循环  (main.rs:309, 仅 --runtime-only 模式)
      │  strip_prefix "[QQ group from " 解析 group_id → by_group 分片（防跨群串台 P1-53）
      │  group_id → session 映射 (agent_start，上限 512，LRU 驱逐)
      │  host.agent_send(sid, combined)   ★消耗 DeepSeek token★
      │  解析 agent 输出 JSON: suspend / respond
      ▼
⑧ 若 respond: host.invoke("qq","qq_send",{target:"group:<gid>",message})  (main.rs:408)
      ▼
⑨ qq_send  (lib.rs:1174) → onebot_send_group_msg → HTTP POST 到 onebot_url/send_group_msg
      ▼
⑩ OneBot 客户端发到 QQ 群
```

**注意**：agent 触发有两条并存路径 —— (a) `start_agent_poller` 主动 `agent_trigger`（上图 ⑤→⑥）；(b) agent 主动 `qq_fetch_messages` 拉取。本次验证走 (a) 主动推送路径。另外 inbox 里 agent 的 system hint 要求 agent 自己调 `qq_send` 后 `suspend`，而 main.rs:399 的 `respond` 分支也会兜底发一次——本次实测 agent 走的是 hint 指定的「自己调 qq_send + suspend」路径，sink 收到的正是 agent 主动发的那条。

**分段验证结果**：

| 段 | 内容 | 验证手段 | 结果 |
|----|------|----------|------|
| ② webhook 接收 | /health 就绪 + 事件 POST 返回 200 | `scripts/qq_e2e_verify.sh` A/B 段 | PASS |
| ② 签名校验 | 合法 sig→200，错误 sig→401，无 sig(配token)→401 | `qq_e2e_verify.sh` B/C/D 段 | PASS |
| ③ 事件解析/CQ | segments + raw_message + CQ code 抽取 | 单测 `extract_message_info_*` | PASS |
| ③ 灰度/黑名单 | 白名单只放行白名单群；黑名单优先 | 单测 `webhook_ingest_dedup_whitelist_blocklist_chain` | PASS |
| ④ 去重/队列 | 相同 message_id 只入队一次 | 单测（同上）+ `qq_e2e_verify.sh` E 段 | PASS |
| ⑤ prompt 格式 | poller 拼的 prompt 能被 inbox 反解出 group_id | 单测 `agent_prompt_format_is_parseable_by_session_router` | PASS |
| ⑤→⑦ trigger→路由 | agent_trigger 到达 inbox 并正确分组 | 探针脚本 serve 日志 `inbox: [123456] batch 1 msgs` | PASS |
| ⑦ agent 处理 | DeepSeek 真实调用，agent 回复 | `scripts/qq_e2e_send_probe.sh`（消耗 token，跑 1 次） | PASS |
| ⑧⑨ qq_send 出口 | 回复动作构造正确并到达 HTTP 出口 | mock sink 落盘 | PASS |

出口段实证（mock OneBot sink 捕获）：发「bot 请只回复两个字：收到」，sink 收到
`{"endpoint":"send_group_msg","params":{"group_id":123456,"message":"收到"},"authorization":"Bearer <token>"}`
—— target/message/token 三者构造正确，`group:123456` 被正确解析为 `send_group_msg` + `group_id`。

**脚本**（`scripts/`，均可重复执行）：
- `mock_onebot_sink.py` — mock OneBot v11 HTTP API，拦 send_group_msg/send_private_msg 落盘。
- `send_onebot_event.sh` — 用 openssl 算 HMAC-SHA1 X-Signature，构造并 POST OneBot v11 群消息事件；支持 `--bad-sig`/`--no-sig`/自定义 message_id，便于断言签名与去重。
- `qq_e2e_verify.sh` — 不消耗 token 的分段编排（A~E 段），起 sink + serve，跑签名/去重段，输出 PASS/FAIL。
- `qq_e2e_send_probe.sh` — 消耗 token 的出口段探针（需 `CONFIRM_TOKEN_SPEND=1`），真实 DeepSeek 触发 → 断言 sink 收到 send_group_msg。

**新增测试**：`fixtures/plugins/qq/src/lib.rs` 内 `chain_tests` 模块 +6（webhook 入队/去重/灰度/黑名单、非消息忽略、prompt 格式契约、segments/CQ 抽取、should_process 过滤）。`cargo test -p qq` 由 5 → 11 全绿。

**移交 runtime 的问题（本次只记录，未改）**：
1. **测试隔离依赖 `config/` 同级目录**：`discover_config_dir`（config.rs:266）用 `fixtures_root.parent()/config` 定位 `llm_api.yaml`。用临时 fixtures 目录跑 `--runtime-only` 时找不到 config，inbox 报「LLM API key missing」。且 `config/` 被 `.gitignore`，git worktree 里默认不存在，需手动 symlink 才能跑通 agent 段。建议：支持 `CORDIS_CONFIG_DIR` 环境变量显式指定，或让 `--runtime-only` 也接受 `CORDIS_FIXTURES_ROOT` 联动定位 config。
2. **agent 触发路径无背压/无确认**：`start_agent_poller` 每 5s 排空 MESSAGE_QUEUE 并逐条 `agent_trigger`，但 trigger 只是 `tx.send` 到 inbox channel，poller 不感知 agent 是否处理完；若 DeepSeek 慢，channel 会堆积（inbox 侧靠 `by_group` 合并缓解，但无上限背压）。属 runtime inbox 设计，记录待评估。
3. **`_cordis_agent_trigger` 副作用写死路径**：main.rs:49 每次 trigger 都 `std::fs::write("/tmp/trigger_called.txt", ...)`，疑似调试残留，生产环境下多实例会互相覆盖。建议移除或改为可配置。

### 5.2.22 sha256 同步 + handle panic 隔离（D 批，2026-07-20）

紧跟 5.2.21 E2E smoke 的两条 finding，一次修完：

- **`rebuild_plugin_workspace` 后同步 `index.json` 里 `entry.sha256`**（[plugin/tooling.rs:246-278](../../crates/cordis-runtime/src/plugin/tooling.rs#L246-L278)）：stage-swap 完 `.so` 后，直接读回新文件 sha256，只更新受本次 rebuild 影响的 entry（避免对未 build 的 entry 也刷新，从而误 unlock 一些"stale 但 required"的检测）。原子写回 `index.json`。这条修复消除了 5.2.21 里 verifier 判 `HashMismatch → rolled_back` 的假失败。
- **插件 `handle` panic 隔离**（[plugin/invoke.rs:230, 350-425](../../crates/cordis-runtime/src/plugin/invoke.rs#L230)、[cordis-plugin-host/src/lib.rs:347-370](../../crates/cordis-plugin-host/src/lib.rs#L347-L370)）：`api.handle` 是普通 `fn`（不是 `extern "C"`），Rust 允许 panic 跨帧 unwind，直接把 runtime 撕成 corrupted 状态。两处调用点都包 `catch_unwind(AssertUnwindSafe(...))`，捕获后转 `RuntimeError::InvalidArgument` / `PluginHostError::PluginInvocationFailed`，plugin_path 一并带出。**Service `start`/`stop`（`extern "C" fn`）不在此覆盖范围**：modern rustc 会在任何 panic 尝试 unwind 过 C ABI 时自动 `abort()`，runtime 侧的 `catch_unwind` 拦不住；后续要做 SDK 侧的服务包装。lib.rs 里 code comment 已注明这条边界。

新增单测：`plugin::invoke::panic_isolation_tests` +2（handle panic 转 Err、正常 handle 透传）。lib 测试 87 → 89，全 pass。

**顺带**：`tests/architecture.rs` 里 `scheduler_is_deterministic_across_runs` 引用了 P2-7 已删除的 `run_deterministic` / `ScheduledNode`，本次一并 stub 掉（保留 test 名 + 一段 comment，避免误再引入）。此前 `tests/architecture.rs` 本身另有 11 项因 `root/child` fixture 早年被拆解（P2-10）而失效，非本批范围，记 5.7 待清理。

**E2E 复验**：注入相同 bug 二次跑 `iterate_plugins` — final JSON 因 REPL `quit` 打断没打完，但落盘验证：源码保留 fix 未 rollback；`index.json` sha256 与 `.so` bytes 一致。安全网从"误触发"变为"正确沉默"，agent 的 fix 得以持久化。

### 5.2.23 SDK 侧 service panic 隔离（E 批任务 B，2026-07-20）

紧接 5.2.22 留下的边界：`ServiceVTable::{start,stop}` 是 `extern "C" fn`（[cordis-plugin-sdk/src/lib.rs:302-304](../../crates/cordis-plugin-sdk/src/lib.rs#L302-L304)），modern rustc 在 panic 试图 unwind 穿越 C ABI 帧时自动 `abort()`，runtime 侧（[plugin/invoke.rs:270-273](../../crates/cordis-runtime/src/plugin/invoke.rs#L270-L273) 的 `(vtable.start)(vtable.data)`）的 `catch_unwind` 拦不住（实测 SIGABRT）。隔离必须做在插件侧、在 Rust 帧上、在跨 ABI **之前**。

- **`guard_service_call(op, data, body)`**（SDK）：把用户的 `fn(*mut c_void) -> i32` 服务体包进 `catch_unwind(AssertUnwindSafe(...))`。捕获 panic 后 `eprintln!` 带 `op`（start/stop）与 panic message，返回 `-1`；正常路径透传用户返回码。unwind 在到达任何 `extern "C"` 帧之前就被吃掉，进程不 abort。
- **`service_vtable!{ data=, start=, stop= }` 宏**（SDK，`#[macro_export]`）：生成两个 `extern "C" fn` shim，各自 body 只调 `guard_service_call`，再组装成 `ServiceVTable`。插件只写会 panic 的安全 Rust `fn`，宏保证 catch 在 ABI 内侧。这是 5.2.22 里 "后续要做的 SDK 侧服务包装" 的落地。

**插件迁移情况**：现存唯一真实 service `qq_serve` 目前**不经过** `ServiceVTable` 路径——它跑在普通 `handle` fn 分支（已由 D 批 5.2.22 的 runtime 侧 `catch_unwind` 覆盖，因为 `handle` 是普通 `fn`）。`_cordis_create_service` / `ServiceVTable` 是前瞻性路径，当前无任何插件导出它。因此本任务只落地 SDK 包装层，无需迁移插件；未来任何真正用 `ServiceVTable` 的插件都应通过 `service_vtable!` 构造，天然获得隔离。

**新增单测**（SDK crate，`service_panic_isolation_tests` +3，SDK lib 测试 3 → 6 全 pass）：
- `guarded_panicking_start_returns_minus_one_without_abort` — 直接调 `guard_service_call`，panicky start 返回 -1，进程不 abort。
- `service_vtable_macro_isolates_start_panic` — 用公开宏建 vtable，按 runtime 的方式 `(vtable.start)(vtable.data)` / `(vtable.stop)(vtable.data)` 调用，两者都返回 -1（证明 catch 在 `extern "C"` shim 内侧生效）。
- `service_vtable_macro_passes_through_success_code` — 正常 start/stop 透传返回码（0），不误伤。

**验证**：`cargo test -p cordis-plugin-sdk` 6 pass；`cargo build` 通过（SDK crate 零 warning；main.rs 的 3 条 warning 为既有、非本改动）；`cargo test -p cordis-runtime --lib` 89 pass（fresh worktree 需先补 `fixtures/artifacts/`，该目录为 build 产物非 git-tracked，与本改动无关）。

### 5.2.21 自迭代端到端 smoke test（C 批，2026-07-20）

用真实 DeepSeek LLM + serve REPL 端到端跑一次 `iterate_plugins`，验证从 bug 注入 → 测试失败 → 自动定位 → 编辑 → 编译 → 测试 → rollback 判定的整条链路：

1. **注入 bug**：`fixtures/plugins/time/src/lib.rs` `handle_time_now` 里 `let timestamp = 0i64;`（原本 `dur.as_secs() as i64`）。
2. **注入 sentinel**：`fixtures/plugins/time/tests/sanity.rs` 断言 `timestamp > 0` 且距参考时钟 ±1d。手动 `cargo test --test sanity` 立即失败：`time_now timestamp must be positive, got 0`。
3. **REPL 触发**：`kernel iterate-plugins {"target_plugin_paths":["time"], "instruction":"...", "tests_command":"..."}`。9 个 tool call、5 verification attempts、2 successes 后 agent 自主完成：`list_context_files` → `read_context_files` → `replace_files_exact`（编辑正好一处，`0i64` → `dur.as_secs() as i64`）→ `run_plugin_check` → `rebuild_plugin_workspace` → 三次 `run_plugin_test`（前两次因 manifest-path 错误 fail，第三次 agent 自主纠错切到 `plugins/Cargo.toml`）→ `record_iteration_summary`。
4. **rollback 判定**：`final_verdict = rolled_back, blocked_reason = "plugin unavailable: time, reason=HashMismatch"`。P0-13 硬编码 `entry.sha256` 校验（loader 里当时的 TODO 直接信任，此处已改为实际计算并对比）触发 —— 新 rebuild 的 `.so` 与 `index.json` 里的 sha256 不匹配，安全网正确地拒绝上线；journal 回放把源码恢复到 pre-edit 字节。
5. **验证结论**：agent 端 self-iteration 循环 **功能上完全跑通**（正确定位 + 正确编辑 + 通过测试）；hard-rollback 层 **也如设计工作**（sha256 保护线拒绝 stale index 的 promote）。**遗留 finding**：`refresh_artifact_index` 在 `rebuild_plugin_workspace` 后未刷新 `index.json`，需要 candidate 通道在 promote 前统一 sync 一次；已记入下一次修复批次。

耗时：~1.5 min（含 DeepSeek 8 次交互 + 一次 `cargo build fixtures`）。

### 5.2.20 崩溃恢复集成测试（B 批，2026-07-20）

`crates/cordis-runtime/tests/crash_recovery.rs` 用 6 项集成测试覆盖 P0-6 / P0-7 rollback journal 的跨 boot 恢复语义 —— 这些是"设计上安全，但只被单元测试碰过"的最关键路径：

- **`recovery_restores_edited_file_after_simulated_crash`** — persist_journal → 手动 mutate 文件 → drop rollback（模拟 SIGKILL 未 clear_journal）→ 新 boot 调用 recovery → 验证：文件回到 pre-edit 字节，journal 已清理。
- **`double_boot_after_recovery_is_a_noop`** — recovery 完成后用户合法 edit → 第二次 boot 不再有 journal → recovery 是 no-op → 用户 edit 不被 clobber。
- **`recovery_is_idempotent_when_crash_between_rollback_and_clear`** — P0-7 核心场景。手动构造"rollback 完但 clear_journal 前崩溃"状态（journal 存在 + marker 存在 + 用户已 edit）→ 第二次 boot 见到 marker.id == journal.id → 立即短路，不再 rollback → 用户 edit 保留。
- **`stale_marker_does_not_block_a_new_journal`** — 旧 iteration 的 marker 残留 + 新 iteration 的 journal（不同 generation_id）→ recovery 仍然执行新 journal 的 replay。marker 只保护"同一 journal 不重复应用"，不阻塞其它 iteration。
- **`journal_generation_id_is_stable_across_reload`** — journal 落盘 → load_journal 重建 → re-persist 生成新 generation_id。保证 P0-6 durability + P0-7 identity 双属性。
- **`in_memory_rollback_path_applies_when_no_journal`** — 同进程失败恢复分支：没 on-disk journal 但传入 `PluginEditRollback` 时，应用 in-memory rollback。

**关键设计**：`restore_plugin_iteration_workspace` 拆成 `apply_plugin_iteration_journal`（纯 journal 层）+ 后置的 `rebuild_plugin_workspace`。集成测试只调 journal 层，避免依赖真实 cargo build 环境（fixtures 完整性、network 状态、macOS 平台）。前者标 `#[doc(hidden)] pub` 明示是 test hook。

### 5.2.19 回归测试大规模补齐（U/V/W/Y 批，2026-07-20）

第二轮 review 把之前遗漏的 P0/P1/P2 修复补上单测。原则：**测试跟被测代码在同一 crate**——runtime crate 修的 bug 用 runtime 单测，plugin 内部的行为放到 plugin 自己的 `#[cfg(test)]` 里，避免 runtime 测试知道 plugin 实现细节。

**Runtime 层（`crates/cordis-runtime/src/**`）新增 36 项，从 51 → 87：**

- **context/mod.rs** +2：P1-1 CAS 独占语义（8 线程并发 commit 仅 1 成功）；P1-2 反向锁序双工无死锁。
- **execution/gate.rs** +5：P1-8 `eval_first_success` 在 completion_order 不完整时也能收敛；AllOf / AtLeast 语义、cancel_pending 分支。
- **execution/engine.rs** +3：P1-6 `KeyedPair` arity=1/=3 拒绝、AllOf 单入口接受、`cmp_ready` 排序（P1-4）。
- **kernel/plugin_iteration.rs** +7：journal generation_id 唯一性（P0-6）、`load_journal` round-trip、`clear_journal` 幂等、P2-23 `absorb` dedup 保留 first backup、混合 create+delete 的 rollback 语义。
- **kernel/auto_update.rs** +5：P2-27 find≠1 拒绝（0 / 多重）、P1-18 per-patch 失败回滚已应用部分、verify 失败回滚。
- **kernel/notify.rs** +3：P1-10 register dedup、`unregister` 按 (plugin,node) 单点、`unregister_plugin` 只影响 target。
- **kernel/health.rs** +2：P1-11 `sleep_interruptible` 早退（500ms 内响应停止）、自然完成路径。
- **plugin/package.rs** +2：P1-49 `normalize_crate_name` 冲突（foo-bar / foo_bar / a/b / a-b 归一同名）。
- **plugin/tooling.rs** +2：P1-17 `write_pretty_json` tmp+rename 无 leftover、`stage_then_rename_file` 保留 exec 位。
- **agent.rs** +3：P1-32/33 `compact_history` 阈值下 no-op、阈值上 shrink+重算 estimated_tokens、`estimate_tokens` 长度线性。

**Plugin 层（`fixtures/plugins/**`）新增 30 项：**

- **git/src/lib.rs** +7：P0-18 `is_safe_ref` 拒 leading-dash / shell metachars / 空、`is_safe_hash` 拒 slash、`validate_commit_message` 只拒空（不再子串禁词）、接受含 `--force` 字面的正常 commit message。
- **filesystem/src/lib.rs** +3：P0-19 `canonicalise_for_whitelist` 已存在路径 / 不存在 leaf 走 ancestor / 不可能路径要么返回 None 要么仍是绝对路径。
- **shell/src/lib.rs** +7：P1-42 `split_script_commands` 尊重双/单引号内 `;`、按 `\n` 或 `;` 顶层分、`split_tokens` 用 shell_words 拒未闭合引号 / 处理带空格的引用参数 / 空输入。
- **gacha/src/lib.rs** +5：P1-43 `gacha_status` 节点拒 `reset` / `pull`、`gacha_entry` 节点接受全命令集、未知 node_id 拒、缺 node_id 走 entry 语义。
- **expr/parser/src/core.rs** +3：P1-45 深嵌套括号 / 右递归 `1^1^1...` 触发 `TooDeep`、正常输入通过。
- **expr/evaluator/src/core.rs** +5：P1-47 pow → Inf / NaN 都触发 `NonFinite`、finite 结果通过、`DivisionByZero` 语义不被 NonFinite 吞、P1-46 factorial overflow 冒泡到 EvaluateExpressionError。
- **expr/evaluator/factorial/src/core.rs** +5：P1-46 n>170 拒绝、170! 仍 finite、大 n 快速拒（<50ms）、domain error 语义、`0!=1!=1`。

**测试组织约定**：runtime 层测试放 runtime crate；插件内部行为放插件自己的 `#[cfg(test)] mod tests`；跨插件共享的 SSRF 单测集中在 `fixtures/plugins/_net/` crate；SDK 层的 `AbiFingerprint::current_build` 单测在 SDK crate。

**运行方式**：
```
cargo test --lib                                    # SDK 3 + runtime 87 = 90
cargo test --manifest-path fixtures/plugins/Cargo.toml --lib
                                                    # git+fs+shell+gacha 22 + expr 组合 13 + net 3
cd fixtures/plugins/expr/parser && cargo test --lib # 3 (excluded from plugin workspace)
cd fixtures/plugins/expr/evaluator/factorial && cargo test --lib # 5 (同上)
cd fixtures/plugins/expr/lexer && cargo test --lib  # excluded, external tests only
```

至此测试面从 51 (runtime lib only) 扩展到 **runtime 87 + SDK 3 + net 3 + plugins 30 = 123 单测**。

### 5.2.18 P2 收尾（T 批，2026-07-17 已闭合）

- [x] **P2-11 空 vision 子目录清理** — `fixtures/plugins/vision/vision-ocr/` 和 `vision-describe/` 只包含 docs 存根、无 Cargo.toml/src；已删除。vision plugin 的 `children = []` 保留，不再有"半吊子子插件"的目录。
- [x] **P2-2 抽 `cordis-net` 共享 crate** — 新 `fixtures/plugins/_net/` (rlib, plugin workspace 内 `exclude` 保证 loader 不当它是 plugin)；`web` 和 `vision` 都改成 `cordis_net::{check_url_safety, ip_is_forbidden}`。SSRF 单测 3 项移入 `_net`，两处原地拷贝彻底消除。
- [x] **P2-13 fingerprint 编译时自动填充** — SDK 加 `build.rs` 通过 `cargo:rustc-env` stamp `CORDIS_RUSTC_VERSION` (从 `rustc --version`) 和 `CORDIS_TARGET`；SDK 加 `AbiFingerprint::current_build(crate_hash, api_hash)` 帮手，plugin 只需提供 plugin-specific hash。fixtures 里的 hard-coded fingerprint 保留（迁移需同步 index.json 的 fingerprint 缓存 + Cargo.toml metadata，动作太大），但注释指路：新插件应用 `current_build()`。SDK 新增 1 个 test 验证 stamp 生效。

**至此 review 计划的所有 findings 全部处理完毕。**

**P2-1（Kernel↔Plugin 工具下沉）标为 not-a-finding**：review 时把它当作违反 CLAUDE.md 边界原则的问题。经确认，`read_file` / `write_file` / `search_code` / `run_command` 这类 agent 直接依赖的基础能力**属于 Kernel = 最小可用单位** 的设计定义之内 —— Kernel 不是"零工具的调度框架"，而是"agent 拿起就能跑的最小自包含 runtime"。CLAUDE.md 里 "shell/filesystem/web/git 应作为 plugin" 描述的是**能力扩展方向**，不是要把已有的基础工具全部剥离出 kernel。因此：

- `filesystem` / `shell` / `web` / `git` 插件继续存在，用于**扩展 & 覆写**（比如换 web 搜索源、切换 shell backend）。
- Kernel 里内建的 `agent_read_file` / `write_file` / `search_code` / `run_command` 保留作为**默认实现**，避免"没插件 agent 什么都做不了"的启动死锁。
- 未来若要真做插件覆写，`invoke_plugin("/filesystem/*", ...)` 已经可用；agent 可以选择性调用。

未来 review 无需再把这一条列为 finding。

### 5.2.17 P2 剩余打磨（Q/R/S 批，2026-07-17 已闭合）

**Q 批 (rollback / promote 语义):**

- [x] **P2-22 stage_process_command 对称 canonicalize** — 两侧同时成功才用 canonical 路径，任一侧失败则同时 fallback，`starts_with` 比较像与像，不再 false-positive Invariant。
- [x] **P2-23 absorb dedup** — `PluginEditRollback::absorb` 按 rel_path 去重（保留 first backup），rollback 反向仍到达 pre-edit 状态；journal 大小不再随长 iteration 循环爆炸。
- [x] **P2-24 Blocked 语义说明** — Partial + !manual_approved 保留 candidate 供后续 approve 是 by design；加代码注释说明下次 iterate 会替换 candidate 的 UX hazard。
- [x] **P2-25 promote 失败 rollback** — Pass/Pass 与 Pass/Partial+approved 分支的 `promote_candidate()?` 都换成 match，Err 时显式 rollback candidate + restore workspace，把 promote error 挂到 blocked_reason 后返回。之前 promote fail 让工作树 + journal 双残留。
- [x] **P2-26 expect_substring 严格化** — 新增 `exact:` 与 `line:` 前缀，让 verifier plugin 输出可用精确匹配；无前缀保持 legacy contains 行为。

**R 批 (tooling):**

- [x] **P2-19 cleanup_fixture_lockfiles 加 opt-in** — 只在 `CORDIS_CLEAN_FIXTURE_LOCKFILES=1` 时才删；避免每次 build 后误删并发读者持有的 Cargo.lock，也不再影响可重现构建。
- [x] **P2-28 modtime 失败取 now**（非 UNIX_EPOCH）— dirty-tracking 不再把无法读 mtime 的文件误判为"极老"；log stderr 提示。
- [x] **P2-29 rebuild_plugin_workspace 加 strip_proxy_envs** — 与 `run_command` 对齐；企业代理下不再挂起。
- [x] **P2-30 rebuild 加 build timeout** — 新 helper `run_command_with_timeout` (20min 默认，`CORDIS_BUILD_TIMEOUT_SECS` 覆盖) + poll-based kill+wait；死循环 build.rs 不再挂死 iteration 管道。
- [x] **P2-12 QQ 硬编码路径**（部分修）— 新 `runtime_config_path()` 从 `$CORDIS_FIXTURES_ROOT` 派生；env 未设置时回退到历史 `/root/CordisClaw/...`。
- [ ] **P2-13 fingerprint per-build** — 需要 SDK build.rs + fixture 全量重构（每个 plugin 用 const 而非硬编码字符串）；改动面太大，留待后续。

**S 批 (fixture / naming / misc):**

- [x] **P2-3 / P2-4 tool spec 命名冲突注释** — `run_plugin_check` / `run_plugin_test` / `rebuild_plugin_workspace` 在 RuntimeShell 和 PluginIteration 两个 backend 里同名不同 schema；加注释说明未来若统一 dispatch table 需要重命名。
- [x] **P2-5 `#[repr(C)]` on String 结构体注释澄清** — 说明 outer layout stable、String 内部不稳定，跨 toolchain 只靠 `AbiFingerprint::rustc_version` 校验；真正 FFI 需要 `*mut c_char`。
- [x] **P2-10 `fixtures/plugins/root/Cargo.toml` children 清空** — `./child` 已删；`children = []` 让 `PackageResolver` 不再报缺失依赖。

**Kernel↔Plugin 边界 — 设计说明（非 finding）:**

- `filesystem` / `shell` / `web` / `git` 插件：**扩展与覆写点**，让用户可以替换 web 搜索源、shell backend 等。
- Kernel 内建 `agent_read_file` / `write_file` / `search_code` / `run_command`：**默认实现**，保证"最小可用 runtime"—— 没插件时 agent 也能干活。
- 这里的 Kernel 定义是"agent 拿起就能跑的最小自包含系统"，不是"零工具的纯调度框架"。CLAUDE.md 中"shell/filesystem/web/git 应作为 plugin"应理解为**能力扩展方向**，而非要求从 kernel 剥离已有基础工具。

**未做（大改动，非紧急）:**

- **P2-9 / P2-11 Task/Gate/Terminal fixture + `vision-ocr` / `vision-describe` 空目录** — P2-11 已在 T 批处理；P2-9 属于 test coverage 提升，需要真实场景 fixture。

### 5.2.16 P2 文档同步（P 批，2026-07-17 已闭合）

- [x] **`rs-files-responsibility.md` 重写**（P2-14）— execution/kernel/agent/service 表全部更新为当前代码状态。删除 `actor.rs`、`run_deterministic` 死代码条目；加 `notify.rs`、`health.rs`、`html_render.rs`；标注每个模块里已应用的 P0/P1 修复号。
- [x] **`runtime-semantics.md` self-iteration 章节**（P2-16）— 2.3 Actor 段去掉；3.1 `RuntimeHost` API 明确 `iterate_plugins()` 是主入口，`run_iteration()` 是 legacy；3.3 AutoUpdater 标 legacy 并列出 P1-18 / P2-27 修复。
- [x] **`design-blueprint.md` 行号 / 工具数**（P2-17）— "16 个内核工具" 更新为"约 20 个"并标注 P2-1（file/shell 类工具下沉计划）；`host.rs:2045` 换为函数名引用（源码行号随时变化）。
- [ ] **P2-1 Kernel↔Plugin 边界收敛（工具下沉）** — Kernel 侧 file/shell/search 工具仍在 `agent.rs`，未真正下沉到 `filesystem`/`shell`/`web`/`git` 插件（agent 端提示词也未改）。属于产品级重构，不在本轮 review 范围。
- [ ] **P2-2 SSRF util 抽 `cordis-plugin-net`** — web / vision 依然复制粘贴同一份 `ip_is_forbidden`；抽 crate 需要 workspace 布局调整，留到后续。
- [ ] **P2-15 status-and-open-items 日期与内容落后** — 已随本轮更新；本文件即"最新状态"。

### 5.2.15 P2 代码打磨（O 批，2026-07-17 已闭合）

- [x] **`make_execution_id` unique**（P2-18）— 加进程本地 `AtomicU64` seq；纳秒 + seq 组合，时钟回拨或同纳秒 boot 不再撞 id。
- [x] **`plugin/invoke.rs` NUL/JSON 严格化**（P2-20/21）— `CString::new(node_id)` 失败明确报错；`payload` 反序列失败也不再吞成 null。
- [x] **`net.rs::ArcSpec.required` 注释更新**（P2-6）— 说明 required 已由 `is_transition_ready` 强制。
- [x] **`scheduler.rs::run_deterministic` 死代码删除**（P2-7）— 连同 `ScheduledNode` / `ExecutionReport` / `ReadyItem` / 私有 `cmp_ready` 一并删除；保留 `SchedulerConfig`（`ExecutionConfig` 依赖它）。
- [x] **`auto_update.rs` text patch 严格化**（P2-27）— `find` 命中数 !=1 直接报错（0 → 未找到；>1 → 需上下文），不再 `replacen(1)` 静默取首个。
- [x] **`respond` err path 补 Assistant placeholder**（P2-31）— 分离 `respond` / `respond_inner`；Err 时若 `transcript` 里有 orphan User，插入 `[error] <msg>` Assistant 条目，retry 同 session 不再 double-record 用户输入。
- [x] **`from_snapshot` api_key 澄清**（P2-33）— 加注释说明 `#[serde(skip_serializing)]` 让 api_key 恢复时为 `None`，`resolve_api_key` 从 env 补，避免"雷带毒配置"担忧。
- [x] **`AGENT_INJECT_QUEUE` 已知局限文档化**（P2-32）— 加注释说明单 static + 多 session 的 mis-routing 风险为 latent（当前只跑一个 primary session），per-session queue 是未来工作。
- [x] **`loader::read_plugin_docs` 显式记诊断**（P2-34）— `if let Ok` 换成 `match`，Err 分支写 stderr；架构不兼容/missing symbol 的 dylib drift 不再静默。
- [x] **QQ `unwrap()` 换 `unwrap_or_default`**（P2-35）— `/health` 响应 JSON 编码失败不再让整个事件循环 panic。
- [x] **Gacha state path**（P2-38）— 优先用 `$CORDIS_FIXTURES_ROOT/../data/gacha/state.json`，摆脱 cwd 依赖。
- [x] **Gacha avg 5★ 公式**（P2-39）— 新增 `char_5_star_count` 字段并用于 avg 计算；旧公式 `total_pulls / pity_5_total` 实际 ≈ 1，一直是错的。
- [x] **Shell `ShellPlugin::fixtures_root` 注释**（P2-36）— 说明当前始终 None，保留字段作为未来 host-injected default 的扩展点。
- [x] **Shell `run_repl` isatty 检查**（P2-37）— 非 TTY 立即返回错误，headless host 不再 hang。

### 5.2.14 P1-25 收尾（2026-07-17 已闭合）

- [x] **Session self-lookup 冲突**（P1-25）— 新增 `PendingSessionAction` 与 `RuntimeHost::pending_session_actions` 侧信道；`agent_compact_context` 先尝试 `get_mut(session_id)`，命中就立刻应用（例如从别的 session 调过来的），未命中则 `queue_session_action(session_id, CompactHistory)` 并返回 `{"deferred": true, ...}`。`agent_send` 在 respond 完成后 reinsert 前 drain 队列并应用；单一活跃 session 的 self-lookup 不再 `AgentSessionNotFound`。之前评估需要 `Arc<Mutex<Session>>` 大重构；这个方案只加了一个 side-channel Mutex，兼容现有 remove/insert 模式，代价是 compact 在 turn 结束时执行而非中途。

### 5.2.13 P1 收尾（L/M/N 批，2026-07-17 已闭合）

**L 批 — Agent 深修 (P1-31, 34, 36, 37):**

- [x] **96-turn overflow 保留 partial history**（P1-31）— err path 前把 `messages` 里超出 `self.history` 的部分抄回 `history` 并累加 `estimated_tokens`；retry 同 session 不再从零 replan。
- [x] **Streaming reader 生命周期文档化**（P1-34）— 加显式注释说明 reader thread 通过 `reqwest::Client::timeout` bound 停止；未做零成本 abort（需要底层 TCP-level cancel）。
- [x] **`execute_agent_tool_call` unknown-tool 防双计**（P1-36）— 增加 `debug_assert!` 保证外层 filter 起作用；不再自增 `unknown_tool_strikes`（respond 已加过一次）。
- [x] **Timeout 联动**（P1-37）— `respond` 里的 turn budget 用 `max(timeout_ms, stream_timeout_secs * 5s)`，两套配置合并为 effective_budget_ms。

**M 批 — Shell / Gacha / package.rs (P1-41, 42, 44, 48, 49):**

- [x] **Shell cwd escape**（P1-41）— `BuiltinShell` 加 `sandbox_root: PathBuf`，`cd` canonicalize 后必须 `starts_with(sandbox_root)`；`cd /etc` / `cd ../../..` 被拒。
- [x] **Shell 引号 + 未闭合报错**（P1-42）— `split_script_commands` 重写按引号感知切分 `;`/`\n`；`split_tokens` 换用 `shell_words::split` 并对未闭合引号返回 Err，caller 显式处理。
- [x] **Gacha 并发保护 + 反序列化不 clobber**（P1-44）— 新增 `STATE_LOCK: Mutex<()>` 包住 read-modify-write；load 失败不 default-reset，改设 `SAVE_BLOCKED` 阻止后续写；save 走 tmp + rename。
- [x] **`generated_agent_docs_allowed` 需显式 opt-in**（P1-48）— `CordisMetadata::allow_generated_docs` 默认 `false`；只 `crate-type=["dylib"]` 不再自动豁免 docs 契约。
- [x] **`normalize_crate_name` 冲突检测**（P1-49）— `PackageResolver::resolve` 收尾扫所有 plugin 路径的规范化 crate name，出现冲突立即 `RuntimeError::Invariant` 报告冲突组，不再等 cargo 阶段拿到晦涩错误。

**N 批 — main.rs + graph (P1-51, 52, 53):**

- [x] **Graph cycle 与真根区分**（P1-51）— cycle 参与节点 topo_level = `usize::MAX` 而非 `0`；HTML render / execution ordering 现在可以按 level 排序时把 cycle 排最后。
- [x] **main.rs sessions LRU eviction**（P1-52）— `MAX_SESSIONS = 512` 硬上限；超出时淘汰最早 group 并 `delete_session_snapshot` 清理磁盘。
- [x] **main.rs message misrouting**（P1-53）— 移除 group 循环里 `while let Ok(late) = rx.try_recv() { inject_queue.push_back(late) }`——那段代码把任意 group 消息灌到当前 group 的 inject 队列。由外层 `loop { rx.recv() }` 保证按 group 分片。

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
