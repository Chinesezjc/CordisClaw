下面给出一份**面向 Rust + Cordis 风格系统的推荐架构蓝图**。目标是：插件可热插拔、执行可并行、流程可扩展（DAG + Gate）、并能在不同隔离级别（rlib / dylib / cdylib / WASM / 外部进程）之间切换。

---

# 1) 总体分层

```text
┌───────────────────────────────────────────────┐
│                 Plugin Packaging              │
│  rlib | dylib | cdylib | wasm | external proc │
└───────────────┬───────────────────────────────┘
                ▼
┌───────────────────────────────────────────────┐
│            Plugin Runtime Adapter             │
│  统一调用接口：init / nodes / handle / drop   │
└───────────────┬───────────────────────────────┘
                ▼
┌───────────────────────────────────────────────┐
│               Node Registry                   │
│  NodeMeta: id / consumes / produces / gate    │
└───────────────┬───────────────────────────────┘
                ▼
┌───────────────────────────────────────────────┐
│             Execution Graph (DAG)             │
│  Node / Edge / Gate / Router                  │
└───────────────┬───────────────────────────────┘
                ▼
┌───────────────────────────────────────────────┐
│        Scheduler + Actor Executor             │
│  ready queue / dependency tracker / mailbox   │
└───────────────┬───────────────────────────────┘
                ▼
┌───────────────────────────────────────────────┐
│               ExecutionState                  │
│  Context / running nodes / outputs            │
└───────────────────────────────────────────────┘
```

**职责分离**

* 插件：提供能力（nodes）
* DAG：定义依赖关系
* Scheduler：驱动执行
* Actor：具体执行单元

---

# 2) 插件形式（推荐策略）

不同插件可以使用不同打包方式，通过 **Runtime Adapter** 统一。

| 类型               | 适合场景                        | 推荐程度 |
| ---------------- | --------------------------- | ---- |
| rlib             | 内置核心插件                      | ⭐⭐⭐⭐ |
| dylib            | 内部同构热更新插件（同 toolchain 受控部署） | ⭐⭐   |
| cdylib           | 跨版本可热插拔 Rust 插件（C ABI）       | ⭐⭐⭐  |
| WASM             | 第三方插件生态                     | ⭐⭐⭐  |
| external process | 不可信插件                       | ⭐⭐   |

推荐组合：

```text
Core plugins → rlib
Internal hot-path plugins → dylib
Extension plugins → cdylib
Third-party plugins → wasm
Untrusted plugins → external process
```

插件拓扑：

```text
支持完全嵌套插件树（Composite Plugin Tree）
父插件可声明 children，子插件可继续声明 children
```

### dylib 支持边界（受控模式）

`dylib` 可支持，但只能用于**内部受控环境**，不面向不可信插件市场。

约束：

```text
1) target triple 必须一致（OS/ARCH/LIBC）
2) rustc 版本必须一致（建议精确到 patch）
3) DylibAbiKind 固定为 Rust（纯 Rust ABI，非 C ABI）
4) 严格指纹校验必须全匹配：rustc_version/target_triple/crate_hash/api_hash
5) 禁止从 dylib 自动降级到 cdylib 或 wasm
```

加载策略：

```text
discover dylib
  → 校验 manifest + abi_fingerprint
  → libloading 获取导出符号
  → Runtime Adapter 包装为统一 PluginNode
  → Node Registry 注册
```

推荐做法：

```text
把 dylib 视为“高性能、低兼容”的内部通道
把 cdylib 视为“高兼容、低耦合”的默认通道
cdylib/wasm 只能显式选择，不能作为 dylib 失败兜底
```

---

# 3) 插件 API（Node 定义）

插件不声明完整 DAG，只声明 **节点能力**。

```rust
pub struct NodeMeta {
    pub id: &'static str,
    pub consumes: Vec<TypeId>,
    pub produces: Vec<TypeId>,
    pub node_type: NodeType,
}
```

NodeType：

```rust
enum NodeType {
    Task,
    Router,
    Gate,
    Terminal,
}
```

插件实现：

```rust
trait PluginNode {
    fn meta(&self) -> NodeMeta;

    async fn handle(
        &mut self,
        ctx: &mut Context,
    ) -> NodeResult;
}
```

### 完全嵌套插件树（CompositePlugin）

支持插件内套插件，且子插件可继续声明子插件，形成多层插件树。

路径命名：

```text
plugin_path: root/child/grandchild
node_fqn:    plugin_path::node_id
```

约束：

```text
1) plugin_path 必须全局唯一
2) node_fqn 必须全局唯一
3) 冲突在 resolve 阶段 fail-fast
```

子插件声明（文档 API）：

```rust
struct ChildPluginSpec {
    id_path: String,
    source: PluginSource, // 相对父插件目录路径，如 ./child 或 ./child/grandchild
    required: bool,
    grants: Vec<String>,
}

trait CompositePlugin {
    fn children(&self) -> Vec<ChildPluginSpec>;
}
```

`dylib` ABI 契约（文档级）：

```rust
enum DylibAbiKind {
    Rust,
}

struct AbiFingerprint {
    rustc_version: String,
    target_triple: String,
    crate_hash: String,
    api_hash: String,
}

enum PluginUnavailableReason {
    AbiMismatch,
    SymbolMissing,
    InitFailed,
    BudgetExceeded,
}

enum PluginLoadResult {
    Loaded,
    Unavailable(PluginUnavailableReason),
}
```

约束：

```text
ChildPluginSpec.required 仅控制父子故障传播
不触发 dylib -> cdylib/wasm 打包类型降级
```

故障传播：

```text
child required=true  初始化失败 -> parent 失败
child required=false 初始化失败 -> 标记 Unavailable + parent 继续 + 记录告警
```

### 插件工程结构（源码 / 测试 / 文档）

每个插件包必须同时包含三部分：

```text
source  -> 实现节点与插件入口
tests   -> 单测/集成/回归样例
docs    -> 面向 agent 与人类的可读说明
```

推荐目录：

```text
plugin_path = root/child/grandchild
目录映射 = plugins/root/child/grandchild/

plugins/
 ├─ Cargo.toml                 # 仅列顶层插件 members（不覆盖全部嵌套）
 │
 └─ root/
     ├─ Cargo.toml
     ├─ src/
     ├─ tests/
     ├─ docs/
     │   ├─ agent/
     │   │   ├─ index.md
     │   │   ├─ interfaces.json
     │   │   ├─ nodes.md
     │   │   ├─ constraints.md
     │   │   └─ examples.md
     │   └─ human/
     │       └─ overview.md
     │
     └─ child/
         ├─ Cargo.toml
         ├─ src/
         ├─ tests/
         ├─ docs/
         │   ├─ agent/
         │   │   └─ interfaces.json
         │   └─ human/
         │       └─ overview.md
         │
         └─ grandchild/
             ├─ Cargo.toml
             ├─ src/
             ├─ tests/
             └─ docs/
                 ├─ agent/
                 │   └─ interfaces.json
                 └─ human/
                     └─ overview.md
```

Cargo.toml metadata 契约（示意）：

```toml
# plugins/Cargo.toml（顶层）
[workspace]
members = ["root"] # 仅顶层插件

# plugins/root/Cargo.toml（父插件）
[package.metadata.cordis]
plugin_path = "root"
children = ["./child"] # 仅直接下属

[[package.metadata.cordis.children_meta]]
source = "./child"
required = true
grants = ["service.db", "service.cache"]

# plugins/root/child/Cargo.toml（子插件）
[package.metadata.cordis]
plugin_path = "root/child"
children = ["./grandchild"] # 仅直接下属
```

目录与路径一致性规则：

```text
1) manifest.plugin_path 必须与相对目录路径完全一致
2) crate 名由 plugin_path 规范化生成（root/child -> root_child），且全局唯一
3) 每一层插件必须独立拥有 src/tests/docs 三件套
4) node_fqn = plugin_path::node_id，必须与目录映射一致
5) children 路径必须相对父目录，禁止 ../ 越界
6) 每个插件只声明 direct children，不允许跨层声明孙级
7) 任一不一致在 resolve 阶段 fail-fast
```

### Agent 文档契约（必须）

`docs/agent/interfaces.json` 为机器可读接口描述，`docs/agent/*.md` 为语义补充。

```rust
#[derive(Serialize, Deserialize)]
pub struct PluginDocs {
    pub plugin_id: String,
    pub plugin_path: String,
    pub plugin_version: String,
    pub abi_version: u32,
    pub nodes: Vec<NodeDoc>,
}

#[derive(Serialize, Deserialize)]
pub struct NodeDoc {
    pub id: String,
    pub summary: String,
    pub input_schema: serde_json::Value,
    pub output_schema: serde_json::Value,
    pub side_effects: Vec<String>,
    pub failure_modes: Vec<String>,
}
```

插件需提供文档读取接口：

```rust
trait PluginIntrospection {
    fn docs(&self) -> PluginDocs;
}
```

Runtime 对外提供文档查询接口（供 agent 检索）：

```text
GET /plugins/{plugin_path}/docs
GET /plugins/{plugin_path}/nodes/{node_id}/docs
```

---

# 4) Node 类型设计

### TaskNode

普通执行步骤

```text
parser
card.query
render
```

---

### RouterNode

动态选择 pipeline

```text
command_resolve
      │
      ▼
choose pipeline
```

---

### GateNode

控制依赖关系

```text
mirrorA
mirrorB
mirrorC
   │
   ▼
FirstSuccessGate
```

---

### TerminalNode

产生最终结果

```text
send_message
```

---

# 5) Edge 类型

Edge 不止一种。

### 数据依赖

```text
producer(output) → consumer(input)
```

示例：

```text
ParsedCommand → QueryPlan
```

---

### 控制依赖

```text
A → B
```

只控制顺序。

---

### 条件依赖

用于 Router。

```text
resolve → pipeline.card
resolve → pipeline.song
```

---

# 6) Gate 类型（依赖策略）

统一抽象：

```rust
enum GatePolicy {
    AllOf,
    AnyOf,
    FirstSuccess,
    FirstCompleted,
    AtLeast(usize),
}
```

示例：

### AND

```text
A
B
C
 │
 ▼
AllOfGate
```

---

### OR

```text
A
B
C
 │
 ▼
AnyOfGate
```

---

### FirstSuccess

```text
mirrorA
mirrorB
mirrorC
   │
   ▼
FirstSuccessGate
```

---

# 7) 自动 DAG 生成

系统根据：

```text
consumes / produces
```

自动连线。

规则：

```text
nodeA produces X
nodeB consumes X
```

生成：

```text
nodeA → nodeB
```

例如：

```text
parser.parse
   │
   ▼
command.resolve
   │
   ▼
card.query
   │
   ▼
render
```

---

# 8) 动态子图（Pipeline）

RouterNode 负责实例化子图。

```text
message
   │
   ▼
parser
   │
   ▼
command_resolve
   │
   ├──► card_pipeline
   │
   ├──► song_pipeline
   │
   └──► admin_pipeline
```

pipeline 本身也是 DAG。

---

# 9) Scheduler 设计

Scheduler 维护：

```text
ExecutionState
├─ ready_queue
├─ running_nodes
├─ finished_nodes
├─ dependency_counter
└─ outputs
```

调度流程：

```text
1 build execution graph
2 push entry nodes to ready_queue
3 actor executes node
4 update dependencies
5 push new ready nodes
```

---

# 10) DAG 语义契约（v1）

本节定义可直接实现的执行语义，避免不同 Runtime/Plugin 对 DAG 行为产生歧义。

### 10.1 构图语义规则

多生产者冲突处理顺序：

```text
显式绑定 > priority > node_id(asc)
```

规则：

```text
1) 同一输入类型存在多个 producer 时，先看显式绑定
2) 未绑定时，选择 priority 更高者
3) priority 相同时，选择 node_id 字典序最小者
4) 若仍无法唯一确定（例如策略冲突/元数据缺失），构图阶段 fail-fast
```

构图阶段必须执行：

```text
1) cycle detection（返回完整环路径）
2) required input completeness check（必需输入缺失时拒绝启动）
3) producer conflict report（输出冲突节点列表）
```

### 10.2 统一节点结果类型

```rust
enum NodeOutcome {
    Success,
    Failure,
    Timeout,
    Cancelled,
    Skipped,
}
```

约束：

```text
Gate/Scheduler 仅基于 NodeOutcome 聚合
不直接依赖节点内部错误格式
```

### 10.3 Gate 执行语义

统一运行参数：

```rust
struct RunPolicy {
    timeout_ms: u64,
    max_retries: u32,
    backoff: BackoffPolicy,
}
```

各 Gate 的触发/完成/失败语义：

```text
AllOf:
  trigger   = 所有上游开始可观测
  complete  = 所有上游 Success
  fail      = 任一上游 Failure/Timeout 且重试耗尽

AnyOf:
  trigger   = 任一上游完成
  complete  = 任一上游 Success
  fail      = 所有上游均 Failure/Timeout/Cancelled

FirstSuccess:
  trigger   = 任一上游完成
  complete  = 首个 Success 出现
  fail      = 所有上游都无法 Success
  side      = 首个 Success 后取消其余仍在运行分支

FirstCompleted:
  trigger   = 任一上游完成
  complete  = 首个终态出现（Success/Failure/Timeout/Cancelled）
  fail      = 若首个终态非 Success，则按 Failure 传播

AtLeast(k):
  trigger   = 至少 1 个上游完成
  complete  = 成功数 >= k
  fail      = 剩余可成功上游数量 < k
```

timeout/cancel 传播规则：

```text
1) timeout 视为终态，按 Failure 类处理参与 Gate 聚合
2) Cancelled 默认不沿数据依赖传播成功信号
3) FirstSuccess/FirstCompleted 主动取消产生的 Cancelled 仅影响同 Gate 分支
4) 被取消分支的下游节点标记 Skipped，不进入 ready_queue
```

### 10.4 Scheduler 确定性规则

`ready_queue` 排序键固定为：

```text
topo_level(asc) -> priority(desc) -> node_id(asc)
```

并发与重试规则：

```text
1) 并发上限仅由 max_parallelism 控制
2) 重试节点重新入队时，保持原 topo_level 与 priority
3) 同一排序键下，重试节点优先于新就绪节点
4) 相同输入 + 相同图定义应产生相同执行顺序与输出顺序
```

### 10.5 Router 子图边界

上下文模型：

```text
parent context (read snapshot)
        │
        ▼
subgraph overlay (write)
```

事务接口（与 Context 章节一致）：

```text
begin_subgraph() -> SubgraphOverlay
commit_overlay(subgraph_id)
rollback_overlay(subgraph_id)
```

合并规则：

```text
1) 子图 Success 时，将 overlay 合并回 parent
2) 子图 Failure/Timeout/Cancelled 时回滚 overlay
3) 子图取消不得污染 parent 已完成节点输出
```

### 10.6 可观测性最小指标集

```text
dag_build_ms
dag_cycle_detected_total
node_retry_total
gate_wait_ms
execution_cancel_total
```

追踪约束：

```text
每次执行必须生成 execution_id
execution_id 必须贯穿日志、指标、错误报告
```

### 10.7 测试与验收场景

```text
1) 多 producer 同类型输出且无显式绑定
   => 构图失败，并返回冲突节点列表

2) 图中存在环
   => 构图失败，并返回完整环路径

3) FirstSuccess 出现首个 Success
   => 其余分支被取消，且记录取消原因

4) 节点 timeout 且重试耗尽
   => NodeOutcome 固定为 Timeout
   => 正确触发下游传播策略

5) 相同输入重复执行两次
   => ready 顺序与最终输出顺序一致

6) Router 子图失败
   => parent context 无脏写，已完成输出保持不变
```

### 10.8 v1 默认策略

```text
1) 本轮仅补全文档契约，不修改 runtime 代码
2) 冲突/环/缺失输入全部 fail-fast
3) 不启用隐式容错或自动降级
```

---

# 11) Actor Executor

每个 node 在运行时变成 actor。

```text
ThreadPool
 ├─ Actor(parser)
 ├─ Actor(resolve)
 ├─ Actor(card.query)
 ├─ Actor(render)
 └─ Actor(send)
```

Actor 之间通信：

```text
message passing
```

避免共享状态。

---

# 12) Context 设计

Context 采用三层模型，默认策略是**一致性与隔离优先**。

### 12.1 分层模型

```text
GlobalContext   -> 进程级只读（services/config/plugin registry）
SessionContext  -> 会话级快照读 + 显式提交写
RequestContext  -> 请求级可写 overlay（节点执行主工作区）
```

读写矩阵：

```text
Node 默认只读: Global + Session Snapshot
Node 默认可写: Request Overlay
Node 直接写 Session: 禁止（需显式 commit_session）
Node 写 Global: 禁止
```

### 12.2 标识与生命周期

```text
execution_id: 一次完整执行（日志/指标主关联键）
request_id:   单次请求实例
session_id:   会话身份
subgraph_id:  Router 子图实例
```

生命周期：

```text
1) load session snapshot
2) create request overlay
3) run DAG / subgraph
4) merge or rollback overlay
5) optional commit_session
```

### 12.3 强类型槽位模型

```rust
struct ContextKey {
    namespace: String,
    name: String,
    version: u32,
}

struct SlotMeta {
    required: bool,
    ttl_ms: Option<u64>,
    sensitivity: Sensitivity,
    owner: String,
}
```

版本兼容规则：

```text
同主版本兼容
主版本不兼容 -> 拒绝读取（fail-fast）
```

### 12.4 Cordis 风格上下文注册

注册模型采用 `provide/inject/dispose`，并支持父子上下文继承，语义对齐 Cordis 风格：

```rust
enum ContextScope {
    Global,
    Session,
    Request,
}

struct ServiceDescriptor {
    id: &'static str,
    scope: ContextScope,
    required: bool,
}

trait ContextRegistry {
    fn provide<T: Send + Sync + 'static>(
        &mut self,
        id: &'static str,
        scope: ContextScope,
        service: T,
    ) -> Result<(), ContextError>;

    fn inject<T: Send + Sync + 'static>(&self, id: &'static str) -> Result<Arc<T>, ContextError>;
    fn maybe<T: Send + Sync + 'static>(&self, id: &'static str) -> Option<Arc<T>>;
    fn dispose(&mut self, id: &'static str) -> Result<(), ContextError>;
}
```

依赖声明（插件/节点）：

```rust
trait Injectable {
    fn requires() -> &'static [&'static str];
    fn optional() -> &'static [&'static str];
}
```

注册与解析规则：

```text
1) Global/Session/Request 形成父子上下文链
2) 插件局部上下文形成 plugin_path 父子链
3) inject 查找顺序: Local(current) -> Local(parent...) -> Request -> Session -> Global
4) 子插件仅允许访问父插件 grants 白名单中的 service，默认拒绝继承
5) 同 scope 重复 provide 默认拒绝（除非显式 allow_override）
6) required 依赖缺失时，节点注册失败（fail-fast）
7) inject 命中 Unavailable 插件时返回 PluginUnavailable
8) dispose 按注册逆序执行，确保依赖先于被依赖对象释放
```

### 12.5 Context 文档接口

```rust
trait ContextRead {
    fn get<T>(&self, key: &ContextKey) -> Result<Option<T>, ContextError>;
    fn contains(&self, key: &ContextKey) -> bool;
    fn list_by_ns(&self, namespace: &str) -> Vec<ContextKey>;
}

trait ContextWrite {
    fn put<T>(&mut self, key: ContextKey, value: T, meta: SlotMeta) -> Result<(), ContextError>;
    fn remove(&mut self, key: &ContextKey) -> Result<(), ContextError>;
    fn mark_skipped(&mut self, node_id: &str) -> Result<(), ContextError>;
}

trait ContextTxn {
    fn begin_subgraph(&mut self, subgraph_id: &str) -> Result<(), ContextError>;
    fn commit_overlay(&mut self, subgraph_id: &str) -> Result<(), ContextError>;
    fn rollback_overlay(&mut self, subgraph_id: &str) -> Result<(), ContextError>;
    fn commit_session(&mut self, session_id: &str, expected_version: u64) -> Result<(), ContextError>;
}
```

`commit_session` 采用 CAS 版本校验，冲突返回 `CommitConflict`，默认不自动重试。

### 12.6 合并与回滚语义

```text
1) 节点执行只写 RequestContext
2) 子图 Success -> commit_overlay
3) 子图 Failure/Timeout/Cancelled -> rollback_overlay
4) Session 写入只允许在终结节点或显式提交节点
```

### 12.7 失败与传播规则

```text
1) required 槽位缺失 -> 执行前校验失败，拒绝启动
2) 运行期读取版本不兼容 -> 当前节点 Failure
3) 回滚后下游缺值:
   - 输入 required=true  -> Failure
   - 输入 required=false -> Skipped
```

### 12.8 可观测性（Context 专项）

```text
context_read_total
context_write_total
context_overlay_rollback_total
session_commit_conflict_total
session_commit_latency_ms
```

日志约束：

```text
context 相关日志必须包含 execution_id + request_id + session_id
```

### 12.9 测试与验收场景

```text
1) 并发节点写同一 Request key -> 结果满足确定性顺序
2) 子图成功/失败 -> 分别验证 commit_overlay / rollback_overlay
3) session CAS 冲突 -> commit_session 失败且 session 原值不变
4) required 槽位缺失 -> 启动阶段失败并返回缺失键列表
5) schema 主版本不兼容 -> 拒绝读取并记录结构化错误
6) 相同输入重复执行 -> context 读写轨迹与输出顺序一致
7) 缺失 required service -> inject 阶段失败并阻止节点注册
8) Request 覆盖 Session 同名 service -> inject 命中 Request 实例
9) 子插件访问未授权服务 -> inject 返回 PermissionDenied
10) 多层子插件注入解析 -> 命中 Local(current) 或沿 parent 链回退
11) inject 命中 Unavailable 插件 -> 返回 PluginUnavailable
```

---

# 13) 一次命令执行流程

用户：

```text
/cn组卡 抓包模式 haruki
```

Runtime：

```text
MessageEvent
   │
   ▼
load_session_snapshot
   │
   ▼
build_request_overlay
   │
   ▼
parser
   │
   ▼
command_resolve
   │
   ▼
card_pipeline
   │
   ▼
card.query
   │
   ▼
render
   │
   ▼
optional_commit_session
   │
   ▼
send
```

---

# 14) 推荐工程结构

```text
plugins/
 ├─ Cargo.toml                  # workspace members 仅列顶层插件
 └─ <plugin_path mirrored dirs> # 每层 Cargo.toml 通过 metadata 声明 direct children

runtime/
 ├─ plugin/
 │   ├─ adapter/
 │   ├─ composite.rs
 │   ├─ tree.rs
 │   ├─ loader/
 │   │   ├─ discover.rs         # 由顶层 members + children metadata 递归发现
 │   │   ├─ resolve.rs
 │   │   ├─ instantiate.rs
 │   │   ├─ budget.rs
 │   │   ├─ dylib.rs
 │   │   ├─ cdylib.rs
 │   │   └─ manifest.rs
 │   ├─ registry.rs
 │   └─ node.rs
 │
 ├─ dag/
 │   ├─ graph.rs
 │   ├─ gate.rs
 │   └─ router.rs
 │
 ├─ scheduler/
 │   ├─ executor.rs
 │   ├─ state.rs
 │   └─ actor.rs
 │
 ├─ context/
 │   ├─ global.rs
 │   ├─ session.rs
 │   ├─ request.rs
 │   ├─ registry.rs
 │   ├─ inject.rs
 │   ├─ key.rs
 │   ├─ slot.rs
 │   ├─ txn.rs
 │   └─ store.rs
 │
 ├─ kernel/
 │   ├─ loop.rs
 │   ├─ policy.rs
 │   ├─ evaluator.rs
 │   └─ memory.rs
 │
 └─ service/
     ├─ container.rs
     ├─ doc_registry.rs
     └─ registry.rs
```

---

# 15) dylib Rust ABI 与 Loader 设计

统一导出符号（示意）：

```rust
pub struct RustPluginApiV2 {
    pub abi_kind: DylibAbiKind,          // 固定为 Rust
    pub abi_fingerprint: AbiFingerprint, // 严格指纹
    pub init: fn() -> Box<dyn RuntimePlugin>,
    pub nodes: fn(&dyn RuntimePlugin) -> Vec<NodeMeta>,
    pub docs: fn(&dyn RuntimePlugin) -> PluginDocs,
    pub handle: fn(&mut dyn RuntimePlugin, PluginRequest) -> PluginResponse,
    pub drop: fn(Box<dyn RuntimePlugin>),
}
```

嵌套加载预算（无深度上限时的硬保护）：

```rust
pub struct LoaderBudget {
    pub max_total_plugins: usize,
    pub max_total_nodes: usize,
    pub load_timeout_ms: u64,
}
```

Kernel 侧加载流程：

```text
Phase A: discover/resolve
1) 读取 plugins/Cargo.toml 的顶层 workspace members
2) 逐个读取 member 的 package.metadata.cordis.children
3) 按 direct-children metadata 递归展开插件树
4) 检测循环依赖（返回完整环路径）
5) 校验目录与声明一致性：
   manifest.plugin_path == 相对目录路径
   crate_name == normalize(plugin_path) 且全局唯一
6) 校验 plugin_path 唯一性、node_fqn(plugin_path::node_id) 唯一性
7) 校验 children 路径合法：相对路径且禁止 ../ 越界
8) 校验每层插件三件套完整：src/tests/docs（含 docs/agent/interfaces.json）
9) 校验 abi_kind=Rust + 严格指纹全匹配：
   rustc_version + target_triple + crate_hash + api_hash
10) 校验 manifest + docs contract
11) 校验预算：max_total_plugins / max_total_nodes / load_timeout_ms

Phase B: instantiate
1) 按拓扑顺序 load_library + dlsym("cordis_plugin_api_rust_v2")
2) 为每个 plugin_path 创建 Local Context，并应用 grants 白名单
3) 调用 docs() 并写入 doc_registry
4) 注册到 PluginRegistry（key = plugin_path）
5) 注册节点并进入调度
```

故障策略：

```text
symbol 缺失 / Rust ABI 不匹配 / 初始化失败 / 预算超限
  → PluginLoadResult::Unavailable(reason)
  → 不尝试 cdylib/wasm fallback
  → 上报告警与版本画像

docs() 缺失 / interfaces.json 不合规
  → 拒绝注册（不满足 agent 可读约束）
  → 上报文档契约错误

child required=true 初始化失败
  → parent 标记失败，整条父链停止实例化

child required=false 初始化失败
  → 标记 child=Unavailable
  → parent 继续
  → 记录 unavailable 插件状态与告警
  → 不阻塞同级 required 子插件加载
```

嵌套插件测试与验收场景：

```text
1) plugins/Cargo.toml 仅顶层 members
   => 嵌套子插件仍可通过 metadata 递归发现

2) 父插件未声明某孙级路径
   => 该目录不得被自动发现（禁止隐式全目录扫描）

3) children 路径越界或不存在（如 ../x）
   => resolve 阶段失败并返回具体父插件路径

4) manifest.plugin_path 与目录不一致
   => resolve 阶段失败并返回冲突路径

5) 同层重复 child 声明或产生 plugin_path/node_fqn 冲突
   => fail-fast 并给出冲突对

6) optional 子插件加载失败
   => 标记 Unavailable，父插件继续，且不触发 cdylib/wasm 回退

7) 严格指纹不匹配（rustc/target/crate_hash/api_hash 任一不一致）
   => required 子插件阻断父链
   => optional 子插件标记 Unavailable 并继续同级加载
   => 不触发 cdylib/wasm 路径

8) Rust ABI 符号缺失或签名不匹配
   => 返回 SymbolMissing/AbiMismatch
   => 行为符合 required/optional 传播规则

9) grants 白名单生效
   => 未授权服务 inject 返回 PermissionDenied

10) 深层树高压场景
   => 触发 max_total_plugins / max_total_nodes / load_timeout_ms 保护
```

可观测性（dylib 禁回退）：

```text
dylib_abi_mismatch_total
dylib_no_fallback_total
plugin_unavailable_total
```

日志字段：

```text
plugin_path
required
fingerprint_diff
```

---

# 16) OpenClaw 接入 Kernel（自迭代）

目标：让 Kernel 在安全边界内具备“观察-改进-验证-发布”的闭环。

最小闭环：

```text
observe(telemetry/test failure)
  → diagnose(root cause)
  → plan(patch proposal)
  → apply(branch sandbox)
  → verify(test/bench/security gate)
  → score
  → promote or rollback
```

与现有 DAG/Gate 的结合：

```text
OpenClawPlannerNode
  │
  ├─► PatchNode
  │
  ├─► VerifyNode
  │
  └─► PromoteGate(AtLeast(2): tests + safety_checks)
```

建议分阶段：

```text
Phase 1: 只读诊断（生成 patch 建议，不自动提交）
Phase 2: 受限自动修复（仅非核心目录 + 强制人工 Gate）
Phase 3: 小流量自迭代（canary + 自动回滚 + 质量评分）
```

Kernel 需要补充的能力：

```text
1) IterationPolicy（目录白名单、最大 diff、时间预算）
2) EvalHarness（测试/基准/回归评分）
3) ChangeMemory（记录“问题-补丁-结果”用于下一轮策略）
4) SafetyGate（敏感路径必须人工确认）
```

---

# 17) 最终架构总结

整个系统可以理解为：

```text
Plugin Packaging
   │
   ▼
Plugin Nodes
   │
   ▼
Execution DAG
   │
   ▼
Scheduler
   │
   ▼
Actor Runtime
   │
   ▼
ExecutionState
   │
   ▼
OpenClaw Iteration Loop
```

核心原则：

```text
插件声明能力
DAG描述依赖
调度器负责执行
OpenClaw负责持续优化
```
