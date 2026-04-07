# 维护指南

## 1. 测试矩阵

`crates/cordis-runtime/tests` 基本覆盖了这个原型的几条主线：

- `architecture.rs`
  - resolver / loader / grants / graph 导出 / 子插件 invoke
- `semantics.rs`
  - CPN Net / Context / Engine 语义
- `actor_executor.rs`
  - Actor 批量调度与并发上限
- `shell_plugin.rs`
  - shell 外部插件调用、REPL 和 `Expr` 命令分发
- `kernel.rs`
  - Self-Iteration Kernel 的闭环判定
- `auto_update.rs`
  - 自动更新应用、验证失败回滚、路径安全
- `tooling.rs`
  - docs 自动回写与 artifact index 自动刷新

如果你要修改某个子系统，最稳妥的方式不是先猜运行结果，而是先找到对应测试文件看“项目当前把什么行为当成契约”。

## 2. 推荐阅读顺序

建议按下面顺序进入代码：

1. [crates/cordis-plugin-sdk/src/lib.rs](../../crates/cordis-plugin-sdk/src/lib.rs)
2. [crates/cordis-runtime/src/core/models.rs](../../crates/cordis-runtime/src/core/models.rs)
3. [crates/cordis-runtime/src/plugin/package.rs](../../crates/cordis-runtime/src/plugin/package.rs)
4. [crates/cordis-runtime/src/plugin/loader.rs](../../crates/cordis-runtime/src/plugin/loader.rs)
5. [crates/cordis-runtime/src/context/mod.rs](../../crates/cordis-runtime/src/context/mod.rs)
6. [crates/cordis-runtime/src/plugin/invoke.rs](../../crates/cordis-runtime/src/plugin/invoke.rs)
7. [crates/cordis-runtime/src/service/doc_registry.rs](../../crates/cordis-runtime/src/service/doc_registry.rs)
8. [crates/cordis-runtime/src/service/graph_registry.rs](../../crates/cordis-runtime/src/service/graph_registry.rs)
9. [crates/cordis-runtime/src/execution/engine.rs](../../crates/cordis-runtime/src/execution/engine.rs)
10. [crates/cordis-runtime/src/kernel/loop.rs](../../crates/cordis-runtime/src/kernel/loop.rs)
11. `crates/cordis-runtime/tests/*.rs`
12. `fixtures/plugins/*`

## 3. 新成员最容易踩的点

- `expr` 已经不是根 workspace 成员，不要按“普通内部 crate”思路改它。
- 插件父子关系不看 Cargo 依赖树，只看 `package.metadata.cordis.children`。
- `docs/agent/interfaces.json` 是运行时输入，不能把它当可有可无的说明文件。
- 插件加载失败时，很多场景是设计上的 fail-fast，不是“为什么不自动兜底”。
- shell 插件虽然叫 shell，但它调用的是 CordisClaw builtin shell，不是系统 shell。
- `graph-html` / `net-html` 展示的是“已注册视图”，不等于真实业务执行流。
- auto-update 目前是文本补丁事务，不是 AST 级重写器。

## 4. 适合继续扩展的方向

如果后续要继续演进，这个仓库最自然的方向包括：

- 增加更多外部插件样例，验证 loader 边界。
- 丰富 `interfaces.json` 与 net 推导规则。
- 把 execution engine 接到更真实的运行入口。
- 为 Kernel 增加更真实的 patch 生成与验证器集成。
- 给 graph/doc registry 加上更稳定的服务边界，而不只是 route-style helper。

在当前阶段，更重要的不是功能数量，而是继续保持三件事：

- 契约清晰
- 边界显式
- 失败可解释
