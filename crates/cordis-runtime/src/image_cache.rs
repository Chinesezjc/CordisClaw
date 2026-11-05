//! 内容寻址的图片缓存：`data/media/<sha256>`。
//!
//! 为什么需要本模块：请求体字节预算（[`crate::image_budget`]）会把超预算的
//! 历史图片换成一行占位符，占位符里只剩 sha256 前 8 位与尺寸——模型从此再也
//! 看不到那张图。给占位符补一段 `path data/media/<sha256>`，再用 `view_image`
//! 工具按路径（或摘要前缀）把原始字节读回来，就补上了"取回"这一环。
//!
//! **存**：kernel 下载入站图片时把**原始字节**写到 `<data>/media/<sha256>`；
//! 同名文件已存在就跳过写（内容寻址，天然去重）。写入走"临时文件 + rename"，
//! 读侧永远看不到半截文件。
//!
//! **取**：`view_image` 只接受 `data/media/<name>` 形状的相对路径（拒绝绝对
//! 路径、`..`、子目录），或 8~64 位十六进制摘要前缀（在缓存目录里唯一匹配）。
//!
//! 测试不碰仓库里的 `data/`：所有落盘根目录都由调用方注入（测试传临时目录）。

use serde_json::{json, Value};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// 占位符里给出的缓存目录（相对仓库工作目录）。
pub const MEDIA_DIR: &str = "data/media";

/// 图片字节的 sha256（64 位小写十六进制）。缓存文件名用的就是它。
pub fn digest_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// 占位符里引用的相对路径：`data/media/<sha256>`。
pub fn placeholder_path(digest: &str) -> String {
    format!("{MEDIA_DIR}/{digest}")
}

/// 把原始字节写进内容寻址缓存，返回落盘路径。
///
/// 已存在就跳过写——同一张图被下载两次是常态（群里转发、渠道重放），按摘要
/// 去重后第二次是纯 `stat`。写入先落临时文件再 rename，避免并发读看到半截。
pub fn store(root: &Path, bytes: &[u8]) -> Result<PathBuf, String> {
    let name = digest_hex(bytes);
    let path = root.join(&name);
    if path.exists() {
        return Ok(path);
    }
    std::fs::create_dir_all(root).map_err(|err| format!("create {}: {err}", root.display()))?;
    let staging = staging_path(root, &name);
    std::fs::write(&staging, bytes).map_err(|err| format!("write {}: {err}", staging.display()))?;
    std::fs::rename(&staging, &path).map_err(|err| format!("rename {}: {err}", path.display()))?;
    Ok(path)
}

/// 同目录下的唯一临时名（`.tmp` 后缀不会与 64 位十六进制的缓存名冲突）。
fn staging_path(root: &Path, name: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    root.join(format!(".{name}.tmp.{}.{seq}", std::process::id()))
}

/// kernel 的下载入口：取回字节后**顺带**落进内容寻址缓存。
///
/// 缓存写失败只记一行日志、不影响本次请求——图片已经在内存里了，缓存是
/// "以后还能取回"的额外保障，不是发图的前置条件。
pub fn fetch_and_cache(url: &str, timeout_ms: u64, cache_dir: &Path) -> Result<Vec<u8>, String> {
    let bytes = crate::image_input::fetch_http(url, timeout_ms)?;
    if let Err(err) = store(cache_dir, &bytes) {
        eprintln!("image_cache: store failed for {url}: {err}");
    }
    Ok(bytes)
}

/// 注入给 [`crate::image_input::plan_image_parts`] 的 fetcher：下载 + 落缓存。
///
/// 单独成函数（而不是在 host 里就地写闭包）是为了让它有自己的单测：真正的
/// 下载路径由 [`fetch_and_cache`] 承担，这里只做一次转接。
pub fn caching_fetcher(cache_dir: &Path) -> impl Fn(&str, u64) -> Result<Vec<u8>, String> + '_ {
    move |url: &str, timeout_ms: u64| fetch_and_cache(url, timeout_ms, cache_dir)
}

/// `view_image` 的读取实现：把缓存里的一张图变成 `image_url` content part。
///
/// `path` 与 `sha256` 二选一。返回 `(占位符里那种相对路径, content part)`，
/// 调用方把 part 挂到下一次请求上、把路径回给模型。
pub fn load_part(
    media_dir: &Path,
    path: Option<&str>,
    sha256: Option<&str>,
) -> Result<(String, Value), String> {
    let file = match (path, sha256) {
        (Some(path), None) => media_dir.join(media_relative(path)?),
        (None, Some(digest)) => resolve_by_prefix(media_dir, digest.trim())?,
        (Some(_), Some(_)) => return Err("provide either path or sha256, not both".to_string()),
        (None, None) => return Err("either path or sha256 is required".to_string()),
    };
    let bytes = std::fs::read(&file).map_err(|err| format!("read {}: {err}", file.display()))?;
    let mime = sniff_image(&file, &bytes)?;
    let part = json!({
        "type": "image_url",
        "image_url": { "url": format!("data:{mime};base64,{}", crate::image_input::base64_encode(&bytes)) },
    });
    Ok((placeholder_path(&file_name_of(&file)), part))
}

/// 缓存文件名（路径一定是 `<media>/<name>`，取不到时退回空串）。
fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn sniff_image(path: &Path, bytes: &[u8]) -> Result<&'static str, String> {
    crate::image_input::sniff_mime(bytes).ok_or_else(|| {
        format!(
            "{} is not a recognized image (png/jpeg/gif/webp)",
            path.display()
        )
    })
}

/// 校验 `data/media/<name>` 形状的相对路径，返回文件名。
///
/// 逐段匹配而不是字符串前缀：字符串前缀会让 `data/media/../etc/passwd` 与
/// `data/media-x/y` 都蒙混过关，而这里 `ParentDir`、`RootDir`、子目录一律
/// 落到 `_` 臂被拒。
fn media_relative(path: &str) -> Result<String, String> {
    let invalid = || format!("path must be {MEDIA_DIR}/<sha256>: {path}");
    let mut parts = Path::new(path).components();
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (
            Some(Component::Normal(first)),
            Some(Component::Normal(second)),
            Some(Component::Normal(name)),
            None,
        ) if first == "data" && second == "media" => Ok(name.to_string_lossy().into_owned()),
        _ => Err(invalid()),
    }
}

/// 按摘要前缀在缓存目录里找唯一匹配；0 个或多个都报错。
fn resolve_by_prefix(root: &Path, prefix: &str) -> Result<PathBuf, String> {
    if prefix.is_empty() || !prefix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("sha256 must be hexadecimal: {prefix}"));
    }
    let entries =
        std::fs::read_dir(root).map_err(|err| format!("read {}: {err}", root.display()))?;
    let mut hits: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with(prefix) {
            continue;
        }
        // 只认普通文件：`data/media/` 下还躺着 QQ 群图归档的子目录
        //（`data/media/<群号>/`），前缀撞上群号时不能把它当缓存条目。
        if !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        hits.push(entry.path());
    }
    match hits.len() {
        0 => Err(format!("no cached image matches sha256 prefix {prefix}")),
        1 => Ok(hits.remove(0)),
        _ => Err(format!(
            "sha256 prefix {prefix} matches {} cached images; use more characters",
            hits.len()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR";

    fn media_root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("media")).expect("media dir");
        dir
    }

    #[test]
    fn digest_matches_known_vectors() {
        assert_eq!(&digest_hex(b"")[..8], "e3b0c442");
        assert_eq!(&digest_hex(b"abc")[..8], "ba7816bf");
        assert_eq!(digest_hex(b"abc").len(), 64);
    }

    #[test]
    fn placeholder_path_is_the_relative_cache_address() {
        assert_eq!(
            placeholder_path("ab12cd34"),
            "data/media/ab12cd34".to_string()
        );
    }

    #[test]
    fn store_is_content_addressed_and_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("media");
        let first = store(&root, PNG).expect("store");
        assert_eq!(first.file_name().unwrap(), digest_hex(PNG).as_str());
        assert_eq!(std::fs::read(&first).expect("read back"), PNG);
        // 第二次走 `exists` 短路：路径相同，内容不变。
        let second = store(&root, PNG).expect("store again");
        assert_eq!(second, first);
        assert_eq!(std::fs::read(&second).expect("read back"), PNG);
        // 只有那一个文件：临时文件已经被 rename 掉。
        assert_eq!(std::fs::read_dir(&root).expect("dir").count(), 1);
    }

    #[test]
    fn store_reports_a_cache_root_that_cannot_be_created() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("media");
        // 目录位置被文件占住：create_dir_all 必失败。
        std::fs::write(&root, b"not a dir").expect("write squatter");
        let err = store(&root, PNG).expect_err("must fail");
        assert!(err.starts_with("create "), "err={err}");
    }

    /// 起一个只应答一次的 HTTP server，返回 `(url, join handle)`。
    fn serve_once(payload: Vec<u8>) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = std::thread::spawn(move || {
            let mut stream = listener.accept().expect("accept").0;
            let mut buf = [0u8; 512];
            let _ = stream.read(&mut buf);
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(&payload);
        });
        (format!("http://{addr}/image.png"), server)
    }

    #[test]
    fn fetch_and_cache_downloads_and_stores() {
        let payload = b"\x89PNG\r\n\x1a\nfetched".to_vec();
        let (url, server) = serve_once(payload.clone());
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("media");
        let bytes = fetch_and_cache(&url, 2_000, &root).expect("download");
        let _ = server.join();
        assert_eq!(bytes, payload);
        let cached = root.join(digest_hex(&payload));
        assert_eq!(std::fs::read(&cached).expect("cached"), payload);
    }

    /// 缓存写失败不是下载失败：字节照样返回给本轮请求，只打一行日志。
    #[test]
    fn fetch_and_cache_keeps_bytes_when_the_cache_is_unwritable() {
        let payload = b"\x89PNG\r\n\x1a\nunwritable".to_vec();
        let (url, server) = serve_once(payload.clone());
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("media");
        std::fs::write(&root, b"not a dir").expect("write squatter");
        let bytes = fetch_and_cache(&url, 2_000, &root).expect("download");
        let _ = server.join();
        assert_eq!(bytes, b"\x89PNG\r\n\x1a\nunwritable");
    }

    /// host 直接把 [`caching_fetcher`] 交给图片规划器：下载、落缓存、并组出
    /// 一张 `image_url` part。
    #[test]
    fn caching_fetcher_downloads_through_the_image_planner() {
        let payload = b"\x89PNG\r\n\x1a\nplanner".to_vec();
        let (url, server) = serve_once(payload.clone());
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("media");
        let (parts, skips) = crate::image_input::plan_image_parts(
            &[url],
            crate::image_input::ImageLimits::default(),
            &caching_fetcher(&root),
        );
        let _ = server.join();
        assert!(skips.is_empty(), "{skips:?}");
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["type"], "image_url");
        let cached = root.join(digest_hex(&payload));
        assert_eq!(std::fs::read(&cached).expect("cached"), payload);
    }

    #[test]
    fn fetch_and_cache_propagates_transport_failures() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = fetch_and_cache("http://127.0.0.1:1/x.png", 200, dir.path()).expect_err("fail");
        assert!(err.starts_with("request:"), "err={err}");
    }

    #[test]
    fn load_part_by_path_reads_the_cached_bytes() {
        let dir = media_root();
        let root = dir.path().join("media");
        let stored = store(&root, PNG).expect("store");
        let digest = digest_hex(PNG);
        let (rel, part) = load_part(&root, Some(&placeholder_path(&digest)), None).expect("load");
        assert_eq!(rel, placeholder_path(&digest));
        assert_eq!(part["type"], "image_url");
        assert_eq!(
            part["image_url"]["url"],
            format!(
                "data:image/png;base64,{}",
                crate::image_input::base64_encode(PNG)
            )
        );
        assert!(stored.exists());
    }

    #[test]
    fn load_part_by_digest_prefix_resolves_a_unique_entry() {
        let dir = media_root();
        let root = dir.path().join("media");
        store(&root, PNG).expect("store");
        let digest = digest_hex(PNG);
        let (rel, _part) = load_part(&root, None, Some(&digest[..8])).expect("load");
        assert_eq!(rel, placeholder_path(&digest));
    }

    #[test]
    fn load_part_by_digest_rejects_ambiguous_and_missing_prefixes() {
        let dir = media_root();
        let root = dir.path().join("media");
        std::fs::write(root.join("aa11"), PNG).expect("write a");
        std::fs::write(root.join("aa22"), PNG).expect("write b");
        let err = load_part(&root, None, Some("aa")).expect_err("ambiguous");
        assert!(err.contains("matches 2 cached images"), "err={err}");
        let err = load_part(&root, None, Some("ff")).expect_err("missing");
        assert!(err.contains("no cached image"), "err={err}");
        let err = load_part(&root, None, Some("zz")).expect_err("not hex");
        assert!(err.contains("must be hexadecimal"), "err={err}");
        let err = load_part(&root, None, Some("")).expect_err("empty");
        assert!(err.contains("must be hexadecimal"), "err={err}");
    }

    #[test]
    fn load_part_requires_exactly_one_selector() {
        let dir = media_root();
        let root = dir.path().join("media");
        let err = load_part(&root, Some("data/media/x"), Some("ab")).expect_err("both");
        assert_eq!(err, "provide either path or sha256, not both");
        let err = load_part(&root, None, None).expect_err("neither");
        assert_eq!(err, "either path or sha256 is required");
    }

    #[test]
    fn load_part_rejects_paths_outside_the_cache() {
        let dir = media_root();
        let root = dir.path().join("media");
        for path in [
            "data/media/../secret.png",
            "/etc/passwd",
            "data/media",
            "data/media/sub/x.png",
            "media/x.png",
            "data/other/x.png",
        ] {
            let err = load_part(&root, Some(path), None).expect_err(path);
            assert!(
                err.starts_with("path must be data/media/<sha256>"),
                "{path}: {err}"
            );
        }
    }

    #[test]
    fn load_part_reports_missing_files_and_non_images() {
        let dir = media_root();
        let root = dir.path().join("media");
        let err = load_part(&root, Some("data/media/abc"), None).expect_err("missing");
        assert!(err.starts_with("read "), "err={err}");

        std::fs::write(root.join("abc"), b"just text").expect("write");
        let err = load_part(&root, Some("data/media/abc"), None).expect_err("not an image");
        assert!(err.contains("not a recognized image"), "err={err}");
    }

    #[test]
    fn resolve_by_prefix_reports_an_unreadable_cache_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = resolve_by_prefix(&dir.path().join("nope"), "ab").expect_err("missing dir");
        assert!(err.starts_with("read "), "err={err}");
    }

    /// 同名前缀的子目录（`data/media/` 下的 QQ 群图归档）不算缓存条目。
    #[test]
    fn resolve_by_prefix_ignores_subdirectories_that_share_the_prefix() {
        let dir = media_root();
        let root = dir.path().join("media");
        std::fs::create_dir_all(root.join("ab12")).expect("group dir");
        std::fs::write(root.join("ab34"), PNG).expect("write cache file");
        let hit = resolve_by_prefix(&root, "ab").expect("unique regular file");
        assert_eq!(hit, root.join("ab34"));
    }

    #[test]
    fn media_relative_accepts_only_the_cache_shape() {
        assert_eq!(media_relative("data/media/abc").expect("ok"), "abc");
        assert!(media_relative("data/media").is_err());
        assert!(media_relative("data/media/").is_err());
        assert!(media_relative("data/media/a/b").is_err());
        assert!(media_relative("data/media/.").is_err());
    }
}
