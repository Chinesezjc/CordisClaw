//! Coverage-focused integration tests for the LLM network paths in
//! `host.rs`: `agent_send`, `agent_send_with_fallback` (success, retry,
//! profile-fallback state machine, optimistic recovery probe),
//! `swap_session_profile`, and `set_degraded`.
//!
//! Unlike `runtime_host.rs`, these tests do NOT gate behind
//! `linux_dylib_artifacts_available()`. They boot a *minimal, plugin-free*
//! fixtures tree (an empty `artifacts/index.json`) so `RuntimeHost::boot`
//! succeeds on any host target (arm64 macOS included) without needing the
//! pre-built x86_64-linux fixture dylibs. The LLM endpoint is pointed at the
//! chunked mock SSE server shared with `runtime_host.rs` via `mod support`.
//!
//! Boot on an empty index is near-instant, so these tests avoid the ~120s
//! `ensure_fixture_artifacts` dylib rebuild that the full-fixture tests pay.

use cordis_runtime::core::error::RuntimeError;
use cordis_runtime::host::{AgentSessionKind, AgentStartOptions, RuntimeHost};
use cordis_runtime::kernel::plugin_iteration::KernelPluginIssueSource;
use serde_json::json;
use serial_test::serial;
use std::fs;
use std::net::TcpListener;
use std::path::Path;
use tempfile::TempDir;

mod support;

use support::{pin_private_snapshot_root, spawn_chunked_mock_llm_server_sequence, sse_response};

/// Build a minimal fixtures tree that boots with zero plugins. `boot` reads
/// `artifacts/index.json` (schema_version 2, empty entries), so the loader
/// registers no plugins and never touches a dylib — cross-platform safe.
/// 转发型假 provider 的执行脚本：读 payload 里的 `transport.base_url`，用
/// urllib 打过去，把 SSE 增量拼成一条补全。保留真实 HTTP 语义（503 会经
/// HTTPError 变成失败回复），这样 kernel 侧的重试/降级逻辑仍被真正驱动。
fn write_proxy_llm_script(path: &Path) {
    let body = r#"#!/bin/sh
payload=$(cat)
python3 - "$payload" <<'PY'
import json, sys, urllib.request, urllib.error
req = json.loads(sys.argv[1])
url = req["transport"]["base_url"].rstrip("/") + "/chat/completions"
body = dict(req["body"]); body["stream"] = True
# profile 内的 5xx 重试属于**传输层**，拆分后归 provider 插件管
# （真插件 llm_openai 里是 AGENT_REQUEST_MAX_ATTEMPTS 那段）。这里照做，
# 否则 kernel 侧的"重试后恢复"用例会因为假 provider 一撞 503 就放弃而失败。
def fetch():
    r = urllib.request.Request(url, data=json.dumps(body).encode(),
                               headers={"Content-Type": "application/json"})
    return urllib.request.urlopen(r, timeout=30)

try:
    content, reasoning, calls, rid, finish = "", "", {}, None, None
    last = None
    resp = None
    for _ in range(3):
        try:
            resp = fetch()
            break
        except urllib.error.HTTPError as e:
            last = e
            if e.code < 500:      # 4xx 是永久错误，不重试
                raise
        except Exception as e:
            last = e
    if resp is None:
        raise last
    with resp:
        for raw in resp:
            line = raw.decode("utf-8", "replace").strip()
            if not line.startswith("data:"):
                continue
            chunk = line[5:].strip()
            if chunk == "[DONE]":
                break
            try:
                ev = json.loads(chunk)
            except Exception:
                continue
            rid = rid or ev.get("id")
            for ch in ev.get("choices", []):
                d = ch.get("delta", {})
                content += d.get("content") or ""
                reasoning += d.get("reasoning_content") or ""
                finish = ch.get("finish_reason") or finish
                for tc in d.get("tool_calls", []):
                    slot = calls.setdefault(tc.get("index", 0),
                                            {"id": "", "type": "function",
                                             "function": {"name": "", "arguments": ""}})
                    if tc.get("id"):
                        slot["id"] = tc["id"]
                    fn = tc.get("function") or {}
                    if fn.get("name"):
                        slot["function"]["name"] = fn["name"]
                    slot["function"]["arguments"] += fn.get("arguments") or ""
    msg = {"content": content or None, "reasoning_content": reasoning or None,
           "tool_calls": [calls[k] for k in sorted(calls)]}
    print(json.dumps({"ok": True, "message": msg,
                      "response_id": rid, "finish_reason": finish}))
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

fn abi_fingerprint_value() -> serde_json::Value {
    serde_json::to_value(cordis_plugin_sdk::AbiFingerprint::current_build(
        "crate_proxyllm_v1",
        "api_v2",
    ))
    .expect("serialize fingerprint")
}

fn proxy_llm_docs() -> serde_json::Value {
    serde_json::json!({
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

fn setup_empty_fixture() -> TempDir {
    let temp = TempDir::new().expect("tempdir");
    let fixtures = temp.path().join("fixtures");
    let artifacts = fixtures.join("artifacts");
    fs::create_dir_all(&artifacts).expect("create artifacts dir");

    // 拆分后 kernel 不自带 LLM 传输，`agent_send` 需要有插件声明 `llm_complete`。
    // 本文件的用例考的是 kernel 侧的重试/降级/auto-save，仍需要真实 HTTP 语义
    // （503 重试、端点不可达），所以给一个把请求转发到各用例自己那个 mock server
    // 的最小 provider：JSON 工件 + Process 执行，脚本读 payload 里的 base_url。
    let script = temp.path().join("proxyllm.sh");
    write_proxy_llm_script(&script);
    let artifact = artifacts.join("proxyllm.json");
    let docs = proxy_llm_docs();
    fs::write(
        &artifact,
        serde_json::to_string_pretty(&serde_json::json!({
            "plugin_path": "proxyllm",
            "abi_fingerprint": abi_fingerprint_value(),
            "docs": docs,
            "exports": [],
            "execution": {"kind": "process", "command": script.display().to_string(), "args": []},
        }))
        .expect("serialize artifact"),
    )
    .expect("write proxy artifact");
    let sha = sha256_of(&artifact);

    fs::write(
        artifacts.join("index.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "schema_version": 2,
            "generated_at": "2026-07-23T00:00:00Z",
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
                "execution": {"kind": "process", "command": script.display().to_string(), "args": []},
                "artifact_kind": "json",
                "build_fingerprint": "bf-proxyllm",
                "input_probe": {"files": []},
                "local_path_deps": []
            }]
        }))
        .expect("serialize index"),
    )
    .expect("write artifact index");
    // fixtures root 目录名是 "fixtures"，`discover_config_dir` 走同级分支 →
    // `temp/config`，与本文件写 llm_api.yaml 的目录一致。
    pin_private_snapshot_root(temp.path(), &fixtures);
    temp
}

fn fixtures_dir(temp: &TempDir) -> std::path::PathBuf {
    temp.path().join("fixtures")
}

/// One-turn assistant SSE reply (content only, finish_reason=stop).
fn assistant_reply(response_id: &str, content: &str) -> Vec<(u64, String)> {
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

/// Two-profile config: `default` (with a `fallback: fast` pointer) and `fast`.
fn write_profiles_config(fixtures_root: &Path, default_url: &str, fast_url: &str) {
    // config/ is discovered as a sibling of a directory literally named
    // "fixtures" (see discover_config_dir).
    let config_dir = fixtures_root
        .parent()
        .expect("fixtures parent")
        .join("config");
    fs::create_dir_all(&config_dir).expect("create config dir");
    fs::write(
        config_dir.join("llm_api.yaml"),
        format!(
            "profiles:\n  default:\n    provider: deepseek\n    base_url: {default_url}\n    api_key: test-key\n    model: deepseek-reasoner\n    timeout_ms: 10000\n    fallback: fast\n  fast:\n    provider: deepseek\n    base_url: {fast_url}\n    api_key: test-key\n    model: deepseek-chat\n    timeout_ms: 10000\n"
        ),
    )
    .expect("write llm profiles config");
}

/// Single-profile config (no fallback pointer): exercises agent_send and the
/// no-fallback-entry branch of agent_send_with_fallback.
fn write_single_profile_config(fixtures_root: &Path, url: &str) {
    let config_dir = fixtures_root
        .parent()
        .expect("fixtures parent")
        .join("config");
    fs::create_dir_all(&config_dir).expect("create config dir");
    fs::write(
        config_dir.join("llm_api.yaml"),
        format!(
            "provider: deepseek\nbase_url: {url}\napi_key: test-key\nmodel: deepseek-reasoner\ntemperature: 0.0\nmax_tokens: 256\ntimeout_ms: 10000\n"
        ),
    )
    .expect("write single-profile llm config");
}

/// A long-lived scripted HTTP server bound to an ephemeral port. Each incoming
/// request pops the next `ServerStep`: `Fail503` returns a retryable HTTP 503
/// (exhausting `send_chat_request`'s in-profile retries), `Reply(content)`
/// returns a 200 SSE assistant turn. The server owns its listener for its whole
/// life, so — unlike a reserve-then-drop port dance — no port-reuse race can
/// make a live profile look dead. It exits after `steps.len()` requests.
///
/// Returns the base URL and a join handle yielding the number of requests
/// actually served (asserts the script was fully consumed).
#[derive(Clone)]
enum ServerStep {
    Fail503,
    Reply(String),
}

fn spawn_scripted_server(steps: Vec<ServerStep>) -> (String, std::thread::JoinHandle<usize>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind scripted server");
    let addr = listener.local_addr().expect("addr");
    let handle = std::thread::spawn(move || {
        use std::io::{BufRead, BufReader, Read as _, Write as _};
        let mut served = 0usize;
        for step in steps {
            let (mut stream, _) = listener.accept().expect("accept scripted request");
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            let mut line = String::new();
            reader.read_line(&mut line).expect("request line");
            let mut content_length = 0usize;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).expect("header line");
                if header == "\r\n" {
                    break;
                }
                if let Some(v) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = v.trim().parse().expect("content length");
                }
            }
            let mut body = vec![0u8; content_length];
            reader.read_exact(&mut body).expect("request body");
            served += 1;
            match step {
                ServerStep::Fail503 => {
                    // 503 is retryable (not a permanent 4xx), so the in-profile
                    // retry loop keeps trying up to AGENT_REQUEST_MAX_ATTEMPTS.
                    write!(
                        stream,
                        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndown"
                    )
                    .expect("write 503");
                    stream.flush().expect("flush 503");
                }
                ServerStep::Reply(content) => {
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
                    )
                    .expect("headers");
                    for (_, chunk) in assistant_reply("scripted", &content) {
                        write!(stream, "{:X}\r\n{}\r\n", chunk.len(), chunk).expect("chunk");
                    }
                    write!(stream, "0\r\n\r\n").expect("chunked end");
                    stream.flush().expect("flush");
                }
            }
        }
        served
    });
    (format!("http://{addr}/v1"), handle)
}

// ── agent_send: happy path ──────────────────────────────────────────────
// A RuntimeShell session sends one turn; the mock returns a plain assistant
// reply. Exercises agent_send success + auto_save (RuntimeShell) path.
#[test]
#[serial]
fn agent_send_returns_assistant_reply_and_auto_saves() {
    let temp = setup_empty_fixture();
    let fixtures = fixtures_dir(&temp);

    let (url, requests_rx, handle) =
        spawn_chunked_mock_llm_server_sequence(vec![assistant_reply("send_ok", "hello from mock")]);
    write_single_profile_config(&fixtures, &url);

    let host = RuntimeHost::boot(&fixtures).expect("host should boot on empty index");
    let sid = host
        .agent_start(AgentSessionKind::RuntimeShell)
        .expect("agent start")
        .session_id;

    let reply = host
        .agent_send(&sid, "hi")
        .expect("agent_send should succeed");
    assert_eq!(reply.content, "hello from mock");

    let requests = requests_rx.recv().expect("captured requests");
    handle.join().expect("join mock server");
    assert_eq!(requests.len(), 1, "one turn == one upstream request");
    assert!(requests[0].contains("deepseek-reasoner"), "model in body");

    // auto_save fires on a successful RuntimeShell send: the snapshot lands
    // under <workspace>/data/sessions/<sid>.json (data_dir = fixtures.parent).
    let snapshot = temp
        .path()
        .join("data/sessions")
        .join(format!("{sid}.json"));
    assert!(
        snapshot.exists(),
        "auto_save should persist session snapshot at {}",
        snapshot.display()
    );
}

// ── agent_send: session-not-found ────────────────────────────────────────
#[test]
#[serial]
fn agent_send_unknown_session_errors() {
    let temp = setup_empty_fixture();
    let fixtures = fixtures_dir(&temp);
    write_single_profile_config(&fixtures, "http://127.0.0.1:1/v1");
    let host = RuntimeHost::boot(&fixtures).expect("host should boot");

    let err = host
        .agent_send("no-such-session", "hi")
        .expect_err("unknown session must error");
    assert!(
        matches!(err, RuntimeError::AgentSessionNotFound { .. }),
        "expected AgentSessionNotFound, got {err:?}"
    );
}

// ── agent_send_with_fallback: no fallback entry → plain send ─────────────
// A session started via a registry whose profile has no fallback pointer
// still routes through agent_send_with_fallback; the success branch with a
// non-degraded entry must NOT emit a recovery notify or issue.
#[test]
#[serial]
fn fallback_wrapper_without_fallback_pointer_is_plain_send() {
    let temp = setup_empty_fixture();
    let fixtures = fixtures_dir(&temp);

    let (url, requests_rx, handle) = spawn_chunked_mock_llm_server_sequence(vec![assistant_reply(
        "plain_ok",
        "served directly",
    )]);
    write_single_profile_config(&fixtures, &url);

    let host = RuntimeHost::boot(&fixtures).expect("host should boot");
    let sid = host
        .agent_start_with(
            AgentSessionKind::RuntimeShell,
            AgentStartOptions {
                profile: Some("default".to_string()),
                ..Default::default()
            },
        )
        .expect("agent start")
        .session_id;

    let reply = host
        .agent_send_with_fallback(&sid, "hi")
        .expect("plain send should succeed");
    assert_eq!(reply.content, "served directly");

    let captured = requests_rx.recv().expect("captured requests");
    assert_eq!(
        captured.len(),
        1,
        "the single scripted response must have been dialled"
    );
    handle.join().expect("join mock server");

    // No degradation happened → no /llm-profile issue recorded.
    assert!(
        !host
            .kernel()
            .plugin_issues()
            .iter()
            .any(|i| i.root_plugin_path == "/llm-profile"),
        "healthy single-profile send must not record a profile issue"
    );
}

// ── agent_send_with_fallback: full degrade → recover state machine ──────
// 1) default dead → swap to fast, set_degraded(true), record kernel issue.
// 2) default revived → optimistic probe (swap back) succeeds, set_degraded(false).
#[test]
#[serial]
fn fallback_degrades_to_fast_then_recovers_to_default() {
    let temp = setup_empty_fixture();
    let fixtures = fixtures_dir(&temp);

    // default: three 503s exhaust the in-profile retries on the first send
    // (→ primary error → fallback engages), then a 200 for the recovery probe.
    let (default_url, default_handle) = spawn_scripted_server(vec![
        ServerStep::Fail503,
        ServerStep::Fail503,
        ServerStep::Fail503,
        ServerStep::Reply("served by default".to_string()),
    ]);
    // fast: serves the single fallback turn.
    let (fast_url, fast_handle) =
        spawn_scripted_server(vec![ServerStep::Reply("served by fast".to_string())]);
    write_profiles_config(&fixtures, &default_url, &fast_url);

    let host = RuntimeHost::boot(&fixtures).expect("host should boot");
    let sid = host
        .agent_start_with(
            AgentSessionKind::RuntimeShell,
            AgentStartOptions {
                profile: Some("default".to_string()),
                ..Default::default()
            },
        )
        .expect("agent start")
        .session_id;

    // 1) default failing → degrade to fast; reply still succeeds.
    let reply = host
        .agent_send_with_fallback(&sid, "hello")
        .expect("fallback should rescue the request");
    assert_eq!(reply.content, "served by fast");

    let issues = host.kernel().plugin_issues();
    assert!(
        issues.iter().any(|i| i.root_plugin_path == "/llm-profile"
            && i.source == KernelPluginIssueSource::InvokeFailure
            && i.summary.contains("degraded to fallback 'fast'")),
        "degradation must record a /llm-profile InvokeFailure issue: {issues:?}"
    );

    // 2) default comes back → optimistic probe swaps back on next send.
    let reply = host
        .agent_send_with_fallback(&sid, "are you back?")
        .expect("recovered profile should serve");
    assert_eq!(reply.content, "served by default");

    assert_eq!(
        default_handle.join().expect("join default server"),
        4,
        "default: 3 retryable failures + 1 recovery success"
    );
    assert_eq!(
        fast_handle.join().expect("join fast server"),
        1,
        "fast served exactly one fallback turn"
    );
}

// ── agent_send_with_fallback: both profiles down → primary error, restore ──
// default AND fast are dead. The wrapper swaps to fast, fails again, restores
// the desired profile (set_degraded(false)) and surfaces the PRIMARY error.
#[test]
#[serial]
fn fallback_both_profiles_down_returns_primary_error_and_restores_desired() {
    let temp = setup_empty_fixture();
    let fixtures = fixtures_dir(&temp);

    // default: 3 retryable 503s (first send fails) + 1 success for the later
    // restored-desired send. fast: 3 retryable 503s (fallback also fails).
    let (default_url, default_handle) = spawn_scripted_server(vec![
        ServerStep::Fail503,
        ServerStep::Fail503,
        ServerStep::Fail503,
        ServerStep::Reply("default is back".to_string()),
    ]);
    let (fast_url, fast_handle) = spawn_scripted_server(vec![
        ServerStep::Fail503,
        ServerStep::Fail503,
        ServerStep::Fail503,
    ]);
    write_profiles_config(&fixtures, &default_url, &fast_url);

    let host = RuntimeHost::boot(&fixtures).expect("host should boot");
    let sid = host
        .agent_start_with(
            AgentSessionKind::RuntimeShell,
            AgentStartOptions {
                profile: Some("default".to_string()),
                ..Default::default()
            },
        )
        .expect("agent start")
        .session_id;

    let err = host
        .agent_send_with_fallback(&sid, "anyone home?")
        .expect_err("both profiles down must return an error");
    // Primary error is an LLM request failure (HTTP 503 exhausted), surfaced
    // in preference to the fallback error.
    assert!(
        matches!(err, RuntimeError::LlmRequestFailed { .. }),
        "expected primary LlmRequestFailed, got {err:?}"
    );

    // The failure path restored `desired` (default) and cleared degraded, so
    // the next send goes straight through default — now healthy — proving the
    // profile was restored rather than pinned to `fast`.
    let reply = host
        .agent_send_with_fallback(&sid, "retry")
        .expect("restored desired profile should serve once healthy");
    assert_eq!(reply.content, "default is back");

    assert_eq!(
        default_handle.join().expect("join default server"),
        4,
        "default: 3 failures on the first send + 1 success on retry"
    );
    assert_eq!(
        fast_handle.join().expect("join fast server"),
        3,
        "fast: 3 retryable failures during the fallback attempt"
    );
}

// ── agent_send_with_fallback: multi-attempt retry inside send_chat_request ──
// The mock accepts the connection but returns HTTP 500 twice, then 200 with a
// valid reply. Exercises the retry loop (AGENT_REQUEST_MAX_ATTEMPTS=3) that
// precedes any fallback switch — the request succeeds on the SAME profile
// without degrading.
#[test]
#[serial]
fn agent_send_retries_transient_5xx_then_succeeds_without_degrading() {
    let temp = setup_empty_fixture();
    let fixtures = fixtures_dir(&temp);

    // Bind one listener and hand-serve: 500, 500, then a real SSE 200.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind retry server");
    let addr = listener.local_addr().expect("addr");
    let server = std::thread::spawn(move || {
        use std::io::{BufRead, BufReader, Read as _, Write as _};
        let mut served = 0usize;
        for attempt in 0..3 {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut line = String::new();
            reader.read_line(&mut line).expect("request line");
            let mut content_length = 0usize;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).expect("header");
                if header == "\r\n" {
                    break;
                }
                if let Some(v) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = v.trim().parse().expect("len");
                }
            }
            let mut body = vec![0u8; content_length];
            reader.read_exact(&mut body).expect("body");
            served += 1;
            if attempt < 2 {
                // Transient failure: 500 is retryable (not a permanent 4xx).
                write!(
                    stream,
                    "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 5\r\nConnection: close\r\n\r\nretry"
                )
                .expect("write 500");
                stream.flush().expect("flush 500");
            } else {
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n"
                )
                .expect("headers");
                for (_, chunk) in assistant_reply("retry_ok", "recovered after retries") {
                    write!(stream, "{:X}\r\n{}\r\n", chunk.len(), chunk).expect("chunk");
                }
                write!(stream, "0\r\n\r\n").expect("end");
                stream.flush().expect("flush");
            }
        }
        served
    });

    write_single_profile_config(&fixtures, &format!("http://{addr}/v1"));

    let host = RuntimeHost::boot(&fixtures).expect("host should boot");
    let sid = host
        .agent_start(AgentSessionKind::RuntimeShell)
        .expect("agent start")
        .session_id;

    let reply = host
        .agent_send(&sid, "please retry")
        .expect("retry loop should recover on the third attempt");
    assert_eq!(reply.content, "recovered after retries");
    assert_eq!(
        server.join().expect("join retry server"),
        3,
        "server should see all three attempts (500, 500, 200)"
    );

    // No fallback registry here → no profile issue recorded despite retries.
    assert!(
        !host
            .kernel()
            .plugin_issues()
            .iter()
            .any(|i| i.root_plugin_path == "/llm-profile"),
        "in-profile retry recovery must not record a fallback issue"
    );
}
