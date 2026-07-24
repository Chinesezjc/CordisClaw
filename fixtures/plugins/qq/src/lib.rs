//! QQ adapter plugin using the NoneBot (OneBot v11) protocol.
//!
//! This plugin communicates with a OneBot-compatible QQ client
//! (e.g. go-cqhttp, NapCat, LLOneBot) via its HTTP API.
//!
//! Nodes:
//! - `qq_entry`         — original multi-action entry (configure/send/status/call/block/unblock)
//! - `qq_serve`         — Task node: starts HTTP server to receive OneBot events
//! - `qq_fetch_messages` — return queued incoming messages (agent polls this)
//! - `qq_send`          — send a message to a group or private chat

use cordis_plugin_sdk::{
    export_plugin_api, json_response, node_doc, plugin_docs, task_node_doc, AbiFingerprint,
    PluginRequest, PluginResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;
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

/// ⚠ CRITICAL — incoming message queue. Populated by handle_onebot_event,
/// drained by start_agent_poller → agent_trigger → inbox loop.
static MESSAGE_QUEUE: Mutex<VecDeque<IncomingMessage>> = Mutex::new(VecDeque::new());

/// Server running flag.
static SERVER_RUNNING: Mutex<bool> = Mutex::new(false);

static SERVER_SHUTDOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static EVENT_LOOP_HANDLE: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);

/// WebSocket reverse-connection server state (Napcat connects here as a WS
/// client). Kept separate from the HTTP server's SERVER_SHUTDOWN /
/// EVENT_LOOP_HANDLE / SERVER_RUNNING so stopping one server never trips the
/// other's accept loop.
static WS_SERVER_SHUTDOWN: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
static WS_SERVER_HANDLE: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);
/// WebSocket server running flag — makes repeated qq_ws_serve start idempotent.
static WS_SERVER_RUNNING: Mutex<bool> = Mutex::new(false);

/// Stored agent session ID for message routing.
static AGENT_SESSION_ID: Mutex<Option<String>> = Mutex::new(None);

/// Dedup: recently processed message IDs (prevents double-processing from zombie pollers / duplicate events).
/// P1-38: bounded FIFO dedup for message ids.  Was `HashSet<String>` whose
/// iteration order is unspecified — "drain the oldest" was actually
/// "drain 100 arbitrary entries", so the true-oldest ids could linger
/// while recent ids got dropped. `VecDeque` gives us proper FIFO
/// eviction; contains() is O(n) but n is capped at 200.
static RECENT_MESSAGE_IDS: std::sync::LazyLock<Mutex<VecDeque<String>>> =
    std::sync::LazyLock::new(|| Mutex::new(VecDeque::new()));

/// P1-39: counter for silently-dropped messages when MESSAGE_QUEUE is
/// full. Read + reset by qq_fetch_messages so the agent can see how
/// many events it missed since the last poll.
static MESSAGE_QUEUE_DROPPED: AtomicU64 = AtomicU64::new(0);

/// Guards against spawning more than one agent poller thread. Both
/// qq_serve and qq_ws_serve start the poller; without this guard a
/// process running both servers (or a repeated start) would spawn
/// duplicate pollers all draining the same MESSAGE_QUEUE.
static POLLER_STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

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

/// reply_to arrives as i64 from legacy callers and as a string from the
/// runtime inbox (envelope reply_to is stringly-typed). Accept both;
/// unparseable strings degrade to None (send without quote) rather than
/// failing the whole request.
fn de_reply_to<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<i64>, D::Error> {
    let v = Option::<Value>::deserialize(d)?;
    Ok(match v {
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
        _ => None,
    })
}

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
    /// Accepts both i64 (legacy direct invoke) and string (runtime inbox
    /// reply routing serialises envelope reply_to as a string).
    #[serde(default, deserialize_with = "de_reply_to")]
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

fn onebot_call(
    base_url: &str,
    endpoint: &str,
    params: &Value,
    token: Option<&str>,
) -> Result<Value, String> {
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
        let retcode = parsed.get("retcode").and_then(|v| v.as_i64()).unwrap_or(-1);
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
    let (kind_str, id_str) = raw.split_once(':').ok_or_else(|| {
        format!("invalid target '{raw}': expected 'group:<id>' or 'private:<id>'")
    })?;
    let id: i64 = id_str
        .trim()
        .parse()
        .map_err(|e| format!("invalid target id '{id_str}': {e}"))?;
    let kind = match kind_str.trim().to_lowercase().as_str() {
        "group" | "g" => TargetKind::Group,
        "private" | "priv" | "p" | "user" | "u" => TargetKind::Private,
        other => {
            return Err(format!(
                "unknown target kind '{other}'; use 'group' or 'private'"
            ))
        }
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
    if let Some(url) = req.url {
        state.onebot_url = Some(url);
    }
    if let Some(target) = req.target {
        parse_target(&target)?;
        state.default_target = Some(target);
    }
    // Parse allow_groups from payload.
    if let Some(ref payload) = req.payload {
        if let Some(arr) = payload.get("allow_groups").and_then(|v| v.as_array()) {
            state.allow_groups = arr
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
        }
        if let Some(arr) = payload.get("block_groups").and_then(|v| v.as_array()) {
            state.block_groups = arr
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
        }
    }
    Ok(QqResponse {
        ok: true,
        action: "configure".to_string(),
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
    let target = req
        .target
        .as_deref()
        .ok_or("block requires 'target' field (group:<id>)")?;
    let (_kind, id) = parse_target(target)?;
    let gid = id.to_string();
    let mut state = STATE.lock().map_err(|e| format!("lock: {e}"))?;
    if !state.block_groups.contains(&gid) {
        state.block_groups.push(gid.clone());
    }
    // Persist to runtime config.
    if let Some(mut config) = load_runtime_config() {
        config["block_groups"] = json!(state.block_groups);
        save_runtime_config(&config);
    }
    Ok(QqResponse {
        ok: true,
        action: "block".to_string(),
        message: Some(format!(
            "group {} blocked. block_groups={:?}",
            gid, state.block_groups
        )),
        data: None,
    })
}

fn handle_unblock(req: QqRequest) -> Result<QqResponse, String> {
    let target = req
        .target
        .as_deref()
        .ok_or("unblock requires 'target' field (group:<id>)")?;
    let (_kind, id) = parse_target(target)?;
    let gid = id.to_string();
    let mut state = STATE.lock().map_err(|e| format!("lock: {e}"))?;
    state.block_groups.retain(|g| g != &gid);
    // Persist to runtime config.
    if let Some(mut config) = load_runtime_config() {
        config["block_groups"] = json!(state.block_groups);
        save_runtime_config(&config);
    }
    Ok(QqResponse {
        ok: true,
        action: "unblock".to_string(),
        message: Some(format!(
            "group {} unblocked. block_groups={:?}",
            gid, state.block_groups
        )),
        data: None,
    })
}

fn handle_allow_group(req: QqRequest) -> Result<QqResponse, String> {
    let target = req
        .target
        .as_deref()
        .ok_or("allow_group requires 'target' field (group:<id>)")?;
    let (_kind, id) = parse_target(target)?;
    let gid = id.to_string();
    let mut state = STATE.lock().map_err(|e| format!("lock: {e}"))?;
    if !state.allow_groups.contains(&gid) {
        state.allow_groups.push(gid.clone());
    }
    Ok(QqResponse {
        ok: true,
        action: "allow_group".to_string(),
        message: Some(format!(
            "group {} added to allow list. allow_groups={:?}",
            gid, state.allow_groups
        )),
        data: None,
    })
}

fn handle_disallow_group(req: QqRequest) -> Result<QqResponse, String> {
    let target = req
        .target
        .as_deref()
        .ok_or("disallow_group requires 'target' field (group:<id>)")?;
    let (_kind, id) = parse_target(target)?;
    let gid = id.to_string();
    let mut state = STATE.lock().map_err(|e| format!("lock: {e}"))?;
    state.allow_groups.retain(|g| g != &gid);
    Ok(QqResponse {
        ok: true,
        action: "disallow_group".to_string(),
        message: Some(format!(
            "group {} removed from allow list. allow_groups={:?}",
            gid, state.allow_groups
        )),
        data: None,
    })
}

fn handle_list_groups() -> Result<QqResponse, String> {
    let state = STATE.lock().map_err(|e| format!("lock: {e}"))?;
    Ok(QqResponse {
        ok: true,
        action: "list_groups".to_string(),
        message: Some(format!(
            "allow_groups={:?} block_groups={:?}",
            state.allow_groups, state.block_groups,
        )),
        data: None,
    })
}

fn handle_send(req: QqRequest) -> Result<QqResponse, String> {
    let message = req.message.as_deref().unwrap_or("").trim().to_string();
    if message.is_empty() {
        return Err("message is empty".to_string());
    }
    let (kind, id) = {
        let state = STATE.lock().map_err(|e| format!("lock: {e}"))?;
        let target_str = req
            .target
            .as_deref()
            .or(state.default_target.as_deref())
            .ok_or("no target configured; use 'configure' first or provide a 'target' field")?;
        parse_target(target_str)?
    };
    let base_url = {
        let state = STATE.lock().map_err(|e| format!("lock: {e}"))?;
        state
            .onebot_url
            .clone()
            .ok_or("no OneBot URL configured; use 'configure' first")?
    };
    let data = match kind {
        TargetKind::Group => onebot_send_group_msg(&base_url, id, &message, None, None)?,
        TargetKind::Private => onebot_send_private_msg(&base_url, id, &message, None, None)?,
    };
    let msg_id = data
        .get("data")
        .and_then(|d| d.get("message_id"))
        .and_then(|v| v.as_i64());
    Ok(QqResponse {
        ok: true,
        action: "send".to_string(),
        message: msg_id.map(|mid| format!("message_id={mid}")),
        data: Some(data),
    })
}

fn handle_status() -> Result<QqResponse, String> {
    let state = STATE.lock().map_err(|e| format!("lock: {e}"))?;
    let connected = if let Some(ref url) = state.onebot_url {
        onebot_call(url, "get_status", &json!({}), None).is_ok()
    } else {
        false
    };
    Ok(QqResponse {
        ok: true,
        action: "status".to_string(),
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
    let endpoint = payload
        .get("endpoint")
        .and_then(|v| v.as_str())
        .ok_or("payload must contain 'endpoint' string")?;
    let params = payload.get("params").cloned().unwrap_or(json!({}));
    let base_url = {
        let state = STATE.lock().map_err(|e| format!("lock: {e}"))?;
        state
            .onebot_url
            .clone()
            .ok_or("no OneBot URL configured; use 'configure' first")?
    };
    let data = onebot_call(&base_url, endpoint, &params, None)?;
    Ok(QqResponse {
        ok: true,
        action: "call".to_string(),
        message: None,
        data: Some(data),
    })
}

// ---------------------------------------------------------------------------
// HTTP Server — receives OneBot event POSTs
// ---------------------------------------------------------------------------

fn stop_qq_serve() {
    SERVER_SHUTDOWN.store(true, Ordering::SeqCst);
    if let Ok(mut guard) = EVENT_LOOP_HANDLE.lock() {
        if let Some(handle) = guard.take() {
            let _ = handle.join();
        }
    }
    SERVER_SHUTDOWN.store(false, Ordering::SeqCst);
}

fn run_event_loop(server: tiny_http::Server) {
    loop {
        if SERVER_SHUTDOWN.load(Ordering::SeqCst) {
            break;
        }
        match server.recv_timeout(Duration::from_millis(500)) {
            Ok(Some(mut request)) => {
                if request.url() == "/onebot/event" && request.method() == &tiny_http::Method::Post
                {
                    // P0-23: OneBot v11 signs the request body with HMAC-SHA1
                    // keyed by `access_token`, and passes it in the
                    // `X-Signature: sha1=<hex>` header. If a token is
                    // configured we MUST verify — otherwise anyone who can
                    // reach the port could push arbitrary "user messages"
                    // into the agent.  If no token is configured, log a
                    // warning and accept (backwards-compat with dev setups);
                    // production deployments should always set access_token.
                    let signature_header = request
                        .headers()
                        .iter()
                        .find(|h| h.field.equiv("X-Signature"))
                        .map(|h| h.value.as_str().to_string());
                    let mut body = String::new();
                    if request.as_reader().read_to_string(&mut body).is_ok() {
                        let expected_token = STATE
                            .lock()
                            .ok()
                            .and_then(|s| s.access_token.clone())
                            .or_else(|| {
                                load_runtime_config()
                                    .and_then(|c| c.get("access_token")?.as_str().map(String::from))
                            });
                        if let Some(token) = expected_token.as_deref() {
                            let ok = signature_header
                                .as_deref()
                                .map(|s| verify_onebot_signature(token, body.as_bytes(), s))
                                .unwrap_or(false);
                            if !ok {
                                eprintln!("[qq] webhook signature check failed");
                                let _ = request.respond(
                                    tiny_http::Response::from_string("bad signature")
                                        .with_status_code(401),
                                );
                                continue;
                            }
                        } else {
                            eprintln!(
                                "[qq] warning: /onebot/event received with no access_token \
                                 configured; accepting without signature check"
                            );
                        }
                        let _ = request.respond(tiny_http::Response::from_string(
                            serde_json::to_string(&json!({"status":"ok"})).unwrap_or_default(),
                        ));
                        if let Ok(event) = serde_json::from_str::<OneBotEvent>(&body) {
                            handle_onebot_event(&event);
                        }
                    } else {
                        let _ = request.respond(
                            tiny_http::Response::from_string("bad request").with_status_code(400),
                        );
                    }
                } else if request.url() == "/health" {
                    let _ = request.respond(tiny_http::Response::from_string(
                        // P2-35: unwrap_or_default keeps the response body
                        // empty on a JSON-encode failure rather than panic
                        // the whole event loop.
                        serde_json::to_string(&json!({"status":"ok"})).unwrap_or_default(),
                    ));
                } else {
                    let _ = request.respond(
                        tiny_http::Response::from_string("not found").with_status_code(404),
                    );
                }
            }
            Ok(None) => {}
            Err(e) => {
                eprintln!("[qq] server recv error: {e}");
                break;
            }
        }
    }
}
// P1-40 had removed the WebSocket reverse-connection code as unwired dead
// code. It is restored below and wired through the node dispatch, because
// production Napcat connects over a WS port rather than posting HTTP webhooks.

// ---------------------------------------------------------------------------
// WebSocket Server — receives OneBot events via WS reverse connection
// ---------------------------------------------------------------------------

/// Accept loop for the OneBot v11 reverse WebSocket connection. Napcat (or
/// any OneBot client) connects here as a WS client and streams event frames.
/// Non-blocking accept + WS_SERVER_SHUTDOWN check so a stop request can break
/// the loop and let the thread exit (required before the .so is dlclose'd on
/// reload — a thread still executing unloaded code causes SIGILL).
fn start_ws_server(listener: std::net::TcpListener, port: u16) -> Result<(), String> {
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("ws: set_nonblocking: {e}"))?;
    eprintln!("[qq] WebSocket server listening on port {port}");
    loop {
        if WS_SERVER_SHUTDOWN.load(Ordering::SeqCst) {
            eprintln!("[qq] WebSocket server shutting down");
            break;
        }
        match listener.accept() {
            Ok((stream, _addr)) => {
                // 5s read timeout so active connections drain promptly on
                // shutdown instead of blocking the thread indefinitely.
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let ws = match tungstenite::accept(stream) {
                    Ok(w) => w,
                    Err(e) => {
                        eprintln!("[qq] ws handshake: {e}");
                        continue;
                    }
                };
                eprintln!("[qq] WebSocket client connected");
                handle_ws_connection(ws);
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(500));
                continue;
            }
            Err(e) => {
                eprintln!("[qq] ws accept: {e}");
                continue;
            }
        }
    }
    Ok(())
}

/// Read frames from one WS client. Text frames are parsed as OneBot events
/// and fed into the shared inbound path (handle_onebot_event), so WS events
/// go through the same dedup / whitelist / envelope machinery as HTTP.
fn handle_ws_connection(mut ws: tungstenite::WebSocket<std::net::TcpStream>) {
    loop {
        if WS_SERVER_SHUTDOWN.load(Ordering::SeqCst) {
            let _ = ws.close(None);
            break;
        }
        match ws.read() {
            Ok(tungstenite::Message::Text(text)) => {
                if let Ok(event) = serde_json::from_str::<OneBotEvent>(&text) {
                    handle_onebot_event(&event);
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
                // A read timeout (WouldBlock) is expected every 5s; keep the
                // loop alive so the shutdown check runs, only bail on real
                // errors.
                if let tungstenite::Error::Io(ref io) = e {
                    if io.kind() == std::io::ErrorKind::WouldBlock
                        || io.kind() == std::io::ErrorKind::TimedOut
                    {
                        continue;
                    }
                }
                eprintln!("[qq] ws read error: {e}");
                break;
            }
            _ => {}
        }
    }
}

/// Stop the WebSocket server: signal shutdown, join the accept thread (so no
/// code from this .so is still running), then clear the flag and running
/// state so a later start can rebind the port cleanly.
fn stop_qq_ws_serve() {
    WS_SERVER_SHUTDOWN.store(true, Ordering::SeqCst);
    if let Ok(mut guard) = WS_SERVER_HANDLE.lock() {
        if let Some(handle) = guard.take() {
            let _ = handle.join();
        }
    }
    WS_SERVER_SHUTDOWN.store(false, Ordering::SeqCst);
    if let Ok(mut running) = WS_SERVER_RUNNING.lock() {
        *running = false;
    }
}

fn handle_onebot_event(event: &OneBotEvent) {
    if event.post_type != "message" {
        return;
    }

    // Extract message text and reply context.
    let (message_text, reply_to_msg_id) =
        extract_message_info(&event.message, event.raw_message.as_deref());

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
        let block = STATE
            .lock()
            .ok()
            .map(|s| s.block_groups.clone())
            .unwrap_or_default();
        if block.contains(&gid_str) {
            return;
        }

        // Grayscale whitelist check.
        let allow = STATE
            .lock()
            .ok()
            .map(|s| s.allow_groups.clone())
            .unwrap_or_default();
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
        if seen.iter().any(|k| k == &dedup_key) {
            return; // duplicate OneBot event
        }
        seen.push_back(dedup_key);
        // FIFO eviction (P1-38).
        while seen.len() > 200 {
            seen.pop_front();
        }
    }

    if let Ok(mut queue) = MESSAGE_QUEUE.lock() {
        if queue.len() < 128 {
            queue.push_back(msg);
        } else {
            // P1-39: don't drop silently — surface the count.
            let dropped = MESSAGE_QUEUE_DROPPED.fetch_add(1, Ordering::Relaxed) + 1;
            if dropped.is_power_of_two() {
                eprintln!("[qq] MESSAGE_QUEUE full (>=128); dropped total = {dropped}");
            }
        }
    }
}

/// Parse a CQ code string (e.g. "[CQ:at,qq=123456,name=bot]") into a human-readable form.
/// Returns an iterator of (part_type, text) where part_type is "text", "at", "image", etc.
fn parse_cq_codes(raw: &str) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut remaining = raw;
    while let Some(start) = remaining.find("[CQ:") {
        // Push text before the CQ code.
        if start > 0 {
            parts.push(remaining[..start].to_string());
        }
        // Find the closing bracket.
        if let Some(end) = remaining[start..].find(']') {
            let cq = &remaining[start + 1..start + end]; // inside brackets, e.g. "CQ:at,qq=123"
            let cq_body = cq.strip_prefix("CQ:").unwrap_or(cq);
            let (cq_type, cq_data) = cq_body.split_once(',').unwrap_or((cq_body, ""));

            match cq_type {
                "at" => {
                    let qq = cq_data
                        .split(',')
                        .find(|kv| kv.starts_with("qq="))
                        .and_then(|kv| kv.strip_prefix("qq="))
                        .unwrap_or("unknown");
                    let name = cq_data
                        .split(',')
                        .find(|kv| kv.starts_with("name="))
                        .and_then(|kv| kv.strip_prefix("name="))
                        .unwrap_or(qq);
                    parts.push(format!("@[id={},name={}]", qq, name));
                }
                "image" => {
                    let url = cq_data
                        .split(',')
                        .find(|kv| kv.starts_with("url="))
                        .and_then(|kv| kv.strip_prefix("url="))
                        .unwrap_or("");
                    let file = cq_data
                        .split(',')
                        .find(|kv| kv.starts_with("file="))
                        .and_then(|kv| kv.strip_prefix("file="))
                        .unwrap_or("");
                    if !url.is_empty() {
                        parts.push(format!("[image: {}]", url));
                    } else if !file.is_empty() {
                        parts.push(format!("[image file: {}]", file));
                    } else {
                        parts.push("[image]".to_string());
                    }
                }
                "reply" => {
                    // reply CQ codes carry message_id; we can't extract reply_to here
                    // (caller already handles reply segments). Skip silently.
                }
                "face" | "sticker" => {
                    parts.push("[sticker]".to_string());
                }
                "record" | "audio" => {
                    parts.push("[audio]".to_string());
                }
                "video" => {
                    parts.push("[video]".to_string());
                }
                "file" => {
                    parts.push("[file]".to_string());
                }
                _ => {
                    // Unknown CQ code — keep as-is for debugging.
                    parts.push(format!("[{cq_type}]"));
                }
            }
            remaining = &remaining[start + end + 1..]; // skip past ']'
        } else {
            // No closing bracket found; push the rest as-is.
            parts.push(remaining.to_string());
            remaining = "";
            break;
        }
    }
    if !remaining.is_empty() {
        parts.push(remaining.to_string());
    }
    parts
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
                    if let Some(id_val) = seg.get("data").and_then(|d| d.get("id")) {
                        reply_to = extract_i64(id_val);
                        parts.push(format!("[reply to msg_id={}]", reply_to.unwrap_or(0)));
                    }
                }
                "text" => {
                    if let Some(t) = seg
                        .get("data")
                        .and_then(|d| d.get("text"))
                        .and_then(|t| t.as_str())
                    {
                        parts.push(t.to_string());
                    }
                }
                "image" => {
                    if let Some(url) = seg
                        .get("data")
                        .and_then(|d| d.get("url"))
                        .and_then(|u| u.as_str())
                    {
                        parts.push(format!("[image: {url}]"));
                    } else if let Some(file) = seg
                        .get("data")
                        .and_then(|d| d.get("file"))
                        .and_then(|f| f.as_str())
                    {
                        parts.push(format!("[image file: {file}]"));
                    }
                }
                "json" => {
                    if let Some(data) = seg
                        .get("data")
                        .and_then(|d| d.get("data"))
                        .and_then(|d| d.as_str())
                    {
                        parts.push(format!("[json: {data}]"));
                    }
                }
                "forward" => {
                    if let Some(id) = seg
                        .get("data")
                        .and_then(|d| d.get("id"))
                        .and_then(|d| d.as_str())
                    {
                        parts.push(format!("[chat history: id={id}]"));
                    }
                }
                "at" => {
                    let qq = seg
                        .get("data")
                        .and_then(|d| d.get("qq"))
                        .and_then(|q| q.as_str())
                        .unwrap_or("unknown");
                    let name = seg
                        .get("data")
                        .and_then(|d| d.get("name"))
                        .and_then(|n| n.as_str())
                        .unwrap_or(qq);
                    parts.push(format!("@[id={},name={}]", qq, name));
                }
                "face" | "sticker" => {
                    parts.push("[sticker]".to_string());
                }
                _ => {}
            }
        }
    } else if let Some(raw) = raw_message {
        if !raw.is_empty() {
            // Parse CQ codes in raw_message to get human-readable text.
            parts.extend(parse_cq_codes(raw));
        }
    }
    if parts.is_empty() {
        if let Value::String(s) = message {
            // Parse CQ codes in plain string message.
            parts.extend(parse_cq_codes(s));
        }
    }

    (parts.join(""), reply_to)
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

/// `/`-prefixed commands ARE forwarded (N批): the runtime's command
/// router executes them without the LLM (usable during model outages).
fn should_process(text: &str) -> bool {
    if text.starts_with('/') {
        return text.len() > 1;
    }
    text.len() > 2
}

/// Build the runtime routing envelope (mirrors feishu's build_envelope).
/// sender_id is "group:<gid>" or "private:<uid>"; the actual person is
/// user_id, so identity is "qq:<user_id>".
fn build_envelope(msg: &IncomingMessage) -> String {
    let reply_target = format!("{}:{}", msg.message_type, msg.sender_id);
    let session_key = format!("qq:{reply_target}");
    let display = format!(
        "[QQ {} from {} (user {})]: {}",
        msg.message_type, msg.sender_id, msg.user_id, msg.message
    );
    let mut env = json!({
        "source_plugin": "qq",
        "reply_node": "qq_send",
        "session_key": session_key,
        "display": display,
        "reply_target": reply_target,
        "sender_id": format!("qq:{}", msg.user_id),
        "conversation_kind": msg.message_type,
    });
    if let Some(mid) = msg.message_id {
        env["reply_to"] = json!(mid.to_string());
    }
    env.to_string()
}

// ═══════════════════════════════════════════════════════════════════════
// ⚠ CRITICAL — message pump.  Do NOT delete or refactor away.
// The runtime inbox loop depends on this polling thread to drain
// MESSAGE_QUEUE and push messages via agent_trigger().
//
// Must be started inside handle_qq_serve after SERVER_RUNNING flips true.
// If this function is missing, QQ messages will silently accumulate in
// MESSAGE_QUEUE and never reach the agent.
// ═══════════════════════════════════════════════════════════════════════
fn start_agent_poller() {
    // Idempotent: only the first caller spawns the poller thread. qq_serve
    // and qq_ws_serve both call this; a second poller would double-drain
    // MESSAGE_QUEUE and double-trigger the agent.
    if POLLER_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    thread::spawn(move || {
        thread::sleep(std::time::Duration::from_secs(2));
        loop {
            let msgs: Vec<IncomingMessage> = {
                let mut queue = MESSAGE_QUEUE.lock().unwrap_or_else(|p| p.into_inner());
                queue.drain(..).collect()
            };
            for msg in msgs {
                if !should_process(&msg.message) {
                    continue;
                }
                cordis_plugin_sdk::agent_trigger(&build_envelope(&msg));
            }
            thread::sleep(std::time::Duration::from_secs(5));
        }
    });
}

fn handle_qq_serve(req: &NodeRequest) -> Result<NodeResponse, String> {
    if req.action.as_deref() == Some("stop") {
        stop_qq_serve();
        *SERVER_RUNNING.lock().map_err(|e| format!("lock: {e}"))? = false;
        return Ok(NodeResponse {
            ok: true,
            node_id: "qq_serve".to_string(),
            message: Some("stopped".to_string()),
            messages: None,
            data: None,
            error: None,
        });
    }
    let port: u16 = req
        .payload
        .as_ref()
        .and_then(|p| p.get("port"))
        .and_then(|v| v.as_u64())
        .unwrap_or(8080) as u16;

    let allow_groups: Vec<String> = req
        .payload
        .as_ref()
        .and_then(|p| p.get("allow_groups"))
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let block_groups: Vec<String> = req
        .payload
        .as_ref()
        .and_then(|p| p.get("block_groups"))
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    // Store configuration. Merge with persisted values from runtime config.
    {
        let mut state = STATE.lock().map_err(|e| format!("lock: {e}"))?;
        // Load persisted block_groups from runtime config as a base.
        let persisted_block: Vec<String> = load_runtime_config()
            .and_then(|c| c.get("block_groups")?.as_array().cloned())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        state.allow_groups = allow_groups.clone();
        state.block_groups = if block_groups.is_empty() {
            persisted_block
        } else {
            block_groups.clone()
        };
        if let Some(url) = req
            .payload
            .as_ref()
            .and_then(|p| p.get("onebot_url"))
            .and_then(|v| v.as_str())
        {
            state.onebot_url = Some(url.to_string());
        }
        if let Some(t) = req
            .payload
            .as_ref()
            .and_then(|p| p.get("access_token"))
            .and_then(|v| v.as_str())
        {
            state.access_token = Some(t.to_string());
        }
        if let Some(u) = req
            .payload
            .as_ref()
            .and_then(|p| p.get("llm_api_url"))
            .and_then(|v| v.as_str())
        {
            state.llm_api_url = Some(u.to_string());
        }
        if let Some(k) = req
            .payload
            .as_ref()
            .and_then(|p| p.get("llm_api_key"))
            .and_then(|v| v.as_str())
        {
            state.llm_api_key = Some(k.to_string());
        }
        if let Some(m) = req
            .payload
            .as_ref()
            .and_then(|p| p.get("llm_model"))
            .and_then(|v| v.as_str())
        {
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
            let handle = thread::spawn(move || run_event_loop(server));
            if let Ok(mut guard) = EVENT_LOOP_HANDLE.lock() {
                *guard = Some(handle);
            }
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
            None => format!("hash:{},{},{}", msg.sender_id, msg.user_id, msg.message),
        };
        let mut seen = RECENT_MESSAGE_IDS.lock().unwrap_or_else(|p| p.into_inner());
        if seen.iter().any(|k| k == &dedup_key) {
            continue; // already processed
        }
        seen.push_back(dedup_key);
        while seen.len() > 200 {
            seen.pop_front();
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

/// P2-12: resolve the QQ runtime-config path relative to
/// `$CORDIS_FIXTURES_ROOT` (falling back to the historical hard-coded
/// `/root/CordisClaw/fixtures/...` when the env is missing). The
/// filesystem plugin already reads this env var; this makes QQ portable
/// across environments and testing setups.
fn runtime_config_path() -> std::path::PathBuf {
    match std::env::var("CORDIS_FIXTURES_ROOT") {
        Ok(root) => std::path::PathBuf::from(root).join(".cordis-drafts/qq_runtime_config.json"),
        Err(_) => std::path::PathBuf::from(
            "/root/CordisClaw/fixtures/.cordis-drafts/qq_runtime_config.json",
        ),
    }
}

fn load_runtime_config() -> Option<serde_json::Value> {
    let path = runtime_config_path();
    let data = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&data).ok()
}

fn save_runtime_config(config: &serde_json::Value) {
    let path = runtime_config_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // P0-24: this file contains the QQ access_token. Write it with 0o600 on
    // Unix so other users on the box can't read the token. Use OpenOptions
    // with a mode so the perms are set at create time (chmod after write
    // leaves a race window during which the file is world-readable).
    let bytes = serde_json::to_string_pretty(config).unwrap_or_default();
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        match std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(mut f) => {
                let _ = f.write_all(bytes.as_bytes());
            }
            Err(_) => {
                let _ = std::fs::write(&path, &bytes);
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = std::fs::write(&path, &bytes);
    }
}

/// P0-23: verify OneBot v11 `X-Signature: sha1=<hex>` against `body` using
/// `access_token` as the HMAC-SHA1 key. Comparison is constant-time via
/// `subtle::ConstantTimeEq` to avoid timing side channels.
fn verify_onebot_signature(access_token: &str, body: &[u8], header_value: &str) -> bool {
    use hmac::{Hmac, Mac};
    use sha1::Sha1;
    use subtle::ConstantTimeEq;
    let hex_from_header = header_value
        .trim()
        .strip_prefix("sha1=")
        .unwrap_or(header_value.trim());
    let expected_bytes = match hex::decode(hex_from_header) {
        Ok(b) => b,
        Err(_) => return false,
    };
    let mut mac = match Hmac::<Sha1>::new_from_slice(access_token.as_bytes()) {
        Ok(m) => m,
        Err(_) => return false,
    };
    mac.update(body);
    let computed = mac.finalize().into_bytes();
    if computed.len() != expected_bytes.len() {
        return false;
    }
    computed.ct_eq(&expected_bytes).into()
}

fn handle_qq_get_group_members(req: &NodeRequest) -> Result<NodeResponse, String> {
    let target = req.target.as_deref().unwrap_or("").trim();
    if target.is_empty() {
        return Err("target is required for qq_get_group_members (group:<id>)".to_string());
    }
    let (kind, id) = parse_target(target)?;
    if kind != TargetKind::Group {
        return Err("target must be group:<id> for qq_get_group_members".to_string());
    }

    let base_url = req
        .payload
        .as_ref()
        .and_then(|p| p.get("onebot_url"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| STATE.lock().ok().and_then(|s| s.onebot_url.clone()))
        .or_else(|| {
            load_runtime_config().and_then(|c| c.get("onebot_url")?.as_str().map(|s| s.to_string()))
        })
        .ok_or("no OneBot URL configured")?;

    let token = req
        .payload
        .as_ref()
        .and_then(|p| p.get("access_token"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| STATE.lock().ok().and_then(|s| s.access_token.clone()))
        .or_else(|| {
            load_runtime_config()
                .and_then(|c| c.get("access_token")?.as_str().map(|s| s.to_string()))
        });

    let params = json!({ "group_id": id });
    let data = onebot_call(
        &base_url,
        "get_group_member_list",
        &params,
        token.as_deref(),
    )?;

    Ok(NodeResponse {
        ok: true,
        node_id: "qq_get_group_members".to_string(),
        messages: None,
        message: Some(format!("group {} member list", id)),
        data: Some(data),
        error: None,
    })
}

/// System notification handler — called by the kernel NotificationBus.
/// Sends a message to all configured test groups.
fn handle_qq_system_notify(req: &NodeRequest) -> Result<NodeResponse, String> {
    let msg = req.message.as_deref().unwrap_or("").trim();
    if msg.is_empty() {
        return Err("message is required for qq_system_notify".to_string());
    }

    let test_groups: Vec<String> = load_runtime_config()
        .and_then(|c| c.get("test_groups")?.as_array().cloned())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    if test_groups.is_empty() {
        return Err("no test_groups configured".to_string());
    }

    let base_url = STATE
        .lock()
        .ok()
        .and_then(|s| s.onebot_url.clone())
        .or_else(|| {
            load_runtime_config().and_then(|c| c.get("onebot_url")?.as_str().map(|s| s.to_string()))
        })
        .ok_or("no OneBot URL configured")?;
    let token = STATE
        .lock()
        .ok()
        .and_then(|s| s.access_token.clone())
        .or_else(|| {
            load_runtime_config()
                .and_then(|c| c.get("access_token")?.as_str().map(|s| s.to_string()))
        });

    for gid in &test_groups {
        // P1-26: was `.parse().unwrap_or(0)` — a mis-typed / non-numeric
        // group id in the config silently routed the system notify to
        // group 0, which either drops or (worse) lands somewhere else in
        // the real world. Skip bad entries with a warning instead.
        match gid.parse::<i64>() {
            Ok(id) if id > 0 => {
                let _ = onebot_send_group_msg(&base_url, id, msg, None, token.as_deref());
            }
            _ => {
                eprintln!("[qq_system_notify] skipping invalid group id in test_groups: {gid}");
            }
        }
    }

    Ok(NodeResponse {
        ok: true,
        node_id: "qq_system_notify".to_string(),
        messages: None,
        message: Some(format!("notified {} groups", test_groups.len())),
        data: None,
        error: None,
    })
}

fn handle_qq_get_group_info(req: &NodeRequest) -> Result<NodeResponse, String> {
    let target = req.target.as_deref().unwrap_or("").trim();
    if target.is_empty() {
        return Err("target is required for qq_get_group_info (group:<id>)".to_string());
    }
    let (kind, id) = parse_target(target)?;
    if kind != TargetKind::Group {
        return Err("target must be group:<id> for qq_get_group_info".to_string());
    }

    let base_url = req
        .payload
        .as_ref()
        .and_then(|p| p.get("onebot_url"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| STATE.lock().ok().and_then(|s| s.onebot_url.clone()))
        .or_else(|| {
            load_runtime_config().and_then(|c| c.get("onebot_url")?.as_str().map(|s| s.to_string()))
        })
        .ok_or("no OneBot URL configured")?;

    let token = req
        .payload
        .as_ref()
        .and_then(|p| p.get("access_token"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| STATE.lock().ok().and_then(|s| s.access_token.clone()))
        .or_else(|| {
            load_runtime_config()
                .and_then(|c| c.get("access_token")?.as_str().map(|s| s.to_string()))
        });

    let params = json!({ "group_id": id });
    let data = onebot_call(&base_url, "get_group_info", &params, token.as_deref())?;

    let group_name = data
        .get("data")
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

    if target.is_empty() {
        return Err("target is required for qq_send".to_string());
    }
    if message.is_empty() {
        return Err("message is required for qq_send".to_string());
    }

    // Read config: payload → STATE → config file (persisted by qq_serve).
    let base_url = req
        .payload
        .as_ref()
        .and_then(|p| p.get("onebot_url"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| STATE.lock().ok().and_then(|s| s.onebot_url.clone()))
        .or_else(|| {
            load_runtime_config().and_then(|c| c.get("onebot_url")?.as_str().map(|s| s.to_string()))
        })
        .ok_or("no OneBot URL configured")?;
    let token = req
        .payload
        .as_ref()
        .and_then(|p| p.get("access_token"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| STATE.lock().ok().and_then(|s| s.access_token.clone()))
        .or_else(|| {
            load_runtime_config()
                .and_then(|c| c.get("access_token")?.as_str().map(|s| s.to_string()))
        });

    let (kind, id) = parse_target(target)?;
    let data = match kind {
        TargetKind::Group => {
            onebot_send_group_msg(&base_url, id, message, reply_to, token.as_deref())?
        }
        TargetKind::Private => {
            onebot_send_private_msg(&base_url, id, message, reply_to, token.as_deref())?
        }
    };
    let _msg_id = data
        .get("data")
        .and_then(|d| d.get("message_id"))
        .and_then(|v| v.as_i64());
    let reply_note = reply_to
        .map(|mid| format!(" (reply to {})", mid))
        .unwrap_or_default();

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

fn handle_qq_ws_serve(req: &NodeRequest) -> Result<NodeResponse, String> {
    if req.action.as_deref() == Some("stop") {
        stop_qq_ws_serve();
        return Ok(NodeResponse {
            ok: true,
            node_id: "qq_ws_serve".to_string(),
            message: Some("WebSocket server stopped".to_string()),
            messages: None,
            data: None,
            error: None,
        });
    }
    let port: u16 = req
        .payload
        .as_ref()
        .and_then(|p| p.get("port"))
        .and_then(|v| v.as_u64())
        .unwrap_or(8002) as u16;

    // Store agent session ID if provided.
    if let Some(ref sid) = req.agent_session_id {
        *AGENT_SESSION_ID.lock().map_err(|e| format!("lock: {e}"))? = Some(sid.clone());
    }

    // Repeated start is idempotent: if the server is already running, report
    // success without rebinding (which would fail with address-in-use).
    {
        let mut running = WS_SERVER_RUNNING.lock().map_err(|e| format!("lock: {e}"))?;
        if !*running {
            // ead1241 ordering: bind synchronously first so a port conflict
            // surfaces as an Err to the caller. Only after a successful bind
            // do we flip the running flag and spawn the accept thread. Hand
            // the bound listener to the thread (rather than dropping and
            // rebinding) so there is no bind race between check and serve.
            let addr = format!("0.0.0.0:{port}");
            let listener = std::net::TcpListener::bind(&addr)
                .map_err(|e| format!("qq_ws_serve: cannot bind {addr}: {e}"))?;
            // Reset shutdown flag now that we hold the socket (in case of a
            // restart after a previous stop).
            WS_SERVER_SHUTDOWN.store(false, Ordering::SeqCst);
            *running = true;
            drop(running);
            let handle = thread::spawn(move || {
                if let Err(e) = start_ws_server(listener, port) {
                    eprintln!("[qq] ws server error: {e}");
                }
            });
            if let Ok(mut guard) = WS_SERVER_HANDLE.lock() {
                *guard = Some(handle);
            }
            start_agent_poller();
        }
    }

    Ok(NodeResponse {
        ok: true,
        node_id: "qq_ws_serve".to_string(),
        message: Some(format!("WebSocket server started on port {port}")),
        messages: None,
        data: None,
        error: None,
    })
}

fn handle(req: &NodeRequest) -> Result<NodeResponse, String> {
    match req.node_id.as_str() {
        "qq_serve" => handle_qq_serve(req),
        "qq_ws_serve" => handle_qq_ws_serve(req),
        "qq_fetch_messages" => handle_qq_fetch_messages(),
        "qq_send" => handle_qq_send(req),
        "qq_get_group_members" => handle_qq_get_group_members(req),
        "qq_system_notify" => handle_qq_system_notify(req),
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
                "QQ adapter using the NoneBot (OneBot v11) protocol. Connect to a OneBot-compatible QQ client for sending messages. Actions: configure, send, status, call, block, unblock, allow, disallow.",
                json!({
                    "type": "object", "required": ["action"],
                    "properties": {
                        "action": { "type": "string", "description": "configure | send | status | call | block | unblock | allow | disallow" },
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
            task_node_doc(
                "qq_ws_serve",
                "Start a WebSocket server to receive OneBot v11 message events via reverse WS connection. Configure your OneBot client (e.g. Napcat) to connect as a WebSocket client to ws://<host>:<port>/. Supports the same grayscale group whitelist as qq_serve.",
                json!({
                    "type": "object", "required": ["node_id"],
                    "properties": {
                        "node_id": { "type": "string", "const": "qq_ws_serve" },
                        "action": { "type": "string", "description": "Set to \"stop\" to shut down the WebSocket server and release the port" },
                        "payload": {
                            "type": "object",
                            "properties": {
                                "port": { "type": "integer", "description": "WebSocket listen port (default 8002)" }
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
                &["starts a WebSocket server thread", "listens on configured port for OneBot WS client connections"],
                &["port already in use", "OneBot WS client not configured to connect"],
            ),
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
            node_doc(
                "qq_system_notify",
                "Kernel notification handler — receives system messages and forwards to configured test groups.",
                json!({
                    "type": "object", "required": ["node_id", "message"],
                    "properties": {
                        "node_id": { "type": "string", "const": "qq_system_notify" },
                        "message": { "type": "string", "description": "Notification text" }
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
                &["sends QQ message to all configured test_groups"],
                &["no OneBot URL configured", "no test_groups configured"],
            ),
        ],
    Some("\
QQ GROUP CHAT MODE — you are running in a QQ group. Messages may be casual chat NOT directed at you.\n\
CRITICAL: Always decide whether the message is actually talking to YOU before responding.\n\
A message IS directed at you ONLY if: contains @[id=3832224285] (explicit @mention of bot); or starts with \"bot\" or \"Bot\" or \"机器人\".\n\
A message is NOT directed at you if: general group discussion; questions not explicitly addressed to bot; casual chat; emoji/sticker; talking about someone else. When in doubt, suspend.\n\
If NOT directed at you: use {\"action\":\"suspend\"}.\n\
If directed at you: invoke_plugin(qq, qq_send, ...) to send your reply, then output {\"action\":\"suspend\"}.\n\
IMPORTANT: NEVER use {\"action\":\"respond\"} — it will cause duplicate messages. Always send via qq_send and finish with {\"action\":\"suspend\"}.\n\
\n\
ANTI-HALLUCINATION rules:\n\
- If a question is ambiguous or you lack context, ask for clarification — do NOT guess.\n\
- If web search fails to yield a clear answer after 2 attempts, stop and tell the user what you found (or didn't find). Ask them to clarify.\n\
- Pay attention to the group's recent conversation. The topic may be group-specific and not searchable on the web.\n\
- A short honest \"not sure, can you clarify?\" is always better than a long wrong answer.\n\
\n\
KERNEL TOOL usage in QQ chat:\n\
- Kernel introspection tools (get_runtime_status, list_plugins, list_nodes, get_kernel_status, get_kernel_issues) are for internal diagnostics only.\n\
- Only call them when a user @mentions you with an explicit question about bot/runtime/plugin status.\n\
- For images, stickers, casual chat, or ambiguous messages: suspend immediately — do NOT call any kernel tool.\n\
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
    AbiFingerprint::current_build("crate_qq_v1", "api_v2")
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

#[cfg(test)]
mod signature_tests {
    use super::verify_onebot_signature;

    #[test]
    fn valid_signature_is_accepted() {
        use hmac::{Hmac, Mac};
        use sha1::Sha1;
        let token = "example-token";
        let body = b"{\"post_type\":\"message\"}";
        let mut mac = Hmac::<Sha1>::new_from_slice(token.as_bytes()).unwrap();
        mac.update(body);
        let sig = mac.finalize().into_bytes();
        let header = format!("sha1={}", hex::encode(sig));
        assert!(verify_onebot_signature(token, body, &header));
        // Also accept a bare hex form without the sha1= prefix (some proxies
        // strip it).
        let bare = hex::encode(sig);
        assert!(verify_onebot_signature(token, body, &bare));
    }

    #[test]
    fn wrong_signature_is_rejected() {
        assert!(!verify_onebot_signature(
            "example-token",
            b"payload",
            "sha1=deadbeefdeadbeefdeadbeefdeadbeefdeadbeef"
        ));
    }

    #[test]
    fn wrong_token_is_rejected() {
        use hmac::{Hmac, Mac};
        use sha1::Sha1;
        let mut mac = Hmac::<Sha1>::new_from_slice(b"other").unwrap();
        mac.update(b"payload");
        let sig = mac.finalize().into_bytes();
        let header = format!("sha1={}", hex::encode(sig));
        assert!(!verify_onebot_signature("expected", b"payload", &header));
    }

    #[test]
    fn malformed_header_is_rejected() {
        assert!(!verify_onebot_signature("t", b"payload", "not-hex"));
        assert!(!verify_onebot_signature("t", b"payload", "sha1=zzz"));
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Task D (E 批): 端到端链路分段验证 — 不消耗 token 的各段。
// 覆盖：webhook 事件解析 → 去重 → 灰度白名单/黑名单 → 队列入列，
// 以及 poller 产出的 agent 触发 prompt 是否能被 runtime 侧 session
// 路由（main.rs:322 附近的 strip_prefix 逻辑）正确解析出 group_id。
// agent 触发（消耗 DeepSeek token）不在单测内验证，见 scripts/ 下脚本。
// ═══════════════════════════════════════════════════════════════════════
#[cfg(test)]
mod chain_tests {
    use super::*;

    fn clear_globals() {
        MESSAGE_QUEUE.lock().unwrap().clear();
        RECENT_MESSAGE_IDS
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
        let mut s = STATE.lock().unwrap();
        s.allow_groups.clear();
        s.block_groups.clear();
    }

    fn group_event(mid: i64, gid: i64, text: &str) -> OneBotEvent {
        let v = json!({
            "post_type": "message",
            "message_type": "group",
            "message": [{"type": "text", "data": {"text": text}}],
            "message_id": mid,
            "user_id": 111,
            "group_id": gid
        });
        serde_json::from_value(v).unwrap()
    }

    fn queue_len() -> usize {
        MESSAGE_QUEUE.lock().unwrap().len()
    }

    // 单测串行化：所有触碰全局静态（MESSAGE_QUEUE / STATE / RECENT_MESSAGE_IDS）
    // 的断言集中在这一个 test 内，避免 cargo 并行跑 test 时的全局态污染。
    #[test]
    fn webhook_ingest_dedup_whitelist_blocklist_chain() {
        clear_globals();

        // ── 段1 webhook 收到 → 入队 ──────────────────────────────
        handle_onebot_event(&group_event(2001, 222, "hello world"));
        assert_eq!(queue_len(), 1, "群消息应进入 MESSAGE_QUEUE");

        // ── 段2 去重：相同 message_id 再来一次被丢弃 ──────────────
        handle_onebot_event(&group_event(2001, 222, "hello world"));
        assert_eq!(queue_len(), 1, "重复 message_id 不应二次入队");
        MESSAGE_QUEUE.lock().unwrap().clear();

        // ── 段3 灰度白名单：设置 allow_groups 后只放行白名单群 ─────
        STATE.lock().unwrap().allow_groups = vec!["222".to_string()];
        handle_onebot_event(&group_event(2002, 999, "not allowed"));
        assert_eq!(queue_len(), 0, "非白名单群应被丢弃");
        handle_onebot_event(&group_event(2003, 222, "allowed"));
        assert_eq!(queue_len(), 1, "白名单群应放行");
        MESSAGE_QUEUE.lock().unwrap().clear();

        // ── 段4 黑名单优先级高于白名单 ────────────────────────────
        STATE.lock().unwrap().block_groups = vec!["222".to_string()];
        handle_onebot_event(&group_event(2004, 222, "blocked"));
        assert_eq!(queue_len(), 0, "黑名单群即使在白名单里也应被丢弃");

        clear_globals();
    }

    // 非消息事件（心跳/通知）不应入队。
    #[test]
    fn non_message_event_is_ignored() {
        clear_globals();
        let ev: OneBotEvent = serde_json::from_value(json!({
            "post_type": "meta_event",
            "user_id": 0
        }))
        .unwrap();
        handle_onebot_event(&ev);
        assert_eq!(
            MESSAGE_QUEUE.lock().unwrap().len(),
            0,
            "非 message 事件应被忽略"
        );
        clear_globals();
    }

    // poller 产出的 prompt 必须能被 runtime session 路由解析出 group_id。
    // 复刻 main.rs:322 的 strip 逻辑，验证格式契约（这是链路契约校验，
    // 不是测 runtime 代码）。
    #[test]
    fn agent_prompt_format_is_parseable_by_session_router() {
        let sender_id = "222";
        let user_id = "111";
        let prompt = format!(
            "[QQ group from {} (user {})]: {}",
            sender_id, user_id, "在吗"
        );
        let gid = prompt
            .strip_prefix("[QQ group from ")
            .and_then(|rest| rest.find("]: ").map(|end| &rest[..end]))
            .map(|prefix| {
                prefix
                    .split_once(" (user ")
                    .map(|(g, _)| g)
                    .unwrap_or(prefix)
                    .to_string()
            });
        assert_eq!(
            gid.as_deref(),
            Some("222"),
            "runtime 应能从 prompt 还原 group_id"
        );
    }

    // 消息文本抽取：结构化 segments（text/at/image/reply）。
    #[test]
    fn extract_message_info_handles_segments() {
        let msg = json!([
            {"type": "reply", "data": {"id": "555"}},
            {"type": "at", "data": {"qq": "3832224285", "name": "bot"}},
            {"type": "text", "data": {"text": " hello"}}
        ]);
        let (text, reply_to) = extract_message_info(&msg, None);
        assert_eq!(reply_to, Some(555));
        assert!(text.contains("@[id=3832224285,name=bot]"));
        assert!(text.contains("hello"));
    }

    // 消息文本抽取：raw_message 中的 CQ code。
    #[test]
    fn extract_message_info_handles_cq_codes() {
        let msg = json!("");
        let (text, _) = extract_message_info(&msg, Some("[CQ:at,qq=123]你好"));
        assert!(text.contains("@[id=123"));
        assert!(text.contains("你好"));
    }

    // should_process 过滤：过短消息不转发；斜杠指令放行（N批：kernel
    // 指令路由器接管，不经 LLM）。
    #[test]
    fn should_process_filters_short_and_forwards_slash() {
        assert!(!should_process("ok"), "过短消息不触发");
        assert!(should_process("/help"), "斜杠指令放行给指令路由器");
        assert!(!should_process("/"), "裸斜杠不触发");
        assert!(should_process("这是一条正常消息"), "正常消息触发");
    }

    // envelope 必须携带路由 + 身份字段（J批：qq 从纯文本升级为 envelope）。
    #[test]
    fn build_envelope_carries_route_and_identity() {
        let msg = IncomingMessage {
            message_type: "group".to_string(),
            sender_id: "123456".to_string(),
            user_id: "789".to_string(),
            message: "hello".to_string(),
            message_id: Some(42),
            reply_to_msg_id: None,
            raw_event: None,
        };
        let env: Value = serde_json::from_str(&build_envelope(&msg)).unwrap();
        assert_eq!(env["source_plugin"], "qq");
        assert_eq!(env["reply_node"], "qq_send");
        assert_eq!(env["session_key"], "qq:group:123456");
        assert_eq!(env["reply_target"], "group:123456");
        assert_eq!(env["sender_id"], "qq:789");
        assert_eq!(env["conversation_kind"], "group");
        assert_eq!(env["reply_to"], "42");
    }

    // reply_to 双类型解析：i64（legacy）与字符串（inbox 路由）都接受。
    #[test]
    fn node_request_reply_to_accepts_both_types() {
        let req: NodeRequest =
            serde_json::from_str(r#"{"node_id":"qq_send","reply_to":42}"#).unwrap();
        assert_eq!(req.reply_to, Some(42));
        let req: NodeRequest =
            serde_json::from_str(r#"{"node_id":"qq_send","reply_to":"42"}"#).unwrap();
        assert_eq!(req.reply_to, Some(42));
        let req: NodeRequest =
            serde_json::from_str(r#"{"node_id":"qq_send","reply_to":"om_x"}"#).unwrap();
        assert_eq!(req.reply_to, None, "不可解析字符串降级为无引用");
    }
}

// ═══════════════════════════════════════════════════════════════════════
// qq_ws_serve 生命周期单测：bind 竞争 / 停机释放端口 / 重复 start 幂等。
// 三段断言集中在一个 test 内串行执行，避免 cargo 并行跑 test 时对 WS
// 全局静态（WS_SERVER_RUNNING / WS_SERVER_HANDLE / WS_SERVER_SHUTDOWN）的
// 争用。用 OS 分配的随机高位端口，规避固定端口冲突。
// ═══════════════════════════════════════════════════════════════════════
#[cfg(test)]
mod ws_tests {
    use super::*;
    use std::net::TcpListener;

    /// Ask the OS for a free port, then release it so the caller can rebind.
    /// A later bind may in theory race another process, but on a test host
    /// the window is small and the high ephemeral range keeps collisions rare.
    fn free_port() -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        port
    }

    fn start_req(port: u16) -> NodeRequest {
        serde_json::from_value(json!({
            "node_id": "qq_ws_serve",
            "payload": { "port": port }
        }))
        .unwrap()
    }

    fn stop_req() -> NodeRequest {
        serde_json::from_value(json!({
            "node_id": "qq_ws_serve",
            "action": "stop"
        }))
        .unwrap()
    }

    #[test]
    fn ws_serve_bind_stop_idempotent_lifecycle() {
        // Ensure a clean starting state regardless of test ordering.
        stop_qq_ws_serve();

        // ── 段1 端口被占用时 start 返回 Err ──────────────────────────
        let occupied = free_port();
        let _held = TcpListener::bind(("0.0.0.0", occupied))
            .expect("test harness should be able to bind the probe port");
        let err = handle_qq_ws_serve(&start_req(occupied));
        assert!(err.is_err(), "bind 被占用端口应返回 Err，实际: {err:?}");
        assert!(
            !*WS_SERVER_RUNNING.lock().unwrap(),
            "bind 失败后 running 标志不应被置位"
        );
        drop(_held);

        // ── 段2 stop 后端口释放，可重新 bind ─────────────────────────
        let port = free_port();
        let resp = handle_qq_ws_serve(&start_req(port)).expect("start 应成功");
        assert!(resp.ok, "首次 start 应 ok");
        assert!(
            *WS_SERVER_RUNNING.lock().unwrap(),
            "start 成功后 running 标志应被置位"
        );
        // 端口此刻被服务线程持有，外部无法 bind。
        assert!(
            TcpListener::bind(("0.0.0.0", port)).is_err(),
            "服务运行期间端口应被占用"
        );
        // 停机：join 线程、释放端口。
        let resp = handle_qq_ws_serve(&stop_req()).expect("stop 应成功");
        assert!(resp.ok, "stop 应 ok");
        assert!(
            !*WS_SERVER_RUNNING.lock().unwrap(),
            "stop 后 running 标志应清除"
        );
        // 停机后同一端口应可被重新 bind。
        let rebind = TcpListener::bind(("0.0.0.0", port));
        assert!(
            rebind.is_ok(),
            "stop 后端口应释放可重新 bind，实际: {:?}",
            rebind.err()
        );
        drop(rebind);

        // ── 段3 重复 start 幂等：第二次 start 不报错、不重复 bind ─────
        let port = free_port();
        let r1 = handle_qq_ws_serve(&start_req(port)).expect("第一次 start 应成功");
        assert!(r1.ok);
        // 第二次用同端口 start：若非幂等会因 address-in-use 而 Err。
        let r2 = handle_qq_ws_serve(&start_req(port)).expect("重复 start 应幂等返回 Ok");
        assert!(r2.ok, "重复 start 应 ok");

        // 清理，避免影响其它 test / 泄漏线程占用端口。
        let _ = handle_qq_ws_serve(&stop_req());
    }
}
