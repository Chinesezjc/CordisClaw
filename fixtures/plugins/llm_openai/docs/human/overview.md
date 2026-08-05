# llm_openai

OpenAI 兼容的 LLM provider 插件。承载全部供应商专有的传输细节，kernel 只保留
agent 循环。

## 节点

- `llm_complete` — 执行一次补全。

## 配置

传输参数由 kernel 从 `llm_api.yaml` 的 profile 投影后随请求传入（`base_url` /
`api_key_env` / `organization` / `project` / `timeout_ms` / `stream_timeout_secs`）。

**API key 不经 payload 传递**：契约类型只带环境变量名，key 在请求时由插件从进程
环境读取，避免密钥落进日志或崩溃快照。

## token 流式

请求带 `sink_addr` 时，插件边读上游 SSE 边把增量写到该 TCP 地址（行分隔 JSON，
首帧握手）。这是纯旁路——连不上或写失败都不影响补全结果，只是 REPL 不再逐字显示。
