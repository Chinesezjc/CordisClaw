# 架构与计划完成度

## 1. 判定口径

- 本文基于当前仓库现状整理，时间点为 2026-03-12。
- 历史规划蓝图已经吸收进 [design-blueprint.md](./design-blueprint.md)，因此本文结论来自三类证据的交叉比对：
  - 设计蓝图：[design-blueprint.md](./design-blueprint.md)
  - 架构文档：[system-overview.md](./system-overview.md)、[contracts-and-loading.md](./contracts-and-loading.md)、[runtime-semantics.md](./runtime-semantics.md)、[maintenance-guide.md](./maintenance-guide.md)
  - 运行时代码与测试：`crates/cordis-runtime/src/*`、`crates/cordis-runtime/tests/*`

一句话结论：

- “架构设计阶段”本身已经基本完成到“契约冻结 + 可运行原型”。
- 当前真正未完成的，主要是把这些原型接到更真实的入口、服务边界和自迭代闭环上。

## 2. 状态总表

| 主题 | 状态 | 结论 |
|---|---|---|
| Stage A-E 架构冻结 | 已完成 | 插件契约、ABI 契约、Loader、Artifact、Context/Security 都已有实现与文档归纳 |
| Resolver / Loader 主链路 | 已完成 | 发现、解析、拓扑加载、预算、哈希、指纹、required/optional 传播都已落地 |
| 文档契约、Graph/Doc helper、tooling | 已完成 | docs 回写、artifact index 刷新、注册图导出都可运行 |
| Execution engine | 部分完成 | 语义与库实现已完成，且已接入 `execute` / `serve execute` 入口；仍缺更真实的数据面与长期运行验证 |
| Kernel 自迭代 | 部分完成 | 已有评估、策略、记忆、回滚骨架，并补上 verifier pipeline / plugin verifier / structured config patch；仍没有真正的语义级代码 patch 与 canary 闭环 |
| 插件封装形态蓝图 | 部分完成 | 目前主要落地的是 `dylib` 与 JSON artifact / process，未覆盖最初蓝图全量形态 |
| 更真实的运行入口与服务化边界 | 部分完成 | 已有 `RuntimeHost`、`serve`、结构化 `reload`/`status`/`execute` 控制面；尚未服务化为稳定外部边界 |
| YAML 配置入口 | 已完成 | 运行时已覆盖 runtime / kernel / llm_api / plugins 配置模型；模板位于 `config.example/`，本地配置目录为 `config/` |

## 3. 已完成

### 3.1 Stage A-E 已经落地到可运行原型

[system-overview.md](./system-overview.md) 明确把当前实现归纳为 Stage A-E：

- Stage A：插件工程发现与元数据契约
- Stage B：运行时 ABI 契约与指纹一致性
- Stage C：`discover -> resolve -> instantiate` 的 loader 架构
- Stage D：预构建工件索引与哈希校验
- Stage E：上下文注入、作用域与授权链路

同时文档也说明，在这五段之上，仓库已经额外实现了执行引擎原型、图可视化、tooling、Kernel 骨架。

### 3.2 插件发现、契约校验、加载主链路已完成

[contracts-and-loading.md](./contracts-and-loading.md) 和代码实现对应关系已经比较稳定：

- [plugin/package.rs](../../crates/cordis-runtime/src/plugin/package.rs) 负责从顶层 workspace members 起步，递归解析 `package.metadata.cordis.children`，并做路径、crate name、docs、循环、越界等 fail-fast 校验。
- [plugin/loader.rs](../../crates/cordis-runtime/src/plugin/loader.rs) 负责预算校验、artifact index 读取、ABI 指纹比对、哈希校验、实例化、注册、required/optional 故障传播。
- 当前 loader 只消费预构建工件索引，不做运行时编译，也不做跨类型 fallback，这和计划中的 DoD 一致。

### 3.3 文档驱动注册、图导出、插件调用与工具链已完成

当前仓库已经具备一组可工作的支撑能力：

- `docs/agent/interfaces.json` 作为运行时输入，参与节点注册、文档查询和图导出。
- `DocRegistry` 已提供稳定的 route-style 查询约定。
- `GraphRegistry` 已能导出“已注册节点图”和“已注册 net”的 JSON/HTML。
- CLI 已暴露 `invoke`、`graph-html`、`net-html`、`sync-plugin-docs`、`refresh-artifact-index`、`auto-update` 等入口。

### 3.4 测试矩阵覆盖了当前原型的主线契约

[maintenance-guide.md](./maintenance-guide.md) 已把测试矩阵列出来，覆盖：

- `architecture.rs`：resolver / loader / grants / graph / invoke
- `semantics.rs`：CPN Net / Context / Engine
- `kernel.rs`：自迭代闭环判定
- `auto_update.rs`：补丁应用、回滚、路径安全
- `tooling.rs`：docs 回写与工件索引刷新

这意味着“当前把什么当作契约”，已经不仅停留在设计文档里。

## 4. 部分完成

### 4.1 Execution engine 已经实现，并已接到最小正式入口

[runtime-semantics.md](./runtime-semantics.md) 明确写到：

- CPN Net、Router、Actor、Scheduler、`execute_net()` 这些执行语义已经作为库实现完成。
- 当前已新增 `execute` CLI 与 `serve execute` 控制面命令，可对注册节点跑一条受控的执行链路，并返回 `execution_id`、顺序、结果与 metrics。

因此它已经从“纯库级语义”推进到了“有正式入口的运行时原型”。

尚未完成的是把 execution engine 接到更真实的数据流与业务图，而不只是保守的注册 net / 节点执行入口。

### 4.2 Kernel 已有最小闭环，但仍是骨架级实现

计划里的 OpenClaw 接入 Kernel 分为三步：

- Phase 1：只读诊断
- Phase 2：受限自动修复
- Phase 3：小流量自迭代（canary + 自动回滚 + 质量评分）

当前代码已经落地了其中一部分基础设施：

- [kernel/policy.rs](../../crates/cordis-runtime/src/kernel/policy.rs)：路径白名单、敏感路径人工确认、diff/时间预算
- [kernel/evaluator.rs](../../crates/cordis-runtime/src/kernel/evaluator.rs)：验证结果与质量评分聚合
- [kernel/memory.rs](../../crates/cordis-runtime/src/kernel/memory.rs)：问题-补丁-结果记忆
- [kernel/loop.rs](../../crates/cordis-runtime/src/kernel/loop.rs)：`observe -> diagnose -> plan -> apply -> verify -> score -> safety_gate -> promote/rollback`
- [kernel/auto_update.rs](../../crates/cordis-runtime/src/kernel/auto_update.rs)：文本补丁事务、JSON/TOML 结构化补丁与失败回滚
- [kernel/verifier.rs](../../crates/cordis-runtime/src/kernel/verifier.rs)：verification profile、`static_check/tests/safety` stage、shell/plugin verifier 统一输出
- [host.rs](../../crates/cordis-runtime/src/host.rs)：`KernelPlanResult` / `KernelPlanApplyResult` 已带 verification plan，host 内保留 last reload diagnostics

但文档同时也明确限制了当前边界：

- Kernel 本身仍不负责语义级代码补丁生成，只负责在“补丁已应用、验证结果已得出”之后做判定。
- AutoUpdater 虽已支持结构化 config patch，但还不是 AST 级 Rust 改写器；verification pipeline 也还没有接入真正的 benchmark / sandbox / canary。

所以这里的状态应理解为“闭环骨架完成，真实自迭代能力未完成”。

### 4.3 插件封装形态只落地了蓝图的一部分

[design-blueprint.md](./design-blueprint.md) 里保留的历史蓝图把插件封装形态写成：

- `rlib`
- `dylib`
- `cdylib`
- `WASM`
- `external process`

而当前实际落地的主路径是：

- `dylib`
- JSON artifact
- JSON artifact + `execution = process`

这说明“统一插件运行适配层”的大蓝图还没有全部实现。现状更准确的说法是：

- 内部受控 `dylib` 路径已落地
- JSON/process 调用路径已落地
- `cdylib` / `WASM` / 更完整的 runtime adapter 生态还没有落地

## 5. 明确未完成

这些项在文档里被直接描述为“后续演进方向”或从当前实现边界可以明确看出仍未完成。

### 5.1 把 execution engine 接到更真实的数据面与运行入口

[maintenance-guide.md](./maintenance-guide.md) 直接把这件事列为后续方向之一。

当前状态：

- engine 语义存在
- registry 与 context 也存在
- `execute` / `serve execute` 已提供最小正式入口
- 但仍缺真实数据传递、更多节点类型和长期运行语义验证

### 5.2 为 Kernel 增加真实 patch 生成与验证器集成

[maintenance-guide.md](./maintenance-guide.md) 也直接把这件事列为后续方向。

目前还缺少的是真正的：

- patch proposal 生成器
- 测试/基准/回归/安全检查的真实执行集成
- 基于结果自动迭代下一轮策略的完整链路

### 5.3 小流量自迭代（canary）尚未落地

计划里明确把 canary + 自动回滚 + 质量评分列为 Phase 3。

当前已经有：

- 质量评分
- promote / rollback 判定
- 文本补丁回滚

但还没有：

- 流量分层
- canary 发布机制
- 真实环境验证与自动晋升

### 5.4 Graph / Doc 还没有升级为更稳定的服务边界

当前 `DocRegistry` 的“GET”只是 route-style helper，不是 HTTP 服务。

`GraphRegistry` 导出的 net 也是“注册视图”，并不等于真实业务执行流。现有 net 推导仍然偏保守，主要基于 schema 属性名匹配。

因此“服务边界稳定化”和“更丰富的 net 推导规则”都还没有完成。

### 5.5 更多插件样例与 loader 边界验证还可以继续补

维护指南明确建议继续增加更多外部插件样例，验证 loader 边界。

这类工作虽然不改变主架构，但会直接提升：

- 契约的稳定性
- 边界条件的验证强度
- 后续扩展新插件形态时的信心

## 6. 建议优先级

如果接下来要继续推进，这个仓库最值得优先做的通常是：

1. 把 execution engine 接到一个真实入口，避免执行层长期只停留在库语义。
2. 把 Kernel 从“已应用补丁后的判定器”升级为“能生成 patch proposal 并运行真实验证”的闭环。
3. 把 graph/doc helper 收敛成更稳定的服务边界，并增强 net 推导规则。
4. 补更多插件样例与契约测试，再决定是否继续扩展到 `cdylib` / `WASM`。

## 7. 当前最准确的总体判断

截至 2026-03-12，这个仓库最准确的描述不是“架构还没做完”，而是：

- 架构主干和契约已经做完了。
- 原型验证已经做到了可运行、可测试、可解释。
- 未完成的是把这些原型进一步产品化、入口化、服务化，以及把 Kernel 提升为更真实的自动演进系统。
