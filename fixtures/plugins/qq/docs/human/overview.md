# QQ Plugin (OneBot v11)

QQ adapter using the NoneBot (OneBot v11) protocol.

## Nodes

### `qq_entry`
Legacy multi-action node: configure, send, status, call.

### `qq_serve` (Task)
Start an HTTP server to receive OneBot v11 message events.
Configure your OneBot client to POST events to `http://<host>:<port>/onebot/event`.

Supports grayscale testing via `allow_groups` whitelist.

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

## Child Plugins

### `cd` — Continuous Deployment
Handles automated deployment tasks triggered from QQ messages.
Node: `cd_entry`

## Agent 意图判断规则

Agent 通过 LLM 自行判断消息是否对它说的。以下情况会触发响应：
- 显式提到机器人名称、"Cordis"、"bot"、"机器人"
- 直接提问或命令语气
- 包含疑问词（how、why、what、怎么、为什么、帮我）

以下情况返回 suspend（会话挂起）：
- 群友之间的闲聊
- 表情、贴纸、单字回复
- 不涉及机器人的陈述
