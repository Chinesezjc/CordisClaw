//! Feishu (Lark) plugin — OneBot-style protocol adapter for Feishu open
//! platform, structurally analogous to the `qq` plugin.
//!
//! Nodes:
//! - `feishu_serve` (Task) — HTTP server on :8100 receiving Feishu event
//!   subscription callbacks at `POST /feishu/event`. Handles the
//!   url_verification challenge, (optional) AES decryption, token check,
//!   dedup, @-mention gating, then enqueues messages and a poller emits
//!   structured envelopes to the runtime agent via `agent_trigger`.
//! - `feishu_send` (Router) — outbound: send text / interactive card,
//!   optionally as a quote-reply, using a cached tenant_access_token.
//! - `feishu_entry` (Router) — configure (persist app_id/secret/tokens).

use cordis_plugin_sdk::{
    export_plugin_api, json_response, node_doc, plugin_docs, task_node_doc, AbiFingerprint,
    PluginRequest, PluginResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::thread;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Global state
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FeishuState {
    app_id: Option<String>,
    app_secret: Option<String>,
    /// Event-subscription verification token (checked on every callback).
    verification_token: Option<String>,
    /// AES key for encrypted event mode; None = plaintext events.
    encrypt_key: Option<String>,
    /// The bot's own open_id, used for @-mention gating in groups.
    bot_open_id: Option<String>,
    /// Feishu API base, overridable for tests (default open.feishu.cn).
    api_base: Option<String>,
}

static STATE: Mutex<FeishuState> = Mutex::new(FeishuState {
    app_id: None,
    app_secret: None,
    verification_token: None,
    encrypt_key: None,
    bot_open_id: None,
    api_base: None,
});

/// Cached tenant_access_token: (token, expires_at). Refreshed 60s early.
static TENANT_TOKEN: Mutex<Option<(String, Instant)>> = Mutex::new(None);

static MESSAGE_QUEUE: LazyLock<Mutex<VecDeque<IncomingMessage>>> =
    LazyLock::new(|| Mutex::new(VecDeque::new()));
const MESSAGE_QUEUE_CAP: usize = 128;
static MESSAGE_QUEUE_DROPPED: AtomicU64 = AtomicU64::new(0);

/// FIFO dedup by message_id (Feishu may redeliver on non-2xx / retry).
static RECENT_MESSAGE_IDS: LazyLock<Mutex<VecDeque<String>>> =
    LazyLock::new(|| Mutex::new(VecDeque::new()));
const RECENT_IDS_CAP: usize = 200;

static SERVER_RUNNING: Mutex<bool> = Mutex::new(false);
static SERVER_SHUTDOWN: AtomicBool = AtomicBool::new(false);
static EVENT_LOOP_HANDLE: Mutex<Option<thread::JoinHandle<()>>> = Mutex::new(None);
static POLLER_STARTED: AtomicBool = AtomicBool::new(false);

const DEFAULT_API_BASE: &str = "https://open.feishu.cn";
const DEFAULT_PORT: u16 = 8100;

// ---------------------------------------------------------------------------
// A queued inbound message
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct IncomingMessage {
    /// "group" | "p2p"
    chat_type: String,
    /// Feishu chat_id (oc_...) — the reply target.
    chat_id: String,
    /// Sender open_id (ou_...).
    open_id: String,
    /// Extracted plain text.
    text: String,
    /// message_id (om_...) for dedup + quote-reply.
    message_id: String,
    /// thread root for threaded replies (root_id), if any.
    root_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Node request / response
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct NodeRequest {
    node_id: String,
    #[serde(default)]
    action: Option<String>,
    /// "chat:oc_xxx" | "user:ou_xxx"
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    message: Option<String>,
    /// message_id to quote-reply to.
    #[serde(default)]
    reply_to: Option<String>,
    /// interactive card JSON (when sending a card instead of text).
    #[serde(default)]
    card: Option<Value>,
    #[serde(default)]
    payload: Option<Value>,
}

#[derive(Debug, Serialize)]
struct NodeResponse {
    ok: bool,
    node_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl NodeResponse {
    fn err(node_id: &str, e: impl Into<String>) -> Self {
        NodeResponse { ok: false, node_id: node_id.to_string(), message: None, data: None, error: Some(e.into()) }
    }
}

// ---------------------------------------------------------------------------
// Runtime config persistence (analogous to qq; 0600 for secrets)
// ---------------------------------------------------------------------------

fn runtime_config_path() -> PathBuf {
    match std::env::var("CORDIS_FIXTURES_ROOT") {
        Ok(root) if !root.is_empty() => {
            PathBuf::from(root).join(".cordis-drafts/feishu_runtime_config.json")
        }
        _ => PathBuf::from("fixtures/.cordis-drafts/feishu_runtime_config.json"),
    }
}

fn load_runtime_config() -> Option<Value> {
    let path = runtime_config_path();
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn save_runtime_config(config: &Value) -> Result<(), String> {
    let path = runtime_config_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let bytes = serde_json::to_string_pretty(config).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        // P0-24 style: create with 0600 so secrets are never world-readable.
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .map_err(|e| e.to_string())?;
        f.write_all(bytes.as_bytes()).map_err(|e| e.to_string())?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(&path, bytes.as_bytes()).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Read a config value with three-level fallback: request payload → STATE → file.
fn config_str(payload: Option<&Value>, key: &str, state_get: impl Fn(&FeishuState) -> Option<String>) -> Option<String> {
    payload
        .and_then(|p| p.get(key))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| STATE.lock().ok().and_then(|s| state_get(&s)))
        .or_else(|| load_runtime_config().and_then(|c| c.get(key)?.as_str().map(|s| s.to_string())))
}

// ---------------------------------------------------------------------------
// Inbound: challenge, token check, AES decrypt, event parse
// ---------------------------------------------------------------------------

/// AES-256-CBC decrypt a Feishu encrypted event body.
/// key = SHA256(encrypt_key); IV = first 16 bytes of ciphertext.
fn decrypt_feishu(encrypt_key: &str, b64_cipher: &str) -> Result<String, String> {
    use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
    use base64::Engine;
    use sha2::{Digest, Sha256};

    let cipher = base64::engine::general_purpose::STANDARD
        .decode(b64_cipher.trim())
        .map_err(|e| format!("base64 decode failed: {e}"))?;
    if cipher.len() < 16 {
        return Err("ciphertext too short".to_string());
    }
    let key = Sha256::digest(encrypt_key.as_bytes());
    let (iv, data) = cipher.split_at(16);

    type Dec = cbc::Decryptor<aes::Aes256>;
    let dec = Dec::new_from_slices(&key, iv).map_err(|e| format!("aes init: {e}"))?;
    let mut buf = data.to_vec();
    let plain = dec
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|e| format!("aes decrypt: {e}"))?;
    String::from_utf8(plain.to_vec()).map_err(|e| format!("utf8: {e}"))
}

/// Result of interpreting an inbound HTTP body.
enum InboundOutcome {
    /// url_verification handshake — respond with this exact JSON body.
    Challenge(String),
    /// A message to enqueue.
    Message(IncomingMessage),
    /// A card button action — treated as a fresh inbound message.
    CardAction(IncomingMessage),
    /// Valid but not actionable (non-message event, wrong token, etc.).
    Ignore(String),
    /// Token/verification rejected — respond 401.
    Rejected(String),
}

/// Parse a raw HTTP body into an outcome. `verification_token`/`encrypt_key`
/// come from config; `bot_open_id` gates group @-mentions.
fn interpret_inbound(
    raw_body: &str,
    verification_token: Option<&str>,
    encrypt_key: Option<&str>,
    bot_open_id: Option<&str>,
) -> InboundOutcome {
    // If encrypted, the outer body is {"encrypt": "..."} — decrypt first.
    let body: String = match serde_json::from_str::<Value>(raw_body) {
        Ok(v) => {
            if let Some(enc) = v.get("encrypt").and_then(|e| e.as_str()) {
                match encrypt_key {
                    Some(k) => match decrypt_feishu(k, enc) {
                        Ok(plain) => plain,
                        Err(e) => return InboundOutcome::Rejected(format!("decrypt failed: {e}")),
                    },
                    None => return InboundOutcome::Rejected("encrypted event but no encrypt_key configured".into()),
                }
            } else {
                raw_body.to_string()
            }
        }
        Err(e) => return InboundOutcome::Rejected(format!("body not JSON: {e}")),
    };

    let v: Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => return InboundOutcome::Rejected(format!("decrypted body not JSON: {e}")),
    };

    // url_verification handshake (schema 1.0 style: type at top level).
    if v.get("type").and_then(|t| t.as_str()) == Some("url_verification") {
        // token is at top level for the handshake.
        if let Some(expected) = verification_token {
            let got = v.get("token").and_then(|t| t.as_str()).unwrap_or("");
            if got != expected {
                return InboundOutcome::Rejected("url_verification token mismatch".into());
            }
        }
        let challenge = v.get("challenge").and_then(|c| c.as_str()).unwrap_or("");
        return InboundOutcome::Challenge(json!({ "challenge": challenge }).to_string());
    }

    // Event schema 2.0: {"header": {...}, "event": {...}}.
    // Token lives in header.token.
    if let Some(expected) = verification_token {
        let got = v
            .get("header")
            .and_then(|h| h.get("token"))
            .and_then(|t| t.as_str())
            .or_else(|| v.get("token").and_then(|t| t.as_str()))
            .unwrap_or("");
        if got != expected {
            return InboundOutcome::Rejected("event token mismatch".into());
        }
    }

    // Card action callback: {"type":"...","action":{...},"open_id":...} or
    // schema-2.0 card.action.trigger. We accept a value blob under
    // event.action.value.text (our own convention when building cards).
    let event_type = v
        .get("header")
        .and_then(|h| h.get("event_type"))
        .and_then(|t| t.as_str())
        .unwrap_or("");

    if event_type == "card.action.trigger" || v.get("action").is_some() {
        if let Some(ic) = parse_card_action(&v) {
            return InboundOutcome::CardAction(ic);
        }
        return InboundOutcome::Ignore("card action without actionable value".into());
    }

    if event_type != "im.message.receive_v1" {
        return InboundOutcome::Ignore(format!("ignored event_type: {event_type}"));
    }

    match parse_message_event(&v, bot_open_id) {
        Some(Ok(im)) => InboundOutcome::Message(im),
        Some(Err(reason)) => InboundOutcome::Ignore(reason),
        None => InboundOutcome::Ignore("malformed message event".into()),
    }
}

/// Parse an `im.message.receive_v1` event. Returns:
/// - Some(Ok(msg)) when actionable,
/// - Some(Err(reason)) when a valid message we deliberately skip (e.g. group
///   message that doesn't @ the bot),
/// - None when structurally malformed.
fn parse_message_event(v: &Value, bot_open_id: Option<&str>) -> Option<Result<IncomingMessage, String>> {
    let event = v.get("event")?;
    let message = event.get("message")?;
    let chat_id = message.get("chat_id")?.as_str()?.to_string();
    let chat_type = message.get("chat_type").and_then(|c| c.as_str()).unwrap_or("group").to_string();
    let message_id = message.get("message_id")?.as_str()?.to_string();
    let root_id = message.get("root_id").and_then(|r| r.as_str()).map(|s| s.to_string());
    let open_id = event
        .get("sender")
        .and_then(|s| s.get("sender_id"))
        .and_then(|s| s.get("open_id"))
        .and_then(|o| o.as_str())
        .unwrap_or("")
        .to_string();

    // content is a JSON string; text messages carry {"text":"..."}.
    let content_raw = message.get("content").and_then(|c| c.as_str()).unwrap_or("{}");
    let content: Value = serde_json::from_str(content_raw).unwrap_or(Value::Null);
    let text = content
        .get("text")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .to_string();

    // @-mention gating for group chats. p2p (direct) is always actionable.
    if chat_type == "group" {
        let mentioned = message
            .get("mentions")
            .and_then(|m| m.as_array())
            .map(|arr| {
                arr.iter().any(|m| {
                    let mid = m
                        .get("id")
                        .and_then(|i| i.get("open_id"))
                        .and_then(|o| o.as_str());
                    match (mid, bot_open_id) {
                        // If we know our open_id, require an exact match.
                        (Some(mid), Some(bot)) => mid == bot,
                        // If bot_open_id unknown, any @ counts (best-effort).
                        (Some(_), None) => true,
                        _ => false,
                    }
                })
            })
            .unwrap_or(false);
        if !mentioned {
            return Some(Err("group message without @bot; skipped".to_string()));
        }
    }

    // Strip @-mention placeholder text like "@_user_1 " that Feishu leaves.
    let cleaned = strip_mention_tokens(&text);

    Some(Ok(IncomingMessage {
        chat_type,
        chat_id,
        open_id,
        text: cleaned,
        message_id,
        root_id,
    }))
}

/// Feishu leaves "@_user_N" placeholders in text where mentions were; drop them.
fn strip_mention_tokens(text: &str) -> String {
    text.split_whitespace()
        .filter(|tok| !tok.starts_with("@_user_") && !tok.starts_with("@_all"))
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
}

/// Parse a card button action into a synthetic inbound message. Our cards
/// embed `{"value": {"text": "...", "chat_id": "..."}}` on buttons.
fn parse_card_action(v: &Value) -> Option<IncomingMessage> {
    let action = v.get("event").and_then(|e| e.get("action")).or_else(|| v.get("action"))?;
    let value = action.get("value")?;
    let text = value.get("text").and_then(|t| t.as_str())?.to_string();
    let chat_id = value
        .get("chat_id")
        .and_then(|c| c.as_str())
        .or_else(|| v.get("open_chat_id").and_then(|c| c.as_str()))
        .unwrap_or("")
        .to_string();
    if chat_id.is_empty() {
        return None;
    }
    let open_id = v
        .get("event")
        .and_then(|e| e.get("operator"))
        .and_then(|o| o.get("open_id"))
        .and_then(|o| o.as_str())
        .or_else(|| v.get("open_id").and_then(|o| o.as_str()))
        .unwrap_or("")
        .to_string();
    Some(IncomingMessage {
        chat_type: "group".to_string(),
        chat_id,
        open_id,
        text,
        message_id: format!("cardaction:{}", value.get("text").and_then(|t| t.as_str()).unwrap_or("")),
        root_id: None,
    })
}

// ---------------------------------------------------------------------------
// Outbound: tenant_access_token cache + send message / card / reply
// ---------------------------------------------------------------------------

fn api_base() -> String {
    STATE
        .lock()
        .ok()
        .and_then(|s| s.api_base.clone())
        .or_else(|| load_runtime_config().and_then(|c| c.get("api_base")?.as_str().map(|s| s.to_string())))
        .unwrap_or_else(|| DEFAULT_API_BASE.to_string())
}

/// Fetch (or return cached) tenant_access_token. Refreshes 60s before expiry.
fn tenant_access_token(app_id: &str, app_secret: &str) -> Result<String, String> {
    {
        let cache = TENANT_TOKEN.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((tok, exp)) = cache.as_ref() {
            if Instant::now() < *exp {
                return Ok(tok.clone());
            }
        }
    }
    let url = format!("{}/open-apis/auth/v3/tenant_access_token/internal", api_base());
    let resp = ureq::post(&url)
        .set("Content-Type", "application/json; charset=utf-8")
        .send_json(json!({ "app_id": app_id, "app_secret": app_secret }))
        .map_err(|e| format!("tenant token request failed: {e}"))?;
    let body: Value = resp.into_json().map_err(|e| format!("tenant token parse: {e}"))?;
    let code = body.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        return Err(format!("tenant token error code={code}: {}", body.get("msg").and_then(|m| m.as_str()).unwrap_or("")));
    }
    let token = body
        .get("tenant_access_token")
        .and_then(|t| t.as_str())
        .ok_or("no tenant_access_token in response")?
        .to_string();
    let expire = body.get("expire").and_then(|e| e.as_u64()).unwrap_or(7200);
    let refresh_at = Instant::now() + Duration::from_secs(expire.saturating_sub(60));
    {
        let mut cache = TENANT_TOKEN.lock().unwrap_or_else(|p| p.into_inner());
        *cache = Some((token.clone(), refresh_at));
    }
    Ok(token)
}

/// "chat:oc_xxx" / "user:ou_xxx" → (receive_id_type, receive_id).
fn parse_target(target: &str) -> Result<(&'static str, String), String> {
    let (kind, id) = target
        .split_once(':')
        .ok_or_else(|| format!("invalid target (expected chat:<id> or user:<id>): {target}"))?;
    match kind.trim() {
        "chat" | "c" => Ok(("chat_id", id.trim().to_string())),
        "user" | "u" | "open_id" => Ok(("open_id", id.trim().to_string())),
        other => Err(format!("unknown target kind: {other}")),
    }
}

fn feishu_send_message(
    token: &str,
    receive_id_type: &str,
    receive_id: &str,
    msg_type: &str,
    content: &str,
    reply_to: Option<&str>,
) -> Result<Value, String> {
    let base = api_base();
    let (url, body) = match reply_to {
        Some(mid) if !mid.is_empty() && !mid.starts_with("cardaction:") => {
            // Quote-reply endpoint.
            (
                format!("{base}/open-apis/im/v1/messages/{mid}/reply"),
                json!({ "msg_type": msg_type, "content": content }),
            )
        }
        _ => (
            format!("{base}/open-apis/im/v1/messages?receive_id_type={receive_id_type}"),
            json!({ "receive_id": receive_id, "msg_type": msg_type, "content": content }),
        ),
    };
    let resp = ureq::post(&url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json; charset=utf-8")
        .send_json(body)
        .map_err(|e| format!("feishu send failed: {e}"))?;
    let parsed: Value = resp.into_json().map_err(|e| format!("feishu send parse: {e}"))?;
    let code = parsed.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        return Err(format!("feishu send error code={code}: {}", parsed.get("msg").and_then(|m| m.as_str()).unwrap_or("")));
    }
    Ok(parsed)
}

// ---------------------------------------------------------------------------
// Node handlers
// ---------------------------------------------------------------------------

fn handle_feishu_send(req: &NodeRequest) -> Result<NodeResponse, String> {
    let target = req.target.as_deref().unwrap_or("").trim();
    if target.is_empty() {
        return Err("target is required for feishu_send (chat:<id> or user:<id>)".to_string());
    }
    let app_id = config_str(req.payload.as_ref(), "app_id", |s| s.app_id.clone())
        .ok_or("no app_id configured")?;
    let app_secret = config_str(req.payload.as_ref(), "app_secret", |s| s.app_secret.clone())
        .ok_or("no app_secret configured")?;
    let token = tenant_access_token(&app_id, &app_secret)?;
    let (recv_type, recv_id) = parse_target(target)?;

    let (msg_type, content) = if let Some(card) = &req.card {
        ("interactive", card.to_string())
    } else {
        let message = req.message.as_deref().unwrap_or("").trim();
        if message.is_empty() {
            return Err("message or card is required for feishu_send".to_string());
        }
        ("text", json!({ "text": message }).to_string())
    };

    let data = feishu_send_message(
        &token,
        recv_type,
        &recv_id,
        msg_type,
        &content,
        req.reply_to.as_deref(),
    )?;
    Ok(NodeResponse { ok: true, node_id: "feishu_send".to_string(), message: Some("sent".to_string()), data: Some(data), error: None })
}

fn handle_feishu_entry(req: &NodeRequest) -> Result<NodeResponse, String> {
    match req.action.as_deref() {
        Some("configure") => {
            let payload = req.payload.clone().unwrap_or(Value::Null);
            // Merge into existing config so partial updates work.
            let mut config = load_runtime_config().unwrap_or_else(|| json!({}));
            for key in ["app_id", "app_secret", "verification_token", "encrypt_key", "bot_open_id", "api_base"] {
                if let Some(val) = payload.get(key).and_then(|v| v.as_str()) {
                    config[key] = json!(val);
                }
            }
            save_runtime_config(&config)?;
            // Refresh in-memory STATE too.
            if let Ok(mut s) = STATE.lock() {
                apply_config_to_state(&mut s, &config);
            }
            Ok(NodeResponse { ok: true, node_id: "feishu_entry".to_string(), message: Some("configured".to_string()), data: None, error: None })
        }
        Some("status") => {
            let s = STATE.lock().map_err(|e| format!("lock: {e}"))?;
            let data = json!({
                "app_id_set": s.app_id.is_some(),
                "verification_token_set": s.verification_token.is_some(),
                "encrypt_key_set": s.encrypt_key.is_some(),
                "bot_open_id_set": s.bot_open_id.is_some(),
                "server_running": SERVER_RUNNING.lock().map(|g| *g).unwrap_or(false),
                "queue_dropped": MESSAGE_QUEUE_DROPPED.load(Ordering::Relaxed),
            });
            Ok(NodeResponse { ok: true, node_id: "feishu_entry".to_string(), message: None, data: Some(data), error: None })
        }
        other => Err(format!("unknown feishu_entry action: {other:?}")),
    }
}

fn apply_config_to_state(s: &mut FeishuState, config: &Value) {
    let g = |k: &str| config.get(k).and_then(|v| v.as_str()).map(|v| v.to_string());
    s.app_id = g("app_id");
    s.app_secret = g("app_secret");
    s.verification_token = g("verification_token");
    s.encrypt_key = g("encrypt_key");
    s.bot_open_id = g("bot_open_id");
    s.api_base = g("api_base");
}

// ---------------------------------------------------------------------------
// feishu_serve (Task): HTTP event loop + poller
// ---------------------------------------------------------------------------

fn handle_feishu_serve(req: &NodeRequest) -> Result<NodeResponse, String> {
    if req.action.as_deref() == Some("stop") {
        SERVER_SHUTDOWN.store(true, Ordering::SeqCst);
        *SERVER_RUNNING.lock().map_err(|e| format!("lock: {e}"))? = false;
        return Ok(NodeResponse { ok: true, node_id: "feishu_serve".to_string(), message: Some("stopped".to_string()), data: None, error: None });
    }

    // Hydrate STATE from persisted config on boot.
    if let Some(config) = load_runtime_config() {
        if let Ok(mut s) = STATE.lock() {
            apply_config_to_state(&mut s, &config);
        }
    }

    let port: u16 = req
        .payload
        .as_ref()
        .and_then(|p| p.get("port"))
        .and_then(|v| v.as_u64())
        .map(|v| v as u16)
        .unwrap_or(DEFAULT_PORT);

    let mut running = SERVER_RUNNING.lock().map_err(|e| format!("lock: {e}"))?;
    if *running {
        return Ok(NodeResponse { ok: true, node_id: "feishu_serve".to_string(), message: Some(format!("already running on {port}")), data: None, error: None });
    }
    let server = tiny_http::Server::http(format!("0.0.0.0:{port}"))
        .map_err(|e| format!("feishu_serve: cannot bind port {port}: {e}"))?;
    *running = true;
    drop(running);
    SERVER_SHUTDOWN.store(false, Ordering::SeqCst);

    let handle = thread::spawn(move || run_event_loop(server));
    if let Ok(mut guard) = EVENT_LOOP_HANDLE.lock() {
        *guard = Some(handle);
    }
    start_agent_poller();

    Ok(NodeResponse {
        ok: true,
        node_id: "feishu_serve".to_string(),
        message: Some(format!("HTTP server listening on port {port}, path /feishu/event")),
        data: None,
        error: None,
    })
}

fn run_event_loop(server: tiny_http::Server) {
    loop {
        if SERVER_SHUTDOWN.load(Ordering::SeqCst) {
            break;
        }
        let request = match server.recv_timeout(Duration::from_millis(500)) {
            Ok(Some(r)) => r,
            Ok(None) => continue,
            Err(_) => break,
        };
        let url = request.url().to_string();
        let method = request.method().clone();

        if url == "/health" {
            let _ = request.respond(tiny_http::Response::from_string("ok"));
            continue;
        }
        if url != "/feishu/event" || method != tiny_http::Method::Post {
            let _ = request.respond(tiny_http::Response::from_string("not found").with_status_code(404));
            continue;
        }

        let mut request = request;
        let mut body = String::new();
        if request.as_reader().read_to_string(&mut body).is_err() {
            let _ = request.respond(tiny_http::Response::from_string("bad body").with_status_code(400));
            continue;
        }

        let (vtoken, ekey, bot) = {
            let s = STATE.lock().unwrap_or_else(|p| p.into_inner());
            (s.verification_token.clone(), s.encrypt_key.clone(), s.bot_open_id.clone())
        };

        match interpret_inbound(&body, vtoken.as_deref(), ekey.as_deref(), bot.as_deref()) {
            InboundOutcome::Challenge(resp) => {
                let _ = request.respond(
                    tiny_http::Response::from_string(resp)
                        .with_header("Content-Type: application/json".parse::<tiny_http::Header>().unwrap()),
                );
            }
            InboundOutcome::Message(im) | InboundOutcome::CardAction(im) => {
                // Always 200 first so Feishu doesn't retry.
                let _ = request.respond(tiny_http::Response::from_string(r#"{"code":0}"#));
                enqueue_message(im);
            }
            InboundOutcome::Ignore(reason) => {
                eprintln!("[feishu] ignore: {reason}");
                let _ = request.respond(tiny_http::Response::from_string(r#"{"code":0}"#));
            }
            InboundOutcome::Rejected(reason) => {
                eprintln!("[feishu] rejected: {reason}");
                let _ = request.respond(tiny_http::Response::from_string("unauthorized").with_status_code(401));
            }
        }
    }
    eprintln!("[feishu] event loop stopped");
}

fn enqueue_message(im: IncomingMessage) {
    // Dedup by message_id.
    {
        let dedup_key = format!("msg:{}", im.message_id);
        let mut seen = RECENT_MESSAGE_IDS.lock().unwrap_or_else(|p| p.into_inner());
        if seen.iter().any(|k| k == &dedup_key) {
            return;
        }
        seen.push_back(dedup_key);
        while seen.len() > RECENT_IDS_CAP {
            seen.pop_front();
        }
    }
    let mut queue = MESSAGE_QUEUE.lock().unwrap_or_else(|p| p.into_inner());
    if queue.len() >= MESSAGE_QUEUE_CAP {
        let dropped = MESSAGE_QUEUE_DROPPED.fetch_add(1, Ordering::Relaxed) + 1;
        eprintln!("[feishu] queue full ({MESSAGE_QUEUE_CAP}), dropped {dropped} total");
        return;
    }
    queue.push_back(im);
}

fn start_agent_poller() {
    if POLLER_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    thread::spawn(move || {
        thread::sleep(Duration::from_secs(2));
        loop {
            if SERVER_SHUTDOWN.load(Ordering::SeqCst) {
                break;
            }
            let msgs: Vec<IncomingMessage> = {
                let mut queue = MESSAGE_QUEUE.lock().unwrap_or_else(|p| p.into_inner());
                queue.drain(..).collect()
            };
            for msg in msgs {
                if !should_process(&msg.text) {
                    continue;
                }
                let envelope = build_envelope(&msg);
                cordis_plugin_sdk::agent_trigger(&envelope);
            }
            thread::sleep(Duration::from_secs(5));
        }
    });
}

/// Skip trivial / command-style inputs (mirrors qq's should_process).
fn should_process(text: &str) -> bool {
    let t = text.trim();
    if t.chars().count() <= 1 {
        return false;
    }
    if t.starts_with('/') {
        return false;
    }
    true
}

/// Build the runtime routing envelope for a message.
fn build_envelope(msg: &IncomingMessage) -> String {
    let reply_target = format!("chat:{}", msg.chat_id);
    let session_key = format!("feishu:{}", reply_target);
    let scope = if msg.chat_type == "p2p" { "private" } else { "group" };
    let display = format!(
        "[Feishu {} from {} (user {})]: {}",
        scope, msg.chat_id, msg.open_id, msg.text
    );
    let mut env = json!({
        "source_plugin": "feishu",
        "reply_node": "feishu_send",
        "session_key": session_key,
        "display": display,
        "reply_target": reply_target,
    });
    // Prefer threaded reply if the message is in a thread.
    if let Some(root) = &msg.root_id {
        env["reply_to"] = json!(root);
    } else if !msg.message_id.starts_with("cardaction:") {
        env["reply_to"] = json!(msg.message_id);
    }
    env.to_string()
}

// ---------------------------------------------------------------------------
// Dispatch + SDK exports
// ---------------------------------------------------------------------------

fn handle(req: &NodeRequest) -> Result<NodeResponse, String> {
    match req.node_id.as_str() {
        "feishu_serve" => handle_feishu_serve(req),
        "feishu_send" => handle_feishu_send(req),
        "feishu_entry" => handle_feishu_entry(req),
        other => Err(format!("unknown node_id: {other}")),
    }
}

fn api_handle(req: PluginRequest) -> PluginResponse {
    match serde_json::from_str::<NodeRequest>(&req.payload)
        .map_err(|e| format!("feishu plugin: {e}"))
        .and_then(|r| handle(&r))
    {
        Ok(resp) => json_response(&resp),
        Err(e) => json_response(&NodeResponse::err("error", e)),
    }
}

fn docs_value() -> cordis_plugin_sdk::PluginDocs {
    plugin_docs(
        "feishu",
        "feishu",
        "0.1.0",
        Some("Feishu"),
        vec![
            task_node_doc(
                "feishu_serve",
                "Start the Feishu event-subscription HTTP server (webhook). Receives im.message.receive_v1 events + card actions, handles the url_verification challenge, verifies token, decrypts (if encrypt_key set), gates group messages on @bot, dedups, and forwards to the agent.",
                json!({"type":"object","properties":{"node_id":{"const":"feishu_serve"},"action":{"type":"string","enum":["stop"]},"payload":{"type":"object","properties":{"port":{"type":"integer"}}}}}),
                json!({"type":"object","properties":{"ok":{"type":"boolean"},"message":{"type":"string"}}}),
                &["network:listen"],
                &["port bind failure"],
            ),
            node_doc(
                "feishu_send",
                "Send a message to a Feishu chat or user. target is 'chat:<chat_id>' or 'user:<open_id>'. Provide either `message` (text) or `card` (interactive card JSON). Optional `reply_to` quote-replies to a message_id.",
                json!({"type":"object","required":["node_id","target"],"properties":{"node_id":{"const":"feishu_send"},"target":{"type":"string"},"message":{"type":"string"},"card":{"type":"object"},"reply_to":{"type":"string"}}}),
                json!({"type":"object","properties":{"ok":{"type":"boolean"},"data":{"type":"object"}}}),
                &["network:feishu-api"],
                &["auth failure","invalid target"],
            ).with_agent_accessible(),
            node_doc(
                "feishu_entry",
                "Configure the Feishu plugin (app_id/app_secret/verification_token/encrypt_key/bot_open_id) or query status. action='configure' with payload, or action='status'.",
                json!({"type":"object","required":["node_id"],"properties":{"node_id":{"const":"feishu_entry"},"action":{"type":"string","enum":["configure","status"]},"payload":{"type":"object"}}}),
                json!({"type":"object","properties":{"ok":{"type":"boolean"}}}),
                &["config write"],
                &["missing fields"],
            ),
        ],
        Some(
            "You are chatting in Feishu (Lark). Each incoming message is prefixed with its source, e.g. \"[Feishu group from <chat_id> (user <open_id>)]: <text>\".\n\
Your FINAL output each turn MUST be exactly one JSON object:\n\
  {\"action\":\"respond\",\"message\":\"<reply text>\"}  — to reply to the chat, OR\n\
  {\"action\":\"suspend\"}  — when no reply is warranted.\n\
The runtime routes your \"respond\" back to the originating Feishu chat automatically; do NOT call feishu_send yourself for the direct reply.\n\
To proactively send an extra message (e.g. a progress update) to a chat: invoke_plugin(feishu, feishu_send, {\"node_id\":\"feishu_send\",\"target\":\"chat:<chat_id>\",\"message\":\"<text>\"}).\n\
Keep replies concise and helpful; a short honest \"not sure\" beats a long wrong answer.",
        ),
    )
}

fn abi_fingerprint_value() -> AbiFingerprint {
    AbiFingerprint {
        rustc_version: "1.85.1".to_string(),
        target_triple: "x86_64-unknown-linux-gnu".to_string(),
        crate_hash: "crate_feishu_v1".to_string(),
        api_hash: "api_v2".to_string(),
    }
}

export_plugin_api! {
    abi_fingerprint = abi_fingerprint_value(),
    docs = docs_value(),
    handle = api_handle,
}

// ---------------------------------------------------------------------------
// Tests (no network)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenge_handshake_echoes_challenge() {
        let body = json!({"type":"url_verification","challenge":"abc123","token":"vtok"}).to_string();
        match interpret_inbound(&body, Some("vtok"), None, None) {
            InboundOutcome::Challenge(resp) => {
                let v: Value = serde_json::from_str(&resp).unwrap();
                assert_eq!(v.get("challenge").unwrap().as_str().unwrap(), "abc123");
            }
            other => panic!("expected Challenge, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[test]
    fn challenge_wrong_token_rejected() {
        let body = json!({"type":"url_verification","challenge":"x","token":"WRONG"}).to_string();
        assert!(matches!(
            interpret_inbound(&body, Some("vtok"), None, None),
            InboundOutcome::Rejected(_)
        ));
    }

    #[test]
    fn event_token_mismatch_rejected() {
        let body = json!({
            "header": {"event_type":"im.message.receive_v1","token":"BAD"},
            "event": {}
        }).to_string();
        assert!(matches!(
            interpret_inbound(&body, Some("vtok"), None, None),
            InboundOutcome::Rejected(_)
        ));
    }

    #[test]
    fn p2p_message_is_actionable() {
        let body = message_event("p2p", "oc_1", "om_1", "hello there", &[]).to_string();
        match interpret_inbound(&body, Some("vtok"), None, Some("ou_bot")) {
            InboundOutcome::Message(im) => {
                assert_eq!(im.chat_id, "oc_1");
                assert_eq!(im.text, "hello there");
            }
            _ => panic!("expected Message"),
        }
    }

    #[test]
    fn group_message_without_at_is_skipped() {
        let body = message_event("group", "oc_2", "om_2", "just chatting", &[]).to_string();
        assert!(matches!(
            interpret_inbound(&body, Some("vtok"), None, Some("ou_bot")),
            InboundOutcome::Ignore(_)
        ));
    }

    #[test]
    fn group_message_with_at_bot_is_actionable() {
        let body = message_event("group", "oc_3", "om_3", "@_user_1 hi bot", &["ou_bot"]).to_string();
        match interpret_inbound(&body, Some("vtok"), None, Some("ou_bot")) {
            InboundOutcome::Message(im) => {
                assert_eq!(im.chat_id, "oc_3");
                // mention placeholder stripped
                assert_eq!(im.text, "hi bot");
            }
            _ => panic!("expected Message"),
        }
    }

    #[test]
    fn group_at_someone_else_is_skipped() {
        let body = message_event("group", "oc_4", "om_4", "@_user_1 hi", &["ou_someone"]).to_string();
        assert!(matches!(
            interpret_inbound(&body, Some("vtok"), None, Some("ou_bot")),
            InboundOutcome::Ignore(_)
        ));
    }

    #[test]
    fn parse_target_variants() {
        assert_eq!(parse_target("chat:oc_x").unwrap(), ("chat_id", "oc_x".to_string()));
        assert_eq!(parse_target("user:ou_y").unwrap(), ("open_id", "ou_y".to_string()));
        assert!(parse_target("bogus").is_err());
    }

    #[test]
    fn envelope_has_routing_fields() {
        let im = IncomingMessage {
            chat_type: "group".to_string(),
            chat_id: "oc_5".to_string(),
            open_id: "ou_u".to_string(),
            text: "hello".to_string(),
            message_id: "om_5".to_string(),
            root_id: None,
        };
        let env: Value = serde_json::from_str(&build_envelope(&im)).unwrap();
        assert_eq!(env["source_plugin"], "feishu");
        assert_eq!(env["reply_node"], "feishu_send");
        assert_eq!(env["session_key"], "feishu:chat:oc_5");
        assert_eq!(env["reply_target"], "chat:oc_5");
        assert_eq!(env["reply_to"], "om_5");
        assert!(env["display"].as_str().unwrap().contains("hello"));
    }

    #[test]
    fn aes_roundtrip_decrypts() {
        // Encrypt a known plaintext the same way Feishu does, then decrypt.
        use aes::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
        use base64::Engine;
        use sha2::{Digest, Sha256};
        let ekey = "test_encrypt_key";
        let plaintext = json!({"header":{"event_type":"im.message.receive_v1"}}).to_string();
        let key = Sha256::digest(ekey.as_bytes());
        let iv = [7u8; 16];
        type Enc = cbc::Encryptor<aes::Aes256>;
        let enc = Enc::new_from_slices(&key, &iv).unwrap();
        // Buffer-form encrypt (no `alloc` cipher feature needed): pad the
        // plaintext into a buffer sized to the next block multiple.
        let pt = plaintext.as_bytes();
        let mut buf = vec![0u8; ((pt.len() / 16) + 1) * 16];
        buf[..pt.len()].copy_from_slice(pt);
        let ct = enc
            .encrypt_padded_mut::<Pkcs7>(&mut buf, pt.len())
            .unwrap()
            .to_vec();
        let mut framed = iv.to_vec();
        framed.extend_from_slice(&ct);
        let b64 = base64::engine::general_purpose::STANDARD.encode(&framed);
        let decrypted = decrypt_feishu(ekey, &b64).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn card_action_becomes_message() {
        let body = json!({
            "header": {"event_type":"card.action.trigger","token":"vtok"},
            "event": {
                "action": {"value": {"text":"button clicked","chat_id":"oc_card"}},
                "operator": {"open_id":"ou_op"}
            }
        }).to_string();
        match interpret_inbound(&body, Some("vtok"), None, None) {
            InboundOutcome::CardAction(im) => {
                assert_eq!(im.chat_id, "oc_card");
                assert_eq!(im.text, "button clicked");
            }
            _ => panic!("expected CardAction"),
        }
    }

    // Build an im.message.receive_v1 event with optional @mention open_ids.
    fn message_event(chat_type: &str, chat_id: &str, msg_id: &str, text: &str, mentions: &[&str]) -> Value {
        let mention_arr: Vec<Value> = mentions
            .iter()
            .map(|oid| json!({"id":{"open_id":oid}}))
            .collect();
        json!({
            "header": {"event_type":"im.message.receive_v1","token":"vtok"},
            "event": {
                "sender": {"sender_id": {"open_id":"ou_sender"}},
                "message": {
                    "chat_id": chat_id,
                    "chat_type": chat_type,
                    "message_id": msg_id,
                    "content": json!({"text": text}).to_string(),
                    "mentions": mention_arr
                }
            }
        })
    }
}
