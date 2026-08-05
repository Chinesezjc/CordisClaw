//! `llm_openai` 传输层测试。
//!
//! 这些用例随传输代码一起从 kernel 搬来——它们考的是 HTTP/SSE 行为（重试、
//! 状态码分流、增量解析、流式旁路），而这些代码现在住在这里。kernel 侧只保留
//! agent 循环的测试。
//!
//! 复用仓库既有手法：起一个说 OpenAI SSE 的 `TcpListener` 当上游。

use serde_json::json;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// 一个脚本化的上游：按序对每个连接回放一段响应。
/// 返回 (base_url, 收到的请求体, join handle)。
fn spawn_upstream(
    script: Vec<(u16, String)>,
) -> (String, mpsc::Receiver<String>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind upstream");
    let addr = listener.local_addr().expect("addr");
    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        for (status, body) in script {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut line = String::new();
            reader.read_line(&mut line).ok();
            let mut len = 0usize;
            loop {
                let mut h = String::new();
                if reader.read_line(&mut h).unwrap_or(0) == 0 {
                    break;
                }
                if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
                if h.trim().is_empty() {
                    break;
                }
            }
            let mut buf = vec![0u8; len];
            reader.read_exact(&mut buf).ok();
            let _ = tx.send(String::from_utf8_lossy(&buf).to_string());

            let reason = if status == 200 { "OK" } else { "Error" };
            let head = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body.as_bytes());
            let _ = stream.flush();
        }
    });
    (format!("http://{addr}/v1"), rx, handle)
}

fn sse(events: &[serde_json::Value]) -> String {
    let mut out = String::new();
    for e in events {
        out.push_str(&format!("data: {e}\n\n"));
    }
    out.push_str("data: [DONE]\n\n");
    out
}

fn content_chunk(text: &str) -> serde_json::Value {
    json!({"id":"resp-1","choices":[{"delta":{"content":text}}]})
}

fn request(base_url: &str, sink: Option<(&str, &str)>) -> String {
    let mut payload = json!({
        "node_id": "llm_complete",
        "body": {"model":"m","messages":[{"role":"user","content":"hi"}]},
        "transport": {
            "base_url": base_url,
            "api_key_env": "LLM_OPENAI_TEST_KEY",
            "timeout_ms": 5000,
            "stream_timeout_secs": 5
        }
    });
    if let Some((addr, key)) = sink {
        payload["sink_addr"] = json!(addr);
        payload["sink_key"] = json!(key);
    }
    payload.to_string()
}

fn invoke(payload: String) -> serde_json::Value {
    let resp = llm_openai::__test_handle(payload);
    serde_json::from_str(&resp).expect("response is json")
}

/// 正常路径：SSE 增量被重组成完整消息，且 Bearer 头带上了 env 里的 key。
#[test]
fn streams_sse_into_a_complete_message() {
    std::env::set_var("LLM_OPENAI_TEST_KEY", "sk-test");
    let (base, reqs, handle) = spawn_upstream(vec![(
        200,
        sse(&[content_chunk("Hel"), content_chunk("lo")]),
    )]);
    let out = invoke(request(&base, None));
    assert_eq!(out["ok"], json!(true), "got: {out}");
    assert_eq!(out["message"]["content"], json!("Hello"));
    let body = reqs
        .recv_timeout(Duration::from_secs(5))
        .expect("captured request");
    assert!(
        body.contains("\"stream\":true"),
        "must request streaming: {body}"
    );
    handle.join().ok();
}

/// 5xx 是可重试的：第一次 503、第二次 200，最终成功。
/// 这是 profile **内**的重试，随传输一起搬到插件；跨 profile 降级仍在 kernel。
#[test]
fn retries_transient_5xx_then_succeeds() {
    std::env::set_var("LLM_OPENAI_TEST_KEY", "sk-test");
    let (base, reqs, handle) = spawn_upstream(vec![
        (503, "upstream busy".to_string()),
        (200, sse(&[content_chunk("ok")])),
    ]);
    let out = invoke(request(&base, None));
    assert_eq!(out["ok"], json!(true), "got: {out}");
    assert_eq!(out["message"]["content"], json!("ok"));
    // 收到两次请求 = 确实重试了。
    assert!(reqs.recv_timeout(Duration::from_secs(5)).is_ok());
    assert!(reqs.recv_timeout(Duration::from_secs(5)).is_ok());
    handle.join().ok();
}

/// 4xx 是永久错误：不重试，直接把错误报上去。
#[test]
fn does_not_retry_permanent_4xx() {
    std::env::set_var("LLM_OPENAI_TEST_KEY", "sk-test");
    let (base, reqs, handle) = spawn_upstream(vec![
        (400, "{\"error\":{\"message\":\"bad model\"}}".to_string()),
        (200, sse(&[content_chunk("unreachable")])),
    ]);
    let out = invoke(request(&base, None));
    assert_eq!(out["ok"], json!(false), "4xx must fail: {out}");
    assert!(
        out["error"]
            .as_str()
            .unwrap_or_default()
            .contains("bad model"),
        "error text should surface upstream message: {out}"
    );
    assert!(reqs.recv_timeout(Duration::from_secs(5)).is_ok());
    // 第二次请求不该发生。
    assert!(
        reqs.recv_timeout(Duration::from_millis(300)).is_err(),
        "a permanent 4xx must not be retried"
    );
    // 不 join：脚本里排了第二段响应而它按预期不会被取用，upstream 线程此刻正
    // 卡在 accept 上。这恰恰是本用例要证明的事——join 反而会永久阻塞。
    drop(handle);
}

/// 缺 API key 时直接失败，且不发出任何请求。
#[test]
fn missing_api_key_fails_before_sending() {
    std::env::remove_var("LLM_OPENAI_ABSENT_KEY");
    let payload = json!({
        "node_id": "llm_complete",
        "body": {"model":"m"},
        "transport": {
            "base_url": "http://127.0.0.1:1",
            "api_key_env": "LLM_OPENAI_ABSENT_KEY",
            "timeout_ms": 1000,
            "stream_timeout_secs": 1
        }
    })
    .to_string();
    let out = invoke(payload);
    assert_eq!(out["ok"], json!(false));
    assert!(
        out["error"]
            .as_str()
            .unwrap_or_default()
            .contains("LLM_OPENAI_ABSENT_KEY"),
        "error must name the missing env var: {out}"
    );
}

/// 把 api_key_env 误填成密钥本身会被拒绝——否则密钥会被当成变量名去查，
/// 报"变量不存在"而掩盖真正的配置错误。
#[test]
fn rejects_api_key_value_in_the_env_name_slot() {
    let payload = json!({
        "node_id": "llm_complete",
        "body": {"model":"m"},
        "transport": {
            "base_url": "http://127.0.0.1:1",
            "api_key_env": "sk-proj-abcdefghijklmnopqrstuvwxyz0123456789",
            "timeout_ms": 1000,
            "stream_timeout_secs": 1
        }
    })
    .to_string();
    let out = invoke(payload);
    assert_eq!(out["ok"], json!(false));
    assert!(
        out["error"]
            .as_str()
            .unwrap_or_default()
            .contains("environment variable NAME"),
        "must explain the misconfiguration: {out}"
    );
}

/// 流式旁路：增量按序抵达 sink，**且完整内容仍在返回值里**。
/// 两条路径读的是同一次补全——REPL 靠增量显示，inbox 靠返回值。
#[test]
fn streams_deltas_to_the_sink_and_still_returns_full_content() {
    std::env::set_var("LLM_OPENAI_TEST_KEY", "sk-test");
    let sink = TcpListener::bind("127.0.0.1:0").expect("bind sink");
    let sink_addr = sink.local_addr().expect("sink addr").to_string();
    let (frames_tx, frames_rx) = mpsc::channel();
    let sink_thread = thread::spawn(move || {
        let Ok((stream, _)) = sink.accept() else {
            return;
        };
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            let _ = frames_tx.send(line);
        }
    });

    let (base, _reqs, handle) = spawn_upstream(vec![(
        200,
        sse(&[content_chunk("Hel"), content_chunk("lo")]),
    )]);
    let out = invoke(request(&base, Some((&sink_addr, "key-1"))));
    assert_eq!(
        out["message"]["content"],
        json!("Hello"),
        "full text in return value"
    );

    let mut got = Vec::new();
    while let Ok(line) = frames_rx.recv_timeout(Duration::from_secs(5)) {
        got.push(line);
        if got.len() >= 3 {
            break;
        }
    }
    // 首帧必须是握手，随后是按序的增量。
    assert_eq!(
        got[0], r#"{"t":"key","key":"key-1"}"#,
        "handshake first: {got:?}"
    );
    assert_eq!(got[1], r#"{"t":"content","d":"Hel"}"#, "{got:?}");
    assert_eq!(got[2], r#"{"t":"content","d":"lo"}"#, "{got:?}");
    handle.join().ok();
    sink_thread.join().ok();
}

/// sink 地址连不上时**降级为非流式**，补全照常返回。
/// 这是"sink 是纯旁路"这条契约的核心保证。
#[test]
fn unreachable_sink_degrades_to_non_streaming() {
    std::env::set_var("LLM_OPENAI_TEST_KEY", "sk-test");
    // 绑一个端口随即关闭，确保它无人监听。
    let dead = TcpListener::bind("127.0.0.1:0").expect("bind");
    let dead_addr = dead.local_addr().expect("addr").to_string();
    drop(dead);

    let (base, _reqs, handle) = spawn_upstream(vec![(200, sse(&[content_chunk("fine")]))]);
    let out = invoke(request(&base, Some((&dead_addr, "key-1"))));
    assert_eq!(
        out["ok"],
        json!(true),
        "a dead sink must not fail the completion: {out}"
    );
    assert_eq!(out["message"]["content"], json!("fine"));
    handle.join().ok();
}

/// 未知 node_id 报错而不是 panic。
#[test]
fn unknown_node_id_is_an_error() {
    let out = invoke(json!({"node_id": "nope"}).to_string());
    assert_eq!(out["ok"], json!(false));
    assert!(out["error"].as_str().unwrap_or_default().contains("nope"));
}

/// 畸形 payload 报错而不是 panic。
#[test]
fn malformed_payload_is_an_error() {
    let out = invoke("not json".to_string());
    assert_eq!(out["ok"], json!(false));
    assert!(out["error"]
        .as_str()
        .unwrap_or_default()
        .contains("invalid llm_openai request payload"));
}
