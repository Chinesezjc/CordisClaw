//! `qq_send` 的出站历史：每次成功发送后追加一行到
//! `data/qq_sent/<群号>.jsonl`，`qq_get_sent` 只读这一条路径读回最近若干条。
//!
//! 这里走的是 kernel 的同一个入口（`qq::__test_call_handle` → `api_handle`），
//! 用一个本地 OneBot stub 冒充渠道，不连任何真实地址。

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

/// 一个唯一、已创建的临时数据目录（用例自己清理）。
fn temp_root(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let dir = std::env::temp_dir().join(format!(
        "qq-sent-it-{}-{}-{}",
        std::process::id(),
        tag,
        nanos
    ));
    std::fs::create_dir_all(&dir).expect("create temp data dir");
    dir
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

/// 最小 OneBot v11 stub：`get_login_info` / `get_group_member_info`（未禁言）
/// 让发送闸门放行，`send_group_msg` 回一个递增的 message_id。
fn spawn_fake_onebot() -> String {
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind fake OneBot");
    let port = server
        .server_addr()
        .to_ip()
        .expect("fake server must listen on an IP socket")
        .port();
    thread::spawn(move || {
        let mut message_id = 1000i64;
        for mut request in server.incoming_requests() {
            let mut body = String::new();
            let _ = request.as_reader().read_to_string(&mut body);
            let reply = if request.url().ends_with("/get_login_info") {
                json!({"status":"ok","retcode":0,"data":{"user_id":10000}})
            } else if request.url().ends_with("/get_group_member_info") {
                json!({"status":"ok","retcode":0,"data":{"shut_up_timestamp":0}})
            } else if request.url().ends_with("/send_group_msg") {
                message_id += 1;
                json!({"status":"ok","retcode":0,"data":{"message_id":message_id}})
            } else {
                json!({"status":"ok","retcode":0,"data":{}})
            };
            let header =
                tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                    .expect("static header is valid");
            let _ = request
                .respond(tiny_http::Response::from_string(reply.to_string()).with_header(header));
        }
    });
    format!("http://127.0.0.1:{port}")
}

/// 经 kernel 的同一个入口调一个节点，返回解析后的响应。
fn call(request: Value) -> Value {
    let raw = qq::__test_call_handle(request.to_string());
    serde_json::from_str(&raw)
        .unwrap_or_else(|e| panic!("plugin response is not JSON ({e}): {raw}"))
}

/// 这个群可以发言：内置默认是关闭的，不写策略 `qq_send` 会直接拒绝。
fn allow_group(root: &Path) {
    std::fs::write(
        root.join("qq_reply_policies.json"),
        json!({ "default": { "enabled": true } }).to_string(),
    )
    .expect("write reply policy");
}

#[test]
fn qq_send_records_and_qq_get_sent_reads_back_in_order() {
    let root = temp_root("records");
    allow_group(&root);
    let data_dir = root.to_str().expect("utf-8 temp path");
    let url = spawn_fake_onebot();
    let group = 900_000_123i64;
    let target = format!("group:{group}");

    let started = now_ms();
    for text in ["第一条", "第二条"] {
        let resp = call(json!({
            "node_id": "qq_send",
            "data_dir": data_dir,
            "target": target,
            "message": text,
            "payload": { "onebot_url": url },
        }));
        assert_eq!(resp["ok"], true, "qq_send failed: {resp}");
    }
    let finished = now_ms();

    // 记录落在 qq_send 自己写的那条路径上。
    let history = root.join("qq_sent").join(format!("{group}.jsonl"));
    assert!(history.is_file(), "missing {}", history.display());

    let resp = call(json!({
        "node_id": "qq_get_sent",
        "data_dir": data_dir,
        "group": group.to_string(),
    }));
    assert_eq!(resp["ok"], true, "{resp}");
    let data = &resp["data"];
    assert_eq!(data["count"], 2, "{resp}");
    let items = data["items"].as_array().expect("items array");
    assert_eq!(items.len(), 2, "{resp}");

    // 顺序：先发的在前。
    assert_eq!(items[0]["message"], "第一条");
    assert_eq!(items[1]["message"], "第二条");
    assert_eq!(items[0]["target"], target);
    assert_eq!(items[1]["target"], target);

    // 时间戳：升序，而且就落在两次发送之间。
    let first_at = items[0]["at_ms"].as_u64().expect("at_ms on item 0");
    let second_at = items[1]["at_ms"].as_u64().expect("at_ms on item 1");
    assert!(first_at <= second_at, "ascending: {first_at} <= {second_at}");
    assert!(
        first_at >= started && second_at <= finished,
        "timestamps must sit between the sends: {first_at}..{second_at} vs {started}..{finished}"
    );

    // OneBot 回的 message_id 也一起记下来，方便按 id 引用。
    let first_id = items[0]["msg_id"].as_i64().expect("msg_id on item 0");
    let second_id = items[1]["msg_id"].as_i64().expect("msg_id on item 1");
    assert!(first_id > 0, "{first_id}");
    assert!(second_id > first_id, "{first_id} -> {second_id}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn missing_history_is_empty_and_limit_keeps_the_newest() {
    let root = temp_root("limits");
    let data_dir = root.to_str().expect("utf-8 temp path");
    let group = 900_000_456i64;

    // 文件不存在 → 空数组，不是报错。
    let resp = call(json!({
        "node_id": "qq_get_sent",
        "data_dir": data_dir,
        "group": group.to_string(),
    }));
    assert_eq!(resp["ok"], true, "{resp}");
    assert_eq!(resp["data"]["count"], 0, "{resp}");
    assert!(resp["data"]["items"]
        .as_array()
        .expect("items array")
        .is_empty());

    // 手写 5 条（格式就是 qq_send 落盘的格式）：limit 只给最近几条，仍然升序；
    // 坏行跳过而不是让整份历史读不出来。
    let dir = root.join("qq_sent");
    std::fs::create_dir_all(&dir).expect("create qq_sent dir");
    let mut body = String::new();
    for index in 1..=5u64 {
        body.push_str(
            &json!({
                "target": format!("group:{group}"),
                "message": format!("第 {index} 条"),
                "at_ms": 1_700_000_000_000u64 + index,
                "msg_id": index as i64,
            })
            .to_string(),
        );
        body.push('\n');
    }
    body.push_str("{not json\n");
    std::fs::write(dir.join(format!("{group}.jsonl")), body).expect("write sent history");

    let resp = call(json!({
        "node_id": "qq_get_sent",
        "data_dir": data_dir,
        "group": group.to_string(),
        "payload": { "limit": 2 },
    }));
    assert_eq!(resp["ok"], true, "{resp}");
    assert_eq!(resp["data"]["limit"], 2, "{resp}");
    let items = resp["data"]["items"].as_array().expect("items array");
    assert_eq!(items.len(), 2, "{resp}");
    assert_eq!(items[0]["message"], "第 4 条");
    assert_eq!(items[1]["message"], "第 5 条");
    assert!(items[0]["at_ms"].as_u64() < items[1]["at_ms"].as_u64());

    // limit 超过上限不报错，只被夹到 500。
    let resp = call(json!({
        "node_id": "qq_get_sent",
        "data_dir": data_dir,
        "group": group.to_string(),
        "payload": { "limit": 10_000 },
    }));
    assert_eq!(resp["data"]["limit"], 500, "{resp}");
    assert_eq!(resp["data"]["items"].as_array().map(Vec::len), Some(5));

    // `group:<id>` 也能用（模型常把 reply_target 整串抄过来）。
    let resp = call(json!({
        "node_id": "qq_get_sent",
        "data_dir": data_dir,
        "group": format!("group:{group}"),
        "payload": { "limit": 1 },
    }));
    assert_eq!(resp["data"]["count"], 1, "{resp}");
    assert_eq!(resp["data"]["items"][0]["message"], "第 5 条");

    // group 必填：缺了或全空白都是同一个明确错误。
    for request in [
        json!({ "node_id": "qq_get_sent", "data_dir": data_dir }),
        json!({ "node_id": "qq_get_sent", "data_dir": data_dir, "group": "   " }),
    ] {
        let resp = call(request);
        assert_eq!(resp["ok"], false, "{resp}");
        let error = resp["error"].as_str().unwrap_or_default();
        assert!(error.contains("group is required"), "{resp}");
    }

    let _ = std::fs::remove_dir_all(&root);
}

// 读路径不接受任意文件名：只有 `<data>/qq_sent/<清洗后的群号>.jsonl`。
#[test]
fn path_like_group_names_cannot_escape_the_sent_history_dir() {
    let root = temp_root("escape");
    let data_dir = root.to_str().expect("utf-8 temp path");
    let secret = root.join("secret.jsonl");
    std::fs::write(
        &secret,
        json!({"target":"group:1","message":"secret","at_ms":1,"msg_id":1}).to_string(),
    )
    .expect("write a sibling file the node must never read");

    for group in ["../secret", "../../secret", "/etc/passwd", "..", "."] {
        let resp = call(json!({
            "node_id": "qq_get_sent",
            "data_dir": data_dir,
            "group": group,
        }));
        assert_eq!(resp["ok"], true, "{resp}");
        assert_eq!(
            resp["data"]["count"], 0,
            "group {group} must not read anything outside qq_sent/: {resp}"
        );
    }

    // 私聊目标写在 private-<uid>.jsonl，同一个只读入口也能读回来。
    let dir = root.join("qq_sent");
    std::fs::create_dir_all(&dir).expect("create qq_sent dir");
    std::fs::write(
        dir.join("private-789.jsonl"),
        json!({"target":"private:789","message":"私聊","at_ms":2,"msg_id":None::<i64>}).to_string()
            + "\n",
    )
    .expect("write private history");
    let resp = call(json!({
        "node_id": "qq_get_sent",
        "data_dir": data_dir,
        "group": "private-789",
    }));
    assert_eq!(resp["data"]["count"], 1, "{resp}");
    assert_eq!(resp["data"]["items"][0]["message"], "私聊");
    assert!(resp["data"]["items"][0]["msg_id"].is_null(), "{resp}");

    let _ = std::fs::remove_dir_all(&root);
}
