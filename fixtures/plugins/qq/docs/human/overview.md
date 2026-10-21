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

### `qq_fetch_messages`
Fetch queued incoming messages. The agent polls this node to check for new messages.
Returns all queued messages and drains the queue.

### `qq_send`
Send a message to a QQ group or private chat.

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
插件 `system_hint` 里硬编码的 `IS directed at you ONLY if …`）。没有该行时退回
保守默认：显式 @ / 点名 "bot"、"Bot"、"机器人" 才响应；其余一律 suspend。

默认策略（可在群里让 bot 用 `qq_set_reply_policy` 改）：

> Reply when the message is clearly addressed to you (contains "bot"/"Bot"/"机器人",
> an @mention of you, or the sender explicitly asks you to answer). Otherwise stay
> silent.

以下情况默认返回 suspend（会话挂起）：
- 群友之间的闲聊
- 表情、贴纸、单字回复
- 不涉及机器人的陈述

## ⚠ 关键代码依赖

以下组件构成消息生命周期，**修改时不可删除**：

| 组件 | 作用 | 后果 |
|------|------|------|
| `MESSAGE_QUEUE` | 消息缓冲区 | 删除后无法入队 |
| `handle_onebot_event()` | HTTP事件→消息入队 | 删除后收不到事件 |
| `start_agent_poller()` | 每5秒 drain 队列→`agent_trigger`→inbox | **删除后消息永远积压** |
| `handle_qq_serve` 中的 `start_agent_poller()` 调用 | 启动 poller | 删除后 poller 不启动 |
