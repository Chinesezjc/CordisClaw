# Rust 文件职责清单

本文档覆盖仓库当前全部 `.rs` 文件（排除 `target/`），说明每个文件的职责边界与关键入口。

## 1. Core 层 (`crates/cordis-runtime/src/core`)

| 文件 | 职责定位 | 关键入口 |
|---|---|---|
| `crates/cordis-runtime/src/core/models.rs` | 统一数据契约：ABI、工件、文档、执行结果等基础结构 | `AbiFingerprint`、`PluginLoadResult`、`NodeOutcome` |
| `crates/cordis-runtime/src/core/error.rs` | 统一错误模型，覆盖 discover/resolve/load/context/execution | `RuntimeError` |
| `crates/cordis-runtime/src/core/mod.rs` | Core 模块导出聚合 | `pub mod error/models` |

## 2. Plugin 层 (`crates/cordis-runtime/src/plugin`)

| 文件 | 职责定位 | 关键入口 |
|---|---|---|
| `crates/cordis-runtime/src/plugin/abi.rs` | Rust ABI 函数表契约定义（host 与插件） | `RustPluginApiV2` |
| `crates/cordis-runtime/src/plugin/artifact.rs` | 预构建工件索引读取与哈希计算 | `load_artifact_index()`、`sha256_file()` |
| `crates/cordis-runtime/src/plugin/dynamic.rs` | 动态库加载与固定符号解析 | `LoadedDylibApi::open()` |
| `crates/cordis-runtime/src/plugin/package.rs` | Phase A：按 direct-children metadata 递归发现并做 fail-fast 校验 | `PackageResolver::resolve()` |
| `crates/cordis-runtime/src/plugin/loader.rs` | Phase B：实例化、注册、required/optional 传播与禁回退策略 | `Loader::load()` |
| `crates/cordis-runtime/src/plugin/registry.rs` | 插件/节点注册中心，维护唯一性与状态 | `PluginRegistry`、`NodeRegistry` |
| `crates/cordis-runtime/src/plugin/shell.rs` | Shell 插件实现：内置 Cordis shell（非系统 shell）与 terminal 会话 | `ShellPlugin::handle()` |
| `crates/cordis-runtime/src/plugin/mod.rs` | Plugin 模块导出聚合 | `pub mod abi/artifact/...` |

## 3. Execution 层 (`crates/cordis-runtime/src/execution`)

| 文件 | 职责定位 | 关键入口 |
|---|---|---|
| `crates/cordis-runtime/src/execution/actor.rs` | Actor 执行原语：mailbox、批量派发、事件回传 | `ActorExecutor::dispatch_batch()` |
| `crates/cordis-runtime/src/execution/dag.rs` | DAG 构图语义：冲突消解、缺失输入、环检测 | `build_dag()` |
| `crates/cordis-runtime/src/execution/gate.rs` | Gate 策略评估：AllOf/AnyOf/FirstSuccess/... | `evaluate_gate()` |
| `crates/cordis-runtime/src/execution/router.rs` | Router 子图事务边界：begin/commit/rollback + 指标 | `execute_router()` |
| `crates/cordis-runtime/src/execution/scheduler.rs` | 确定性调度原型与 ready 队列规则 | `run_deterministic()` |
| `crates/cordis-runtime/src/execution/engine.rs` | 集成执行引擎：调度 + actor + retry/backoff + cancel 传播 | `execute_graph()` |
| `crates/cordis-runtime/src/execution/mod.rs` | Execution 模块导出聚合 | `pub mod actor/dag/...` |

## 4. Context 层 (`crates/cordis-runtime/src/context`)

| 文件 | 职责定位 | 关键入口 |
|---|---|---|
| `crates/cordis-runtime/src/context/mod.rs` | `provide/inject/dispose`、overlay 事务、session CAS、context 指标 | `ContextRegistry`、`ContextTxn`、`RuntimeContext::metrics()` |

## 5. Kernel 层 (`crates/cordis-runtime/src/kernel`)

| 文件 | 职责定位 | 关键入口 |
|---|---|---|
| `crates/cordis-runtime/src/kernel/policy.rs` | 自迭代策略边界：路径白名单、敏感路径人工确认（SafetyGate）、diff/时间预算 | `IterationPolicy` |
| `crates/cordis-runtime/src/kernel/evaluator.rs` | 验证结果聚合与评分判定 | `EvalHarness::evaluate()` |
| `crates/cordis-runtime/src/kernel/memory.rs` | 迭代历史记忆（问题-补丁-结果） | `ChangeMemory::record()` |
| `crates/cordis-runtime/src/kernel/loop.rs` | OpenClaw 最小闭环状态机骨架 + promote/rollback 计数指标 | `SelfIterationKernel::run_once()` |
| `crates/cordis-runtime/src/kernel/auto_update.rs` | 自动更新执行器：应用补丁、回调验证、失败回滚 | `AutoUpdater::execute()` |
| `crates/cordis-runtime/src/kernel/mod.rs` | Kernel 模块导出聚合 | `pub mod auto_update/policy/evaluator/memory/loop` |

## 6. Service 层 (`crates/cordis-runtime/src/service`)

| 文件 | 职责定位 | 关键入口 |
|---|---|---|
| `crates/cordis-runtime/src/service/doc_registry.rs` | docs 注册与查询（含 GET 路径解析） | `DocRegistry::handle_get()` |
| `crates/cordis-runtime/src/service/mod.rs` | Service 模块导出聚合 | `pub mod doc_registry` |

## 7. Crate 根与 CLI

| 文件 | 职责定位 | 关键入口 |
|---|---|---|
| `crates/cordis-runtime/src/lib.rs` | crate 对外模块导出 | `pub mod core/.../kernel` |
| `crates/cordis-runtime/src/main.rs` | 运行入口示例（加载 fixtures 并输出结果） | `main()` |

## 8. Runtime 测试 (`crates/cordis-runtime/tests`)

| 文件 | 职责定位 | 关键入口 |
|---|---|---|
| `crates/cordis-runtime/tests/architecture.rs` | 架构契约验收：discover/resolve/load、grants、required/optional、hash mismatch、Unavailable 注入行为 | `load_success_and_grants_enforced` 等 |
| `crates/cordis-runtime/tests/semantics.rs` | 语义契约验收：DAG/Gate/Context/Engine 确定性 | `dag_*`、`engine_*`、`context_*` |
| `crates/cordis-runtime/tests/actor_executor.rs` | Actor 执行批次和并发上限行为 | `actor_executor_respects_parallel_limit_and_order` |
| `crates/cordis-runtime/tests/auto_update.rs` | 自动更新行为验收：应用成功保留、验证失败回滚、路径越界拒绝 | `auto_update_*` |
| `crates/cordis-runtime/tests/shell_plugin.rs` | Shell 插件验收：启动成功、Expr 计算、非法 action | `shell_plugin_*` |
| `crates/cordis-runtime/tests/kernel.rs` | Kernel 自迭代闭环规则（含 SafetyGate 与迭代计数） | `kernel_*` |

## 9. 外部表达式聚合插件 (`crates/cordis-expr-plugin`)

| 文件 | 职责定位 | 关键入口 |
|---|---|---|
| `crates/cordis-expr-plugin/src/lib.rs` | 外部表达式聚合插件：仅委托 evaluator 并暴露稳定错误类型 | `evaluate_expression()` |
| `crates/cordis-expr-plugin/tests/eval.rs` | 外部表达式插件验收：算术优先级、括号、非法表达式 | `evaluate_*` |

## 10. 外部表达式子插件（child 形式，当前启用）

| 文件 | 职责定位 | 关键入口 |
|---|---|---|
| `crates/cordis-expr-plugin/child/lexer/src/lib.rs` | 词法子插件：把文本转成 token 流 | `lex()` |
| `crates/cordis-expr-plugin/child/lexer/tests/lexer.rs` | 词法子插件测试 | `lexes_*` / `rejects_*` |
| `crates/cordis-expr-plugin/child/parser/src/lib.rs` | 语法子插件：把文本/Token 解析成 AST | `parse_expression()` / `parse()` |
| `crates/cordis-expr-plugin/child/parser/tests/parser.rs` | 语法子插件测试 | `parses_*` / `rejects_*` |
| `crates/cordis-expr-plugin/child/evaluator/src/lib.rs` | 计算子插件：编排 parser + add/sub/mul/div 并对 AST 求值 | `evaluate_expression()` / `evaluate()` |
| `crates/cordis-expr-plugin/child/evaluator/tests/evaluator.rs` | 计算子插件测试 | `evaluates_*` / `rejects_*` |

## 10.1 Evaluator 算子子插件（当前启用）

| 文件 | 职责定位 | 关键入口 |
|---|---|---|
| `crates/cordis-expr-plugin/child/evaluator/child/add/src/lib.rs` | 加法算子子插件 | `AddPlugin::apply()` |
| `crates/cordis-expr-plugin/child/evaluator/child/add/tests/add.rs` | 加法算子子插件测试 | `add_works` |
| `crates/cordis-expr-plugin/child/evaluator/child/sub/src/lib.rs` | 减法算子子插件 | `SubPlugin::apply()` |
| `crates/cordis-expr-plugin/child/evaluator/child/sub/tests/sub.rs` | 减法算子子插件测试 | `sub_works` |
| `crates/cordis-expr-plugin/child/evaluator/child/mul/src/lib.rs` | 乘法算子子插件 | `MulPlugin::apply()` |
| `crates/cordis-expr-plugin/child/evaluator/child/mul/tests/mul.rs` | 乘法算子子插件测试 | `mul_works` |
| `crates/cordis-expr-plugin/child/evaluator/child/div/src/lib.rs` | 除法算子子插件（含除零保护） | `DivPlugin::apply()` |
| `crates/cordis-expr-plugin/child/evaluator/child/div/tests/div.rs` | 除法算子子插件测试 | `div_*` |

## 11. 插件样例工程 (`fixtures/plugins`)

| 文件 | 职责定位 | 关键入口 |
|---|---|---|
| `fixtures/plugins/root/src/lib.rs` | 顶层样例插件源码占位 | `root_plugin_marker()` |
| `fixtures/plugins/root/tests/basic.rs` | 顶层样例插件测试占位 | `root_scaffold_test()` |
| `fixtures/plugins/root/child/src/lib.rs` | 子插件样例源码占位 | `child_plugin_marker()` |
| `fixtures/plugins/root/child/tests/basic.rs` | 子插件样例测试占位 | `child_scaffold_test()` |

## 12. 推荐阅读顺序

1. `core/models.rs` + `core/error.rs`（先建立契约与错误语义）。
2. `plugin/package.rs` + `plugin/loader.rs`（发现/解析/实例化主流程）。
3. `context/mod.rs`（注入链、overlay、CAS）。
4. `execution/dag.rs` + `execution/gate.rs` + `execution/actor.rs`（执行语义骨架）。
5. `execution/engine.rs` + `execution/router.rs`（运行时集成与子图边界）。
6. `kernel/` 五文件（含自动更新执行器）。
7. `tests/*.rs`（对照验收场景）。

## 13. 覆盖声明

当前文档覆盖以下 `.rs` 文件（通过 `rg --files -g "*.rs" -g "!target/**"` 校验）：

- `crates/cordis-runtime/src/context/mod.rs`
- `crates/cordis-runtime/src/core/error.rs`
- `crates/cordis-runtime/src/core/mod.rs`
- `crates/cordis-runtime/src/core/models.rs`
- `crates/cordis-runtime/src/execution/actor.rs`
- `crates/cordis-runtime/src/execution/dag.rs`
- `crates/cordis-runtime/src/execution/engine.rs`
- `crates/cordis-runtime/src/execution/gate.rs`
- `crates/cordis-runtime/src/execution/mod.rs`
- `crates/cordis-runtime/src/execution/router.rs`
- `crates/cordis-runtime/src/execution/scheduler.rs`
- `crates/cordis-runtime/src/kernel/evaluator.rs`
- `crates/cordis-runtime/src/kernel/auto_update.rs`
- `crates/cordis-runtime/src/kernel/loop.rs`
- `crates/cordis-runtime/src/kernel/memory.rs`
- `crates/cordis-runtime/src/kernel/mod.rs`
- `crates/cordis-runtime/src/kernel/policy.rs`
- `crates/cordis-runtime/src/lib.rs`
- `crates/cordis-runtime/src/main.rs`
- `crates/cordis-runtime/src/plugin/abi.rs`
- `crates/cordis-runtime/src/plugin/artifact.rs`
- `crates/cordis-runtime/src/plugin/dynamic.rs`
- `crates/cordis-runtime/src/plugin/loader.rs`
- `crates/cordis-runtime/src/plugin/mod.rs`
- `crates/cordis-runtime/src/plugin/package.rs`
- `crates/cordis-runtime/src/plugin/registry.rs`
- `crates/cordis-runtime/src/plugin/shell.rs`
- `crates/cordis-runtime/src/service/doc_registry.rs`
- `crates/cordis-runtime/src/service/mod.rs`
- `crates/cordis-runtime/tests/actor_executor.rs`
- `crates/cordis-runtime/tests/auto_update.rs`
- `crates/cordis-runtime/tests/architecture.rs`
- `crates/cordis-runtime/tests/kernel.rs`
- `crates/cordis-runtime/tests/shell_plugin.rs`
- `crates/cordis-runtime/tests/semantics.rs`
- `crates/cordis-expr-plugin/src/lib.rs`
- `crates/cordis-expr-plugin/tests/eval.rs`
- `crates/cordis-expr-plugin/child/lexer/src/lib.rs`
- `crates/cordis-expr-plugin/child/lexer/tests/lexer.rs`
- `crates/cordis-expr-plugin/child/parser/src/lib.rs`
- `crates/cordis-expr-plugin/child/parser/tests/parser.rs`
- `crates/cordis-expr-plugin/child/evaluator/src/lib.rs`
- `crates/cordis-expr-plugin/child/evaluator/tests/evaluator.rs`
- `crates/cordis-expr-plugin/child/evaluator/child/add/src/lib.rs`
- `crates/cordis-expr-plugin/child/evaluator/child/add/tests/add.rs`
- `crates/cordis-expr-plugin/child/evaluator/child/sub/src/lib.rs`
- `crates/cordis-expr-plugin/child/evaluator/child/sub/tests/sub.rs`
- `crates/cordis-expr-plugin/child/evaluator/child/mul/src/lib.rs`
- `crates/cordis-expr-plugin/child/evaluator/child/mul/tests/mul.rs`
- `crates/cordis-expr-plugin/child/evaluator/child/div/src/lib.rs`
- `crates/cordis-expr-plugin/child/evaluator/child/div/tests/div.rs`
- `fixtures/plugins/root/child/src/lib.rs`
- `fixtures/plugins/root/child/tests/basic.rs`
- `fixtures/plugins/root/src/lib.rs`
- `fixtures/plugins/root/tests/basic.rs`
