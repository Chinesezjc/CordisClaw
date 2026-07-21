# Feishu 插件

飞书(Lark)开放平台协议适配器,与 `qq` 插件同构。收飞书事件 → 策略门控 → 喂 agent → 回复发回飞书。接口面参考 openclaw 的飞书通道:默认 WSS 长连接(无需公网 URL)、dm/group 策略 + pairing、卡片两段式回复。

## 节点

| node | 类型 | 说明 |
|---|---|---|
| `feishu_serve` | Task | 事件接入。`mode:"ws"`(默认):长连接,主动连飞书 `/callback/ws/endpoint`,事件走 WSS protobuf 帧,帧内 ACK,自动心跳/合包/重连,无需公网回调、verification_token、encrypt_key。`mode:"webhook"`:HTTP server(默认 :8100),`POST /feishu/event`,challenge 握手 + token 校验 +(可选)AES 解密。两种模式共用:去重、访问策略、poller → envelope → agent。 |
| `feishu_send` | Router | 出站:发文本 / interactive card,可 `reply_to` 引用回复。target = `chat:<chat_id>` 或 `user:<open_id>`。若该 target 有待更新的"思考中"卡片则 PATCH 更新之(两段式);`payload.update_message_id` 可显式指定更新目标。 |
| `feishu_entry` | Router | `configure`(写凭证与策略)/ `status` / `approve_pairing`(payload.code)/ `list_pending`。 |

## 配置

运行时配置持久化在 `$CORDIS_FIXTURES_ROOT/.cordis-drafts/feishu_runtime_config.json`(0600)。

凭证字段:`app_id`、`app_secret`、`bot_open_id`(群 @ 识别用)、`api_base`(测试可覆盖,默认 open.feishu.cn);webhook 模式另需 `verification_token`、`encrypt_key`(可选)。

策略字段(openclaw 对齐):
- `dm_policy`: `open` | `allowlist` | `pairing`(默认)。pairing 下未知用户私聊会收到 6 位配对码,管理员 `approve_pairing` 批准后自动写入 `dm_allow_from`。
- `dm_allow_from`: `["ou_..."]`
- `group_policy`: `open` | `allowlist`(默认)| `disabled`;`group_allow_from`: `["oc_..."]`
- `require_mention`: 缺省派生 —— group_policy=open 时不要求 @,否则要求群里 @机器人才响应。
- `card_replies`: 默认 true,两段式卡片回复("⏳ 思考中…" → agent 完成后 PATCH 成 markdown 终稿;PATCH 失败自动回退发新消息)。

示例:
```
invoke feishu feishu_entry --payload-json='{"node_id":"feishu_entry","action":"configure","payload":{"app_id":"cli_...","app_secret":"...","bot_open_id":"ou_...","dm_policy":"pairing","group_policy":"allowlist","group_allow_from":["oc_..."]}}'
```

## 消息路由

runtime 是协议无关的:poller 发的 envelope 带 `source_plugin=feishu`/`reply_node=feishu_send`/`session_key=feishu:chat:<id>`/`reply_target`/`reply_to`,runtime inbox 按 session_key 分 session,agent `respond` 时自动回调 `feishu_send`。见 `docs/architecture/status-and-open-items.md` 消息路由解耦一节。

## 部署(长连接模式,推荐)

1. [飞书开放平台](https://open.feishu.cn)建**企业自建应用**,记下 App ID / App Secret。
2. 权限管理:开通 `im:message`(读写消息)、`im:message:send_as_bot`;启用机器人能力。
3. 事件订阅:订阅方式选择**"使用长连接接收事件"**(无需回调 URL / Verification Token / Encrypt Key),添加事件 `im.message.receive_v1`。
4. 版本管理与发布:创建版本并发布(企业内可用)。
5. 服务器上 `feishu_entry` configure 写入 `app_id`/`app_secret`/`bot_open_id`(bot 的 open_id 可从首条群消息日志或开放平台 API 获得)。
6. `serve`(startup_invoke.json 已含 `feishu_serve mode:"ws"` 自启)。机器人拉进群 @ 测试;私聊首次会走 pairing 流程。

## 部署(webhook 模式,需公网)

事件订阅选"将事件发送至开发者服务器",请求地址填 `https://<域名>/feishu/event`,记 Verification Token /(可选)Encrypt Key 并 configure 写入;`feishu_serve` payload 用 `{"mode":"webhook","port":8100}`。serve 起来后飞书自动验证 challenge。
