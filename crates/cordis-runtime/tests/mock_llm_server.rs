//! `support::spawn_chunked_mock_llm_server_sequence` 自身的行为测试。
//!
//! 这个 mock 曾经在 accept 超时时**静默**送出一个部分（常常为空）的请求向量，
//! 调用点写 `let _ = requests_rx.recv()` 就把"一个请求都没抓到"这件事丢掉了。
//! 后果是一个根本没人拨的 mock 能烧满 CI 的 15 分钟 accept 预算而测试照常绿：
//! `host_boot_session_arms` 的四个用例改由假 provider 插件应答后正是如此，
//! 单个 suite 从 4.75 秒涨到 3602 秒，CI 从 23 分钟涨到 84 分钟。
//!
//! 现在超时改为 panic，且首次 accept 有独立的短预算。这里直接注入毫秒级预算
//! 来钉住这两条，不必真等 2 分钟。

use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

mod support;
use support::{spawn_chunked_mock_llm_server_sequence_with_timeouts, sse_response};

const TINY: Duration = Duration::from_millis(200);
/// 远大于 `TINY`：用来证明超时用的是"首次"预算而不是后续预算。
const HUGE: Duration = Duration::from_secs(300);

fn one_turn(id: &str, content: &str) -> Vec<(u64, String)> {
    sse_response(vec![
        serde_json::json!({ "id": id, "choices": [{ "delta": { "content": content } }] }),
        serde_json::json!({ "id": id, "choices": [{ "delta": {}, "finish_reason": "stop" }] }),
    ])
}

/// 发一个最小的 chat-completions 请求并读完响应体。
fn dial(url: &str) {
    let address = url
        .trim_start_matches("http://")
        .trim_end_matches("/v1")
        .to_string();
    let mut stream = TcpStream::connect(&address).expect("connect to mock");
    let body = r#"{"model":"m","messages":[]}"#;
    write!(
        stream,
        "POST /v1/chat/completions HTTP/1.1\r\nHost: {address}\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .expect("write request");
    stream.flush().expect("flush request");
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    while reader.read_line(&mut line).expect("read response") > 0 {
        if line.trim_end() == "0" {
            break;
        }
        line.clear();
    }
}

/// 没人拨号时，server 必须 panic 而不是静默送出一个空向量。
/// `join()` 因此返回 `Err`，调用点的 `.expect("join mock server")` 会炸。
#[test]
fn never_dialled_mock_panics_instead_of_reporting_success() {
    let (_url, requests_rx, handle) = spawn_chunked_mock_llm_server_sequence_with_timeouts(
        vec![one_turn("never", "unused")],
        TINY,
        HUGE,
    );

    let joined = handle.join();
    let payload = joined.expect_err("a mock nobody dialled must fail loudly");
    let message = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .expect("panic payload should be a formatted String");
    assert!(
        message.contains("timed out"),
        "the panic must name the timeout: {message}"
    );
    assert!(
        message.contains("dead scaffolding"),
        "the panic must point at the likely cause: {message}"
    );

    // 线程 panic 掉了 sender，所以 recv 也失败——旧行为下这里会拿到一个
    // 冒充正常结果的空向量。
    requests_rx
        .recv()
        .expect_err("no partial result may masquerade as a completed capture");
}

/// 首次 accept 用的是首次预算，不是后续 accept 的长预算。
/// `HUGE` 作为后续预算传入，若被误用于首次，这个测试会挂满 300 秒。
#[test]
fn first_accept_uses_the_short_budget() {
    let started = Instant::now();
    let (_url, _rx, handle) = spawn_chunked_mock_llm_server_sequence_with_timeouts(
        vec![one_turn("never", "unused")],
        TINY,
        HUGE,
    );
    handle.join().expect_err("must time out on the first accept");

    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(30),
        "the first accept must honour the short budget, took {elapsed:?}"
    );
}

/// 正常路径不受影响：脚本里的响应都被拨到时，server 正常送出捕获结果并干净退出。
#[test]
fn fully_dialled_sequence_still_reports_every_request() {
    let (url, requests_rx, handle) = spawn_chunked_mock_llm_server_sequence_with_timeouts(
        vec![one_turn("a", "first"), one_turn("b", "second")],
        HUGE,
        HUGE,
    );

    dial(&url);
    dial(&url);

    let captured = requests_rx.recv().expect("captured requests");
    assert_eq!(captured.len(), 2, "both scripted turns must be captured");
    assert!(
        captured.iter().all(|r| r.contains("chat/completions")),
        "each capture must hold the raw request: {captured:?}"
    );
    handle.join().expect("a fully dialled mock joins cleanly");
}

/// 后续 accept 仍享有长预算：第一次拨号后停手，server 必须在第二次 accept 上
/// 等待（用 `TINY` 作后续预算让它快速超时），并且 panic 消息报的是 2/2。
#[test]
fn later_accept_timeout_names_the_missing_request() {
    let (url, _rx, handle) = spawn_chunked_mock_llm_server_sequence_with_timeouts(
        vec![one_turn("a", "first"), one_turn("b", "never")],
        HUGE,
        TINY,
    );

    dial(&url);

    let payload = handle.join().expect_err("the second turn was never dialled");
    let message = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .expect("panic payload should be a formatted String");
    assert!(
        message.contains("request 2/2"),
        "the panic must identify which request is missing: {message}"
    );
    assert!(
        message.contains("captured 1 so far"),
        "the panic must report progress made before the stall: {message}"
    );
}
