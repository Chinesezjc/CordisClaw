//! Vision plugin — OCR and AI image understanding.
//!
//! Nodes:
//! - `vision_ocr`      — download an image URL and run tesseract OCR (text extraction)
//! - `vision_describe` — send image to OpenAI-compatible vision API for AI description
//!
//! Safety: only http/https URLs are allowed; localhost and private IPs blocked.

use cordis_plugin_sdk::{
    export_plugin_api, json_response, plugin_docs, task_node_doc, AbiFingerprint, PluginRequest,
    PluginResponse,
};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::Read;
use std::net::{IpAddr, ToSocketAddrs};
use std::process::{Command, Stdio};
use std::time::Duration;

const TIMEOUT_SECS: u64 = 10;
const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024; // 20 MB

// ---------------------------------------------------------------------------
// SSRF guard (P0-22)
// ---------------------------------------------------------------------------
//
// Duplicated from `fixtures/plugins/web/src/lib.rs` — see P2-2 in the review
// plan for the plan to extract this into a shared `cordis-plugin-net` util
// crate. Keep the two in lock-step until the extraction lands.

fn ip_is_forbidden(ip: IpAddr) -> Option<&'static str> {
    match ip {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            if v4.is_loopback() {
                return Some("loopback address");
            }
            if v4.is_private() {
                return Some("RFC1918 private address");
            }
            if v4.is_link_local() {
                return Some("link-local address (cloud metadata surface)");
            }
            if v4.is_broadcast() || v4.is_unspecified() || v4.is_multicast() {
                return Some("special-purpose address");
            }
            if octets[0] == 100 && (octets[1] & 0b1100_0000) == 0b0100_0000 {
                return Some("CGNAT (100.64/10) address");
            }
            if octets[0] == 0 {
                return Some("0.0.0.0/8 address");
            }
            None
        }
        IpAddr::V6(v6) => {
            if v6.is_loopback() {
                return Some("loopback address");
            }
            if v6.is_unspecified() || v6.is_multicast() {
                return Some("special-purpose address");
            }
            let seg = v6.segments();
            if seg[0] == 0
                && seg[1] == 0
                && seg[2] == 0
                && seg[3] == 0
                && seg[4] == 0
                && seg[5] == 0xffff
            {
                let mapped = std::net::Ipv4Addr::new(
                    (seg[6] >> 8) as u8,
                    (seg[6] & 0xff) as u8,
                    (seg[7] >> 8) as u8,
                    (seg[7] & 0xff) as u8,
                );
                return ip_is_forbidden(IpAddr::V4(mapped));
            }
            if (seg[0] & 0xfe00) == 0xfc00 {
                return Some("IPv6 ULA (fc00::/7)");
            }
            if (seg[0] & 0xffc0) == 0xfe80 {
                return Some("IPv6 link-local (fe80::/10)");
            }
            None
        }
    }
}

fn check_url_safety(url_str: &str) -> Result<(), String> {
    let parsed = url::Url::parse(url_str).map_err(|_| format!("invalid URL: {url_str}"))?;
    let scheme = parsed.scheme();
    if scheme != "http" && scheme != "https" {
        return Err(format!("only http/https allowed, got: {scheme}"));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| format!("URL missing host: {url_str}"))?;
    if let Ok(ip) = host.parse::<IpAddr>() {
        if let Some(reason) = ip_is_forbidden(ip) {
            return Err(format!("host {host} is forbidden ({reason})"));
        }
        return Ok(());
    }
    let addrs = (host, 0u16)
        .to_socket_addrs()
        .map_err(|e| format!("DNS resolution failed for {host}: {e}"))?;
    let mut saw_any = false;
    for sa in addrs {
        saw_any = true;
        if let Some(reason) = ip_is_forbidden(sa.ip()) {
            return Err(format!(
                "host {host} resolves to forbidden address {}: {reason}",
                sa.ip()
            ));
        }
    }
    if !saw_any {
        return Err(format!("host {host} did not resolve to any address"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Request / Response
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct VisionRequest {
    /// "vision_ocr" | "vision_describe"
    node_id: String,

    /// Image URL to download and process
    #[serde(default)]
    url: Option<String>,

    /// For vision_describe: optional prompt override (default: "Describe this image in detail")
    #[serde(default)]
    prompt: Option<String>,

    /// For vision_ocr: language override (default "chi_sim+eng")
    #[serde(default)]
    lang: Option<String>,
}

#[derive(Debug, Serialize)]
struct VisionResponse {
    ok: bool,
    node_id: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    truncated: Option<bool>,

    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

// ---------------------------------------------------------------------------
// Image download
// ---------------------------------------------------------------------------

fn download_image(url_str: &str) -> Result<Vec<u8>, String> {
    // P0-22: full SSRF check (see the module-level guard for coverage).
    // Note: `ureq::AgentBuilder` here does not follow redirects by default
    // for `.call()`, so a single check on `url_str` covers the wire path.
    check_url_safety(url_str)?;

    let resp = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .redirects(0) // block redirects; caller must re-issue if needed
        .build()
        .get(url_str)
        .call()
        .map_err(|e| format!("HTTP request: {e}"))?;

    let content_length: usize = resp
        .header("Content-Length")
        .and_then(|v: &str| v.parse().ok())
        .unwrap_or(0);

    if content_length > MAX_IMAGE_BYTES {
        return Err(format!(
            "image too large: {content_length} bytes (max {MAX_IMAGE_BYTES})"
        ));
    }

    let mut bytes: Vec<u8> = Vec::new();
    resp.into_reader()
        .take((MAX_IMAGE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("read body: {e}"))?;

    if bytes.len() > MAX_IMAGE_BYTES {
        return Err(format!(
            "image too large: {} bytes (max {MAX_IMAGE_BYTES})",
            bytes.len()
        ));
    }

    Ok(bytes)
}

// ---------------------------------------------------------------------------
// vision_ocr — tesseract
// ---------------------------------------------------------------------------

fn guess_mime(data: &[u8]) -> &'static str {
    if data.len() >= 4 && &data[..4] == b"\x89PNG" {
        "png"
    } else if data.len() >= 3 && &data[..3] == b"\xFF\xD8\xFF" {
        "jpg"
    } else if data.len() >= 4 && &data[..4] == b"RIFF" && data.len() >= 12 && &data[8..12] == b"WEBP" {
        "webp"
    } else if data.len() >= 3 && &data[..3] == b"GIF" {
        "gif"
    } else if data.len() >= 2 && &data[..2] == b"BM" {
        "bmp"
    } else {
        "png" // default
    }
}

fn vision_ocr(url: &str, lang: &str) -> Result<String, String> {
    let data = download_image(url)?;
    let ext = guess_mime(&data);

    // P0-27: previously `cordis_ocr_<pid>.<ext>` — concurrent callers within
    // the same process clobber each other's file, so agent A reads agent B's
    // image. Append a monotonic in-process counter + a nanosecond timestamp
    // so every filename is unique.
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp_dir = std::env::temp_dir();
    let tmp_path = tmp_dir.join(format!(
        "cordis_ocr_{}_{seq:x}_{nanos:x}.{ext}",
        std::process::id()
    ));
    std::fs::write(&tmp_path, &data)
        .map_err(|e| format!("write temp file: {e}"))?;

    // Run tesseract
    let child = Command::new("tesseract")
        .arg(&tmp_path)
        .arg("stdout")
        .arg("-l")
        .arg(lang)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("spawn tesseract (is it installed?): {e}"))?;

    let output = child
        .wait_with_output()
        .map_err(|e| format!("wait tesseract: {e}"))?;

    // Clean up temp file
    let _ = std::fs::remove_file(&tmp_path);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("tesseract failed: {stderr}"));
    }

    let text = String::from_utf8_lossy(&output.stdout)
        .trim()
        .to_string();

    if text.is_empty() {
        return Err("tesseract returned no text (maybe no text in image, or language pack missing)".to_string());
    }

    Ok(text)
}

// ---------------------------------------------------------------------------
// vision_describe — OpenAI-compatible vision API
// ---------------------------------------------------------------------------

fn vision_describe(url: &str, prompt: &str) -> Result<(String, String), String> {
    let api_key = std::env::var("OPENAI_API_KEY")
        .or_else(|_| std::env::var("VISION_API_KEY"))
        .map_err(|_| {
            "OPENAI_API_KEY or VISION_API_KEY environment variable not set".to_string()
        })?;

    let base_url = std::env::var("OPENAI_BASE_URL")
        .or_else(|_| std::env::var("VISION_BASE_URL"))
        .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());

    let model = std::env::var("VISION_MODEL")
        .unwrap_or_else(|_| "gpt-4o-mini".to_string());

    let data = download_image(url)?;
    let mime = guess_mime(&data);
    let data_url = format!(
        "data:image/{};base64,{}",
        mime,
        base64::engine::general_purpose::STANDARD.encode(&data)
    );

    let body = json!({
        "model": model,
        "messages": [
            {
                "role": "user",
                "content": [
                    {
                        "type": "text",
                        "text": prompt
                    },
                    {
                        "type": "image_url",
                        "image_url": {
                            "url": data_url,
                            "detail": "auto"
                        }
                    }
                ]
            }
        ],
        "max_tokens": 1024
    });

    let body_str = serde_json::to_string(&body)
        .map_err(|e| format!("serialize request: {e}"))?;

    let resp = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .build()
        .post(&format!("{base_url}/chat/completions"))
        .set("Authorization", &format!("Bearer {api_key}"))
        .set("Content-Type", "application/json")
        .send_string(&body_str)
        .map_err(|e| format!("API request: {e}"))?;

    let status = resp.status();
    let mut resp_body = String::new();
    resp.into_reader()
        .read_to_string(&mut resp_body)
        .map_err(|e| format!("read response: {e}"))?;

    if status != 200 {
        return Err(format!("API error ({status}): {resp_body}"));
    }

    let json: Value =
        serde_json::from_str(&resp_body).map_err(|e| format!("parse response JSON: {e}"))?;

    let text = json["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| "unexpected API response format".to_string())?
        .to_string();

    Ok((text, model))
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn handle(req: &VisionRequest) -> Result<VisionResponse, String> {
    match req.node_id.as_str() {
        "vision_ocr" => {
            let url = req.url.as_deref().unwrap_or("").trim();
            if url.is_empty() {
                return Err("url is required for vision_ocr".to_string());
            }
            let lang = req.lang.as_deref().unwrap_or("chi_sim+eng");
            match vision_ocr(url, lang) {
                Ok(text) => Ok(VisionResponse {
                    ok: true,
                    node_id: "vision_ocr".to_string(),
                    text: Some(text),
                    truncated: None,
                    model: None,
                    error: None,
                }),
                Err(e) => Ok(VisionResponse {
                    ok: false,
                    node_id: "vision_ocr".to_string(),
                    text: None,
                    truncated: None,
                    model: None,
                    error: Some(e),
                }),
            }
        }
        "vision_describe" => {
            let url = req.url.as_deref().unwrap_or("").trim();
            if url.is_empty() {
                return Err("url is required for vision_describe".to_string());
            }
            let prompt = req
                .prompt
                .as_deref()
                .unwrap_or("Describe this image in detail. What do you see? Reply in Chinese if the image contains Chinese text or context.");
            match vision_describe(url, prompt) {
                Ok((text, model)) => Ok(VisionResponse {
                    ok: true,
                    node_id: "vision_describe".to_string(),
                    text: Some(text),
                    truncated: None,
                    model: Some(model),
                    error: None,
                }),
                Err(e) => Ok(VisionResponse {
                    ok: false,
                    node_id: "vision_describe".to_string(),
                    text: None,
                    truncated: None,
                    model: None,
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
        "vision",
        "vision",
        "0.1.0",
        None,
        vec![
            task_node_doc(
                "vision_ocr",
                "Download an image from URL and run tesseract OCR to extract text. Requires tesseract installed on the system. Default language: chi_sim+eng.",
                json!({
                    "type": "object",
                    "required": ["node_id", "url"],
                    "properties": {
                        "node_id": { "type": "string", "const": "vision_ocr" },
                        "url": { "type": "string", "description": "Image URL to OCR" },
                        "lang": { "type": "string", "description": "Tesseract language code (default: chi_sim+eng)" }
                    }
                }),
                json!({
                    "type": "object",
                    "properties": {
                        "ok": { "type": "boolean" },
                        "text": { "type": ["string", "null"] },
                        "error": { "type": ["string", "null"] }
                    }
                }),
                &["downloads image from URL", "runs tesseract OCR"],
                &["tesseract not installed", "network error", "language pack missing", "no text in image"],
            ),
            task_node_doc(
                "vision_describe",
                "Download an image from URL and send to an OpenAI-compatible vision API (default: gpt-4o-mini) for AI-powered description. Requires OPENAI_API_KEY (or VISION_API_KEY) env var. Supports OPENAI_BASE_URL for custom endpoints.",
                json!({
                    "type": "object",
                    "required": ["node_id", "url"],
                    "properties": {
                        "node_id": { "type": "string", "const": "vision_describe" },
                        "url": { "type": "string", "description": "Image URL to analyze" },
                        "prompt": { "type": "string", "description": "Custom prompt for the vision model" }
                    }
                }),
                json!({
                    "type": "object",
                    "properties": {
                        "ok": { "type": "boolean" },
                        "text": { "type": ["string", "null"] },
                        "model": { "type": ["string", "null"] },
                        "error": { "type": ["string", "null"] }
                    }
                }),
                &["downloads image from URL", "sends to OpenAI-compatible vision API"],
                &["API key not set", "network error", "rate limited", "API quota exceeded", "image too large"],
            ),
        ],
    None
    )
}

fn abi_fingerprint_value() -> AbiFingerprint {
    AbiFingerprint {
        rustc_version: "1.85.1".to_string(),
        target_triple: "x86_64-unknown-linux-gnu".to_string(),
        crate_hash: "crate_vision_v1".to_string(),
        api_hash: "api_v2".to_string(),
    }
}

fn api_handle(req: PluginRequest) -> PluginResponse {
    match serde_json::from_str::<VisionRequest>(&req.payload)
        .map_err(|e| format!("vision plugin: {e}"))
        .and_then(|r| handle(&r))
    {
        Ok(resp) => json_response(&resp),
        Err(e) => json_response(&VisionResponse {
            ok: false,
            node_id: "error".to_string(),
            text: None,
            truncated: None,
            model: None,
            error: Some(e),
        }),
    }
}

export_plugin_api! {
    abi_fingerprint = abi_fingerprint_value(),
    docs = docs_value(),
    handle = api_handle,
}
