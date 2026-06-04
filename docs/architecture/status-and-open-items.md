# 架构与计划完成度

## 1. 判定口径

- 本文基于当前仓库现状整理，最近更新：2026-06-05。
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
| 交互式 Agent 对话 | 已完成 | 流式输出、15 个工具、readline 编辑、Ctrl+C draft 安全、`/捷径` |
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
- `qq` — OneBot v11 QQ 适配器（configure/send/status/call）
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
- [x] Web fetch / search — `web` 插件 (`web_search`, `web_fetch` 节点)
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
