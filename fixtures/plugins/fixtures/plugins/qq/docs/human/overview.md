# qq

QQ adapter using the NoneBot (OneBot v11) protocol.

Connect to a OneBot-compatible QQ client (go-cqhttp, NapCat, LLOneBot) via HTTP API.

## Actions

- `configure` — set OneBot HTTP endpoint URL and default target
- `send` — send a message to a group or private chat
- `status` — report current configuration and connectivity
- `call` — call an arbitrary OneBot API action

## Example

```json
{"action": "configure", "url": "http://127.0.0.1:5700", "target": "group:123456"}
{"action": "send", "message": "Hello from Cordis!"}
```
