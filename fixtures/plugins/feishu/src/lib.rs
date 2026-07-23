//! Feishu (Lark) plugin — OneBot-style protocol adapter for Feishu open
//! platform, structurally analogous to the `qq` plugin.
//!
//! Nodes:
//! - `feishu_serve` (Task) — event intake in one of two modes:
//!   * `mode:"ws"` (default, openclaw-style): long connection — dial out to
//!     Feishu via `/callback/ws/endpoint`, receive events over WSS (see
//!     `ws.rs`). No public URL / verification_token / encrypt_key needed.
//!   * `mode:"webhook"`: HTTP server on :8100 receiving event-subscription
//!     callbacks at `POST /feishu/event` (url_verification challenge,
//!     optional AES decryption, token check).
//!
//!   Both modes share: dedup, openclaw-style access policy (dm_policy /
//!   group_policy / require_mention / pairing), then a poller emits
//!   structured envelopes to the runtime agent via `agent_trigger`.
//!
//! - `feishu_send` (Router) — outbound: send text / interactive card,
//!   quote-reply, or PATCH-update a previously sent card (two-stage
//!   "thinking… → final" replies), using a cached tenant_access_token.
//! - `feishu_entry` (Router) — configure (persist app_id/secret/policies),
//!   approve_pairing / list_pending, status.

use cordis_plugin_sdk::{
    export_plugin_api, json_response, node_doc, plugin_docs, task_node_doc, AbiFingerprint,
    PluginRequest, PluginResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::thread;
use std::time::{Duration, Instant};

mod ws;

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
    /// openclaw-style access policy overrides (None/empty = defaults).
    dm_policy: Option<String>,
    dm_allow_from: Vec<String>,
    group_policy: Option<String>,
    group_allow_from: Vec<String>,
    require_mention: Option<bool>,
    /// Two-stage card replies ("thinking… → final"); default on.
    card_replies: Option<bool>,
}

static STATE: Mutex<FeishuState> = Mutex::new(FeishuState {
    app_id: None,
    app_secret: None,
    verification_token: None,
    encrypt_key: None,
    bot_open_id: None,
    api_base: None,
    dm_policy: None,
    dm_allow_from: Vec::new(),
    group_policy: None,
    group_allow_from: Vec::new(),
    require_mention: None,
    card_replies: None,
});

// ---------------------------------------------------------------------------
// Access policy (openclaw-compatible: dmPolicy / groupPolicy / requireMention)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) struct AccessPolicy {
    /// "open" | "allowlist" | "pairing" (default: pairing — unknown DM users
    /// get a pairing code an admin must approve).
    pub dm_policy: String,
    pub dm_allow_from: Vec<String>,
    /// "open" | "allowlist" | "disabled" (default: allowlist).
    pub group_policy: String,
    pub group_allow_from: Vec<String>,
    /// None → derived: mention required unless group_policy == "open".
    pub require_mention: Option<bool>,
}

impl Default for AccessPolicy {
    fn default() -> Self {
        AccessPolicy {
            dm_policy: "pairing".to_string(),
            dm_allow_from: Vec::new(),
            group_policy: "allowlist".to_string(),
            group_allow_from: Vec::new(),
            require_mention: None,
        }
    }
}

impl AccessPolicy {
    fn require_mention_effective(&self) -> bool {
        self.require_mention.unwrap_or(self.group_policy != "open")
    }
}

/// Snapshot the effective policy from STATE (falling back to defaults).
pub(crate) fn current_policy() -> AccessPolicy {
    let s = STATE.lock().unwrap_or_else(|p| p.into_inner());
    let mut p = AccessPolicy::default();
    if let Some(v) = &s.dm_policy {
        p.dm_policy = v.clone();
    }
    p.dm_allow_from = s.dm_allow_from.clone();
    if let Some(v) = &s.group_policy {
        p.group_policy = v.clone();
    }
    p.group_allow_from = s.group_allow_from.clone();
    p.require_mention = s.require_mention;
    p
}

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
    /// Whether the bot was @-mentioned (group chats; parse-time fact,
    /// policy decides what to do with it).
    mentioned: bool,
    /// Feishu message_type ("text" | "image" | "post" | "file" | "audio" |
    /// "media" | "sticker" | ...). Defaults to "text" when absent.
    message_type: String,
    /// True when the extracted text contains a media placeholder (image /
    /// file / audio / video / sticker / unsupported), so the message is
    /// forwarded even though its literal text may be short.
    has_media: bool,
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
pub(crate) enum InboundOutcome {
    /// url_verification handshake — respond with this exact JSON body.
    Challenge(String),
    /// A message to enqueue.
    Message(IncomingMessage),
    /// A card button action — treated as a fresh inbound message.
    CardAction(IncomingMessage),
    /// dm_policy=pairing and the sender is unknown: reply with a pairing
    /// code instead of forwarding to the agent. (open_id, code)
    Pairing(String, String),
    /// Valid but not actionable (non-message event, wrong token, etc.).
    Ignore(String),
    /// Token/verification rejected — respond 401.
    Rejected(String),
}

/// Parse a raw HTTP body into an outcome. `verification_token`/`encrypt_key`
/// come from config (webhook mode; pass None in ws mode); `bot_open_id`
/// resolves @-mentions; `policy` gates who may talk to the agent.
pub(crate) fn interpret_inbound(
    raw_body: &str,
    verification_token: Option<&str>,
    encrypt_key: Option<&str>,
    bot_open_id: Option<&str>,
    policy: &AccessPolicy,
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
        Some(im) => policy_gate(im, policy),
        None => InboundOutcome::Ignore("malformed message event".into()),
    }
}

/// Apply the openclaw-style access policy to a parsed message.
fn policy_gate(im: IncomingMessage, policy: &AccessPolicy) -> InboundOutcome {
    if im.chat_type == "p2p" {
        return match policy.dm_policy.as_str() {
            "open" => InboundOutcome::Message(im),
            "allowlist" => {
                if policy.dm_allow_from.iter().any(|a| a == &im.open_id) {
                    InboundOutcome::Message(im)
                } else {
                    InboundOutcome::Ignore(format!(
                        "dm from {} not in allowlist; skipped",
                        im.open_id
                    ))
                }
            }
            // Default & explicit "pairing": known users pass, unknown users
            // get a pairing code an admin must approve via feishu_entry.
            _ => {
                if policy.dm_allow_from.iter().any(|a| a == &im.open_id) {
                    InboundOutcome::Message(im)
                } else {
                    let code = pairing_code_for(&im.open_id);
                    InboundOutcome::Pairing(im.open_id.clone(), code)
                }
            }
        };
    }

    // Group chats.
    match policy.group_policy.as_str() {
        "disabled" => {
            return InboundOutcome::Ignore("group_policy=disabled; skipped".to_string())
        }
        "open" => {}
        // Default & explicit "allowlist".
        _ => {
            if !policy.group_allow_from.iter().any(|a| a == &im.chat_id) {
                return InboundOutcome::Ignore(format!(
                    "group {} not in allowlist; skipped",
                    im.chat_id
                ));
            }
        }
    }
    if policy.require_mention_effective() && !im.mentioned {
        return InboundOutcome::Ignore("group message without @bot; skipped".to_string());
    }
    InboundOutcome::Message(im)
}

/// Extract a plain-text rendering of a message `content` value given its
/// `message_type`. Returns `(text, has_media)`. Media messages become a
/// bracketed placeholder carrying the resource key, so the agent can fetch
/// the resource via `feishu_fetch_resource`. Malformed content (missing
/// keys) degrades to empty-string placeholders and never panics.
fn extract_content(message_type: &str, content: &Value) -> (String, bool) {
    let s = |key: &str| content.get(key).and_then(|v| v.as_str()).unwrap_or("");
    match message_type {
        "text" => (s("text").to_string(), false),
        "image" => (format!("[image file_key={}]", s("image_key")), true),
        "post" => render_post(content),
        "file" => (
            format!("[file file_key={} name=\"{}\"]", s("file_key"), s("file_name")),
            true,
        ),
        "audio" => {
            let duration = content
                .get("duration")
                .and_then(|d| d.as_i64())
                .map(|d| d.to_string())
                .unwrap_or_else(|| s("duration").to_string());
            (format!("[audio file_key={} duration={duration}ms]", s("file_key")), true)
        }
        "media" => (
            format!("[video file_key={} name=\"{}\"]", s("file_key"), s("file_name")),
            true,
        ),
        "sticker" => (format!("[sticker file_key={}]", s("file_key")), true),
        other => (format!("[unsupported message_type={other}]"), true),
    }
}

/// Render a Feishu rich-text (post) `content` into plain text. A non-empty
/// title becomes a leading line. Each paragraph is a row of runs; runs are
/// concatenated directly and paragraphs are joined with "\n". `img` runs
/// become an image placeholder (setting has_media); `at` runs render as
/// "@<name>"; `text`/`a` runs contribute their `text` field.
fn render_post(content: &Value) -> (String, bool) {
    let mut has_media = false;
    let mut lines: Vec<String> = Vec::new();

    if let Some(title) = content.get("title").and_then(|t| t.as_str()) {
        if !title.is_empty() {
            lines.push(title.to_string());
        }
    }

    if let Some(paragraphs) = content.get("content").and_then(|c| c.as_array()) {
        for paragraph in paragraphs {
            let Some(runs) = paragraph.as_array() else { continue };
            let mut line = String::new();
            for run in runs {
                let tag = run.get("tag").and_then(|t| t.as_str()).unwrap_or("");
                match tag {
                    "text" | "a" => {
                        line.push_str(run.get("text").and_then(|t| t.as_str()).unwrap_or(""));
                    }
                    "at" => {
                        let name = run
                            .get("user_name")
                            .and_then(|n| n.as_str())
                            .or_else(|| run.get("user_id").and_then(|n| n.as_str()))
                            .unwrap_or("");
                        line.push('@');
                        line.push_str(name);
                    }
                    "img" => {
                        let key = run.get("image_key").and_then(|k| k.as_str()).unwrap_or("");
                        line.push_str(&format!("[image file_key={key}]"));
                        has_media = true;
                    }
                    _ => {}
                }
            }
            lines.push(line);
        }
    }

    (lines.join("\n"), has_media)
}

/// Parse an `im.message.receive_v1` event structurally (no policy).
/// Returns None when malformed. Mention state is recorded on the message;
/// `policy_gate` decides whether it matters.
fn parse_message_event(v: &Value, bot_open_id: Option<&str>) -> Option<IncomingMessage> {
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

    // content is a JSON string; its shape depends on message_type.
    let message_type = message
        .get("message_type")
        .and_then(|t| t.as_str())
        .unwrap_or("text")
        .to_string();
    let content_raw = message.get("content").and_then(|c| c.as_str()).unwrap_or("{}");
    let content: Value = serde_json::from_str(content_raw).unwrap_or(Value::Null);
    let (text, has_media) = extract_content(&message_type, &content);

    // Record whether the bot was @-mentioned; policy_gate decides relevance.
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

    // Strip @-mention placeholder text like "@_user_1 " that Feishu leaves.
    // Only text and post carry such placeholders; media placeholders (with
    // quoted file names / spaced tokens) are kept verbatim so they survive.
    let cleaned = if message_type == "text" || message_type == "post" {
        strip_mention_tokens(&text)
    } else {
        text
    };

    Some(IncomingMessage {
        chat_type,
        chat_id,
        open_id,
        text: cleaned,
        message_id,
        root_id,
        mentioned,
        message_type,
        has_media,
    })
}

// ---------------------------------------------------------------------------
// Pairing (dm_policy = "pairing", openclaw-style)
// ---------------------------------------------------------------------------

/// Pending pairing requests: code → open_id.
static PENDING_PAIRINGS: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Deterministic 6-digit pairing code per open_id (stable across retries so
/// the user always sees the same code until approved).
fn pairing_code_for(open_id: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(open_id.as_bytes());
    let n = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]) % 1_000_000;
    let code = format!("{n:06}");
    let mut pending = PENDING_PAIRINGS.lock().unwrap_or_else(|p| p.into_inner());
    pending.insert(code.clone(), open_id.to_string());
    code
}

/// Approve a pairing code: moves the open_id into dm_allow_from (persisted).
fn approve_pairing(code: &str) -> Result<String, String> {
    let open_id = {
        let mut pending = PENDING_PAIRINGS.lock().unwrap_or_else(|p| p.into_inner());
        pending
            .remove(code.trim())
            .ok_or_else(|| format!("no pending pairing with code {code}"))?
    };
    let mut config = load_runtime_config().unwrap_or_else(|| json!({}));
    let mut allow: Vec<String> = config
        .get("dm_allow_from")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    if !allow.iter().any(|a| a == &open_id) {
        allow.push(open_id.clone());
    }
    config["dm_allow_from"] = json!(allow);
    save_runtime_config(&config)?;
    if let Ok(mut s) = STATE.lock() {
        apply_config_to_state(&mut s, &config);
    }
    Ok(open_id)
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
        // A button click is always an explicit interaction with the bot.
        mentioned: true,
        message_type: "text".to_string(),
        has_media: false,
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

/// Two-stage card replies: reply_target → message_id of the "thinking…"
/// card we posted when the message was accepted. The agent's reply then
/// PATCHes that card instead of sending a new message.
static PENDING_CARDS: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn loading_card() -> Value {
    json!({
        "config": { "wide_screen_mode": true },
        "elements": [
            { "tag": "div", "text": { "tag": "lark_md", "content": "⏳ 思考中…" } }
        ]
    })
}

fn final_card(text: &str) -> Value {
    json!({
        "config": { "wide_screen_mode": true },
        "elements": [
            { "tag": "markdown", "content": text }
        ]
    })
}

/// PATCH an existing interactive-card message with new content.
fn feishu_update_card(token: &str, message_id: &str, card: &Value) -> Result<Value, String> {
    let url = format!("{}/open-apis/im/v1/messages/{message_id}", api_base());
    let resp = ureq::request("PATCH", &url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", "application/json; charset=utf-8")
        .send_json(json!({ "content": card.to_string() }))
        .map_err(|e| format!("feishu card update failed: {e}"))?;
    let parsed: Value = resp.into_json().map_err(|e| format!("feishu card update parse: {e}"))?;
    let code = parsed.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        return Err(format!(
            "feishu card update error code={code}: {}",
            parsed.get("msg").and_then(|m| m.as_str()).unwrap_or("")
        ));
    }
    Ok(parsed)
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
// Media resource helpers (download / upload / get / chats)
// ---------------------------------------------------------------------------

/// Cap for any resource we download or upload (Feishu message resources).
const MAX_RESOURCE_BYTES: usize = 20 * 1024 * 1024;

/// Monotonic counter for temp resource filenames (unique across concurrent
/// callers within the process; see the vision plugin's P0-27 fix).
static RESOURCE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Sniff a small set of image magic bytes; returns an extension without the
/// dot. Falls back to "bin" for anything unrecognized.
fn guess_ext(bytes: &[u8]) -> &'static str {
    if bytes.len() >= 4 && &bytes[..4] == b"\x89PNG" {
        "png"
    } else if bytes.len() >= 3 && &bytes[..3] == b"\xFF\xD8\xFF" {
        "jpg"
    } else if bytes.len() >= 3 && &bytes[..3] == b"GIF" {
        "gif"
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "webp"
    } else {
        "bin"
    }
}

/// A unique temp path for a downloaded resource: pid + in-process counter +
/// nanosecond timestamp, so concurrent callers never clobber each other.
fn temp_resource_path(ext: &str) -> PathBuf {
    use std::time::{SystemTime, UNIX_EPOCH};
    let seq = RESOURCE_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "cordis_feishu_{}_{seq:x}_{nanos:x}.{ext}",
        std::process::id()
    ))
}

/// Download a message resource (image or file). `kind` selects the API
/// `type` query param: "image" for image_key resources (image messages and
/// post `img` runs), "file" for file_key resources (file/audio/media/
/// sticker). Rejects synthetic card-action message ids (no real resource),
/// enforces the 20MB cap, and surfaces Feishu error JSON as an Err.
fn feishu_download_resource(
    api_base: &str,
    token: &str,
    message_id: &str,
    file_key: &str,
    kind: &str,
) -> Result<Vec<u8>, String> {
    if message_id.starts_with("cardaction:") {
        return Err("synthetic cardaction message_id has no downloadable resource".to_string());
    }
    let url = format!(
        "{api_base}/open-apis/im/v1/messages/{message_id}/resources/{file_key}?type={kind}"
    );
    let resp = ureq::get(&url)
        .set("Authorization", &format!("Bearer {token}"))
        .call()
        .map_err(|e| format!("feishu resource download failed: {e}"))?;
    let content_type = resp.content_type().to_string();
    let reader = resp.into_reader();
    let mut bytes = Vec::new();
    use std::io::Read;
    reader
        .take((MAX_RESOURCE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("feishu resource read: {e}"))?;
    if bytes.len() > MAX_RESOURCE_BYTES {
        return Err(format!("resource exceeds {MAX_RESOURCE_BYTES} bytes"));
    }
    // A JSON body on this endpoint is always an error payload, never a
    // resource — parse it and surface code/msg instead of writing garbage.
    let looks_json = content_type.contains("application/json")
        || bytes.first().is_some_and(|b| *b == b'{');
    if looks_json {
        if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
            let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
            let msg = v.get("msg").and_then(|m| m.as_str()).unwrap_or("");
            return Err(format!("feishu resource error code={code}: {msg}"));
        }
    }
    Ok(bytes)
}

/// GET a Feishu API endpoint with Bearer auth, parse JSON, require code==0,
/// return the `data` object (or Null when absent).
fn feishu_get_json(api_base: &str, token: &str, path: &str) -> Result<Value, String> {
    let url = format!("{api_base}{path}");
    let resp = ureq::get(&url)
        .set("Authorization", &format!("Bearer {token}"))
        .call()
        .map_err(|e| format!("feishu GET {path} failed: {e}"))?;
    let parsed: Value = resp.into_json().map_err(|e| format!("feishu GET {path} parse: {e}"))?;
    let code = parsed.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        return Err(format!(
            "feishu GET {path} error code={code}: {}",
            parsed.get("msg").and_then(|m| m.as_str()).unwrap_or("")
        ));
    }
    Ok(parsed.get("data").cloned().unwrap_or(Value::Null))
}

fn feishu_get_message_api(api_base: &str, token: &str, message_id: &str) -> Result<Value, String> {
    if message_id.starts_with("cardaction:") {
        return Err("synthetic cardaction message_id cannot be fetched".to_string());
    }
    feishu_get_json(api_base, token, &format!("/open-apis/im/v1/messages/{message_id}"))
}

fn feishu_get_chat_api(api_base: &str, token: &str, chat_id: &str) -> Result<Value, String> {
    feishu_get_json(api_base, token, &format!("/open-apis/im/v1/chats/{chat_id}"))
}

fn feishu_list_chats_api(
    api_base: &str,
    token: &str,
    page_size: Option<i64>,
    page_token: Option<&str>,
) -> Result<Value, String> {
    let mut query = String::new();
    if let Some(size) = page_size {
        query.push_str(&format!("page_size={size}"));
    }
    if let Some(tok) = page_token {
        if !query.is_empty() {
            query.push('&');
        }
        query.push_str(&format!("page_token={tok}"));
    }
    let path = if query.is_empty() {
        "/open-apis/im/v1/chats".to_string()
    } else {
        format!("/open-apis/im/v1/chats?{query}")
    };
    feishu_get_json(api_base, token, &path)
}

/// Build a multipart/form-data body for the image upload endpoint: an
/// `image_type=message` field plus an `image` file part carrying the bytes.
/// Kept pure (no I/O) so it can be unit-tested.
fn build_multipart_image_body(boundary: &str, bytes: &[u8], ext: &str) -> Vec<u8> {
    let mut body = Vec::new();
    let mut push = |s: &str| body.extend_from_slice(s.as_bytes());
    push(&format!("--{boundary}\r\n"));
    push("Content-Disposition: form-data; name=\"image_type\"\r\n\r\n");
    push("message\r\n");
    push(&format!("--{boundary}\r\n"));
    push(&format!(
        "Content-Disposition: form-data; name=\"image\"; filename=\"image.{ext}\"\r\n"
    ));
    push("Content-Type: application/octet-stream\r\n\r\n");
    body.extend_from_slice(bytes);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

/// Upload an image to Feishu, returning the resulting image_key for use in
/// an `image` message.
fn feishu_upload_image(api_base: &str, token: &str, bytes: &[u8]) -> Result<String, String> {
    if bytes.len() > MAX_RESOURCE_BYTES {
        return Err(format!("image exceeds {MAX_RESOURCE_BYTES} bytes"));
    }
    let ext = guess_ext(bytes);
    let boundary = format!("cordisfeishu{:x}", RESOURCE_SEQ.fetch_add(1, Ordering::Relaxed));
    let body = build_multipart_image_body(&boundary, bytes, ext);
    let url = format!("{api_base}/open-apis/im/v1/images");
    let resp = ureq::post(&url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Content-Type", &format!("multipart/form-data; boundary={boundary}"))
        .send_bytes(&body)
        .map_err(|e| format!("feishu image upload failed: {e}"))?;
    let parsed: Value = resp.into_json().map_err(|e| format!("feishu image upload parse: {e}"))?;
    let code = parsed.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    if code != 0 {
        return Err(format!(
            "feishu image upload error code={code}: {}",
            parsed.get("msg").and_then(|m| m.as_str()).unwrap_or("")
        ));
    }
    parsed
        .get("data")
        .and_then(|d| d.get("image_key"))
        .and_then(|k| k.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| "no image_key in upload response".to_string())
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

    // Two-stage card flow: if we posted a "thinking…" card for this target,
    // PATCH it into the final content instead of sending a new message.
    // (Explicit `update_message_id` in the payload takes precedence.)
    let update_mid = req
        .payload
        .as_ref()
        .and_then(|p| p.get("update_message_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            let mut pending = PENDING_CARDS.lock().unwrap_or_else(|p| p.into_inner());
            pending.remove(target)
        });
    if let Some(mid) = update_mid {
        let card = if let Some(card) = &req.card {
            card.clone()
        } else {
            let message = req.message.as_deref().unwrap_or("").trim();
            if message.is_empty() {
                return Err("message or card is required for feishu_send".to_string());
            }
            final_card(message)
        };
        match feishu_update_card(&token, &mid, &card) {
            Ok(data) => {
                return Ok(NodeResponse {
                    ok: true,
                    node_id: "feishu_send".to_string(),
                    message: Some("updated".to_string()),
                    data: Some(data),
                    error: None,
                })
            }
            // Card update can fail (e.g. card expired); fall through to a
            // plain send so the reply is never lost.
            Err(e) => eprintln!("[feishu] card update failed, sending fresh message: {e}"),
        }
    }

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

/// Resolve app credentials from payload/STATE/config and mint a token.
fn resolve_token(payload: Option<&Value>) -> Result<String, String> {
    let app_id = config_str(payload, "app_id", |s| s.app_id.clone()).ok_or("no app_id configured")?;
    let app_secret =
        config_str(payload, "app_secret", |s| s.app_secret.clone()).ok_or("no app_secret configured")?;
    tenant_access_token(&app_id, &app_secret)
}

/// Shared core for feishu_fetch_resource / feishu_fetch_image. Downloads a
/// resource to a temp file (kept on disk so downstream nodes like vision can
/// read it by path) and returns metadata. `forced_type` pins the resource
/// kind ("image" for feishu_fetch_image); when None the payload `type`
/// (default "image") is used.
fn fetch_resource_core(payload: &Value, forced_type: Option<&str>) -> Result<Value, String> {
    let message_id = payload
        .get("message_id")
        .and_then(|v| v.as_str())
        .ok_or("message_id is required")?;
    let file_key = payload
        .get("file_key")
        .and_then(|v| v.as_str())
        .ok_or("file_key is required")?;
    let kind = forced_type.unwrap_or_else(|| {
        payload.get("type").and_then(|v| v.as_str()).unwrap_or("image")
    });
    if kind != "image" && kind != "file" {
        return Err(format!("type must be 'image' or 'file', got {kind}"));
    }
    let as_base64 = payload.get("as_base64").and_then(|v| v.as_bool()).unwrap_or(false);

    let token = resolve_token(Some(payload))?;
    let base = api_base();
    let bytes = feishu_download_resource(&base, &token, message_id, file_key, kind)?;
    let ext = guess_ext(&bytes);
    let path = temp_resource_path(ext);
    std::fs::write(&path, &bytes).map_err(|e| format!("write temp resource: {e}"))?;

    let mut data = json!({
        "path": path.to_string_lossy(),
        "size": bytes.len(),
        "mime": ext,
    });
    // Inline base64 only for small resources, to avoid bloating the response.
    if as_base64 && bytes.len() <= 1024 * 1024 {
        use base64::Engine;
        data["base64"] = json!(base64::engine::general_purpose::STANDARD.encode(&bytes));
    }
    Ok(data)
}

fn handle_feishu_fetch_resource(req: &NodeRequest, forced_type: Option<&str>) -> Result<NodeResponse, String> {
    let payload = req.payload.as_ref().ok_or("payload is required")?;
    let data = fetch_resource_core(payload, forced_type)?;
    let node_id = req.node_id.clone();
    Ok(NodeResponse { ok: true, node_id, message: None, data: Some(data), error: None })
}

fn handle_feishu_send_image(req: &NodeRequest) -> Result<NodeResponse, String> {
    let payload = req.payload.clone().unwrap_or(Value::Null);
    let target = req
        .target
        .as_deref()
        .or_else(|| payload.get("target").and_then(|v| v.as_str()))
        .unwrap_or("")
        .trim()
        .to_string();
    if target.is_empty() {
        return Err("target is required for feishu_send_image (chat:<id> or user:<id>)".to_string());
    }
    let reply_to = req
        .reply_to
        .clone()
        .or_else(|| payload.get("reply_to").and_then(|v| v.as_str()).map(|s| s.to_string()));

    let token = resolve_token(req.payload.as_ref())?;
    let base = api_base();

    // An explicit image_key skips upload; otherwise a local temp path must be
    // read and uploaded first.
    let image_key = if let Some(key) = payload.get("image_key").and_then(|v| v.as_str()) {
        key.to_string()
    } else {
        let path = payload
            .get("path")
            .and_then(|v| v.as_str())
            .ok_or("either image_key or path is required")?;
        // Confine reads to the temp dir: the resource must canonicalize to a
        // location under the (canonicalized) system temp dir.
        let canon = std::fs::canonicalize(path).map_err(|e| format!("canonicalize path: {e}"))?;
        let tmp_root = std::fs::canonicalize(std::env::temp_dir())
            .map_err(|e| format!("canonicalize temp dir: {e}"))?;
        if !canon.starts_with(&tmp_root) {
            return Err("path must be located under the system temp directory".to_string());
        }
        let bytes = std::fs::read(&canon).map_err(|e| format!("read image: {e}"))?;
        if bytes.len() > MAX_RESOURCE_BYTES {
            return Err(format!("image exceeds {MAX_RESOURCE_BYTES} bytes"));
        }
        feishu_upload_image(&base, &token, &bytes)?
    };

    let (recv_type, recv_id) = parse_target(&target)?;
    let content = json!({ "image_key": image_key }).to_string();
    let sent = feishu_send_message(&token, recv_type, &recv_id, "image", &content, reply_to.as_deref())?;
    let message_id = sent
        .get("data")
        .and_then(|d| d.get("message_id"))
        .and_then(|m| m.as_str())
        .map(|s| s.to_string());
    Ok(NodeResponse {
        ok: true,
        node_id: "feishu_send_image".to_string(),
        message: Some("sent".to_string()),
        data: Some(json!({ "image_key": image_key, "message_id": message_id })),
        error: None,
    })
}

fn handle_feishu_get_message(req: &NodeRequest) -> Result<NodeResponse, String> {
    let payload = req.payload.as_ref().ok_or("payload is required")?;
    let message_id = payload
        .get("message_id")
        .and_then(|v| v.as_str())
        .ok_or("message_id is required")?;
    let token = resolve_token(req.payload.as_ref())?;
    let data = feishu_get_message_api(&api_base(), &token, message_id)?;
    Ok(NodeResponse {
        ok: true,
        node_id: "feishu_get_message".to_string(),
        message: None,
        data: Some(data),
        error: None,
    })
}

fn handle_feishu_get_chat_info(req: &NodeRequest) -> Result<NodeResponse, String> {
    let payload = req.payload.as_ref().ok_or("payload is required")?;
    let chat_id = payload
        .get("chat_id")
        .and_then(|v| v.as_str())
        .ok_or("chat_id is required")?;
    let token = resolve_token(req.payload.as_ref())?;
    let data = feishu_get_chat_api(&api_base(), &token, chat_id)?;
    Ok(NodeResponse {
        ok: true,
        node_id: "feishu_get_chat_info".to_string(),
        message: None,
        data: Some(data),
        error: None,
    })
}

fn handle_feishu_list_chats(req: &NodeRequest) -> Result<NodeResponse, String> {
    let payload = req.payload.clone().unwrap_or(Value::Null);
    let page_size = payload.get("page_size").and_then(|v| v.as_i64());
    let page_token = payload.get("page_token").and_then(|v| v.as_str());
    let token = resolve_token(req.payload.as_ref())?;
    let data = feishu_list_chats_api(&api_base(), &token, page_size, page_token)?;
    Ok(NodeResponse {
        ok: true,
        node_id: "feishu_list_chats".to_string(),
        message: None,
        data: Some(data),
        error: None,
    })
}

fn handle_feishu_entry(req: &NodeRequest) -> Result<NodeResponse, String> {
    match req.action.as_deref() {
        Some("configure") => {
            let payload = req.payload.clone().unwrap_or(Value::Null);
            // Merge into existing config so partial updates work.
            let mut config = load_runtime_config().unwrap_or_else(|| json!({}));
            for key in [
                "app_id", "app_secret", "verification_token", "encrypt_key",
                "bot_open_id", "api_base", "dm_policy", "group_policy",
            ] {
                if let Some(val) = payload.get(key).and_then(|v| v.as_str()) {
                    config[key] = json!(val);
                }
            }
            for key in ["dm_allow_from", "group_allow_from"] {
                if let Some(val) = payload.get(key).and_then(|v| v.as_array()) {
                    config[key] = json!(val);
                }
            }
            for key in ["require_mention", "card_replies"] {
                if let Some(val) = payload.get(key).and_then(|v| v.as_bool()) {
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
            let policy = current_policy();
            let s = STATE.lock().map_err(|e| format!("lock: {e}"))?;
            let data = json!({
                "app_id_set": s.app_id.is_some(),
                "verification_token_set": s.verification_token.is_some(),
                "encrypt_key_set": s.encrypt_key.is_some(),
                "bot_open_id_set": s.bot_open_id.is_some(),
                "server_running": SERVER_RUNNING.lock().map(|g| *g).unwrap_or(false),
                "queue_dropped": MESSAGE_QUEUE_DROPPED.load(Ordering::Relaxed),
                "dm_policy": policy.dm_policy,
                "group_policy": policy.group_policy,
                "require_mention": policy.require_mention_effective(),
                "dm_allow_count": policy.dm_allow_from.len(),
                "group_allow_count": policy.group_allow_from.len(),
                "pending_pairings": PENDING_PAIRINGS.lock().map(|g| g.len()).unwrap_or(0),
            });
            Ok(NodeResponse { ok: true, node_id: "feishu_entry".to_string(), message: None, data: Some(data), error: None })
        }
        Some("approve_pairing") => {
            let code = req
                .payload
                .as_ref()
                .and_then(|p| p.get("code"))
                .and_then(|c| c.as_str())
                .ok_or("approve_pairing requires payload.code")?;
            let open_id = approve_pairing(code)?;
            Ok(NodeResponse {
                ok: true,
                node_id: "feishu_entry".to_string(),
                message: Some(format!("paired {open_id}")),
                data: Some(json!({ "open_id": open_id })),
                error: None,
            })
        }
        Some("list_pending") => {
            let pending = PENDING_PAIRINGS.lock().unwrap_or_else(|p| p.into_inner());
            let items: Vec<Value> = pending
                .iter()
                .map(|(code, oid)| json!({ "code": code, "open_id": oid }))
                .collect();
            Ok(NodeResponse {
                ok: true,
                node_id: "feishu_entry".to_string(),
                message: None,
                data: Some(json!({ "pending": items })),
                error: None,
            })
        }
        other => Err(format!("unknown feishu_entry action: {other:?}")),
    }
}

pub(crate) fn apply_config_to_state(s: &mut FeishuState, config: &Value) {
    let g = |k: &str| config.get(k).and_then(|v| v.as_str()).map(|v| v.to_string());
    let list = |k: &str| -> Vec<String> {
        config
            .get(k)
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    };
    s.app_id = g("app_id");
    s.app_secret = g("app_secret");
    s.verification_token = g("verification_token");
    s.encrypt_key = g("encrypt_key");
    s.bot_open_id = g("bot_open_id");
    s.api_base = g("api_base");
    s.dm_policy = g("dm_policy");
    s.dm_allow_from = list("dm_allow_from");
    s.group_policy = g("group_policy");
    s.group_allow_from = list("group_allow_from");
    s.require_mention = config.get("require_mention").and_then(|v| v.as_bool());
    s.card_replies = config.get("card_replies").and_then(|v| v.as_bool());
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

    let mode = req
        .payload
        .as_ref()
        .and_then(|p| p.get("mode"))
        .and_then(|v| v.as_str())
        .unwrap_or("ws")
        .to_string();

    let mut running = SERVER_RUNNING.lock().map_err(|e| format!("lock: {e}"))?;
    if *running {
        return Ok(NodeResponse { ok: true, node_id: "feishu_serve".to_string(), message: Some(format!("already running (mode={mode})")), data: None, error: None });
    }

    let message = if mode == "webhook" {
        let port: u16 = req
            .payload
            .as_ref()
            .and_then(|p| p.get("port"))
            .and_then(|v| v.as_u64())
            .map(|v| v as u16)
            .unwrap_or(DEFAULT_PORT);
        let server = tiny_http::Server::http(format!("0.0.0.0:{port}"))
            .map_err(|e| format!("feishu_serve: cannot bind port {port}: {e}"))?;
        SERVER_SHUTDOWN.store(false, Ordering::SeqCst);
        let handle = thread::spawn(move || run_event_loop(server));
        if let Ok(mut guard) = EVENT_LOOP_HANDLE.lock() {
            *guard = Some(handle);
        }
        format!("HTTP server listening on port {port}, path /feishu/event")
    } else {
        // Long-connection (openclaw-style default): dial out to Feishu.
        // Credentials may be configured later; the loop waits for them.
        SERVER_SHUTDOWN.store(false, Ordering::SeqCst);
        let handle = thread::spawn(ws::run_ws_loop);
        if let Ok(mut guard) = EVENT_LOOP_HANDLE.lock() {
            *guard = Some(handle);
        }
        "long-connection (wss) event loop started".to_string()
    };

    *running = true;
    drop(running);
    start_agent_poller();

    Ok(NodeResponse {
        ok: true,
        node_id: "feishu_serve".to_string(),
        message: Some(message),
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
        let policy = current_policy();

        match interpret_inbound(&body, vtoken.as_deref(), ekey.as_deref(), bot.as_deref(), &policy) {
            InboundOutcome::Challenge(resp) => {
                let _ = request.respond(
                    tiny_http::Response::from_string(resp)
                        .with_header("Content-Type: application/json".parse::<tiny_http::Header>().unwrap()),
                );
            }
            InboundOutcome::Rejected(reason) => {
                eprintln!("[feishu] rejected: {reason}");
                let _ = request.respond(tiny_http::Response::from_string("unauthorized").with_status_code(401));
            }
            outcome => {
                // Always 200 first so Feishu doesn't retry.
                let _ = request.respond(tiny_http::Response::from_string(r#"{"code":0}"#));
                process_outcome(outcome);
            }
        }
    }
    eprintln!("[feishu] event loop stopped");
}

/// Shared post-parse handling for both webhook and ws modes: enqueue
/// messages, answer pairing requests, log ignores.
pub(crate) fn process_outcome(outcome: InboundOutcome) {
    match outcome {
        InboundOutcome::Message(im) | InboundOutcome::CardAction(im) => enqueue_message(im),
        InboundOutcome::Pairing(open_id, code) => {
            eprintln!("[feishu] pairing request from {open_id}: code {code}");
            // Best-effort: tell the user their pairing code. Failure is
            // non-fatal (e.g. credentials not yet configured).
            if let Err(e) = send_text_to(&format!("user:{open_id}"), &format!(
                "你还未获得使用授权。配对码：{code}\n请联系管理员执行 approve_pairing 批准。"
            )) {
                eprintln!("[feishu] pairing reply failed: {e}");
            }
        }
        InboundOutcome::Ignore(reason) => eprintln!("[feishu] ignore: {reason}"),
        // Challenge/Rejected are transport-level; handled by the caller.
        _ => {}
    }
}

/// Minimal internal text send (used for pairing replies).
fn send_text_to(target: &str, message: &str) -> Result<(), String> {
    let app_id = config_str(None, "app_id", |s| s.app_id.clone()).ok_or("no app_id configured")?;
    let app_secret =
        config_str(None, "app_secret", |s| s.app_secret.clone()).ok_or("no app_secret configured")?;
    let token = tenant_access_token(&app_id, &app_secret)?;
    let (recv_type, recv_id) = parse_target(target)?;
    feishu_send_message(
        &token,
        recv_type,
        &recv_id,
        "text",
        &json!({ "text": message }).to_string(),
        None,
    )
    .map(|_| ())
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
                if !should_process(&msg) {
                    continue;
                }
                // Two-stage card reply: post a "thinking…" card now; the
                // agent's reply will PATCH it into the final card.
                if card_replies_enabled() {
                    post_loading_card(&msg);
                }
                let envelope = build_envelope(&msg);
                cordis_plugin_sdk::agent_trigger(&envelope);
            }
            thread::sleep(Duration::from_secs(5));
        }
    });
}

fn card_replies_enabled() -> bool {
    STATE
        .lock()
        .ok()
        .and_then(|s| s.card_replies)
        .unwrap_or(true)
}

/// Post the "thinking…" loading card and remember its message_id keyed by
/// reply_target, so the agent's reply PATCHes it instead of double-posting.
fn post_loading_card(msg: &IncomingMessage) {
    let target = format!("chat:{}", msg.chat_id);
    let post = || -> Result<String, String> {
        let app_id =
            config_str(None, "app_id", |s| s.app_id.clone()).ok_or("no app_id configured")?;
        let app_secret = config_str(None, "app_secret", |s| s.app_secret.clone())
            .ok_or("no app_secret configured")?;
        let token = tenant_access_token(&app_id, &app_secret)?;
        let (recv_type, recv_id) = parse_target(&target)?;
        let data = feishu_send_message(
            &token,
            recv_type,
            &recv_id,
            "interactive",
            &loading_card().to_string(),
            None,
        )?;
        data.get("data")
            .and_then(|d| d.get("message_id"))
            .and_then(|m| m.as_str())
            .map(|s| s.to_string())
            .ok_or("no message_id in send response".to_string())
    };
    match post() {
        Ok(mid) => {
            let mut pending = PENDING_CARDS.lock().unwrap_or_else(|p| p.into_inner());
            pending.insert(target, mid);
        }
        Err(e) => eprintln!("[feishu] loading card failed (falling back to plain reply): {e}"),
    }
}

/// Skip trivial inputs. `/`-prefixed commands ARE forwarded (N批): the
/// runtime's command router handles them without the LLM, which keeps
/// /status etc. usable during a model outage. Media messages are always
/// forwarded regardless of literal text length.
fn should_process(msg: &IncomingMessage) -> bool {
    let t = msg.text.trim();
    if t.starts_with('/') {
        return t.chars().count() > 1;
    }
    t.chars().count() > 1 || msg.has_media
}

/// Build the runtime routing envelope for a message.
fn build_envelope(msg: &IncomingMessage) -> String {
    let reply_target = format!("chat:{}", msg.chat_id);
    let session_key = format!("feishu:{}", reply_target);
    let scope = if msg.chat_type == "p2p" { "private" } else { "group" };
    // Media messages carry a msg=<message_id> in the display prefix so the
    // agent can pass it to feishu_fetch_resource alongside the file_key from
    // the placeholder. Synthetic card-action ids are not real messages.
    let display = if msg.has_media && !msg.message_id.starts_with("cardaction:") {
        format!(
            "[Feishu {} from {} (user {}) msg={}]: {}",
            scope, msg.chat_id, msg.open_id, msg.message_id, msg.text
        )
    } else {
        format!(
            "[Feishu {} from {} (user {})]: {}",
            scope, msg.chat_id, msg.open_id, msg.text
        )
    };
    let mut env = json!({
        "source_plugin": "feishu",
        "reply_node": "feishu_send",
        "session_key": session_key,
        "display": display,
        "reply_target": reply_target,
        "sender_id": format!("feishu:{}", msg.open_id),
        "conversation_kind": scope,
        "message_type": msg.message_type,
        "has_media": msg.has_media,
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
        "feishu_fetch_resource" => handle_feishu_fetch_resource(req, None),
        "feishu_fetch_image" => handle_feishu_fetch_resource(req, Some("image")),
        "feishu_send_image" => handle_feishu_send_image(req),
        "feishu_get_message" => handle_feishu_get_message(req),
        "feishu_get_chat_info" => handle_feishu_get_chat_info(req),
        "feishu_list_chats" => handle_feishu_list_chats(req),
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
                "Start Feishu event intake. mode='ws' (default): long connection — dials out to Feishu (/callback/ws/endpoint), no public URL needed. mode='webhook': HTTP server receiving event callbacks at POST /feishu/event (url_verification challenge, token check, optional AES decryption). Both modes apply the access policy (dm_policy/group_policy/require_mention/pairing), dedup, and forward accepted messages to the agent.",
                json!({"type":"object","properties":{"node_id":{"const":"feishu_serve"},"action":{"type":"string","enum":["stop"]},"payload":{"type":"object","properties":{"mode":{"type":"string","enum":["ws","webhook"]},"port":{"type":"integer"}}}}}),
                json!({"type":"object","properties":{"ok":{"type":"boolean"},"message":{"type":"string"}}}),
                &["network:listen","network:feishu-api"],
                &["port bind failure","bootstrap auth failure"],
            ),
            node_doc(
                "feishu_send",
                "Send a message to a Feishu chat or user. target is 'chat:<chat_id>' or 'user:<open_id>'. Provide either `message` (text) or `card` (interactive card JSON). Optional `reply_to` quote-replies to a message_id. If a pending 'thinking…' card exists for the target (two-stage replies) the content PATCHes that card; payload.update_message_id forces updating a specific message.",
                json!({"type":"object","required":["node_id","target"],"properties":{"node_id":{"const":"feishu_send"},"target":{"type":"string"},"message":{"type":"string"},"card":{"type":"object"},"reply_to":{"type":"string"},"payload":{"type":"object","properties":{"update_message_id":{"type":"string"}}}}}),
                json!({"type":"object","properties":{"ok":{"type":"boolean"},"data":{"type":"object"}}}),
                &["network:feishu-api"],
                &["auth failure","invalid target"],
            ).with_agent_accessible(),
            node_doc(
                "feishu_entry",
                "Configure the Feishu plugin or manage access. action='configure' with payload (app_id/app_secret/bot_open_id/api_base/verification_token/encrypt_key + policies: dm_policy open|allowlist|pairing, dm_allow_from[], group_policy open|allowlist|disabled, group_allow_from[], require_mention, card_replies). action='status' for a summary. action='approve_pairing' with payload.code approves a pending DM pairing. action='list_pending' lists pending pairings.",
                json!({"type":"object","required":["node_id"],"properties":{"node_id":{"const":"feishu_entry"},"action":{"type":"string","enum":["configure","status","approve_pairing","list_pending"]},"payload":{"type":"object"}}}),
                json!({"type":"object","properties":{"ok":{"type":"boolean"}}}),
                &["config write"],
                &["missing fields"],
            ),
            node_doc(
                "feishu_fetch_resource",
                "Download a Feishu message resource (image or file) to a local temp file and return its path. Use the file_key from an inbound placeholder, and take message_id from the display prefix's msg=<id>. type='image' for image messages and post img runs (image_key); type='file' for file/audio/video/sticker messages (file_key). Optional as_base64=true inlines base64 for resources <=1MB.",
                json!({"type":"object","required":["node_id","payload"],"properties":{"node_id":{"const":"feishu_fetch_resource"},"payload":{"type":"object","required":["message_id","file_key"],"properties":{"message_id":{"type":"string"},"file_key":{"type":"string"},"type":{"type":"string","enum":["image","file"]},"as_base64":{"type":"boolean"}}}}}),
                json!({"type":"object","properties":{"ok":{"type":"boolean"},"data":{"type":"object","properties":{"path":{"type":"string"},"size":{"type":"integer"},"mime":{"type":"string"},"base64":{"type":"string"}}}}}),
                &["network:feishu-api","file write (temp)"],
                &["auth failure","resource not found","synthetic cardaction message_id","resource too large (>20MB)"],
            ).with_agent_accessible(),
            node_doc(
                "feishu_fetch_image",
                "Download an image resource (type is forced to 'image') to a local temp file and return its path. Same as feishu_fetch_resource with type='image'; use the file_key from an '[image file_key=...]' placeholder and message_id from the display prefix's msg=<id>.",
                json!({"type":"object","required":["node_id","payload"],"properties":{"node_id":{"const":"feishu_fetch_image"},"payload":{"type":"object","required":["message_id","file_key"],"properties":{"message_id":{"type":"string"},"file_key":{"type":"string"},"as_base64":{"type":"boolean"}}}}}),
                json!({"type":"object","properties":{"ok":{"type":"boolean"},"data":{"type":"object","properties":{"path":{"type":"string"},"size":{"type":"integer"},"mime":{"type":"string"}}}}}),
                &["network:feishu-api","file write (temp)"],
                &["auth failure","resource not found","synthetic cardaction message_id","resource too large (>20MB)"],
            ).with_agent_accessible(),
            node_doc(
                "feishu_send_image",
                "Send an image to a Feishu chat or user. target is 'chat:<chat_id>' or 'user:<open_id>'. Provide either image_key (already uploaded) or path (a local file under the system temp directory, uploaded first). Optional reply_to quote-replies. URLs are not supported.",
                json!({"type":"object","required":["node_id","target"],"properties":{"node_id":{"const":"feishu_send_image"},"target":{"type":"string"},"reply_to":{"type":"string"},"payload":{"type":"object","properties":{"image_key":{"type":"string"},"path":{"type":"string"},"target":{"type":"string"},"reply_to":{"type":"string"}}}}}),
                json!({"type":"object","properties":{"ok":{"type":"boolean"},"data":{"type":"object","properties":{"image_key":{"type":"string"},"message_id":{"type":"string"}}}}}),
                &["network:feishu-api"],
                &["auth failure","invalid target","resource too large (>20MB)","path outside temp directory"],
            ).with_agent_accessible(),
            node_doc(
                "feishu_get_message",
                "Fetch a message by message_id (im/v1/messages/{message_id}). Synthetic cardaction ids are rejected.",
                json!({"type":"object","required":["node_id","payload"],"properties":{"node_id":{"const":"feishu_get_message"},"payload":{"type":"object","required":["message_id"],"properties":{"message_id":{"type":"string"}}}}}),
                json!({"type":"object","properties":{"ok":{"type":"boolean"},"data":{"type":"object"}}}),
                &["network:feishu-api"],
                &["auth failure","resource not found","synthetic cardaction message_id"],
            ).with_agent_accessible(),
            node_doc(
                "feishu_get_chat_info",
                "Fetch chat/group info by chat_id (im/v1/chats/{chat_id}).",
                json!({"type":"object","required":["node_id","payload"],"properties":{"node_id":{"const":"feishu_get_chat_info"},"payload":{"type":"object","required":["chat_id"],"properties":{"chat_id":{"type":"string"}}}}}),
                json!({"type":"object","properties":{"ok":{"type":"boolean"},"data":{"type":"object"}}}),
                &["network:feishu-api"],
                &["auth failure","resource not found"],
            ).with_agent_accessible(),
            node_doc(
                "feishu_list_chats",
                "List chats the bot belongs to (im/v1/chats). Optional page_size and page_token for pagination.",
                json!({"type":"object","required":["node_id"],"properties":{"node_id":{"const":"feishu_list_chats"},"payload":{"type":"object","properties":{"page_size":{"type":"integer"},"page_token":{"type":"string"}}}}}),
                json!({"type":"object","properties":{"ok":{"type":"boolean"},"data":{"type":"object"}}}),
                &["network:feishu-api"],
                &["auth failure"],
            ).with_agent_accessible(),
        ],
        Some(
            "You are chatting in Feishu (Lark). Each incoming message is prefixed with its source, e.g. \"[Feishu group from <chat_id> (user <open_id>)]: <text>\".\n\
Your FINAL output each turn MUST be exactly one JSON object:\n\
  {\"action\":\"respond\",\"message\":\"<reply text>\"}  — to reply to the chat, OR\n\
  {\"action\":\"suspend\"}  — when no reply is warranted.\n\
The runtime routes your \"respond\" back to the originating Feishu chat automatically; do NOT call feishu_send yourself for the direct reply.\n\
To proactively send an extra message (e.g. a progress update) to a chat: invoke_plugin(feishu, feishu_send, {\"node_id\":\"feishu_send\",\"target\":\"chat:<chat_id>\",\"message\":\"<text>\"}).\n\
When you receive a media placeholder like \"[image file_key=...]\", \"[file file_key=... name=...]\", \"[audio ...]\", \"[video ...]\" or \"[sticker ...]\", the resource is not inlined: read the message_id from the display prefix's msg=<message_id>, then call invoke_plugin(feishu, feishu_fetch_resource, {\"node_id\":\"feishu_fetch_resource\",\"payload\":{\"message_id\":\"<id>\",\"file_key\":\"<key>\",\"type\":\"image|file\"}}) to get a local {path}. Use type=image for image placeholders (image messages and post img runs) and type=file for file/audio/video/sticker. Then pass that path to vision_ocr / vision_describe for images.\n\
Keep replies concise and helpful; a short honest \"not sure\" beats a long wrong answer.",
        ),
    )
}

fn abi_fingerprint_value() -> AbiFingerprint {
    AbiFingerprint::current_build("crate_feishu_v1", "api_v2")
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

    /// Fully-open policy: what the pre-policy tests assumed (any DM, any
    /// group, mention still required via explicit require_mention).
    fn open_policy() -> AccessPolicy {
        AccessPolicy {
            dm_policy: "open".to_string(),
            dm_allow_from: Vec::new(),
            group_policy: "open".to_string(),
            group_allow_from: Vec::new(),
            require_mention: Some(true),
        }
    }

    #[test]
    fn challenge_handshake_echoes_challenge() {
        let body = json!({"type":"url_verification","challenge":"abc123","token":"vtok"}).to_string();
        match interpret_inbound(&body, Some("vtok"), None, None, &open_policy()) {
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
            interpret_inbound(&body, Some("vtok"), None, None, &open_policy()),
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
            interpret_inbound(&body, Some("vtok"), None, None, &open_policy()),
            InboundOutcome::Rejected(_)
        ));
    }

    #[test]
    fn p2p_message_is_actionable() {
        let body = message_event("p2p", "oc_1", "om_1", "hello there", &[]).to_string();
        match interpret_inbound(&body, Some("vtok"), None, Some("ou_bot"), &open_policy()) {
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
            interpret_inbound(&body, Some("vtok"), None, Some("ou_bot"), &open_policy()),
            InboundOutcome::Ignore(_)
        ));
    }

    #[test]
    fn group_message_with_at_bot_is_actionable() {
        let body = message_event("group", "oc_3", "om_3", "@_user_1 hi bot", &["ou_bot"]).to_string();
        match interpret_inbound(&body, Some("vtok"), None, Some("ou_bot"), &open_policy()) {
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
            interpret_inbound(&body, Some("vtok"), None, Some("ou_bot"), &open_policy()),
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
            mentioned: true,
            message_type: "text".to_string(),
            has_media: false,
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
        match interpret_inbound(&body, Some("vtok"), None, None, &open_policy()) {
            InboundOutcome::CardAction(im) => {
                assert_eq!(im.chat_id, "oc_card");
                assert_eq!(im.text, "button clicked");
            }
            _ => panic!("expected CardAction"),
        }
    }

    // ---- Access-policy matrix -------------------------------------------

    fn msg(chat_type: &str, chat_id: &str, open_id: &str, mentioned: bool) -> IncomingMessage {
        IncomingMessage {
            chat_type: chat_type.to_string(),
            chat_id: chat_id.to_string(),
            open_id: open_id.to_string(),
            text: "hi".to_string(),
            message_id: "om_t".to_string(),
            root_id: None,
            mentioned,
            message_type: "text".to_string(),
            has_media: false,
        }
    }

    // envelope 必须携带身份字段（soul 作用域依赖）。
    #[test]
    fn build_envelope_carries_identity() {
        let env: Value =
            serde_json::from_str(&build_envelope(&msg("p2p", "oc_1", "ou_abc", false))).unwrap();
        assert_eq!(env["sender_id"], "feishu:ou_abc");
        assert_eq!(env["conversation_kind"], "private");
        let env: Value =
            serde_json::from_str(&build_envelope(&msg("group", "oc_2", "ou_abc", true))).unwrap();
        assert_eq!(env["conversation_kind"], "group");
    }

    #[test]
    fn dm_open_allows_anyone() {
        let p = AccessPolicy { dm_policy: "open".into(), ..AccessPolicy::default() };
        assert!(matches!(
            policy_gate(msg("p2p", "oc", "ou_stranger", false), &p),
            InboundOutcome::Message(_)
        ));
    }

    #[test]
    fn dm_allowlist_gates_unknown_users() {
        let p = AccessPolicy {
            dm_policy: "allowlist".into(),
            dm_allow_from: vec!["ou_friend".into()],
            ..AccessPolicy::default()
        };
        assert!(matches!(
            policy_gate(msg("p2p", "oc", "ou_friend", false), &p),
            InboundOutcome::Message(_)
        ));
        assert!(matches!(
            policy_gate(msg("p2p", "oc", "ou_stranger", false), &p),
            InboundOutcome::Ignore(_)
        ));
    }

    #[test]
    fn dm_pairing_issues_code_for_unknown_then_allows_after_approve() {
        let p = AccessPolicy::default(); // dm_policy = pairing
        let outcome = policy_gate(msg("p2p", "oc", "ou_new", false), &p);
        let code = match outcome {
            InboundOutcome::Pairing(oid, code) => {
                assert_eq!(oid, "ou_new");
                assert_eq!(code.len(), 6);
                code
            }
            _ => panic!("expected Pairing"),
        };
        // Same user retries → same code (deterministic).
        match policy_gate(msg("p2p", "oc", "ou_new", false), &p) {
            InboundOutcome::Pairing(_, code2) => assert_eq!(code2, code),
            _ => panic!("expected Pairing again"),
        }
        // Known users pass without pairing.
        let p_known = AccessPolicy {
            dm_allow_from: vec!["ou_new".into()],
            ..AccessPolicy::default()
        };
        assert!(matches!(
            policy_gate(msg("p2p", "oc", "ou_new", false), &p_known),
            InboundOutcome::Message(_)
        ));
    }

    #[test]
    fn group_disabled_ignores_everything() {
        let p = AccessPolicy { group_policy: "disabled".into(), ..AccessPolicy::default() };
        assert!(matches!(
            policy_gate(msg("group", "oc_g", "ou", true), &p),
            InboundOutcome::Ignore(_)
        ));
    }

    #[test]
    fn group_allowlist_gates_unknown_groups() {
        let p = AccessPolicy {
            group_policy: "allowlist".into(),
            group_allow_from: vec!["oc_ok".into()],
            ..AccessPolicy::default()
        };
        assert!(matches!(
            policy_gate(msg("group", "oc_ok", "ou", true), &p),
            InboundOutcome::Message(_)
        ));
        assert!(matches!(
            policy_gate(msg("group", "oc_other", "ou", true), &p),
            InboundOutcome::Ignore(_)
        ));
    }

    #[test]
    fn group_open_waives_mention_by_default() {
        let p = AccessPolicy {
            group_policy: "open".into(),
            require_mention: None,
            ..AccessPolicy::default()
        };
        // openclaw: requireMention defaults false when policy is open.
        assert!(matches!(
            policy_gate(msg("group", "oc_any", "ou", false), &p),
            InboundOutcome::Message(_)
        ));
        // Explicit require_mention=true still gates.
        let p2 = AccessPolicy { require_mention: Some(true), ..p };
        assert!(matches!(
            policy_gate(msg("group", "oc_any", "ou", false), &p2),
            InboundOutcome::Ignore(_)
        ));
    }

    #[test]
    fn group_allowlist_requires_mention_by_default() {
        let p = AccessPolicy {
            group_policy: "allowlist".into(),
            group_allow_from: vec!["oc_ok".into()],
            require_mention: None,
            ..AccessPolicy::default()
        };
        assert!(matches!(
            policy_gate(msg("group", "oc_ok", "ou", false), &p),
            InboundOutcome::Ignore(_)
        ));
    }

    #[test]
    fn approve_pairing_unknown_code_errors() {
        assert!(approve_pairing("999999xxx").is_err());
    }

    #[test]
    fn cards_have_expected_shape() {
        let lc = loading_card();
        assert!(lc["elements"][0]["text"]["content"].as_str().unwrap().contains("思考中"));
        let fc = final_card("**done**");
        assert_eq!(fc["elements"][0]["tag"], "markdown");
        assert_eq!(fc["elements"][0]["content"], "**done**");
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

    // Build a media im.message.receive_v1 event with an explicit
    // message_type and a content object (serialized to the JSON string that
    // Feishu delivers).
    fn media_event(
        chat_type: &str,
        chat_id: &str,
        msg_id: &str,
        message_type: &str,
        content_json: Value,
    ) -> Value {
        json!({
            "header": {"event_type":"im.message.receive_v1","token":"vtok"},
            "event": {
                "sender": {"sender_id": {"open_id":"ou_sender"}},
                "message": {
                    "chat_id": chat_id,
                    "chat_type": chat_type,
                    "message_id": msg_id,
                    "message_type": message_type,
                    "content": content_json.to_string()
                }
            }
        })
    }

    #[test]
    fn image_message_becomes_placeholder_and_is_actionable() {
        let body = media_event(
            "p2p",
            "oc_img",
            "om_img",
            "image",
            json!({"image_key": "img_key_1"}),
        )
        .to_string();
        match interpret_inbound(&body, Some("vtok"), None, Some("ou_bot"), &open_policy()) {
            InboundOutcome::Message(im) => {
                assert_eq!(im.text, "[image file_key=img_key_1]");
                assert!(im.has_media);
                assert_eq!(im.message_type, "image");
            }
            _ => panic!("expected Message"),
        }
    }

    #[test]
    fn image_message_passes_should_process() {
        let (text, has_media) = extract_content("image", &json!({"image_key": "k"}));
        let im = IncomingMessage {
            chat_type: "p2p".into(),
            chat_id: "oc".into(),
            open_id: "ou".into(),
            text,
            message_id: "om_x".into(),
            root_id: None,
            mentioned: false,
            message_type: "image".into(),
            has_media,
        };
        assert!(should_process(&im));
    }

    #[test]
    fn post_message_renders_text_at_and_image() {
        let content = json!({
            "title": "标题",
            "content": [
                [
                    {"tag":"text","text":"hello "},
                    {"tag":"at","user_name":"Alice","user_id":"ou_a"},
                    {"tag":"text","text":" see "},
                    {"tag":"img","image_key":"img_in_post"}
                ]
            ]
        });
        let (text, has_media) = extract_content("post", &content);
        assert!(has_media);
        assert_eq!(text, "标题\nhello @Alice see [image file_key=img_in_post]");
    }

    #[test]
    fn post_at_falls_back_to_user_id() {
        let content = json!({
            "content": [[{"tag":"at","user_id":"ou_only"}]]
        });
        let (text, has_media) = extract_content("post", &content);
        assert!(!has_media);
        assert_eq!(text, "@ou_only");
    }

    #[test]
    fn file_audio_media_sticker_placeholders() {
        let (t, m) = extract_content("file", &json!({"file_key":"fk","file_name":"a.pdf"}));
        assert!(m);
        assert_eq!(t, "[file file_key=fk name=\"a.pdf\"]");
        let (t, m) = extract_content("audio", &json!({"file_key":"ak","duration":1200}));
        assert!(m);
        assert_eq!(t, "[audio file_key=ak duration=1200ms]");
        let (t, m) = extract_content("media", &json!({"file_key":"vk","file_name":"v.mp4"}));
        assert!(m);
        assert_eq!(t, "[video file_key=vk name=\"v.mp4\"]");
        let (t, m) = extract_content("sticker", &json!({"file_key":"sk"}));
        assert!(m);
        assert_eq!(t, "[sticker file_key=sk]");
    }

    #[test]
    fn unknown_message_type_is_forwarded_as_unsupported() {
        let body = media_event("p2p", "oc_u", "om_u", "share_chat", json!({"chat_id":"x"}))
            .to_string();
        match interpret_inbound(&body, Some("vtok"), None, Some("ou_bot"), &open_policy()) {
            InboundOutcome::Message(im) => {
                assert_eq!(im.text, "[unsupported message_type=share_chat]");
                assert!(im.has_media);
            }
            _ => panic!("expected Message"),
        }
    }

    #[test]
    fn extract_content_malformed_does_not_panic() {
        // Missing keys degrade to empty placeholders.
        assert_eq!(extract_content("image", &Value::Null), ("[image file_key=]".to_string(), true));
        assert_eq!(
            extract_content("file", &json!({})),
            ("[file file_key= name=\"\"]".to_string(), true)
        );
        assert_eq!(extract_content("text", &Value::Null), (String::new(), false));
        // A post with no content array is just an empty string.
        assert_eq!(extract_content("post", &json!({})), (String::new(), false));
    }

    #[test]
    fn media_message_envelope_carries_msg_id() {
        let im = IncomingMessage {
            chat_type: "group".into(),
            chat_id: "oc_m".into(),
            open_id: "ou_m".into(),
            text: "[image file_key=k]".into(),
            message_id: "om_media".into(),
            root_id: None,
            mentioned: true,
            message_type: "image".into(),
            has_media: true,
        };
        let env: Value = serde_json::from_str(&build_envelope(&im)).unwrap();
        assert!(env["display"].as_str().unwrap().contains("msg=om_media"));
    }

    #[test]
    fn text_message_envelope_has_no_msg_prefix() {
        let im = IncomingMessage {
            chat_type: "group".into(),
            chat_id: "oc_t".into(),
            open_id: "ou_t".into(),
            text: "hello".into(),
            message_id: "om_t2".into(),
            root_id: None,
            mentioned: true,
            message_type: "text".into(),
            has_media: false,
        };
        let env: Value = serde_json::from_str(&build_envelope(&im)).unwrap();
        assert!(!env["display"].as_str().unwrap().contains("msg="));
    }

    #[test]
    fn download_rejects_cardaction_id_without_network() {
        // The cardaction guard is the first check, so no network is touched.
        let err = feishu_download_resource("http://127.0.0.1:0", "tok", "cardaction:foo", "k", "image")
            .unwrap_err();
        assert!(err.contains("cardaction"));
        let err = feishu_get_message_api("http://127.0.0.1:0", "tok", "cardaction:foo").unwrap_err();
        assert!(err.contains("cardaction"));
    }

    #[test]
    fn guess_ext_sniffs_known_magics() {
        assert_eq!(guess_ext(b"\x89PNG\r\n\x1a\n"), "png");
        assert_eq!(guess_ext(b"\xFF\xD8\xFF\xE0"), "jpg");
        assert_eq!(guess_ext(b"GIF89a"), "gif");
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&[0, 0, 0, 0]);
        webp.extend_from_slice(b"WEBP");
        assert_eq!(guess_ext(&webp), "webp");
        assert_eq!(guess_ext(b"\x00\x01\x02\x03"), "bin");
    }

    #[test]
    fn temp_resource_path_is_unique_per_call() {
        let a = temp_resource_path("png");
        let b = temp_resource_path("png");
        assert_ne!(a, b);
        assert!(a.starts_with(std::env::temp_dir()));
    }

    #[test]
    fn multipart_image_body_shape() {
        let boundary = "BOUND123";
        let bytes = b"\x89PNGdata";
        let body = build_multipart_image_body(boundary, bytes, "png");
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("--BOUND123\r\n"));
        assert!(text.contains("name=\"image_type\""));
        assert!(text.contains("\r\n\r\nmessage\r\n"));
        assert!(text.contains("name=\"image\"; filename=\"image.png\""));
        assert!(text.contains("Content-Type: application/octet-stream"));
        assert!(text.ends_with("--BOUND123--\r\n"));
        // The raw bytes are embedded verbatim.
        let needle = b"\r\n\r\n\x89PNGdata\r\n";
        assert!(body.windows(needle.len()).any(|w| w == needle));
    }
}
