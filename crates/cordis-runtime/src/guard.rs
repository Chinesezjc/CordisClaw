//! P5: deterministic chat guards (no semantics, no LLM).
//!
//! Two guards cover the mechanical half of chat incidents without pretending to
//! understand intent:
//!
//! - [`crisis_reply`] short-circuits the LLM for a small set of high-precision
//!   crisis phrases and answers with real helpline information;
//! - [`FloodGuard`] brakes repeated identical messages from one speaker
//!   (flood/spam), *not* ordinary multi-turn conversation.
//!
//! An earlier consecutive-turn budget ("one speaker may use at most N turns")
//! was removed after it refused a normal "Hangzhou / Beijing / Wuhan weather"
//! sequence in production: a turn cap cannot tell a curious user from a troll,
//! and the conduct contract already covers refusal stability and provocation.
//! Only exact repeats inside a short window trip this guard.
//!
//! Anything that needs to *understand* intent (repeated out-of-bounds asks,
//! sarcasm, grooming) is deliberately out of scope: a keyword guess for that is
//! worse than no guard, because it both misses the real case and misfires on
//! ordinary chat.

/// Fixed, non-LLM reply for a crisis phrase. Short, non-diagnostic, and with
/// real mainland-China helplines (12356 psychological-aid line plus emergency
/// numbers).
pub const CRISIS_REPLY: &str = "先停一下：如果你现在有伤害自己的念头，请立刻联系身边可信任的人，或拨打心理援助热线 12356、希望 24 热线 400-161-9995；有即时危险请拨 110/120。你不需要一个人扛。";

/// One-line reply when the same message has been repeated too often.
pub const FLOOD_WARNING: &str =
    "同样的话你连着发了好几遍，我先不重复回了。换个说法，或者等会儿再说。";

/// High-precision crisis phrases. Ambiguous fragments (`想死` matches
/// `想死你了`, `笑死`) are deliberately absent — a false positive here would
/// turn ordinary banter into a helpline pitch.
const CRISIS_PHRASES: &[&str] = &[
    "自杀",
    "自残",
    "轻生",
    "跳楼",
    "不想活了",
    "活不下去",
    "活着没意思",
    "结束生命",
];

/// Return the fixed crisis reply when `text` contains a high-precision phrase.
pub fn crisis_reply(text: &str) -> Option<&'static str> {
    CRISIS_PHRASES
        .iter()
        .any(|phrase| text.contains(phrase))
        .then_some(CRISIS_REPLY)
}

/// Collapse a user message to its letters/digits so trivial variations
/// ("杭州天气！" / "杭州 天气" / "HANGZHOU") count as the same request.
pub fn normalize_request(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// How long a repeat stays counted.
pub const FLOOD_WINDOW_MS: u64 = 300_000;

/// Same normalized message seen this many times inside the window trips it.
pub const FLOOD_REPEAT_THRESHOLD: u32 = 3;

/// Messages shorter than this (after normalization) are never treated as
/// repeats: "北京呢" / "?" carry too little signal and must stay answerable.
pub const FLOOD_MIN_LEN: usize = 4;

/// What the inbox should do with the current turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FloodVerdict {
    /// Forward to the agent as usual.
    Allow,
    /// Do not call the LLM; send [`FLOOD_WARNING`] once instead.
    Warn,
    /// Stay silent for this turn.
    Drop,
}

/// Per-(room, speaker) tracker for repeated identical messages.
#[derive(Debug, Default)]
pub struct FloodGuard {
    recent: std::collections::HashMap<String, std::collections::VecDeque<(String, u64)>>,
    warned: std::collections::HashSet<String>,
}

impl FloodGuard {
    /// Classify the incoming turn. `key` is the channel identity of the
    /// conversation + speaker; `now_ms` is a wall-clock timestamp so the window
    /// stays testable.
    pub fn begin(&mut self, key: &str, text: &str, now_ms: u64) -> FloodVerdict {
        let normalized = normalize_request(text);
        if normalized.chars().count() < FLOOD_MIN_LEN {
            return FloodVerdict::Allow;
        }
        let queue = self.recent.entry(key.to_string()).or_default();
        while let Some((_, at)) = queue.front() {
            if now_ms.saturating_sub(*at) > FLOOD_WINDOW_MS {
                queue.pop_front();
            } else {
                break;
            }
        }
        let repeats = queue.iter().filter(|(seen, _)| *seen == normalized).count();
        queue.push_back((normalized.clone(), now_ms));
        while queue.len() > 32 {
            queue.pop_front();
        }
        if (repeats as u32) + 1 < FLOOD_REPEAT_THRESHOLD {
            return FloodVerdict::Allow;
        }
        let tag = format!("{key}|{normalized}");
        if self.warned.insert(tag) {
            FloodVerdict::Warn
        } else {
            FloodVerdict::Drop
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crisis_phrases_are_high_precision() {
        for text in [
            "我想自杀",
            "最近在自残",
            "有过轻生的念头",
            "真的不想活了",
            "感觉活不下去",
            "活着没意思",
            "想结束生命",
        ] {
            assert_eq!(crisis_reply(text), Some(CRISIS_REPLY), "missed: {text}");
        }
        // Ambiguous banter must not trigger a helpline pitch.
        for text in ["笑死我了", "我想死你了", "活着真好", "打游戏被虐死"] {
            assert_eq!(crisis_reply(text), None, "false positive: {text}");
        }
    }

    #[test]
    fn normalize_collapses_punctuation_and_case() {
        assert_eq!(normalize_request("杭州 天气！"), "杭州天气");
        assert_eq!(normalize_request("MPFT??"), "mpft");
        assert_eq!(normalize_request("？?"), "");
    }

    #[test]
    fn flood_trips_on_third_identical_message() {
        let mut guard = FloodGuard::default();
        assert_eq!(guard.begin("g|u", "杭州天气", 1_000), FloodVerdict::Allow);
        assert_eq!(
            guard.begin("g|u", "杭州 天气！", 2_000),
            FloodVerdict::Allow
        );
        assert_eq!(guard.begin("g|u", "杭州天气", 3_000), FloodVerdict::Warn);
        // Further identical sends stay silent until the message changes.
        assert_eq!(guard.begin("g|u", "杭州天气", 4_000), FloodVerdict::Drop);
        assert_eq!(guard.begin("g|u", "北京天气", 5_000), FloodVerdict::Allow);
    }

    #[test]
    fn flood_ignores_short_and_distinct_messages() {
        let mut guard = FloodGuard::default();
        // A normal three-question sequence must all be allowed.
        for (i, text) in ["杭州的", "北京呢", "武汉呢"].iter().enumerate() {
            assert_eq!(
                guard.begin("g|u", text, 1_000 + i as u64 * 1_000),
                FloodVerdict::Allow
            );
        }
        // Short messages never trip the guard no matter how often they repeat.
        for i in 0..5 {
            assert_eq!(guard.begin("g|u", "？", 10_000 + i), FloodVerdict::Allow);
        }
    }

    #[test]
    fn flood_window_expires_and_keys_are_isolated() {
        let mut guard = FloodGuard::default();
        assert_eq!(guard.begin("g|u", "在吗在吗", 0), FloodVerdict::Allow);
        assert_eq!(guard.begin("g|u", "在吗在吗", 1_000), FloodVerdict::Allow);
        // Outside the window the old repeats no longer count.
        let later = 1_000 + FLOOD_WINDOW_MS + 1;
        assert_eq!(guard.begin("g|u", "在吗在吗", later), FloodVerdict::Allow);
        // Another speaker in the same room is unaffected.
        assert_eq!(guard.begin("g|v", "在吗在吗", later), FloodVerdict::Allow);
        assert_eq!(
            guard.begin("g|v", "在吗在吗", later + 1),
            FloodVerdict::Allow
        );
        assert_eq!(
            guard.begin("g|v", "在吗在吗", later + 2),
            FloodVerdict::Warn
        );
    }
}
