# CordisClaw Runtime Prototype

This repository now contains a runnable Rust prototype that implements Stage A-E from `plan.md`:

- Stage A: package contract (`children` merged object-array metadata, direct-children recursion)
- Stage B: runtime ABI contract (`cordis_plugin_api_rust_v2`, strict ABI fingerprint model)
- Stage C: loader architecture (`discover -> resolve -> instantiate`, fail-fast checks)
- Stage D: artifact architecture (prebuilt artifact index + sha256/fingerprint verification)
- Stage E: context/security (`provide/inject/dispose`, grants-based parent-chain access)

Additional runtime pieces now in code:

- Actor-style execution mailbox integrated into `execution::engine`
- Kernel self-iteration skeleton (`observe -> diagnose -> plan -> apply -> verify -> score -> promote/rollback`)
- Kernel `SafetyGate` (sensitive path changes require manual approval)
- Kernel auto-update runner (apply text patch -> verify -> rollback on failed verdict)

## Layout

- `crates/cordis-runtime`: runtime implementation + tests
- `fixtures/plugins`: nested plugin project fixtures
- `fixtures/artifacts`: prebuilt artifact fixtures + index
- `docs`: architecture and Rust file responsibility documentation

## Run

```bash
cargo run -p cordis-runtime -- fixtures
```

```bash
cargo run -p cordis-runtime -- auto-update /path/to/workspace README.md "old_text" "new_text" --quality-score=95
```

```bash
cargo run -p cordis-runtime -- shell-terminal --command="echo terminal started"
```

```bash
cargo run -p cordis-runtime -- graph-html fixtures --output=registered-nodes.html
```

```bash
cargo run -p cordis-runtime -- dag-html fixtures --output=registered-dag.html
```

`shell-terminal` 使用内置 Cordis shell，不会调用系统 `/bin/bash`、`/bin/sh` 或 `cmd.exe`。
表达式计算能力现在完全位于外部插件样例树：`fixtures/plugins/expr` 负责编排，词法/语法/计算分别位于 `fixtures/plugins/expr/{lexer,parser,evaluator}`，`evaluator` 内部再拆成 `add/sub/mul/div` 四个算子子插件。
这些插件通过 `package.metadata.cordis.children` 描述父子关系；运行时只消费 `fixtures/artifacts/index.json` 与预构建工件，不再由根 workspace 直接管理。
`expr` 顶层执行工件采用 `JSON artifact + external process`。runtime 不再硬编码 `expr`/`expr_entry`/`expression`，而是按已加载插件的 docs 与 artifact 契约动态分发命令；`Expr` 只是当前样例里的一个外部插件命令：

```bash
cargo run -p cordis-runtime -- shell-terminal --command="Expr 1 + 2 * 3"
# output: Value: 7
```

已注册节点图支持导出为 HTML：

```bash
cargo run -p cordis-runtime -- graph-html fixtures --output=registered-nodes.html
# output: graph_html written to /abs/path/registered-nodes.html
```

已注册节点的 DAG 也支持导出为 HTML。当前实现会根据 `docs/agent/interfaces.json` 中的 `input_schema` / `output_schema` 属性名推导数据边，再按拓扑层级布局：

```bash
cargo run -p cordis-runtime -- dag-html fixtures --output=registered-dag.html
# output: dag_html written to /abs/path/registered-dag.html
```

## Test

```bash
cargo test -p cordis-runtime
```
