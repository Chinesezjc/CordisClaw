# 插件与工具链

## 1. 调用路径与样例插件

### 1.1 `PluginInvoker`

[plugin/invoke.rs](../../crates/cordis-runtime/src/plugin/invoke.rs) 提供统一调用入口：

- 先通过 loader 获取完整注册状态。
- 校验插件已注册且状态为 `Loaded`。
- 校验目标节点在 docs 中存在。
- 根据 artifact 类型决定：
  - 直接调 dylib `handle()`
  - 或启动 JSON artifact 描述的外部进程

所以 `invoke` 是运行时侧的统一桥，不关心插件是 shell、expr 还是别的实现。

在当前仓库里，`PluginInvoker` 主要用于一次性调用和向后兼容；常驻运行入口已经转到 [host.rs](../../crates/cordis-runtime/src/host.rs) 的 `RuntimeHost`。

### 1.2 `shell` 插件

[fixtures/plugins/shell/src/lib.rs](../../fixtures/plugins/shell/src/lib.rs) 是一个真正的外部 dylib 插件。它的特点是：

- 对外只暴露 `shell_entry` 节点。
- 目前只支持 `start_terminal` 动作。
- 启动的是 CordisClaw 自己的 builtin shell，不会调用系统 `/bin/bash`、`/bin/sh` 或 `cmd.exe`。
- 内建命令有：
  - `help`
  - `pwd`
  - `cd`
  - `echo`
  - `whoami`
  - `env`
  - `exit`
- 对于未知命令，它会扫描已加载插件 docs 中的 `command_name` 来做外部命令路由。

因此 `Expr 1 + 2 * 3` 实际上是：

```text
shell builtin parser
  -> resolve command_name = "Expr"
  -> PluginInvoker::invoke("expr", "expr_entry", {"expression": ...})
  -> expr dylib handle()
```

### 1.3 `expr` 插件树

表达式插件树是当前最完整的外部插件样例：

```text
expr
├── lexer
├── parser
└── evaluator
    ├── add
    ├── sub
    ├── mul
    └── div
```

职责拆分如下：

- `expr`：顶层对外命令插件，暴露 `command_name = "Expr"`。
- `expr/lexer`：把表达式字符串转成 token。
- `expr/parser`：把 token 转成 AST。
- `expr/evaluator`：计算 AST，并把具体四则运算委托给算子子插件核心逻辑。
- `expr/evaluator/{add,sub,mul,div}`：最细粒度的算子插件。

这棵树有两个价值：

- 演示 runtime 如何处理多层插件树与 required 子链。
- 演示 docs 驱动的 DAG 推导可以自然得到 `lexer -> parser -> evaluator` 这条链。

### 1.4 `root` / `root/child` 样例

`root` 这组样例更偏向契约和上下文：

- 使用 JSON artifact。
- `root` 导出 `service.db`、`service.cache`。
- `root/child` 只被授权使用 `service.db`。

它不是复杂功能插件，而是用来验证 grants、exports 和 JSON artifact 路径的最小样例。

## 2. 工具链与日常开发流程

### 2.1 CLI 能力

[crates/cordis-runtime/src/main.rs](../../crates/cordis-runtime/src/main.rs) 当前暴露的 CLI 命令包括：

- `cargo run -p cordis-runtime -- fixtures`
  - 加载 fixtures 并打印插件、节点和指标
- `cargo run -p cordis-runtime -- serve [fixtures_root]`
  - 启动常驻 `RuntimeHost`
  - 支持 `plugins` / `reload` / `invoke ...` / `kernel status` / `kernel history` / `kernel apply-plan ...`
- `cargo run -p cordis-runtime -- invoke <plugin_path> <node_id> --payload-json=...`
  - 调用任意插件节点
- `cargo run -p cordis-runtime -- graph-html fixtures --output=...`
  - 导出已注册节点图 HTML
- `cargo run -p cordis-runtime -- dag-html fixtures --output=...`
  - 导出已注册 DAG HTML
- `cargo run -p cordis-runtime -- sync-plugin-docs fixtures`
  - 从 dylib `docs()` 回写 `interfaces.json`
- `cargo run -p cordis-runtime -- refresh-artifact-index fixtures`
  - 刷新工件索引中的 `sha256`
- `cargo run -p cordis-runtime -- auto-update ...`
  - 运行一次自动更新事务

### 2.2 插件工件重建入口

[scripts/rebuild-plugin-artifacts.sh](../../scripts/rebuild-plugin-artifacts.sh) 是当前统一入口，它会做五件事：

1. 构建顶层外部插件 `expr` 和 `shell`
2. 构建 `expr` 的各层 dylib 子插件
3. 把产物复制到 `fixtures/artifacts/*.so`
4. 调用 `sync-plugin-docs` 回写 `docs/agent/interfaces.json`
5. 调用 `refresh-artifact-index` 刷新索引哈希

这个脚本的意义是把“构建工件”“更新 docs”“刷新 index”三件经常忘记同步的事合成一步。

### 2.3 YAML 配置目录

仓库根目录提供了 `config.example/` 模板目录；本地运行时配置放在 gitignored 的 `config/`：

```text
config.example/
├── runtime.yaml
├── llm_api.yaml
└── plugins/
    ├── _template.yaml
    ├── expr.yaml
    └── shell.yaml
```

其中：

- `runtime.yaml` 用来配置 RuntimeHost / Kernel 的基础参数
- `llm_api.yaml` 用来配置内建 Agent/Kernel 的大模型 API
- `plugins/*.yaml` 为插件级配置预留扩展位

当前实现会在启动时自动读取本地 `config/` 里的这些 YAML；缺失时回退到内建默认值。
