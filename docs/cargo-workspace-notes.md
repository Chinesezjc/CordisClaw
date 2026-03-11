# Cargo Workspace Notes

这份说明记录当前仓库里和 workspace 最容易混淆的边界。

## 1. 当前真实状态

根 [Cargo.toml](/root/CordisClaw/Cargo.toml) 现在只管理一个工作区成员：

- `crates/cordis-runtime`

`expr` 已经迁移到外部插件样例目录，不再属于根 workspace：

- `fixtures/plugins/expr`
- `fixtures/plugins/expr/lexer`
- `fixtures/plugins/expr/parser`
- `fixtures/plugins/expr/evaluator`
- `fixtures/plugins/expr/evaluator/{add,sub,mul,div}`

## 2. 为什么 `expr` 不再放在根 workspace

这次迁移的目标不是“把 expr 变成另一个业务 crate”，而是把它变成真正由 runtime 发现和执行的外部插件样例。

所以现在我们刻意把职责拆开：

- 根 workspace：只负责 runtime 自身代码与测试
- `fixtures/plugins/expr`：负责插件工程结构、`children` metadata、源码/测试/文档三件套
- `fixtures/artifacts`：负责预构建工件与索引

这样 `cargo test` 不会再把 `expr` 当作根项目成员来管理，runtime 对 `expr` 也没有编译期依赖。

## 3. `fixtures/plugins` 的 workspace 在控制什么

[fixtures/plugins/Cargo.toml](/root/CordisClaw/fixtures/plugins/Cargo.toml) 是插件样例自己的 workspace 根。

它的职责是：

- 给 `PackageResolver` 提供顶层插件起点
- 表达“哪些插件是顶层 member”
- 允许插件树内部保持自己的 Cargo 组织方式

当前它只列两类顶层插件：

- `root`
- `expr`

它不是根项目的主 workspace，也不会影响 `cargo test` 在仓库根目录下的默认目标集合。

## 4. 子插件关系现在怎么表达

`expr` 这棵树已经不再依赖 Cargo 子 crate 依赖链来表达父子关系。

父子关系统一写在各层的：

- `package.metadata.cordis.children`

例如：

- `expr` 声明 `lexer/parser/evaluator`
- `expr/evaluator` 声明 `add/sub/mul/div`

也就是说：

- Cargo 只负责把单个插件工程编出来
- Cordis metadata 负责描述插件树
- Loader 只按 metadata 递归发现，不做隐式全目录扫描

## 5. 运行时真正消费什么

运行时启动时消费的是：

- [fixtures/plugins](/root/CordisClaw/fixtures/plugins) 里的 manifest + docs 契约
- [fixtures/artifacts/index.json](/root/CordisClaw/fixtures/artifacts/index.json) 里的工件索引
- [fixtures/artifacts](/root/CordisClaw/fixtures/artifacts) 里的预构建 JSON / 可执行工件

对 `expr` 来说，当前执行链是：

- loader 注册 `expr` 顶层 JSON artifact
- shell 先按插件 docs + artifact 动态解析 `Expr` 命令
- `PluginInvoker` 读取 artifact 的 `execution.kind=process`
- runtime 启动 [expr_runner](/root/CordisClaw/fixtures/artifacts/expr_runner)
- shell 根据 node `input_schema` 自动组装 `{"expression":"1 + 2 * 3"}` 并通过 stdin 发给它

## 6. 实际效果

现在运行：

```bash
cargo test
```

效果是：

- 只执行根 workspace 的 runtime 测试
- 不把 `expr` 当作根成员编排
- shell 里的 `Expr` 仍然通过外部插件工件成功执行

对应验证命令：

```bash
cargo run -p cordis-runtime -- fixtures
cargo run -p cordis-runtime -- shell-terminal --command="Expr 1 + 2 * 3"
```
