下面给出一份**面向 Rust + Cordis 风格系统的推荐架构蓝图**。目标是：插件可热插拔、执行可并行、流程可扩展（DAG + Gate）、并能在不同隔离级别（rlib / cdylib / WASM / 外部进程）之间切换。

---

## 1) 总体分层

```text
┌───────────────────────────────────────────────┐
│                 Plugin Packaging              │
│  rlib | cdylib | wasm | external process      │
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

| 类型               | 适合场景         | 推荐程度 |
| ---------------- | ------------ | ---- |
| rlib             | 内置核心插件       | ⭐⭐⭐⭐ |
| cdylib           | 可热插拔 Rust 插件 | ⭐⭐⭐  |
| WASM             | 第三方插件生态      | ⭐⭐⭐  |
| external process | 不可信插件        | ⭐⭐   |
| dylib            | Rust ABI 不稳定 | ❌    |

推荐组合：

```text
Core plugins → rlib
Extension plugins → cdylib
Third-party plugins → wasm
Untrusted plugins → external process
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

# 10) Actor Executor

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

# 11) Context 设计

Context 分两层：

### GlobalContext

```text
services
config
plugin registry
```

---

### RequestContext

```text
event
session
scratch data
outputs
```

例：

```text
scratch
├─ ParsedCommand
├─ QueryPlan
└─ CardResult
```

---

# 12) 一次命令执行流程

用户：

```text
/cn组卡 抓包模式 haruki
```

Runtime：

```text
MessageEvent
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
send
```

---

# 13) 推荐工程结构

```text
runtime/
 ├─ plugin/
 │   ├─ adapter/
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
 │   └─ request.rs
 │
 └─ service/
     ├─ container.rs
     └─ registry.rs
```

---

# 14) 最终架构总结

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
```

核心原则：

```text
插件声明能力
DAG描述依赖
调度器负责执行
```