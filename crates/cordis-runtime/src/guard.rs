//! P5: deterministic chat guards (no semantics, no LLM).
//!
//! The 2026-10-23 group incident had a mechanical half that prompt wording
//! cannot enforce: one member kept re-asking the same out-of-bounds thing and
//! the bot kept answering, which is exactly the reward loop a troll wants.
//! Two guards close that mechanical half:
//!
//! - [`crisis_reply`] short-circuits the LLM for a small set of high-precision
//!   crisis phrases and answers with real helpline information;
//! - [`ReplyBudget`] caps consecutive turns addressed to the same speaker in a
//!   room, so a single member cannot farm an escalation ladder out of the bot.
//!
//! Anything that needs to *understand* intent (repeated out-of-bounds asks,
//! sarcasm, grooming) is deliberately out of scope: a keyword guess for that is
//! worse than no guard, because it both misses the real case and misfires on
//! ordinary chat. The prompt-level contract plus these two deterministic guards
//! are what this layer can honestly promise.

/// Fixed, non-LLM reply for a crisis phrase. Short, non-diagnostic, and with
/// real mainland-China helplines (12356 psychological-aid line plus emergency
/// numbers).
pub const CRISIS_REPLY: &str = "先停一下：如果你现在有伤害自己的念头，请立刻联系身边可信任的人，或拨打心理援助热线 12356、希望 24 热线 400-161-9995；有即时危险请拨 110/120。你不需要一个人扛。";

/// One-line reply when a speaker has used up their turn budget.
pub const BUDGET_WARNING: &str = "这个话头我先不接了，换个话题吧。";

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

/// Default consecutive-turn cap per (room, speaker). Four normal turns leaves
/// room for a real back-and-forth; the fifth gets the fixed warning and the
/// sixth+ are dropped until someone else speaks.
pub const DEFAULT_TURN_CAP: u32 = 4;

/// What the inbox should do with the current turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnVerdict {
    /// Forward to the agent as usual.
    Allow,
    /// Do not call the LLM; send [`BUDGET_WARNING`] once instead.
    Warn,
    /// Stay silent for this turn.
    Drop,
}

/// Per-room consecutive-turn budget for one speaker at a time.
///
/// The budget resets as soon as a different speaker addresses the bot, so a
/// normal group conversation never notices it; only one member monopolising the
/// bot hits it.
#[derive(Debug)]
pub struct ReplyBudget {
    cap: u32,
    last_sender: Option<String>,
    turns: u32,
    warned: bool,
}

impl ReplyBudget {
    pub fn new(cap: u32) -> Self {
        Self {
            cap,
            last_sender: None,
            turns: 0,
            warned: false,
        }
    }

    /// Classify the incoming turn. `sender` is the stable per-channel identity
    /// (e.g. `qq:123456`), not the display name.
    pub fn begin_turn(&mut self, sender: &str) -> TurnVerdict {
        if self.last_sender.as_deref() != Some(sender) {
            self.last_sender = Some(sender.to_string());
            self.turns = 0;
            self.warned = false;
            return TurnVerdict::Allow;
        }
        if self.turns < self.cap {
            return TurnVerdict::Allow;
        }
        if self.warned {
            TurnVerdict::Drop
        } else {
            self.warned = true;
            TurnVerdict::Warn
        }
    }

    /// Count a turn that actually reached the agent (or sent the warning).
    pub fn record_turn(&mut self) {
        self.turns += 1;
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
    fn budget_allows_cap_turns_then_warns_then_drops() {
        let mut budget = ReplyBudget::new(3);
        for turn in 1..=3 {
            assert_eq!(budget.begin_turn("qq:1"), TurnVerdict::Allow, "turn {turn}");
            budget.record_turn();
        }
        // Fourth turn: over the cap, warning sent once and counted.
        assert_eq!(budget.begin_turn("qq:1"), TurnVerdict::Warn);
        budget.record_turn();
        // Fifth+ turns to the same speaker are dropped.
        assert_eq!(budget.begin_turn("qq:1"), TurnVerdict::Drop);
        assert_eq!(budget.begin_turn("qq:1"), TurnVerdict::Drop);
    }

    #[test]
    fn budget_resets_when_someone_else_speaks() {
        let mut budget = ReplyBudget::new(1);
        assert_eq!(budget.begin_turn("qq:1"), TurnVerdict::Allow);
        budget.record_turn();
        assert_eq!(budget.begin_turn("qq:1"), TurnVerdict::Warn);
        budget.record_turn();
        assert_eq!(budget.begin_turn("qq:1"), TurnVerdict::Drop);
        // A different speaker resets the ladder, and the old speaker starts
        // fresh the next time they speak.
        assert_eq!(budget.begin_turn("qq:2"), TurnVerdict::Allow);
        assert_eq!(budget.begin_turn("qq:1"), TurnVerdict::Allow);
    }
}
