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

## 2. 执行层：CPN Net、Router、Actor、Engine

当前 CLI 主要暴露 loader / invoke / tooling，但执行层已经作为库实现完成了原型。

### 2.1 Net 构建

[execution/net.rs](../../crates/cordis-runtime/src/execution/net.rs) 负责：

- `Place / Transition / Arc` 唯一性与引用检查。
- `JoinPolicy`（`all_of/any_of/quorum/first_success/first_completed/keyed_pair/keyed_group`）语义入口。
- Correlation key 和 token metadata（`execution_id/transition_id/logical_group/outcome`）载体。

### 2.2 Router 语义

[execution/router.rs](../../crates/cordis-runtime/src/execution/router.rs) 把子图执行包在 overlay 事务里：

- 成功：提交 overlay
- 失败 / 超时 / 取消 / 跳过：回滚 overlay

这让子图可以有类似事务边界的上下文语义。

### 2.3 Actor 与调度器

- [execution/actor.rs](../../crates/cordis-runtime/src/execution/actor.rs) 提供 mailbox 风格批量分发。
- [execution/scheduler.rs](../../crates/cordis-runtime/src/execution/scheduler.rs) 提供调度配置。

当前引擎默认吞吐优先调度，并保留 `SchedulerMode::Deterministic` 供测试/排障复现。

### 2.4 `execute_net()`

[execution/engine.rs](../../crates/cordis-runtime/src/execution/engine.rs) 把前面几层集成起来，负责：

- Net build
- keyed token 匹配与 join policy 评估
- ready queue / retry / backoff
- timeout
- late token tombstone（zombie drop）
- Router overlay commit/rollback
- metrics 汇总

它产出的 `ExecutionOutput` 包含：

- `execution_id`
- 实际执行顺序
- 每个节点最终 `NodeOutcome`
- 一组执行指标

## 3. Kernel 与自动更新链路

在当前原型里，Kernel 不再只是一次性 CLI 辅助逻辑；它现在挂在常驻 [host.rs](../../crates/cordis-runtime/src/host.rs) 的 `RuntimeHost` 上，跨 `reload` 持续保留历史和指标。

### 3.1 自迭代（Agent Loop）

自迭代已经从固定 9 阶段 Petri Net 管道升级为 open-ended agent loop。
原 `kernel/loop.rs` 和 `kernel/planner.rs`（~9200 行）已被删除，替换为：

- [host.rs](../../crates/cordis-runtime/src/host.rs)：`iterate_plugins()` — agent loop + 顺序 finalization（rebuild → stage → verify → canary → promote/rollback）
- [agent.rs](../../crates/cordis-runtime/src/agent.rs)：`AgentSession::respond()` — 统一的 tool-calling loop（最多 96 轮），代理可以自主决定每一步做什么
- [kernel/plugin_iteration.rs](../../crates/cordis-runtime/src/kernel/plugin_iteration.rs)：策略验证、回滚日志持久化、canary 回放

回退安全网有四层：panic guard、增量 journal 持久化、draft patch 保存、workspace 恢复。
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
