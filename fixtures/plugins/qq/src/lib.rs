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
use std::collections::VecDeque;
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
}

static STATE: Mutex<QqState> = Mutex::new(QqState {
    onebot_url: None,
    default_target: None,
    allow_groups: Vec::new(),
});

/// Incoming message queue — populated by the HTTP server, drained by
/// `qq_fetch_messages`.
static MESSAGE_QUEUE: Mutex<VecDeque<IncomingMessage>> = Mutex::new(VecDeque::new());

/// Server running flag.
static SERVER_RUNNING: Mutex<bool> = Mutex::new(false);

/// Stored agent session ID for message routing.
static AGENT_SESSION_ID: Mutex<Option<String>> = Mutex::new(None);

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

fn onebot_call(base_url: &str, endpoint: &str, params: &Value) -> Result<Value, String> {
    let url = format!("{}/{}", base_url.trim_end_matches('/'), endpoint);
    let body = serde_json::to_string(params).map_err(|e| format!("json encode: {e}"))?;

    let resp = ureq::post(&url)
        .set("Content-Type", "application/json")
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

fn onebot_send_private_msg(base_url: &str, user_id: i64, message: &str) -> Result<Value, String> {
    let params = json!({ "user_id": user_id, "message": message });
    onebot_call(base_url, "send_private_msg", &params)
}

fn onebot_send_group_msg(base_url: &str, group_id: i64, message: &str) -> Result<Value, String> {
    let params = json!({ "group_id": group_id, "message": message });
    onebot_call(base_url, "send_group_msg", &params)
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
        other => Err(format!("unsupported action: {other}")),
    }
}

fn handle_configure(req: QqRequest) -> Result<QqResponse, String> {
    let mut state = STATE.lock().map_err(|e| format!("lock: {e}"))?;
    if let Some(url) = req.url { state.onebot_url = Some(url); }
    if let Some(target) = req.target { parse_target(&target)?; state.default_target = Some(target); }
    Ok(QqResponse {
        ok: true, action: "configure".to_string(),
        message: Some(format!(
            "url={} target={}",
            state.onebot_url.as_deref().unwrap_or("(unchanged)"),
            state.default_target.as_deref().unwrap_or("(unchanged)")
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
        TargetKind::Group => onebot_send_group_msg(&base_url, id, &message)?,
        TargetKind::Private => onebot_send_private_msg(&base_url, id, &message)?,
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
        onebot_call(url, "get_status", &json!({})).is_ok()
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
    let data = onebot_call(&base_url, endpoint, &params)?;
    Ok(QqResponse { ok: true, action: "call".to_string(), message: None, data: Some(data) })
}

// ---------------------------------------------------------------------------
// HTTP Server — receives OneBot event POSTs
// ---------------------------------------------------------------------------

fn start_event_server(port: u16) -> Result<(), String> {
    let server = tiny_http::Server::http(format!("0.0.0.0:{port}"))
        .map_err(|e| format!("qq_serve: cannot bind port {port}: {e}"))?;

    *SERVER_RUNNING.lock().map_err(|e| format!("lock: {e}"))? = true;

    for mut request in server.incoming_requests() {
        if request.url() == "/onebot/event" && request.method() == &tiny_http::Method::Post {
            let mut body = String::new();
            if let Ok(_) = request.as_reader().read_to_string(&mut body) {
                let _ = request.respond(tiny_http::Response::from_string("ok"));
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

    Ok(())
}

fn handle_onebot_event(event: &OneBotEvent) {
    if event.post_type != "message" {
        return;
    }

    // Extract message text (OneBot can send message as string or array of segments).
    let message_text = extract_message_text(&event.message, event.raw_message.as_deref());

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

    let msg = IncomingMessage {
        message_type: msg_type,
        sender_id,
        user_id,
        message: message_text,
        raw_event: Some(serde_json::to_value(event).unwrap_or_default()),
    };

    if let Ok(mut queue) = MESSAGE_QUEUE.lock() {
        if queue.len() < 128 {
            queue.push_back(msg);
        }
    }
}

fn extract_message_text(message: &Value, raw_message: Option<&str>) -> String {
    // Prefer raw_message if available.
    if let Some(raw) = raw_message {
        if !raw.is_empty() {
            return raw.to_string();
        }
    }
    match message {
        Value::String(s) => s.clone(),
        Value::Array(segments) => {
            segments.iter()
                .filter_map(|seg| seg.get("data").and_then(|d| d.get("text")).and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("")
        }
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// New node handlers
// ---------------------------------------------------------------------------

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

    // Store configuration.
    {
        let mut state = STATE.lock().map_err(|e| format!("lock: {e}"))?;
        state.allow_groups = allow_groups.clone();
        if let Some(url) = req.payload.as_ref().and_then(|p| p.get("onebot_url")).and_then(|v| v.as_str()) {
            state.onebot_url = Some(url.to_string());
        }
    }

    // Store agent session ID if provided.
    if let Some(ref sid) = req.agent_session_id {
        *AGENT_SESSION_ID.lock().map_err(|e| format!("lock: {e}"))? = Some(sid.clone());
    }

    // Start HTTP server in background thread.
    let running = *SERVER_RUNNING.lock().map_err(|e| format!("lock: {e}"))?;
    if !running {
        thread::spawn(move || {
            if let Err(e) = start_event_server(port) {
                eprintln!("qq_serve HTTP server error: {e}");
            }
        });
        // Give the server a moment to start.
        thread::sleep(std::time::Duration::from_millis(100));
    }

    Ok(NodeResponse {
        ok: true,
        node_id: "qq_serve".to_string(),
        messages: None,
        message: Some(format!(
            "HTTP server listening on port {port}, allow_groups={:?}",
            allow_groups
        )),
        data: None,
        error: None,
    })
}

fn handle_qq_fetch_messages() -> Result<NodeResponse, String> {
    let messages: Vec<IncomingMessage> = {
        let mut queue = MESSAGE_QUEUE.lock().map_err(|e| format!("lock: {e}"))?;
        queue.drain(..).collect()
    };
    Ok(NodeResponse {
        ok: true,
        node_id: "qq_fetch_messages".to_string(),
        messages: Some(messages),
        message: None,
        data: None,
        error: None,
    })
}

fn handle_qq_send(req: &NodeRequest) -> Result<NodeResponse, String> {
    let target = req.target.as_deref().unwrap_or("").trim();
    let message = req.message.as_deref().unwrap_or("").trim();

    if target.is_empty() { return Err("target is required for qq_send".to_string()); }
    if message.is_empty() { return Err("message is required for qq_send".to_string()); }

    let (kind, id) = parse_target(target)?;
    let base_url = {
        let state = STATE.lock().map_err(|e| format!("lock: {e}"))?;
        state.onebot_url.clone().ok_or("no OneBot URL configured; use configure first")?
    };

    let data = match kind {
        TargetKind::Group => onebot_send_group_msg(&base_url, id, message)?,
        TargetKind::Private => onebot_send_private_msg(&base_url, id, message)?,
    };
    let msg_id = data.get("data").and_then(|d| d.get("message_id")).and_then(|v| v.as_i64());

    Ok(NodeResponse {
        ok: true,
        node_id: "qq_send".to_string(),
        messages: None,
        message: msg_id.map(|mid| format!("message_id={mid}")),
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
            ),
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
                "Send a message to a QQ group or private chat via OneBot v11 HTTP API.",
                json!({
                    "type": "object", "required": ["node_id", "target", "message"],
                    "properties": {
                        "node_id": { "type": "string", "const": "qq_send" },
                        "target": { "type": "string", "description": "group:<id> or private:<id>" },
                        "message": { "type": "string", "description": "Message text to send" }
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
            ),
        ],
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
