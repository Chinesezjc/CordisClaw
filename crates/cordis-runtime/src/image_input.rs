//! 图片入站：把消息里带的图片 URL 变成 OpenAI 兼容的 `image_url` content part。
//!
//! 为什么由 kernel 自己下载并内联成 `data:` URI，而不是把 URL 甩给供应商：
//! 供应商侧抓取不可靠（实测 `deepseek-flash` 拉一张 wikimedia 图直接
//! `400 Failed to download image`），而 QQ 的图片链接是
//! `multimedia.nt.qq.com.cn/...&rkey=...` 这种带时效的地址。所以这里
//! 「先取字节 → 嗅探类型 → base64 内联」，供应商只需解码。
//!
//! 三条边界都只降级、不失败：单图超字节、每批超张数、下载失败 —— 都是记一条
//! [`ImageSkip`] 后跳过该图，本轮照常用文本继续（绝不因为一张图毁掉整轮）。
//!
//! 本模块是纯逻辑 + 一个注入式 fetch，网络边界放在 [`fetch_http`]，
//! 因此所有分支都能用假 fetcher 直接单测。

use serde_json::{json, Map, Value};

/// 每批图片的预算。默认值见 [`ImageLimits::default`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageLimits {
    /// 单张图片的字节上限（内联后请求体会更大，约 4/3 倍）。
    pub max_bytes: usize,
    /// 每批最多内联几张；超出的按输入顺序丢弃。
    pub max_per_batch: usize,
    /// 单张图片的下载超时（毫秒）。
    pub timeout_ms: u64,
}

impl Default for ImageLimits {
    /// 2 张 / 4 MiB / 8s：群里连发图很常见，2 张是 token 成本与"看得见"的
    /// 折中；4 MiB 覆盖 QQ 压缩图，再大内联进请求体会拖慢首字节。
    fn default() -> Self {
        Self {
            max_bytes: 4 * 1024 * 1024,
            max_per_batch: 2,
            timeout_ms: 8_000,
        }
    }
}

/// 一张被跳过的图片及原因（进日志，不进请求）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageSkip {
    pub url: String,
    pub reason: String,
}

/// 按魔数嗅探图片类型；不认识就返回 `None`（宁可不发，也不给供应商塞垃圾）。
pub fn sniff_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    // RIFF....WEBP
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

/// 把 [`ImageLimits`] 之外的一切都跳过：张数、字节数、下载失败、非图片。
///
/// 返回 `(parts, skips)`：`parts` 直接塞进 user 消息的 `content` 数组，
/// `skips` 供调用方打日志。顺序与输入一致；被跳过的不会占位。
pub fn plan_image_parts<F>(
    urls: &[String],
    limits: ImageLimits,
    fetch: &F,
) -> (Vec<Value>, Vec<ImageSkip>)
where
    F: Fn(&str, u64) -> Result<Vec<u8>, String>,
{
    let mut parts = Vec::new();
    let mut skips = Vec::new();
    for url in urls {
        if parts.len() >= limits.max_per_batch {
            skips.push(ImageSkip {
                url: url.clone(),
                reason: format!("batch limit {}", limits.max_per_batch),
            });
            continue;
        }
        let bytes = match fetch(url, limits.timeout_ms) {
            Ok(bytes) => bytes,
            Err(err) => {
                skips.push(ImageSkip {
                    url: url.clone(),
                    reason: format!("fetch failed: {err}"),
                });
                continue;
            }
        };
        if bytes.len() > limits.max_bytes {
            skips.push(ImageSkip {
                url: url.clone(),
                reason: format!("{} bytes over limit {}", bytes.len(), limits.max_bytes),
            });
            continue;
        }
        let mime = match sniff_mime(&bytes) {
            Some(mime) => mime,
            None => {
                skips.push(ImageSkip {
                    url: url.clone(),
                    reason: "unrecognized image type".to_string(),
                });
                continue;
            }
        };
        parts.push(json!({
            "type": "image_url",
            "image_url": { "url": format!("data:{mime};base64,{}", base64_encode(&bytes)) },
        }));
    }
    (parts, skips)
}

/// 按 `runtime.images` 配置决定要不要带图，并把配置转成 [`ImageLimits`]。
///
/// 关掉（`enabled = false`）时返回空 parts 且**不产生 skip**（这是配置选择，
/// 不是失败，不该刷日志）。
pub fn plan_for_settings<F>(
    settings: &crate::config::ImageSettings,
    urls: &[String],
    fetch: &F,
) -> (Vec<Value>, Vec<ImageSkip>)
where
    F: Fn(&str, u64) -> Result<Vec<u8>, String>,
{
    if !settings.enabled {
        return (Vec::new(), Vec::new());
    }
    let limits = ImageLimits {
        max_bytes: settings.max_bytes,
        max_per_batch: settings.max_per_batch,
        timeout_ms: settings.timeout_ms,
    };
    plan_image_parts(urls, limits, fetch)
}

/// 组一条 user 消息的 `content`。
///
/// **无图时返回 `Value::String`**，与引入图片之前逐字节一致（既有请求体形状
/// 不变，避免历史/工具链断言被动到）；有图时返回
/// `[{"type":"text"}, {"type":"image_url"}...]`，文本在前。
pub fn user_content_with_images(text: &str, parts: &[Value]) -> Value {
    if parts.is_empty() {
        return Value::String(text.to_string());
    }
    let mut content: Vec<Value> = Vec::with_capacity(parts.len() + 1);
    let mut text_part = Map::new();
    text_part.insert("type".to_string(), Value::String("text".to_string()));
    text_part.insert("text".to_string(), Value::String(text.to_string()));
    content.push(Value::Object(text_part));
    content.extend(parts.iter().cloned());
    Value::Array(content)
}

/// 真实下载：阻塞式 HTTP + 硬超时。失败一律返回 `Err(String)` 交给上层跳过。
pub fn fetch_http(url: &str, timeout_ms: u64) -> Result<Vec<u8>, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_millis(timeout_ms.max(1)))
        .build()
        .map_err(|err| format!("client: {err}"))?;
    let response = client
        .get(url)
        .send()
        .map_err(|err| format!("request: {err}"))?;
    if !response.status().is_success() {
        return Err(format!("status {}", response.status()));
    }
    response
        .bytes()
        .map(|bytes| bytes.to_vec())
        .map_err(|err| format!("body: {err}"))
}

/// 标准 base64（带 `=` 填充）。自己实现 20 行，省掉一个新依赖。
pub fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[(triple >> 18) as usize & 63] as char);
        out.push(TABLE[(triple >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(triple >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[triple as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR";
    const JPEG: &[u8] = b"\xFF\xD8\xFF\xE0rest";
    const GIF: &[u8] = b"GIF89a-rest";
    const WEBP: &[u8] = b"RIFF\x24\x00\x00\x00WEBPVP8 ";

    #[test]
    fn sniff_recognizes_the_four_supported_types() {
        assert_eq!(sniff_mime(PNG), Some("image/png"));
        assert_eq!(sniff_mime(JPEG), Some("image/jpeg"));
        assert_eq!(sniff_mime(GIF), Some("image/gif"));
        assert_eq!(sniff_mime(WEBP), Some("image/webp"));
        assert_eq!(sniff_mime(b"GIF87a.."), Some("image/gif"));
    }

    #[test]
    fn sniff_rejects_unknown_and_short_buffers() {
        assert_eq!(sniff_mime(b"not an image"), None);
        assert_eq!(sniff_mime(b""), None);
        // RIFF but not WEBP (e.g. a wav file)
        assert_eq!(sniff_mime(b"RIFF\x24\x00\x00\x00WAVEfmt "), None);
        // Truncated PNG magic
        assert_eq!(sniff_mime(b"\x89PNG"), None);
    }

    #[test]
    fn base64_covers_all_three_chunk_lengths() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn plan_builds_parts_in_input_order() {
        let urls = vec!["a".to_string(), "b".to_string()];
        let (parts, skips) = plan_image_parts(
            &urls,
            ImageLimits::default(),
            &(|_url: &str, _t: u64| Ok(PNG.to_vec())),
        );
        assert!(skips.is_empty());
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["type"], "image_url");
        let uri = parts[0]["image_url"]["url"].as_str().unwrap_or_default();
        assert!(uri.starts_with("data:image/png;base64,"), "uri={uri}");
    }

    #[test]
    fn plan_skips_beyond_the_batch_limit() {
        let limits = ImageLimits {
            max_per_batch: 1,
            ..ImageLimits::default()
        };
        let urls = vec!["a".to_string(), "b".to_string()];
        let (parts, skips) =
            plan_image_parts(&urls, limits, &(|_url: &str, _t: u64| Ok(PNG.to_vec())));
        assert_eq!(parts.len(), 1);
        assert_eq!(skips.len(), 1);
        assert_eq!(skips[0].url, "b");
        assert!(skips[0].reason.starts_with("batch limit"), "{:?}", skips[0]);
    }

    #[test]
    fn plan_skips_fetch_failures_and_keeps_the_rest() {
        let urls = vec!["bad".to_string(), "good".to_string()];
        let (parts, skips) = plan_image_parts(
            &urls,
            ImageLimits::default(),
            &(|url: &str, _t: u64| {
                if url == "bad" {
                    Err("boom".to_string())
                } else {
                    Ok(PNG.to_vec())
                }
            }),
        );
        assert_eq!(parts.len(), 1);
        assert_eq!(skips.len(), 1);
        assert_eq!(skips[0].reason, "fetch failed: boom");
    }

    #[test]
    fn plan_skips_oversized_and_unknown_types() {
        let big = vec![0u8; 8];
        let limits = ImageLimits {
            max_bytes: 4,
            ..ImageLimits::default()
        };
        let urls = vec!["big".to_string()];
        let (parts, skips) =
            plan_image_parts(&urls, limits, &(|_url: &str, _t: u64| Ok(big.clone())));
        assert!(parts.is_empty());
        assert!(skips[0].reason.contains("over limit"), "{:?}", skips[0]);

        let urls = vec!["unknown".to_string()];
        let (parts, skips) = plan_image_parts(
            &urls,
            ImageLimits::default(),
            &(|_url: &str, _t: u64| Ok(b"just text".to_vec())),
        );
        assert!(parts.is_empty());
        assert_eq!(skips[0].reason, "unrecognized image type");
    }

    #[test]
    fn plan_for_settings_disabled_attaches_nothing_and_skips_nothing() {
        let settings = crate::config::ImageSettings {
            enabled: false,
            ..crate::config::ImageSettings::default()
        };
        let urls = vec!["a".to_string()];
        let (parts, skips) =
            plan_for_settings(&settings, &urls, &(|_u: &str, _t: u64| Ok(PNG.to_vec())));
        assert!(parts.is_empty());
        assert!(skips.is_empty());
    }

    #[test]
    fn plan_for_settings_enabled_uses_the_configured_limits() {
        let settings = crate::config::ImageSettings {
            max_per_batch: 1,
            ..crate::config::ImageSettings::default()
        };
        let urls = vec!["a".to_string(), "b".to_string()];
        let (parts, skips) =
            plan_for_settings(&settings, &urls, &(|_u: &str, _t: u64| Ok(PNG.to_vec())));
        assert_eq!(parts.len(), 1);
        assert_eq!(skips.len(), 1);
        assert!(skips[0].reason.starts_with("batch limit"));
    }

    #[test]
    fn user_content_without_images_stays_a_plain_string() {
        let content = user_content_with_images("hello", &[]);
        assert_eq!(content, Value::String("hello".to_string()));
    }

    #[test]
    fn user_content_with_images_is_a_text_first_array() {
        let parts = vec![json!({"type": "image_url", "image_url": {"url": "data:x"}})];
        let content = user_content_with_images("看图", &parts);
        let array = content.as_array().expect("array content");
        assert_eq!(array.len(), 2);
        assert_eq!(array[0]["type"], "text");
        assert_eq!(array[0]["text"], "看图");
        assert_eq!(array[1]["type"], "image_url");
    }

    #[test]
    fn fetch_http_returns_the_body_bytes_on_success() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let payload = b"PNGDATA".to_vec();
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind 200 server");
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
        let bytes = fetch_http(&format!("http://{addr}/ok.png"), 2_000).expect("200");
        let _ = server.join();
        assert_eq!(bytes, b"PNGDATA");
    }

    #[test]
    fn fetch_http_reports_a_non_success_status() {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind 404 server");
        let addr = listener.local_addr().expect("addr");
        let server = std::thread::spawn(move || {
            // `expect` 而不是 `if let Ok`：后者会留一个永不执行的隐式 else，
            // 把收尾的 `}` 那一行拖成零计数。
            let mut stream = listener.accept().expect("accept").0;
            let mut buf = [0u8; 512];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(
                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
        });
        let err = fetch_http(&format!("http://{addr}/missing.png"), 2_000).expect_err("404");
        let _ = server.join();
        assert!(err.starts_with("status "), "err={err}");
    }

    #[test]
    fn fetch_http_reports_transport_failure() {
        // 127.0.0.1:1 必定拒绝连接：走 request 错误臂，不需要真网。
        let err = fetch_http("http://127.0.0.1:1/x.png", 200).expect_err("must fail");
        assert!(err.starts_with("request:"), "err={err}");
    }
}
