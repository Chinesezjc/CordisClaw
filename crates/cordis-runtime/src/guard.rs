//! P5: deterministic crisis guard.
//!
//! [`crisis_reply`] short-circuits the LLM for a small set of high-precision
//! crisis phrases and answers with real helpline information. It is the only
//! mechanical guard left in this module.
//!
//! Repetition / flood handling is deliberately **prompt-level** (conduct rule
//! 11): a code-level repeat brake was removed after a turn cap misfired on a
//! normal multi-turn weather conversation in production, and tuning "same
//! message repeated" in code cannot see intent either. The model now judges
//! repetition with its own context; the tradeoff is that it is softer and not
//! unit-testable, and silent code drops hid legitimate follow-ups.

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
}
