# 系统概览

## 1. 项目定位

`CordisClaw` 当前是一个 Rust 运行时原型，核心方向不是“做一个普通业务 crate”，而是验证一套面向插件树的运行时体系：

- 插件发现是显式的，依赖 `package.metadata.cordis.children`，不做隐式目录扫描。
- 插件加载是严格的，依赖 ABI 指纹、工件索引和 `sha256` 校验。
- 插件对外能力是文档驱动的，运行时通过 `docs/agent/interfaces.json` 建立节点注册、文档查询和图可视化。
- 插件之间的服务访问不是默认开放的，而是通过父子边上的 `grants` 精确授权。
- 执行层不是“直接调函数”，而是围绕 CPN Net、Router、Actor mailbox 与受控调度组织。
- 内核层保留了一个最小的自迭代闭环，用来验证自动修改、验证、评分、回滚这条链。
- 宿主层现在支持常驻 `RuntimeHost`，通过原子快照切换完成显式插件热插拔。

当前运行时实现可以按 Stage A-E 这五段来理解：

- Stage A：插件工程发现与元数据契约。
- Stage B：运行时 ABI 契约与指纹一致性。
- Stage C：`discover -> resolve -> instantiate` 的 loader 架构。
- Stage D：预构建工件索引与哈希校验。
- Stage E：上下文注入、作用域与授权链路。

在这五个阶段之上，仓库还额外实现了：

- `RuntimeHost` 常驻宿主与 `serve` 入口。
- 执行引擎原型。
- 图注册与 HTML 可视化。
- 插件 docs 自动回写与工件索引刷新工具。
- Kernel 自迭代骨架与 auto-update 原型。

## 2. 当前仓库的真实边界

当前根 workspace 只管理运行时本身：

- `crates/cordis-plugin-sdk`
- `crates/cordis-runtime`

外部插件样例不属于根 workspace，它们位于 `fixtures/plugins` 下，由运行时在“外部插件”的语义下发现和加载：

- `root` / `root/child`：JSON 工件样例，主要用于校验 metadata、exports 和 grants。
- `shell`：Rust dylib 外部插件，提供内建 shell 与命令路由。
- `expr`：Rust dylib 外部插件树，负责表达式解析与计算。
- `qq`：OneBot v11 QQ 适配器，HTTP 事件接收 + 消息发送。
- `gacha`：原神抽卡模拟器，保底概率模型 + JSON 文件持久化。
- `web`：网页搜索（DeepSeek Anthropic API）+ 网页抓取。
- `git`：Git 操作（diff/log/status/commit/amend）。
- `vision`：图片识别（OCR + 描述）。
- `filesystem`：文件系统操作。
- `time`：时间查询。

这意味着：

- 根 `cargo test` 主要覆盖 runtime/sdk。
- `expr` 不再是“根项目直接依赖的库”，而是“被 runtime 发现和调用的外部插件树”。
- 运行时真正消费的是 `fixtures/plugins` 中的 manifest/docs，加上 `fixtures/artifacts` 中按需生成的工件与索引。

更细的 workspace 说明见 [../cargo-workspace-notes.md](../cargo-workspace-notes.md)。

## 3. 仓库布局

```text
CordisClaw/
├── crates/
│   ├── cordis-plugin-sdk/      # 共享 ABI / docs helper / 导出宏
│   └── cordis-runtime/         # runtime、loader、execution、kernel、service
├── config.example/             # 版本库内的 YAML 模板
├── config/                     # 本地 runtime/kernel/LLM/plugin YAML 配置（gitignored）
├── data/                       # 运行时持久化数据（gitignored）：插件状态、退出快照
├── docs/                       # 架构与维护文档
├── fixtures/
│   ├── plugins/                # 外部插件样例工程
│   │   ├── root/               # JSON artifact 样例父插件
│   │   ├── shell/              # 外部 dylib shell 插件
│   │   └── expr/               # 外部 dylib 表达式插件树
│   └── artifacts/              # 本地生成的 JSON/.so 工件与 index.json（gitignored）
```

按职责可以把代码再切成六层：

| 层 | 目录 | 主要职责 |
|---|---|---|
| SDK | `crates/cordis-plugin-sdk` | 给 runtime 和 dylib 插件提供同一份 ABI / docs 类型定义 |
| Core | `crates/cordis-runtime/src/core` | 统一错误模型和基础数据契约 |
| Plugin | `crates/cordis-runtime/src/plugin` | 发现、解析、工件校验、动态加载、调用、注册 |
| Context | `crates/cordis-runtime/src/context` | `provide/inject/dispose`、overlay、session CAS |
| Execution | `crates/cordis-runtime/src/execution` | CPN Net、Router、Actor、Scheduler、Engine |
| Kernel / Service | `crates/cordis-runtime/src/kernel`、`src/service` | 自迭代闭环、文档服务、图服务 |
| Host / Config | `crates/cordis-runtime/src/host.rs`、`src/config.rs`、`config.example/`、`config/` | 常驻宿主、快照切换、Kernel 配置模板与本地 YAML 入口 |

全部 Rust 文件的职责清单见 [../rs-files-responsibility.md](../rs-files-responsibility.md)。

## 4. 核心设计原则

### 4.1 显式优于隐式

- 只从 `fixtures/plugins/Cargo.toml` 的顶层 members 起步。
- 父子关系只看 `package.metadata.cordis.children`。
- 子插件 `source` 必须是 `./child` 这种“直接子目录”。
- 不允许 `../`、绝对路径或跨层级隐式发现。

### 4.2 契约优于约定俗成

一个插件想被 runtime 接受，需要同时满足：

- `Cargo.toml` 中存在 `package.metadata.cordis`。
- `plugin_path` 与目录推导值一致。
- crate 名与 `plugin_path` 归一化结果一致。
- 插件 scaffold 完整存在：
  - `src/`
  - `tests/`
  - `docs/`
  - `docs/agent/interfaces.json`
  - `docs/human/overview.md`
- `interfaces.json` 能被解析成 `PluginDocs`，且 `docs.plugin_path` 与插件路径一致。

### 4.3 Fail-fast 优于静默容错

以下情况会直接阻断加载或将插件标记为 `Unavailable`：

- ABI 指纹不匹配。
- 工件缺失。
- 工件哈希不匹配。
- dylib 固定符号缺失。
- docs / artifact 内部 `plugin_path` 不匹配。
- 必需子插件初始化失败。

当前实现刻意不做“跨类型 fallback”。例如 dylib 加载失败，不会自动回退到另一种 artifact 形式。

### 4.4 文档是运行时输入，不只是说明文字

`docs/agent/interfaces.json` 不只是给人看的说明，它直接参与：

- 节点注册。
- 文档路由查询。
- Shell 命令发现。
- 注册图导出。
- 已注册 net 推导。

## 5. 主流程总览

可以把一次 runtime 启动理解为下面这条链：

```text
fixtures/plugins/Cargo.toml
  -> PackageResolver::resolve()
  -> ResolvedPluginGraph
  -> load_artifact_index(index.json)
  -> Loader::load_with_staging_root()
  -> staged snapshot artifacts
  -> PluginRegistry + NodeRegistry + RuntimeContext
  -> DocRegistry + GraphRegistry
  -> RuntimeHost / PluginInvoker / graph-html / net-html / tests
```

更细一点：

1. `PackageResolver` 从插件 workspace 顶层成员出发，递归读取每个插件的 `Cargo.toml` 和 `docs/agent/interfaces.json`。
2. resolver 根据 `children` 建出插件树、拓扑顺序、父子边和 grants 信息。
3. `Loader` 读取 `fixtures/artifacts/index.json`，逐个插件做预算检查、ABI 指纹检查、哈希检查和 artifact 实例化。
4. `RuntimeHost` 会把候选工件复制到 snapshot 专属 staging 目录，避免旧请求被新工件覆盖。
5. 成功加载的插件进入 `PluginRegistry`，其节点进入 `NodeRegistry`，上下文权限映射进入 `RuntimeContext`。
6. `DocRegistry` 从已注册插件收集 docs，`GraphRegistry` 从注册表派生插件图和节点 net。
7. CLI 或测试再基于这些结果做 invoke、显式 `reload`、导图、上下文验证或执行语义验证。
