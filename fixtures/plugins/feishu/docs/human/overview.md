# Feishu 插件

飞书(Lark)开放平台协议适配器,与 `qq` 插件同构。收飞书事件订阅回调 → 喂 agent → 回复发回飞书。

## 节点

| node | 类型 | 说明 |
|---|---|---|
| `feishu_serve` | Task | HTTP server(默认 :8100),`POST /feishu/event`。处理 challenge 握手、token 校验、(可选)AES 解密、@机器人门控、去重,入队后由 poller 发结构化 envelope 给 runtime。 |
| `feishu_send` | Router | 出站:发文本 / interactive card,可 `reply_to` 引用回复。target = `chat:<chat_id>` 或 `user:<open_id>`。 |
| `feishu_entry` | Router | `configure`(写 app_id/secret/token 等)/ `status`。 |

## 配置

运行时配置持久化在 `$CORDIS_FIXTURES_ROOT/.cordis-drafts/feishu_runtime_config.json`(0600)。字段:`app_id`、`app_secret`、`verification_token`、`encrypt_key`(可选,开加密时)、`bot_open_id`(群 @ 门控用)、`api_base`(测试可覆盖,默认 open.feishu.cn)。

通过 `feishu_entry` configure 写入,例如:
```
invoke feishu feishu_entry --payload-json='{"node_id":"feishu_entry","action":"configure","payload":{"app_id":"...","app_secret":"...","verification_token":"...","encrypt_key":"...","bot_open_id":"ou_..."}}'
```

## 消息路由

runtime 是协议无关的:poller 发的 envelope 带 `source_plugin=feishu`/`reply_node=feishu_send`/`session_key=feishu:chat:<id>`/`reply_target`/`reply_to`,runtime inbox 按 session_key 分 session,agent `respond` 时自动回调 `feishu_send`。见 `docs/architecture/status-and-open-items.md` 消息路由解耦一节。

## 部署

1. 飞书开发者后台建自建应用,开 `im:message`/`im:message.group_at_msg`/`im:chat`/`contact:user.id` 权限。
2. 事件订阅请求地址填 `https://<域名>/feishu/event`,订阅 `im.message.receive_v1`;记 Verification Token /(可选)Encrypt Key。
3. `feishu_entry` configure 写入凭证。
4. serve 起来后飞书验证请求地址(自动应答 challenge),测试群 @机器人。
