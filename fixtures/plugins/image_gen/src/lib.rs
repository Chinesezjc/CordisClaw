//! Image generation plugin — text-to-image through an OpenAI-compatible
//! `/v1/images/generations` endpoint.
//!
//! Node:
//! - `generate_image` — render one prompt to a file on disk and return the
//!   absolute path plus a ready-to-send `[CQ:image,file=…]` snippet.
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
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DEFAULT_BASE_URL: &str = "http://127.0.0.1:8792/v1";
const DEFAULT_KEY_ENV: &str = "DEEPSEEK_GATEWAY_TOKEN";
const DEFAULT_MODEL: &str = "gpt-image-2.5-flare";
const DEFAULT_OUTPUT_DIR: &str = "data/images";
const DEFAULT_TIMEOUT_SECS: u64 = 300;
/// Refuse absurd responses instead of writing multi-hundred-MB files.
const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024;
/// How much of an upstream error body is echoed back to the agent.
const ERROR_BODY_PREVIEW: usize = 400;

const MODEL_FLARE: &str = "gpt-image-2.5-flare";
const MODEL_SUNBURST: &str = "gpt-image-2.5-sunburst";

static FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Deserialize)]
struct NodeRequest {
    node_id: String,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    size: Option<String>,
    #[serde(default)]
    quality: Option<String>,
}

#[derive(Debug, Clone)]
struct Config {
    base_url: String,
    key_env: String,
    default_model: String,
    output_dir: PathBuf,
    timeout: Duration,
}

impl Config {
    fn from_env() -> Self {
        Config {
            base_url: env_or("CORDIS_IMAGE_BASE_URL", DEFAULT_BASE_URL),
            key_env: env_or("CORDIS_IMAGE_API_KEY_ENV", DEFAULT_KEY_ENV),
            default_model: env_or("CORDIS_IMAGE_MODEL", DEFAULT_MODEL),
            output_dir: PathBuf::from(env_or("CORDIS_IMAGE_OUTPUT_DIR", DEFAULT_OUTPUT_DIR)),
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

/// `base` + `/images/generations`, tolerating a trailing slash on either side.
fn endpoint(base_url: &str) -> String {
    format!("{}/images/generations", base_url.trim_end_matches('/'))
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
    let token = match std::env::var(&config.key_env) {
        Ok(value) if !value.trim().is_empty() => value.trim().to_string(),
        _ => {
            return json!({
                "ok": false,
                "error": format!(
                    "image API token is not configured: environment variable {} is unset or empty",
                    config.key_env
                ),
            })
        }
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
    let url = endpoint(&config.base_url);
    let response = match client
        .post(&url)
        .bearer_auth(&token)
        .json(&payload)
        .send()
    {
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
    let absolute = path.display().to_string();
    json!({
        "ok": true,
        "node_id": "generate_image",
        "model": model,
        "path": absolute,
        "cq_image": format!("[CQ:image,file={absolute}]"),
        "bytes": bytes.len(),
        "format": image_extension(&bytes),
        "revised_prompt": datum.revised_prompt,
    })
}

fn handle(request: NodeRequest) -> Result<Value, String> {
    match request.node_id.as_str() {
        "generate_image" => {
            let config = Config::from_env();
            Ok(generate_image(&config, &request))
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
            "Generate one image from a text prompt through the gpt-image-2.5 image API and save it on this host. Returns the absolute path and a ready-to-send [CQ:image,file=<path>] snippet; pass that snippet to qq_send to show the picture.",
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
                    "bytes": { "type": "integer" },
                    "format": { "type": "string" },
                    "revised_prompt": { "type": ["string", "null"] },
                    "error": { "type": "string" }
                }
            }),
            &["writes one image file under the configured output directory", "makes a POST to the image API"],
            &[
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
【结果】ok=true 时返回 path（本机绝对路径）和 cq_image（[CQ:image,file=<path>]）。\n\
【发图】把 cq_image 原样放进 qq_send 的 message，可以和说明文字同一条；不要只发路径、不要自己拼 base64、不要拿网图冒充。\n\
【失败】ok=false 时 error 写明原因（token 未配置 / 上游拒绝 / 超时）。把原因简短告诉对方，同一轮不要反复重试同一条。",
        ),
    )
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
    fn mock_server(status: &str, body: String) -> (String, thread::JoinHandle<()>) {
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
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes());
        });
        (format!("http://{addr}"), handle)
    }

    fn test_config(base_url: String, output_dir: PathBuf, key_env: &str) -> Config {
        Config {
            base_url,
            key_env: key_env.to_string(),
            default_model: DEFAULT_MODEL.to_string(),
            output_dir,
            timeout: Duration::from_secs(10),
        }
    }

    fn request(prompt: &str, model: Option<&str>) -> NodeRequest {
        NodeRequest {
            node_id: "generate_image".to_string(),
            prompt: Some(prompt.to_string()),
            model: model.map(str::to_string),
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
    fn endpoint_joins_without_doubling_slashes() {
        assert_eq!(
            endpoint("http://127.0.0.1:8792/v1"),
            "http://127.0.0.1:8792/v1/images/generations"
        );
        assert_eq!(
            endpoint("http://127.0.0.1:8792/v1/"),
            "http://127.0.0.1:8792/v1/images/generations"
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

        std::env::remove_var("IMAGE_GEN_TEST_TOKEN_OK_SET");
        let _ = std::fs::remove_dir_all(&dir);
        handle.join().expect("mock server thread");
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
            model: None,
            size: None,
            quality: None,
        })
        .expect_err("unknown node must fail");
        assert!(error.contains("unknown node_id"));
    }
}
