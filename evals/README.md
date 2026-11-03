# 回归集合：真实轮次

从 `data/sessions/*.json` 里把**每一轮真实对话**取出来，作为模型回归/训练的语料。
输入是运行时当时写给模型的那段文本，不是重新渲染的近似值。

## 用法

```bash
python3 evals/extract.py                      # 全部轮次 -> evals/cases.jsonl
python3 evals/extract.py --only-flagged \     # 只留能证明出问题的轮次
    --out evals/failures.jsonl
```

两个产物都在 `.gitignore` 里：它们含真实群聊内容，而且体积有几十 MB。

## 一条 case 长什么样

```json
{
  "id": "agent-session-1793382306125-1-204",
  "session": "agent-session-1793382306125-1",
  "context": [{"input": "…上一轮的批次…", "text": "…上一轮模型说的话…"}],
  "input": "CURRENT_INCOMING_BATCH_START\n[QQ group …]: 你呢\n…",
  "calls": [{"name": "invoke_plugin", "arguments": {"…": "…"}, "ok": true}],
  "text": "模型直接输出的文本（没有工具调用时才有）",
  "flags": ["malformed_call", "tool_error"],
  "verdict": {"label": null, "failure": null, "expected": null}
}
```

- `context`：之前最多 6 轮，旧的在前面，让一条 case 自带上下文；
- `input`：本轮批次原文（含 `CURRENT_INCOMING_BATCH_START/END` 与策略行）；
- `calls`：本轮真正执行过的工具调用，按顺序；
- `flags`：机器能证明的异常，见下；
- `verdict`：留空给人标。`label` 取 `good` / `bad`，`failure` 写错在哪，
  `expected` 写应该怎么做（例如 `suspend` 或 `先问清楚指的是什么`）。

## flags 的含义

| flag | 由什么推出 | 说明 |
|---|---|---|
| `tool_error` | 工具结果 `ok:false` 或输出里 `"ok":false` | 动作没成 |
| `malformed_call` | 错误文本命中 `target is required` / `unknown tool` / `not a function` 等 | **调用本身写错**，不是动作失败；双层 `payload_json` 就落在这里 |
| `duplicate_send` | 同一轮里发了完全相同的消息 | 重试把同一句话发了两遍 |
| `no_message_sent` | 本轮没有任何发送 | 正常沉默，或"想说但没出手" |

## 看不到的东西

- **被 harness 逼着说话**：`agent_ended_without_action` / `agent_missing_end_tool`
  只写进 journal，不进 transcript。要塞进 case，得按时间把
  `journalctl -u cordisclaw` 的这两行 join 到对应轮次上（`inbox: [qq:group:…] batch`
  那行给出 group 与时间）。
- **模型为什么这么说**：reasoning 只记了长度，没有正文。要还原得在
  `CORDIS_LLM_DEBUG` 下把请求/响应整体落盘。

## 喂给训练前

1. 先定位 `malformed_call`：这类是**格式**错误，和内容质量无关，训练时别混进
   内容样本里，否则模型会学到"写错参数"也算好答案。
2. 再人标 `verdict`：一个 case 只说"错"没用，要写清期望（沉默、追问、或答什么）。
3. 同一个 session 的相邻轮次高度相关，切分训练/验证集时要**按 session 切**，
   否则等于把同一段上下文放进两边。
