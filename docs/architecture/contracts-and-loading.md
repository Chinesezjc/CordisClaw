# 契约与加载链路

## 1. 共享契约与数据模型

### 1.1 SDK：runtime 与插件共享的最小语言

[crates/cordis-plugin-sdk/src/lib.rs](../../crates/cordis-plugin-sdk/src/lib.rs) 定义了共享 ABI：

- `RustPluginApiV2`
- `PluginRequest`
- `PluginResponse`
- `PluginDocs`
- `NodeDoc`
- `AbiFingerprint`
- `export_plugin_api!`

这让 runtime 和 dylib 插件共用同一份符号表与 JSON 类型定义，避免 host/plugin 两边各维护一套结构。

### 1.2 `package.metadata.cordis`

运行时读到的插件元数据结构在 [core/models.rs](../../crates/cordis-runtime/src/core/models.rs) 中表现为 `CordisMetadata`，关键字段如下：

- `plugin_path`：规范插件路径，例如 `expr/evaluator/div`。
- `abi_kind`：当前只接受 `rust`。
- `abi_fingerprint`：严格 ABI 身份。
- `children`：直接子插件列表。
- `declared_nodes`：声明级节点列表，用于契约检查和预算统计。

父子边上的 `children` 项还携带：

- `source`：子插件目录，必须是 `./child`。
- `required`：初始化失败是否向父链传播。
- `grants`：父插件向该子插件开放的服务白名单。

### 1.3 插件 docs 契约

`PluginDocs` / `NodeDoc` 描述插件对外暴露的节点能力，最关键的字段有：

- `plugin_id`
- `plugin_path`
- `plugin_version`
- `command_name`
- `nodes[].id`
- `nodes[].input_schema`
- `nodes[].output_schema`
- `nodes[].side_effects`
- `nodes[].failure_modes`

其中：

- `command_name` 主要被 shell 插件用于命令发现和分发。
- `input_schema` / `output_schema` 的属性名会被 `GraphRegistry` 用来推导已注册 DAG。

### 1.4 工件索引

[fixtures/artifacts/index.json](../../fixtures/artifacts/index.json) 是运行时加载工件的总目录。每个条目至少包含：

- `plugin_path`
- `version`
- `abi_fingerprint`
- `artifact_path`
- `sha256`
- `built_at`

当前工件形态有两类：

- JSON artifact：例如 `root.json`、`root_child.json`
- Rust dylib artifact：例如 `shell.so`、`expr.so`、`expr_evaluator_div.so`

### 1.5 插件运行状态

loader 会把插件状态归一为：

- `PluginLoadResult::Loaded`
- `PluginLoadResult::Unavailable(reason)`

`reason` 目前包括：

- `AbiMismatch`
- `SymbolMissing`
- `InitFailed`
- `BudgetExceeded`
- `ArtifactMissing`
- `HashMismatch`
- `ContractViolation`

## 2. 插件发现与加载链路

### 2.1 发现：`PackageResolver`

[plugin/package.rs](../../crates/cordis-runtime/src/plugin/package.rs) 负责 Phase A：

- 从 `fixtures/plugins/Cargo.toml` 顶层 members 起步。
- 递归读取每个插件的 `Cargo.toml`。
- 校验 `plugin_path`、crate name 和 scaffold 完整性。
- 解析 `docs/agent/interfaces.json`。
- 根据 `children` 建出 `ResolvedPluginGraph` 和 `topo_order`。
- 做重复路径检测、循环检测和非法 child source 检测。

它不做的事情：

- 不扫描任意目录找插件。
- 不允许多级 `./a/b` child source。
- 不允许同一插件被两个不同父节点占有。

### 2.2 加载：`Loader`

[plugin/loader.rs](../../crates/cordis-runtime/src/plugin/loader.rs) 负责 Phase B：

- 校验总插件数和节点数是否超出 `LoaderBudget`。
- 读取 artifact index。
- 按拓扑顺序逐个加载插件。
- 对每个插件做：
  - 父插件状态检查
  - ABI kind 校验
  - index entry 存在性校验
  - metadata 与 index 的 ABI 指纹比对
  - artifact 文件存在性与 `sha256` 校验
  - dylib 固定符号加载或 JSON artifact 反序列化
  - runtime 导出的 docs / 指纹再次校验
- 把结果写入 `PluginRegistry`、`NodeRegistry` 和 `RuntimeContext`

required 子插件失败后，`Loader::propagate_parent_failure()` 会沿着 required 父链向上标记 `InitFailed`，直到遇到第一个非 required 父边为止。

### 2.3 两种 artifact 路径

当前运行时支持两种实例化方式：

- dylib：
  - 通过 [plugin/dynamic.rs](../../crates/cordis-runtime/src/plugin/dynamic.rs) 打开动态库。
  - 读取固定符号 `cordis_plugin_api_rust_v2`。
  - 通过 `abi_fingerprint()`、`docs()`、`handle()` 与插件交互。
- JSON artifact：
  - 直接读入 `PluginArtifact`。
  - 可携带 `exports`，也可携带 `execution = process` 描述。

当前样例里：

- `root` / `root/child` 走 JSON artifact。
- `shell` 与整棵 `expr` 子树走 dylib artifact。

## 3. 注册表、文档服务与图服务

### 3.1 `PluginRegistry` / `NodeRegistry`

[plugin/registry.rs](../../crates/cordis-runtime/src/plugin/registry.rs) 是 runtime 的基础索引：

- `PluginRegistry` 存插件级状态、父路径、required 标记、grants、docs、artifact_path。
- `NodeRegistry` 以 `plugin_path::node_id` 的 FQN 维护节点唯一性。

注册表是后续文档查询、图导出和调用分发的共同输入。

### 3.2 `DocRegistry`

[service/doc_registry.rs](../../crates/cordis-runtime/src/service/doc_registry.rs) 提供 machine-readable 文档查询：

- `GET /plugins/{plugin_path}/docs`
- `GET /plugins/{plugin_path}/nodes/{node_id}/docs`

这里的“GET”是 route-style helper，不是 HTTP server；但它已经把路由约定稳定下来了。

### 3.3 `GraphRegistry`

[service/graph_registry.rs](../../crates/cordis-runtime/src/service/graph_registry.rs) 从注册表生成两类图：

- 已注册节点图：
  - 关注插件树与节点归属关系
  - 可导出 JSON 和自包含 HTML
- 已注册 DAG：
  - 从节点 docs 的 `input_schema` / `output_schema` 推导数据边
  - 也可导出 JSON 和自包含 HTML

注意这里的 DAG 是“文档推导的注册 DAG”，不是执行引擎真实运行时传入的任意 DAG。当前推导规则比较保守，主要基于 schema 属性名匹配。
