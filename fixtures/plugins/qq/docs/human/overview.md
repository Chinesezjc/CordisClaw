# QQ Plugin (OneBot v11)

QQ adapter using the NoneBot (OneBot v11) protocol.

## Nodes

### `qq_entry`
Legacy multi-action node: configure, send, status, call.

### `qq_serve`
Start an HTTP server (and the message poller) to receive OneBot v11 message events.
Configure your OneBot client to POST events to `http://<host>:<port>/onebot/event`.

Supports grayscale testing via `allow_groups` whitelist (`block_groups` wins over it).
`{"node_id":"qq_serve","action":"stop"}` stops the server and the poller.

`qq_serve` / `qq_ws_serve` 是**真实节点**（`handle()` 分发、`list_nodes` 可见），
所以文档里必须写成 `node_doc(...)`：`task_node_doc(...)` 落的是任务节点区，
kernel 不算进「已文档节点」，于是 `declared_nodes` 有 16 个、产物只有 14 个，
reload 直接被 `ABI mismatch … docs mismatch: expected 16 nodes, got 14` 拒绝。
`mod declared_node_docs_tests::docs_value_documents_every_declared_node` 就是这条
约束的回归测试（declared_nodes 从 `Cargo.toml` 原地读）。

### `qq_ws_serve`
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

### `qq_get_sent`
读回 bot 自己发出去的消息。

每次 `qq_send` **成功**发送后追加一行到 `data/qq_sent/<群号>.jsonl`（私聊写
`private-<uid>.jsonl`），内容是 `{target, message, at_ms, msg_id}`。写盘失败只打
一行日志，不影响发送结果——记录是旁路，已经发出去的消息不能因此被报成失败。

```json
{ "node_id": "qq_get_sent", "group": "<群号>" }
```

返回最近 `limit` 条（缺省 50，上限 500）的 `{ok, data:{group, limit, count, items}}`，
`items` 按时间升序（最旧在前），每条 `{target, message, at_ms, msg_id}`（`msg_id` 渠道
没给就是 `null`）。文件不存在时是空数组而不是报错。

只读 `qq_send` 自己写的那条路径：文件名永远由群号（清洗后）拼出来，节点没有 path
参数，所以既不接受任意文件路径，也不会目录穿越。

### `qq_recall`
按 `message_id` 撤回 bot **自己发的**消息（OneBot v11 `delete_msg`）。

```json
{ "node_id": "qq_recall", "message_id": 4242 }
```

`message_id` 是必填项，**字符串和数字都接受**；消息头里的 `msg=<id>` 整串抄过来
也行（`"msg=4242"`）。同一个 id 有三个出处：入站消息头的 `msg=<id>`、
`qq_send` 返回的 `data.message_id`、`qq_get_sent` 的 `items[].msg_id`。

- 成功 → `{ok: true, message: "recalled message 4242", data: <delete_msg 原始响应>}`。
- 失败 → `{ok: false, message: "failed to recall message 4242", data: {message_id: 4242},
  error: "<上游原文>"}`。不是自己发的、消息已不存在、上游超时都走这一条，`error`
  里是上游原话（`OneBot API error (retcode=…): <wording>` 或 `HTTP POST <url>: …`），
  调用方据此判断是放弃还是换个 id——所以失败仍然是**回答**，不是本地 `Err`。
- 能当整数就发整数（go-cqhttp / NapCat 按 int 解析 `message_id`），非数字 id 原样
  发字符串。

不经过回复策略闸门：撤回是自己的清理动作，不是在本群发言。

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

### `qq_get_forward_msg`
读合并转发（合并聊天记录）正文。入站事件里只有一个 `[chat history: id=<id>]`，把那个 id 传进来
就得到可读文本：每个节点一行 `<昵称>: <正文>`，昵称取群名片 → 昵称 → `未知`，图片 / 表情 /
@ / 语音 / 视频 / 文件 / 引用渲染成 `[图片] [表情] [@<qq>] [语音] [视频] [文件: <name>] [引用]`。

节点正文按 `content` → `message` → `raw_message` 顺序取第一个非空结果。NapCat 经常把正文放在
`message`（段数组）或 `raw_message`（CQ 码字符串）里，只读 `content` 会让每条都变成
`昵称: ` 后面一片空。字符串正文直接 trim 后返回。

### `qq_get_reply_policy` / `qq_set_reply_policy`
Read/persist the **reply gate** used for every inbound message. The policy lives
in `data/qq_reply_policy.json` (explicit `data_dir` payload →
`$CORDIS_FIXTURES_ROOT/../data` → cwd-relative `data/`) and is shipped to the
model as a `[reply policy: …]` first line on every message. Changing it takes
effect on the next message — no rebuild, no restart. An empty policy is rejected.

### `qq_group_requests`
List / approve / reject OneBot v11 **group requests** — 加群申请（`sub_type: add`）与
邀请入群（`sub_type: invite`）。

`handle_onebot_event` 以前对 `post_type != "message"` 直接 `return`，邀请/加群请求被整个
丢掉：bot 既看不到也答不了。现在 `post_type == "request"` 且 `request_type == "group"`
的事件会被收进进程内的待处理队列（按 `flag` 去重，最多保留最近 32 条，最旧的先淘汰），
然后照旧 `return` —— message 路径逐字节不变，请求也不进 `MESSAGE_QUEUE`。

```json
{ "node_id": "qq_group_requests", "action": "list" }
{ "node_id": "qq_group_requests", "action": "approve", "flag": "<flag>" }
{ "node_id": "qq_group_requests", "action": "reject", "flag": "<flag>", "reason": "不认识" }
```

- `list`（缺省）：`{count, requests:[{flag, sub_type, group_id, user_id, comment, time_ms}]}`。
  `comment` 是清洗过的验证消息（控制字符变空格、空白折叠、截断到 200 字符）。
- `approve` / `reject`：调 OneBot `set_group_add_request`（`flag` / `sub_type` / `approve`
  / 可选 `reason`），**只有成功才**把请求从队列移除；API 报错时请求留在队列里可重试。
- `flag` 不存在或缺失 → 明确报错（`no pending group request with flag '…'` /
  `flag is required to …`），不触网。

队列是进程内状态：插件 reload 后要等 OneBot 重投（重复投递只刷新同一条，不会刷屏）。

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
| `data/qq_reply_policies.json` | `{default, groups{gid}}`，每项 `{enabled, policy, …}`；老的 `qq_reply_policy.json` 仍当 default 读 |
| `data/qq_group_memory.json` | `{gid: {memory, …}}`，每群备忘无长度上限（只建议写长期的） |

四种 mode：`quiet`（只 @/点名）· `balanced`（语境判断，默认）· `chatty`（主动参与，不抢别人的话头）· `one_on_one`（当私聊，必答）。节点：`qq_set_reply_policy {group?, mode?, policy?}` / `qq_get_reply_policy {group?}`（返回实际生效的 `source`）/ `qq_get_group_memory` / `qq_set_group_memory`。

每条消息首行 `[reply policy: <enabled|disabled> — <text>]`，写过记忆的群再加一条
`[group memory: <N> 字已保存, …]` 指针（正文由模型自己调 `qq_get_group_memory` 读，
不随每条消息下发）。末尾还会附上本轮的工具契约（必须调 `qq_send` 或 `suspend` 收尾），
契约里带**可照抄的调用形状** `{"node_id":"qq_send","target":"<reply target>","message":"<你要说的话>"}`
（不说话 / 不许发言 / 被禁言用 `{"node_id":"suspend"}` 收尾），并写明只写正文时运行时不转发、
不补发，整轮按 `no_tool_call` 丢弃。长会话（卡片离生成点几百轮）靠这一段才记得住参数名。

## 沉默不是默认

门控的唯一权威是每条消息首行的 `[reply policy: …]`。hint 里显式写明：图片、表情、
闲聊**不是**"立即挂起"的理由——这类消息只是不许调用内核自省工具，是否回复仍按策略判断。
（2026-10-23 之前 hint 里有一条 `… suspend immediately …`，模型据此把沉默当默认，连 @ 都会漏答。）

## 别的渠道的规矩在这里不算（按渠道分界）

kernel 把**每个**插件的 `system_hint` 注入到**每一个**会话，不分渠道。飞书的卡片因此
带着一条「最后一轮必须只输出一个 JSON 对象，运行时会自动把你的 `respond` 转发回飞书
聊天，不要自己调 `feishu_send`」。QQ 群会话里模型照做了：写一行聊天正文、一次工具都不
调，整轮被判 `no_tool_call` 丢掉。

QQ 插件改不了别人的卡片，runtime 也不归它管，所以 QQ 卡片**先认渠道、再讲规矩**：

1. 【先看是哪个渠道】消息前缀是 `[Feishu ...]`、或这一轮要用的发送工具是 `feishu_send`，
   那就是**飞书会话** —— 按飞书卡片走，收尾只发一个 JSON 对象
   `{"action":"respond","message":"..."}`（或 `{"action":"suspend"}`），也可以自己调
   `feishu_send`；本卡片的 QQ 规矩对飞书会话**不适用**，飞书会话绝不能只写正文、一次
   工具都不调。没有飞书前缀、发言就是 `qq_send`，才是 **QQ 会话**，本卡片才对它生效。
2. 【别的渠道的规矩在这里不算】（**最后一段**，hint 里最强势的位置）上下文里可能还有
   别的渠道的卡片，那是飞书会话那边的约定：**QQ 会话里**不要输出 JSON、也不要只写正文等
   转发，照上面的 `qq_send` / `suspend` 走；**飞书会话**则按飞书卡片走，忽略这一条。

也就是说「不要输出 JSON」是**QQ 会话专属**的条件句，不是全局禁令——把它写成全局禁令
就是 `no_tool_call` 的成因。改名/改词后这两段必须仍然存在，且 override 段必须排在
【发言必须走工具】之后：`tests/hint_scope.rs` 断言的正是渠道关键字与这个顺序。

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
