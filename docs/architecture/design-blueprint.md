# 设计蓝图

本文档用于承接历史规划中的架构蓝图内容，并把它收敛进当前的文档体系。

阅读约定：

- 这里保留的是“设计基线”和“目标模型”。
- 具体到当前已经实现了什么、代码边界在哪里，请同时对照：
  - [system-overview.md](./system-overview.md)
  - [contracts-and-loading.md](./contracts-and-loading.md)
  - [runtime-semantics.md](./runtime-semantics.md)
  - [plugins-and-tooling.md](./plugins-and-tooling.md)
  - [status-and-open-items.md](./status-and-open-items.md)

## 1. 总体分层

原始规划的系统分层可以概括为：

```text
Plugin Packaging
   -> Plugin Runtime Adapter
   -> Node Registry
   -> Execution Graph
   -> Scheduler + Actor Executor
   -> ExecutionState
   -> OpenClaw Iteration Loop
```

这套分层强调四件事：

- 插件负责声明能力，而不是手写整条业务 net。
- CPN net 负责描述依赖关系。
- Scheduler / Actor 负责确定性执行。
- Kernel 负责在安全边界内做观察、验证、评分与演进。

在当前文档体系中的对应位置：

- 系统边界与主流程：见 [system-overview.md](./system-overview.md)
- 插件契约与加载：见 [contracts-and-loading.md](./contracts-and-loading.md)
- 执行、Context、Kernel：见 [runtime-semantics.md](./runtime-semantics.md)

## 2. 插件封装策略

原始蓝图把插件封装形式分成五类：

- `rlib`：内置核心插件
- `dylib`：内部受控热更新插件
- `cdylib`：跨版本、偏稳定 ABI 的扩展插件
- `WASM`：第三方插件生态
- `external process`：不可信插件

当前架构文档已经稳定下来的关键原则是：

- `dylib` 属于受控内部通道
- ABI 指纹必须严格匹配
- 不做 `dylib -> cdylib/wasm` 自动降级
- loader 只消费预构建工件和索引

这些“已经收敛为当前实现约束”的部分，见 [contracts-and-loading.md](./contracts-and-loading.md)。

而 `cdylib` / `WASM` / 更完整 runtime adapter 生态，仍属于设计蓝图的一部分，当前完成度见 [status-and-open-items.md](./status-and-open-items.md)。

## 3. 插件节点与插件树模型

原始规划里，插件的核心职责是暴露节点能力：

```rust
pub struct NodeMeta {
    pub id: &'static str,
    pub consumes: Vec<TypeId>,
    pub produces: Vec<TypeId>,
    pub node_type: NodeType,
}
```

节点类型分为：

- `Task`
- `Router`
- `Gate`
- `Terminal`

插件树模型强调：

- 支持完全嵌套插件树
- `plugin_path` 全局唯一
- `node_fqn = plugin_path::node_id` 全局唯一
- 父子关系只看 `package.metadata.cordis.children`
- 冲突在 resolve 阶段 fail-fast

这些语义在当前实现中已经具体化为：

- `plugin_path`
- `declared_nodes`
- `children`
- `grants`
- `required`

对应实现和当前约束请看 [contracts-and-loading.md](./contracts-and-loading.md)。

## 4. Execution Graph 设计基线

原始蓝图把执行图拆成三部分：

### 4.1 节点类型

- `TaskNode`：普通执行步骤
- `RouterNode`：动态选择子 pipeline
- `GateNode`：聚合上游分支的控制策略
- `TerminalNode`：产出最终结果或触发最终提交

### 4.2 边类型

- 数据依赖：`producer(output) -> consumer(input)`
- 控制依赖：只表达先后顺序
- 条件依赖：由 Router 根据分支结果选择

### 4.3 自动 Net 生成

蓝图默认希望系统能根据 `produces / consumes` 自动连线，并遵守：

- 多 producer 冲突时先看显式绑定
- 其次看 priority
- 再次看 `node_id`
- 无法唯一确定时 fail-fast

当前运行时文档里已经落地的部分包括：

- net build
- required input 缺失检测
- 多 producer 冲突检测
- 环检测

对应实现与当前边界见 [runtime-semantics.md](./runtime-semantics.md)。

## 5. Gate、Router、Scheduler 与 Actor

原始规划要求的 Gate 策略包括：

- `AllOf`
- `AnyOf`
- `FirstSuccess`
- `FirstCompleted`
- `AtLeast(k)`

并且明确了三条重要语义：

- `FirstSuccess` 可以在首个成功后取消其余分支
- `timeout` 属于终态，并按失败类参与聚合
- 被取消分支的下游应被标记为 `Skipped`

Router 的设计基线是：

- Router 负责实例化子图
- 子图执行包在 overlay 事务中
- 成功提交 overlay
- 失败 / 超时 / 取消时回滚 overlay

Scheduler / Actor 的设计基线是：

- Scheduler 维护 ready queue、运行中节点、依赖计数和输出
- Actor 负责执行节点，尽量通过 message passing 降低共享状态
- ready queue 排序键固定，保证相同输入下执行顺序尽量确定

这些内容已经被当前文档收敛到 [runtime-semantics.md](./runtime-semantics.md) 中。

## 6. Context 设计基线

原始规划里，Context 的核心目标是“一致性与隔离优先”。

关键约束包括：

- 分层作用域：`Global -> Session -> Request`
- 再叠加按 `plugin_path` 组织的 `Local` 链
- 默认写入发生在 request / overlay 内
- session 写需要显式提交
- global 默认只读

服务注册与解析基线是：

- `provide / inject / dispose`
- 查找顺序：`Local(current -> parent...) -> Request -> Session -> Global`
- 子插件默认不能继承父插件 Local 服务，必须经过 `grants`
- required service 缺失要 fail-fast

事务与一致性基线是：

- `begin_subgraph`
- `commit_overlay`
- `rollback_overlay`
- `commit_session` 使用 CAS 校验

这些内容在当前实现里已经基本成型，并被整理到 [runtime-semantics.md](./runtime-semantics.md) 中。

## 7. 一次命令执行的参考流程

原始规划把一次命令执行抽象成：

```text
load_session_snapshot
  -> build_request_overlay
  -> parser
  -> command_resolve
  -> selected_pipeline
  -> render
  -> optional_commit_session
  -> send
```

它想表达的不是某个具体命令，而是统一运行模式：

- 先拿到 session 快照
- 再建 request overlay
- 中间通过 parser / router / pipeline 进入具体 net
- 在结束时决定是否提交 session

当前仓库里更贴近现实的调用路径与样例插件，请看 [plugins-and-tooling.md](./plugins-and-tooling.md)。

## 8. Loader 与 ABI 设计基线

原始规划里，loader 的设计重点有四个：

- 只从顶层 workspace members 起步
- 通过 direct-children metadata 递归展开插件树
- 只消费预构建 artifact index
- 用严格 ABI 指纹和哈希校验控制加载

配套的故障策略是：

- `ArtifactMissing`
- `HashMismatch`
- `AbiMismatch`
- `SymbolMissing`
- `InitFailed`
- `BudgetExceeded`
- `ContractViolation`

以及：

- `required` 子插件失败要沿父链传播
- `optional` 子插件失败只标记为 `Unavailable`
- 不做跨类型 fallback

这些内容已经进入 [contracts-and-loading.md](./contracts-and-loading.md) 的正式文档语义。

## 9. 推荐工程结构

原始规划中给出的工程结构，核心意图不是逐目录照搬，而是强调职责边界：

- plugin 层负责发现、解析、加载、注册、调用
- execution 层负责 CPN Net、Router、Scheduler、Actor
- context 层负责作用域、键模型、事务与注入
- kernel 层负责策略、评估、记忆与自迭代闭环
- service 层负责 doc / graph 等对外查询能力

当前仓库已经按相近的方式落到了：

- `crates/cordis-runtime/src/plugin`
- `crates/cordis-runtime/src/execution`
- `crates/cordis-runtime/src/context`
- `crates/cordis-runtime/src/kernel`
- `crates/cordis-runtime/src/service`

具体职责分工见 [rs-files-responsibility.md](../rs-files-responsibility.md)。

## 10. Agent 与自迭代

原始蓝图设想了一条固定的 Kernel 流水线：

```text
observe -> diagnose -> plan -> apply -> verify -> score -> promote/rollback
```

这个固定 9 阶段 Petri Net（`kernel/loop.rs` + `kernel/planner.rs`，~9200 行）**已被删除**。
替换为 **open-ended agent loop**——由 LLM 自主决定每一步做什么。

### 10.1 Agent 模型

当前只有一个 Agent session 类型在生产中使用：**RuntimeShell**（`RuntimeShellAgentBackend`, agent.rs）。
它拥有 15 个内核工具（文件读写、搜索、构建、插件调用等），负责 QQ 聊天和 REPL 交互。

**PluginIteration**（`PluginIterationAgentBackend`, host.rs:3113）提供额外工具：
`replace_files_exact`, `run_plugin_check`, `run_plugin_test`, `rebuild_plugin_workspace`, `record_iteration_summary`。
这些是插件迭代**能力**，当前仅被 `iterate_plugins()`（通过 `llm-auto-update` CLI 或 Kernel 自动触发）
使用的独立 session 调用，RuntimeShell 目前拿不到。

**待解决的设计问题**：PluginIteration 工具应当在用户通过 RuntimeShell 请求时也可用。
即：用户说"改进 gacha" → RuntimeShell agent 应当能调用 `run_plugin_check` 等迭代工具，
而不是走一条完全分离的代码路径。这需要合并两个 backend 或让 RuntimeShell 能够访问 PluginIteration 工具集。

### 10.2 自迭代流程

当前 `iterate_plugins()` (host.rs:2045) 的工作流：

1. **准备阶段**（`PreparedPluginIteration`）：
   - 收集目标插件上下文（`collect_plugin_context_paths`）
   - 加载安全策略（`PluginIterationPolicy`）——路径白名单、敏感路径、diff 上限、时间预算
   - 如果已有预审批 edit plan，直接执行（跳过 agent session）
2. **Agent 阶段**（`run_plugin_iteration_agent`）：
   - 启动独立的 `PluginIterationAgent` session
   - Agent 自主读代码、写补丁、跑检查、跑测试
   - `record_iteration_summary` 记录最终结果
3. **Finalization**（`finalize_iteration`）：
   - rebuild → stage → verify → canary → promote/rollback
   - 回退安全网：panic guard + commit journal + draft patch + workspace 恢复

### 10.3 Stage A-E 架构冻结

以下阶段已在当前实现中收敛：

- Stage A：Package Contract Freeze
- Stage B：Runtime Contract Freeze
- Stage C：Loader Design Freeze
- Stage D：Artifact Design Freeze
- Stage E：Context & Security Freeze

对应实现与当前边界见 [contracts-and-loading.md](./contracts-and-loading.md)、[runtime-semantics.md](./runtime-semantics.md)、[status-and-open-items.md](./status-and-open-items.md)。

## 11. 现在应该怎么使用这套文档

如果你关心的是：

- “最初想把系统设计成什么样”：
  看本文。
- “现在代码真实实现到哪里了”：
  看 [system-overview.md](./system-overview.md)、[contracts-and-loading.md](./contracts-and-loading.md)、[runtime-semantics.md](./runtime-semantics.md)。
- “哪些已经做完，哪些还是计划”：
  看 [status-and-open-items.md](./status-and-open-items.md)。

这样历史蓝图、当前实现和当前完成度，就不会再混在同一个文件里。
