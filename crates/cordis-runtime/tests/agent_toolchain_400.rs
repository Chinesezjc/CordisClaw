//! WS-1 集成回归：一轮里「一个 tool_call 失败 + 一个成功」之后，第二次请求的
//! `messages` 必须仍然满足 tool_calls 链不变量。
//!
//! 线上事故（10-20 23:39）：失败提示曾被就地 push 成 user 消息，正好落在同一个
//! assistant 的两条 tool 结果之间；DeepSeek 因此返回
//! `400 Messages with role 'tool' must be a response to a preceding message with
//! 'tool_calls'`，整轮被丢弃，用户没收到任何回复。
//!
//! 修复前本用例的链不变量断言会失败：`[tool_calls repair]` 扫到 user 就 break，
//! 只把 assistant 的 tool_calls 裁成第一个，第二条 tool 结果于是变成孤儿。
//!
//! 用最小空 artifacts 索引 + 一个转发型 provider 插件（JSON 工件 + Process 执行；
//! 脚本按 payload 里的 `transport.base_url` 打本地 mock SSE）。因此本文件不依赖
//! fixture dylib，boot 也快。

use cordis_runtime::host::{AgentSessionKind, RuntimeHost};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

mod support;

use support::{pin_private_snapshot_root, spawn_chunked_mock_llm_server_sequence, sse_response};

/// 转发型假 provider：读 payload 的 `transport.base_url`，打过去，把 SSE 增量
/// 拼成一条补全（含 tool_calls）。输出形状与真插件 `llm_openai` 一致，因此
/// kernel 侧 `parse_llm_reply` 走的是同一条解析路径。
fn write_proxy_llm_script(path: &Path) {
    let body = r#"#!/bin/sh
payload=$(cat)
python3 - "$payload" <<'PY'
import json, sys, urllib.request
req = json.loads(sys.argv[1])
url = req["transport"]["base_url"].rstrip("/") + "/chat/completions"
body = dict(req["body"]); body["stream"] = True
try:
    content, calls, rid, finish = "", {}, None, None
    handle = urllib.request.urlopen(
        urllib.request.Request(
            url,
            data=json.dumps(body).encode(),
            headers={"Content-Type": "application/json"},
        ),
        timeout=30,
    )
    with handle:
        for raw in handle:
            line = raw.decode("utf-8", "replace").strip()
            if not line.startswith("data:"):
                continue
            chunk = line[5:].strip()
            if chunk == "[DONE]":
                break
            ev = json.loads(chunk)
            rid = rid or ev.get("id")
            for ch in ev.get("choices", []):
                d = ch.get("delta", {})
                content += d.get("content") or ""
                finish = ch.get("finish_reason") or finish
                for tc in d.get("tool_calls", []):
                    slot = calls.setdefault(
                        tc.get("index", 0),
                        {"id": "", "type": "function",
                         "function": {"name": "", "arguments": ""}},
                    )
                    if tc.get("id"):
                        slot["id"] = tc["id"]
                    fn = tc.get("function") or {}
                    if fn.get("name"):
                        slot["function"]["name"] = fn["name"]
                    slot["function"]["arguments"] += fn.get("arguments") or ""
    print(json.dumps({"ok": True,
                      "message": {"content": content or None,
                                  "tool_calls": [calls[k] for k in sorted(calls)]},
                      "response_id": rid,
                      "finish_reason": finish}))
except Exception as e:
    print(json.dumps({"ok": False, "error": "upstream failed: %s" % e}))
PY
"#;
    fs::write(path, body).expect("write proxy script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).expect("chmod script");
    }
}

fn abi_fingerprint_value() -> Value {
    serde_json::to_value(cordis_plugin_sdk::AbiFingerprint::current_build(
        "crate_proxyllm_400",
        "api_v2",
    ))
    .expect("serialize fingerprint")
}

fn proxy_llm_docs() -> Value {
    json!({
        "plugin_id": "proxyllm",
        "plugin_path": "proxyllm",
        "plugin_version": "0.1.0",
        "abi_version": 2,
        "command_name": null,
        "nodes": [{
            "id": "llm_complete",
            "summary": "proxy completion to the per-test mock upstream",
            "input_schema": {"type": "object"},
            "output_schema": {"type": "object"},
            "side_effects": [],
            "failure_modes": [],
            "node_type": "task",
            "agent_accessible": false
        }],
        "system_hint": null
    })
}

fn sha256_of(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    let bytes = fs::read(path).expect("read artifact");
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    hex::encode(hasher.finalize())
}

/// 只带一个 provider 插件的最小 fixtures 树：空 plugins，boot 不碰任何 dylib。
fn setup_empty_fixture() -> TempDir {
    let temp = TempDir::new().expect("tempdir");
    let fixtures = temp.path().join("fixtures");
    let artifacts = fixtures.join("artifacts");
    fs::create_dir_all(&artifacts).expect("create artifacts dir");

    let script = temp.path().join("proxyllm.sh");
    write_proxy_llm_script(&script);
    let execution = json!({
        "kind": "process",
        "command": script.display().to_string(),
        "args": [],
    });
    let docs = proxy_llm_docs();
    let artifact = artifacts.join("proxyllm.json");
    fs::write(
        &artifact,
        serde_json::to_string_pretty(&json!({
            "plugin_path": "proxyllm",
            "abi_fingerprint": abi_fingerprint_value(),
            "docs": docs,
            "exports": [],
            "execution": execution,
        }))
        .expect("serialize artifact"),
    )
    .expect("write proxy artifact");
    let sha = sha256_of(&artifact);

    fs::write(
        artifacts.join("index.json"),
        serde_json::to_string_pretty(&json!({
            "schema_version": 2,
            "generated_at": "2026-08-06T00:00:00Z",
            "topo_order": ["proxyllm"],
            "entries": [{
                "plugin_path": "proxyllm",
                "version": "0.1.0",
                "abi_fingerprint": abi_fingerprint_value(),
                "artifact_path": "proxyllm.json",
                "sha256": sha,
                "built_at": "0",
                "parent": null,
                "required": false,
                "grants_from_parent": [],
                "docs": docs,
                "exports": [],
                "execution": execution,
                "artifact_kind": "json",
                "build_fingerprint": "bf-proxyllm",
                "input_probe": {"files": []},
                "local_path_deps": []
            }]
        }))
        .expect("serialize index"),
    )
    .expect("write artifact index");

    // fixtures root 目录名是 "fixtures"，`discover_config_dir` 落到同级
    // `temp/config`，与下面写 llm_api.yaml 的目录一致。
    pin_private_snapshot_root(temp.path(), &fixtures);
    temp
}

fn fixtures_dir(temp: &TempDir) -> PathBuf {
    temp.path().join("fixtures")
}

fn write_llm_config(fixtures_root: &Path, url: &str) {
    let config_dir = fixtures_root
        .parent()
        .expect("fixtures parent")
        .join("config");
    fs::create_dir_all(&config_dir).expect("create config dir");
    fs::write(
        config_dir.join("llm_api.yaml"),
        format!(
            "provider: deepseek\nbase_url: {url}\napi_key: test-key\nmodel: deepseek-reasoner\ntemperature: 0.0\nmax_tokens: 4096\ntimeout_ms: 10000\nstream_timeout_secs: 5\n"
        ),
    )
    .expect("write llm config");
}

/// 一轮里带 `tool_calls` 的 SSE 补全。
fn tool_call_response(response_id: &str, calls: Vec<(&str, &str, Value)>) -> Vec<(u64, String)> {
    sse_response(vec![
        json!({
            "id": response_id,
            "choices": [{
                "delta": {
                    "tool_calls": calls
                        .into_iter()
                        .enumerate()
                        .map(|(index, (call_id, name, arguments))| json!({
                            "index": index,
                            "id": call_id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": serde_json::to_string(&arguments)
                                    .expect("serialize tool arguments"),
                            }
                        }))
                        .collect::<Vec<_>>()
                }
            }]
        }),
        json!({
            "id": response_id,
            "choices": [{ "delta": {}, "finish_reason": "tool_calls" }]
        }),
    ])
}

fn assistant_response(response_id: &str, content: &str) -> Vec<(u64, String)> {
    sse_response(vec![
        json!({
            "id": response_id,
            "choices": [{ "delta": { "content": content } }]
        }),
        json!({
            "id": response_id,
            "choices": [{ "delta": {}, "finish_reason": "stop" }]
        }),
    ])
}

fn role(message: &Value) -> Option<&str> {
    message.get("role").and_then(Value::as_str)
}

fn content_of(message: &Value) -> Option<&str> {
    message.get("content").and_then(Value::as_str)
}

/// mock 抓到的原始请求（首行 + 头 + 体）→ 请求体的 JSON。
fn request_body(request: &str) -> Value {
    let (_, body) = request
        .split_once("\r\n\r\n")
        .expect("captured request must have a header/body separator");
    serde_json::from_str(body).expect("request body must be JSON")
}

/// 从 `index` 往前跳过连续的 tool 消息，找最近的一条非 tool 消息；它必须是一条
/// 声明了 `id` 的 assistant，`id` 才算「紧邻应答」。
fn declaring_assistant(messages: &[Value], index: usize, id: &str) -> Option<usize> {
    for back in (0..index).rev() {
        let message = &messages[back];
        if role(message) == Some("tool") {
            continue;
        }
        let declares = message
            .get("tool_calls")
            .and_then(Value::as_array)
            .is_some_and(|calls| {
                calls
                    .iter()
                    .any(|call| call.get("id").and_then(Value::as_str) == Some(id))
            });
        return (role(message) == Some("assistant") && declares).then_some(back);
    }
    None
}

/// 不变量：每条 `role=="tool"` 都能在其之前找到一条 assistant，其 tool_calls
/// 含该 tool_call_id，且两者之间没有非 tool 消息。
fn assert_tool_chain_invariant(messages: &[Value], expected_tools: usize) {
    let mut tool_messages = 0usize;
    for (index, message) in messages.iter().enumerate() {
        if role(message) != Some("tool") {
            continue;
        }
        tool_messages += 1;
        let id = message
            .get("tool_call_id")
            .and_then(Value::as_str)
            .expect("every tool message carries a tool_call_id");
        let cond = declaring_assistant(messages, index, id).is_some();
        assert!(
            cond,
            "tool 消息 #{index} (id={id}) 的行为非法; messages={messages:?}"
        );
    }
    assert_eq!(tool_messages, expected_tools, "tool 结果数量");
}

/// 线上现场复现：第 1 轮 2 个 tool_calls（`invoke_plugin` 指向不存在的插件 →
/// 必定失败，`list_nodes` → 成功），第 2 轮给最终答复。
///
/// 断言第 2 次请求的 messages：链不变量成立、两个 tool 结果都在、失败提示被合并
/// 成恰好一条 user 消息且排在全部 tool 结果之后（措辞逐字节不变）。
/// 修复前这里必然失败（tool(B) 成为孤儿 / 或 assistant 的 tool_calls 被裁成 1 条）。
#[test]
fn failed_tool_call_does_not_break_the_tool_call_chain() {
    let temp = setup_empty_fixture();
    let fixtures = fixtures_dir(&temp);
    let final_json = r#"{"ok":true,"summary":"done"}"#;
    let (url, requests_rx, handle) = spawn_chunked_mock_llm_server_sequence(vec![
        tool_call_response(
            "turn1",
            vec![
                (
                    "call_invoke",
                    "invoke_plugin",
                    json!({
                        "plugin_path": "/no-such-plugin",
                        "node_id": "no_such_node",
                        "payload_json": {},
                    }),
                ),
                ("call_list_nodes", "list_nodes", json!({})),
            ],
        ),
        assistant_response("turn2", final_json),
    ]);
    write_llm_config(&fixtures, &url);

    let host = RuntimeHost::boot(&fixtures).expect("host should boot on empty index");
    let sid = host
        .agent_start(AgentSessionKind::RuntimeShell)
        .expect("agent start")
        .session_id;
    // 不 panic、不返回 Err：这就是"整轮没被 400 丢掉"的最小证据。
    let reply = host
        .agent_send(&sid, "bot，写个插件")
        .expect("a failed tool call must not fail the whole turn");
    assert_eq!(reply.content, final_json);

    let requests = requests_rx.recv().expect("captured requests");
    handle.join().expect("join mock server");
    assert_eq!(requests.len(), 2, "一轮 tool_calls + 一轮最终答复");

    let second = request_body(&requests[1]);
    let messages = second
        .get("messages")
        .and_then(Value::as_array)
        .expect("request body carries messages");
    assert_tool_chain_invariant(messages, 2);

    // 两条 tool 结果都在，且顺序与 assistant 的 tool_calls 一致。
    let tool_ids: Vec<&str> = messages
        .iter()
        .filter(|message| role(message) == Some("tool"))
        .filter_map(|message| message.get("tool_call_id").and_then(Value::as_str))
        .collect();
    assert_eq!(tool_ids, vec!["call_invoke", "call_list_nodes"]);

    // 失败提示恰好一条 user 消息，且必须排在全部 tool 结果之后。
    let last_tool = messages
        .iter()
        .rposition(|message| role(message) == Some("tool"))
        .expect("two tool results were asserted above");
    let prompt_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| content_of(message).is_some_and(|text| text.starts_with("🔴 TOOL ")))
        .map(|(index, _)| index)
        .collect();
    assert_eq!(
        prompt_indices.len(),
        1,
        "失败提示必须合并成恰好一条 user 消息"
    );
    assert!(
        prompt_indices[0] > last_tool,
        "失败提示不能插在 tool 结果之间"
    );
    assert_eq!(role(&messages[prompt_indices[0]]), Some("user"));

    let prompt = content_of(&messages[prompt_indices[0]]).expect("prompt content");
    let expected_suffix = "\nDo NOT proceed as if it succeeded. Fix the cause and retry, or use a different approach.";
    let wording_ok =
        prompt.starts_with("🔴 TOOL 'invoke_plugin' FAILED: ") && prompt.ends_with(expected_suffix);
    assert!(wording_ok, "失败提示措辞必须逐字节保持: {prompt:?}");
}

/// 混合一轮：一个不存在的 tool + 一个真实 tool。
///
/// 「unknown tool」的错误也曾被就地 push 成 user 消息，正好落在
/// `assistant(tool_calls)` 与它的 tool 结果之间——与失败提示完全同一个破形
/// 方向（旧 repair 会把 assistant 的 tool_calls 全部裁掉，留下孤儿 tool 结果
/// → 400）。修复后两类提示都进同一条 user 消息，并排在最后一个 tool 结果之后。
#[test]
fn unknown_tool_error_does_not_break_the_tool_call_chain() {
    let temp = setup_empty_fixture();
    let fixtures = fixtures_dir(&temp);
    let final_json = r#"{"ok":true,"summary":"done"}"#;
    let (url, requests_rx, handle) = spawn_chunked_mock_llm_server_sequence(vec![
        tool_call_response(
            "turn1",
            vec![
                ("call_bogus", "definitely_not_a_tool", json!({})),
                ("call_list_nodes", "list_nodes", json!({})),
            ],
        ),
        assistant_response("turn2", final_json),
    ]);
    write_llm_config(&fixtures, &url);

    let host = RuntimeHost::boot(&fixtures).expect("host should boot on empty index");
    let sid = host
        .agent_start(AgentSessionKind::RuntimeShell)
        .expect("agent start")
        .session_id;
    let reply = host
        .agent_send(&sid, "bot，试试一个不存在的工具")
        .expect("an unknown tool call must not fail the whole turn");
    assert_eq!(reply.content, final_json);

    let requests = requests_rx.recv().expect("captured requests");
    handle.join().expect("join mock server");
    assert_eq!(requests.len(), 2, "一轮 tool_calls + 一轮最终答复");

    let second = request_body(&requests[1]);
    let messages = second
        .get("messages")
        .and_then(Value::as_array)
        .expect("request body carries messages");
    // 只有真实 tool 的结果留在链里，且紧跟在声明它的 assistant 之后。
    assert_tool_chain_invariant(messages, 1);

    let last_tool = messages
        .iter()
        .rposition(|message| role(message) == Some("tool"))
        .expect("the valid tool result must survive");
    let needle = "STOP — tool 'definitely_not_a_tool' does not exist. Strike 1/1.";
    let prompt_index = messages
        .iter()
        .position(|message| content_of(message).is_some_and(|text| text.contains(needle)));
    let prompt_index = prompt_index.expect("unknown-tool error must reach the model");
    assert!(
        prompt_index > last_tool,
        "unknown-tool 错误不能插在 tool 结果之间 (prompt={prompt_index} last_tool={last_tool})"
    );
    assert_eq!(role(&messages[prompt_index]), Some("user"));

    // 再钉一条独立不变量：请求里不能有「声明了 tool_calls 却没有结果」的 assistant。
    let dangling: Vec<&str> = messages
        .iter()
        .filter(|message| role(message) == Some("assistant"))
        .filter_map(|message| message.get("tool_calls").and_then(Value::as_array))
        .flatten()
        .filter_map(|call| call.get("id").and_then(Value::as_str))
        .filter(|id| {
            !messages
                .iter()
                .any(|m| m.get("tool_call_id").and_then(Value::as_str) == Some(*id))
        })
        .collect();
    assert!(dangling.is_empty(), "dangling tool_calls: {dangling:?}");
}

// ───────────────────────────────────────────────────────────────────────
// 多模态：入站图片 → `image_url` content part。
// 证明 kernel 真的把图片（自己下载 + base64 内联）塞进了请求体，
// 而不是只留一个 `[image: url]` 文本占位符。
// ───────────────────────────────────────────────────────────────────────

/// 1×1 红色 PNG（够 `sniff_mime` 认出 image/png，也够供应商解码）。
const RED_PNG: &[u8] = &[
    0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53,
    0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08, 0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00,
    0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D, 0xB0, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E,
    0x44, 0xAE, 0x42, 0x60, 0x82,
];

/// 起一个只服务一次请求的最小 HTTP 图片服务器，返回图片 URL。
fn serve_png_once() -> String {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind png server");
    let addr = listener.local_addr().expect("addr");
    std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                RED_PNG.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(RED_PNG);
            let _ = stream.flush();
        }
    });
    format!("http://{addr}/red.png")
}

fn user_message_with_array_content(messages: &[Value]) -> &Value {
    messages
        .iter()
        .find(|message| {
            role(message) == Some("user") && message.get("content").is_some_and(Value::is_array)
        })
        .expect("the user message must carry multimodal content")
}

#[test]
fn inbound_image_is_inlined_as_a_multimodal_content_part() {
    let temp = setup_empty_fixture();
    let fixtures = fixtures_dir(&temp);
    let final_json = r#"{"ok":true}"#;
    let (url, requests_rx, handle) =
        spawn_chunked_mock_llm_server_sequence(vec![assistant_response("turn1", final_json)]);
    write_llm_config(&fixtures, &url);
    let png_url = serve_png_once();

    let host = RuntimeHost::boot(&fixtures).expect("host should boot on empty index");
    let sid = host
        .agent_start(AgentSessionKind::RuntimeShell)
        .expect("agent start")
        .session_id;
    let reply = host
        .agent_send_with_images(&sid, "看这张图", &[png_url])
        .expect("a batch with an image must still complete");
    assert_eq!(reply.content, final_json);

    let requests = requests_rx.recv().expect("captured requests");
    handle.join().expect("join mock server");
    let body = request_body(&requests[0]);
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .expect("request body carries messages");
    let content = user_message_with_array_content(messages)
        .get("content")
        .and_then(Value::as_array)
        .expect("array content");
    assert_eq!(content.len(), 2, "文本 + 一张图: {content:?}");
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[0]["text"], "看这张图");
    assert_eq!(content[1]["type"], "image_url");
    let uri = content[1]["image_url"]["url"].as_str().expect("data uri");
    let is_png_data_uri = uri.starts_with("data:image/png;base64,");
    assert!(is_png_data_uri, "uri={uri}");
}

#[test]
fn no_images_keeps_the_plain_string_content() {
    let temp = setup_empty_fixture();
    let fixtures = fixtures_dir(&temp);
    let final_json = r#"{"ok":true}"#;
    let (url, requests_rx, handle) =
        spawn_chunked_mock_llm_server_sequence(vec![assistant_response("turn1", final_json)]);
    write_llm_config(&fixtures, &url);

    let host = RuntimeHost::boot(&fixtures).expect("host should boot on empty index");
    let sid = host
        .agent_start(AgentSessionKind::RuntimeShell)
        .expect("agent start")
        .session_id;
    host.agent_send_with_images(&sid, "纯文字", &[])
        .expect("plain send");

    let requests = requests_rx.recv().expect("captured requests");
    handle.join().expect("join mock server");
    let body = request_body(&requests[0]);
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .expect("messages");
    let last_user = messages
        .iter()
        .rev()
        .find(|message| role(message) == Some("user"))
        .expect("a user message");
    assert_eq!(content_of(last_user), Some("纯文字"));
}

// ───────────────────────────────────────────────────────────────────────
// 一次性上下文（notes）：只进本次请求，绝不进 history。
//
// 渠道插件（QQ）每条消息都会下发 `[reply policy: …]`（约 600 字符）和
// `[group memory: …]`；如果这些也写进 history，254 条历史就是十几万字节的
// 纯噪声，会把真正的对话挤淡。这里同时断言两件事：
//   1) 第一次请求的 user content 里有 notes；
//   2) 落盘的 session 快照 history 里没有 notes。
// ───────────────────────────────────────────────────────────────────────
fn session_history_from_disk(fixtures: &Path) -> Vec<Value> {
    let sessions = fixtures
        .parent()
        .expect("fixtures parent")
        .join("data")
        .join("sessions");
    let mut newest: Option<(std::time::SystemTime, Value)> = None;
    for entry in std::fs::read_dir(&sessions).expect("sessions dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let modified = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        let parsed: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_default())
                .unwrap_or(Value::Null);
        if newest.as_ref().is_none_or(|(t, _)| modified >= *t) {
            newest = Some((modified, parsed));
        }
    }
    let (_, snapshot) = newest.expect("a session snapshot must be written");
    snapshot
        .get("history")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

#[test]
fn one_shot_notes_reach_the_request_but_not_the_history() {
    let temp = setup_empty_fixture();
    let fixtures = fixtures_dir(&temp);
    let final_json = r#"{"ok":true}"#;
    let (url, requests_rx, handle) =
        spawn_chunked_mock_llm_server_sequence(vec![assistant_response("turn1", final_json)]);
    write_llm_config(&fixtures, &url);

    let host = RuntimeHost::boot(&fixtures).expect("host should boot on empty index");
    let sid = host
        .agent_start(AgentSessionKind::RuntimeShell)
        .expect("agent start")
        .session_id;
    let notes = "[reply policy: quiet — only when named]\n[group memory: 这个群偏安静]";
    host.agent_send_with_turn_context(&sid, "纯文字", &[], notes)
        .expect("send with notes");

    let requests = requests_rx.recv().expect("captured requests");
    handle.join().expect("join mock server");
    let body = request_body(&requests[0]);
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .expect("messages");
    let last_user = messages
        .iter()
        .rev()
        .find(|message| role(message) == Some("user"))
        .expect("a user message");
    let content = content_of(last_user).expect("string content");
    let has_notes = content.contains("[reply policy: quiet");
    assert!(has_notes, "notes must reach the model: {content}");
    let has_memory = content.contains("[group memory: 这个群偏安静]");
    assert!(has_memory, "memory line must reach the model: {content}");
    let has_input = content.ends_with("纯文字");
    assert!(has_input, "the user text must be there: {content}");

    // 落盘快照里：history 不能带 notes。
    let history = session_history_from_disk(&fixtures);
    assert!(!history.is_empty(), "history must be persisted");
    let polluted: Vec<&Value> = history
        .iter()
        .filter(|message| {
            content_of(message).is_some_and(|text| text.contains("[reply policy: quiet"))
        })
        .collect();
    assert!(
        polluted.is_empty(),
        "history must not carry one-shot notes: {polluted:?}"
    );
}

// ───────────────────────────────────────────────────────────────────────
// 可信元数据：`__meta` 必须由 kernel 注入到**每次** invoke 的 payload。
//
// 这条链路有一个很容易漏的接缝：`AgentToolHost::inject_turn_meta` 在 trait 上
// 有 no-op 默认实现，`RuntimeHost` 的 trait impl 必须转发到真实方法——漏了转发
// 的话，插件收不到 __meta，鉴权会静默退化成"当作 CLI 操作员放行"（线上踩过）。
// 这里用一个回显型 process 插件把 payload 原样回给模型，再从第二个请求里读回来。
// ───────────────────────────────────────────────────────────────────────

/// 回显插件：把收到的 payload 原样打印成工具结果。
fn write_echo_plugin(fixtures: &Path) -> Value {
    let artifacts = fixtures.join("artifacts");
    let script = fixtures.join("echo_probe.sh");
    fs::write(
        &script,
        "#!/bin/sh\npayload=$(cat)\nprintf '{\"ok\":true,\"node_id\":\"echo_probe\",\"data\":%s}\\n' \"$payload\"\n",
    )
    .expect("write echo script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    let execution = json!({
        "kind": "process",
        "command": script.display().to_string(),
        "args": [],
    });
    let docs = json!({
        "plugin_id": "echoprobe",
        "plugin_path": "echoprobe",
        "plugin_version": "0.1.0",
        "abi_version": 2,
        "command_name": null,
        "nodes": [{
            "id": "echo_probe",
            "summary": "echo the payload back",
            "input_schema": {"type": "object"},
            "output_schema": {"type": "object"},
            "side_effects": [],
            "failure_modes": [],
            "node_type": "router",
            "agent_accessible": true
        }],
        "system_hint": null
    });
    let artifact = artifacts.join("echoprobe.json");
    fs::write(
        &artifact,
        serde_json::to_string_pretty(&json!({
            "plugin_path": "echoprobe",
            "abi_fingerprint": abi_fingerprint_value(),
            "docs": docs,
            "exports": [],
            "execution": execution,
        }))
        .expect("serialize"),
    )
    .expect("write echo artifact");
    let sha = sha256_of(&artifact);
    let entry = json!({
        "plugin_path": "echoprobe",
        "version": "0.1.0",
        "abi_fingerprint": abi_fingerprint_value(),
        "artifact_path": "echoprobe.json",
        "sha256": sha,
        "built_at": "0",
        "parent": null,
        "required": false,
        "grants_from_parent": [],
        "docs": docs,
        "exports": [],
        "execution": execution,
        "artifact_kind": "json",
        "build_fingerprint": "bf-echoprobe",
        "input_probe": {"files": []},
        "local_path_deps": []
    });
    entry
}

#[test]
fn trusted_turn_meta_reaches_the_plugin_payload() {
    let temp = setup_empty_fixture();
    let fixtures = fixtures_dir(&temp);
    let echo_entry = write_echo_plugin(&fixtures);

    // 把 echoprobe 追加进 artifacts/index.json。
    let index_path = fixtures.join("artifacts").join("index.json");
    let mut index: Value =
        serde_json::from_str(&fs::read_to_string(&index_path).expect("index")).expect("json");
    if let Some(entries) = index.get_mut("entries").and_then(Value::as_array_mut) {
        entries.push(echo_entry);
    }
    if let Some(order) = index.get_mut("topo_order").and_then(Value::as_array_mut) {
        order.push(json!("echoprobe"));
    }
    fs::write(
        &index_path,
        serde_json::to_string_pretty(&index).expect("ser"),
    )
    .expect("write index");

    let final_json = r#"{"ok":true}"#;
    let (url, requests_rx, handle) = spawn_chunked_mock_llm_server_sequence(vec![
        tool_call_response(
            "turn1",
            vec![(
                "call_echo",
                "invoke_plugin",
                json!({
                    "plugin_path": "echoprobe",
                    "node_id": "echo_probe",
                    "payload_json": {"probe": "hello"}
                }),
            )],
        ),
        assistant_response("turn2", final_json),
    ]);
    write_llm_config(&fixtures, &url);
    let host = RuntimeHost::boot(&fixtures).expect("boot");
    let sid = host
        .agent_start(AgentSessionKind::RuntimeShell)
        .expect("agent start")
        .session_id;

    let mut meta = std::collections::BTreeMap::new();
    meta.insert("actor".to_string(), "qq:1751557948".to_string());
    meta.insert("target".to_string(), "group:999999".to_string());
    host.agent_send_with_turn_meta(&sid, "probe", &[], "", &meta)
        .expect("send");

    let requests = requests_rx.recv().expect("requests");
    handle.join().expect("join mock");

    let second = request_body(&requests[1]);
    let messages = second
        .get("messages")
        .and_then(Value::as_array)
        .expect("messages");
    let tool_result = messages
        .iter()
        .filter(|message| role(message) == Some("tool"))
        .filter_map(|message| content_of(message))
        .find(|content| content.contains("echo_probe"))
        .expect("the echo tool result must be in the second request");
    let has_meta = tool_result.contains("__meta");
    assert!(has_meta, "plugin must receive __meta: {tool_result}");
    let actor_ok = tool_result.contains("qq:1751557948");
    assert!(
        actor_ok,
        "actor must survive into the payload: {tool_result}"
    );
    // 注入后清理干净。
    let leaked = host.debug_turn_meta_len();
    assert_eq!(leaked, 0, "turn meta must be cleared after the send");
}
