//! 发送前的图片字节预算：内联图片攒到超预算时，决定剥掉哪些。
//!
//! 为什么需要本模块：`estimated_tokens` 是**保守的请求重量**估算（图片按
//! base64 折 token），而网关拒收看的是**请求体字节**——两者是不同的旋钮，
//! 谁也不能代替谁。线上一个 1081 条消息的群会话曾攒到 47.6 MB 请求体（46 条
//! 带 `image_url`，最大 5 条各 2.7–4.2 MB），被网关 413
//! `Failed to buffer the request body: length limit exceeded` 拒收，消息落进
//! `data/pending/` 重放队列，机器人整轮发不出话。
//!
//! 本模块是**纯函数**，不碰 I/O：输入「按时间顺序的消息列表」+ 字节预算，
//! 输出「哪些消息里的图片需要被剥掉」。方向固定为**从最老的一张开始**——
//! 最新的图片最可能正是这一轮在讨论的内容，先舍旧的。
//!
//! 字节口径：一张原始字节数为 `len` 的图片内联成 base64 后，请求体里占
//! `ceil(len / 3) * 4` 字节。本模块先按 base64 规则反解出 `len`，再用该公式
//! 折算，而不是拿 URI 字符串长度近似——只有 payload 规范时两者才相等，而
//! 供应商能接受的正是规范 payload。
//!
//! 只认 `data:...;base64,` 的内联 URI：远程 `https://` 图片不占内联字节
//! （由供应商侧抓取），因此既不计入预算，也不会被剥掉。

use serde_json::{json, Value};

/// 一次内联请求体里图片字节的目标上限：10 MiB。
///
/// 对齐 DSH 的 inline 20 MiB 的一半：保守，给 system prompt、文本历史与
/// JSON 包装留出余量。这是文档化常量而非 CLI 参数——预算是请求装配的内部
/// 约束，不该变成运维旋钮。
pub const INLINE_IMAGE_BUDGET_BYTES: usize = 10 * 1024 * 1024;

/// 一条「把这里的图片换成占位符」的指令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageOmission {
    /// 消息在输入列表里的下标（列表按时间顺序，因此也是历史顺序）。
    pub message_index: usize,
    /// 该消息 `content` 数组里图片 part 的下标。
    pub part_index: usize,
    /// 替换进去的文本占位符。
    pub placeholder: String,
}

/// 一次预算规划的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageBudgetPlan {
    /// 规划前的内联图片总字节。
    pub bytes_before: usize,
    /// 按计划剥掉之后剩下的内联图片总字节（≤ 预算）。
    pub bytes_after: usize,
    /// 需要剥掉的图片，按从老到新排列。
    pub omissions: Vec<ImageOmission>,
}

impl ImageBudgetPlan {
    /// 一张都不用剥。
    pub fn is_empty(&self) -> bool {
        self.omissions.is_empty()
    }

    /// 剥掉的张数。
    pub fn omitted(&self) -> usize {
        self.omissions.len()
    }
}

/// 消息里的一张内联图片（借用 URI，避免为了记账复制几 MB 字符串）。
struct InlineImage<'a> {
    message_index: usize,
    part_index: usize,
    uri: &'a str,
    bytes: usize,
}

/// `bytes` 个原始字节内联成 base64 之后占的精确字节数。
pub fn inline_base64_len(bytes: usize) -> usize {
    bytes.div_ceil(3) * 4
}

/// 一张内联图片占的请求体字节；不是 `data:...;base64,` 就返回 `None`。
pub fn inline_image_bytes(uri: &str) -> Option<usize> {
    base64_decoded_len(data_uri_payload(uri)?).map(inline_base64_len)
}

/// 一组 content part（一条 user 消息的 `content` 数组）里的内联图片总字节。
///
/// 调用方用它预留「这一轮新消息里的图片」——新图片还没进 history，但同样占
/// 请求体，不从历史预算里扣掉就还是会超。
pub fn image_parts_bytes(parts: &[Value]) -> usize {
    parts
        .iter()
        .filter_map(image_url_of)
        .filter_map(inline_image_bytes)
        .sum()
}

/// 规划：从最老的图片开始剥，直到剩下的内联图片字节 ≤ `budget`。
///
/// 纯函数——只看输入，不改 `messages`。应用由 [`apply_image_omissions`] 负责。
pub fn plan_image_omissions(messages: &[Value], budget: usize) -> ImageBudgetPlan {
    let mut images: Vec<InlineImage<'_>> = Vec::new();
    for (message_index, message) in messages.iter().enumerate() {
        let Some(parts) = message.get("content").and_then(Value::as_array) else {
            continue;
        };
        for (part_index, part) in parts.iter().enumerate() {
            let Some(uri) = image_url_of(part) else {
                continue;
            };
            let Some(bytes) = inline_image_bytes(uri) else {
                continue;
            };
            images.push(InlineImage {
                message_index,
                part_index,
                uri,
                bytes,
            });
        }
    }
    let bytes_before = images.iter().map(|image| image.bytes).sum();
    let mut bytes_after = bytes_before;
    let mut omissions = Vec::new();
    for image in images {
        if bytes_after <= budget {
            break;
        }
        bytes_after -= image.bytes;
        omissions.push(ImageOmission {
            message_index: image.message_index,
            part_index: image.part_index,
            placeholder: placeholder_for(image.uri),
        });
    }
    ImageBudgetPlan {
        bytes_before,
        bytes_after,
        omissions,
    }
}

/// 就地按计划把图片 part 换成文本占位符，返回实际替换的张数。
///
/// 幂等：下标已经指向文本 part（或消息/数组已不存在）时跳过，所以对同一个
/// 计划跑两次不会产生第二层占位符。
pub fn apply_image_omissions(messages: &mut [Value], plan: &ImageBudgetPlan) -> usize {
    let mut applied = 0;
    for omission in &plan.omissions {
        let part = messages
            .get_mut(omission.message_index)
            .and_then(|message| message.get_mut("content"))
            .and_then(Value::as_array_mut)
            .and_then(|parts| parts.get_mut(omission.part_index));
        let Some(part) = part else {
            continue;
        };
        if part.get("type").and_then(Value::as_str) != Some("image_url") {
            continue;
        }
        *part = json!({ "type": "text", "text": omission.placeholder });
        applied += 1;
    }
    applied
}

/// 规划 + 应用一步到位，返回计划（`is_empty()` 即「什么也没剥」）。
///
/// 这是请求装配点该调的入口：剥掉的图片在 `messages` 里就地变成占位符，
/// 因此调用方若传入的是**持久化历史**，占位符会跟着历史一起留下来，下一轮
/// 不必再从原图重算。
pub fn enforce_image_budget(messages: &mut [Value], budget: usize) -> ImageBudgetPlan {
    let plan = plan_image_omissions(messages, budget);
    apply_image_omissions(messages, &plan);
    plan
}

/// 把消息里**所有**内联图片换成占位符，返回替换的张数。
///
/// 等价于预算为零的 [`enforce_image_budget`]，但意图更直白：调用方要的是
/// 「一条内联 base64 都不留」，而不是「预算恰好是 0」。会话快照落盘前用它把
/// `history` 里几十 MB 的 base64 换成短占位符；远程 `https://` 图片不是内联
/// 字节，不会被替换。占位符构造仍走 [`placeholder_for`]，不复制实现。
pub fn strip_inline_images(messages: &mut [Value]) -> usize {
    enforce_image_budget(messages, 0).omitted()
}

/// 一个 content part 的 `image_url.url`（不是图片 part 就返回 `None`）。
fn image_url_of(part: &Value) -> Option<&str> {
    match part.get("type").and_then(Value::as_str) {
        Some("image_url") => part.get("image_url")?.get("url")?.as_str(),
        _ => None,
    }
}

/// `data:<mime>;base64,<payload>` 里的 payload。
fn data_uri_payload(uri: &str) -> Option<&str> {
    let rest = uri.strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    if !meta.ends_with(";base64") {
        return None;
    }
    Some(payload)
}

/// payload 解码后的原始字节数（顺带校验它是规范 base64）。
///
/// 只做「数」不做「解」：本轮的绝大多数图片并不会被剥掉，为它们分配几十 MB
/// 缓冲区只是白烧内存；真正要剥的那几张才在 [`placeholder_for`] 里完整解码
/// （算 sha256 与尺寸）。这里只扫一遍字符校验，算出原始字节数再按
/// `ceil(len / 3) * 4` 折算。
fn base64_decoded_len(payload: &str) -> Option<usize> {
    let bytes = payload.as_bytes();
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
        return None;
    }
    // 填充只允许在尾部，且最多两个。
    let mut padding = 0usize;
    for byte in bytes.iter().rev() {
        if *byte != b'=' {
            break;
        }
        padding += 1;
    }
    if padding > 2
        || !bytes[..bytes.len() - padding]
            .iter()
            .all(|b| is_base64_char(*b))
    {
        return None;
    }
    Some(bytes.len() / 4 * 3 - padding)
}

fn is_base64_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'/'
}

/// 把 base64 payload 解成原始字节；只接受规范编码（长度 4 的倍数、填充在尾部）。
fn base64_decode(payload: &str) -> Option<Vec<u8>> {
    let bytes = payload.as_bytes();
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        let mut triple = 0u32;
        let mut padding = 0usize;
        for (index, byte) in chunk.iter().enumerate() {
            let value = match decode_char(*byte) {
                Some(value) => value,
                None if *byte == b'=' && index >= 2 => {
                    padding += 1;
                    0
                }
                None => return None,
            };
            triple = (triple << 6) | u32::from(value);
        }
        out.push((triple >> 16) as u8);
        if padding < 2 {
            out.push((triple >> 8) as u8);
        }
        if padding < 1 {
            out.push(triple as u8);
        }
    }
    Some(out)
}

fn decode_char(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// 占位符文本。形状照 DSH：
/// `[image omitted to fit request byte budget; sha256:<前8位>; <W>x<H> <fmt>]`。
///
/// 拿不到尺寸或格式时只写 sha256；连字节都解不出来时只写 omitted——占位符
/// 只说「这张图被预算挤掉了」，不猜内容。
fn placeholder_for(uri: &str) -> String {
    const PREFIX: &str = "[image omitted to fit request byte budget";
    let Some(bytes) = data_uri_payload(uri).and_then(base64_decode) else {
        return format!("{PREFIX}]");
    };
    let digest = sha256(&bytes);
    let short = &digest[..8];
    match (dimensions(&bytes), sniff_format(&bytes)) {
        (Some((width, height)), Some(format)) => {
            format!("{PREFIX}; sha256:{short}; {width}x{height} {format}]")
        }
        _ => format!("{PREFIX}; sha256:{short}]"),
    }
}

fn sha256(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// 图片格式短名（`png` / `jpeg` / `gif` / `webp`），来自魔数嗅探。
fn sniff_format(bytes: &[u8]) -> Option<&'static str> {
    crate::image_input::sniff_mime(bytes).map(|mime| mime.strip_prefix("image/").unwrap_or(mime))
}

/// 从图片字节里读 (宽, 高)；不认识的格式或截断的头部返回 `None`。
fn dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    match crate::image_input::sniff_mime(bytes) {
        Some("image/png") => png_dimensions(bytes),
        Some("image/jpeg") => jpeg_dimensions(bytes),
        Some("image/gif") => gif_dimensions(bytes),
        Some("image/webp") => webp_dimensions(bytes),
        _ => None,
    }
}

fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.get(12..16) != Some(&b"IHDR"[..]) {
        return None;
    }
    let width = be_u32(bytes.get(16..20)?)?;
    let height = be_u32(bytes.get(20..24)?)?;
    Some((width, height))
}

/// 扫到第一个 SOF 段（`FFC0`–`FFCF` 里排除 `C4`/`C8`/`CC`）读尺寸。
fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut index = 2; // 跳过 SOI
    while index + 3 < bytes.len() {
        if bytes[index] != 0xFF {
            return None;
        }
        let marker = bytes[index + 1];
        if matches!(marker, 0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF) {
            let height = be_u16(bytes.get(index + 5..index + 7)?)?;
            let width = be_u16(bytes.get(index + 7..index + 9)?)?;
            return Some((u32::from(width), u32::from(height)));
        }
        let length = be_u16(bytes.get(index + 2..index + 4)?)? as usize;
        if length < 2 {
            return None;
        }
        index += 2 + length;
    }
    None
}

fn gif_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let width = le_u16(bytes.get(6..8)?)?;
    let height = le_u16(bytes.get(8..10)?)?;
    Some((u32::from(width), u32::from(height)))
}

fn webp_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    match bytes.get(12..16)? {
        // VP8X: 4 字节 flags 之后是 24 位 LE 的 canvas 宽高减一。
        b"VP8X" => {
            let width = le_u24(bytes.get(24..27)?)? + 1;
            let height = le_u24(bytes.get(27..30)?)? + 1;
            Some((width, height))
        }
        // VP8（有损）：3 字节 frame tag + 起点码，然后 14 位宽、14 位高。
        b"VP8 " => {
            if bytes.get(23..26) != Some(&b"\x9d\x01\x2a"[..]) {
                return None;
            }
            let width = u32::from(le_u16(bytes.get(26..28)?)? & 0x3fff);
            let height = u32::from(le_u16(bytes.get(28..30)?)? & 0x3fff);
            Some((width, height))
        }
        // VP8L（无损）与未知 chunk：拿不到尺寸就走「只写 sha256」的占位符。
        _ => None,
    }
}

fn be_u16(bytes: &[u8]) -> Option<u16> {
    let array: [u8; 2] = bytes.try_into().ok()?;
    Some(u16::from_be_bytes(array))
}

fn be_u32(bytes: &[u8]) -> Option<u32> {
    let array: [u8; 4] = bytes.try_into().ok()?;
    Some(u32::from_be_bytes(array))
}

fn le_u16(bytes: &[u8]) -> Option<u16> {
    let array: [u8; 2] = bytes.try_into().ok()?;
    Some(u16::from_le_bytes(array))
}

fn le_u24(bytes: &[u8]) -> Option<u32> {
    let array: [u8; 3] = bytes.try_into().ok()?;
    Some(u32::from(array[0]) | (u32::from(array[1]) << 8) | (u32::from(array[2]) << 16))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_input::base64_encode;

    /// 一张真实形状的内联图片 part。
    fn image_part(bytes: &[u8]) -> Value {
        json!({
            "type": "image_url",
            "image_url": { "url": format!("data:image/png;base64,{}", base64_encode(bytes)) },
        })
    }

    /// 一条 user 消息：文本 part + 若干图片 part。
    fn user_message(text: &str, images: &[Value]) -> Value {
        let mut parts = vec![json!({ "type": "text", "text": text })];
        parts.extend(images.iter().cloned());
        json!({ "role": "user", "content": parts })
    }

    /// 640x480 的 PNG 头部（魔数 + IHDR 宽高）。
    fn png_header(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR".to_vec();
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes
    }

    fn jpeg_header(width: u16, height: u16) -> Vec<u8> {
        let mut bytes = vec![0xFF, 0xD8];
        // APP0，段长 4（含长度字段本身）
        bytes.extend_from_slice(&[0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00]);
        bytes.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x11, 0x08]);
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes
    }

    fn gif_header(width: u16, height: u16) -> Vec<u8> {
        let mut bytes = b"GIF89a".to_vec();
        bytes.extend_from_slice(&width.to_le_bytes());
        bytes.extend_from_slice(&height.to_le_bytes());
        bytes
    }

    fn webp_vp8x(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = b"RIFF\x00\x00\x00\x00WEBP".to_vec();
        bytes.extend_from_slice(b"VP8X");
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        bytes.extend_from_slice(&(width - 1).to_le_bytes()[..3]);
        bytes.extend_from_slice(&(height - 1).to_le_bytes()[..3]);
        bytes
    }

    fn webp_vp8(width: u16, height: u16) -> Vec<u8> {
        let mut bytes = b"RIFF\x00\x00\x00\x00WEBP".to_vec();
        bytes.extend_from_slice(b"VP8 ");
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&[0, 0, 0]);
        bytes.extend_from_slice(&[0x9d, 0x01, 0x2a]);
        bytes.extend_from_slice(&width.to_le_bytes());
        bytes.extend_from_slice(&height.to_le_bytes());
        bytes
    }

    #[test]
    fn inline_base64_len_uses_the_exact_three_byte_groups() {
        assert_eq!(inline_base64_len(0), 0);
        assert_eq!(inline_base64_len(1), 4);
        assert_eq!(inline_base64_len(2), 4);
        assert_eq!(inline_base64_len(3), 4);
        assert_eq!(inline_base64_len(4), 8);
        assert_eq!(inline_base64_len(3 * 1024 * 1024), 4 * 1024 * 1024);
    }

    #[test]
    fn inline_image_bytes_only_counts_base64_data_uris() {
        // 3 字节 → 4 个 base64 字符。
        assert_eq!(inline_image_bytes("data:image/png;base64,Zm9v"), Some(4));
        // 带填充：2 字节 → 4 个字符。
        assert_eq!(inline_image_bytes("data:image/png;base64,Zm8="), Some(4));
        // 远程 URL 不占内联字节。
        assert_eq!(inline_image_bytes("https://example.com/a.png"), None);
        // 没有 `;base64` 标记。
        assert_eq!(inline_image_bytes("data:text/plain,hello"), None);
        // 没有逗号。
        assert_eq!(inline_image_bytes("data:image/png;base64"), None);
        // 长度不是 4 的倍数。
        assert_eq!(inline_image_bytes("data:image/png;base64,Zm9"), None);
        // 非法字符。
        assert_eq!(inline_image_bytes("data:image/png;base64,Zm9!"), None);
        // 空 payload。
        assert_eq!(inline_image_bytes("data:image/png;base64,"), None);
        // 填充出现在中间。
        assert_eq!(inline_image_bytes("data:image/png;base64,AA==AAAA"), None);
        // 填充超过 2 个。
        assert_eq!(inline_image_bytes("data:image/png;base64,===="), None);
    }

    #[test]
    fn image_parts_bytes_sums_inline_images_only() {
        let png = png_header(2, 2);
        let parts = vec![
            json!({ "type": "text", "text": "hi" }),
            image_part(&png),
            // 远程图片：不计入。
            json!({ "type": "image_url", "image_url": { "url": "https://x/y.png" } }),
            // 缺 url / url 不是字符串 / 缺 image_url：一律跳过。
            json!({ "type": "image_url" }),
            json!({ "type": "image_url", "image_url": { "url": 7 } }),
            json!({ "type": "image_url", "image_url": {} }),
        ];
        assert_eq!(
            image_parts_bytes(&parts),
            inline_base64_len(png.len()),
            "only the inline png counts"
        );
        assert_eq!(image_parts_bytes(&[]), 0);
    }

    #[test]
    fn plan_keeps_everything_when_the_budget_is_roomy() {
        let image = image_part(&png_header(2, 2));
        let messages = vec![user_message("看图", std::slice::from_ref(&image))];
        let plan = plan_image_omissions(&messages, INLINE_IMAGE_BUDGET_BYTES);
        assert!(plan.is_empty());
        assert_eq!(plan.omitted(), 0);
        assert_eq!(plan.bytes_before, plan.bytes_after);
        assert_eq!(plan.bytes_before, image_parts_bytes(&[image]));
    }

    #[test]
    fn plan_strips_the_oldest_images_first() {
        // 三条消息各 3 字节图片 = 每条 4 个 base64 字符，总 12。
        let messages = vec![
            user_message("最早", &[image_part(b"aaa")]),
            user_message("中间", &[image_part(b"bbb")]),
            user_message("最新", &[image_part(b"ccc")]),
        ];
        let plan = plan_image_omissions(&messages, 8);
        assert_eq!(plan.bytes_before, 12);
        assert_eq!(plan.bytes_after, 8);
        assert_eq!(plan.omitted(), 1, "只剥掉最老的一张就够");
        assert_eq!(plan.omissions[0].message_index, 0);
        assert_eq!(plan.omissions[0].part_index, 1, "文本 part 占了下标 0");
        assert!(plan.omissions[0].placeholder.contains("sha256:"));
    }

    #[test]
    fn plan_strips_everything_when_the_budget_is_zero() {
        let messages = vec![
            user_message("一", &[image_part(b"aaa")]),
            user_message("二", &[image_part(b"bbb")]),
        ];
        let plan = plan_image_omissions(&messages, 0);
        assert_eq!(plan.bytes_after, 0);
        assert_eq!(plan.omitted(), 2);
        assert_eq!(plan.omissions[0].message_index, 0);
        assert_eq!(plan.omissions[1].message_index, 1);
    }

    #[test]
    fn plan_ignores_messages_and_parts_without_inline_images() {
        let messages = vec![
            json!("not a message"),
            json!({ "role": "user", "content": "纯文本" }),
            json!({ "role": "assistant", "content": 42 }),
            json!({
                "role": "user",
                "content": [
                    { "type": "text", "text": "hello" },
                    { "type": "image_url", "image_url": { "url": "https://x/y.png" } },
                ],
            }),
        ];
        let plan = plan_image_omissions(&messages, 0);
        assert!(plan.is_empty());
        assert_eq!(plan.bytes_before, 0);
        assert_eq!(plan.omitted(), 0);
        assert_eq!(plan.bytes_after, 0);
    }

    #[test]
    fn apply_replaces_image_parts_with_text_placeholders() {
        let mut messages = vec![user_message("看图", &[image_part(b"aaa")])];
        let plan = plan_image_omissions(&messages, 0);
        assert_eq!(apply_image_omissions(&mut messages, &plan), 1);
        let content = messages[0]["content"].as_array().expect("array content");
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], plan.omissions[0].placeholder);
        // 幂等：再来一次没有可替换的东西。
        assert_eq!(apply_image_omissions(&mut messages, &plan), 0);
        // 真正的图已经没了，重新规划自然是空的。
        assert!(plan_image_omissions(&messages, 0).is_empty());
    }

    #[test]
    fn apply_skips_plans_that_no_longer_match_the_messages() {
        let image = image_part(b"aaa");
        let mut messages = vec![user_message("看图", std::slice::from_ref(&image))];
        let plan = ImageBudgetPlan {
            bytes_before: 4,
            bytes_after: 0,
            omissions: vec![
                // 消息下标越界。
                ImageOmission {
                    message_index: 9,
                    part_index: 1,
                    placeholder: "x".to_string(),
                },
                // part 下标越界。
                ImageOmission {
                    message_index: 0,
                    part_index: 9,
                    placeholder: "x".to_string(),
                },
                // 消息 content 不是数组。
                ImageOmission {
                    message_index: 1,
                    part_index: 0,
                    placeholder: "x".to_string(),
                },
            ],
        };
        messages.push(json!({ "role": "user", "content": "纯文本" }));
        assert_eq!(apply_image_omissions(&mut messages, &plan), 0);
        assert_eq!(
            messages[0]["content"].as_array().expect("array")[1]["type"],
            "image_url"
        );
    }

    #[test]
    fn enforce_budget_is_idempotent() {
        let mut messages = vec![
            user_message("一", &[image_part(b"aaa")]),
            user_message("二", &[image_part(b"bbb")]),
        ];
        let first = enforce_image_budget(&mut messages, 0);
        assert_eq!(first.omitted(), 2);
        assert_eq!(first.bytes_after, 0);
        let second = enforce_image_budget(&mut messages, 0);
        assert!(second.is_empty());
        assert_eq!(second.bytes_before, 0);
    }

    /// 快照落盘前的入口：所有内联图变成占位符，远程 URL 原样保留，再来一次
    /// 没有可剥的东西（幂等）。
    #[test]
    fn strip_inline_images_replaces_every_inline_picture() {
        let mut messages = vec![
            user_message("看图", &[image_part(b"aaa"), image_part(&png_header(2, 2))]),
            json!({
                "role": "user",
                "content": [
                    { "type": "text", "text": "远程图" },
                    { "type": "image_url", "image_url": { "url": "https://x/y.png" } },
                ],
            }),
        ];
        assert_eq!(strip_inline_images(&mut messages), 2);
        let first = messages[0]["content"].to_string();
        assert_eq!(
            first
                .matches("image omitted to fit request byte budget")
                .count(),
            2
        );
        assert!(!first.contains("base64"));
        assert!(messages[1]["content"]
            .to_string()
            .contains("https://x/y.png"));
        // 已经没有内联图了，再剥一次是 no-op。
        assert_eq!(strip_inline_images(&mut messages), 0);
        assert!(plan_image_omissions(&messages, 0).is_empty());
    }

    #[test]
    fn placeholder_carries_sha256_and_dimensions_when_it_can() {
        let bytes = png_header(640, 480);
        let uri = format!("data:image/png;base64,{}", base64_encode(&bytes));
        let placeholder = placeholder_for(&uri);
        assert_eq!(
            placeholder,
            format!(
                "[image omitted to fit request byte budget; sha256:{}; 640x480 png]",
                &sha256(&bytes)[..8]
            )
        );
    }

    #[test]
    fn placeholder_drops_dimensions_when_the_format_is_unknown() {
        let uri = format!("data:image/png;base64,{}", base64_encode(b"not an image"));
        let placeholder = placeholder_for(&uri);
        assert!(placeholder.contains("sha256:"));
        assert!(!placeholder.contains('x'), "{placeholder}");
        assert!(placeholder.ends_with(']'));
    }

    #[test]
    fn placeholder_without_decodable_bytes_says_only_omitted() {
        assert_eq!(
            placeholder_for("https://example.com/a.png"),
            "[image omitted to fit request byte budget]"
        );
        assert_eq!(
            placeholder_for("data:image/png;base64,!!!"),
            "[image omitted to fit request byte budget]"
        );
    }

    #[test]
    fn sha256_matches_known_vectors() {
        // 64 位十六进制；前 8 位与 FIPS 180-4 的向量一致。
        assert_eq!(&sha256(b"")[..8], "e3b0c442");
        assert_eq!(&sha256(b"abc")[..8], "ba7816bf");
        assert_eq!(sha256(b"abc").len(), 64);
    }

    #[test]
    fn base64_decoding_handles_every_padding_length() {
        assert_eq!(base64_decode("Zm9v"), Some(b"foo".to_vec()));
        assert_eq!(base64_decode("Zm8="), Some(b"fo".to_vec()));
        assert_eq!(base64_decode("Zg=="), Some(b"f".to_vec()));
        assert_eq!(base64_decode("/w=="), Some(vec![0xFF]));
        // '+' 与 '/' 两个非字母数字字符也要走到。
        assert_eq!(base64_decode("+w=="), Some(vec![0xFB]));
        assert_eq!(base64_decode("//8="), Some(vec![0xFF, 0xFF]));
        // 非规范输入一律拒绝：长度、字符、填充位置。
        assert_eq!(base64_decode(""), None);
        assert_eq!(base64_decode("Zm9"), None);
        assert_eq!(base64_decode("Z!9v"), None);
        assert_eq!(base64_decode("=AAA"), None);
    }

    #[test]
    fn png_dimensions_read_the_ihdr() {
        assert_eq!(png_dimensions(&png_header(640, 480)), Some((640, 480)));
        // 魔数对但 chunk 不是 IHDR。
        let mut wrong = png_header(640, 480);
        wrong[12..16].copy_from_slice(b"IDAT");
        assert_eq!(png_dimensions(&wrong), None);
        // 头部被截断。
        assert_eq!(png_dimensions(&png_header(640, 480)[..20]), None);
        assert_eq!(png_dimensions(&png_header(640, 480)[..18]), None);
    }

    #[test]
    fn jpeg_dimensions_walk_to_the_sof_segment() {
        assert_eq!(jpeg_dimensions(&jpeg_header(640, 480)), Some((640, 480)));
        // 段首不是 0xFF。
        let mut bad_marker = jpeg_header(640, 480);
        bad_marker[2] = 0x00;
        assert_eq!(jpeg_dimensions(&bad_marker), None);
        // 段长为 0 的畸形成员。
        let mut bad_length = jpeg_header(640, 480);
        bad_length[4] = 0x00;
        bad_length[5] = 0x00;
        assert_eq!(jpeg_dimensions(&bad_length), None);
        // 只有 APP0，永远走不到 SOF。
        assert_eq!(
            jpeg_dimensions(&[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0, 0]),
            None
        );
        // SOF 段被截断在尺寸之前。
        let sof = jpeg_header(640, 480);
        assert_eq!(jpeg_dimensions(&sof[..14]), None);
    }

    #[test]
    fn gif_dimensions_are_little_endian() {
        assert_eq!(gif_dimensions(&gif_header(320, 240)), Some((320, 240)));
        assert_eq!(gif_dimensions(&gif_header(320, 240)[..8]), None);
        assert_eq!(gif_dimensions(&gif_header(320, 240)[..6]), None);
    }

    #[test]
    fn webp_dimensions_cover_vp8x_and_lossy_vp8() {
        assert_eq!(webp_dimensions(&webp_vp8x(640, 480)), Some((640, 480)));
        assert_eq!(webp_dimensions(&webp_vp8(640, 480)), Some((640, 480)));
        // VP8 起点码不对。
        let mut bad_start = webp_vp8(640, 480);
        bad_start[23] = 0x00;
        assert_eq!(webp_dimensions(&bad_start), None);
        // VP8L 与未知 chunk：拿不到尺寸。
        let mut vp8l = b"RIFF\x00\x00\x00\x00WEBP".to_vec();
        vp8l.extend_from_slice(b"VP8L");
        assert_eq!(webp_dimensions(&vp8l), None);
        let mut unknown = b"RIFF\x00\x00\x00\x00WEBP".to_vec();
        unknown.extend_from_slice(b"XXXX");
        assert_eq!(webp_dimensions(&unknown), None);
    }

    #[test]
    fn dimensions_dispatches_on_the_sniffed_format() {
        assert_eq!(dimensions(&png_header(640, 480)), Some((640, 480)));
        assert_eq!(dimensions(&jpeg_header(640, 480)), Some((640, 480)));
        assert_eq!(dimensions(&gif_header(320, 240)), Some((320, 240)));
        assert_eq!(dimensions(&webp_vp8x(640, 480)), Some((640, 480)));
        // 不是图片。
        assert_eq!(dimensions(b"not an image at all"), None);
    }

    #[test]
    fn sniff_format_strips_the_mime_prefix() {
        assert_eq!(sniff_format(&png_header(2, 2)), Some("png"));
        assert_eq!(sniff_format(b"not an image"), None);
    }

    #[test]
    fn integer_readers_reject_truncated_slices() {
        assert_eq!(be_u16(&[0x01]), None);
        assert_eq!(be_u32(&[0x01, 0x02, 0x03]), None);
        assert_eq!(le_u16(&[0x01]), None);
        assert_eq!(le_u24(&[0x01, 0x02]), None);
    }
}
