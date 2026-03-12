# 运行时语义

## 1. 上下文模型与权限边界

[context/mod.rs](../../crates/cordis-runtime/src/context/mod.rs) 负责 `provide / inject / dispose` 和 overlay 事务。

### 1.1 作用域

当前支持四层作用域：

- `Global`
- `Session`
- `Request`
- `Local`（按 `plugin_path` 区分）

### 1.2 注入顺序

服务解析顺序是：

```text
Local(当前插件 -> 祖先插件中被 grants 明确允许的服务)
  -> Request
  -> Session
  -> Global
```

这意味着子插件默认不能访问父插件 Local 服务，只有父子边上显式写入 `grants` 才行。

例如当前样例里：

- `root` 导出 `service.db` 与 `service.cache`
- `root -> root/child` 只 grant `service.db`
- 所以 `root/child` 能注入 `service.db`，不能注入 `service.cache`

### 1.3 事务与一致性

上下文还带有：

- subgraph overlay：
  - `begin_subgraph`
  - `commit_overlay`
  - `rollback_overlay`
- session CAS：
  - `commit_session(session_id, expected_version)`

以及一组 metrics：

- `context_read_total`
- `context_write_total`
- `context_overlay_rollback_total`
- `session_commit_conflict_total`
- `session_commit_latency_ms`

## 2. 执行层：DAG、Gate、Router、Actor、Engine

当前 CLI 主要暴露 loader / invoke / tooling，但执行层已经作为库实现完成了原型。

### 2.1 DAG 构建

[execution/dag.rs](../../crates/cordis-runtime/src/execution/dag.rs) 负责：

- 节点唯一性检查。
- 根据 `produces` / `consumes` 建立数据边。
- 根据 `control_deps` 建立控制边。
- required input 缺失检测。
- 多 producer 冲突检测。
- 环检测。

默认策略 `DagBuildPolicy` 要求：如果一个输入类型存在多个候选 producer，必须显式绑定，否则 fail-fast。

### 2.2 Gate 语义

[execution/gate.rs](../../crates/cordis-runtime/src/execution/gate.rs) 支持：

- `AllOf`
- `AnyOf`
- `FirstSuccess`
- `FirstCompleted`
- `AtLeast(k)`

当策略需要时，Gate 还能返回“完成并取消其他分支”的决策。

### 2.3 Router 语义

[execution/router.rs](../../crates/cordis-runtime/src/execution/router.rs) 把子图执行包在 overlay 事务里：

- 成功：提交 overlay
- 失败 / 超时 / 取消 / 跳过：回滚 overlay

这让子图可以有类似事务边界的上下文语义。

### 2.4 Actor 与调度器

- [execution/actor.rs](../../crates/cordis-runtime/src/execution/actor.rs) 提供 mailbox 风格批量分发。
- [execution/scheduler.rs](../../crates/cordis-runtime/src/execution/scheduler.rs) 提供确定性 ready queue 调度。

当前 ready 队列排序遵循：

- topo level 升序
- priority 降序
- node id 升序
- retry 项优先级最后参与比较

### 2.5 `execute_graph()`

[execution/engine.rs](../../crates/cordis-runtime/src/execution/engine.rs) 把前面几层集成起来，负责：

- DAG build
- ready queue 管理
- Actor dispatch
- retry / backoff
- timeout
- cancel 传播
- Router overlay commit/rollback
- metrics 汇总

它产出的 `ExecutionOutput` 包含：

- `execution_id`
- 实际执行顺序
- 每个节点最终 `NodeOutcome`
- 一组执行指标

## 3. Kernel 与自动更新链路

在当前原型里，Kernel 不再只是一次性 CLI 辅助逻辑；它现在挂在常驻 [host.rs](../../crates/cordis-runtime/src/host.rs) 的 `RuntimeHost` 上，跨 `reload` 持续保留历史和指标。

### 3.1 Self-Iteration Kernel

[kernel/loop.rs](../../crates/cordis-runtime/src/kernel/loop.rs) 实现了一个 OpenClaw 风格的最小闭环：

```text
observe -> diagnose -> plan -> apply -> verify -> score -> safety_gate -> promote/rollback
```

它本身不负责生成补丁，而是负责在“补丁已应用、验证结果已得出”的前提下进行：

- 策略检查
- 评分
- promote / rollback 判定
- 记忆记录

`RuntimeHost` 对外提供：

- `current_snapshot()`：获取当前不可变快照
- `reload()`：重建整图并在成功后原子切换
- `kernel().status()`：查看当前 kernel / LLM 配置摘要和迭代计数
- `kernel().history()`：读取历史变更记录
- `kernel().run_iteration()`：执行一次受限 auto-update 事务

### 3.2 策略边界

[kernel/policy.rs](../../crates/cordis-runtime/src/kernel/policy.rs) 定义自动变更边界：

- `path_allowlist`
- `sensitive_path_prefixes`
- `require_manual_approval_for_sensitive`
- `max_diff_lines`
- `time_budget_ms`

默认策略下，`core/`、`plugin/`、`kernel/` 等敏感目录需要人工批准。

### 3.3 AutoUpdater

[kernel/auto_update.rs](../../crates/cordis-runtime/src/kernel/auto_update.rs) 提供一个最小可运行更新器：

- 仅支持文本级 `find -> replace` 补丁。
- 所有路径必须是 workspace 内的相对路径。
- 禁止绝对路径和 `..` 路径穿越。
- 如果验证失败或 verdict 是 `Rollback`，会按备份顺序回滚。

这说明当前 auto-update 仍是“安全边界验证原型”，不是完整代码修改系统。

### 3.4 YAML 配置入口

当前运行时会自动查找 YAML 配置目录：

- 如果 `fixtures_root` 目录名是 `fixtures`，读取其同级 `config/`
- 否则读取 `fixtures_root/config/`

当前约定文件包括：

- `config/runtime.yaml`
  - `snapshot_root`
  - `kernel.change_history_limit`
  - `kernel.min_quality_score`
- `config/llm_api.yaml`
  - `provider`
  - `base_url`
  - `api_key_env`
  - `api_key`
  - `model`
  - `temperature`
  - `max_tokens`
  - `timeout_ms`
- `config/plugins/*.yaml`
  - 为各插件预留 `enabled + settings` 配置位

仓库把模板放在 `config.example/`，本地运行时目录仍是 `config/`。

这批配置当前主要服务于：

- RuntimeHost staging 根目录
- Kernel 质量阈值与历史长度
- 内建 Agent/Kernel 未来接入大模型 API 时的宿主侧参数
