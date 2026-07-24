//! Feishu long-connection (WSS) event mode — openclaw-style default.
//!
//! Instead of exposing a public webhook URL, the plugin dials out to
//! Feishu: `POST /callback/ws/endpoint` with app credentials returns a
//! one-shot wss:// URL; events then arrive as protobuf `Frame`s over the
//! WebSocket and are ACKed in-band. No verification_token / encrypt_key
//! is needed in this mode (the channel itself is authenticated).
//!
//! Protocol (mirrors larksuite/oapi-sdk-go v3 `ws/`):
//! - Frame is proto2: SeqID(1) LogID(2) service(3) method(4)
//!   headers(5, repeated Header{key,value}) payload_encoding(6)
//!   payload_type(7) payload(8) LogIDNew(9). method 0=control 1=data.
//! - Control frames: we send `type=ping` every PingInterval; pong may
//!   carry a JSON ClientConfig payload that retunes intervals.
//! - Data frames: headers carry type(event/card), message_id, sum, seq,
//!   trace_id. sum>1 → reassemble by seq. After handling, write
//!   `{"code":200,...}` back into the same frame's payload and send it
//!   as the ACK.
//! - Reconnect: jitter 0..ReconnectNonce seconds, then retry every
//!   ReconnectInterval (ReconnectCount<0 = infinite).

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::{
    api_base, current_policy, interpret_inbound, process_outcome, InboundOutcome, SERVER_SHUTDOWN,
    STATE,
};

// ---------------------------------------------------------------------------
// Protobuf Frame (hand-rolled proto2 codec; 9 fields, varint + bytes only)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Frame {
    pub seq_id: u64,
    pub log_id: u64,
    pub service: i32,
    pub method: i32,
    pub headers: Vec<(String, String)>,
    pub payload_encoding: String,
    pub payload_type: String,
    pub payload: Vec<u8>,
    pub log_id_new: String,
}

pub(crate) const METHOD_CONTROL: i32 = 0;
pub(crate) const METHOD_DATA: i32 = 1;

fn put_varint(buf: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            buf.push(b);
            break;
        }
        buf.push(b | 0x80);
    }
}

fn put_tag(buf: &mut Vec<u8>, field: u32, wire: u32) {
    put_varint(buf, ((field << 3) | wire) as u64);
}

fn put_bytes(buf: &mut Vec<u8>, field: u32, data: &[u8]) {
    put_tag(buf, field, 2);
    put_varint(buf, data.len() as u64);
    buf.extend_from_slice(data);
}

fn get_varint(data: &[u8], pos: &mut usize) -> Result<u64, String> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    loop {
        let b = *data.get(*pos).ok_or("varint: unexpected EOF")?;
        *pos += 1;
        result |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
        if shift >= 64 {
            return Err("varint: overflow".to_string());
        }
    }
}

fn get_chunk<'a>(data: &'a [u8], pos: &mut usize) -> Result<&'a [u8], String> {
    let len = get_varint(data, pos)? as usize;
    let end = pos.checked_add(len).ok_or("chunk: length overflow")?;
    if end > data.len() {
        return Err("chunk: unexpected EOF".to_string());
    }
    let out = &data[*pos..end];
    *pos = end;
    Ok(out)
}

impl Frame {
    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(64 + self.payload.len());
        // required varints — always emitted (proto2 required)
        put_tag(&mut buf, 1, 0);
        put_varint(&mut buf, self.seq_id);
        put_tag(&mut buf, 2, 0);
        put_varint(&mut buf, self.log_id);
        put_tag(&mut buf, 3, 0);
        put_varint(&mut buf, self.service as i64 as u64);
        put_tag(&mut buf, 4, 0);
        put_varint(&mut buf, self.method as i64 as u64);
        for (k, v) in &self.headers {
            let mut h = Vec::with_capacity(k.len() + v.len() + 8);
            put_bytes(&mut h, 1, k.as_bytes());
            put_bytes(&mut h, 2, v.as_bytes());
            put_bytes(&mut buf, 5, &h);
        }
        if !self.payload_encoding.is_empty() {
            put_bytes(&mut buf, 6, self.payload_encoding.as_bytes());
        }
        if !self.payload_type.is_empty() {
            put_bytes(&mut buf, 7, self.payload_type.as_bytes());
        }
        if !self.payload.is_empty() {
            put_bytes(&mut buf, 8, &self.payload);
        }
        if !self.log_id_new.is_empty() {
            put_bytes(&mut buf, 9, self.log_id_new.as_bytes());
        }
        buf
    }

    pub(crate) fn decode(data: &[u8]) -> Result<Frame, String> {
        let mut f = Frame::default();
        let mut pos = 0usize;
        while pos < data.len() {
            let tag = get_varint(data, &mut pos)?;
            let field = (tag >> 3) as u32;
            let wire = (tag & 0x7) as u32;
            match (field, wire) {
                (1, 0) => f.seq_id = get_varint(data, &mut pos)?,
                (2, 0) => f.log_id = get_varint(data, &mut pos)?,
                (3, 0) => f.service = get_varint(data, &mut pos)? as i64 as i32,
                (4, 0) => f.method = get_varint(data, &mut pos)? as i64 as i32,
                (5, 2) => {
                    let chunk = get_chunk(data, &mut pos)?;
                    let mut hp = 0usize;
                    let mut key = String::new();
                    let mut value = String::new();
                    while hp < chunk.len() {
                        let htag = get_varint(chunk, &mut hp)?;
                        match (htag >> 3, htag & 0x7) {
                            (1, 2) => {
                                key =
                                    String::from_utf8_lossy(get_chunk(chunk, &mut hp)?).to_string()
                            }
                            (2, 2) => {
                                value =
                                    String::from_utf8_lossy(get_chunk(chunk, &mut hp)?).to_string()
                            }
                            (_, 2) => {
                                let _ = get_chunk(chunk, &mut hp)?;
                            }
                            (_, 0) => {
                                let _ = get_varint(chunk, &mut hp)?;
                            }
                            _ => return Err("header: unsupported wire type".to_string()),
                        }
                    }
                    f.headers.push((key, value));
                }
                (6, 2) => {
                    f.payload_encoding =
                        String::from_utf8_lossy(get_chunk(data, &mut pos)?).to_string()
                }
                (7, 2) => {
                    f.payload_type = String::from_utf8_lossy(get_chunk(data, &mut pos)?).to_string()
                }
                (8, 2) => f.payload = get_chunk(data, &mut pos)?.to_vec(),
                (9, 2) => {
                    f.log_id_new = String::from_utf8_lossy(get_chunk(data, &mut pos)?).to_string()
                }
                (_, 0) => {
                    let _ = get_varint(data, &mut pos)?;
                }
                (_, 2) => {
                    let _ = get_chunk(data, &mut pos)?;
                }
                _ => return Err(format!("frame: unsupported wire type {wire}")),
            }
        }
        Ok(f)
    }

    fn header(&self, key: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    fn header_int(&self, key: &str) -> i64 {
        self.header(key).and_then(|v| v.parse().ok()).unwrap_or(0)
    }

    fn set_header(&mut self, key: &str, value: String) {
        if let Some(slot) = self.headers.iter_mut().find(|(k, _)| k == key) {
            slot.1 = value;
        } else {
            self.headers.push((key.to_string(), value));
        }
    }
}

pub(crate) fn ping_frame(service_id: i32) -> Frame {
    Frame {
        method: METHOD_CONTROL,
        service: service_id,
        headers: vec![("type".to_string(), "ping".to_string())],
        ..Frame::default()
    }
}

// ---------------------------------------------------------------------------
// Bootstrap: exchange credentials for a one-shot wss:// URL
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) struct WsClientConfig {
    /// <0 = infinite retries.
    pub reconnect_count: i64,
    pub reconnect_interval: Duration,
    /// Max jitter (seconds) before the first reconnect attempt.
    pub reconnect_nonce: u64,
    pub ping_interval: Duration,
}

impl Default for WsClientConfig {
    fn default() -> Self {
        WsClientConfig {
            reconnect_count: -1,
            reconnect_interval: Duration::from_secs(120),
            reconnect_nonce: 30,
            ping_interval: Duration::from_secs(120),
        }
    }
}

fn apply_client_config(cfg: &mut WsClientConfig, v: &Value) {
    if let Some(n) = v.get("ReconnectCount").and_then(|x| x.as_i64()) {
        cfg.reconnect_count = n;
    }
    if let Some(n) = v.get("ReconnectInterval").and_then(|x| x.as_u64()) {
        if n > 0 {
            cfg.reconnect_interval = Duration::from_secs(n);
        }
    }
    if let Some(n) = v.get("ReconnectNonce").and_then(|x| x.as_u64()) {
        cfg.reconnect_nonce = n;
    }
    if let Some(n) = v.get("PingInterval").and_then(|x| x.as_u64()) {
        if n > 0 {
            cfg.ping_interval = Duration::from_secs(n);
        }
    }
}

/// POST /callback/ws/endpoint → (wss url, server-pushed client config).
/// A non-retryable credential error (403 Forbidden) is distinguished so the
/// outer loop can back off much longer instead of hammering.
pub(crate) enum BootstrapError {
    /// Bad credentials / forbidden — retrying fast won't help.
    Fatal(String),
    /// Transient (network, system busy) — retry on the normal schedule.
    Transient(String),
}

pub(crate) fn bootstrap_endpoint(
    app_id: &str,
    app_secret: &str,
) -> Result<(String, WsClientConfig), BootstrapError> {
    let url = format!("{}/callback/ws/endpoint", api_base());
    let resp = ureq::post(&url)
        .set("locale", "zh")
        .set("Content-Type", "application/json")
        .send_json(json!({ "AppID": app_id, "AppSecret": app_secret }));
    let body: Value = match resp {
        Ok(r) => r
            .into_json()
            .map_err(|e| BootstrapError::Transient(format!("endpoint parse: {e}")))?,
        Err(ureq::Error::Status(status, r)) => {
            let text = r.into_string().unwrap_or_default();
            let msg = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("msg").and_then(|m| m.as_str()).map(|s| s.to_string()))
                .unwrap_or_else(|| "system busy".to_string());
            return Err(if status == 403 {
                BootstrapError::Fatal(format!("endpoint {status}: {msg}"))
            } else {
                BootstrapError::Transient(format!("endpoint {status}: {msg}"))
            });
        }
        Err(e) => return Err(BootstrapError::Transient(format!("endpoint request: {e}"))),
    };

    let code = body.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    let msg = body
        .get("msg")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string();
    match code {
        0 => {}
        // 1 = system busy, 1000040343 = internal — transient.
        1 | 1000040343 => {
            return Err(BootstrapError::Transient(format!(
                "endpoint code={code}: {msg}"
            )))
        }
        // 403 forbidden / 514 auth failed / anything else client-side.
        _ => {
            return Err(BootstrapError::Fatal(format!(
                "endpoint code={code}: {msg}"
            )))
        }
    }

    let endpoint = body
        .get("data")
        .and_then(|d| d.get("URL"))
        .and_then(|u| u.as_str())
        .ok_or_else(|| BootstrapError::Transient("endpoint: empty URL".to_string()))?
        .to_string();

    let mut cfg = WsClientConfig::default();
    if let Some(cc) = body.get("data").and_then(|d| d.get("ClientConfig")) {
        apply_client_config(&mut cfg, cc);
    }
    Ok((endpoint, cfg))
}

/// Pull `service_id` out of the wss URL's query string (no url crate).
pub(crate) fn service_id_from_url(url: &str) -> i32 {
    url.split_once('?')
        .map(|(_, q)| q)
        .unwrap_or("")
        .split('&')
        .find_map(|kv| kv.strip_prefix("service_id="))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Fragment reassembly (sum/seq, 5s window)
// ---------------------------------------------------------------------------

/// Per-message reassembly window: arrival time + slot per fragment.
type FragmentWindow = (Instant, Vec<Option<Vec<u8>>>);

pub(crate) struct Reassembler {
    parts: HashMap<String, FragmentWindow>,
}

impl Reassembler {
    pub(crate) fn new() -> Self {
        Reassembler {
            parts: HashMap::new(),
        }
    }

    /// Feed one fragment; returns the full payload once all parts arrived.
    pub(crate) fn feed(
        &mut self,
        msg_id: &str,
        sum: usize,
        seq: usize,
        data: Vec<u8>,
    ) -> Option<Vec<u8>> {
        if sum <= 1 {
            return Some(data);
        }
        // GC expired windows.
        self.parts
            .retain(|_, (t, _)| t.elapsed() < Duration::from_secs(5));

        let entry = self
            .parts
            .entry(msg_id.to_string())
            .or_insert_with(|| (Instant::now(), vec![None; sum]));
        if seq < entry.1.len() {
            entry.1[seq] = Some(data);
        }
        if entry.1.iter().all(|p| p.is_some()) {
            let (_, parts) = self.parts.remove(msg_id).unwrap();
            let mut full = Vec::new();
            for p in parts {
                full.extend_from_slice(&p.unwrap());
            }
            Some(full)
        } else {
            None
        }
    }
}

// ---------------------------------------------------------------------------
// The long-connection loop
// ---------------------------------------------------------------------------

fn now_jitter_ms(max_secs: u64) -> u64 {
    if max_secs == 0 {
        return 0;
    }
    // Pseudo-random from the clock; good enough for reconnect jitter.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    nanos % (max_secs * 1000)
}

fn sleep_interruptible(total: Duration) {
    let deadline = Instant::now() + total;
    while Instant::now() < deadline {
        if SERVER_SHUTDOWN.load(Ordering::SeqCst) {
            return;
        }
        std::thread::sleep(Duration::from_millis(200).min(deadline - Instant::now()));
    }
}

/// Outer supervisor: bootstrap → connect → pump; reconnect forever until
/// SERVER_SHUTDOWN. Runs on its own thread (spawned by `feishu_serve`).
pub(crate) fn run_ws_loop() {
    let mut announced_waiting = false;
    loop {
        if SERVER_SHUTDOWN.load(Ordering::SeqCst) {
            break;
        }

        // Credentials may arrive later via `feishu_entry configure`; idle
        // politely until they do.
        let (app_id, app_secret) = {
            let s = STATE.lock().unwrap_or_else(|p| p.into_inner());
            (s.app_id.clone(), s.app_secret.clone())
        };
        let (app_id, app_secret) = match (app_id, app_secret) {
            (Some(i), Some(s)) if !i.is_empty() && !s.is_empty() => (i, s),
            _ => {
                if !announced_waiting {
                    eprintln!("[feishu-ws] no app_id/app_secret configured; waiting (configure via feishu_entry)");
                    announced_waiting = true;
                }
                sleep_interruptible(Duration::from_secs(30));
                // Re-hydrate from disk in case configure ran in another process.
                if let Some(config) = crate::load_runtime_config() {
                    if let Ok(mut s) = STATE.lock() {
                        crate::apply_config_to_state(&mut s, &config);
                    }
                }
                continue;
            }
        };
        announced_waiting = false;

        let (endpoint, cfg) = match bootstrap_endpoint(&app_id, &app_secret) {
            Ok(v) => v,
            Err(BootstrapError::Fatal(e)) => {
                eprintln!("[feishu-ws] bootstrap fatal: {e}; retrying in 10min");
                sleep_interruptible(Duration::from_secs(600));
                continue;
            }
            Err(BootstrapError::Transient(e)) => {
                eprintln!("[feishu-ws] bootstrap failed: {e}; retrying in 30s");
                sleep_interruptible(Duration::from_secs(30));
                continue;
            }
        };

        match pump_connection(&endpoint, cfg.clone()) {
            Ok(()) => break, // clean shutdown
            Err(e) => {
                eprintln!("[feishu-ws] connection lost: {e}");
                let jitter = now_jitter_ms(cfg.reconnect_nonce);
                sleep_interruptible(Duration::from_millis(jitter));
                // The bootstrap URL is one-shot; each retry re-bootstraps,
                // so the outer loop IS the reconnect loop. Space attempts
                // by a short fixed delay rather than cfg.reconnect_interval
                // (which governs redial of the SAME url in the Go SDK).
                sleep_interruptible(Duration::from_secs(2));
            }
        }
    }
    eprintln!("[feishu-ws] loop stopped");
}

/// One connection lifetime: dial, then single-threaded read/ping/ack pump.
/// Returns Ok(()) only on orderly shutdown.
fn pump_connection(endpoint: &str, cfg: WsClientConfig) -> Result<(), String> {
    use tungstenite::stream::MaybeTlsStream;

    let (mut socket, _resp) = tungstenite::connect(endpoint).map_err(|e| format!("dial: {e}"))?;

    // Short read timeout so one thread can pump reads AND emit pings.
    match socket.get_ref() {
        MaybeTlsStream::Plain(s) => {
            let _ = s.set_read_timeout(Some(Duration::from_millis(500)));
        }
        MaybeTlsStream::Rustls(s) => {
            let _ = s.sock.set_read_timeout(Some(Duration::from_millis(500)));
        }
        _ => {}
    }

    let service_id = service_id_from_url(endpoint);
    eprintln!("[feishu-ws] connected (service_id={service_id})");

    let mut cfg = cfg;
    let mut reassembler = Reassembler::new();
    let mut last_ping = Instant::now();
    // Fire an immediate ping so the server learns we're alive.
    let _ = socket.send(tungstenite::Message::Binary(
        ping_frame(service_id).encode(),
    ));

    loop {
        if SERVER_SHUTDOWN.load(Ordering::SeqCst) {
            let _ = socket.close(None);
            return Ok(());
        }

        if last_ping.elapsed() >= cfg.ping_interval {
            socket
                .send(tungstenite::Message::Binary(
                    ping_frame(service_id).encode(),
                ))
                .map_err(|e| format!("ping: {e}"))?;
            last_ping = Instant::now();
        }

        let msg = match socket.read() {
            Ok(m) => m,
            Err(tungstenite::Error::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue;
            }
            Err(e) => return Err(format!("read: {e}")),
        };

        let data = match msg {
            tungstenite::Message::Binary(b) => b,
            tungstenite::Message::Ping(p) => {
                let _ = socket.send(tungstenite::Message::Pong(p));
                continue;
            }
            tungstenite::Message::Close(_) => return Err("server closed".to_string()),
            _ => continue,
        };

        let frame = match Frame::decode(&data) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("[feishu-ws] bad frame: {e}");
                continue;
            }
        };

        match frame.method {
            METHOD_CONTROL => {
                if frame.header("type") == Some("pong") && !frame.payload.is_empty() {
                    if let Ok(v) = serde_json::from_slice::<Value>(&frame.payload) {
                        apply_client_config(&mut cfg, &v);
                    }
                }
            }
            METHOD_DATA => {
                if let Some(ack) = handle_data_frame(frame, &mut reassembler) {
                    socket
                        .send(tungstenite::Message::Binary(ack))
                        .map_err(|e| format!("ack: {e}"))?;
                }
            }
            _ => {}
        }
    }
}

/// Process a data frame; returns the encoded ACK frame to send back
/// (None while waiting for more fragments).
fn handle_data_frame(mut frame: Frame, reassembler: &mut Reassembler) -> Option<Vec<u8>> {
    let sum = frame.header_int("sum").max(1) as usize;
    let seq = frame.header_int("seq").max(0) as usize;
    let msg_id = frame.header("message_id").unwrap_or("").to_string();
    let msg_type = frame.header("type").unwrap_or("").to_string();

    let payload = reassembler.feed(&msg_id, sum, seq, std::mem::take(&mut frame.payload))?;

    let started = Instant::now();
    if msg_type == "event" || msg_type == "card" {
        let body = String::from_utf8_lossy(&payload).to_string();
        let bot = {
            let s = STATE.lock().unwrap_or_else(|p| p.into_inner());
            s.bot_open_id.clone()
        };
        let policy = current_policy();
        // Long-connection frames arrive over an authenticated channel:
        // no verification_token / encrypt_key checks apply.
        let outcome = interpret_inbound(&body, None, None, bot.as_deref(), &policy);
        if let InboundOutcome::Rejected(reason) | InboundOutcome::Ignore(reason) = &outcome {
            eprintln!("[feishu-ws] event skipped: {reason}");
        }
        process_outcome(outcome);
    }

    // ACK: same frame, payload swapped for a response envelope.
    frame.set_header("biz_rt", started.elapsed().as_millis().to_string());
    frame.payload = json!({"code": 200, "headers": null, "data": null})
        .to_string()
        .into_bytes();
    Some(frame.encode())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip_preserves_all_fields() {
        let f = Frame {
            seq_id: 42,
            log_id: 7,
            service: 123456,
            method: METHOD_DATA,
            headers: vec![
                ("type".to_string(), "event".to_string()),
                ("message_id".to_string(), "om_x".to_string()),
                ("sum".to_string(), "1".to_string()),
            ],
            payload_encoding: "utf-8".to_string(),
            payload_type: "json".to_string(),
            payload: br#"{"hello":"world"}"#.to_vec(),
            log_id_new: "lid".to_string(),
        };
        let decoded = Frame::decode(&f.encode()).unwrap();
        assert_eq!(decoded, f);
    }

    #[test]
    fn ping_frame_shape() {
        let f = ping_frame(99);
        assert_eq!(f.method, METHOD_CONTROL);
        assert_eq!(f.service, 99);
        assert_eq!(f.header("type"), Some("ping"));
        // Round-trips even with empty optional fields.
        let decoded = Frame::decode(&f.encode()).unwrap();
        assert_eq!(decoded.header("type"), Some("ping"));
        assert_eq!(decoded.service, 99);
    }

    #[test]
    fn varint_multibyte_roundtrip() {
        let mut buf = Vec::new();
        for v in [
            0u64,
            1,
            127,
            128,
            300,
            16383,
            16384,
            u32::MAX as u64,
            u64::MAX,
        ] {
            buf.clear();
            put_varint(&mut buf, v);
            let mut pos = 0;
            assert_eq!(get_varint(&buf, &mut pos).unwrap(), v);
            assert_eq!(pos, buf.len());
        }
    }

    #[test]
    fn reassembler_combines_fragments_in_any_order() {
        let mut r = Reassembler::new();
        assert!(r.feed("m1", 3, 2, b"C".to_vec()).is_none());
        assert!(r.feed("m1", 3, 0, b"A".to_vec()).is_none());
        let full = r.feed("m1", 3, 1, b"B".to_vec()).unwrap();
        assert_eq!(full, b"ABC");
        // Single-part messages pass straight through.
        assert_eq!(r.feed("m2", 1, 0, b"solo".to_vec()).unwrap(), b"solo");
    }

    #[test]
    fn service_id_parsed_from_url() {
        assert_eq!(
            service_id_from_url("wss://x.feishu.cn/ws?device_id=abc&service_id=777"),
            777
        );
        assert_eq!(service_id_from_url("wss://x.feishu.cn/ws"), 0);
    }

    #[test]
    fn decode_skips_unknown_fields() {
        // A frame with an extra unknown varint field (field 15) must still decode.
        let mut buf = Frame {
            seq_id: 1,
            method: METHOD_CONTROL,
            ..Frame::default()
        }
        .encode();
        put_tag(&mut buf, 15, 0);
        put_varint(&mut buf, 12345);
        let decoded = Frame::decode(&buf).unwrap();
        assert_eq!(decoded.seq_id, 1);
    }
}
