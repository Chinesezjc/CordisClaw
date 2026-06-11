//! QQ adapter plugin using the NoneBot (OneBot v11) protocol.
//!
//! This plugin communicates with a OneBot-compatible QQ client
//! (e.g. go-cqhttp, NapCat, LLOneBot) via its HTTP API.
//!
//! Nodes:
//! - `qq_entry`         — original multi-action entry (configure/send/status/call)
//! - `qq_serve`         — Task node: starts HTTP server to receive OneBot events
//! - `qq_fetch_messages` — return queued incoming messages (agent polls this)
//! - `qq_send`          — send a message to a group or private chat

use cordis_plugin_sdk::{
    export_plugin_api, json_response, node_doc, plugin_docs, task_node_doc, AbiFingerprint,
    PluginRequest, PluginResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashSet, VecDeque};
use std::sync::Mutex;
use std::thread;
// ---------------------------------------------------------------------------
// Plugin state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct QqState {
    /// OneBot HTTP API base URL, e.g. "http://127.0.0.1:5700"
    onebot_url: Option<String>,
    /// Default target for send actions, e.g. "group:123456" or "private:789012"
    default_target: Option<String>,
    /// Groups allowed to trigger agent (grayscale whitelist)
    allow_groups: Vec<String>,
    /// Groups blocked from triggering agent (blacklist, takes priority over allow_groups)
    block_groups: Vec<String>,
    /// OneBot access token
    access_token: Option<String>,
    /// LLM API config
    llm_api_url: Option<String>,
    llm_api_key: Option<String>,
    llm_model: Option<String>,
}

static STATE: Mutex<QqState> = Mutex::new(QqState {
    onebot_url: None,
    default_target: None,
    allow_groups: Vec::new(),
    block_groups: Vec::new(),
    access_token: None,
    llm_api_url: None,
    llm_api_key: None,
    llm_model: None,
});

/// Incoming message queue — populated by the HTTP server, drained by
/// `qq_fetch_messages`.
static MESSAGE_QUEUE: Mutex<VecDeque<IncomingMessage>> = Mutex::new(VecDeque::new());

/// Server running flag.
static SERVER_RUNNING: Mutex<bool> = Mutex::new(false);

/// Stored agent session ID for message routing.
static AGENT_SESSION_ID: Mutex<Option<String>> = Mutex::new(None);

/// Dedup: recently processed message IDs (prevents double-processing from zombie pollers / duplicate events).
static RECENT_MESSAGE_IDS: std::sync::LazyLock<Mutex<HashSet<String>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashSet::new()));

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IncomingMessage {
    /// "group" | "private"
    message_type: String,
    /// QQ group_id or user_id
    sender_id: String,
    /// Sender nickname or user_id
    user_id: String,
    /// Message text
    message: String,
    /// OneBot message_id for quoting/reply; None for message events missing it
    #[serde(skip_serializing_if = "Option::is_none")]
    message_id: Option<i64>,
    /// Quoted message_id if this message is a reply to another message
    #[serde(skip_serializing_if = "Option::is_none")]
    reply_to_msg_id: Option<i64>,
    /// Raw OneBot event for debugging
    #[serde(skip_serializing_if = "Option::is_none")]
    raw_event: Option<Value>,
}

#[derive(Debug, Deserialize, Serialize)]
struct OneBotEvent {
    #[serde(default)]
    post_type: String,

    // message events
    #[serde(default)]
    message_type: String,
    #[serde(default)]
    message: Value, // can be string or array
    #[serde(default)]
    message_id: Option<Value>, // number or string
    #[serde(default)]
    user_id: Value, // number or string
    #[serde(default)]
    group_id: Option<Value>,
    #[serde(default)]
    sender: Option<OneBotSender>,
    #[serde(default)]
    raw_message: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct OneBotSender {
    #[serde(default)]
    nickname: Option<String>,
    #[serde(default)]
    user_id: Option<Value>,
}

// ---------------------------------------------------------------------------
// Request / Response types (legacy qq_entry)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct QqRequest {
    action: String,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    payload: Option<Value>,
}

#[derive(Debug, Serialize)]
struct QqResponse {
    ok: bool,
    action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

// ---------------------------------------------------------------------------
// Request / Response (new nodes)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct NodeRequest {
    node_id: String,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    reply_to: Option<i64>,
    #[serde(default)]
    payload: Option<Value>,
    #[serde(default)]
    agent_session_id: Option<String>,
}

#[derive(Debug, Serialize)]
struct NodeResponse {
    ok: bool,
    node_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    messages: Option<Vec<IncomingMessage>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

// ---------------------------------------------------------------------------
// OneBot v11 HTTP API helpers (legacy — unchanged)
// ---------------------------------------------------------------------------

fn onebot_call(base_url: &str, endpoint: &str, params: &Value, token: Option<&str>) -> Result<Value, String> {
    let url = format!("{}/{}", base_url.trim_end_matches('/'), endpoint);
    let body = serde_json::to_string(params).map_err(|e| format!("json encode: {e}"))?;

    let mut req = ureq::post(&url).set("Content-Type", "application/json");
    if let Some(t) = token {
        req = req.set("Authorization", &format!("Bearer {t}"));
    }
    let resp = req
        .send_string(&body)
        .map_err(|e| format!("HTTP POST {url}: {e}"))?;

    let status = resp.status();
    let text = resp
        .into_string()
        .map_err(|e| format!("read response body: {e}"))?;

    let parsed: Value =
        serde_json::from_str(&text).map_err(|e| format!("json decode (status {status}): {e}"))?;

    let api_status = parsed
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    if api_status == "failed" {
        let retcode = parsed
            .get("retcode")
            .and_then(|v| v.as_i64())
            .unwrap_or(-1);
        let wording = parsed
            .get("wording")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown error");
        return Err(format!("OneBot API error (retcode={retcode}): {wording}"));
    }
    Ok(parsed)
}

fn onebot_send_private_msg(
    base_url: &str,
    user_id: i64,
    message: &str,
    reply_to: Option<i64>,
    token: Option<&str>,
) -> Result<Value, String> {
    let full_message = build_reply_message(message, reply_to);
    let params = json!({ "user_id": user_id, "message": full_message });
    onebot_call(base_url, "send_private_msg", &params, token)
}

fn onebot_send_group_msg(
    base_url: &str,
    group_id: i64,
    message: &str,
    reply_to: Option<i64>,
    token: Option<&str>,
) -> Result<Value, String> {
    let full_message = build_reply_message(message, reply_to);
    let params = json!({ "group_id": group_id, "message": full_message });
    onebot_call(base_url, "send_group_msg", &params, token)
}

/// Prepend a OneBot reply segment when reply_to is Some.
fn build_reply_message(message: &str, reply_to: Option<i64>) -> String {
    match reply_to {
        Some(mid) => format!("[CQ:reply,id={}]{}", mid, message),
        None => message.to_string(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    Group,
    Private,
}

pub fn parse_target(raw: &str) -> Result<(TargetKind, i64), String> {
    let (kind_str, id_str) = raw
        .split_once(':')
        .ok_or_else(|| format!("invalid target '{raw}': expected 'group:<id>' or 'private:<id>'"))?;
    let id: i64 = id_str
        .trim()
        .parse()
        .map_err(|e| format!("invalid target id '{id_str}': {e}"))?;
    let kind = match kind_str.trim().to_lowercase().as_str() {
        "group" | "g" => TargetKind::Group,
        "private" | "priv" | "p" | "user" | "u" => TargetKind::Private,
        other => return Err(format!("unknown target kind '{other}'; use 'group' or 'private'")),
    };
    Ok((kind, id))
}

// ---------------------------------------------------------------------------
// Legacy qq_entry handlers
// ---------------------------------------------------------------------------

fn handle_legacy(req: QqRequest) -> Result<QqResponse, String> {
    match req.action.as_str() {
        "configure" => handle_configure(req),
        "send" => handle_send(req),
        "status" => handle_status(),
        "call" => handle_call(req),
        "block" => handle_block(req),
        "unblock" => handle_unblock(req),
        "allow_group" => handle_allow_group(req),
        "disallow_group" => handle_disallow_group(req),
        "list_groups" => handle_list_groups(),
        other => Err(format!("unsupported action: {other}")),
    }
}

fn handle_configure(req: QqRequest) -> Result<QqResponse, String> {
    let mut state = STATE.lock().map_err(|e| format!("lock: {e}"))?;
    if let Some(url) = req.url { state.onebot_url = Some(url); }
    if let Some(target) = req.target { parse_target(&target)?; state.default_target = Some(target); }
    // Parse allow_groups from payload.
    if let Some(ref payload) = req.payload {
        if let Some(arr) = payload.get("allow_groups").and_then(|v| v.as_array()) {
            state.allow_groups = arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect();
        }
        if let Some(arr) = payload.get("block_groups").and_then(|v| v.as_array()) {
            state.block_groups = arr.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect();
        }
    }
    Ok(QqResponse {
        ok: true, action: "configure".to_string(),
        message: Some(format!(
            "url={} target={} allow={:?} block={:?}",
            state.onebot_url.as_deref().unwrap_or("(unchanged)"),
            state.default_target.as_deref().unwrap_or("(unchanged)"),
            state.allow_groups,
            state.block_groups,
        )),
        data: None,
    })
}

fn handle_block(req: QqRequest) -> Result<QqResponse, String> {
    let target = req.target.as_deref().ok_or("block requires 'target' field (group:<id>)")?;
    let (_kind, id) = parse_target(target)?;
    let gid = id.to_string();
    let mut state = STATE.lock().map_err(|e| format!("lock: {e}"))?;
    if !state.block_groups.contains(&gid) {
        state.block_groups.push(gid.clone());
    }
    Ok(QqResponse {
        ok: true, action: "block".to_string(),
        message: Some(format!("group {} blocked. block_groups={:?}", gid, state.block_groups)),
        data: None,
    })
}

fn handle_unblock(req: QqRequest) -> Result<QqResponse, String> {
    let target = req.target.as_deref().ok_or("unblock requires 'target' field (group:<id>)")?;
    let (_kind, id) = parse_target(target)?;
    let gid = id.to_string();
    let mut state = STATE.lock().map_err(|e| format!("lock: {e}"))?;
    state.block_groups.retain(|g| g != &gid);
    Ok(QqResponse {
        ok: true, action: "unblock".to_string(),
        message: Some(format!("group {} unblocked. block_groups={:?}", gid, state.block_groups)),
        data: None,
    })
}

fn handle_allow_group(req: QqRequest) -> Result<QqResponse, String> {
    let target = req.target.as_deref().ok_or("allow_group requires 'target' field (group:<id>)")?;
    let (_kind, id) = parse_target(target)?;
    let gid = id.to_string();
    let mut state = STATE.lock().map_err(|e| format!("lock: {e}"))?;
    if !state.allow_groups.contains(&gid) {
        state.allow_groups.push(gid.clone());
    }
    Ok(QqResponse {
        ok: true, action: "allow_group".to_string(),
        message: Some(format!("group {} added to allow list. allow_groups={:?}", gid, state.allow_groups)),
        data: None,
    })
}

fn handle_disallow_group(req: QqRequest) -> Result<QqResponse, String> {
    let target = req.target.as_deref().ok_or("disallow_group requires 'target' field (group:<id>)")?;
    let (_kind, id) = parse_target(target)?;
    let gid = id.to_string();
    let mut state = STATE.lock().map_err(|e| format!("lock: {e}"))?;
    state.allow_groups.retain(|g| g != &gid);
    Ok(QqResponse {
        ok: true, action: "disallow_group".to_string(),
        message: Some(format!("group {} removed from allow list. allow_groups={:?}", gid, state.allow_groups)),
        data: None,
    })
}

fn handle_list_groups() -> Result<QqResponse, String> {
    let state = STATE.lock().map_err(|e| format!("lock: {e}"))?;
    Ok(QqResponse {
        ok: true, action: "list_groups".to_string(),
        message: Some(format!(
            "allow_groups={:?} block_groups={:?}",
            state.allow_groups,
            state.block_groups,
        )),
        data: None,
    })
}

fn handle_send(req: QqRequest) -> Result<QqResponse, String> {
    let message = req.message.as_deref().unwrap_or("").trim().to_string();
    if message.is_empty() { return Err("message is empty".to_string()); }
    let (kind, id) = {
        let state = STATE.lock().map_err(|e| format!("lock: {e}"))?;
        let target_str = req.target.as_deref()
            .or(state.default_target.as_deref())
            .ok_or("no target configured; use 'configure' first or provide a 'target' field")?;
        parse_target(target_str)?
    };
    let base_url = {
        let state = STATE.lock().map_err(|e| format!("lock: {e}"))?;
        state.onebot_url.clone().ok_or("no OneBot URL configured; use 'configure' first")?
    };
    let data = match kind {
        TargetKind::Group => onebot_send_group_msg(&base_url, id, &message, None, None)?,
        TargetKind::Private => onebot_send_private_msg(&base_url, id, &message, None, None)?,
    };
    let msg_id = data.get("data").and_then(|d| d.get("message_id")).and_then(|v| v.as_i64());
    Ok(QqResponse {
        ok: true, action: "send".to_string(),
        message: msg_id.map(|mid| format!("message_id={mid}")),
        data: Some(data),
    })
}

fn handle_status() -> Result<QqResponse, String> {
    let state = STATE.lock().map_err(|e| format!("lock: {e}"))?;
    let connected = if let Some(ref url) = state.onebot_url {
        onebot_call(url, "get_status", &json!({}), None).is_ok()
    } else { false };
    Ok(QqResponse {
        ok: true, action: "status".to_string(),
        message: Some(format!(
            "url={} target={} connected={connected}",
            state.onebot_url.as_deref().unwrap_or("(not set)"),
            state.default_target.as_deref().unwrap_or("(not set)")
        )),
        data: None,
    })
}

fn handle_call(req: QqRequest) -> Result<QqResponse, String> {
    let payload = req.payload.ok_or("missing 'payload' for call action")?;
    let endpoint = payload.get("endpoint").and_then(|v| v.as_str())
        .ok_or("payload must contain 'endpoint' string")?;
    let params = payload.get("params").cloned().unwrap_or(json!({}));
    let base_url = {
        let state = STATE.lock().map_err(|e| format!("lock: {e}"))?;
        state.onebot_url.clone().ok_or("no OneBot URL configured; use 'configure' first")?
    };
    let data = onebot_call(&base_url, endpoint, &params, None)?;
    Ok(QqResponse { ok: true, action: "call".to_string(), message: None, data: Some(data) })
}

// ---------------------------------------------------------------------------
// HTTP Server — receives OneBot event POSTs
// ---------------------------------------------------------------------------

fn run_event_loop(server: tiny_http::Server) {
    for mut request in server.incoming_requests() {
        if request.url() == "/onebot/event" && request.method() == &tiny_http::Method::Post {
            let mut body = String::new();
            if let Ok(_) = request.as_reader().read_to_string(&mut body) {
                let _ = request.respond(tiny_http::Response::from_string(
                    serde_json::to_string(&json!({"status":"ok"})).unwrap(),
                ));
                if let Ok(event) = serde_json::from_str::<OneBotEvent>(&body) {
                    handle_onebot_event(&event);
                }
            } else {
                let _ = request.respond(
                    tiny_http::Response::from_string("bad request")
                        .with_status_code(400),
                );
            }
        } else if request.url() == "/health" {
            let _ = request.respond(tiny_http::Response::from_string(
                serde_json::to_string(&json!({"status":"ok"})).unwrap(),
            ));
        } else {
            let _ = request.respond(
                tiny_http::Response::from_string("not found").with_status_code(404),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// WebSocket Server — receives OneBot events via WS reverse connection
// ---------------------------------------------------------------------------

fn start_ws_server() -> Result<(), String> {
    use std::net::TcpListener;
    let listener = TcpListener::bind("0.0.0.0:8002")
        .map_err(|e| format!("ws: cannot bind port 8000: {e}"))?;
    listener.set_nonblocking(false)
        .map_err(|e| format!("ws: set_nonblocking: {e}"))?;
    eprintln!("[qq] WebSocket server listening on port 8002");
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => { eprintln!("[qq] ws accept: {e}"); continue; }
        };
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(3600)));
        let ws = match tungstenite::accept(stream) {
            Ok(w) => w,
            Err(e) => { eprintln!("[qq] ws handshake: {e}"); continue; }
        };
        eprintln!("[qq] WebSocket client connected");
        handle_ws_connection(ws);
    }
    Ok(())
}

fn handle_ws_connection(mut ws: tungstenite::WebSocket<std::net::TcpStream>) {
    loop {
        match ws.read() {
            Ok(tungstenite::Message::Text(text)) => {
                if let Ok(event) = serde_json::from_str::<OneBotEvent>(&text) {
                    handle_onebot_event(&event);
                } else {
                    // OneBot WS protocol: events may be wrapped differently.
                    // Try parsing as generic JSON and extract event fields.
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&text) {
                        let event = OneBotEvent {
                            post_type: val.get("post_type").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                            message_type: val.get("message_type").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                            message: val.get("message").cloned().unwrap_or_default(),
                            message_id: val.get("message_id").cloned(),
                            user_id: val.get("user_id").cloned().unwrap_or_default(),
                            group_id: val.get("group_id").cloned(),
                            sender: val.get("sender").and_then(|s| {
                                Some(OneBotSender {
                                    nickname: s.get("nickname").and_then(|v| v.as_str()).map(|s| s.to_string()),
                                    user_id: s.get("user_id").cloned(),
                                })
                            }),
                            raw_message: val.get("raw_message").and_then(|v| v.as_str()).map(|s| s.to_string()),
                        };
                        handle_onebot_event(&event);
                    }
                }
            }
            Ok(tungstenite::Message::Ping(data)) => {
                let _ = ws.send(tungstenite::Message::Pong(data));
            }
            Ok(tungstenite::Message::Close(_)) => {
                eprintln!("[qq] WebSocket client disconnected");
                break;
            }
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => {
                break;
            }
            Err(e) => {
                eprintln!("[qq] ws read error: {e}");
                break;
            }
            _ => {}
        }
    }
}

fn handle_onebot_event(event: &OneBotEvent) {
    if event.post_type != "message" {
        return;
    }

    // Extract message text and reply context.
    let (message_text, reply_to_msg_id) = extract_message_info(&event.message, event.raw_message.as_deref());

    // Extract user_id.
    let user_id = match &event.user_id {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        _ => "unknown".to_string(),
    };

    // Only process group messages for grayscale testing.
    let (sender_id, msg_type) = if let Some(ref gid) = event.group_id {
        let gid_str = match gid {
            Value::Number(n) => n.to_string(),
            Value::String(s) => s.clone(),
            _ => return,
        };

        // Blacklist check (takes priority over allow_groups).
        let block = STATE.lock().ok().map(|s| s.block_groups.clone()).unwrap_or_default();
        if block.contains(&gid_str) {
            return;
        }

        // Grayscale whitelist check.
        let allow = STATE.lock().ok().map(|s| s.allow_groups.clone()).unwrap_or_default();
        if !allow.is_empty() && !allow.contains(&gid_str) {
            return;
        }

        (gid_str, "group".to_string())
    } else {
        (user_id.clone(), "private".to_string())
    };

    if message_text.is_empty() {
        return;
    }

    let message_id = event.message_id.as_ref().and_then(|v| match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.parse::<i64>().ok(),
        _ => None,
    });

    let msg = IncomingMessage {
        message_type: msg_type,
        sender_id,
        user_id,
        message: message_text,
        message_id,
        reply_to_msg_id,
        raw_event: Some(serde_json::to_value(event).unwrap_or_default()),
    };

    // ---- dedup at ingest: skip duplicate OneBot events (by message_id only) ----
    // We only dedup by message_id here, not by content hash, because
    // content-hash dedup happens at consumption time in qq_fetch_messages.
    if let Some(mid) = msg.message_id {
        let dedup_key = format!("msg:{}", mid);
        let mut seen = RECENT_MESSAGE_IDS.lock().unwrap_or_else(|p| p.into_inner());
        if seen.contains(&dedup_key) {
            return; // duplicate OneBot event
        }
        seen.insert(dedup_key);
        if seen.len() > 200 {
            let drain_count = seen.len() - 100;
            let keys: Vec<String> = seen.iter().take(drain_count).cloned().collect();
            for k in keys {
                seen.remove(&k);
            }
        }
    }

    if let Ok(mut queue) = MESSAGE_QUEUE.lock() {
        if queue.len() < 128 {
            queue.push_back(msg);
        }
    }
}

/// Returns (message_text, reply_to_msg_id) extracted from the OneBot message.
fn extract_message_info(message: &Value, raw_message: Option<&str>) -> (String, Option<i64>) {
    let mut parts: Vec<String> = Vec::new();
    let mut reply_to: Option<i64> = None;

    // Prefer structured segments array when available.
    // Fall back to raw_message or plain string otherwise.
    if let Value::Array(segments) = message {
        for seg in segments {
            let seg_type = seg.get("type").and_then(|t| t.as_str()).unwrap_or("");
            match seg_type {
                "reply" => {
                    // Extract the replied message id.
                    if let Some(id_val) = seg.get("data").and_then(|d| d.get("id")) {
                        reply_to = extract_i64(id_val);
                    }
                }
                "text" => {
                    if let Some(t) = seg.get("data").and_then(|d| d.get("text")).and_then(|t| t.as_str()) {
                        parts.push(t.to_string());
                    }
                }
                "image" => {
                    if let Some(url) = seg.get("data").and_then(|d| d.get("url")).and_then(|u| u.as_str()) {
                        parts.push(format!("[image: {url}]"));
                    } else if let Some(file) = seg.get("data").and_then(|d| d.get("file")).and_then(|f| f.as_str()) {
                        parts.push(format!("[image file: {file}]"));
                    }
                }
                "at" => {
                    let qq = seg.get("data").and_then(|d| d.get("qq")).and_then(|q| q.as_str()).unwrap_or("unknown");
                    // OneBot implementations may provide a "name" field (group card/nickname).
                    // Fall back to qq number if not available.
                    let name = seg.get("data").and_then(|d| d.get("name")).and_then(|n| n.as_str()).unwrap_or(qq);
                    parts.push(format!("@[id={},name={}]", qq, name));
                }
                _ => {}
            }
        }
    } else if let Some(raw) = raw_message {
        if !raw.is_empty() {
            parts.push(raw.to_string());
        }
    } else if let Value::String(s) = message {
        parts.push(s.clone());
    }

    (parts.join("\n"), reply_to)
}

fn extract_i64(val: &Value) -> Option<i64> {
    match val {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.parse::<i64>().ok(),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// New node handlers
// ---------------------------------------------------------------------------

fn should_process(text: &str) -> bool {
    if text.len() <= 2 { return false; }
    if text.starts_with('/') || text.starts_with("[CQ:") { return false; }
    true
}

fn start_agent_poller() {
    thread::spawn(move || {
        thread::sleep(std::time::Duration::from_secs(2));
        loop {
            let msgs: Vec<IncomingMessage> = {
                let mut queue = MESSAGE_QUEUE.lock().unwrap_or_else(|p| p.into_inner());
                queue.drain(..).collect()
            };
            for msg in msgs {
                if !should_process(&msg.message) { continue; }
                let prompt = format!("[QQ group from {} (user {})]: {}", msg.sender_id, msg.user_id, msg.message);
                cordis_plugin_sdk::agent_trigger(&prompt);
            }
            thread::sleep(std::time::Duration::from_secs(5));
        }
    });
}

fn handle_qq_serve(req: &NodeRequest) -> Result<NodeResponse, String> {
    let port: u16 = req.payload.as_ref()
        .and_then(|p| p.get("port"))
        .and_then(|v| v.as_u64())
        .unwrap_or(8080) as u16;

    let allow_groups: Vec<String> = req.payload.as_ref()
        .and_then(|p| p.get("allow_groups"))
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();

    let block_groups: Vec<String> = req.payload.as_ref()
        .and_then(|p| p.get("block_groups"))
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(|s| s.to_string())).collect())
        .unwrap_or_default();

    // Store configuration.
    {
        let mut state = STATE.lock().map_err(|e| format!("lock: {e}"))?;
        state.allow_groups = allow_groups.clone();
        state.block_groups = block_groups.clone();
        if let Some(url) = req.payload.as_ref().and_then(|p| p.get("onebot_url")).and_then(|v| v.as_str()) {
            state.onebot_url = Some(url.to_string());
        }
        if let Some(t) = req.payload.as_ref().and_then(|p| p.get("access_token")).and_then(|v| v.as_str()) {
            state.access_token = Some(t.to_string());
        }
        if let Some(u) = req.payload.as_ref().and_then(|p| p.get("llm_api_url")).and_then(|v| v.as_str()) {
            state.llm_api_url = Some(u.to_string());
        }
        if let Some(k) = req.payload.as_ref().and_then(|p| p.get("llm_api_key")).and_then(|v| v.as_str()) {
            state.llm_api_key = Some(k.to_string());
        }
        if let Some(m) = req.payload.as_ref().and_then(|p| p.get("llm_model")).and_then(|v| v.as_str()) {
            state.llm_model = Some(m.to_string());
        }
    }

    // Store agent session ID if provided.
    if let Some(ref sid) = req.agent_session_id {
        *AGENT_SESSION_ID.lock().map_err(|e| format!("lock: {e}"))? = Some(sid.clone());
    }

    // Start HTTP server in background thread.  Bind synchronously so we
    // can report errors, then spawn the accept loop.
    {
        let mut running = SERVER_RUNNING.lock().map_err(|e| format!("lock: {e}"))?;
        if !*running {
            *running = true;
            drop(running);
            let server = tiny_http::Server::http(format!("0.0.0.0:{port}"))
                .map_err(|e| format!("qq_serve: cannot bind port {port}: {e}"))?;
            thread::spawn(move || run_event_loop(server));
            start_agent_poller();
        }
    }

    Ok(NodeResponse {
        ok: true,
        node_id: "qq_serve".to_string(),
        messages: None,
        message: Some(format!(
            "HTTP server listening on port {port}, allow_groups={:?}, block_groups={:?}",
            allow_groups, block_groups
        )),
        data: None,
        error: None,
    })
}

fn handle_qq_fetch_messages() -> Result<NodeResponse, String> {
    let drained: Vec<IncomingMessage> = {
        let mut queue = MESSAGE_QUEUE.lock().map_err(|e| format!("lock: {e}"))?;
        queue.drain(..).collect()
    };

    // Filter out messages already processed (dedup).
    let mut messages = Vec::new();
    for msg in drained {
        let dedup_key = match msg.message_id {
            Some(mid) => format!("msg:{}", mid),
            None => format!(
                "hash:{},{},{}",
                msg.sender_id, msg.user_id, msg.message
            ),
        };
        let mut seen = RECENT_MESSAGE_IDS.lock().unwrap_or_else(|p| p.into_inner());
        if seen.contains(&dedup_key) {
            continue; // already processed
        }
        seen.insert(dedup_key);
        if seen.len() > 200 {
            let drain_count = seen.len() - 100;
            let keys: Vec<String> = seen.iter().take(drain_count).cloned().collect();
            for k in keys {
                seen.remove(&k);
            }
        }
        messages.push(msg);
    }

    Ok(NodeResponse {
        ok: true,
        node_id: "qq_fetch_messages".to_string(),
        messages: Some(messages),
        message: None,
        data: None,
        error: None,
    })
}

fn load_runtime_config() -> Option<serde_json::Value> {
    let path = "/root/CordisClaw/fixtures/.cordis-drafts/qq_runtime_config.json";
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

fn handle_qq_get_group_members(req: &NodeRequest) -> Result<NodeResponse, String> {
    let target = req.target.as_deref().unwrap_or("").trim();
    if target.is_empty() { return Err("target is required for qq_get_group_members (group:<id>)".to_string()); }
    let (kind, id) = parse_target(target)?;
    if kind != TargetKind::Group { return Err("target must be group:<id> for qq_get_group_members".to_string()); }

    let base_url = req.payload.as_ref()
        .and_then(|p| p.get("onebot_url")).and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| STATE.lock().ok().and_then(|s| s.onebot_url.clone()))
        .or_else(|| load_runtime_config().and_then(|c| c.get("onebot_url")?.as_str().map(|s| s.to_string())))
        .ok_or("no OneBot URL configured")?;

    let token = req.payload.as_ref()
        .and_then(|p| p.get("access_token")).and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| STATE.lock().ok().and_then(|s| s.access_token.clone()))
        .or_else(|| load_runtime_config().and_then(|c| c.get("access_token")?.as_str().map(|s| s.to_string())));

    let params = json!({ "group_id": id });
    let data = onebot_call(&base_url, "get_group_member_list", &params, token.as_deref())?;

    Ok(NodeResponse {
        ok: true,
        node_id: "qq_get_group_members".to_string(),
        messages: None,
        message: Some(format!("group {} member list", id)),
        data: Some(data),
        error: None,
    })
}

fn handle_qq_get_group_info(req: &NodeRequest) -> Result<NodeResponse, String> {
    let target = req.target.as_deref().unwrap_or("").trim();
    if target.is_empty() { return Err("target is required for qq_get_group_info (group:<id>)".to_string()); }
    let (kind, id) = parse_target(target)?;
    if kind != TargetKind::Group { return Err("target must be group:<id> for qq_get_group_info".to_string()); }

    let base_url = req.payload.as_ref()
        .and_then(|p| p.get("onebot_url")).and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| STATE.lock().ok().and_then(|s| s.onebot_url.clone()))
        .or_else(|| load_runtime_config().and_then(|c| c.get("onebot_url")?.as_str().map(|s| s.to_string())))
        .ok_or("no OneBot URL configured")?;

    let token = req.payload.as_ref()
        .and_then(|p| p.get("access_token")).and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| STATE.lock().ok().and_then(|s| s.access_token.clone()))
        .or_else(|| load_runtime_config().and_then(|c| c.get("access_token")?.as_str().map(|s| s.to_string())));

    let params = json!({ "group_id": id });
    let data = onebot_call(&base_url, "get_group_info", &params, token.as_deref())?;

    let group_name = data.get("data")
        .and_then(|d| d.get("group_name"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    Ok(NodeResponse {
        ok: true,
        node_id: "qq_get_group_info".to_string(),
        messages: None,
        message: Some(format!("group {}: {}", id, group_name)),
        data: Some(data),
        error: None,
    })
}

fn handle_qq_send(req: &NodeRequest) -> Result<NodeResponse, String> {
    let target = req.target.as_deref().unwrap_or("").trim();
    let message = req.message.as_deref().unwrap_or("").trim();
    let reply_to = req.reply_to;

    if target.is_empty() { return Err("target is required for qq_send".to_string()); }
    if message.is_empty() { return Err("message is required for qq_send".to_string()); }

    // Read config: payload → STATE → config file (persisted by qq_serve).
    let base_url = req.payload.as_ref()
        .and_then(|p| p.get("onebot_url")).and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| STATE.lock().ok().and_then(|s| s.onebot_url.clone()))
        .or_else(|| load_runtime_config().and_then(|c| c.get("onebot_url")?.as_str().map(|s| s.to_string())))
        .ok_or("no OneBot URL configured")?;
    let token = req.payload.as_ref()
        .and_then(|p| p.get("access_token")).and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| STATE.lock().ok().and_then(|s| s.access_token.clone()))
        .or_else(|| load_runtime_config().and_then(|c| c.get("access_token")?.as_str().map(|s| s.to_string())));

    let (kind, id) = parse_target(target)?;
    let data = match kind {
        TargetKind::Group => onebot_send_group_msg(&base_url, id, message, reply_to, token.as_deref())?,
        TargetKind::Private => onebot_send_private_msg(&base_url, id, message, reply_to, token.as_deref())?,
    };
    let _msg_id = data.get("data").and_then(|d| d.get("message_id")).and_then(|v| v.as_i64());
    let reply_note = reply_to.map(|mid| format!(" (reply to {})", mid)).unwrap_or_default();

    Ok(NodeResponse {
        ok: true,
        node_id: "qq_send".to_string(),
        messages: None,
        message: Some(format!("sent [{}]: {}{}", target, message, reply_note)),
        data: Some(data),
        error: None,
    })
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn handle(req: &NodeRequest) -> Result<NodeResponse, String> {
    match req.node_id.as_str() {
        "qq_serve" => handle_qq_serve(req),
        "qq_fetch_messages" => handle_qq_fetch_messages(),
        "qq_send" => handle_qq_send(req),
        "qq_get_group_members" => handle_qq_get_group_members(req),
        "qq_get_group_info" => handle_qq_get_group_info(req),
        // For qq_entry, delegate to legacy handler.
        "qq_entry" => {
            let legacy = QqRequest {
                action: req.action.clone().unwrap_or_default(),
                url: req.url.clone(),
                target: req.target.clone(),
                message: req.message.clone(),
                payload: req.payload.clone(),
            };
            match handle_legacy(legacy) {
                Ok(resp) => Ok(NodeResponse {
                    ok: resp.ok,
                    node_id: "qq_entry".to_string(),
                    messages: None,
                    message: resp.message,
                    data: resp.data,
                    error: None,
                }),
                Err(e) => Ok(NodeResponse {
                    ok: false,
                    node_id: "qq_entry".to_string(),
                    messages: None,
                    message: None,
                    data: None,
                    error: Some(e),
                }),
            }
        }
        other => Err(format!("unknown node_id: {other}")),
    }
}

// ---------------------------------------------------------------------------
// Plugin API exports
// ---------------------------------------------------------------------------

fn docs_value() -> cordis_plugin_sdk::PluginDocs {
    plugin_docs(
        "qq",
        "qq",
        "0.1.0",
        Some("Qq"),
        vec![
            node_doc(
                "qq_entry",
                "QQ adapter using the NoneBot (OneBot v11) protocol. Connect to a OneBot-compatible QQ client for sending messages. Actions: configure, send, status, call.",
                json!({
                    "type": "object", "required": ["action"],
                    "properties": {
                        "action": { "type": "string", "description": "configure | send | status | call" },
                        "url": { "type": "string", "description": "OneBot HTTP API base URL (for configure)" },
                        "target": { "type": "string", "description": "group:<id> or private:<id>" },
                        "message": { "type": "string", "description": "Message text (for send)" },
                        "payload": { "type": "object", "description": "Raw API call payload (for call)" }
                    }
                }),
                json!({
                    "type": "object",
                    "properties": {
                        "ok": { "type": "boolean" },
                        "action": { "type": "string" },
                        "message": { "type": ["string", "null"] },
                        "data": {}
                    }
                }),
                &["requires OneBot HTTP server running"],
                &["no OneBot URL configured", "invalid target format", "message is empty", "OneBot API error", "unsupported action"],
    ).with_agent_accessible(),
            task_node_doc(
                "qq_serve",
                "Start an HTTP server to receive OneBot v11 message events. Configure your OneBot client to POST events to http://<host>:<port>/onebot/event. Supports grayscale group whitelist.",
                json!({
                    "type": "object", "required": ["node_id"],
                    "properties": {
                        "node_id": { "type": "string", "const": "qq_serve" },
                        "payload": {
                            "type": "object",
                            "properties": {
                                "port": { "type": "integer", "description": "HTTP listen port (default 8080)" },
                                "onebot_url": { "type": "string", "description": "OneBot HTTP API URL" },
                                "allow_groups": { "type": "array", "items": { "type": "string" }, "description": "Group whitelist for grayscale testing" }
                            }
                        },
                        "agent_session_id": { "type": "string", "description": "Optional: agent session ID for message routing" }
                    }
                }),
                json!({
                    "type": "object",
                    "properties": {
                        "ok": { "type": "boolean" },
                        "message": { "type": ["string", "null"] },
                        "error": { "type": ["string", "null"] }
                    }
                }),
                &["starts an HTTP server thread", "listens on configured port"],
                &["port already in use", "OneBot client not configured to POST events"],
            ).with_agent_accessible(),
            node_doc(
                "qq_fetch_messages",
                "Fetch queued incoming QQ messages received by qq_serve. Returns all messages and drains the queue. Agent should poll this periodically.",
                json!({
                    "type": "object", "required": ["node_id"],
                    "properties": {
                        "node_id": { "type": "string", "const": "qq_fetch_messages" }
                    }
                }),
                json!({
                    "type": "object",
                    "properties": {
                        "ok": { "type": "boolean" },
                        "messages": { "type": "array", "items": { "type": "object" } },
                        "error": { "type": ["string", "null"] }
                    }
                }),
                &["drains the message queue"],
                &[],
            ),
            node_doc(
                "qq_send",
                "Send a message to a QQ group or private chat via OneBot v11 HTTP API. Supports reply/quote via reply_to.",
                json!({
                    "type": "object", "required": ["node_id", "target", "message"],
                    "properties": {
                        "node_id": { "type": "string", "const": "qq_send" },
                        "target": { "type": "string", "description": "group:<id> or private:<id>" },
                        "message": { "type": "string", "description": "Message text to send" },
                        "reply_to": { "type": "integer", "description": "Optional message_id to reply/quote" }
                    }
                }),
                json!({
                    "type": "object",
                    "properties": {
                        "ok": { "type": "boolean" },
                        "message": { "type": ["string", "null"] },
                        "error": { "type": ["string", "null"] }
                    }
                }),
                &["sends HTTP request to OneBot API"],
                &["no OneBot URL configured", "invalid target format", "message is empty"],
    ).with_agent_accessible(),
            node_doc(
                "qq_get_group_info",
                "Get group information (name, member count, etc.) via OneBot v11 get_group_info API.",
                json!({
                    "type": "object", "required": ["node_id", "target"],
                    "properties": {
                        "node_id": { "type": "string", "const": "qq_get_group_info" },
                        "target": { "type": "string", "description": "group:<id>" }
                    }
                }),
                json!({
                    "type": "object",
                    "properties": {
                        "ok": { "type": "boolean" },
                        "message": { "type": ["string", "null"] },
                        "data": { "type": "object", "description": "Raw OneBot get_group_info response" },
                        "error": { "type": ["string", "null"] }
                    }
                }),
                &["calls OneBot get_group_info API"],
                &["no OneBot URL configured", "invalid target format", "group not found"],
            ).with_agent_accessible(),
        ],
    Some("\
QQ GROUP CHAT MODE — you are running in a QQ group. Messages may be casual chat NOT directed at you.\n\
CRITICAL: Always decide whether the message is actually talking to YOU before responding.\n\
A message IS directed at you if: mentions \"机器人\", \"bot\", \"Cordis\"; asks a direct question; gives a command; has question words (how, why, what, 怎么, 为什么, 如何, 帮我).\n\
A message is NOT directed at you if: casual chat between members; emoji/sticker/single-word; talking about someone else; statements not asking for anything.\n\
If NOT directed at you: use {\"action\":\"suspend\"}.\n\
If directed at you: use {\"action\":\"respond\",\"message\":\"your reply here\"}.\n\
\n\
To send a progress update or proactive message to the group you're talking to:\n\
  invoke_plugin(qq, qq_send, {\"node_id\":\"qq_send\",\"target\":\"group:<group_id>\",\"message\":\"<your message>\"})
Replace <group_id> with the actual group ID from the incoming message.
Send a brief progress message BEFORE any tool that may take more than a moment (build, test, search, etc.), and a follow-up when the tool completes.

To query group members: invoke_plugin(qq, qq_get_group_members, {\"node_id\":\"qq_get_group_members\",\"target\":\"group:<id>\"})\n\
To proactively send to a group: invoke_plugin(qq, qq_send, {\"node_id\":\"qq_send\",\"target\":\"group:<id>\",\"message\":\"<text>\"})")
    )
}

fn abi_fingerprint_value() -> AbiFingerprint {
    AbiFingerprint {
        rustc_version: "1.85.1".to_string(),
        target_triple: "x86_64-unknown-linux-gnu".to_string(),
        crate_hash: "crate_qq_v1".to_string(),
        api_hash: "api_v2".to_string(),
    }
}

fn api_handle(req: PluginRequest) -> PluginResponse {
    match serde_json::from_str::<NodeRequest>(&req.payload)
        .map_err(|e| format!("qq plugin: {e}"))
        .and_then(|r| handle(&r))
    {
        Ok(resp) => json_response(&resp),
        Err(e) => json_response(&NodeResponse {
            ok: false,
            node_id: "error".to_string(),
            messages: None,
            message: None,
            data: None,
            error: Some(e),
        }),
    }
}

export_plugin_api! {
    abi_fingerprint = abi_fingerprint_value(),
    docs = docs_value(),
    handle = api_handle,
}
