//! `qq_recall`：按 message_id 撤回 bot 自己发的消息（OneBot v11 `delete_msg`）。
//!
//! 走的是 kernel 的同一个入口（`qq::__test_call_handle` → `api_handle`），用
//! 一个本地 OneBot stub 冒充渠道，不连任何真实地址。三条约定：
//!   1. 成功 → ok=true，message 说明撤的是哪一条，data 是上游原响应；
//!   2. 失败（不是自己发的 / 消息不存在 / 上游超时）→ ok=false，`error`
//!      里带**上游原文**；
//!   3. `message_id` 数字、数字字符串、消息头里整串 `msg=<id>` 都接受。

use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

type SeenCalls = Arc<Mutex<Vec<(String, Value)>>>;

/// 最小 OneBot v11 stub：记录每次 (path, 参数)，`fail` 为真时回 OneBot 的
/// 失败包（`status: failed` + `retcode` + `wording`）。
fn spawn_fake_onebot(fail: bool) -> (String, SeenCalls) {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind fake OneBot");
    let port = server
        .server_addr()
        .to_ip()
        .expect("fake server must listen on an IP socket")
        .port();
    let seen: SeenCalls = Arc::new(Mutex::new(Vec::new()));
    let recorder = seen.clone();
    let fail = AtomicBool::new(fail);
    thread::spawn(move || {
        for mut request in server.incoming_requests() {
            let mut body = String::new();
            let _ = request.as_reader().read_to_string(&mut body);
            let params: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            recorder
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .push((request.url().to_string(), params));
            let reply = if fail.load(Ordering::SeqCst) {
                json!({
                    "status": "failed",
                    "retcode": 1200,
                    "wording": "delete_msg failed: message not found"
                })
            } else {
                json!({"status": "ok", "retcode": 0, "data": {}})
            };
            let header =
                tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                    .expect("static header is valid");
            let _ = request
                .respond(tiny_http::Response::from_string(reply.to_string()).with_header(header));
        }
    });
    (format!("http://127.0.0.1:{port}"), seen)
}

/// 经 kernel 的同一个入口调一个节点，返回解析后的响应。
fn call(request: Value) -> Value {
    let raw = qq::__test_call_handle(request.to_string());
    serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("plugin response is not JSON ({e}): {raw}"))
}

#[test]
fn recall_deletes_its_own_message() {
    let (url, seen) = spawn_fake_onebot(false);

    for message_id in [json!(4242), json!("4242"), json!("msg=4242")] {
        let resp = call(json!({
            "node_id": "qq_recall",
            "payload": { "message_id": message_id, "onebot_url": url },
        }));
        assert_eq!(resp["ok"], true, "{resp}");
        assert_eq!(resp["node_id"], "qq_recall");
        assert_eq!(resp["message"], "recalled message 4242", "{resp}");
        assert!(resp["error"].is_null(), "{resp}");
        assert_eq!(resp["data"]["status"], "ok", "{resp}");
    }

    let calls = seen.lock().unwrap_or_else(|poison| poison.into_inner());
    assert_eq!(calls.len(), 3, "each recall is one delete_msg call");
    for (path, params) in calls.iter() {
        assert!(path.ends_with("/delete_msg"), "path: {path}");
        // 能当整数就发整数：go-cqhttp / NapCat 都按 int 解析 message_id。
        assert_eq!(params["message_id"], json!(4242), "{params}");
    }
}

#[test]
fn failed_recall_keeps_the_upstream_error_verbatim() {
    let (url, _seen) = spawn_fake_onebot(true);
    let resp = call(json!({
        "node_id": "qq_recall",
        "payload": { "message_id": 555, "onebot_url": url },
    }));
    assert_eq!(resp["ok"], false, "{resp}");
    assert_eq!(resp["node_id"], "qq_recall");
    assert_eq!(resp["message"], "failed to recall message 555", "{resp}");
    let error = resp["error"].as_str().unwrap_or_default();
    assert!(error.contains("retcode=1200"), "{resp}");
    assert!(
        error.contains("delete_msg failed: message not found"),
        "the upstream wording must survive verbatim: {resp}"
    );
    // 实际发出的参数也回给调用方，便于核对撤的是哪一条。
    assert_eq!(resp["data"]["message_id"], json!(555), "{resp}");
}

#[test]
fn transport_failure_is_ok_false_with_the_upstream_text() {
    // 没有 OneBot 在听这个端口：连接失败 / 超时都要落成 ok=false + 原文。
    let resp = call(json!({
        "node_id": "qq_recall",
        "payload": { "message_id": 1, "onebot_url": "http://127.0.0.1:1" },
    }));
    assert_eq!(resp["ok"], false, "{resp}");
    let error = resp["error"].as_str().unwrap_or_default();
    assert!(error.contains("HTTP POST"), "{resp}");
}

#[test]
fn missing_message_id_is_a_clear_local_error() {
    for request in [
        json!({ "node_id": "qq_recall" }),
        json!({ "node_id": "qq_recall", "payload": { "message_id": "   " } }),
        json!({ "node_id": "qq_recall", "message_id": null }),
    ] {
        let resp = call(request);
        assert_eq!(resp["ok"], false, "{resp}");
        let error = resp["error"].as_str().unwrap_or_default();
        assert!(
            error.starts_with("message_id is required for qq_recall"),
            "{resp}"
        );
    }
}
