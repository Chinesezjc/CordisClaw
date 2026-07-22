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

### 2.3 调度器

- [execution/scheduler.rs](../../crates/cordis-runtime/src/execution/scheduler.rs) 仅承载 `SchedulerConfig` 载体（`max_parallelism` / `max_concurrency`）。历史上的 `execution/actor.rs` mailbox 已合并回 `engine.rs`（5cc7c65），`run_deterministic` 死代码已在 P2-7 清理；实际调度、批处理和 parallel key-shard 逻辑都在 [execution/engine.rs](../../crates/cordis-runtime/src/execution/engine.rs) 内。

当前引擎默认单线程；`SchedulerConfig::max_concurrency > 1` 启用 parallel key-shard 分批（P1-3/4/5/6/7/8 系列修复覆盖了 Router skip 对称、优先级排序、runner panic 转 Err、KeyedPair arity 校验、Router Timeout override 语义、eval_first_success 及时收敛）。

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
- `kernel().run_iteration()`：legacy 单文本-patch 的 `AutoUpdater` 事务（见 3.3）
- `iterate_plugins()`：**主入口**，运行 agent-loop 驱动的自迭代（agent + edit + rebuild + stage + verify + canary + promote/rollback），支持 hard rollback + journal 恢复

### 3.2 策略边界

[kernel/policy.rs](../../crates/cordis-runtime/src/kernel/policy.rs) 定义自动变更边界：

- `path_allowlist`
- `sensitive_path_prefixes`
- `require_manual_approval_for_sensitive`
- `max_diff_lines`
- `time_budget_ms`

默认策略下，`core/`、`plugin/`、`kernel/` 等敏感目录需要人工批准。

### 3.3 AutoUpdater (legacy)

[kernel/auto_update.rs](../../crates/cordis-runtime/src/kernel/auto_update.rs) 是 open-ended agent loop 之前的最小更新器，仍保留供简单单文本 patch 使用：

- 仅支持文本级 `find -> replace` 补丁。
- 所有路径必须是 workspace 内的相对路径。
- 禁止绝对路径和 `..` 路径穿越。
- P2-27: `find` 命中数 != 1 直接报错，不再 replacen(1) 静默取首个。
- P1-18: 中途 per-patch 失败，已应用的补丁一并回滚，不留脏工作树。
- 如果验证失败或 verdict 是 `Rollback`，会按备份顺序回滚。

自迭代的主入口现在是 `RuntimeHost::iterate_plugins()`（agent-loop + hard rollback + journal 持久化），见 [design-blueprint.md](./design-blueprint.md) 10 节。

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
- `config/llm_api.yaml` — 两种格式（K批）：
  - **具名 profile 表（推荐）**：顶层 `profiles: {name: {...}}`，每个 profile 是完整的 API 配置（provider/base_url/api_key_env/api_key/model/temperature/max_tokens/timeout_ms）外加可选 `fallback: <另一 profile 名>`（请求打穿后机械切换目标，L批）。`default` 必在（缺失自动补齐）；未知名引用回落 default。
  - **旧单份格式（兼容）**：整份文件是一个 API 配置，加载时自动包装为 `default` profile。
- `config/plugins/*.yaml`
  - 为各插件预留 `enabled + settings` 配置位

仓库把模板放在 `config.example/`，本地运行时目录仍是 `config/`。

### 3.5 多用户消息链路语义（J-P 批，2026-07-21）

`--runtime-only` 模式下，渠道插件（feishu/qq）经 `agent_trigger` FFI 投递 JSON envelope，单 inbox 线程串行消费。envelope 携带回复路由（source_plugin/reply_node/reply_target）与**身份**（`sender_id`、`conversation_kind`）；soul 作用域键 = `{sender_id}#{conversation_kind}`（无身份的 legacy 调用回落 session_key）。

一条消息的处理顺序：

1. **pending 重放**（M批）：`data/pending/{key}.json` 存在则前置拼接（LLM 恢复后旧消息不丢）。
2. **指令拦截**（N批）：正文以 `/` 开头 → `command_router::dispatch`，不经 LLM，经 envelope 回复通路直接返回。`/status /help /reset /soul` 内建；插件经 docs `command_name` + `command_entry` 节点注册。这是 LLM 全挂时仍可用的管理面。
3. **会话创建**（O批）：首条消息按 soul 记录的 `profile` 名选 LLM 配置（`agent_start_with`），soul 的 `persona` 作为 system prompt 第二段（base + soul overlay + plugin hints 三段式）。soul 变更只影响新会话（/reset 后生效）。
4. **发送与降级**（L批）：`agent_send_with_fallback` — 打穿后切 profile 声明的 `fallback` 重试一次；降级期每次先乐观探测原 profile；切换/恢复记 kernel issue + notify，绝不静默。
5. **彻底失败**（M批）：消息落盘 pending、渠道收到固定模板回执（不经 LLM）、notify 告警。

soul 存储：kernel 内建 `FileSoulProvider`（`data/souls/`），插件同时声明 `soul_get`+`soul_set` 节点即覆写（样例 `fixtures/plugins/soul_store`，SQLite）。写路径是 agent 工具 `set_soul`（soul_key 由 host 绑定当前会话，不可越权）。

#### 3.5.1 批处理与群聊身份语义（5.2.34 修正，2026-07-22）

inbox 一次可取到一批消息（同一渠道多条堆积）。批处理不是"整批走同一条路"，而是**逐条划分**：

- **命令 vs 普通消息逐条切分**：批内每条按正文是否 `/` 前缀分类。命令**逐条 dispatch**，各自用**自己 envelope 的 ctx**（身份 = 各自发送者，不是整批统一身份）；普通消息重组成一个 batch 送 agent。命令先于普通消息处理。
- **`/reset` 的同批语义**：`/reset` 走 `drop_session`（清 `agent_sessions` / `pending_session_actions` / `profile_fallback` 三张 map + 删磁盘快照，幂等）。同一批里 `/reset` 之后到来的普通消息进入**新 session**。
- **soul 随最近发言者刷新**：群聊里一条 session 服务多个发言者。每批 send 前，inbox 把 session 的 `soul_key` 刷成**最近发言者**的（`refresh_session_soul`），persona overlay 每轮从 `session.soul_key` 重建，因此发言者切换后 system prompt 立即换成对应的人格。**profile（LLM 端点）保持 session 起点不随刷新变**——换 profile 需 `/reset` 起新会话。
- **多 sender 批的妥协**：一批里若有多个发言者，persona 取**最后一个发言者**（last）的；该批内非 last 成员的 `set_soul` 意图也会落到 last 的 soul。这是"一 session 多用户"下的已知取舍，phase 2 若需按发言者分裂 session 再议。
- **纯命令批不碰 pending**：整批都是命令时不触发 pending spill / 重放（命令本就不经 LLM，无"打穿"可言）。

soul_key 越权防护：H2 的 `drop_session` 幂等，重复 drop 不 panic；`command_router` 的 `/soul` 在 `ctx.soul_key` 为空（无身份会话）时回"无身份"提示而非泄漏任何其他作用域的 persona。

这批配置当前主要服务于：

- RuntimeHost staging 根目录
- Kernel 质量阈值与历史长度
- 内建 Agent/Kernel 未来接入大模型 API 时的宿主侧参数
