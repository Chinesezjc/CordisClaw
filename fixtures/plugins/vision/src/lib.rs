//! Vision plugin — OCR and AI image understanding.
//!
//! Nodes:
//! - `vision_ocr`      — download an image URL and run tesseract OCR (text extraction)
//! - `vision_describe` — send image to OpenAI-compatible vision API for AI description
//!
//! Safety: only http/https URLs are allowed; localhost and private IPs blocked.

use base64::Engine;
use cordis_plugin_sdk::{
    export_plugin_api, json_response, plugin_docs, task_node_doc, AbiFingerprint, PluginRequest,
    PluginResponse,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::Duration;

const TIMEOUT_SECS: u64 = 10;
const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024; // 20 MB

// ---------------------------------------------------------------------------
// SSRF guard (P0-22)
// ---------------------------------------------------------------------------
//
// P2-2: moved to the shared `cordis-net` crate — this plugin now imports
// the same guard as `web`. Adding a new outbound-HTTP plugin? Depend on
// `cordis-net`.
use cordis_net::check_url_safety;

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

    /// Local image file path. Must resolve to a location inside the system
    /// temp directory. Mutually exclusive with `url`.
    #[serde(default)]
    path: Option<String>,

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
// Local file source (P2: consume feishu_fetch_resource temp files)
// ---------------------------------------------------------------------------

/// Read a local image file. Access is restricted to the system temp directory
/// so callers can only hand off files that a trusted producer (e.g. the feishu
/// plugin's `feishu_fetch_resource`) placed under `std::env::temp_dir()`.
///
/// On macOS both `/tmp` and `/var/folders/...` are symlinks, so the requested
/// path and the temp-dir prefix are each canonicalized before comparison.
fn read_local_image(path: &str) -> Result<Vec<u8>, String> {
    let real = std::fs::canonicalize(path).map_err(|e| format!("resolve path {path}: {e}"))?;

    let temp_root = std::fs::canonicalize(std::env::temp_dir())
        .map_err(|e| format!("resolve temp dir: {e}"))?;

    if !real.starts_with(&temp_root) {
        return Err(format!(
            "path outside temp dir: {} (must be under {})",
            real.display(),
            temp_root.display()
        ));
    }

    let meta = std::fs::metadata(&real).map_err(|e| format!("stat {}: {e}", real.display()))?;
    if meta.len() > MAX_IMAGE_BYTES as u64 {
        return Err(format!(
            "image too large: {} bytes (max {MAX_IMAGE_BYTES})",
            meta.len()
        ));
    }

    std::fs::read(&real).map_err(|e| format!("read {}: {e}", real.display()))
}

/// Load image bytes from exactly one of `url` or `path`. Supplying both is an
/// error; supplying neither is an error. `path` reads a local temp-dir file,
/// `url` goes through the SSRF-guarded HTTP download.
fn load_source(url: Option<&str>, path: Option<&str>) -> Result<Vec<u8>, String> {
    let url = url.map(str::trim).filter(|s| !s.is_empty());
    let path = path.map(str::trim).filter(|s| !s.is_empty());
    match (url, path) {
        (Some(_), Some(_)) => Err("provide either url or path, not both".to_string()),
        (Some(u), None) => download_image(u),
        (None, Some(p)) => read_local_image(p),
        (None, None) => Err("either url or path is required".to_string()),
    }
}

// ---------------------------------------------------------------------------
// vision_ocr — tesseract
// ---------------------------------------------------------------------------

fn guess_mime(data: &[u8]) -> &'static str {
    if data.len() >= 4 && &data[..4] == b"\x89PNG" {
        "png"
    } else if data.len() >= 3 && &data[..3] == b"\xFF\xD8\xFF" {
        "jpg"
    } else if data.len() >= 4
        && &data[..4] == b"RIFF"
        && data.len() >= 12
        && &data[8..12] == b"WEBP"
    {
        "webp"
    } else if data.len() >= 3 && &data[..3] == b"GIF" {
        "gif"
    } else if data.len() >= 2 && &data[..2] == b"BM" {
        "bmp"
    } else {
        "png" // default
    }
}

fn vision_ocr(url: Option<&str>, path: Option<&str>, lang: &str) -> Result<String, String> {
    let data = load_source(url, path)?;
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
    std::fs::write(&tmp_path, &data).map_err(|e| format!("write temp file: {e}"))?;

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

    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();

    if text.is_empty() {
        return Err(
            "tesseract returned no text (maybe no text in image, or language pack missing)"
                .to_string(),
        );
    }

    Ok(text)
}

// ---------------------------------------------------------------------------
// vision_describe — OpenAI-compatible vision API
// ---------------------------------------------------------------------------

fn vision_describe(
    url: Option<&str>,
    path: Option<&str>,
    prompt: &str,
) -> Result<(String, String), String> {
    let api_key = std::env::var("OPENAI_API_KEY")
        .or_else(|_| std::env::var("VISION_API_KEY"))
        .map_err(|_| "OPENAI_API_KEY or VISION_API_KEY environment variable not set".to_string())?;

    let base_url = std::env::var("OPENAI_BASE_URL")
        .or_else(|_| std::env::var("VISION_BASE_URL"))
        .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());

    let model = std::env::var("VISION_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_string());

    let data = load_source(url, path)?;
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

    let body_str = serde_json::to_string(&body).map_err(|e| format!("serialize request: {e}"))?;

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
            let lang = req.lang.as_deref().unwrap_or("chi_sim+eng");
            match vision_ocr(req.url.as_deref(), req.path.as_deref(), lang) {
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
            let prompt = req
                .prompt
                .as_deref()
                .unwrap_or("Describe this image in detail. What do you see? Reply in Chinese if the image contains Chinese text or context.");
            match vision_describe(req.url.as_deref(), req.path.as_deref(), prompt) {
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
                "Run tesseract OCR on an image to extract text. Supply the image with either `url` (downloaded over HTTP) or `path` (a local file under the system temp directory); provide exactly one of them. Requires tesseract installed on the system. Default language: chi_sim+eng.",
                json!({
                    "type": "object",
                    "required": ["node_id"],
                    "properties": {
                        "node_id": { "type": "string", "const": "vision_ocr" },
                        "url": { "type": "string", "description": "Image URL to OCR (provide url or path, not both)" },
                        "path": { "type": "string", "description": "Absolute path to a local image file. Must be located under the system temp directory. Provide url or path, not both." },
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
                &["reads image from url or local temp path", "runs tesseract OCR"],
                &["tesseract not installed", "network error", "language pack missing", "no text in image", "path outside temp dir"],
            ),
            task_node_doc(
                "vision_describe",
                "Send an image to an OpenAI-compatible vision API (default: gpt-4o-mini) for AI-powered description. Supply the image with either `url` (downloaded over HTTP) or `path` (a local file under the system temp directory); provide exactly one of them. Requires OPENAI_API_KEY (or VISION_API_KEY) env var. Supports OPENAI_BASE_URL for custom endpoints.",
                json!({
                    "type": "object",
                    "required": ["node_id"],
                    "properties": {
                        "node_id": { "type": "string", "const": "vision_describe" },
                        "url": { "type": "string", "description": "Image URL to analyze (provide url or path, not both)" },
                        "path": { "type": "string", "description": "Absolute path to a local image file. Must be located under the system temp directory. Provide url or path, not both." },
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
                &["reads image from url or local temp path", "sends to OpenAI-compatible vision API"],
                &["API key not set", "network error", "rate limited", "API quota exceeded", "image too large", "path outside temp dir"],
            ),
        ],
    None
    )
}

fn abi_fingerprint_value() -> AbiFingerprint {
    AbiFingerprint::current_build("crate_vision_v1", "api_v2")
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Build a unique path inside the temp dir without touching the filesystem.
    fn unique_temp_path(tag: &str) -> std::path::PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!(
            "cordis_vision_test_{tag}_{}_{seq:x}_{nanos:x}.bin",
            std::process::id()
        ))
    }

    #[test]
    fn read_local_image_rejects_path_outside_temp_dir() {
        // Cargo.toml lives in the crate directory, not the temp dir.
        let outside = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
        let err = read_local_image(outside).unwrap_err();
        assert!(
            err.contains("path outside temp dir"),
            "expected temp-dir rejection, got: {err}"
        );
    }

    #[test]
    fn read_local_image_reads_temp_file() {
        let path = unique_temp_path("read");
        let payload: &[u8] = b"\x89PNG\r\n\x1a\nfake image bytes";
        std::fs::write(&path, payload).expect("write temp file");

        let result = read_local_image(path.to_str().unwrap());
        let _ = std::fs::remove_file(&path);

        assert_eq!(result.expect("read back"), payload);
    }

    #[test]
    fn read_local_image_errors_on_missing_path() {
        let path = unique_temp_path("missing");
        // Never created — canonicalize must fail.
        let err = read_local_image(path.to_str().unwrap()).unwrap_err();
        assert!(
            err.contains("resolve path"),
            "expected resolve failure, got: {err}"
        );
    }

    #[test]
    fn load_source_rejects_both_url_and_path() {
        let err = load_source(Some("https://example.com/a.png"), Some("/tmp/x.png")).unwrap_err();
        assert_eq!(err, "provide either url or path, not both");
    }

    #[test]
    fn load_source_rejects_neither_url_nor_path() {
        let err = load_source(None, None).unwrap_err();
        assert_eq!(err, "either url or path is required");

        // Empty / whitespace strings are treated as absent.
        let err = load_source(Some("   "), Some("")).unwrap_err();
        assert_eq!(err, "either url or path is required");
    }

    #[test]
    fn load_source_path_branch_reads_temp_file() {
        let path = unique_temp_path("load");
        let payload: &[u8] = b"local via load_source";
        std::fs::write(&path, payload).expect("write temp file");

        let result = load_source(None, Some(path.to_str().unwrap()));
        let _ = std::fs::remove_file(&path);

        assert_eq!(result.expect("load_source path branch"), payload);
    }
}
