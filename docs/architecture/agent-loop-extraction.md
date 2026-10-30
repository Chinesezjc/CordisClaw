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

## `agent.rs` 切分图（2026-10-31 实测行号）

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
