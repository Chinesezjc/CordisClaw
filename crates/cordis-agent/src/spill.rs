//! Keep a big tool result out of the context without losing it.
//!
//! A `read_file` on a large source file is megabytes; replaying that on every
//! later request would eat the window and trip the provider's body limit. The
//! first attempt at this truncated the text and dropped the rest — the worst of
//! both worlds: the tokens were still spent, and the answer was gone.
//!
//! So the result is written to disk and the context keeps its head and tail
//! plus a pointer, the way DSH's spill policy does it. Anything the model still
//! needs can be read back with `read_file` (offset/limit) or searched.

use std::path::Path;

/// Estimated-token budget for one tool result kept inline.
///
/// Text and the notice share it. 12_500 is the number DSH uses for the same
/// job: large enough for a real answer, small enough that one tool result can
/// never dominate the window.
pub const MAX_INLINE_TOKENS: usize = 12_500;

/// Conservative token estimate: one token per two characters.
///
/// CJK text is close to one token per character, so this under-counts there and
/// over-counts for Latin text — erring toward spilling earlier rather than
/// later, because the failure being prevented is a request that cannot be sent.
pub fn estimate_tokens(text: &str) -> usize {
    text.chars().count() / 2
}

/// What a spilled result looks like once written back into history.
#[derive(Debug, Clone, PartialEq)]
pub struct Spilled {
    /// Head, notice and tail, ready to store.
    pub rendered: String,
    /// Where the full text went.
    pub path: String,
    /// Bytes that did not fit inline.
    pub omitted_bytes: usize,
}

/// Bound `text` to [`MAX_INLINE_TOKENS`], writing the full copy under `dir`.
///
/// Returns `None` when the text already fits, when writing fails (inline
/// content is never thrown away just because the disk refused it), or when the
/// head and tail would be so small that the preview says nothing.
pub fn spill(text: &str, dir: &Path, name: &str) -> Option<Spilled> {
    if estimate_tokens(text) <= MAX_INLINE_TOKENS {
        return None;
    }
    let full = dir.join(name);
    if std::fs::create_dir_all(dir).is_err() {
        return None;
    }
    if std::fs::write(&full, text).is_err() {
        return None;
    }
    let path = full.display().to_string();
    let rendered = bound(text, &path)?;
    Some(Spilled {
        rendered,
        path,
        omitted_bytes: text.len(),
    })
}

/// Whether this text is too big to keep inline.
pub fn needs_spill(text: &str) -> bool {
    estimate_tokens(text) > MAX_INLINE_TOKENS
}

/// Head, notice and tail for a result whose full copy lives at `path`.
///
/// `None` when the notice would leave too little room to preview anything —
/// the caller then keeps the text inline rather than storing a pointer to a
/// file with no readable excerpt around it.
pub fn bound(text: &str, path: &str) -> Option<String> {
    let omitted_bytes = text.len();
    let notice = format!(
        "…（已省略 {omitted_bytes} 字节。完整结果：{path}，用 read_file 带 offset/limit 分段读，或搜索该文件）…"
    );
    let budget = MAX_INLINE_TOKENS.saturating_sub(estimate_tokens(&notice));
    if budget < 64 {
        return None;
    }
    let (head, tail) = split_head_tail(text, budget / 2);
    Some(format!("{head}\n\n{notice}\n\n{tail}"))
}

/// First and last `each` tokens' worth of characters.
fn split_head_tail(text: &str, each: usize) -> (String, String) {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= each * 2 {
        return (text.to_string(), String::new());
    }
    let head: String = chars[..each].iter().collect();
    let tail: String = chars[chars.len() - each..].iter().collect();
    (head, tail)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn big(scale: usize) -> String {
        "结果行\n".repeat(scale)
    }

    #[test]
    fn a_small_result_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        assert!(spill("短结果", dir.path(), "a.txt").is_none());
    }

    #[test]
    fn a_large_result_keeps_its_head_and_tail_and_points_at_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let text = big(20_000);
        let spilled = spill(&text, dir.path(), "big.txt").expect("must spill");
        assert!(spilled.rendered.contains("完整结果"));
        assert!(spilled.rendered.contains("big.txt"));
        assert!(spilled.rendered.contains("已省略"));
        // Both ends survive.
        assert!(spilled.rendered.starts_with("结果行"));
        assert!(spilled.rendered.trim_end().ends_with("结果行"));
        // The full text is recoverable.
        let written = std::fs::read_to_string(&spilled.path).unwrap();
        assert_eq!(written, text);
        assert!(estimate_tokens(&spilled.rendered) <= MAX_INLINE_TOKENS);
    }

    #[test]
    fn an_unwritable_directory_keeps_the_content_inline() {
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("file-not-a-dir");
        std::fs::write(&blocked, "x").unwrap();
        assert!(spill(&big(20_000), &blocked, "x.txt").is_none());
    }

    #[test]
    fn head_and_tail_split_the_ends() {
        let (head, tail) = split_head_tail("abcdefghij", 3);
        assert_eq!(head, "abc");
        assert_eq!(tail, "hij");
    }

    #[test]
    fn a_result_shorter_than_the_split_is_returned_whole() {
        let (head, tail) = split_head_tail("abc", 5);
        assert_eq!(head, "abc");
        assert!(tail.is_empty());
    }

    #[test]
    fn token_estimate_is_two_characters_per_token() {
        assert_eq!(estimate_tokens("abcd"), 2);
        assert_eq!(estimate_tokens(""), 0);
    }
}
