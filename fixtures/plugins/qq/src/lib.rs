//! QQ adapter plugin using the NoneBot (OneBot v11) protocol.
//!
//! This plugin communicates with a OneBot-compatible QQ client
//! (e.g. go-cqhttp, NapCat, LLOneBot) via its HTTP API.
//!
//! Supported actions:
//! - `configure`  — set the OneBot HTTP endpoint URL
//! - `send`       — send a message to a group or private chat
//! - `status`     — report current configuration and connectivity
//! - `call`       — call an arbitrary OneBot API action

use cordis_plugin_sdk::{
    export_plugin_api, json_response, node_doc, plugin_docs, AbiFingerprint, PluginRequest,
    PluginResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Mutex;

// ---------------------------------------------------------------------------
// Plugin state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
struct QqState {
    /// OneBot HTTP API base URL, e.g. "http://127.0.0.1:5700"
    onebot_url: Option<String>,
    /// Default target for send actions, e.g. "group:123456" or "private:789012"
    default_target: Option<String>,
}

static STATE: Mutex<QqState> = Mutex::new(QqState {
    onebot_url: None,
    default_target: None,
});

// ---------------------------------------------------------------------------
// Request / Response types
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

    /// Raw JSON payload for generic `call` action
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
// OneBot v11 HTTP API helpers
// ---------------------------------------------------------------------------

/// Call a OneBot HTTP API endpoint and return the parsed JSON body.
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

    // OneBot v11 returns { "status": "ok"/"failed", "retcode": ..., "data": ... }
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

/// Send a private message via OneBot.
fn onebot_send_private_msg(base_url: &str, user_id: i64, message: &str) -> Result<Value, String> {
    let params = json!({
        "user_id": user_id,
        "message": message,
    });
    onebot_call(base_url, "send_private_msg", &params)
}

/// Send a group message via OneBot.
fn onebot_send_group_msg(base_url: &str, group_id: i64, message: &str) -> Result<Value, String> {
    let params = json!({
        "group_id": group_id,
        "message": message,
    });
    onebot_call(base_url, "send_group_msg", &params)
}

/// Parse a target string like "group:123456" or "private:789012".
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    Group,
    Private,
}

// ---------------------------------------------------------------------------
// Plugin logic
// ---------------------------------------------------------------------------

fn handle_request(req: QqRequest) -> Result<QqResponse, String> {
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

    if let Some(url) = req.url {
        state.onebot_url = Some(url);
    }
    if let Some(target) = req.target {
        // Validate the target format eagerly
        parse_target(&target)?;
        state.default_target = Some(target);
    }

    Ok(QqResponse {
        ok: true,
        action: "configure".to_string(),
        message: Some(format!(
            "url={} target={}",
            state.onebot_url.as_deref().unwrap_or("(unchanged)"),
            state.default_target.as_deref().unwrap_or("(unchanged)")
        )),
        data: None,
    })
}

fn handle_send(req: QqRequest) -> Result<QqResponse, String> {
    let message = req
        .message
        .as_deref()
        .unwrap_or("")
        .trim()
        .to_string();
    if message.is_empty() {
        return Err("message is empty".to_string());
    }

    let (kind, id) = {
        let state = STATE.lock().map_err(|e| format!("lock: {e}"))?;
        let target_str = req
            .target
            .as_deref()
            .or(state.default_target.as_deref())
            .ok_or_else(|| {
                "no target configured; use 'configure' first or provide a 'target' field".to_string()
            })?;
        parse_target(target_str)?
    };

    let base_url = {
        let state = STATE.lock().map_err(|e| format!("lock: {e}"))?;
        state
            .onebot_url
            .clone()
            .ok_or_else(|| "no OneBot URL configured; use 'configure' first".to_string())?
    };

    let data = match kind {
        TargetKind::Group => onebot_send_group_msg(&base_url, id, &message)?,
        TargetKind::Private => onebot_send_private_msg(&base_url, id, &message)?,
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
        // Try a lightweight call to check connectivity
        match onebot_call(url, "get_status", &json!({})) {
            Ok(_) => true,
            Err(_) => false,
        }
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
    let payload = req.payload.ok_or_else(|| "missing 'payload' for call action".to_string())?;

    let endpoint = payload
        .get("endpoint")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "payload must contain 'endpoint' string".to_string())?;

    let params = payload.get("params").cloned().unwrap_or(json!({}));

    let base_url = {
        let state = STATE.lock().map_err(|e| format!("lock: {e}"))?;
        state
            .onebot_url
            .clone()
            .ok_or_else(|| "no OneBot URL configured; use 'configure' first".to_string())?
    };

    let data = onebot_call(&base_url, endpoint, &params)?;

    Ok(QqResponse {
        ok: true,
        action: "call".to_string(),
        message: None,
        data: Some(data),
    })
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
        vec![node_doc(
            "qq_entry",
            "QQ adapter using the NoneBot (OneBot v11) protocol. \
             Connect to a OneBot-compatible QQ client for sending messages.",
            json!({
                "type": "object",
                "required": ["action"],
                "properties": {
                    "action": { "type": "string", "description": "configure | send | status | call" },
                    "url": { "type": "string", "description": "OneBot HTTP API base URL (for configure)" },
                    "target": { "type": "string", "description": "group:<id> or private:<id> (for configure / send)" },
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
            &[
                "no OneBot URL configured",
                "invalid target format",
                "message is empty",
                "OneBot API error",
                "unsupported action"
            ],
        )],
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
    let parsed = serde_json::from_str::<QqRequest>(&req.payload)
        .map_err(|e| format!("qq plugin invalid request: {e}"));

    match parsed.and_then(handle_request) {
        Ok(resp) => json_response(&resp),
        Err(message) => {
            let resp = QqResponse {
                ok: false,
                action: "error".to_string(),
                message: Some(message),
                data: None,
            };
            json_response(&resp)
        }
    }
}

export_plugin_api! {
    abi_fingerprint = abi_fingerprint_value(),
    docs = docs_value(),
    handle = api_handle,
}
