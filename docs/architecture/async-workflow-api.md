# Async Workflow API（非宏草案）

本文描述一个“作者写 `async/await`，运行时保持受控语义”的 API 草案。

核心目标：

1. 作者端保持连续函数书写体验。
2. 挂起点只允许框架原语（`call/join/race/wait_event/sleep/ask_user`）。
3. 不依赖 `#[workflow]` 或自定义语法宏。
4. 运行时可把每个等待点映射为 `WaitHandle/Continuation`。

实现入口位于：

- [crates/cordis-plugin-sdk/src/workflow.rs](../../crates/cordis-plugin-sdk/src/workflow.rs)

## 1. 作者视角

作者可以写普通 async 函数：

```rust
use cordis_plugin_sdk::workflow::{
    session, CallSpec, JoinPolicy, JoinSpec, RacePolicy, RaceSpec, WorkflowError, WorkflowRuntime,
};

#[derive(serde::Deserialize)]
struct Manifest {
    url: String,
}

#[derive(serde::Deserialize)]
struct Asset {
    bytes: usize,
}

#[derive(serde::Deserialize)]
struct Card {
    id: String,
}

async fn build_card(runtime: &mut dyn WorkflowRuntime) -> Result<Card, WorkflowError> {
    let mut wf = session(runtime);

    let manifest: Manifest = wf
        .call(CallSpec::try_new("resolver", "resolve_manifest", serde_json::json!({}))?)
        .await?;

    let race = RaceSpec::new(
        RacePolicy::FirstSuccess,
        vec![
            CallSpec::try_new("downloader/a", "download", serde_json::json!({ "url": manifest.url }))?,
            CallSpec::try_new("downloader/b", "download", serde_json::json!({ "url": manifest.url }))?,
        ],
    );
    let asset: Asset = wf.race(race).await?;

    let join = JoinSpec::new(
        JoinPolicy::All,
        vec![
            CallSpec::try_new("validator/hash", "check", serde_json::json!({ "bytes": asset.bytes }))?,
            CallSpec::try_new("validator/signature", "check", serde_json::json!({ "bytes": asset.bytes }))?,
        ],
    );
    let _: serde_json::Value = wf.join(join).await?;

    let card: Card = wf
        .call(CallSpec::try_new("builder", "build", serde_json::json!({ "bytes": asset.bytes }))?)
        .await?;
    Ok(card)
}
```

这个写法有几个关键点：

1. 仍是普通 Rust async 函数。
2. `.await` 的目标是 SDK 提供的受控原语 future。
3. 无需流程宏扩展语法。

## 2. 运行时契约

`WorkflowRuntime` trait 是作者层与内核层的最小边界：

1. `submit_wait(spec) -> WaitHandle`
2. `poll_wait(handle, cx) -> Poll<WaitOutcome>`
3. `cancel_wait(handle) -> Result<(), WorkflowError>`

这允许 runtime 在内部实现：

1. wait handle 追踪
2. generation/tombstone/zombie 处理
3. timeout/cancel 语义
4. unload/drain 时的失效与恢复策略

## 3. 受控挂起点（Builtin Primitive）

当前草案内置六类 `WaitSpec`：

1. `Call`
2. `Join`
3. `Race`
4. `Event`
5. `Sleep`
6. `AskUser`

设计意图是把“可 await 的对象”限制在 runtime 能解释的集合内，避免任意外部 future 破坏系统语义。

## 4. 错误模型

`WorkflowError` 采用结构化 kind：

1. `EncodePayload`
2. `DecodeOutput`
3. `Runtime`
4. `Cancelled`
5. `Timeout`
6. `Zombie`

这让作者层能区分“编解码问题”和“运行时状态问题”。

## 5. 与宏方案的边界

这个 API 草案明确是“非宏作者接口”：

1. 工作流编写不依赖 `#[workflow]` 或 `workflow!`。
2. 插件导出宏（例如 `export_plugin_api!`）仍可独立存在，不影响 workflow 书写模型。

换句话说：

1. 插件入口声明可以是宏。
2. 工作流逻辑本身采用普通 async Rust。

## 6. 当前状态与后续

当前状态：

1. SDK 已有可 await 的 primitive future 封装。
2. 包含最小单测（手动 poll，验证序列化和错误映射）。

后续建议：

1. 在 runtime 侧实现 `WorkflowRuntime` 适配层，把 `WaitSpec` 映射到现有 execution/kernel 组件。
2. 为 `Join/Race` 增加可观测 trace 字段（winner/loser/cancel reason）。
3. 把 `WaitHandle` 与 runtime snapshot generation 绑定，落实 zombie rejection。
