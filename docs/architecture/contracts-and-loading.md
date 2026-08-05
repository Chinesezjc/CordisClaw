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
- `workflow`（`WorkflowRuntime`、`WaitHandle`、`WaitSpec` 等非宏 async workflow 原语）

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
- `input_schema` / `output_schema` 的属性名会被 `GraphRegistry` 用来推导已注册 net。

### 1.4 工件索引

`fixtures/artifacts/index.json` 是运行时加载工件的总目录。它由源码生成，本地缺失或过期时会被 tooling 重建。每个条目至少包含：

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
  - dylib：`target_triple` 与宿主平台预检（不匹配 → `AbiMismatch`，不尝试 dlopen）
  - dylib 固定符号加载（失败 → `SymbolMissing`，不回落 cached docs）或 JSON artifact 反序列化
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
- 已注册 net：
  - 从节点 docs 的 `input_schema` / `output_schema` 推导数据边
  - 也可导出 JSON 和自包含 HTML

注意这里的 net 是“文档推导的注册 net”，不是执行引擎真实运行时传入的任意 net。当前推导规则比较保守，主要基于 schema 属性名匹配。

## 4. 约定能力节点（capability-node）覆写契约（O/P 批，2026-07-21）

节点 FQN 冲突是 fail-fast 的（无同名覆写机制），因此"插件覆写 kernel 默认实现"走**约定 node_id** 模式：kernel 在取用某能力时扫描 registry，找到声明了约定节点的已加载插件就 invoke 它，否则用内建默认。每次取用时解析，reload 自动切换。

当前已定义的能力契约：

### 4.1 Soul 存储（`soul_get` + `soul_set`，成对出现才生效）

- `soul_get`
  - 入参 payload：`{ soul_key: string, data_dir: string }`（`data_dir` 由 kernel 注入，插件应优先使用它定位存储，勿依赖 cwd/env）
  - 出参：`{ ok: bool, soul: {persona, profile, updated_at_ms, updated_by} | null }`
- `soul_set`
  - 入参 payload：`{ soul_key: string, soul: {…}, data_dir: string }`
  - 出参：`{ ok: bool }`

kernel 默认实现：`FileSoulProvider`（`data/souls/{sanitized_key}.json`）。覆写样例：`fixtures/plugins/soul_store`（rusqlite bundled，`data/souls.db`）。

### 4.3 LLM provider（`llm_complete`，2026-08-05）

声明 `llm_complete` 的已加载插件即成为 LLM 传输层。**这是唯一一个 kernel 不留内建
默认的能力**：没有 provider 插件时 `agent_send` 返回 `NoLlmProvider`，而 boot / REPL /
指令路由（`/status`、`/help`）照常工作。理由见 CLAUDE.md 判断标准里的"刻意例外"。

- 入参 payload：
  - `body`: object — **供应商请求体，由 kernel 构造后原样透传**（消息拼装、工具规格、
    温度/长度上限都在 kernel 侧完成）。插件负责把它送出去。
  - `transport`: `{ base_url, api_key_env, organization?, project?, timeout_ms,
    stream_timeout_secs }` — 端点与超时。**不含明文 api_key**：该结构体会被序列化进
    调用 payload，带上密钥等于扩大日志与崩溃快照的泄露面；插件按 `api_key_env` 在
    请求时自行从进程环境读取。端点路径（如 `/chat/completions`）由插件按 `base_url`
    自己拼——kernel 不知道任何供应商专有的 URL 形状。
  - `sink_addr?` / `sink_key?`: string — token 流式旁路，见下。
- 出参：`{ ok: true, message: LlmMessage, response_id?, finish_reason? }`
  或 `{ ok: false, error: string }`。
  `LlmMessage.tool_calls` **必须结构化返回**——agent 循环在本轮中途就要据此分派工具，
  只回文本的 provider 驱动不了工具调用。

**token 流式旁路**：`sink_addr` 存在时，插件可边读上游边把增量写到该 TCP 回环地址，
行分隔 JSON，首帧必须是 `{"t":"key","key":"<sink_key>"}` 握手，随后
`{"t":"reasoning","d":"…"}` / `{"t":"content","d":"…"}`。**这是纯旁路**：连不上、
写失败、或整个忽略 `sink_addr` 都不影响结果——权威补全始终由本次调用的返回值给出，
host 侧监听器在预算内超时后自行收工。`sink_key` 用于并发下拒绝串台连接
（invoke 不持锁、`handle` 可被多会话并发进入）。

**实现约束**：provider 插件应把节点声明为 `NodeType::Task`（`task_node_doc`）。
Router 节点每次 invoke 后会 `dlclose`，而 HTTP 客户端（reqwest 等）会在 dylib 里注册
TLS 析构函数，进程退出时那些指针已随 unmap 失效 → segfault。`TASK_LIBRARIES` 对 Task
节点保持 dylib 常驻，规避该问题。同理，插件在 `handle` 里 spawn 的任何线程都必须在
返回前 join。

样例：`fixtures/plugins/llm_openai`（OpenAI 兼容，含 SSE 解析与 5xx 重试）。

### 4.2 指令入口（`command_name` + `command_entry`）

插件 docs 声明 `command_name` 且暴露 `command_entry` 节点时，`/{command_name} <args>` 由指令路由器（不经 LLM）分发到该节点，payload：`{ args, session_key, sender_id, conversation_kind }`；回复取响应 JSON 的 `message` 字段（缺失则用原始 payload 文本）。
