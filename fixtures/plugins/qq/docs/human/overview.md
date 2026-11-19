# QQ Plugin (OneBot v11)

QQ adapter using the NoneBot (OneBot v11) protocol.

## Nodes

### `qq_entry`
Legacy multi-action node: configure, send, status, call.

### `qq_serve` (Task)
Start an HTTP server to receive OneBot v11 message events.
Configure your OneBot client to POST events to `http://<host>:<port>/onebot/event`.

Supports grayscale testing via `allow_groups` whitelist.

### `qq_ws_serve` (Task)
Start a WebSocket server for the OneBot v11 reverse WS connection.
Configure your OneBot client (e.g. Napcat) to connect as a WebSocket client to `ws://<host>:<port>/` (default port 8002).
Inbound WS event frames go through the same dedup / grayscale whitelist / envelope path as `qq_serve`.
Send `{"node_id":"qq_ws_serve","action":"stop"}` to stop the server and release the port; repeated start is idempotent.

### 消息泵线程（poller）

`qq_serve` / `qq_ws_serve` 启动时会拉起一条 poller 线程（`start_agent_poller`）：每 5 秒 drain 一次
`MESSAGE_QUEUE`，把消息包成 envelope 后 `agent_trigger` 给 kernel。它只起一条（`POLLER_STARTED`）。

**stop 必须把它 join 掉**：两个 Task 节点的 `stop` 分支都会调 `stop_agent_poller()`（置
`POLLER_SHUTDOWN` → join → 复位 `POLLER_STARTED`）。2026-10-27 之前这条线程没有退出路径，
`reload /qq` 会 `dlclose` 掉它正在执行的 `qq.so`，导致 SIGSEGV；顺便也会让新旧两个 poller
同时 drain。睡眠是 100ms 分片，所以 stop 最多等一个分片就返回。

### `qq_fetch_messages`
Fetch queued incoming messages. The agent polls this node to check for new messages.
Returns all queued messages and drains the queue.

### `qq_send`
Send a message to a QQ group or private chat.

**出站 @（2026-10-27）**：`message` 里写 `@[id=<qq>]`（或 `@[id=<qq>,name=…]`）会被转成 OneBot 的
`[CQ:at,qq=<qq>]` at 段，也就是真正圈人。发送者的 QQ 号就在消息头 `(user <uid>)` 里，所以
「圈一下刚才说话的人」= ``@[id=<uid>] 收到``。没有 id 的裸 `@昵称` 原样当文字发出；`@[id=abc]`
这类畸形标记同样原样保留，不会吞字。`reply_to` 仍然排在 at 段之前（`[CQ:reply,…][CQ:at,…]`）。

## LunaBot 消息中转

LunaBot 作为 OneBot 消息的主处理器，CordisClaw 作为 fallback：

```
QQ消息 → OneBot客户端
              │
              ▼
         LunaBot (主处理器)
              │
         ┌────┴────┐
         │ 匹配成功  │ 匹配失败
         ▼         ▼
     LunaBot    CordisClaw
     直接回复    qq_serve
                (:8080/onebot/event)
                     │
                     ▼
              Agent 判断意图
              (是否对机器人说话)
                     │
              ┌──────┴──────┐
              │ 是          │ 否
              ▼             ▼
         qq_send 回复    返回 suspend
                        (会话挂起)
```

LunaBot 以 OneBot 消息流原样转发，隐藏自身中转身份。
CordisClaw 不知道消息经过了 LunaBot — 它看到的就是普通 QQ 群消息。

## 配置示例

```json
{
  "node_id": "qq_serve",
  "payload": {
    "port": 8080,
    "onebot_url": "http://127.0.0.1:5700",
    "allow_groups": ["123456789"]
  }
}
```

### `qq_get_group_members`
Raw `get_group_member_list` passthrough for `target: group:<id>`. Agent-accessible
since 5.2.40 — before that the node had no node_doc at all, so the tool firewall
answered `Agent is not allowed to call qq::qq_get_group_members` and the model
used to report it as "this API is closed to me".

### `qq_get_group_roles`
Compact role summary for `target: group:<id>`: `{owner, admins, member_count,
source}`. Aggregates `get_group_member_list` (`role` = `owner`/`admin`/`member`);
when the member list carries no owner it falls back to `get_group_info_ex`
(`extInfo.groupOwnerId.memberUin`).

### `qq_get_reply_policy` / `qq_set_reply_policy`
Read/persist the **reply gate** used for every inbound message. The policy lives
in `data/qq_reply_policy.json` (explicit `data_dir` payload →
`$CORDIS_FIXTURES_ROOT/../data` → cwd-relative `data/`) and is shipped to the
model as a `[reply policy: …]` first line on every message. Changing it takes
effect on the next message — no rebuild, no restart. An empty policy is rejected.

## Agent 意图判断规则

门控规则的**唯一权威**是每条入站消息首行的 `[reply policy: …]`（5.2.40 之前是
插件 `system_hint` 里硬编码的 `IS directed at it ONLY if …`）。没有该行时退回
保守默认：显式 @ / 点名 "bot"、"Bot"、"机器人" 才响应；其余一律 suspend。

### 自 @ 标记（2026-10-27）

模型不能凭 QQ 号判断「这条 @ 的是不是我」——以前 prompt 里根本没有 bot 自己的号，
于是它会把 `@[id=3832224285,name=3832224285]` 当成别人，答「@ 的不是我」。

现在 `OneBotEvent.self_id`（OneBot v11 每个事件都带，NapCat 用
`selfInfo.uin` 填）进到 ingest，@ 到 bot 自己的那条渲染成
`@[id=<qq>,self,name=<n>]`（裸形式 `@[id=<qq>]` → `@[id=<qq>,self]`）。
`system_hint` 里有一条静态规则说明 `self` 标记 = 你。

代价：**没有 @ 的消息逐字节不变**，只有 @ 到 bot 的消息在每个 `@[id=…` 上多
5 个字符；`self_id` 常驻事件、不需要配置，所以每轮 prompt 没有新增行，
prefix cache 行为不变。事件里缺 `self_id`（非标准 adapter）时不加标记，
退化成改之前的行为。

默认策略（可在群里让 bot 用 `qq_set_reply_policy` 改）：

> Decide for yourself, from the conversation context, whether this message needs an
> answer from you. Answer when it is addressed to you ("bot"/"Bot"/"机器人", or an
> @mention carrying the `self` marker — that marker means the mention is you),
> AND ALSO when the context makes it clear the sender is talking to you:
> it continues a thread you are part of, asks you to do or explain something, or
> someone in the group explicitly wants your answer — even without naming you. Stay
> silent for conversation between other people, memes/stickers, and statements that
> do not involve you; do not hijack someone else's thread. If it is genuinely
> ambiguous, prefer silence.

以下情况默认返回 suspend（会话挂起）：
- 群友之间的闲聊
- 表情、贴纸、单字回复
- 不涉及机器人的陈述

## 谁能改策略 / 记忆

改某个群的策略或记忆，要求发起人是**该群的群主或管理员**（角色取自 `get_group_member_list`，10 分钟缓存）；改全局默认策略只认 `data/qq_operators.json` 白名单。角色查不到（bot 没权限、接口失败）一律拒绝。

"谁在要求"不是模型自己说的：kernel 会把本轮的 `actor` 注入每次 `invoke_plugin` 的 `payload.__meta`（模型传入的同名字段会被覆盖），插件据此判权。CLI / 运维直接 invoke 没有 `__meta`，视为操作员并记审计。

## 每群策略 + 每群记忆

两件事都落在 `data/`，由 agent 自己写，**下一条消息即生效**（不改 Rust、不重建、不重启）：

| 文件 | 内容 |
|---|---|
| `data/qq_reply_policies.json` | `{default, groups{gid}}`，每项 `{mode, policy, …}`；老的 `qq_reply_policy.json` 仍当 default 读 |
| `data/qq_group_memory.json` | `{gid: {memory, …}}`，每群备忘 ≤ 800 字符 |

四种 mode：`quiet`（只 @/点名）· `balanced`（语境判断，默认）· `chatty`（主动参与，不抢别人的话头）· `one_on_one`（当私聊，必答）。节点：`qq_set_reply_policy {group?, mode?, policy?}` / `qq_get_reply_policy {group?}`（返回实际生效的 `source`）/ `qq_get_group_memory` / `qq_set_group_memory`。

每条消息首行 `[reply policy: <mode> — <text>]`，写过记忆的群再加 `[group memory: …]`。

## 沉默不是默认

门控的唯一权威是每条消息首行的 `[reply policy: …]`。hint 里显式写明：图片、表情、
闲聊**不是**"立即挂起"的理由——这类消息只是不许调用内核自省工具，是否回复仍按策略判断。
（2026-10-23 之前 hint 里有一条 `… suspend immediately …`，模型据此把沉默当默认，连 @ 都会漏答。）

## 入站图片（多模态）

图片消息在正文里留 `[image: <url>]` 占位符，同时把 URL 放进 envelope 的
`images` 数组。kernel 负责下载、嗅探类型（png/jpeg/webp/gif）、base64 内联成
`image_url` content part，再随**本批** user 消息发给 provider —— 模型因此能直接
看到图，不需要 vision 插件。历史里只保留文本占位符，不会每轮重发。

预算（`config/runtime.yaml` 的 `runtime.images`，缺省即下列值）：
`enabled: true` / `max_bytes: 4194304` / `max_per_batch: 2` / `timeout_ms: 8000`。
超限或下载失败的图只记一行 `image_input: skip url=… reason=…`，本轮照常用文本继续。

## ⚠ 关键代码依赖

以下组件构成消息生命周期，**修改时不可删除**：

| 组件 | 作用 | 后果 |
|------|------|------|
| `MESSAGE_QUEUE` | 消息缓冲区 | 删除后无法入队 |
| `handle_onebot_event()` | HTTP事件→消息入队 | 删除后收不到事件 |
| `start_agent_poller()` | 每5秒 drain 队列→`agent_trigger`→inbox | **删除后消息永远积压** |
| `handle_qq_serve` 中的 `start_agent_poller()` 调用 | 启动 poller | 删除后 poller 不启动 |
