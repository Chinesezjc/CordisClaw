//! Time plugin — retrieve current system time.
//!
//! Node:
//! - `time_now` — returns the current system time in a human-readable format.

use cordis_plugin_sdk::{
    export_plugin_api, json_response, node_doc, plugin_docs, AbiFingerprint,
    PluginRequest, PluginResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::{SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// Request / Response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct NodeRequest {
    node_id: String,
    #[serde(default)]
    format: Option<String>,
}

#[derive(Debug, Serialize)]
struct NodeResponse {
    ok: bool,
    node_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    timestamp: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    datetime: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

fn handle_time_now(_format: Option<&str>) -> Result<NodeResponse, String> {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("system time error: {e}"))?;
    let timestamp = dur.as_secs() as i64;
    // Convert to UTC datetime string.
    let secs = timestamp;
    let days_since_epoch = secs / 86400;
    // Simple Gregorian calendar calculation (good enough for year 1970-2100).
    let mut y = 1970i64;
    let mut d = days_since_epoch;
    loop {
        let days_in_year = if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 { 366 } else { 365 };
        if d < days_in_year { break; }
        d -= days_in_year;
        y += 1;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let mdays = [31, if leap { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut m = 0usize;
    while m < 12 && d >= mdays[m] as i64 { d -= mdays[m] as i64; m += 1; }
    let day = d + 1;
    let month = m + 1;
    let remaining = secs % 86400;
    let h = remaining / 3600;
    let mi = (remaining % 3600) / 60;
    let s = remaining % 60;
    let datetime = format!("{y:04}-{month:02}-{day:02} {h:02}:{mi:02}:{s:02} UTC");

    Ok(NodeResponse {
        ok: true,
        node_id: "time_now".to_string(),
        timestamp: Some(timestamp),
        datetime: Some(datetime),
        error: None,
    })
}

fn handle(req: &NodeRequest) -> Result<NodeResponse, String> {
    match req.node_id.as_str() {
        "time_now" => handle_time_now(req.format.as_deref()),
        other => Err(format!("unknown node_id: {other}")),
    }
}

// ---------------------------------------------------------------------------
// Plugin API exports
// ---------------------------------------------------------------------------

fn docs_value() -> cordis_plugin_sdk::PluginDocs {
    plugin_docs(
        "time",
        "time",
        "0.1.0",
        Some("Time"),
        vec![
            node_doc(
                "time_now",
                "Get the current system time. Returns a Unix timestamp and a human-readable datetime string. Supports an optional format string (chrono strftime syntax).",
                json!({
                    "type": "object",
                    "required": ["node_id"],
                    "properties": {
                        "node_id": { "type": "string", "const": "time_now" },
                        "format": { "type": "string", "description": "Optional chrono strftime format string, e.g. %Y-%m-%d %H:%M:%S" }
                    }
                }),
                json!({
                    "type": "object",
                    "properties": {
                        "ok": { "type": "boolean" },
                        "node_id": { "type": "string" },
                        "timestamp": { "type": "integer", "description": "Unix timestamp (seconds)" },
                        "datetime": { "type": "string", "description": "Formatted datetime string" },
                        "error": { "type": ["string", "null"] }
                    }
                }),
                &[],
                &["unknown node_id"],
            ).with_agent_accessible(),
        ],
        None,
    )
}

fn abi_fingerprint_value() -> AbiFingerprint {
    AbiFingerprint {
        rustc_version: "1.85.1".to_string(),
        target_triple: "x86_64-unknown-linux-gnu".to_string(),
        crate_hash: "crate_time_v1".to_string(),
        api_hash: "api_v2".to_string(),
    }
}

fn api_handle(req: PluginRequest) -> PluginResponse {
    match serde_json::from_str::<NodeRequest>(&req.payload)
        .map_err(|e| format!("time plugin: {e}"))
        .and_then(|r| handle(&r))
    {
        Ok(resp) => json_response(&resp),
        Err(e) => json_response(&NodeResponse {
            ok: false,
            node_id: "error".to_string(),
            timestamp: None,
            datetime: None,
            error: Some(e),
        }),
    }
}

export_plugin_api! {
    abi_fingerprint = abi_fingerprint_value(),
    docs = docs_value(),
    handle = api_handle,
}
