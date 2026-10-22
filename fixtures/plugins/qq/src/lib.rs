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
    /// Reply-policy text as a top-level field (WS-4). Accepted in addition to
    /// `payload.policy` so `qq_set_reply_policy` works both through the
    /// kernel's payload convention and via a direct top-level invoke.
    #[serde(default)]
    policy: Option<String>,
    /// Explicit data directory as a top-level field (alternative to
    /// `payload.data_dir`), same dual-accept rationale as `policy`.
    #[serde(default)]
    data_dir: Option<String>,
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

// ---------------------------------------------------------------------------
// Reply policy (WS-4) — runtime-configurable reply gating
// ---------------------------------------------------------------------------
//
// The gating rule ("is this message addressed to me?") used to be hard-coded
// in this plugin's `system_hint`, so letting the bot be chattier required
// editing Rust and rebuilding the artifact. It is data now:
// `data/qq_reply_policy.json`, shipped to the model as a `[reply policy: ...]`
// line at the top of every incoming message.

/// Conservative default, used whenever no usable policy file exists. Kept in
/// English because the model reads it verbatim.
const DEFAULT_REPLY_POLICY: &str = "Decide for yourself, from the conversation context, whether this message needs an answer from you. Answer when it is addressed to you (\"bot\"/\"Bot\"/\"机器人\", or an @mention), AND ALSO when the context makes it clear the sender is talking to you: it continues a thread you are part of, asks you to do or explain something, or someone in the group explicitly wants your answer — even without naming you. Stay silent for conversation between other people, memes/stickers, and statements that do not involve you; do not hijack someone else's thread. If it is genuinely ambiguous, prefer silence.";

/// On-disk shape of `data/qq_reply_policy.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReplyPolicyRecord {
    policy: String,
    #[serde(default)]
    updated_at_ms: u64,
    #[serde(default)]
    updated_by: String,
}

/// Resolve the reply-policy file location. Preference order is exactly
/// soul_store's `db_path()`: explicit `data_dir` from the payload (the kernel
/// knows the workspace root), then `$CORDIS_FIXTURES_ROOT/../data`, then a
/// cwd-relative `data/`.
fn reply_policy_path(data_dir: Option<&str>) -> std::path::PathBuf {
    if let Some(dir) = data_dir {
        if !dir.is_empty() {
            return std::path::PathBuf::from(dir).join("qq_reply_policy.json");
        }
    }
    if let Ok(root) = std::env::var("CORDIS_FIXTURES_ROOT") {
        let ws = std::path::PathBuf::from(&root)
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| std::path::PathBuf::from(&root));
        return ws.join("data/qq_reply_policy.json");
    }
    std::path::PathBuf::from("data/qq_reply_policy.json")
}

/// Read the persisted policy. `None` means "no usable file" — missing,
/// unreadable, malformed, or blank — and every caller falls back to the
/// default rather than failing.
fn read_reply_policy_file(data_dir: Option<&str>) -> Option<String> {
    let path = reply_policy_path(data_dir);
    let raw = std::fs::read_to_string(&path).ok()?;
    let record = serde_json::from_str::<ReplyPolicyRecord>(&raw).ok()?;
    Some(record.policy.trim().to_string()).filter(|p| !p.is_empty())
}

/// Effective policy for this message. NEVER fails: a missing or broken file
/// yields `DEFAULT_REPLY_POLICY`, so envelope construction cannot break.
fn current_reply_policy(data_dir: Option<&str>) -> String {
    read_reply_policy_file(data_dir).unwrap_or_else(|| DEFAULT_REPLY_POLICY.to_string())
}

/// Node arguments may arrive inside `payload` (kernel convention) or as a
/// top-level request field (direct invoke); the payload wins when both exist.
fn node_str_arg(payload: Option<&Value>, top_level: Option<&str>, key: &str) -> Option<String> {
    let from_payload = payload.and_then(|p| p.get(key)).and_then(Value::as_str);
    match from_payload {
        Some(v) => Some(v.to_string()),
        None => top_level.map(str::to_string),
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// Create the policy file's parent directory. `path` is always a join of a
/// data dir and the file name, so a parent exists; if it somehow does not,
/// creating the path itself is the closest honest fallback.
fn ensure_policy_parent_dir(path: &std::path::Path) -> Result<(), String> {
    let parent = path.parent().unwrap_or(path);
    std::fs::create_dir_all(parent).map_err(|e| format!("create data dir: {e}"))
}

/// Build the runtime routing envelope (mirrors feishu's build_envelope).
/// sender_id is "group:<gid>" or "private:<uid>"; the actual person is
/// user_id, so identity is "qq:<user_id>".
/// 从渲染后的正文里按顺序收集 `[image: <url>]` 里的 URL。
///
/// 只认 `[image: `（不是 `[image file: `）——后者是没有 URL 的占位符，
/// 交出去也没法下载。收集到的地址随 envelope 的 `images` 字段交给 kernel，
/// 由 kernel 下载 + 内联成多模态 content part。
fn collect_image_urls(text: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let mut rest = text;
    let marker = "[image: ";
    while let Some(start) = rest.find(marker) {
        let after = &rest[start + marker.len()..];
        let Some(end) = after.find(']') else { break };
        let url = after[..end].trim();
        if !url.is_empty() {
            urls.push(url.to_string());
        }
        rest = &after[end..];
    }
    urls
}

fn build_envelope(msg: &IncomingMessage) -> String {
    // WS-4: every inbound message carries the current reply policy as its
    // first line. Unreadable/missing policy file → DEFAULT_REPLY_POLICY, so
    // this can never fail envelope construction.
    let reply_policy = current_reply_policy(None);
    let reply_target = format!("{}:{}", msg.message_type, msg.sender_id);
    let session_key = format!("qq:{reply_target}");
    let display = format!(
        "[reply policy: {reply_policy}]\n[QQ {} from {} (user {})]: {}",
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
        // 图片地址交给 kernel 处理（下载 + 内联），插件不碰多模态 wire format。
        "images": collect_image_urls(&msg.message),
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
/// `$CORDIS_FIXTURES_ROOT`. The filesystem plugin already reads this env var;
/// this makes QQ portable across environments and testing setups.
///
/// The env-missing fallback used to be the absolute
/// `/root/CordisClaw/fixtures/…`, i.e. a *different* checkout on this box: a
/// runtime started from `/root/CordisClaw-minori` silently read (and, through
/// `save_runtime_config`, wrote) the other repo's drafts file. `serve <root>`
/// runs with the repo root as cwd, so the honest fallback is the cwd-relative
/// `fixtures/.cordis-drafts/…`.
fn runtime_config_path() -> std::path::PathBuf {
    match std::env::var("CORDIS_FIXTURES_ROOT") {
        Ok(root) => std::path::PathBuf::from(root).join(".cordis-drafts/qq_runtime_config.json"),
        Err(_) => std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .join("fixtures/.cordis-drafts/qq_runtime_config.json"),
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

/// Extract one `{user_id, nickname, card}` entry from a OneBot member object.
/// `None` when the object carries no `user_id` — nothing to identify it by.
/// Missing/empty `nickname` or `card` degrade to null instead of dropping the
/// entry (real NapCat payloads omit `card` for members without a group card).
fn group_role_entry(member: &Value) -> Option<Value> {
    let user_id = member.get("user_id")?;
    let nickname = member
        .get("nickname")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let card = member
        .get("card")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    Some(json!({"user_id": user_id, "nickname": nickname, "card": card}))
}

/// Pure aggregation of a `get_group_member_list` response into
/// `{owner, admins, member_count, source}`. Accepts either the full OneBot
/// envelope (`{"data":[..]}`) or the bare member array. An absent `owner` is
/// left null on purpose: the caller then falls back to `get_group_info_ex`.
fn summarize_group_roles(members: &Value) -> Value {
    let list = members.get("data").unwrap_or(members);
    let entries = list.as_array().map_or(&[][..], Vec::as_slice);
    let mut owner: Option<Value> = None;
    let mut admins: Vec<Value> = Vec::new();
    for entry in entries {
        let role = entry.get("role").and_then(Value::as_str).unwrap_or("");
        // `.into_iter()` on the Option keeps the "no user_id" skip in one
        // place without an `if let` whose implicit else edge never runs.
        for role_entry in group_role_entry(entry).into_iter() {
            match role {
                "owner" => owner = Some(role_entry),
                "admin" => admins.push(role_entry),
                _ => {}
            }
        }
    }
    json!({
        "owner": owner,
        "admins": admins,
        "member_count": entries.len(),
        "source": "member_list",
    })
}

/// Owner entry from `get_group_info_ex`'s
/// `data.extInfo.groupOwnerId.memberUin`. This is the fallback for OneBot
/// backends whose member list omits `role`; only the uid is available, so
/// nickname/card stay null.
fn owner_from_group_info_ex(info: &Value) -> Option<Value> {
    let uin = info.pointer("/data/extInfo/groupOwnerId/memberUin")?;
    Some(json!({"user_id": uin, "nickname": null, "card": null}))
}

/// WS-4: answer "who is the group owner / who are the admins?" WITHOUT the
/// model having to invent a permission error. Aggregates the member list by
/// `role`, and falls back to `get_group_info_ex` when the backend omits it.
fn handle_qq_get_group_roles(req: &NodeRequest) -> Result<NodeResponse, String> {
    let target = req.target.as_deref().unwrap_or("").trim();
    if target.is_empty() {
        return Err("target is required for qq_get_group_roles (group:<id>)".to_string());
    }
    let (kind, id) = parse_target(target)?;
    if kind != TargetKind::Group {
        return Err("target must be group:<id> for qq_get_group_roles".to_string());
    }

    // Same resolution order as handle_qq_get_group_members:
    // payload → global STATE → persisted runtime config.
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
    let members = onebot_call(
        &base_url,
        "get_group_member_list",
        &params,
        token.as_deref(),
    )?;
    let summary = summarize_group_roles(&members);
    let roles = if summary["owner"].is_null() {
        // Member list carries no `role` for the owner: ask get_group_info_ex.
        let info = onebot_call(&base_url, "get_group_info_ex", &params, token.as_deref())?;
        json!({
            "owner": owner_from_group_info_ex(&info),
            "admins": [],
            "member_count": Value::Null,
            "source": "group_info_ex",
        })
    } else {
        summary
    };

    Ok(NodeResponse {
        ok: true,
        node_id: "qq_get_group_roles".to_string(),
        messages: None,
        message: Some(format!("group {} roles", id)),
        data: Some(roles),
        error: None,
    })
}

/// WS-4: persist the reply policy so the next inbound message already uses it
/// (build_envelope reads the file per message — no restart, no rebuild).
fn handle_qq_set_reply_policy(req: &NodeRequest) -> Result<NodeResponse, String> {
    let raw =
        node_str_arg(req.payload.as_ref(), req.policy.as_deref(), "policy").unwrap_or_default();
    let policy = raw.trim().to_string();
    if policy.is_empty() {
        return Err("policy must not be empty".to_string());
    }
    let data_dir = node_str_arg(req.payload.as_ref(), req.data_dir.as_deref(), "data_dir");
    let path = reply_policy_path(data_dir.as_deref());
    ensure_policy_parent_dir(&path)?;
    let record = ReplyPolicyRecord {
        policy: policy.clone(),
        updated_at_ms: now_ms(),
        updated_by: "agent".to_string(),
    };
    let body =
        serde_json::to_string_pretty(&record).map_err(|e| format!("encode reply policy: {e}"))?;
    std::fs::write(&path, &body).map_err(|e| format!("write reply policy: {e}"))?;
    Ok(NodeResponse {
        ok: true,
        node_id: "qq_set_reply_policy".to_string(),
        messages: None,
        message: Some(format!("reply policy updated at {}", path.display())),
        data: Some(json!({"policy": policy, "path": path.display().to_string()})),
        error: None,
    })
}

/// WS-4: read the persisted reply policy (`is_default` = nothing stored yet).
fn handle_qq_get_reply_policy(req: &NodeRequest) -> Result<NodeResponse, String> {
    let data_dir = node_str_arg(req.payload.as_ref(), req.data_dir.as_deref(), "data_dir");
    let stored = read_reply_policy_file(data_dir.as_deref());
    let is_default = stored.is_none();
    Ok(NodeResponse {
        ok: true,
        node_id: "qq_get_reply_policy".to_string(),
        messages: None,
        message: None,
        data: Some(json!({
            "policy": stored.unwrap_or_else(|| DEFAULT_REPLY_POLICY.to_string()),
            "is_default": is_default,
        })),
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
        "qq_get_group_roles" => handle_qq_get_group_roles(req),
        "qq_get_reply_policy" => handle_qq_get_reply_policy(req),
        "qq_set_reply_policy" => handle_qq_set_reply_policy(req),
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
                "qq_get_group_members",
                "Get the raw OneBot member list for a group (get_group_member_list). Each entry carries user_id, nickname, card and role (owner/admin/member).",
                json!({
                    "type": "object", "required": ["node_id", "target"],
                    "properties": {
                        "node_id": { "type": "string", "const": "qq_get_group_members" },
                        "target": { "type": "string", "description": "group:<id>" }
                    }
                }),
                json!({
                    "type": "object",
                    "properties": {
                        "ok": { "type": "boolean" },
                        "message": { "type": ["string", "null"] },
                        "data": { "type": "object", "description": "Raw OneBot get_group_member_list response" },
                        "error": { "type": ["string", "null"] }
                    }
                }),
                &["calls OneBot get_group_member_list API"],
                &["no OneBot URL configured", "target must be group:<id>"],
            ).with_agent_accessible(),
            node_doc(
                "qq_get_group_roles",
                "Answer 'who is the group owner / who are the admins?'. Aggregates get_group_member_list by role, and falls back to get_group_info_ex (data.extInfo.groupOwnerId.memberUin) when the member list has no role field. source tells you which path was used.",
                json!({
                    "type": "object", "required": ["node_id", "target"],
                    "properties": {
                        "node_id": { "type": "string", "const": "qq_get_group_roles" },
                        "target": { "type": "string", "description": "group:<id>" },
                        "payload": {
                            "type": "object",
                            "properties": {
                                "onebot_url": { "type": "string", "description": "Optional OneBot HTTP API URL override (defaults to STATE / runtime config)" },
                                "access_token": { "type": "string", "description": "Optional OneBot access token override (defaults to STATE / runtime config)" }
                            }
                        }
                    }
                }),
                json!({
                    "type": "object",
                    "properties": {
                        "ok": { "type": "boolean" },
                        "message": { "type": ["string", "null"] },
                        "data": {
                            "type": "object",
                            "properties": {
                                "owner": { "type": ["object", "null"], "description": "{user_id, nickname, card} or null" },
                                "admins": { "type": "array", "items": { "type": "object" } },
                                "member_count": { "type": ["integer", "null"] },
                                "source": { "type": "string", "description": "member_list | group_info_ex" }
                            }
                        },
                        "error": { "type": ["string", "null"] }
                    }
                }),
                &["calls OneBot get_group_member_list, then get_group_info_ex when the owner is missing"],
                &["no OneBot URL configured", "invalid target format", "OneBot API error"],
            ).with_agent_accessible(),
            node_doc(
                "qq_get_reply_policy",
                "Read the persisted reply policy — the rule the bot follows to decide whether an incoming QQ message is addressed to it. is_default=true means no policy has been stored yet.",
                json!({
                    "type": "object", "required": ["node_id"],
                    "properties": {
                        "node_id": { "type": "string", "const": "qq_get_reply_policy" },
                        "data_dir": { "type": "string", "description": "Optional data directory override" }
                    }
                }),
                json!({
                    "type": "object",
                    "properties": {
                        "ok": { "type": "boolean" },
                        "data": {
                            "type": "object",
                            "properties": {
                                "policy": { "type": "string" },
                                "is_default": { "type": "boolean" }
                            }
                        }
                    }
                }),
                &["reads data/qq_reply_policy.json"],
                &[],
            ).with_agent_accessible(),
            node_doc(
                "qq_set_reply_policy",
                "Persist the reply policy: the rule the bot follows when deciding whether an incoming message is addressed to it. It is shipped to the model as the first line of every message, so it takes effect on the NEXT message — no rebuild and no restart.",
                json!({
                    "type": "object", "required": ["node_id", "policy"],
                    "properties": {
                        "node_id": { "type": "string", "const": "qq_set_reply_policy" },
                        "policy": { "type": "string", "description": "New reply rule text (English; shown to the model verbatim)" },
                        "data_dir": { "type": "string", "description": "Optional data directory override" }
                    }
                }),
                json!({
                    "type": "object",
                    "properties": {
                        "ok": { "type": "boolean" },
                        "data": {
                            "type": "object",
                            "properties": {
                                "policy": { "type": "string" },
                                "path": { "type": "string" }
                            }
                        }
                    }
                }),
                &["writes data/qq_reply_policy.json"],
                &["policy must not be empty"],
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
REPLY POLICY — the authority on whether to answer: every incoming message begins with a line `[reply policy: ...]`. That line IS the rule; follow it exactly. It may be more permissive than you expect (e.g. \"reply whenever someone seems to want an answer\"), so do not apply a stricter rule of your own.\n\
If a message has NO `[reply policy: ...]` line, fall back to the conservative default: answer ONLY when the message @mentions you or addresses you by name (\"bot\", \"Bot\", \"机器人\"); otherwise suspend.\n\
A message is NOT directed at you if: general group discussion; questions not explicitly addressed to you under the policy above; casual chat; emoji/sticker; talking about someone else.\n\
If NOT directed at you: use {\"action\":\"suspend\"}.\n\
If directed at you: invoke_plugin(qq, qq_send, ...) to send your reply, then output {\"action\":\"suspend\"}.\n\
IMPORTANT: NEVER use {\"action\":\"respond\"} — it will cause duplicate messages. Always send via qq_send and finish with {\"action\":\"suspend\"}.\n\
If the user asks you to change the reply rule (e.g. \"reply to everything I say\", \"stop answering so often\"), persist it: invoke_plugin(qq, qq_set_reply_policy, {\"node_id\":\"qq_set_reply_policy\",\"policy\":\"<new rule text>\"}), then tell the user it is already in effect — the next message uses the new policy, no restart needed.\n\
\n\
ANTI-HALLUCINATION rules:\n\
- If a question is ambiguous or you lack context, ask for clarification — do NOT guess.\n\
- If web search fails to yield a clear answer after 2 attempts, stop and tell the user what you found (or didn't find). Ask them to clarify.\n\
- Pay attention to the group's recent conversation. The topic may be group-specific and not searchable on the web.\n\
- A short honest \"not sure, can you clarify?\" is always better than a long wrong answer.\n\
- Never claim a tool or permission is unavailable without actually invoking it first: every node listed in this plugin's interfaces is callable by you.\n\
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
You CAN query group members/roles: invoke_plugin(qq, qq_get_group_roles, {\"node_id\":\"qq_get_group_roles\",\"target\":\"group:<id>\"}). Use it when asked who the group owner or admins are — never guess a name or claim you lack permission.\n\
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

// ═══════════════════════════════════════════════════════════════════════
// WS-4 测试支撑：`CORDIS_FIXTURES_ROOT` 是进程级全局量，凡依赖「env 路径
// 解析」的用例都在这个锁下、用私有 fixtures root 跑；能显式传 `data_dir`
// 的用例就不用 env（避免并行 test 之间的 race）。
// ═══════════════════════════════════════════════════════════════════════
#[cfg(test)]
mod test_support {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());
    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn seq() -> u64 {
        SEQ.fetch_add(1, Ordering::SeqCst)
    }

    /// 在 `CORDIS_FIXTURES_ROOT=<temp>/fixtures` 下执行 `f`，于是
    /// `$CORDIS_FIXTURES_ROOT/../data` 落在 `<temp>/data`。
    pub fn with_isolated_fixtures<T>(f: impl FnOnce(&std::path::Path) -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let root = std::env::temp_dir().join(format!("qq-ws4-{}-{}", std::process::id(), seq()));
        let fixtures = root.join("fixtures");
        std::fs::create_dir_all(&fixtures).expect("create temp fixtures root");
        std::env::set_var("CORDIS_FIXTURES_ROOT", &fixtures);
        let out = f(&root);
        std::env::remove_var("CORDIS_FIXTURES_ROOT");
        let _ = std::fs::remove_dir_all(&root);
        out
    }

    /// 唯一的、已创建的空临时目录（用例自己负责清理）。
    pub fn temp_dir() -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("qq-ws4-data-{}-{}", std::process::id(), seq()));
        std::fs::create_dir_all(&dir).expect("create temp data dir");
        dir
    }
}

// ═══════════════════════════════════════════════════════════════════════
// WS-4 (A)：回复策略读写 + build_envelope 下发 `[reply policy: ...]` 行。
// ═══════════════════════════════════════════════════════════════════════
#[cfg(test)]
mod reply_policy_tests {
    use super::test_support::{temp_dir, with_isolated_fixtures};
    use super::*;

    fn req(value: Value) -> NodeRequest {
        serde_json::from_value(value).expect("well-formed NodeRequest")
    }

    fn policy_req(node_id: &str, data_dir: &std::path::Path) -> NodeRequest {
        req(json!({
            "node_id": node_id,
            "payload": {"data_dir": data_dir.to_str().unwrap()}
        }))
    }

    fn set_req(policy: &str, data_dir: &std::path::Path) -> NodeRequest {
        req(json!({
            "node_id": "qq_set_reply_policy",
            "payload": {"policy": policy, "data_dir": data_dir.to_str().unwrap()}
        }))
    }

    fn sample_msg() -> IncomingMessage {
        IncomingMessage {
            message_type: "group".to_string(),
            sender_id: "123456".to_string(),
            user_id: "789".to_string(),
            message: "hello".to_string(),
            message_id: Some(42),
            reply_to_msg_id: None,
            raw_event: None,
        }
    }

    fn display_of(envelope_json: &str) -> String {
        let env: Value = serde_json::from_str(envelope_json).expect("envelope json");
        env["display"]
            .as_str()
            .expect("display must be a string")
            .to_string()
    }

    // 路径解析顺序与 soul_store::db_path 完全一致：
    // 显式 data_dir → $CORDIS_FIXTURES_ROOT/../data → cwd 相对 data/。
    #[test]
    fn reply_policy_path_resolution_order() {
        with_isolated_fixtures(|root| {
            // 1) payload 的 data_dir 最优先。
            let explicit = reply_policy_path(Some("/tmp/ws4-explicit"));
            assert_eq!(
                explicit,
                std::path::PathBuf::from("/tmp/ws4-explicit/qq_reply_policy.json")
            );
            // 2) 空 data_dir 视作未给 → 走 $CORDIS_FIXTURES_ROOT/../data。
            let from_env = reply_policy_path(Some(""));
            assert_eq!(from_env, root.join("data").join("qq_reply_policy.json"));
            // 3) fixtures root 无父目录时退回 root 自身。
            std::env::set_var("CORDIS_FIXTURES_ROOT", "/");
            let root_self = reply_policy_path(None);
            assert_eq!(
                root_self,
                std::path::PathBuf::from("/data/qq_reply_policy.json")
            );
            // 4) 无 env → cwd 相对的 data/。
            std::env::remove_var("CORDIS_FIXTURES_ROOT");
            let cwd_relative = reply_policy_path(None);
            assert_eq!(
                cwd_relative,
                std::path::PathBuf::from("data/qq_reply_policy.json")
            );
        });
    }

    // 未设置 → 默认策略 + is_default=true；set 之后 get 返回同一文本且
    // is_default=false，落盘格式含 updated_by="agent"。
    #[test]
    fn get_returns_default_then_roundtrips_set_policy() {
        let dir = temp_dir();
        let before = handle_qq_get_reply_policy(&policy_req("qq_get_reply_policy", &dir))
            .expect("get without a file must not fail");
        let before_data = before.data.expect("data");
        assert_eq!(before_data["policy"], DEFAULT_REPLY_POLICY);
        assert_eq!(before_data["is_default"], true);

        let set = handle_qq_set_reply_policy(&set_req(
            "Reply whenever someone seems to want an answer.",
            &dir,
        ))
        .expect("set must succeed");
        let set_data = set.data.expect("data");
        assert_eq!(
            set_data["policy"],
            "Reply whenever someone seems to want an answer."
        );
        assert_eq!(
            set_data["path"],
            dir.join("qq_reply_policy.json").to_str().unwrap()
        );

        let raw = std::fs::read_to_string(dir.join("qq_reply_policy.json")).expect("policy file");
        let record: ReplyPolicyRecord = serde_json::from_str(&raw).expect("policy json");
        assert_eq!(record.updated_by, "agent");
        assert_eq!(
            record.policy,
            "Reply whenever someone seems to want an answer."
        );

        let after = handle_qq_get_reply_policy(&policy_req("qq_get_reply_policy", &dir))
            .expect("get after set");
        let after_data = after.data.expect("data");
        assert_eq!(
            after_data["policy"],
            "Reply whenever someone seems to want an answer."
        );
        assert_eq!(after_data["is_default"], false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // top-level 字段形式（不经 payload）也接受：policy / data_dir。
    #[test]
    fn set_accepts_top_level_policy_and_data_dir() {
        let dir = temp_dir();
        let top_level_set = req(json!({
            "node_id": "qq_set_reply_policy",
            "policy": "Only answer when @mentioned.",
            "data_dir": dir.to_str().unwrap()
        }));
        handle_qq_set_reply_policy(&top_level_set).expect("top-level set");
        let get = req(json!({
            "node_id": "qq_get_reply_policy",
            "data_dir": dir.to_str().unwrap()
        }));
        let resp = handle_qq_get_reply_policy(&get).expect("top-level get");
        assert_eq!(
            resp.data.expect("data")["policy"],
            "Only answer when @mentioned."
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // 两个策略节点必须进 handle() 分发（kernel 只经 handle() 调插件）。
    #[test]
    fn handle_dispatches_policy_nodes() {
        let dir = temp_dir();
        let set = req(json!({
            "node_id": "qq_set_reply_policy",
            "payload": {"policy": "Dispatched through handle().", "data_dir": dir.to_str().unwrap()}
        }));
        let resp = handle(&set).expect("set through handle");
        assert!(resp.ok);
        assert_eq!(resp.node_id, "qq_set_reply_policy");
        let get = req(json!({
            "node_id": "qq_get_reply_policy",
            "payload": {"data_dir": dir.to_str().unwrap()}
        }));
        let resp = handle(&get).expect("get through handle");
        let data = resp.data.expect("data");
        assert_eq!(data["policy"], "Dispatched through handle().");
        assert_eq!(data["is_default"], false);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // 空 / 全空白 / 缺省 policy → 明确错误，且不落盘。
    #[test]
    fn set_rejects_empty_policy() {
        let dir = temp_dir();
        for bad in ["", "   \n\t "] {
            let err = handle_qq_set_reply_policy(&set_req(bad, &dir))
                .expect_err("empty policy must be rejected");
            assert_eq!(err, "policy must not be empty");
        }
        let missing_field = req(json!({
            "node_id": "qq_set_reply_policy",
            "payload": {"data_dir": dir.to_str().unwrap()}
        }));
        let err = handle_qq_set_reply_policy(&missing_field)
            .expect_err("missing policy must be rejected");
        assert_eq!(err, "policy must not be empty");
        let untouched = !dir.join("qq_reply_policy.json").exists();
        assert!(untouched, "a rejected set must not create the file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // I/O 故障注入：数据目录父路径是普通文件 → create_dir_all 失败臂。
    #[test]
    fn set_reports_data_dir_creation_failure() {
        let dir = temp_dir();
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, b"not a directory").expect("write blocker file");
        let unreachable = blocker.join("sub");
        let err = handle_qq_set_reply_policy(&set_req("whatever", &unreachable))
            .expect_err("create_dir_all under a regular file must fail");
        let is_create_error = err.starts_with("create data dir:");
        assert!(is_create_error, "err: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // I/O 故障注入：目标位置被目录占住 → fs::write 失败臂。
    #[test]
    fn set_reports_write_failure() {
        let dir = temp_dir();
        std::fs::create_dir_all(dir.join("qq_reply_policy.json")).expect("occupy target path");
        let err = handle_qq_set_reply_policy(&set_req("whatever", &dir))
            .expect_err("writing over a directory must fail");
        let is_write_error = err.starts_with("write reply policy:");
        assert!(is_write_error, "err: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // 文件损坏 / 空白 → 视作未配置，回落默认策略（绝不让读失败上浮）。
    #[test]
    fn malformed_or_blank_file_falls_back_to_default() {
        let dir = temp_dir();
        let path = dir.join("qq_reply_policy.json");
        let data_dir = dir.to_str().unwrap();
        std::fs::write(&path, b"{not json").expect("write malformed");
        assert!(read_reply_policy_file(Some(data_dir)).is_none());
        std::fs::write(&path, br#"{"policy":"   "}"#).expect("write blank");
        assert!(read_reply_policy_file(Some(data_dir)).is_none());
        assert_eq!(current_reply_policy(Some(data_dir)), DEFAULT_REPLY_POLICY);
        std::fs::write(
            &path,
            br#"{"policy":"  custom rule  ","updated_by":"agent"}"#,
        )
        .expect("write custom");
        assert_eq!(
            read_reply_policy_file(Some(data_dir)).as_deref(),
            Some("custom rule")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // 未设置策略 → display 首行是默认策略行；原始 [QQ group from ...] 仍完整。
    #[test]
    fn build_envelope_uses_default_policy_line() {
        with_isolated_fixtures(|_root| {
            let display = display_of(&build_envelope(&sample_msg()));
            let expected = format!("[reply policy: {DEFAULT_REPLY_POLICY}]");
            assert_eq!(display.lines().next(), Some(expected.as_str()));
            let has_original = display.contains("[QQ group from 123456 (user 789)]: hello");
            assert!(has_original, "display: {display}");
        });
    }

    // 已设置策略（env 解析到同一 data 目录）→ 首行是配置的策略。
    #[test]
    fn build_envelope_uses_configured_policy_line() {
        with_isolated_fixtures(|root| {
            let data_dir = root.join("data");
            std::fs::create_dir_all(&data_dir).expect("create data dir");
            handle_qq_set_reply_policy(&set_req("Reply to every message in the group.", &data_dir))
                .expect("set policy");
            let display = display_of(&build_envelope(&sample_msg()));
            assert_eq!(
                display.lines().next(),
                Some("[reply policy: Reply to every message in the group.]")
            );
            let has_original = display.contains("[QQ group from 123456 (user 789)]: hello");
            assert!(has_original, "display: {display}");
            // 运行时用 rsplit_once("]: ") 取用户文本（命令路由），
            // 策略行不能破坏它。
            let user_text = display.rsplit_once("]: ").map(|(_, t)| t).unwrap_or("");
            assert_eq!(user_text, "hello");
        });
    }
}

// ═══════════════════════════════════════════════════════════════════════
// WS-4 (B)：群主 / 管理员查询 —— 纯函数聚合 + 真回退路径（本地 OneBot stub）。
// ═══════════════════════════════════════════════════════════════════════
#[cfg(test)]
mod role_tests {
    use super::test_support::with_isolated_fixtures;
    use super::*;

    fn req(value: Value) -> NodeRequest {
        serde_json::from_value(value).expect("well-formed NodeRequest")
    }

    fn roles_req(onebot_url: &str, target: &str) -> NodeRequest {
        req(json!({
            "node_id": "qq_get_group_roles",
            "target": target,
            "payload": {"onebot_url": onebot_url}
        }))
    }

    /// 极简 OneBot HTTP stub：按 group_id / endpoint 回假数据，回满
    /// `requests` 次后线程退出。仅监听 127.0.0.1，不碰真网。
    fn spawn_fake_onebot(requests: usize) -> String {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("bind fake OneBot server");
        let port = server
            .server_addr()
            .to_ip()
            .expect("fake server must listen on an IP socket")
            .port();
        thread::spawn(move || {
            // `incoming_requests()` yields only successful reads, so the loop
            // needs no error arm of its own.
            for mut request in server.incoming_requests().take(requests) {
                let mut body = String::new();
                let _ = request.as_reader().read_to_string(&mut body);
                let params: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
                let gid = params.get("group_id").and_then(Value::as_i64).unwrap_or(0);
                let reply = fake_onebot_reply(request.url(), gid);
                let header =
                    tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..])
                        .expect("static header is valid");
                let response =
                    tiny_http::Response::from_string(reply.to_string()).with_header(header);
                let _ = request.respond(response);
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    fn fake_onebot_reply(url: &str, gid: i64) -> Value {
        if url.ends_with("/get_group_member_list") {
            match gid {
                // 后端带 role：owner/admin 都在成员列表里。
                100 => json!({"status":"ok","retcode":0,"data":[
                    {"user_id":1751557948,"nickname":"群主","card":"群主","role":"owner"},
                    {"user_id":2001,"nickname":"AdminA","card":"","role":"admin"},
                    {"user_id":2002,"nickname":"AdminB","role":"admin"},
                    {"user_id":3001,"nickname":"Member","role":"member"}
                ]}),
                // 后端不带 role：owner 只能从 get_group_info_ex 拿。
                200 => json!({"status":"ok","retcode":0,"data":[
                    {"user_id":2001,"nickname":"AdminA","role":"admin"},
                    {"user_id":3001,"nickname":"Member","role":"member"}
                ]}),
                _ => json!({"status":"ok","retcode":0,"data":[
                    {"user_id":3001,"nickname":"Member","role":"member"}
                ]}),
            }
        } else if gid == 200 {
            json!({"status":"ok","retcode":0,
                "data":{"extInfo":{"groupOwnerId":{"memberUin":1751557948}}}})
        } else {
            // 300: get_group_info_ex 也没有 extInfo → owner 保持 null。
            json!({"status":"ok","retcode":0,"data":{}})
        }
    }

    // 纯函数：1 owner + 2 admin + n member。
    #[test]
    fn summarize_group_roles_aggregates_owner_admins_and_count() {
        let members = json!({"status":"ok","retcode":0,"data":[
            {"user_id":1751557948,"nickname":"群主","card":"群主","role":"owner"},
            {"user_id":2001,"nickname":"AdminA","card":"","role":"admin"},
            {"user_id":2002,"nickname":"AdminB","role":"admin"},
            {"user_id":3001,"nickname":"Member","card":"","role":"member"},
            {"user_id":3002,"nickname":"Member2","role":"member"}
        ]});
        let v = summarize_group_roles(&members);
        assert_eq!(v["owner"]["user_id"], 1751557948);
        assert_eq!(v["owner"]["nickname"], "群主");
        assert_eq!(v["owner"]["card"], "群主");
        let admins = v["admins"].as_array().expect("admins must be an array");
        assert_eq!(admins.len(), 2);
        assert_eq!(admins[0]["user_id"], 2001);
        assert_eq!(admins[1]["user_id"], 2002);
        assert_eq!(v["member_count"], 5);
        assert_eq!(v["source"], "member_list");
    }

    // owner 缺失 → owner 为 null（交给回退路径）；缺 role / 缺 nickname /
    // 缺 user_id 的元素都要容错；裸数组输入也要接受。
    #[test]
    fn summarize_group_roles_tolerates_missing_fields() {
        let members = json!([
            {"user_id":2001,"nickname":"AdminA","role":"admin"},
            {"user_id":2002,"role":"admin"},
            {"nickname":"no user id","role":"admin"},
            {"user_id":3001,"nickname":"Member"}
        ]);
        let v = summarize_group_roles(&members);
        assert!(v["owner"].is_null());
        let admins = v["admins"].as_array().expect("admins must be an array");
        assert_eq!(admins.len(), 2);
        assert!(admins[1]["nickname"].is_null());
        assert!(admins[1]["card"].is_null());
        assert_eq!(v["member_count"], 4);
    }

    // 空数组 / 非数组响应都不能炸。
    #[test]
    fn summarize_group_roles_handles_empty_and_non_array() {
        let empty = summarize_group_roles(&json!({"status":"ok","data":[]}));
        assert!(empty["owner"].is_null());
        assert_eq!(empty["admins"].as_array().map(Vec::len), Some(0));
        assert_eq!(empty["member_count"], 0);
        let not_array = summarize_group_roles(&json!({"status":"failed"}));
        assert_eq!(not_array["member_count"], 0);
    }

    // 成员列表带 role → 直接从 member_list 聚合，不发第二枪。
    #[test]
    fn group_roles_reads_owner_and_admins_from_member_list() {
        let url = spawn_fake_onebot(1);
        let resp = handle_qq_get_group_roles(&roles_req(&url, "group:100")).expect("roles");
        let data = resp.data.expect("data");
        assert_eq!(data["source"], "member_list");
        assert_eq!(data["owner"]["user_id"], 1751557948);
        assert_eq!(data["admins"].as_array().map(Vec::len), Some(2));
        assert_eq!(data["member_count"], 4);
    }

    // 成员列表没 owner → 回退 get_group_info_ex（只拿到 uid）；
    // 连 extInfo 都没有 → owner 保持 null，不编造。
    #[test]
    fn group_roles_falls_back_to_group_info_ex() {
        let url = spawn_fake_onebot(4);
        let resp = handle_qq_get_group_roles(&roles_req(&url, "group:200")).expect("roles");
        let data = resp.data.expect("data");
        assert_eq!(data["source"], "group_info_ex");
        assert_eq!(data["owner"]["user_id"], 1751557948);
        assert!(data["owner"]["nickname"].is_null());
        assert!(data["owner"]["card"].is_null());
        assert_eq!(data["admins"].as_array().map(Vec::len), Some(0));
        assert!(data["member_count"].is_null());

        let no_ext = handle_qq_get_group_roles(&roles_req(&url, "group:300")).expect("roles");
        let data = no_ext.data.expect("data");
        assert_eq!(data["source"], "group_info_ex");
        assert!(data["owner"].is_null());
    }

    // WS-4 只给 qq_get_group_members 加了 agent_accessible 标记：输出必须仍是
    // 原始 OneBot 响应，逐字节行为不变。
    #[test]
    fn group_members_output_stays_raw_onebot_response() {
        let url = spawn_fake_onebot(1);
        let r = req(json!({
            "node_id": "qq_get_group_members",
            "target": "group:100",
            "payload": {"onebot_url": url}
        }));
        let resp = handle_qq_get_group_members(&r).expect("members");
        let data = resp.data.expect("data");
        assert_eq!(data["status"], "ok");
        let members = data["data"].as_array().expect("raw member array");
        assert_eq!(members.len(), 4);
        assert_eq!(members[0]["role"], "owner");
        assert_eq!(members[0]["user_id"], 1751557948);
    }

    // qq_get_group_roles 必须进 handle() 分发（kernel 只经 handle() 调插件）。
    #[test]
    fn handle_dispatches_group_roles() {
        let url = spawn_fake_onebot(1);
        let resp = handle(&roles_req(&url, "group:100")).expect("roles through handle");
        assert_eq!(resp.node_id, "qq_get_group_roles");
        assert_eq!(resp.data.expect("data")["source"], "member_list");
    }

    // target 缺省 / 空白 / private / 非法 id 都拒绝。
    #[test]
    fn group_roles_requires_target_and_group_kind() {
        with_isolated_fixtures(|_root| {
            let absent = req(json!({"node_id":"qq_get_group_roles"}));
            let err = handle_qq_get_group_roles(&absent).expect_err("absent target");
            assert!(err.contains("target is required for qq_get_group_roles"));

            let blank = req(json!({"node_id":"qq_get_group_roles","target":"   "}));
            let err = handle_qq_get_group_roles(&blank).expect_err("blank target");
            assert!(err.contains("target is required for qq_get_group_roles"));

            let private = req(json!({"node_id":"qq_get_group_roles","target":"private:1"}));
            let err = handle_qq_get_group_roles(&private).expect_err("private target");
            assert!(err.contains("target must be group:<id>"));

            let malformed = req(json!({"node_id":"qq_get_group_roles","target":"group:abc"}));
            let err = handle_qq_get_group_roles(&malformed).expect_err("bad group id");
            assert!(err.contains("invalid target id"));
        });
    }

    // payload / STATE / runtime config 三级都没有 URL → 明确报错。
    #[test]
    fn group_roles_without_onebot_url_is_an_error() {
        with_isolated_fixtures(|_root| {
            {
                let mut state = STATE.lock().unwrap_or_else(|p| p.into_inner());
                state.onebot_url = None;
                state.access_token = None;
            }
            let r = req(json!({"node_id":"qq_get_group_roles","target":"group:100"}));
            let err = handle_qq_get_group_roles(&r).expect_err("no url configured");
            assert_eq!(err, "no OneBot URL configured");
        });
    }

    // 传输失败要快速失败（本机无监听端口），不依赖真网。
    #[test]
    fn group_roles_reports_transport_failure() {
        let r = roles_req("http://127.0.0.1:1", "group:100");
        let err = handle_qq_get_group_roles(&r).expect_err("transport failure");
        let is_transport_error = err.contains("HTTP POST");
        assert!(is_transport_error, "err: {err}");
    }
}

// ═══════════════════════════════════════════════════════════════════════
// WS-4 文档表回归：kernel 的 `check_agent_accessible` 只认 docs 里的
// `agent_accessible` 标志（declared_nodes 不参与该判断），所以节点必须同时
// 出现在 docs 且带标志，否则 agent 又被拒成 "not allowed to call ..."。
// ═══════════════════════════════════════════════════════════════════════
#[cfg(test)]
mod docs_tests {
    use super::*;

    fn accessible_ids() -> Vec<String> {
        let mut ids: Vec<String> = Vec::new();
        for node in docs_value().nodes.iter().filter(|n| n.agent_accessible) {
            ids.push(node.id.clone());
        }
        ids
    }

    // 群主/管理员与回复策略节点必须对 agent 开放。
    #[test]
    fn docs_mark_new_nodes_agent_accessible() {
        let ids = accessible_ids();
        for expected in [
            "qq_get_group_members",
            "qq_get_group_roles",
            "qq_get_reply_policy",
            "qq_set_reply_policy",
        ] {
            let found = ids.iter().any(|id| id == expected);
            assert!(
                found,
                "node {expected} must be agent_accessible, have {ids:?}"
            );
        }
    }

    // 门控语义的回归保护：hint 不再硬编码 ONLY-if / When-in-doubt，
    // 而是让模型按每条消息首行的 `[reply policy: ...]` 判断；同时保留
    // 「必须用 qq_send、绝不用 respond、最后 suspend」的既有约束。
    #[test]
    fn system_hint_defers_to_reply_policy_line() {
        let hint = docs_value()
            .system_hint
            .expect("qq must ship a system hint");
        let hardcoded_mention_gate = hint.contains("ONLY if: contains @[id=3832224285]");
        assert!(
            !hardcoded_mention_gate,
            "mention gate must not be hard-coded"
        );
        let hardcoded_doubt_rule = hint.contains("When in doubt, suspend");
        assert!(
            !hardcoded_doubt_rule,
            "'when in doubt' must not be hard-coded"
        );
        let points_at_policy = hint.contains("[reply policy: ...]");
        assert!(
            points_at_policy,
            "hint must point at the per-message policy line"
        );
        let advertises_setter = hint.contains("qq_set_reply_policy");
        assert!(
            advertises_setter,
            "hint must tell the model how to persist a new rule"
        );
        let advertises_roles = hint.contains("qq_get_group_roles");
        assert!(advertises_roles, "hint must advertise the roles node");
        let keeps_respond_ban = hint.contains("NEVER use {\"action\":\"respond\"}");
        assert!(keeps_respond_ban, "the respond ban must stay verbatim");
        let keeps_suspend = hint.contains("then output {\"action\":\"suspend\"}");
        assert!(keeps_suspend, "the suspend requirement must stay verbatim");
        let keeps_send = hint.contains("invoke_plugin(qq, qq_send, ...) to send your reply");
        assert!(keeps_send, "the qq_send requirement must stay verbatim");
    }
}

// ═══════════════════════════════════════════════════════════════════════
// P2-12 回归：runtime-config 路径解析。
//
// `$CORDIS_FIXTURES_ROOT` 仍优先；env 缺失时的回落原先写死成
// `/root/CordisClaw/fixtures/...`，而那在本机是**另一个 checkout**——从
// /root/CordisClaw-minori 起的 runtime 会读写 /root/CordisClaw 的草稿文件。
// 现在回落是 cwd 相对的 `fixtures/.cordis-drafts/...`。
// ═══════════════════════════════════════════════════════════════════════
#[cfg(test)]
mod runtime_config_path_tests {
    use super::test_support::with_isolated_fixtures;
    use super::*;

    #[test]
    fn fixtures_root_env_selects_the_drafts_file() {
        with_isolated_fixtures(|root| {
            let expected = root
                .join("fixtures")
                .join(".cordis-drafts/qq_runtime_config.json");
            assert_eq!(runtime_config_path(), expected);
        });
    }

    #[test]
    fn env_root_path_is_never_the_foreign_checkout() {
        with_isolated_fixtures(|_root| {
            let resolved = runtime_config_path();
            let foreign = resolved.starts_with("/root/CordisClaw/");
            assert!(!foreign, "resolved={}", resolved.display());
        });
    }
}

// ═══════════════════════════════════════════════════════════════════════
// 图片入站：正文里的 `[image: url]` 要原样交给 kernel（多模态 content part）。
// ═══════════════════════════════════════════════════════════════════════
#[cfg(test)]
mod image_url_tests {
    use super::*;

    fn msg(text: &str) -> IncomingMessage {
        IncomingMessage {
            message_type: "group".to_string(),
            sender_id: "123456".to_string(),
            user_id: "789".to_string(),
            message: text.to_string(),
            message_id: Some(1),
            reply_to_msg_id: None,
            raw_event: None,
        }
    }

    #[test]
    fn collect_returns_urls_in_order() {
        let urls = collect_image_urls("前 [image: https://a/1.png] 中 [image: https://a/2.jpg] 后");
        assert_eq!(urls, vec!["https://a/1.png", "https://a/2.jpg"]);
    }

    #[test]
    fn collect_ignores_placeholders_without_url() {
        let urls = collect_image_urls("[image file: abc.image] [image] [sticker]");
        assert!(urls.is_empty());
    }

    #[test]
    fn collect_tolerates_unterminated_and_empty_markers() {
        assert!(collect_image_urls("[image: https://a/1.png").is_empty());
        assert!(collect_image_urls("[image:   ]").is_empty());
        assert!(collect_image_urls("没有图").is_empty());
    }

    // 图片地址必须随 envelope 一起交出去，否则 kernel 根本不知道有图。
    #[test]
    fn build_envelope_carries_image_urls() {
        let env: Value = serde_json::from_str(&build_envelope(&msg(
            "看这个 [image: https://a/1.png]",
        )))
        .unwrap();
        let urls = env["images"].as_array().expect("images array");
        assert_eq!(urls.len(), 1);
        assert_eq!(urls[0], "https://a/1.png");
        let display = env["display"].as_str().unwrap_or_default();
        assert!(display.contains("[image: https://a/1.png]"), "{display}");
    }

    #[test]
    fn build_envelope_without_images_still_has_the_field() {
        let env: Value = serde_json::from_str(&build_envelope(&msg("纯文字"))).unwrap();
        let urls = env["images"].as_array().expect("images array");
        assert!(urls.is_empty());
    }

    // 默认策略必须是"按语境判断"，而不是旧的清单式 ONLY if。
    #[test]
    fn default_policy_is_context_based() {
        let policy = DEFAULT_REPLY_POLICY;
        assert!(policy.contains("conversation context"), "{policy}");
        assert!(policy.contains("even without naming you"), "{policy}");
        assert!(
            !policy.contains("Otherwise stay silent."),
            "旧清单式收尾不应再出现: {policy}"
        );
    }
}
