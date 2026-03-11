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

`shell-terminal` 使用内置 Cordis shell，不会调用系统 `/bin/bash`、`/bin/sh` 或 `cmd.exe`。
表达式计算能力由外部插件体系提供：`crates/cordis-expr-plugin` 负责编排，词法/语法/计算分别由 `crates/cordis-expr-plugin/child/{lexer,parser,evaluator}` 三个子插件 crate 提供。`evaluator` 内部再拆成 `add/sub/mul/div` 四个算子子插件。
子插件拓扑通过各自 `Cargo.toml` 的 `package.metadata.cordis.children` 声明，`dependencies` 仅用于编译期链接。
内置 shell 支持 `Expr` 计算：

```bash
cargo run -p cordis-runtime -- shell-terminal --command="Expr 1 + 2 * 3"
# output: Value: 7
```

## Test

```bash
cargo test -p cordis-runtime
```
