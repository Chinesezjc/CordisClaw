//! Vision plugin — image source helpers.
//!
//! The agent now consumes images natively: the kernel downloads a batch's
//! image URLs and inlines them as `image_url` content parts (see
//! `cordis_runtime::image_input`). There is therefore no reason to ship the old
//! "send the image to a vision model / OCR it, then read the text" nodes, and
//! this plugin no longer declares any node.
//!
//! What remains is the image-source layer those nodes were built on:
//!
//! - a short-lived, SSRF-guarded HTTP download with a URL→bytes cache;
//! - a local-file reader restricted to the system temp directory and to the
//!   kernel's content-addressed image cache (`data/media/<sha256>`), which is
//!   where `[image omitted …]` placeholders point after the request byte budget
//!   strips a picture.
//!
//! Both are `pub` library API (covered by the tests at the bottom) rather than
//! agent-callable nodes: no node dispatches to them any more, so `pub` is what
//! keeps them alive instead of dead code.
//!
//! Safety: only http/https URLs are allowed; localhost and private IPs blocked.

use cordis_plugin_sdk::{
    export_plugin_api, json_response, plugin_docs, AbiFingerprint, PluginRequest, PluginResponse,
};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::io::Read;
use std::path::{Component, Path};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Downloading a QQ CDN image is fast; the download timeout only has to cover
/// the HTTP fetch now that inference no longer happens in this plugin.
const DOWNLOAD_TIMEOUT_SECS: u64 = 10;
const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024; // 20 MB

// ---------------------------------------------------------------------------
// Short-lived download cache
// ---------------------------------------------------------------------------
//
// QQ/Tencent image links are signed and expire within minutes (and a given
// link may be effectively one-shot). A follow-up question about the same
// picture ("这张呢" / "这是谁") would otherwise re-download the same URL,
// fail with HTTP 400, and lose the image the bot already saw. Cache the bytes
// for a few minutes so replies stay consistent within one conversation.
const URL_CACHE_TTL: Duration = Duration::from_secs(600);
const URL_CACHE_CAPACITY: usize = 8;

type UrlCache = Mutex<HashMap<String, (Vec<u8>, Instant)>>;

static URL_CACHE: LazyLock<UrlCache> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn cache_lookup(
    cache: &mut HashMap<String, (Vec<u8>, Instant)>,
    url: &str,
    now: Instant,
) -> Option<Vec<u8>> {
    match cache.get(url) {
        Some((bytes, at)) if now.saturating_duration_since(*at) <= URL_CACHE_TTL => {
            Some(bytes.clone())
        }
        Some(_) => {
            cache.remove(url);
            None
        }
        None => None,
    }
}

fn cache_store(
    cache: &mut HashMap<String, (Vec<u8>, Instant)>,
    url: &str,
    bytes: &[u8],
    now: Instant,
) {
    if cache.len() >= URL_CACHE_CAPACITY && !cache.contains_key(url) {
        if let Some(oldest) = cache
            .iter()
            .min_by_key(|(_, (_, at))| *at)
            .map(|(key, _)| key.clone())
        {
            cache.remove(&oldest);
        }
    }
    cache.insert(url.to_string(), (bytes.to_vec(), now));
}

// ---------------------------------------------------------------------------
// SSRF guard (P0-22)
// ---------------------------------------------------------------------------
//
// P2-2: moved to the shared `cordis-net` crate — this plugin now imports
// the same guard as `web`. Adding a new outbound-HTTP plugin? Depend on
// `cordis-net`.
use cordis_net::check_url_safety;

// ---------------------------------------------------------------------------
// Image download
// ---------------------------------------------------------------------------

/// Download an image over HTTP, reusing recently fetched bytes for the same
/// URL so an expiring signed link is not fetched twice.
pub fn download_image(url_str: &str) -> Result<Vec<u8>, String> {
    // A repeat call for the same (expiring) link must reuse the bytes the bot
    // already saw instead of a second download that may 400.
    {
        let mut cache = URL_CACHE.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(bytes) = cache_lookup(&mut cache, url_str, Instant::now()) {
            return Ok(bytes);
        }
    }
    // P0-22: full SSRF check (see the module-level guard for coverage).
    // Note: `ureq::AgentBuilder` here does not follow redirects by default
    // for `.call()`, so a single check on `url_str` covers the wire path.
    check_url_safety(url_str)?;

    let resp = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(DOWNLOAD_TIMEOUT_SECS))
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

    {
        let mut cache = URL_CACHE.lock().unwrap_or_else(|p| p.into_inner());
        cache_store(&mut cache, url_str, &bytes, Instant::now());
    }
    Ok(bytes)
}

// ---------------------------------------------------------------------------
// Local file source (P2: consume feishu_fetch_resource temp files)
// ---------------------------------------------------------------------------

/// Read a local image file.
///
/// Two allowed locations, both canonicalized before comparison:
///
/// - the system **temp directory**, where a trusted producer (e.g. the feishu
///   plugin's `feishu_fetch_resource`) drops files;
/// - the kernel's **content-addressed image cache** (`…/data/media/<sha256>`).
///   When the request byte budget strips an inline picture it leaves
///   `[image omitted …; path data/media/<sha256>]` in its place, so this is the
///   address the agent hands back to reload the original bytes.
///
/// `..` is rejected before any filesystem work (so the error names the real
/// cause) and symlinks are resolved by `canonicalize`, so a link that points
/// outside both roots is refused like any other outside path.
///
/// On macOS both `/tmp` and `/var/folders/...` are symlinks, which is why the
/// requested path and the temp-dir prefix are each canonicalized.
pub fn read_local_image(path: &str) -> Result<Vec<u8>, String> {
    if Path::new(path)
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(format!("path traversal is not allowed: {path}"));
    }

    let real = std::fs::canonicalize(path).map_err(|e| format!("resolve path {path}: {e}"))?;

    // The media cache is checked first: its files usually live outside the temp
    // directory, and a `data/media` file inside the temp dir must still be
    // recognized as a cache entry rather than as a trusted temp drop.
    if !is_media_cache_file(&real) {
        let temp_root = std::fs::canonicalize(std::env::temp_dir())
            .map_err(|e| format!("resolve temp dir: {e}"))?;
        if !real.starts_with(&temp_root) {
            return Err(format!(
                "path outside temp dir and data/media cache: {} (must be under {} or a data/media directory)",
                real.display(),
                temp_root.display()
            ));
        }
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

/// Is this canonical path a file sitting directly in a `data/media` directory?
///
/// Shape check rather than a configured root: the workspace root is not known to
/// a plugin, and the cache is a convention (`data/media/<sha256>`) shared with
/// the kernel's placeholder text. A subdirectory, a sibling `media/`, or a
/// `data/` file all fail.
fn is_media_cache_file(path: &Path) -> bool {
    let Some(dir) = path.parent() else {
        return false;
    };
    let Some(dir_name) = dir.file_name() else {
        return false;
    };
    let Some(parent) = dir.parent() else {
        return false;
    };
    let Some(parent_name) = parent.file_name() else {
        return false;
    };
    dir_name == "media" && parent_name == "data"
}

/// Load image bytes from exactly one of `url` or `path`. Supplying both is an
/// error; supplying neither is an error. `path` reads a local temp-dir file,
/// `url` goes through the SSRF-guarded HTTP download.
pub fn load_source(url: Option<&str>, path: Option<&str>) -> Result<Vec<u8>, String> {
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
// Entry point
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct NodeRequest {
    node_id: String,
}

fn api_handle(req: PluginRequest) -> PluginResponse {
    // The plugin declares no nodes any more, so every payload gets the same
    // structured in-band failure the old dispatcher returned for an unknown
    // node_id. A malformed payload keeps its parse-error message.
    let error = match serde_json::from_str::<NodeRequest>(&req.payload) {
        Ok(request) => format!("unknown node_id: {}", request.node_id),
        Err(e) => format!("vision plugin: {e}"),
    };
    json_response(&json!({ "ok": false, "node_id": "error", "error": error }))
}

// ---------------------------------------------------------------------------
// Plugin API exports
// ---------------------------------------------------------------------------

fn docs_value() -> cordis_plugin_sdk::PluginDocs {
    // No agent-callable nodes: native image input covers "look at the picture",
    // and the download/local-read helpers above are internal plumbing.
    plugin_docs("vision", "vision", "0.1.0", None, Vec::new(), None)
}

fn abi_fingerprint_value() -> AbiFingerprint {
    AbiFingerprint::current_build("crate_vision_v1", "api_v2")
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
        // The guard resolves symlinks before comparing, so the path has to
        // exist *and* sit outside the temp dir. Fixture tests also run inside a
        // temporary workspace copy, which makes the crate dir itself a temp
        // path — so in that case fall back to an existing file outside the temp
        // root. Writing a new file next to the temp root is not an option: on a
        // CI runner that is `/`, which is not writable by the test user.
        let temp_root = std::fs::canonicalize(std::env::temp_dir()).expect("resolve temp dir");
        let manifest = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let outside = if manifest.starts_with(&temp_root) {
            [std::env::var_os("HOME"), Some("/etc/hostname".into())]
                .into_iter()
                .flatten()
                .map(std::path::PathBuf::from)
                .find(|path| path.exists() && !path.starts_with(&temp_root))
                .unwrap_or_else(|| manifest.join("Cargo.toml"))
        } else {
            manifest.join("Cargo.toml")
        };
        let err = read_local_image(outside.to_str().expect("utf-8 path")).unwrap_err();
        assert!(
            err.contains("path outside temp dir"),
            "expected temp-dir rejection, got: {err}"
        );
    }

    #[test]
    fn read_local_image_rejects_parent_dir_components() {
        // Rejected before any filesystem work, so a non-existent path still
        // reports the real cause instead of "resolve path".
        let err = read_local_image("data/media/../../etc/passwd").unwrap_err();
        assert!(
            err.contains("path traversal is not allowed"),
            "expected traversal rejection, got: {err}"
        );
    }

    /// 占位符里的 `data/media/<sha256>` 是缓存地址：内核把被预算剥掉的图片
    /// 原始字节存在那里，这里必须能读回来。
    #[test]
    fn read_local_image_reads_data_media_cache_files() {
        let dir = unique_temp_path("media_cache");
        let media = dir.join("data").join("media");
        std::fs::create_dir_all(&media).expect("create data/media");
        let path = media.join("ab12cd34");
        let payload: &[u8] = b"\x89PNG\r\n\x1a\nrecovered image bytes";
        std::fs::write(&path, payload).expect("write cached image");

        let result = read_local_image(path.to_str().expect("utf-8 path"));
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(result.expect("cache file reads back"), payload);
    }

    /// 只有**直接**落在 `data/media/` 里的文件算缓存地址：同名目录、子目录、
    /// 兄弟 `media/` 与 `data/` 下的文件都不算。
    #[test]
    fn is_media_cache_file_requires_a_file_directly_in_data_media() {
        assert!(is_media_cache_file(Path::new(
            "/workspace/data/media/ab12cd34"
        )));
        assert!(!is_media_cache_file(Path::new("/workspace/data/media")));
        assert!(!is_media_cache_file(Path::new(
            "/workspace/data/media/nested/ab12cd34"
        )));
        assert!(!is_media_cache_file(Path::new("/workspace/media/ab12cd34")));
        assert!(!is_media_cache_file(Path::new("/workspace/data/ab12cd34")));
        assert!(!is_media_cache_file(Path::new("/ab12cd34")));
        assert!(!is_media_cache_file(Path::new("")));
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

    #[test]
    fn url_cache_hits_within_ttl_and_expires_after() {
        let mut cache: HashMap<String, (Vec<u8>, Instant)> = HashMap::new();
        let t0 = Instant::now();
        cache_store(&mut cache, "https://example.com/a.jpg", b"img", t0);
        assert_eq!(
            cache_lookup(
                &mut cache,
                "https://example.com/a.jpg",
                t0 + Duration::from_secs(5)
            ),
            Some(b"img".to_vec())
        );
        // Expired entries are dropped and must not be served.
        assert_eq!(
            cache_lookup(
                &mut cache,
                "https://example.com/a.jpg",
                t0 + URL_CACHE_TTL + Duration::from_secs(1)
            ),
            None
        );
        assert!(cache.is_empty());
    }

    #[test]
    fn url_cache_is_bounded_and_keeps_newest() {
        let mut cache: HashMap<String, (Vec<u8>, Instant)> = HashMap::new();
        let t0 = Instant::now();
        for i in 0..(URL_CACHE_CAPACITY + 2) {
            let url = format!("https://example.com/{i}.jpg");
            cache_store(&mut cache, &url, b"x", t0 + Duration::from_secs(i as u64));
        }
        assert_eq!(cache.len(), URL_CACHE_CAPACITY);
        // The two oldest entries were evicted.
        assert!(!cache.contains_key("https://example.com/0.jpg"));
        assert!(!cache.contains_key("https://example.com/1.jpg"));
        assert!(cache.contains_key("https://example.com/9.jpg"));
    }
}
