//! Image generation plugin — text-to-image and base-image editing through the
//! OpenAI-compatible `/v1/images/generations` and `/v1/images/edits` endpoints.
//!
//! Nodes:
//! - `generate_image` — render one prompt to a file on disk and return the
//!   absolute path plus a ready-to-send `[CQ:image,file=…]` snippet.
//! - `edit_image` — POST a local base image plus a prompt as
//!   `multipart/form-data` to `/images/edits` (restyle, composite, overlay
//!   text) and return the same result shape as `generate_image`.
//!
//! Why a plugin and not a kernel tool: this runtime has no shell tool, so an
//! HTTP image API is only reachable from the agent through a plugin node. The
//! node owns the wire details (endpoint, bearer token, base64 decoding, file
//! naming); the kernel keeps owning the agent loop.
//!
//! # Configuration (environment, all optional)
//!
//! - `CORDIS_IMAGE_BASE_URL` … base of the OpenAI-compatible API, default
//!   `http://127.0.0.1:8792/v1` (the modelhub gateway tunnel this deployment
//!   forwards to MomoiAiri).
//! - `CORDIS_IMAGE_API_KEY_ENV` … name of the env var holding the caller token,
//!   default `DEEPSEEK_GATEWAY_TOKEN`.
//! - `CORDIS_IMAGE_MODEL` … default model id, default `gpt-image-2.5-flare`.
//! - `CORDIS_IMAGE_OUTPUT_DIR` … output directory, default `data/images`
//!   (relative to the service working directory).
//! - `CORDIS_IMAGE_MEDIA_DIR` … content-addressed cache, default `data/media`.
//!   Every rendered image is also written there as `<sha256>.png`, so the
//!   agent can ask the kernel's `view_image` tool for `sha256:<digest>` even
//!   though it has no filesystem write access of its own.
//! - `CORDIS_IMAGE_TIMEOUT_SECS` … per-request timeout, default 300.
//!
//! # Delivery to QQ
//!
//! The node never sends anything itself. The agent takes the returned
//! `cq_image` string and passes it to `qq_send`; NapCat resolves the absolute
//! path on the same host (verified 2026-11-12 with `send_private_msg`).

use base64::Engine;
use cordis_plugin_sdk::{
    export_plugin_api, json_response, node_doc, plugin_docs, AbiFingerprint, PluginRequest,
    PluginResponse,
};
use reqwest::blocking::Client;
use reqwest::header::CONTENT_TYPE;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DEFAULT_BASE_URL: &str = "http://127.0.0.1:8792/v1";
const DEFAULT_KEY_ENV: &str = "DEEPSEEK_GATEWAY_TOKEN";
const DEFAULT_MODEL: &str = "gpt-image-2.5-flare";
const DEFAULT_OUTPUT_DIR: &str = "data/images";
/// The kernel's `view_image` cache (see `cordis_runtime::image_cache`).
const DEFAULT_MEDIA_DIR: &str = "data/media";
const DEFAULT_TIMEOUT_SECS: u64 = 300;
/// Refuse absurd responses instead of writing multi-hundred-MB files.
const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;
/// How much of an upstream error body is echoed back to the agent.
const ERROR_BODY_PREVIEW: usize = 400;

const MODEL_FLARE: &str = "gpt-image-2.5-flare";
const MODEL_SUNBURST: &str = "gpt-image-2.5-sunburst";

static FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Deserialize)]
pub struct NodeRequest {
    node_id: String,
    #[serde(default)]
    prompt: Option<String>,
    /// Base image for `edit_image`: a local path inside `data/media/` or the
    /// image output directory.
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    size: Option<String>,
    #[serde(default)]
    quality: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Config {
    base_url: String,
    key_env: String,
    default_model: String,
    output_dir: PathBuf,
    media_dir: PathBuf,
    timeout: Duration,
}

impl Config {
    fn from_env() -> Self {
        Config {
            base_url: env_or("CORDIS_IMAGE_BASE_URL", DEFAULT_BASE_URL),
            key_env: env_or("CORDIS_IMAGE_API_KEY_ENV", DEFAULT_KEY_ENV),
            default_model: env_or("CORDIS_IMAGE_MODEL", DEFAULT_MODEL),
            output_dir: PathBuf::from(env_or("CORDIS_IMAGE_OUTPUT_DIR", DEFAULT_OUTPUT_DIR)),
            media_dir: PathBuf::from(env_or("CORDIS_IMAGE_MEDIA_DIR", DEFAULT_MEDIA_DIR)),
            timeout: Duration::from_secs(
                env_or("CORDIS_IMAGE_TIMEOUT_SECS", &DEFAULT_TIMEOUT_SECS.to_string())
                    .parse()
                    .unwrap_or(DEFAULT_TIMEOUT_SECS),
            ),
        }
    }
}

fn env_or(name: &str, fallback: &str) -> String {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

/// Resolve the friendly aliases the agent is told to use into real model ids.
fn resolve_model(requested: Option<&str>, default_model: &str) -> String {
    let raw = requested.map(str::trim).filter(|value| !value.is_empty());
    match raw.map(|value| value.to_ascii_lowercase()) {
        None => default_model.to_string(),
        Some(alias) if alias == "flare" => MODEL_FLARE.to_string(),
        Some(alias) if alias == "sunburst" => MODEL_SUNBURST.to_string(),
        Some(_) => raw.unwrap_or_default().to_string(),
    }
}

/// `base` + `/images/<sub>`, tolerating a trailing slash on the base.
///
/// `generate_image` uses `generations` and `edit_image` uses `edits`; both hang
/// off the same OpenAI-compatible prefix, so one helper serves both.
fn endpoint_for(base_url: &str, sub: &str) -> String {
    format!("{}/images/{sub}", base_url.trim_end_matches('/'))
}

/// Multipart boundary for one `/images/edits` request.
///
/// Nothing upstream parses the boundary back to us, so a timestamp, the pid and
/// the shared counter are enough to keep two concurrent requests apart.
fn multipart_boundary() -> String {
    let counter = FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("image-gen-{}-{}-{counter}", now_millis(), std::process::id())
}

/// Declared media type of the file part, sniffed from the bytes themselves.
fn mime_for(bytes: &[u8]) -> &'static str {
    match image_extension(bytes) {
        "png" => "image/png",
        "jpg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "application/octet-stream",
    }
}

/// Encode one `multipart/form-data` body: text fields first, then the base
/// image as a single `file` part, then the closing boundary.
///
/// Built by hand (like `sha256_hex`) so the plugin keeps one HTTP stack instead
/// of pulling in reqwest's `multipart` feature just to wrap a byte slice.
fn multipart_body(
    boundary: &str,
    text_fields: &[(&str, &str)],
    file_name: &str,
    file_bytes: &[u8],
) -> Vec<u8> {
    let mut body = Vec::with_capacity(file_bytes.len() + 512);
    for (name, value) in text_fields {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(value.as_bytes());
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!(
            "Content-Disposition: form-data; name=\"image\"; filename=\"{file_name}\"\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(format!("Content-Type: {}\r\n\r\n", mime_for(file_bytes)).as_bytes());
    body.extend_from_slice(file_bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

#[derive(Debug, Deserialize, Default)]
struct ImagesResponse {
    #[serde(default)]
    data: Vec<ImageDatum>,
    #[serde(default)]
    error: Option<ApiError>,
}

#[derive(Debug, Deserialize, Default)]
struct ImageDatum {
    #[serde(default)]
    b64_json: Option<String>,
    #[serde(default)]
    revised_prompt: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct ApiError {
    #[serde(default)]
    message: Option<String>,
}

/// Sniff the container from its magic bytes; the API returns PNG today but the
/// extension must follow the bytes, not an assumption.
fn image_extension(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "png"
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        "jpg"
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        "gif"
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        "webp"
    } else {
        "bin"
    }
}

/// SHA-256 of the rendered bytes, lowercase hex.
///
/// Hand-rolled for the same reason `image_input::base64_encode` is: the plugin
/// workspace already pays for base64 and HTTP, and content addressing needs
/// exactly one hash — not a new crate. The tests pin the FIPS 180-4 vectors,
/// which also keeps this byte-compatible with the kernel's `sha2`-based
/// `image_cache::digest_hex`.
fn sha256_hex(bytes: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (bytes.len() as u64).wrapping_mul(8);
    let mut padded = Vec::with_capacity(bytes.len() + 72);
    padded.extend_from_slice(bytes);
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());
    for block in padded.chunks_exact(64) {
        let mut schedule = [0u32; 64];
        for (index, word) in schedule.iter_mut().take(16).enumerate() {
            let offset = index * 4;
            *word = u32::from_be_bytes([
                block[offset],
                block[offset + 1],
                block[offset + 2],
                block[offset + 3],
            ]);
        }
        for index in 16..64 {
            let previous = schedule[index - 15];
            let recent = schedule[index - 2];
            let s0 = previous.rotate_right(7) ^ previous.rotate_right(18) ^ (previous >> 3);
            let s1 = recent.rotate_right(17) ^ recent.rotate_right(19) ^ (recent >> 10);
            schedule[index] = schedule[index - 16]
                .wrapping_add(s0)
                .wrapping_add(schedule[index - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for (index, word) in schedule.iter().enumerate() {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choose = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(choose)
                .wrapping_add(K[index])
                .wrapping_add(*word);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
        state[5] = state[5].wrapping_add(f);
        state[6] = state[6].wrapping_add(g);
        state[7] = state[7].wrapping_add(h);
    }
    let mut out = String::with_capacity(64);
    for word in state {
        out.push_str(&format!("{word:08x}"));
    }
    out
}

/// `data/media/<sha256>.png` — the address `view_image` resolves by hash.
fn media_cache_file(media_dir: &Path, digest: &str) -> PathBuf {
    media_dir.join(format!("{digest}.png"))
}

/// Same-directory staging name for the write-then-rename dance.
///
/// The `.tmp` suffix cannot collide with a 64-hex cache name, and the pid keeps
/// two processes from sharing a staging file.
fn media_staging_file(media_dir: &Path, digest: &str) -> PathBuf {
    media_dir.join(format!(".{digest}.png.tmp.{}", std::process::id()))
}

/// Cache one rendered image by content address.
///
/// Same bytes → same digest → same name, so a repeated generation is a pure
/// `is_file` stat. The write goes through a staging file and a rename, so a
/// concurrent `view_image` never sees a half-written PNG. A failure is
/// returned to the caller, which logs it and still returns the image: the
/// cache is a "look at it again" aid, not a precondition for sending the
/// picture.
fn store_media(media_dir: &Path, digest: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    let path = media_cache_file(media_dir, digest);
    if path.is_file() {
        return Ok(path);
    }
    std::fs::create_dir_all(media_dir)
        .map_err(|error| format!("create media dir {}: {error}", media_dir.display()))?;
    let staging = media_staging_file(media_dir, digest);
    std::fs::write(&staging, bytes)
        .map_err(|error| format!("write {}: {error}", staging.display()))?;
    std::fs::rename(&staging, &path)
        .map_err(|error| format!("rename {}: {error}", path.display()))?;
    Ok(path)
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

/// Write the decoded bytes under `dir` with a collision-resistant name, then
/// return the canonical path.
fn write_image(dir: &Path, bytes: &[u8]) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir)
        .map_err(|error| format!("create output dir {}: {error}", dir.display()))?;
    let counter = FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let name = format!(
        "image-{}-{}-{}.{}",
        now_millis(),
        std::process::id(),
        counter,
        image_extension(bytes)
    );
    let path = dir.join(name);
    std::fs::write(&path, bytes).map_err(|error| format!("write {}: {error}", path.display()))?;
    Ok(std::fs::canonicalize(&path).unwrap_or(path))
}

fn response_error(body: &Value) -> Option<String> {
    let parsed: ImagesResponse = serde_json::from_value(body.clone()).ok()?;
    parsed.error.and_then(|error| error.message)
}

fn generate_image(config: &Config, request: &NodeRequest) -> Value {
    let prompt = request
        .prompt
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let Some(prompt) = prompt else {
        return json!({ "ok": false, "error": "prompt is required and must not be empty" });
    };
    let model = resolve_model(request.model.as_deref(), &config.default_model);
    let token = match resolve_token(config) {
        Ok(token) => token,
        Err(error) => return error,
    };

    // No `response_format` field: the upstream behind this gateway rejects it
    // with 400 "Unknown parameter: 'response_format'" and returns b64_json by
    // default (measured 2026-11-12).
    let mut payload = json!({
        "model": model,
        "prompt": prompt,
        "n": 1,
        "size": request.size.as_deref().map(str::trim).filter(|value| !value.is_empty()).unwrap_or("1024x1024"),
    });
    if let Some(quality) = request
        .quality
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        payload["quality"] = json!(quality);
    }

    let client = match Client::builder().timeout(config.timeout).build() {
        Ok(client) => client,
        Err(error) => return json!({ "ok": false, "error": format!("http client: {error}") }),
    };
    let url = endpoint_for(&config.base_url, "generations");
    let request = client.post(&url).bearer_auth(&token).json(&payload);
    execute(config, "generate_image", &model, &url, request)
}

/// The bearer token, or the agent-facing failure payload.
fn resolve_token(config: &Config) -> Result<String, Value> {
    match std::env::var(&config.key_env) {
        Ok(value) if !value.trim().is_empty() => Ok(value.trim().to_string()),
        _ => Err(json!({
            "ok": false,
            "error": format!(
                "image API token is not configured: environment variable {} is unset or empty",
                config.key_env
            ),
        })),
    }
}

/// Shared HTTP leg: send one prepared request, then fold the response into the
/// node result.
///
/// `generate_image` posts JSON and `edit_image` posts multipart, but the status,
/// body, base64 and cache handling are identical, so it lives here once.
fn execute(
    config: &Config,
    node_id: &str,
    model: &str,
    url: &str,
    request: reqwest::blocking::RequestBuilder,
) -> Value {
    let response = match request.send() {
        Ok(response) => response,
        Err(error) => {
            return json!({
                "ok": false,
                "error": format!("POST {url} failed: {error}"),
                "model": model,
            })
        }
    };

    let status = response.status();
    let body_text = match response.text() {
        Ok(text) => text,
        Err(error) => {
            return json!({ "ok": false, "error": format!("read response body: {error}"), "model": model })
        }
    };
    render_outcome(config, node_id, model, status, body_text)
}

/// Turn the upstream JSON body into the agent-facing result and cache the bytes.
fn render_outcome(
    config: &Config,
    node_id: &str,
    model: &str,
    status: reqwest::StatusCode,
    body_text: String,
) -> Value {
    let body: Value = match serde_json::from_str(&body_text) {
        Ok(value) => value,
        Err(error) => {
            return json!({
                "ok": false,
                "error": format!("non-JSON response (HTTP {status}): {error}"),
                "body_preview": body_text.chars().take(ERROR_BODY_PREVIEW).collect::<String>(),
                "model": model,
            })
        }
    };

    if !status.is_success() {
        let detail = response_error(&body).unwrap_or_else(|| {
            body_text
                .chars()
                .take(ERROR_BODY_PREVIEW)
                .collect::<String>()
        });
        return json!({
            "ok": false,
            "error": format!("image API returned HTTP {status}: {detail}"),
            "model": model,
        });
    }

    let parsed: ImagesResponse = match serde_json::from_value(body) {
        Ok(value) => value,
        Err(error) => {
            return json!({ "ok": false, "error": format!("decode response: {error}"), "model": model })
        }
    };
    if let Some(error) = parsed.error.and_then(|error| error.message) {
        return json!({ "ok": false, "error": format!("image API error: {error}"), "model": model });
    }
    let Some(datum) = parsed.data.into_iter().next() else {
        return json!({ "ok": false, "error": "image API returned no data entries", "model": model });
    };
    let Some(encoded) = datum.b64_json else {
        return json!({
            "ok": false,
            "error": "image API returned no b64_json (only URLs are not supported)",
            "model": model,
        });
    };
    let bytes = match base64::engine::general_purpose::STANDARD.decode(encoded.as_bytes()) {
        Ok(bytes) => bytes,
        Err(error) => {
            return json!({ "ok": false, "error": format!("decode b64_json: {error}"), "model": model })
        }
    };
    if bytes.is_empty() {
        return json!({ "ok": false, "error": "image API returned an empty image", "model": model });
    }
    if bytes.len() > MAX_IMAGE_BYTES {
        return json!({
            "ok": false,
            "error": format!("image is {} bytes, above the {} byte limit", bytes.len(), MAX_IMAGE_BYTES),
            "model": model,
        });
    }

    let path = match write_image(&config.output_dir, &bytes) {
        Ok(path) => path,
        Err(error) => return json!({ "ok": false, "error": error, "model": model }),
    };
    let digest = sha256_hex(&bytes);
    let media_cached = match store_media(&config.media_dir, &digest, &bytes) {
        Ok(_) => true,
        Err(error) => {
            // Cache failure is a diagnostic, never a generation failure: the
            // picture already exists and is about to be sent.
            eprintln!("image_gen: media cache write failed for sha256:{digest}: {error}");
            false
        }
    };
    let absolute = path.display().to_string();
    json!({
        "ok": true,
        "node_id": node_id,
        "model": model,
        "path": absolute,
        "cq_image": format!("[CQ:image,file={absolute}]"),
        "sha256": digest,
        "media_path": media_cache_file(&config.media_dir, &digest).display().to_string(),
        "media_cached": media_cached,
        "bytes": bytes.len(),
        "format": image_extension(&bytes),
        "revised_prompt": datum.revised_prompt,
    })
}

/// Absolute form of `path`, canonicalised when it already exists.
fn absolute_path(path: &Path) -> PathBuf {
    if let Ok(resolved) = std::fs::canonicalize(path) {
        return resolved;
    }
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    }
}

/// Resolve the base image for `edit_image`.
///
/// Only two directories are readable this way: the content-addressed media
/// cache the kernel's `view_image` already reads, and this plugin's own output
/// directory. Anything else (a repo file, `/etc/passwd`, `../…`) is refused
/// before a single byte is read, so the node never becomes a general
/// file-exfiltration primitive for the agent.
fn source_image_path(raw: &str, media_dir: &Path, output_dir: &Path) -> Result<PathBuf, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("image is required: pass the local path of an existing image under data/media/ or the image output directory".to_string());
    }
    let candidate = absolute_path(Path::new(trimmed));
    let allowed = [absolute_path(media_dir), absolute_path(output_dir)];
    if !allowed.iter().any(|root| candidate.starts_with(root)) {
        return Err(format!(
            "image path {trimmed} is outside the allowed image directories ({} and {}); copy the base image into data/media/ first",
            allowed[0].display(),
            allowed[1].display()
        ));
    }
    match std::fs::metadata(&candidate) {
        Ok(metadata) if metadata.is_file() => Ok(candidate),
        Ok(_) => Err(format!("image path {trimmed} is not a regular file")),
        Err(error) => Err(format!("image path {trimmed} cannot be read: {error}")),
    }
}

/// Base-image edit: read a cached picture, upload it as a `multipart/form-data`
/// file part plus the prompt, and render the result exactly like
/// `generate_image` does.
fn edit_image(config: &Config, request: &NodeRequest) -> Value {
    let prompt = request
        .prompt
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let Some(prompt) = prompt else {
        return json!({ "ok": false, "error": "prompt is required and must not be empty" });
    };
    let raw_image = request
        .image
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let Some(raw_image) = raw_image else {
        return json!({
            "ok": false,
            "error": "image is required: pass the local path of an existing image under data/media/ or the image output directory",
        });
    };
    let image_path = match source_image_path(raw_image, &config.media_dir, &config.output_dir) {
        Ok(path) => path,
        Err(error) => return json!({ "ok": false, "error": error }),
    };
    let image_bytes = match std::fs::read(&image_path) {
        Ok(bytes) => bytes,
        Err(error) => {
            return json!({
                "ok": false,
                "error": format!("read base image {}: {error}", image_path.display()),
            })
        }
    };
    if image_bytes.is_empty() {
        return json!({ "ok": false, "error": "base image is empty" });
    }
    if image_bytes.len() > MAX_IMAGE_BYTES {
        return json!({
            "ok": false,
            "error": format!(
                "base image is {} bytes, above the {} byte limit",
                image_bytes.len(),
                MAX_IMAGE_BYTES
            ),
        });
    }

    let model = resolve_model(request.model.as_deref(), &config.default_model);
    let token = match resolve_token(config) {
        Ok(token) => token,
        Err(error) => return error,
    };
    let size = request
        .size
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("1024x1024");
    let quality = request
        .quality
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());

    let mut fields: Vec<(&str, &str)> = vec![
        ("model", model.as_str()),
        ("prompt", prompt),
        ("size", size),
        ("n", "1"),
    ];
    if let Some(quality) = quality {
        fields.push(("quality", quality));
    }
    let file_name = image_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| format!("base.{}", image_extension(&image_bytes)));
    let boundary = multipart_boundary();
    let body = multipart_body(&boundary, &fields, &file_name, &image_bytes);

    let client = match Client::builder().timeout(config.timeout).build() {
        Ok(client) => client,
        Err(error) => return json!({ "ok": false, "error": format!("http client: {error}") }),
    };
    let url = endpoint_for(&config.base_url, "edits");
    let request = client
        .post(&url)
        .bearer_auth(&token)
        .header(
            CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(body);
    execute(config, "edit_image", &model, &url, request)
}

fn handle(request: NodeRequest) -> Result<Value, String> {
    match request.node_id.as_str() {
        "generate_image" => {
            let config = Config::from_env();
            Ok(generate_image(&config, &request))
        }
        "edit_image" => {
            let config = Config::from_env();
            Ok(edit_image(&config, &request))
        }
        other => Err(format!("unknown node_id: {other}")),
    }
}

fn docs_value() -> cordis_plugin_sdk::PluginDocs {
    plugin_docs(
        "image_gen",
        "image_gen",
        "0.1.0",
        None,
        vec![node_doc(
            "generate_image",
            "Generate one image from a text prompt through the gpt-image-2.5 image API and save it on this host. Returns the absolute path and a ready-to-send [CQ:image,file=<path>] snippet; pass that snippet to qq_send to show the picture. The same bytes are also cached at data/media/<sha256>.png, and the returned sha256 lets the agent re-read the picture with view_image.",
            json!({
                "type": "object",
                "required": ["node_id", "prompt"],
                "properties": {
                    "node_id": { "type": "string", "const": "generate_image" },
                    "prompt": { "type": "string", "description": "What to draw: subject, style, composition, wording of any text." },
                    "model": { "type": "string", "description": "Optional image model: 'flare' (default) or 'sunburst'; a full model id is also accepted." },
                    "size": { "type": "string", "description": "Optional pixel size, e.g. 1024x1024 (default)." },
                    "quality": { "type": "string", "description": "Optional quality hint passed to the API (e.g. low, medium, high)." }
                }
            }),
            json!({
                "type": "object",
                "properties": {
                    "ok": { "type": "boolean" },
                    "model": { "type": "string" },
                    "path": { "type": "string", "description": "Absolute path of the rendered file on this host" },
                    "cq_image": { "type": "string", "description": "[CQ:image,file=<path>] ready for qq_send's message" },
                    "sha256": { "type": "string", "description": "Lowercase SHA-256 of the rendered bytes; pass it to view_image as sha256:<digest>" },
                    "media_path": { "type": "string", "description": "Cache address of the same bytes, e.g. data/media/<sha256>.png" },
                    "media_cached": { "type": "boolean", "description": "Whether the media cache write succeeded; false is diagnostic only and the generated image is still returned" },
                    "bytes": { "type": "integer" },
                    "format": { "type": "string" },
                    "revised_prompt": { "type": ["string", "null"] },
                    "error": { "type": "string" }
                }
            }),
            &[
                "writes one image file under the configured output directory",
                "writes the same bytes to the content-addressed media cache data/media/<sha256>.png",
                "makes a POST to the image API",
            ],
            &[
                "token environment variable not configured",
                "upstream rejected the request (HTTP error)",
                "request timed out",
                "output directory not writable",
                "media cache not writable (logged as a diagnostic; the image is still returned)",
            ],
        )
        .with_agent_accessible(),
        node_doc(
            "edit_image",
            "Edit one existing local picture with a prompt through the gpt-image-2.5 image API (/images/edits) and save the result on this host. The base image must already live in data/media/ or the image output directory (any other path is refused). Use it to restyle a picture, compose a variation, or overlay text on an image you generated earlier. Returns the same path, cq_image, sha256, media_path and bytes as generate_image.",
            json!({
                "type": "object",
                "required": ["node_id", "image", "prompt"],
                "properties": {
                    "node_id": { "type": "string", "const": "edit_image" },
                    "image": { "type": "string", "description": "Local base image path: a data/media/<sha256>.png cache entry, or a file under the image output directory (data/images). Paths outside those directories are rejected with an error containing 'outside'." },
                    "prompt": { "type": "string", "description": "How to change the picture: new style, added or replaced elements, and the exact wording of any text to overlay." },
                    "model": { "type": "string", "description": "Optional image model: 'flare' (default) or 'sunburst'; a full model id is also accepted." },
                    "size": { "type": "string", "description": "Optional pixel size, e.g. 1024x1024 (default)." },
                    "quality": { "type": "string", "description": "Optional quality hint passed to the API (e.g. low, medium, high)." }
                }
            }),
            json!({
                "type": "object",
                "properties": {
                    "ok": { "type": "boolean" },
                    "model": { "type": "string" },
                    "path": { "type": "string", "description": "Absolute path of the edited file on this host" },
                    "cq_image": { "type": "string", "description": "[CQ:image,file=<path>] ready for qq_send's message" },
                    "sha256": { "type": "string", "description": "Lowercase SHA-256 of the edited bytes; pass it to view_image as sha256:<digest>" },
                    "media_path": { "type": "string", "description": "Cache address of the same bytes, e.g. data/media/<sha256>.png" },
                    "media_cached": { "type": "boolean", "description": "Whether the media cache write succeeded; false is diagnostic only and the edited image is still returned" },
                    "bytes": { "type": "integer" },
                    "format": { "type": "string" },
                    "revised_prompt": { "type": ["string", "null"] },
                    "error": { "type": "string" }
                }
            }),
            &[
                "reads one local image file from data/media/ or the image output directory",
                "writes one image file under the configured output directory",
                "writes the same bytes to the content-addressed media cache data/media/<sha256>.png",
                "makes a POST to the image API",
            ],
            &[
                "image path outside data/media/ and the output directory (error contains 'outside')",
                "base image missing or unreadable",
                "token environment variable not configured",
                "upstream rejected the request (HTTP error)",
                "request timed out",
                "output directory not writable",
            ],
        )
        .with_agent_accessible()],
        Some(
            "生图（image_gen）：有人要你画图、生成图片、做图时用它。\n\
【调用】invoke_plugin {\"plugin_path\":\"image_gen\",\"node_id\":\"generate_image\",\"payload_json\":{\"prompt\":\"<画面描述：主体、风格、构图、要出现的文字>\",\"model\":\"flare\"}}\n\
【模型】model 可选 flare（默认）或 sunburst，是两种不同的出图模型；对方没指定就用默认，指定了另一种就换。\n\
【结果】ok=true 时返回 path（本机绝对路径）、cq_image（[CQ:image,file=<path>]）和 sha256（这张图的内容摘要）。\n\
【发图】把 cq_image 原样放进 qq_send 的 message，可以和说明文字同一条；不要只发路径、不要自己拼 base64、不要拿网图冒充。\n\
【自查】生成时同一份字节已按内容寻址写进 data/media/<sha256>.png。想确认画面有没有崩，调 view_image {\"sha256\":\"sha256:<sha256>\"}（或 {\"path\":\"data/media/<sha256>.png\"}），图会挂到下一轮请求上。view_image 只读这台机器上已缓存的图片，不接受 URL；这条通道不是文件读取权限，也不给你写文件的能力。\n\
【改图】已有图片要改风格、做变体、叠字时用 edit_image：\n\
invoke_plugin {\"plugin_path\":\"image_gen\",\"node_id\":\"edit_image\",\"payload_json\":{\"image\":\"data/media/<sha256>.png\",\"prompt\":\"<怎么改：改成什么风格、加/换什么、要叠的确切文字>\",\"model\":\"flare\"}}\n\
【底图】image 只能是本机已缓存的图：data/media/<sha256>.png，或 data/images/ 下这张插件生成的图。其它路径（包括仓库文件、系统文件、../ 之类）一律被拒，error 会含 outside；这种情况不要换路径硬试，改成让 generate_image 先生成一张，或直接用手里已有的 sha256。\n\
【改图结果】返回字段与 generate_image 完全一致（path、cq_image、sha256、media_path、bytes），同样把 cq_image 交给 qq_send，也同样能用 view_image 回看。\n\
【失败】ok=false 时 error 写明原因（token 未配置 / 上游拒绝 / 超时）。把原因简短告诉对方，同一轮不要反复重试同一条。",
        ),
    )
}

/// Test-only handle on the wire-level pieces of the plugin.
///
/// In production every call arrives through the exported ABI and picks its
/// `Config` up from the environment, but an integration test
/// (`tests/edit_image.rs`) has to point the node at a loopback mock server and
/// inspect the exact multipart bytes that go out. Nothing here adds behaviour:
/// each function forwards to the real private item, so the tests exercise the
/// same code path the agent does.
#[doc(hidden)]
pub mod testing {
    use super::*;

    /// `Config` with explicit directories instead of the environment defaults.
    pub fn config(base_url: &str, output_dir: PathBuf, media_dir: PathBuf, key_env: &str) -> Config {
        Config {
            base_url: base_url.to_string(),
            key_env: key_env.to_string(),
            default_model: DEFAULT_MODEL.to_string(),
            output_dir,
            media_dir,
            timeout: Duration::from_secs(10),
        }
    }

    /// `edit_image` payload; every optional knob stays unset.
    pub fn edit_request(image: &str, prompt: &str) -> NodeRequest {
        NodeRequest {
            node_id: "edit_image".to_string(),
            prompt: Some(prompt.to_string()),
            image: Some(image.to_string()),
            model: None,
            size: None,
            quality: None,
        }
    }

    /// Run the real `edit_image` node body against `config`.
    pub fn edit_image(config: &Config, request: &NodeRequest) -> Value {
        crate::edit_image(config, request)
    }

    /// The hand-built `multipart/form-data` body for `/images/edits`.
    pub fn multipart_body(
        boundary: &str,
        text_fields: &[(&str, &str)],
        file_name: &str,
        file_bytes: &[u8],
    ) -> Vec<u8> {
        crate::multipart_body(boundary, text_fields, file_name, file_bytes)
    }

    /// `base_url` + `/images/<sub>`.
    pub fn endpoint_for(base_url: &str, sub: &str) -> String {
        crate::endpoint_for(base_url, sub)
    }
}

fn abi_fingerprint_value() -> AbiFingerprint {
    AbiFingerprint::current_build("crate_image_gen_v1", "api_v2")
}

fn api_handle(request: PluginRequest) -> PluginResponse {
    match serde_json::from_str::<NodeRequest>(&request.payload)
        .map_err(|error| format!("image_gen plugin: {error}"))
        .and_then(handle)
    {
        Ok(value) => json_response(&value),
        Err(error) => json_response(&json!({
            "ok": false,
            "node_id": "error",
            "error": error,
        })),
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
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    /// 1x1 transparent PNG.
    const TINY_PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";
    /// SHA-256 of the decoded `TINY_PNG_B64` bytes (cross-checked with `sha256sum`).
    const TINY_PNG_SHA256: &str =
        "c414cd0e204de974f73753c7e28d7638e7b3691bb8b1a2bab6b25bb7fed7ce77";

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "image-gen-{tag}-{}-{}",
            std::process::id(),
            now_millis()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    /// Serve exactly one HTTP request with `body`, then close.
    ///
    /// The returned handle yields the request head (request line + headers) so a
    /// test can assert which path and content type went out.
    fn mock_server(status: &str, body: String) -> (String, thread::JoinHandle<String>) {
        let status = status.to_string();
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock server");
        let addr = listener.local_addr().expect("mock addr");
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                match stream.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => {
                        buffer.extend_from_slice(&chunk[..read]);
                        if let Some(head_end) = buffer
                            .windows(4)
                            .position(|window| window == b"\r\n\r\n")
                        {
                            let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
                            let content_length = head
                                .lines()
                                .find_map(|line| {
                                    let (name, value) = line.split_once(':')?;
                                    if name.trim().eq_ignore_ascii_case("content-length") {
                                        value.trim().parse::<usize>().ok()
                                    } else {
                                        None
                                    }
                                })
                                .unwrap_or(0);
                            if buffer.len() >= head_end + 4 + content_length {
                                break;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            let head_end = buffer
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .unwrap_or(buffer.len());
            let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
            head
        });
        (format!("http://{addr}"), handle)
    }

    fn test_config(base_url: String, output_dir: PathBuf, key_env: &str) -> Config {
        let media_dir = output_dir.join("media");
        Config {
            base_url,
            key_env: key_env.to_string(),
            default_model: DEFAULT_MODEL.to_string(),
            output_dir,
            media_dir,
            timeout: Duration::from_secs(10),
        }
    }

    fn request(prompt: &str, model: Option<&str>) -> NodeRequest {
        NodeRequest {
            node_id: "generate_image".to_string(),
            prompt: Some(prompt.to_string()),
            image: None,
            model: model.map(str::to_string),
            size: None,
            quality: None,
        }
    }

    fn edit_request(image: &str, prompt: &str) -> NodeRequest {
        NodeRequest {
            node_id: "edit_image".to_string(),
            prompt: Some(prompt.to_string()),
            image: Some(image.to_string()),
            model: None,
            size: None,
            quality: None,
        }
    }

    #[test]
    fn aliases_resolve_to_model_ids() {
        assert_eq!(resolve_model(None, DEFAULT_MODEL), MODEL_FLARE);
        assert_eq!(resolve_model(Some("flare"), "other"), MODEL_FLARE);
        assert_eq!(resolve_model(Some("SunBurst"), "other"), MODEL_SUNBURST);
        assert_eq!(
            resolve_model(Some("gpt-image-2.5-flare"), "other"),
            "gpt-image-2.5-flare"
        );
        assert_eq!(resolve_model(Some("  "), "other"), "other");
    }

    #[test]
    fn endpoint_for_joins_without_doubling_slashes() {
        assert_eq!(
            endpoint_for("http://127.0.0.1:8792/v1", "generations"),
            "http://127.0.0.1:8792/v1/images/generations"
        );
        assert_eq!(
            endpoint_for("http://127.0.0.1:8792/v1/", "generations"),
            "http://127.0.0.1:8792/v1/images/generations"
        );
        assert_eq!(
            endpoint_for("http://127.0.0.1:8792/v1/", "edits"),
            "http://127.0.0.1:8792/v1/images/edits"
        );
    }

    #[test]
    fn multipart_body_carries_the_image_as_a_file_part() {
        let boundary = "image-gen-boundary-123";
        let image = decode_png();
        let body = multipart_body(
            boundary,
            &[("model", MODEL_FLARE), ("prompt", "overlay BIG SALE")],
            "base.png",
            &image,
        );
        let text = String::from_utf8_lossy(&body);

        assert!(
            text.contains("name=\"image\"; filename=\"base.png\""),
            "the file part must be named image: {text}"
        );
        assert!(
            text.contains("Content-Disposition: form-data; name=\"prompt\""),
            "text fields must be present: {text}"
        );
        assert!(text.contains("Content-Type: image/png"), "body: {text}");
        assert!(
            body.ends_with(format!("\r\n--{boundary}--\r\n").as_bytes()),
            "the body must close on the boundary"
        );
        assert!(
            body.windows(image.len())
                .any(|window| window == image.as_slice()),
            "the raw base image bytes must be in the body"
        );
    }

    #[test]
    fn extension_follows_magic_bytes() {
        assert_eq!(image_extension(b"\x89PNG\r\n\x1a\nxxxx"), "png");
        assert_eq!(image_extension(b"\xff\xd8\xff\xdb"), "jpg");
        assert_eq!(image_extension(b"GIF89a"), "gif");
        assert_eq!(image_extension(b"RIFF\x00\x00\x00\x00WEBPVP8 "), "webp");
        assert_eq!(image_extension(b"not an image"), "bin");
    }

    #[test]
    fn sha256_matches_fips_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // 56-byte message: padding spans a second 512-bit block.
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // Exactly one block of input: the 0x80 byte forces a second block.
        assert_eq!(
            sha256_hex(&[0u8; 64]),
            "f5a5fd42d16a20302798ef6ed309979b43003d2320d9f0e8ea9831a92759fb4b"
        );
        assert_eq!(sha256_hex(&decode_png()), TINY_PNG_SHA256);
    }

    #[test]
    fn media_cache_is_content_addressed_and_idempotent() {
        let dir = temp_dir("media-ok");
        let bytes = decode_png();
        let path = store_media(&dir, TINY_PNG_SHA256, &bytes).expect("store");

        assert_eq!(path, dir.join(format!("{TINY_PNG_SHA256}.png")));
        assert_eq!(std::fs::read(&path).expect("read back"), bytes);
        // Same content again: the `is_file` short-circuit keeps one file.
        let again = store_media(&dir, TINY_PNG_SHA256, &bytes).expect("store again");
        assert_eq!(again, path);
        assert_eq!(std::fs::read(&again).expect("read back"), bytes);
        assert_eq!(std::fs::read_dir(&dir).expect("dir").count(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn media_cache_reports_an_uncreatable_directory_without_panicking() {
        let dir = temp_dir("media-create-fail");
        let blocker = dir.join("blocked");
        std::fs::write(&blocker, b"not a directory").expect("squatter");

        let error = store_media(&blocker, TINY_PNG_SHA256, b"bytes").expect_err("must fail");
        assert!(error.starts_with("create media dir "), "error={error}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn media_cache_reports_write_and_rename_failures_without_panicking() {
        let dir = temp_dir("media-write-fail");
        // 1) The staging name is occupied by a directory → the write fails.
        let staging = media_staging_file(&dir, TINY_PNG_SHA256);
        std::fs::create_dir_all(&staging).expect("staging squatter");
        let error = store_media(&dir, TINY_PNG_SHA256, b"bytes").expect_err("write must fail");
        assert!(error.starts_with("write "), "error={error}");

        // 2) The final name is occupied by a directory → the rename fails.
        std::fs::remove_dir_all(&staging).expect("clear staging");
        std::fs::create_dir_all(media_cache_file(&dir, TINY_PNG_SHA256)).expect("final squatter");
        let error = store_media(&dir, TINY_PNG_SHA256, b"bytes").expect_err("rename must fail");
        assert!(error.starts_with("rename "), "error={error}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn generates_and_writes_the_image() {
        let body = format!(
            r#"{{"created":1,"data":[{{"b64_json":"{TINY_PNG_B64}","revised_prompt":"a red apple"}}]}}"#
        );
        let (base_url, handle) = mock_server("200 OK", body);
        let dir = temp_dir("ok");
        let config = test_config(base_url, dir.clone(), "IMAGE_GEN_TEST_TOKEN_OK_SET");
        std::env::set_var("IMAGE_GEN_TEST_TOKEN_OK_SET", "test-token");

        let output = generate_image(&config, &request("a red apple", Some("sunburst")));

        assert_eq!(output["ok"], json!(true), "output: {output}");
        assert_eq!(output["model"], json!(MODEL_SUNBURST));
        assert_eq!(output["format"], json!("png"));
        assert_eq!(output["bytes"], json!(decode_png().len()));
        let path = PathBuf::from(output["path"].as_str().expect("path"));
        assert!(path.is_absolute(), "path should be absolute: {path:?}");
        assert_eq!(std::fs::read(&path).expect("read image"), decode_png());
        assert_eq!(
            output["cq_image"],
            json!(format!("[CQ:image,file={}]", path.display()))
        );
        assert_eq!(output["revised_prompt"], json!("a red apple"));
        assert_eq!(output["sha256"], json!(TINY_PNG_SHA256));
        assert_eq!(output["media_cached"], json!(true));
        let media = dir.join("media").join(format!("{TINY_PNG_SHA256}.png"));
        assert_eq!(
            output["media_path"],
            json!(media.display().to_string()),
            "media_path must be the cache address"
        );
        assert_eq!(
            std::fs::read(&media).expect("read media cache"),
            decode_png()
        );

        std::env::remove_var("IMAGE_GEN_TEST_TOKEN_OK_SET");
        let _ = std::fs::remove_dir_all(&dir);
        handle.join().expect("mock server thread");
    }

    /// 缓存写失败只是诊断：生图仍然成功，图片仍然可用。
    #[test]
    fn media_cache_failure_does_not_fail_generation() {
        let body = format!(r#"{{"created":1,"data":[{{"b64_json":"{TINY_PNG_B64}"}}]}}"#);
        let (base_url, handle) = mock_server("200 OK", body);
        let dir = temp_dir("media-gen-fail");
        let media_dir = dir.join("media");
        std::fs::write(&media_dir, b"not a directory").expect("media squatter");
        let config = Config {
            media_dir: media_dir.clone(),
            ..test_config(base_url, dir.clone(), "IMAGE_GEN_TEST_TOKEN_MEDIA_FAIL_SET")
        };
        std::env::set_var("IMAGE_GEN_TEST_TOKEN_MEDIA_FAIL_SET", "test-token");

        let output = generate_image(&config, &request("a red apple", None));

        assert_eq!(output["ok"], json!(true), "output: {output}");
        assert_eq!(output["sha256"], json!(TINY_PNG_SHA256));
        assert_eq!(output["media_cached"], json!(false));
        assert_eq!(
            output["media_path"],
            json!(media_dir
                .join(format!("{TINY_PNG_SHA256}.png"))
                .display()
                .to_string())
        );
        let path = PathBuf::from(output["path"].as_str().expect("path"));
        assert_eq!(std::fs::read(&path).expect("read image"), decode_png());

        std::env::remove_var("IMAGE_GEN_TEST_TOKEN_MEDIA_FAIL_SET");
        let _ = std::fs::remove_dir_all(&dir);
        handle.join().expect("mock server thread");
    }

    /// 底图只允许来自 data/media/ 或图片输出目录；仓库文件之类的路径直接被拒。
    #[test]
    fn edit_rejects_a_base_image_outside_the_allowed_directories() {
        let dir = temp_dir("edit-outside");
        let config = test_config(
            "http://127.0.0.1:1/v1".to_string(),
            dir.clone(),
            "IMAGE_GEN_TEST_TOKEN_EDIT_OUTSIDE",
        );
        std::env::set_var("IMAGE_GEN_TEST_TOKEN_EDIT_OUTSIDE", "test-token");

        let output = edit_image(&config, &edit_request("Cargo.toml", "add a red frame"));

        assert_eq!(output["ok"], json!(false), "output: {output}");
        let error = output["error"].as_str().expect("error");
        assert!(error.contains("outside"), "error: {error}");

        std::env::remove_var("IMAGE_GEN_TEST_TOKEN_EDIT_OUTSIDE");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 底图走 multipart 上传到 /images/edits；返回与 generate_image 同形，
    /// 并按同样规则写进 data/media/<sha256>.png。
    #[test]
    fn edits_a_cached_base_image_and_caches_the_result() {
        let body = format!(
            r#"{{"created":1,"data":[{{"b64_json":"{TINY_PNG_B64}","revised_prompt":"BIG SALE on the poster"}}]}}"#
        );
        let (base_url, handle) = mock_server("200 OK", body);
        let dir = temp_dir("edit-ok");
        let config = test_config(base_url, dir.clone(), "IMAGE_GEN_TEST_TOKEN_EDIT_OK_SET");
        std::env::set_var("IMAGE_GEN_TEST_TOKEN_EDIT_OK_SET", "test-token");

        let media_dir = dir.join("media");
        std::fs::create_dir_all(&media_dir).expect("media dir");
        let base_image = media_dir.join("base.png");
        std::fs::write(&base_image, decode_png()).expect("write base image");

        let output = edit_image(
            &config,
            &edit_request(
                &base_image.display().to_string(),
                "add the words BIG SALE",
            ),
        );

        assert_eq!(output["ok"], json!(true), "output: {output}");
        assert_eq!(output["node_id"], json!("edit_image"));
        assert_eq!(output["sha256"], json!(TINY_PNG_SHA256));
        assert_eq!(output["format"], json!("png"));
        assert_eq!(output["bytes"], json!(decode_png().len()));
        let path = PathBuf::from(output["path"].as_str().expect("path"));
        assert!(path.is_absolute(), "path should be absolute: {path:?}");
        assert_eq!(
            std::fs::read(&path).expect("read edited image"),
            decode_png()
        );
        assert_eq!(
            output["cq_image"],
            json!(format!("[CQ:image,file={}]", path.display()))
        );
        assert_eq!(output["revised_prompt"], json!("BIG SALE on the poster"));
        assert_eq!(output["media_cached"], json!(true));
        let media = media_dir.join(format!("{TINY_PNG_SHA256}.png"));
        assert_eq!(
            output["media_path"],
            json!(media.display().to_string()),
            "media_path must be the cache address"
        );
        assert_eq!(
            std::fs::read(&media).expect("read media cache"),
            decode_png()
        );

        std::env::remove_var("IMAGE_GEN_TEST_TOKEN_EDIT_OK_SET");
        let _ = std::fs::remove_dir_all(&dir);
        let head = handle.join().expect("mock server thread");
        assert!(
            head.starts_with("POST /images/edits "),
            "request line: {head}"
        );
        assert!(
            head.to_ascii_lowercase()
                .contains("content-type: multipart/form-data; boundary="),
            "request head: {head}"
        );
    }

    fn decode_png() -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(TINY_PNG_B64)
            .expect("decode fixture")
    }

    #[test]
    fn upstream_http_error_is_reported() {
        let body = r#"{"error":{"message":"prompt rejected by safety system"}}"#.to_string();
        let (base_url, handle) = mock_server("400 Bad Request", body);
        let dir = temp_dir("err");
        let config = test_config(base_url, dir.clone(), "IMAGE_GEN_TEST_TOKEN_ERR_SET");
        std::env::set_var("IMAGE_GEN_TEST_TOKEN_ERR_SET", "test-token");

        let output = generate_image(&config, &request("something", None));

        assert_eq!(output["ok"], json!(false));
        let error = output["error"].as_str().expect("error");
        assert!(error.contains("400"), "error: {error}");
        assert!(error.contains("safety system"), "error: {error}");

        std::env::remove_var("IMAGE_GEN_TEST_TOKEN_ERR_SET");
        let _ = std::fs::remove_dir_all(&dir);
        handle.join().expect("mock server thread");
    }

    #[test]
    fn missing_token_is_reported_before_any_request() {
        let config = test_config(
            "http://127.0.0.1:1/v1".to_string(),
            temp_dir("token"),
            "IMAGE_GEN_TEST_TOKEN_DEFINITELY_UNSET",
        );
        let output = generate_image(&config, &request("something", None));
        assert_eq!(output["ok"], json!(false));
        assert!(output["error"]
            .as_str()
            .expect("error")
            .contains("IMAGE_GEN_TEST_TOKEN_DEFINITELY_UNSET"));
    }

    #[test]
    fn empty_prompt_is_rejected() {
        let config = test_config(
            "http://127.0.0.1:1/v1".to_string(),
            temp_dir("prompt"),
            "IMAGE_GEN_TEST_TOKEN_EMPTY_PROMPT",
        );
        let output = generate_image(&config, &request("   ", None));
        assert_eq!(output["ok"], json!(false));
        assert!(output["error"].as_str().expect("error").contains("prompt"));
    }

    /// Live check against the real gateway (needs `DEEPSEEK_GATEWAY_TOKEN` and
    /// the 8792 tunnel). Ignored by default so `cargo test` never spends quota;
    /// run with `--ignored --nocapture` on the deployment host to see the
    /// returned path.
    #[test]
    #[ignore]
    fn live_gateway_round_trip() {
        let config = Config::from_env();
        let output = generate_image(&config, &request("a red apple on a white plate", Some("flare")));
        println!("{}", serde_json::to_string_pretty(&output).expect("json"));
        assert_eq!(output["ok"], json!(true), "output: {output}");
        let path = PathBuf::from(output["path"].as_str().expect("path"));
        let bytes = std::fs::read(&path).expect("read generated image");
        assert_eq!(image_extension(&bytes), "png");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn unknown_node_is_an_error() {
        let error = handle(NodeRequest {
            node_id: "nope".to_string(),
            prompt: None,
            image: None,
            model: None,
            size: None,
            quality: None,
        })
        .expect_err("unknown node must fail");
        assert!(error.contains("unknown node_id"));
    }

    /// 检入的 `docs/agent/interfaces.json` 必须与 dylib 导出的 docs 逐字段一致：
    /// 它由构建期回写，手工改错会让 agent 看到过期 schema。
    #[test]
    fn checked_in_interfaces_json_matches_the_exported_docs() {
        let expected: Value =
            serde_json::from_str(include_str!("../docs/agent/interfaces.json")).expect("json");
        let actual = serde_json::to_value(docs_value()).expect("serialize docs");
        assert_eq!(actual, expected);
    }
}
