# 把 agent loop 移出 kernel

目标：kernel 只保留**机制**（CPN 引擎、插件加载、context、插件调用、会话注册），
对话循环（请求组装、工具分派、历史/压缩、DSML 协议、回合结束语义）移进独立 crate
`cordis-agent`，由 `cordis-cli` 注入。之后才能谈"多 session / session 间通信 /
session 网络"——循环变成一个可替换组件，才可能同一进程里跑多种循环。

## 为什么

- `agent.rs` 现在 4585 行，是仓库唯一被 coverage 排除的地方：**最大的未测试面**。
  它是策略（prompt 取什么、什么时候结束、失败怎么重试），不是机制。
- 本仓一晚改的三件事（B1 回滚、DSML 协议、end-tool）全部只能改 kernel、全量重建、
  重启服务；如果循环是独立 crate，这些改动可以独立版本化、独立测试。
- 与已有的 `PluginBuilder` 完全同构：kernel 定义 trait，独立 crate 实现，cli 注入。

## 现状（第一步已完成）

`crates/cordis-runtime/src/agent_api.rs` 已抽出接缝类型：`AgentToolHost`、
`AgentBackend`、`AgentReply`、`AgentToolEvent`、`AgentToolSpec`、`AgentSessionStatus`、
`AgentSessionSnapshot`、`AgentTranscriptEntry`。`agent` 模块 re-export，调用点不变。

## 目标结构

```
crates/cordis-runtime/src/
  agent_api.rs        接口：trait + DTO（已有）
  agent_loop.rs       新增：AgentLoop 工厂 trait + AgentSessionApi + 注入点
  agent.rs            过渡期保留：默认实现（DefaultAgentLoop）包住现有 AgentSession
crates/cordis-agent/  新 crate：循环、DSML、prompt、history、iteration/*
crates/cordis-cli/    调用 install_agent_loop(cordis_agent::factory())
```

```rust
// agent_api.rs（kernel 定义，插件/独立 crate 实现）
pub trait AgentSessionApi: Send {
    fn kind(&self) -> &str;
    fn respond_with_runtime_host_via_images(
        &mut self, host: &dyn AgentToolHost, session_id: &str,
        input: &str, images: &[Value], notes: &str,
    ) -> Result<AgentReply, RuntimeError>;
    fn status(&self) -> AgentSessionStatus;
    fn to_snapshot(&self) -> AgentSessionSnapshot;
    fn history_len(&self) -> usize;
    fn compact_history(&mut self) -> Result<(), RuntimeError>;
    fn swap_config(&mut self, api: LlmApiConfig);
    fn set_soul_key(&mut self, key: String);
    fn set_require_end_tool(&mut self, require: bool);
    fn set_caller_policy(&mut self, is_group: bool, is_admin: bool);
    fn set_approval_capability(&mut self, can_approve: bool);
    fn caller_policy(&self) -> (bool, bool);
    fn can_approve_iteration(&self) -> bool;
    fn tool_execution_summary(&self) -> AgentToolExecutionSummary;
    fn transcript(&self) -> Vec<AgentTranscriptEntry>;
    fn replace_last_action_with_reply(&mut self, reply: &str) -> bool;
}

pub trait AgentLoop: Send + Sync {
    fn create(&self, api: LlmApiConfig, kind: &str) -> Result<Box<dyn AgentSessionApi>, RuntimeError>;
    fn restore(&self, snapshot: AgentSessionSnapshot) -> Result<Box<dyn AgentSessionApi>, RuntimeError>;
}

static LOOP: OnceLock<Box<dyn AgentLoop>> = OnceLock::new();
pub fn install_agent_loop(loop_: Box<dyn AgentLoop>) -> bool;   // 同 install_plugin_builder
pub fn agent_loop() -> Option<&'static dyn AgentLoop>;
pub fn missing_loop_error() -> RuntimeError;
```

`RuntimeHost` 的 `agent_sessions` 由 `BTreeMap<String, ManagedAgentSession>` 变成
持有 `Box<dyn AgentSessionApi>`；`ManagedAgentState::PluginIteration(Box<...>)` 里的
`PluginIterationAgentState` 一并移入 `cordis-agent`。

## 依赖方向

```
cordis-cli → { cordis-runtime, cordis-agent, cordis-tooling }
cordis-agent → cordis-runtime          （用 agent_api + config + error）
cordis-runtime ↛ cordis-agent          （只认 trait，不认实现）
```

kernel 不能在正常依赖里出现 `cordis-agent`，否则回到循环依赖。默认实现
（`DefaultAgentLoop`，包住现有 `AgentSession`）保留在 kernel 的 `agent.rs` 里，
由 kernel 自己在 `RuntimeHost::boot` 里注册；`cordis-agent` 装上时覆盖它
（`install_agent_loop` 后装者生效，或首次安装生效——与 builder 一致：幂等、先到先得）。

## 分阶段（每阶段可独立编译、测试、提交）

1. ✅ 抽 `agent_api`（已完成，`ac4d055`）。
2. `agent_loop.rs`：`AgentSessionApi` + `AgentLoop` + 注入点；`AgentSession` 实现
   `AgentSessionApi`；`host` 改持 trait object；默认工厂注册在 `boot`。
   **行为不变**，全部测试必须过。
3. 建 `crates/cordis-agent`，把循环（`AgentSession` 本体、DSML、prompt、history、
   压缩、快照）搬过去；kernel 只留 `default_agent_loop` 的薄壳与 `SessionCaller`。
4. 把 `iteration/` 与 `host/iteration.rs` 搬进 `cordis-agent`（它们只用到循环 + 工具执行
   + 构建器）。
5. 文档与 `docs/rs-files-responsibility.md` 同步；coverage 正则去掉 `agent.rs` 排除
   （循环此时在可测 crate 里）。

## 进展
- 注入路径已打通（`242298a` 之后）：`crates/cordis-agent` 存在，`cordis_agent::install()`
  在 cli 的 `main` 里最先调用（和 `install_plugin_builder` 并列）；`CargoAgentLoop`
  目前把 create/restore 委托给 kernel 的 `KernelAgentLoop`——这是过渡形态，下一步把
  实现搬过来、kernel 删掉 `KernelAgentLoop`。
- 线上验证：05:18 注入事件 → `qq_send` → `sent 1 message(s) through tools`。


- 第二步的接缝已落地：`agent_loop.rs` 提供 `AgentSessionApi` / `AgentLoop` /
  `install_agent_loop`，`KernelAgentLoop` 是默认实现（包住现有 `AgentSession`），
  行为不变，runtime 1003 / cli 49 通过。
- provider 问题按**方案 1** 解决：`LlmProvider` 提升为 `pub`，由 host 继续负责
  "选哪个 provider"（profile 回退、能力节点查找都在 host），循环只接收
  `&dyn LlmProvider`。理由：provider 的**选取**依赖插件注册表，那是 kernel 的机制；
  循环只需要一个补全接口。
- 下一步的两个具体前置（已确认）：`PluginIterationAgentState` 目前是私有 `struct`，
  trait 要给它一个入口就必须先 `pub`（或把 iteration 状态随循环一起搬走，即第 4 步
  提前）；`ManagedAgentSession` 的 `#[derive(Debug)]` 也要改成手写 impl，因为
  `Box<dyn AgentSessionApi>` 不是 `Debug`。
- 下一步：把 `ManagedAgentSession.session` 换成 `Box<dyn AgentSessionApi>`，构造点
  （`agent_start_with`、`detect_crash_and_recover`、`host/iteration.rs`）全部改走
  `agent_loop()`，然后建 `crates/cordis-agent` 搬运循环本体。

## 第二步踩到的硬点（2026-10-31，已按方案 1 解决）

`AgentSessionApi::respond` 不能只收 `&dyn AgentToolHost`：真正的一轮入口是

```rust
pub(crate) fn respond_with_runtime_host_via_images<H: AgentToolHost + ?Sized>(
    &mut self, host: &H, session_id: &str, input: &str, images: &[Value], notes: &str,
    provider: &dyn LlmProvider,                 // ← pub(crate)
    sink: Option<Arc<dyn crate::llm_sink::TokenSink>>,
) -> Result<AgentReply, RuntimeError>
```

`LlmProvider` 是 `pub(crate)`，而 provider 的**选取**（配置解析、profile 回退）也在
kernel。所以接缝必须同时抽象"谁提供补全"：要么把 `LlmProvider` 提升成 `pub` 并
随 trait 一起暴露，要么让循环自己拿 provider 名 + 配置去查 `llm_complete` 能力节点。
后者更符合"传输已移出 kernel"，代价是循环要知道能力节点约定（可接受，它本来就是
协议的一部分）。

结论：第二步开始前先定这一点，否则 trait 签名会返工。

## `agent.rs` 切分图（2026-10-31 实测行号，第二轮校正）

上一版行号是旧修订的，照抄会切错（第一次尝试在 2093 行断在函数中间）。以下由
花括号匹配算出，可直接用：

**移入 `cordis-agent`（循环本体）**
`591–1079`（`SessionCaller` 之后到 `impl AgentSession` 之前：`LlmProvider`、
`LlmCompletionParts`、`AgentSession`、快照、`AGENT_TOOL_END_TURN`、`ShellAgentSession`、
`estimate_tokens`）、`1080–2331`（两个 impl + 失败提示/镜像/消息转换）、
`2583–3415`（`RuntimeShellAgentBackend`、DSML、`dsml_protocol_prompt`、`shell_agent_tools`、
人格/conduct/system prompt）、`3434` 之后（测试）。

**留在 kernel（工具 ABI + 宿主实现）**
常量行 `59–64`、`72`、`172–191`；`81–83`、`104–171`（内省工具过滤）；
`198–207`（`SessionCaller`）；`208–1072`（`impl AgentToolHost`）；
`2332–2451`（全部参数结构体）；`2453–2582`（`execute_agent_tool_call`）；
`3416–3433`（`parse_json_or_string`、`parse_tool_value_arguments`、`to_json_value`）。

第一次尝试失败的原因记录下来：用旧行号切，`agent.rs` 在中途被切断，编译报
`unexpected closing delimiter`；已回滚，仓库保持绿色。

## `agent.rs` 切分图（旧版，行号已失效，仅留内容归属）

| 行段 | 内容 | 归属 |
|---|---|---|
| 31–207 | inject queue、`AGENT_TOOL_*` 常量、`SessionCaller` | **kernel**（工具 ABI） |
| 208–1078 | `impl AgentToolHost for RuntimeHost` | **kernel**（宿主实现） |
| 1079–1176 | `LlmProvider`、`LlmCompletionParts`、`AgentSession`、快照、`AGENT_TOOL_END_TURN`、`ShellAgentSession`、`estimate_tokens` | **cordis-agent** |
| 1177–2267 | `impl AgentSession`、`impl ShellAgentSession` | **cordis-agent** |
| 2268–2331 | `flush_tool_failure_prompts`、`is_outage_notice_text`、`llm_message_to_request` | **cordis-agent** |
| 2332–2452 | 工具参数结构体（`ReadFileArgs` 等） | **kernel**（ABI） |
| 2453–2583 | `execute_agent_tool_call` | **kernel**（分派） |
| 2584–2778 | `RuntimeShellAgentBackend` | **cordis-agent**（后端） |
| 2779–3042 | DSML 解析 + `dsml_protocol_prompt` | **cordis-agent** |
| 3043–3262 | `shell_agent_tools` | **cordis-agent** |
| 3263–3415 | `PERSONA_DISCLAIMER`、`CONDUCT_CONTRACT`、`shell_agent_system_prompt` | **cordis-agent** |
| 3410–3429 | `emit_agent_diagnostic`、`parse_json_or_string`、`parse_tool_value_arguments`、`to_json_value` | 两边都要（公开） |

搬完后的依赖形态：`cordis-agent` → `cordis-runtime`（用 `agent_api`、`agent_loop` 的 trait、
`llm_sink::TokenSink`、`config`、`iteration::report`、`kernel::plugin_iteration` 的数据类型）；
kernel 里不再出现 `AgentSession`，只有 `Box<dyn AgentSessionApi>` 与 `KernelAgentLoop`
（`KernelAgentLoop` 也要搬去 crate，kernel 的 `boot` 改为“没装就报错”，cli 负责注入）。

## 搬 crate 前必须公开的项（已核对，2026-10-31）

循环现在用到的 kernel 内部件，全部要从 `pub(crate)` 提升：

| 项 | 位置 | 现状 |
|---|---|---|
| `llm_sink`（`TokenSink`） | `lib.rs:15` | `mod` → 需 `pub mod` |
| `agent_toolchain`（`sanitize_tool_chains`） | `lib.rs:4` | `pub(crate) mod` → `pub mod` |
| `iteration::policy::*`（`test_changes` / `promotion_verdict` / …） | `iteration/policy.rs` | `pub(crate) fn` |
| `iteration::report::*`（`iteration_result_json` / `approve_iteration_permission`） | `iteration/report.rs` | `pub(crate) fn` |
| `host::PluginIterationAgentState` | `host/mod.rs` | 已 `pub`（本轮改） |
| `host::PluginIterationAgentBackend` | `host/mod.rs` | 已 `pub(crate)`（本轮改） |

`guard` / `image_input` / `soul` / `kernel::notify` / `plugin::node_catalog` 已经是 `pub mod`，
不需要动。

公开这些之后，`crates/cordis-agent` 的 `lib.rs` 就是：
`pub struct CargoAgentLoop;` 实现 `AgentLoop`（create/restore → `AgentSession`），
`pub fn install()` 调 `install_agent_loop`，cli 里在 `RuntimeHost::boot` 之前调用即可。

## 搬本体前的最后一份引用清单（2026-10-31 核对，已最小化）

kernel 里指向 `crate::agent::` 的地方只剩 6 处，全部是类型或测试构造：

```
host/mod.rs:1-5          AgentBackend/AgentReply/AgentSessionStatus/
                         AgentToolExecutionSummary/AgentToolSpec/AgentTranscriptEntry
                         → 全部改从 agent_api
host/tests.rs            AgentToolHost → agent_api
host/region_3500_5500_seam_tests.rs   AgentTranscriptEntry → agent_api；
                         AgentSession::new ×1 → agent_loop().create(...)
host/cov_fa_host_1_3500_tests.rs      AgentSession::new ×1 → 同上
agent_loop.rs            AgentSession（KernelAgentLoop + impl）→ 随循环搬走
```

`UNKNOWN_TOOL_STRIKE_LIMIT` 已移到 `agent_tools`（`4e9aad1` 之后），所以循环搬走后
kernel 不再需要 `agent.rs`。

## 第 10 轮结论：搬循环与搬 iteration 必须同时进行

把 `agent.rs` 复制进 crate 后，编译**能过**，但 `clippy -D warnings` 立刻报 kernel 里
的迭代后端成了死代码：

```
PluginIterationAgentState 多个字段从未被读取
ContextFilesScope 从未使用
PLUGIN_AGENT_TOOL_LIST_CONTEXT_FILES / READ_CONTEXT_FILES 从未使用
```

原因：`PluginIterationAgentBackend` 的 `tool_specs`/`execute_tool` 只被**循环**调用；
循环一走，kernel 里就没有调用者，于是整条迭代装配在 kernel 侧失去引用。也就是说
"循环搬走"和"`host/iteration*.rs` + `iteration/` 搬走"是**同一次改动**，不能分两轮。

已完整回滚到 `bbcfa3a`（绿色），把这条结论落盘。下一轮按"循环 + 迭代一起搬"执行。

## 第 11 轮设计决定：状态归会话，宿主只提供事实

要让循环和迭代装配一起搬走，必须先解决一个结构问题：`ManagedAgentSession` 现在
同时持有 `Box<dyn AgentSessionApi>` 和 `ManagedAgentState`（里面是
`PluginIterationAgentState`）。状态留在宿主，迭代后端就永远搬不走。

决定：**状态属于会话，不属于宿主**。

```
ManagedAgentSession { handle, session: Box<dyn AgentSessionApi> }   ← 只剩这两个
```

- 迭代会话在 crate 里是 `IterationSession { inner: AgentSession, state: PluginIterationAgentState }`，
  自己实现 `AgentSessionApi`；宿主只调 `respond`，不再区分种类；
- 因此 trait 上的 `respond_with_iteration` 消失，只剩 `respond`；
- 迭代后端要读的两样东西（`fixtures_root`、`plugin_iteration_policy`）改由
  `AgentToolHost` 提供（宿主本来就该提供事实，不该由循环反查具体类型）；
- `AgentLoop::create` 需要接受可选的迭代上下文（`prepared` + `context_paths`），
  即 `create(config, kind, Option<IterationSeed>)`。

这样 `iteration/`、`host/iteration*.rs`、`agent.rs` 三者可以一起搬，宿主侧不留残余。

## 第 12 轮结论：dev-dependency 环会造出两份 runtime，测试方案必须重做

循环搬进 crate 后，库能编，但 `cordis-runtime` 的测试撞墙：

```
error: multiple different versions of crate `cordis_runtime` in the dependency graph
error: the trait bound `AgentSession: AgentSessionApi` is not satisfied
```

原因：`cordis-runtime` 为了在测试里拿到 loop，dev-depend 了 `cordis-agent`，而
`cordis-agent` 正常依赖 `cordis-runtime`。cargo 于是编两份 runtime（一份带 `cfg(test)`、
一份不带），`AgentSession` 与 `AgentSessionApi` 来自不同副本，类型不统一。**这条环
不能用来给测试提供 loop。**

两点结论，下一轮必须先解决其一：

1. **测试跟着代码走**：`cordis-agent` 自带针对循环的测试（`imp.rs` 里已经带了 1000+
   行测试，它们是随文件一起搬过去的）；runtime 侧只保留"接缝"测试，用 trait 的
   **测试替身**（`#[cfg(test)] struct FakeSession`）覆盖 `auto_save_session` 这类宿主逻辑；
2. **集成测试换装配**：`tests/*.rs` 里 15 个 `RuntimeHost::boot` 的用例同样拿不到
   loop；它们应改为安装一个 runtime 内部的测试替身，或者整体搬到 `cordis-agent`
   的集成测试里去跑。

已完整回滚到 `04678de`（绿色）。

## 第 15 轮：循环相关集成测试批量迁移完成

9 个 `tests/*.rs`（`runtime_host`、`host_coverage`、`host_llm_coverage`、
`host_coverage_rest`、`host_reload_arms`、`agent_toolchain_400`、`host_ops_arms`、
`host_iterate_stages`、`host_build_coverage`）已移到 `crates/cordis-agent/tests/`，
每个文件顶部用 `#[ctor]` 装 loop。

结果：`host_boot_session_arms` 25/25、`agent_toolchain_400` 6/6、`boot_with_loop` 1/1
等绝大多数通过；`runtime_host` 里 **5 个迭代用例仍失败**，与第 3 轮在 `98ebff2`
上复现的是同一批（canary 判定 `Partial`），属既有问题，不是迁移引入。

## 第 16 轮：循环本体已搬出 kernel ✅

- `crates/cordis-runtime/src/agent.rs` 删除；循环（3303 行）在 `crates/cordis-agent/src/imp.rs`，
  `impl AgentSessionApi for imp::AgentSession` 也搬过去了；
- kernel 的 `agent_loop.rs` 只剩 trait + 注册表 + `missing_loop_error` + **测试替身**
  （`#[cfg(test)] mod test_loop`），`boot` 在 `cfg(test)` 下装替身；
- 宿主测试用的两处 `AgentSession::new` 改为 `agent_loop().create(...)`；
- kernel 里 `LlmCompletionParts` 字段、`sanitize_tool_chains`、`RepairStats`、
  `PluginIterationAgentState/Backend`、`ContextFilesScope` 公开（跨 crate 使用）。

验证：kernel lib 965 通过（从 1003 减去随文件搬走的 36 个循环测试）、crate lib 38 通过、
cli 49 通过、clippy 0 error；线上 06:03 重启后注入事件 → `qq_send` → `sent 1 message(s)`。

## 迭代测试的剩余 5 个失败：已定位到两条互相矛盾的期望（2026-10-31）

逐个隔离跑过的结果：

| 用例 | 期望 | 实测 | 原因 |
|---|---|---|---|
| `boot_recovers_plugin_iteration_journal` | Blocked | Promoted | 与下面两条同源 |
| `blocks_without_canary_evidence_and_approve_promotes` | Blocked | Promoted | 同上 |
| `rollback_candidate_restores_...` | Blocked | Promoted | 同上 |
| `agent_adds_dist_child_plugin_and_promotes` | — | 未单独验证 | 待查 |
| `agent_retries_on_warning_and_promotes` | — | 未单独验证 | 待查 |

关键发现：`shell_entry` 在宿主启动/自检时就被调用过，因此它**总是在 recorder 里**。

- 旧规则（任何副作用都跳过）→ 它被跳过 → canary 无证据 → `Partial` → 三个用例的
  `Blocked` 成立，但 `promotes_after_canary_replay` 永远失败（它明确期望 replay 生效）；
- 新规则（只有会改变状态的副作用才跳过）→ `shell_entry` 可 replay → 该用例通过，
  但另外三个"期望无证据"的用例变成 Promoted。

**已排除一个误判**：在保守规则下单独跑 `rollback_candidate`，同样得到 Promoted。
说明这 4 个 Blocked 期望的失败**与副作用规则无关，且在隔离运行下也复现**——它们是
既有问题（第 3 轮在 `98ebff2` 上复现过），并且在整文件并行跑时集合会漂移，属**顺序相关**。

也就是说这 5 个用例里，**至少有一组期望必须改**：要么承认"启动期对终端节点的调用"
算 canary 证据（改三个 Blocked 用例），要么明确把启动期探测排除在 recorder 之外
（改 replay 用例）。这是产品判断，不是我能单方面定的——**需要你拍板**。

当前保留新规则（语义上更正确：重放 `echo` 两次无害），`promotes_after_canary_replay`
已通过；其余 4 个仍是既有的、顺序相关的失败，需要单独调试（每个 ~70s）。

另一个待查线索：`host.invoke` 每次成功都会 `record_invocation_sample`，所以"启动期是否
探测过 `shell_entry`"决定了这些 Blocked 用例的输入。要彻底说清，需要打印迭代时的
recorder 内容——留给下一轮。

## 第 19 轮：canary 门控是错的，已回退；真正要改的是两条过期断言

实验过程与结论：

1. 打印 `rollback_candidate` 的迭代结果 → `canary=Partial(no_canary_evidence)`、
   `verifier=Pass`、`changed=["plugins/shell/src/lib.rs"]`、`blocked_reason=None` → Promoted。
2. 因此"`Partial` 的 canary 应该挡住晋升"看起来是缺失的门控，于是加进
   `finalize_plugin_iteration`：`canary_unproven || agent.requires_approval`。
   结果：3 个 edit-plan 用例通过，但 **kernel 自己的单元契约测试失败**：
   `finalize_partial_canary_does_not_block_a_tested_change` 明确断言
   "verifier 通过 + canary Partial → 不阻塞"。
3. 改成只对"盲改"（无 agent 的 edit plan）生效后，`agent_*` 用例仍因 verifier
   `RolledBack` 失败（与门控无关）。

结论：**当前 kernel 的契约就是"Partial 不阻塞"**，而三个集成用例期望 Blocked——
它们编码的是旧策略。要不要改这三条断言（`rollback_candidate`、
`blocks_without_canary_evidence_and_approve_promotes`、`boot_recovers_journal`）
是产品决定：要么承认"仅测试通过即可晋升"，三条改成 Promoted/RolledBack；要么
把门控加回去并同时改掉那条单元契约测试。**已回退门控，等拍板。**

现状：kernel 965 通过；`runtime_host` 31/33，剩 2 个 agent 驱动的用例
（`agent_adds_dist_child`、`agent_retries_on_warning`）因 verifier 回滚失败。

## 第 20 轮：agent 迭代用例的 mock 必须迁到 DSML（进行中）

打印失败原因后确认：两个 agent 驱动的用例仍在用**原生 `tool_calls`** 的 mock SSE 脚本，
而循环已改成 DSML 文本协议，于是工具一个都没执行、verifier 拿不到编辑、回滚。

把 `tool_call_response()` 改成发 DSML 文本后，agent 真的跑起来了：

```
tool summary: total_calls=7 successful_calls=7 failed_calls=0
  tool_names=replace_file_exact, run_plugin_test, scaffold_child_plugin
```

但回合计数变了：脚本按"一个 response 一批调用"写，而 DSML 下同一批调用在一个
文本块里、循环的结束条件也不同（原生协议靠 `finish_reason: tool_calls` 收尾，
DSML 下要靠一条纯文本回合或 `record_iteration_summary` 的终止语义）。追加一条
收尾回合会让 `requests.len()` 从 4 变成 5，而移除它又会在
"exited without calling record_iteration_summary" 上失败——说明**终止语义在迁移
时丢了**：prompt 里写着"record_iteration_summary 立即结束会话"，循环却没有据此
停止。这才是要修的代码，不是改断言。

本轮把测试文件恢复原状（避免半截改动），已把结论写在这里。

## 第 22 轮：结束判定放错了位置（重要）

把 mock 迁到 DSML 并补上**包装感知**的检测后，请求数从 5 降到 4、会话提前结束，
但 transcript 里 `record_iteration_summary` 从未执行：

```
summary: total_calls=7 ... tool_names=[replace_file_exact, run_plugin_test, scaffold_child_plugin]
blocked: "exited without calling record_iteration_summary"
```

原因：`ends_after_tools` 的 `return` 落在工具执行**之前**的分支上，于是"看到 summary
就直接收尾"，summary 本身没执行。这不是小事——它会让真实迭代同样丢掉 summary。

正确的落点：在**执行完**那一批工具之后、`continue` 之前判断并返回。因为 DSML 会把
未知节点包成 `invoke_plugin`，检测要同时看 `function.name` 与
`arguments.contains("record_iteration_summary")`。

本轮已回滚这次改动（不留错误的提前返回）。下一轮把判定移到执行之后，再迁移 mock。

## 第 22 轮补充：即使 summary 是直接调用，也停在它之前

去掉 mock 里的 `plugin_path`（让 summary 成为直接调用、不再被包成 `invoke_plugin`）后：

```
n=4 requests, tool summary total_calls=7（仍无 record_iteration_summary）
第 5 次请求把 mock 脚本耗尽 → error sending request
```

两种可能，下一轮先分辨：

1. DSML 解析把 `record_iteration_summary` 当成**未知工具**丢掉（strike → 重试提示 →
   多一轮请求）；
2. `execute_tool` 因"requires at least one edit and one successful verification step"
   拒绝了它（需要打印该工具事件的 ok/err）。

打印 `tool_events` 里每一条的 name/ok/error 即可分辨。**不要再凭猜改循环**——这轮
已经因为"停在执行前"回滚过两次。

## 第 23 轮：summary 在 specs 里，却仍未被调用

核对结果：`PLUGIN_AGENT_TOOL_RECORD_ITERATION_SUMMARY` **在**
`PluginIterationAgentBackend::tool_specs()` 里（iteration_agent.rs:533），执行分支也有
（:683），终止钩子也有（:722）。所以不是"未知工具"。

但实测仍只有 7 个工具：

```
requests=4, tool summary total_calls=7
tool_names=[replace_file_exact, run_plugin_test, scaffold_child_plugin]
第 5 次请求连不上（脚本只有 4 条）
```

4 条脚本按序是：scaffold / 5×replace / run_tests / record_summary。第 4 条被消耗了，
却没有产出 summary 调用——说明**它被当成对某条 nudge 的回复**（`run_plugin_test` 的
warning 触发重试），而重试回合里的工具调用没有执行（或解析为空）。

下一轮先加一条诊断日志：每回合打印解析出的 call 名字列表与是否进入执行分支。
这是唯一能分辨"解析为空"还是"执行被跳过"的观测点。

## 第 24 轮：找到真凶——工具目录只算了一次，且算在回合循环之外

`respond_inner` 在进入回合循环**之前**取了一次 `let tool_specs = backend.tool_specs();`。
可迭代后端的工具列表是**按阶段变化**的：

```rust
// iteration_agent.rs:531
if self.state.verification_successes > 0 && self.state.recorded_summary.is_none() {
    tools.push(record_iteration_summary);
}
```

而 `verification_successes` 恰恰是在**循环内部**跑完 `run_plugin_test` 才 +1 的。于是：

- 第 4 回合模型写 `record_iteration_summary`；
- 解析时用的仍是回合开始前的旧目录 → 该名字不在其中 →
  `serialized_call_to_tool_call_at` 回退成 `invoke_plugin` → 被判未知工具 → 重试 → 脚本耗尽。

修复：每个回合开头重新取 `backend.tool_specs()`，并同步刷新那条 DSML 目录 system 消息。

修复后的观测（同一用例）：

```
turn 1 calls=[scaffold_child_plugin]
turn 2 calls=[replace_file_exact ×5]
turn 3 calls=[run_plugin_test]
turn 4 calls=[record_iteration_summary]     ← 不再被包成 invoke_plugin
canary=Pass "candidate replay matched current response"
blocked_reason=None
verifier=Fail   ← 只剩这一处
```

## 第 25 轮：verifier 失败来自 fixture 里的 vision 测试，与迭代无关

把 `verification` 打进断言消息后读到完整报告：

```
StaticCheck: Passed  (cargo check --manifest-path plugins/Cargo.toml)
Tests:       Failed  (cargo test   --manifest-path plugins/Cargo.toml)
  → -p vision --lib: 10 passed; 1 failed
  → stderr: "error: test failed, to rerun pass `-p vision --lib`"
canary = Pass, blocked_reason = None
```

关键：**同一个命令在仓库里单独跑是 11/11 全过**（`cargo test --quiet --manifest-path
fixtures/plugins/Cargo.toml -p vision --lib`）。差异只在于迭代是在**临时工作区副本**里
跑的。所以这是 fixture 测试对运行环境敏感（临时目录/并行），不是 dist 子插件改动引起，
也不是循环搬家的回归。

下一轮两条路，先试第一条：

1. 在临时副本里复现那条 vision 失败并修 fixture 测试（让它不依赖固定 temp 路径）；
2. 或让 `verify_profile` 只跑受影响的包——但这会放松验证，需谨慎。

## 第 26/27 轮：两个独立的谜团，分开处理

### A. vision 测试对"工作区落在 /tmp"敏感（已复现，已修，暂未提交）

`read_local_image_rejects_path_outside_temp_dir` 用
`concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")` 当"temp 之外的路径"。迭代测试把
整个工作区复制到 `/tmp` 下跑，于是 crate 目录本身就在 temp 内，`unwrap_err()` 拿到
`Ok` 就 panic。修复：manifest 落在 temp 内时，在 temp 根的父目录种一个文件当外部路径，
用完删除。仓库内与镜像工作区都验证 11/11。

### B. 迭代用例的失败集合会随负载漂移

同一份代码：31/33（两次）→ 改 fixture 后 28/33（5 个失败）。可疑点不是 fixture 本身，
而是这 33 个用例在单个测试二进制里并行跑，每个都 boot 宿主、跑 cargo、连 mock SSE；
mock 服务器的请求等待上限是 30s，重负载下会误判。这解释了失败集合的漂移。

串行判据已跑完：**28/33，5 个失败——与并行完全一样**。所以并行负载不是根因，
排除。

串行结果里 `agent_adds_dist_child` 的失败报告给出了确定答案：

```
Stages: StaticCheck Passed, Tests Failed
  → -p vision --lib: tests::read_local_image_rejects_path_outside_temp_dir FAILED
  → 该测试用 CARGO_MANIFEST_DIR 当"temp 之外"，而迭代在 /tmp 下的工作区副本里跑
```

所以 vision 这条修复**必须落盘**，它是 dist_child 用例的直接原因。另外 4 个
（journal / blocks_without_canary / rollback / retries_on_warning）需要各自打印
`verification` 与 `canary` 才能定性；同一用例两次跑出现 `canary=Pass` 与 `canary=Fail`
两种结果，说明它们还受"录制样本"的状态影响（每个 host 自己的 recorder，重放到别的
插件节点时会因工件重建而分歧）。

优先级：先落 vision 修复 → 重跑，看 5 个降到几个。

## 未决问题（2026-10-31，下一轮先查）

`cargo test -p cordis-runtime --test runtime_host` 有 5 个 `iterate_plugins` 相关用例失败，
断言点是 canary 判定 `Partial` 而不是 `Pass`：

```
runtime_host_boot_recovers_plugin_iteration_journal
runtime_host_iterate_plugins_blocks_without_canary_evidence_and_approve_promotes
runtime_host_iterate_plugins_agent_adds_dist_child_plugin_and_promotes
runtime_host_iterate_plugins_promotes_after_canary_replay
runtime_host_iterate_plugins_agent_retries_on_warning_and_promotes
```

两条线索：(a) 复现稳定，不是 flaky；(b) 该用例传了 `edit_plan`，理论上**不经过 agent
回合**，所以与"会话换成 trait object"大概率无关，可能是环境性的（canary 会 replay
录制调用，并 shell 出 `cargo test --manifest-path plugins/shell/Cargo.toml`）。
下一轮第一件事：在 `git worktree` 里检出 `98ebff2` 跑同一个用例，确认是回归还是既有失败。

## 已知硬点

- **崩溃恢复**：`host/mod.rs::detect_crash_and_recover` 现在直接
  `AgentSession::from_snapshot`，必须改成 `agent_loop().restore(...)`。
- **迭代流水线**：`host/iteration.rs` 直接 `AgentSession::new(_, "plugin_iteration")`，
  同样走工厂；`ManagedAgentState::PluginIteration` 的状态类型要跟着移。
- **测试**：`host/*_tests.rs` 里多处直接造 `AgentSession`，要么走工厂，要么留
  `#[cfg(test)]` 的构造便捷函数。
- **单例**：注入点是 `OnceLock`，测试并行跑时只有一个实现——默认实现必须在
  `boot` 之前注册，否则测试顺序会影响结果（builder 已有同样的约束）。
