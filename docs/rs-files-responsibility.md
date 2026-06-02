# Rust 文件职责清单

本文档覆盖仓库当前全部 `.rs` 文件（排除 `target/`），说明每个文件的职责边界与关键入口。

## 1. Shared SDK (`crates/cordis-plugin-sdk`)

| 文件 | 职责定位 | 关键入口 |
|---|---|---|
| `crates/cordis-plugin-sdk/src/lib.rs` | shell/expr 等外部 dylib 插件共用的 Rust ABI、docs helper 与导出宏 | `RustPluginApiV2`、`plugin_docs()`、`node_doc()`、`export_plugin_api!` |
| `crates/cordis-plugin-sdk/src/workflow.rs` | 非宏 async workflow 作者接口：受控 await 原语、wait 句柄、runtime 提交/轮询/取消边界 | `WorkflowRuntime`、`WorkflowSession`、`WaitSpec`、`WaitFuture` |

## 2. Core 层 (`crates/cordis-runtime/src/core`)

| 文件 | 职责定位 | 关键入口 |
|---|---|---|
| `crates/cordis-runtime/src/core/models.rs` | 统一数据契约：重导出共享 ABI/docs 类型，并补 runtime 专属工件、执行结果与加载状态结构 | `AbiFingerprint`、`PluginLoadResult`、`NodeOutcome` |
| `crates/cordis-runtime/src/core/error.rs` | 统一错误模型，覆盖 discover/resolve/load/context/execution | `RuntimeError` |
| `crates/cordis-runtime/src/core/mod.rs` | Core 模块导出聚合 | `pub mod error/models` |
| `crates/cordis-runtime/src/config.rs` | 运行时 YAML 配置入口：加载 `runtime.yaml`、`llm_api.yaml` 和 `plugins/*.yaml` | `RuntimeConfig::load()` |

## 3. Plugin 层 (`crates/cordis-runtime/src/plugin`)

| 文件 | 职责定位 | 关键入口 |
|---|---|---|
| `crates/cordis-runtime/src/plugin/abi.rs` | runtime 侧 ABI 聚合：重导出共享函数表契约，并保留 host 本地元数据/trait | `RustPluginApiV2` |
| `crates/cordis-runtime/src/plugin/artifact.rs` | 预构建工件索引读取与哈希计算 | `load_artifact_index()`、`sha256_file()` |
| `crates/cordis-runtime/src/plugin/dynamic.rs` | 动态库加载与固定符号解析 | `LoadedDylibApi::open()` |
| `crates/cordis-runtime/src/plugin/package.rs` | Phase A：按 direct-children metadata 递归发现并做 fail-fast 校验 | `PackageResolver::resolve()` |
| `crates/cordis-runtime/src/plugin/loader.rs` | Phase B：实例化、注册、required/optional 传播与禁回退策略；shell 与整棵 expr 子树现在都走 dylib 路径 | `Loader::load()` |
| `crates/cordis-runtime/src/plugin/invoke.rs` | 运行时插件调用桥：按统一入口执行 dylib 或外部进程插件 | `PluginInvoker::invoke()` |
| `crates/cordis-runtime/src/plugin/tooling.rs` | 工具化命令：从 dylib `docs()` 回写 `interfaces.json`，并刷新 artifact index 的哈希 | `sync_plugin_docs()`、`refresh_artifact_index()` |
| `crates/cordis-runtime/src/plugin/registry.rs` | 插件/节点注册中心，维护唯一性与状态 | `PluginRegistry`、`NodeRegistry` |
| `crates/cordis-runtime/src/plugin/mod.rs` | Plugin 模块导出聚合 | `pub mod abi/artifact/...` |

## 4. Execution 层 (`crates/cordis-runtime/src/execution`)

| 文件 | 职责定位 | 关键入口 |
|---|---|---|
| `crates/cordis-runtime/src/execution/actor.rs` | Actor 执行原语：mailbox、批量派发、事件回传 | `ActorExecutor::dispatch_batch()` |
| `crates/cordis-runtime/src/execution/net.rs` | CPN Net 模型与校验：Place/Transition/Arc、join policy、token/correlation key 载体 | `build_petri_net()` |
| `crates/cordis-runtime/src/execution/gate.rs` | 运行策略配置（retry/backoff/timeout） | `RunPolicy`、`BackoffPolicy` |
| `crates/cordis-runtime/src/execution/router.rs` | Router 子图事务边界：begin/commit/rollback + 指标 | `execute_router()` |
| `crates/cordis-runtime/src/execution/scheduler.rs` | 确定性调度原型与 ready 队列规则 | `run_deterministic()` |
| `crates/cordis-runtime/src/execution/engine.rs` | 集成执行引擎：调度 + actor + retry/backoff + cancel 传播 | `execute_net()` |
| `crates/cordis-runtime/src/execution/mod.rs` | Execution 模块导出聚合 | `pub mod actor/net/...` |

## 5. Context 层 (`crates/cordis-runtime/src/context`)

| 文件 | 职责定位 | 关键入口 |
|---|---|---|
| `crates/cordis-runtime/src/context/mod.rs` | `provide/inject/dispose`、overlay 事务、session CAS、context 指标，以及 `Service` trait + `ServiceRegistry` 后台服务生命周期管理 | `ContextRegistry`、`ContextTxn`、`Service`、`ServiceRegistry::start_service()` |

## 6. Kernel 层 (`crates/cordis-runtime/src/kernel`)

| 文件 | 职责定位 | 关键入口 |
|---|---|---|
| `crates/cordis-runtime/src/kernel/policy.rs` | 自迭代策略边界：路径白名单、敏感路径人工确认、diff 预算 | `IterationPolicy` |
| `crates/cordis-runtime/src/kernel/evaluator.rs` | 验证结果聚合与评分判定 | `EvalHarness::evaluate()` |
| `crates/cordis-runtime/src/kernel/memory.rs` | 迭代历史记忆（问题-补丁-结果） | `ChangeMemory::record()` |
| `crates/cordis-runtime/src/kernel/auto_update.rs` | 自动更新执行器：应用补丁、回调验证、失败回滚 | `AutoUpdater::execute()` |
| `crates/cordis-runtime/src/kernel/plugin_iteration.rs` | 插件迭代：策略校验、编辑执行器、回滚日志、canary 判定 | `PluginEditExecutor`、`PluginEditRollback`、`CanaryReport` |
| `crates/cordis-runtime/src/kernel/verifier.rs` | 验证 pipeline：static_check / tests / safety stage、shell/plugin verifier | `CommandVerifier`、`VerificationProfile` |
| `crates/cordis-runtime/src/kernel/mod.rs` | Kernel 模块导出聚合 | `pub mod auto_update/plugin_iteration/verifier/...` |

> **已删除**: `kernel/loop.rs`（9 阶段 Petri Net 替换为 agent loop）、`kernel/planner.rs`（独立 LLM 路径合并到 AgentSession）

## 6b. Agent 层 (`crates/cordis-runtime/src/agent.rs`)

| 职责定位 | 关键入口 |
|---|---|
| 统一的 LLM agent 会话：SSE 流解析、tool-calling loop（最多 96 轮）、重试/超时管理、消息历史；两个后端：`RuntimeShellAgentBackend`（15 个交互式工具）和 `PluginIterationAgentBackend`（14 个 plugin-editing 工具） | `AgentSession::respond()`、`ShellAgentSession`、`AgentToolHost` trait |

## 7. Service 层 (`crates/cordis-runtime/src/service`)

| 文件 | 职责定位 | 关键入口 |
|---|---|---|
| `crates/cordis-runtime/src/service/doc_registry.rs` | docs 注册与查询（含 GET 路径解析） | `DocRegistry::handle_get()` |
| `crates/cordis-runtime/src/service/graph_registry.rs` | 已注册插件/节点图与推导 net 服务：输出 JSON 图模型与自包含 HTML 可视化 | `GraphRegistry::render_registered_nodes_html()`、`GraphRegistry::render_registered_net_html()` |
| `crates/cordis-runtime/src/service/mod.rs` | Service 模块导出聚合 | `pub mod doc_registry/graph_registry` |

## 8. Crate 根与 CLI

| 文件 | 职责定位 | 关键入口 |
|---|---|---|
| `crates/cordis-runtime/src/lib.rs` | crate 对外模块导出 | `pub mod core/.../kernel` |
| `crates/cordis-runtime/src/host.rs` | 常驻宿主：持有当前快照、执行原子 `reload`、保留 kernel 状态并清理 retired snapshots | `RuntimeHost::boot()`、`RuntimeHost::reload()` |
| `crates/cordis-runtime/src/main.rs` | 运行入口示例（加载 fixtures、`serve`、通用 invoke、导出图 HTML、运行 auto-update） | `main()` |

## 9. Runtime 测试 (`crates/cordis-runtime/tests`)

| 文件 | 职责定位 | 关键入口 |
|---|---|---|
| `crates/cordis-runtime/tests/architecture.rs` | 架构契约验收：discover/resolve/load、grants、required/optional、hash mismatch、Unavailable 注入行为 | `load_success_and_grants_enforced` 等 |
| `crates/cordis-runtime/tests/semantics.rs` | 语义契约验收：CPN Net/Context/Engine（join policy、late token、deterministic mode） | `petri_net_*`、`engine_*`、`context_*` |
| `crates/cordis-runtime/tests/actor_executor.rs` | Actor 执行批次和并发上限行为 | `actor_executor_respects_parallel_limit_and_order` |
| `crates/cordis-runtime/tests/auto_update.rs` | 自动更新行为验收：应用成功保留、验证失败回滚、路径越界拒绝 | `auto_update_*` |
| `crates/cordis-runtime/tests/shell_plugin.rs` | 外部 shell 插件验收：loader 注册、generic invoke、交互 REPL、Expr 分发 | `shell_plugin_*` |
| `crates/cordis-runtime/tests/tooling.rs` | 工具链验收：自动回写 `interfaces.json` 与自动刷新 artifact index 哈希 | `sync_plugin_docs_*`、`refresh_artifact_index_*` |
| `crates/cordis-runtime/tests/runtime_host.rs` | RuntimeHost 验收：boot/invoke、reload、旧快照隔离、agent 迭代、rollback、canary、serve CLI、YAML config | `runtime_host_*`、`serve_mode_*` |

> **已删除**: `tests/kernel.rs`（测试已删除的 SelfIterationKernel）、`tests/llm_auto_update.rs`（测试已删除的 LlmPatchPlanner）

## 10. 外部表达式聚合插件 (`fixtures/plugins/expr`)

| 文件 | 职责定位 | 关键入口 |
|---|---|---|
| `fixtures/plugins/expr/src/lib.rs` | 外部表达式顶层 dylib：导出 ABI 符号、声明 `Expr` 命令并委托 evaluator core 执行 | `cordis_plugin_api_rust_v2`、`evaluate_expression()` |
| `fixtures/plugins/expr/tests/eval.rs` | 外部表达式插件验收：算术优先级、括号、非法表达式 | `evaluate_*` |
| `fixtures/plugins/expr/lexer/src/lib.rs` | 词法子插件 dylib 包装层：导出 ABI，并把表达式文本转换成 token 流 | `cordis_plugin_api_rust_v2`、`lex()` |
| `fixtures/plugins/expr/lexer/src/core.rs` | 词法核心逻辑：供 lexer dylib、自上层 parser/evaluator 复用 | `Token`、`TokenKind`、`lex()` |
| `fixtures/plugins/expr/parser/src/lib.rs` | 语法子插件 dylib 包装层：导出 ABI，并把 token 流转换成 AST | `cordis_plugin_api_rust_v2`、`parse()` |
| `fixtures/plugins/expr/parser/src/core.rs` | 语法核心逻辑：供 parser dylib、自上层 evaluator 复用 | `ExprAst`、`parse_expression()`、`parse()` |
| `fixtures/plugins/expr/evaluator/src/lib.rs` | 计算子插件 dylib 包装层：导出 ABI，并把 AST 计算成数值 | `cordis_plugin_api_rust_v2`、`evaluate()` |
| `fixtures/plugins/expr/evaluator/src/core.rs` | 计算核心逻辑：供 evaluator dylib 和顶层 expr 复用 | `evaluate_expression()`、`evaluate()` |

## 11. 外部表达式实现插件树（当前启用）

| 文件 | 职责定位 | 关键入口 |
|---|---|---|
| `fixtures/plugins/expr/lexer/tests/lexer.rs` | 词法实现测试 | `lexes_*` / `rejects_*` |
| `fixtures/plugins/expr/parser/tests/parser.rs` | 语法实现测试 | `parses_*` / `rejects_*` |
| `fixtures/plugins/expr/evaluator/tests/evaluator.rs` | 计算实现测试 | `evaluates_*` / `rejects_*` |

## 11.1 Evaluator 算子子插件（当前启用）

| 文件 | 职责定位 | 关键入口 |
|---|---|---|
| `fixtures/plugins/expr/evaluator/add/src/lib.rs` | 加法算子 dylib 包装层 | `cordis_plugin_api_rust_v2`、`apply()` |
| `fixtures/plugins/expr/evaluator/add/src/core.rs` | 加法算子核心逻辑 | `AddPlugin::apply()` |
| `fixtures/plugins/expr/evaluator/add/tests/add.rs` | 加法算子实现测试 | `add_works` |
| `fixtures/plugins/expr/evaluator/sub/src/lib.rs` | 减法算子 dylib 包装层 | `cordis_plugin_api_rust_v2`、`apply()` |
| `fixtures/plugins/expr/evaluator/sub/src/core.rs` | 减法算子核心逻辑 | `SubPlugin::apply()` |
| `fixtures/plugins/expr/evaluator/sub/tests/sub.rs` | 减法算子实现测试 | `sub_works` |
| `fixtures/plugins/expr/evaluator/mul/src/lib.rs` | 乘法算子 dylib 包装层 | `cordis_plugin_api_rust_v2`、`apply()` |
| `fixtures/plugins/expr/evaluator/mul/src/core.rs` | 乘法算子核心逻辑 | `MulPlugin::apply()` |
| `fixtures/plugins/expr/evaluator/mul/tests/mul.rs` | 乘法算子实现测试 | `mul_works` |
| `fixtures/plugins/expr/evaluator/div/src/lib.rs` | 除法算子 dylib 包装层（含除零保护） | `cordis_plugin_api_rust_v2`、`apply()` |
| `fixtures/plugins/expr/evaluator/div/src/core.rs` | 除法算子核心逻辑（含除零保护） | `DivPlugin::apply()` |
| `fixtures/plugins/expr/evaluator/div/tests/div.rs` | 除法算子实现测试 | `div_*` |

## 11b. Evaluator 算子子插件（取模、幂）

| 文件 | 职责定位 | 关键入口 |
|---|---|---|
| `fixtures/plugins/expr/evaluator/modulo/src/lib.rs` | 取模算子 dylib 包装层（含除零保护） | `cordis_plugin_api_rust_v2`、`apply()` |
| `fixtures/plugins/expr/evaluator/modulo/src/core.rs` | 取模算子核心逻辑 | `ModPlugin::apply()` |
| `fixtures/plugins/expr/evaluator/modulo/tests/modulo.rs` | 取模算子实现测试 | `modulo_works`、`modulo_rejects_zero` |
| `fixtures/plugins/expr/evaluator/pow/src/lib.rs` | 幂算子 dylib 包装层 | `cordis_plugin_api_rust_v2`、`apply()` |
| `fixtures/plugins/expr/evaluator/pow/src/core.rs` | 幂算子核心逻辑 | `PowPlugin::apply()` |
| `fixtures/plugins/expr/evaluator/pow/tests/pow.rs` | 幂算子实现测试 | `pow_*` |

## 12. 外部 Shell Dylib 插件 (`fixtures/plugins/shell`)

| 文件 | 职责定位 | 关键入口 |
|---|---|---|
| `fixtures/plugins/shell/src/lib.rs` | 外部 shell dylib 插件：通过共享 SDK 导出 Rust ABI，承接 REPL、脚本执行和基于 `command_name` 的外部命令分发 | `cordis_plugin_api_rust_v2` |
| `fixtures/plugins/shell/tests/basic.rs` | shell 插件工程测试占位 | `shell_scaffold_test()` |

## 13. QQ Adapter 插件 (`fixtures/plugins/qq`)

| 文件 | 职责定位 | 关键入口 |
|---|---|---|
| `fixtures/plugins/qq/src/lib.rs` | QQ 适配器 dylib：OneBot v11 协议 HTTP 客户端，支持 configure/send/status/call | `onebot_call()`、`parse_target()` |
| `fixtures/plugins/qq/tests/test_parse.rs` | QQ 适配器测试：target 字符串解析 | `test_parse_target` |

## 14. 插件样例工程 (`fixtures/plugins`)

| 文件 | 职责定位 | 关键入口 |
|---|---|---|
| `fixtures/plugins/root/src/lib.rs` | 顶层样例插件源码占位 | `root_plugin_marker()` |
| `fixtures/plugins/root/tests/basic.rs` | 顶层样例插件测试占位 | `root_scaffold_test()` |
| `fixtures/plugins/root/child/src/lib.rs` | 子插件样例源码占位 | `child_plugin_marker()` |
| `fixtures/plugins/root/child/tests/basic.rs` | 子插件样例测试占位 | `child_scaffold_test()` |

## 15. 推荐阅读顺序

1. `crates/cordis-plugin-sdk/src/lib.rs` + `crates/cordis-plugin-sdk/src/workflow.rs`（共享 ABI / docs 契约和 workflow 接口）。
2. `core/models.rs` + `core/error.rs`（runtime 专属契约与错误语义）。
3. `plugin/package.rs` + `plugin/loader.rs`（发现/解析/实例化主流程）。
4. `context/mod.rs`（注入链、overlay、CAS、Service 生命周期）。
5. `agent.rs`（LLM agent 会话、工具执行、流式输出）。
6. `host.rs`（常驻宿主、iterate_plugins、自迭代 agent loop、回退安全网）。
7. `execution/net.rs` + `execution/gate.rs` + `execution/actor.rs`（执行语义骨架）。
8. `execution/engine.rs` + `execution/router.rs`（CPN 运行时集成与子图边界）。
9. `kernel/` 各文件（策略、评估、记忆、自动更新、插件迭代、验证器）。
10. `tests/*.rs`（对照验收场景）。

## 16. 覆盖声明
