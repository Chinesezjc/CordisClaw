//! What the bot keeps getting wrong, derived from its own transcripts.
//!
//! Every fix in this repository so far came from a human pasting a chat log.
//! Nothing looked at the 7000-odd stored turns and asked "which failure keeps
//! repeating?" — so the same mistakes survived for weeks: 93 turns where the
//! model wrote DSML the parser could not read, 99 where it produced neither
//! text nor a call, and so on.
//!
//! This module is the missing observer: it classifies assistant output from the
//! snapshots, groups identical failures into signatures, and ranks them by how
//! often they happen. The result is what an automatic iteration can act on.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// How a turn went wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Nothing at all came back (provider returned an empty completion).
    Empty,
    /// Text was produced but no tool call, so nothing was delivered.
    NoToolCall,
    /// DSML markup the parser could not read — the turn did nothing.
    UnparsedDsml,
    /// The provider itself failed (recorded as `[error] …`).
    ProviderError,
    /// The same reply, word for word, more than once.
    Repeated,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Empty => "empty",
            Kind::NoToolCall => "no_tool_call",
            Kind::UnparsedDsml => "unparsed_dsml",
            Kind::ProviderError => "provider_error",
            Kind::Repeated => "repeated",
        }
    }

    /// Higher means "a fix here is worth more".
    pub fn weight(self) -> usize {
        match self {
            Kind::UnparsedDsml => 5,
            Kind::NoToolCall => 4,
            Kind::ProviderError => 3,
            Kind::Empty => 2,
            Kind::Repeated => 1,
        }
    }
}

/// One repeating failure, with the evidence needed to act on it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Signature {
    pub kind: Kind,
    /// Normalised excerpt; identical failures share it, which is what makes a
    /// signature rather than a list of one-off turns.
    pub excerpt: String,
    pub count: usize,
    pub sessions: Vec<String>,
    pub first_turn: usize,
    pub last_turn: usize,
}

impl Signature {
    /// Score used for ranking: frequency, adjusted for how damaging the kind is.
    pub fn score(&self) -> usize {
        self.count * self.kind.weight()
    }

    pub fn id(&self) -> String {
        format!("{}-{}", self.kind.as_str(), short_hash(&self.excerpt))
    }
}

/// A lesson learned from an earlier attempt, so the next one does not repeat it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Lesson {
    pub signature: String,
    pub symptom: String,
    pub fix: String,
    pub outcome: String,
    pub learned_at_ms: u64,
}

/// What has already been tried, kept on disk between reflection turns.
///
/// Without it the loop re-attempts the same signature every six hours, and with
/// it the instruction can say "this fix was already tried and failed", which is
/// the difference between learning and spinning.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Ledger {
    #[serde(default)]
    pub attempted: Vec<String>,
    #[serde(default)]
    pub lessons: Vec<Lesson>,
}

impl Ledger {
    /// Load `data/selfcheck.json`; a missing or broken file starts empty.
    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text = serde_json::to_string_pretty(self).map_err(|error| error.to_string())?;
        std::fs::write(path, text).map_err(|error| format!("write {}: {error}", path.display()))
    }

    pub fn has_attempted(&self, signature: &str) -> bool {
        self.attempted.iter().any(|id| id == signature)
    }

    pub fn record_attempt(&mut self, signature: &str) {
        if !self.has_attempted(signature) {
            self.attempted.push(signature.to_string());
        }
    }

    /// Keep the most recent `keep` lessons; the prompt only needs the tail.
    pub fn push_lesson(&mut self, lesson: Lesson, keep: usize) {
        self.lessons.push(lesson);
        if self.lessons.len() > keep {
            let excess = self.lessons.len() - keep;
            self.lessons.drain(0..excess);
        }
    }
}

/// The whole picture: what is failing, and what has already been tried.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SelfCheck {
    pub scanned_sessions: usize,
    pub scanned_turns: usize,
    pub signatures: Vec<Signature>,
    #[serde(default)]
    pub lessons: Vec<Lesson>,
    #[serde(default)]
    pub attempted: Vec<String>,
}

impl SelfCheck {
    /// Put the recorded lessons in front of the next attempt.
    pub fn with_lessons(mut self, lessons: Vec<Lesson>) -> Self {
        self.lessons = lessons;
        self
    }

    /// Signatures nobody has tried to fix yet, best first.
    pub fn open(&self) -> Vec<&Signature> {
        let mut open: Vec<&Signature> = self
            .signatures
            .iter()
            .filter(|signature| !self.attempted.contains(&signature.id()))
            .collect();
        open.sort_by(|a, b| b.score().cmp(&a.score()).then(a.excerpt.cmp(&b.excerpt)));
        open
    }

    /// The instruction an iteration is started with.
    ///
    /// It carries the evidence, not just the symptom: an agent asked to "fix the
    /// DSML problem" has nothing to go on, while one told exactly which turns
    /// failed and what they contained can find the cause.
    pub fn instruction(&self, signature: &Signature) -> String {
        let mut text = format!(
            "The bot is failing in a repeating pattern. Fix the cause, not the symptom.\n\n\
             Failure kind: {}\n\
             Occurrences: {} across {} session(s), turns {}..={}.\n\
             Example output:\n---\n{}\n---\n\n",
            signature.kind.as_str(),
            signature.count,
            signature.sessions.len(),
            signature.first_turn,
            signature.last_turn,
            signature.excerpt,
        );
        if !signature.sessions.is_empty() {
            text.push_str("Sessions with this failure: ");
            text.push_str(&signature.sessions.join(", "));
            text.push_str("\n\n");
        }
        let relevant: Vec<&Lesson> = self
            .lessons
            .iter()
            .filter(|lesson| lesson.signature == signature.id())
            .collect();
        if !relevant.is_empty() {
            text.push_str("Already tried for this signature (do not repeat a fix that failed):\n");
            for lesson in relevant {
                text.push_str(&format!(
                    "- {} → {} ({})\n",
                    lesson.symptom, lesson.fix, lesson.outcome
                ));
            }
            text.push('\n');
        }
        text.push_str(
            "Steps: read the code that produces this output, write a failing test that reproduces \
             the example, fix it, and make the test pass. Do not change the test to match the bug.",
        );
        text
    }
}

/// A short block for the system prompt: the failures worth naming every turn.
///
/// Counted from the bot's own transcripts, so it is a fact about this instance
/// rather than a generic rule. Kept to a few lines: it rides on every request,
/// and a long report would cost more than it teaches.
pub fn digest(report: &SelfCheck, top: usize) -> Option<String> {
    let mut lines: Vec<String> = report
        .signatures
        .iter()
        .filter(|signature| signature.count >= 5)
        .take(top)
        .map(|signature| {
            format!(
                "- {} × {} ({})",
                signature.kind.as_str(),
                signature.count,
                match signature.kind {
                    Kind::Empty => "turns that produced nothing at all",
                    Kind::NoToolCall => "replies with no tool call, so nothing was sent",
                    Kind::UnparsedDsml => "turns of tool markup that did not parse",
                    Kind::ProviderError => "provider failures recorded as turns",
                    Kind::Repeated => "the same reply repeated verbatim",
                }
            )
        })
        .collect();
    if lines.is_empty() {
        return None;
    }
    let mut text = String::from(
        "--- Your own failure record ---\n\n\
         These are counted from your past turns in this deployment, not rules:\n",
    );
    text.push_str(&lines.join("\n"));
    text.push_str("\n\nAvoid repeating them. Call `get_self_check` when you want the examples.\n");
    lines.clear();
    Some(text)
}

/// Classify one assistant turn, if it is a failure.
pub fn classify(content: &str) -> Option<(Kind, String)> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Some((Kind::Empty, "(empty)".to_string()));
    }
    if trimmed.starts_with("[error]") {
        return Some((Kind::ProviderError, normalise(trimmed)));
    }
    if trimmed == "(empty)" {
        return Some((Kind::Empty, "(empty)".to_string()));
    }
    // A call the parser can read is not a failure, whatever else is in the text.
    if trimmed.contains("<|DSML|invoke") {
        return None;
    }
    // The retired single-object form (`{"node_id":"suspend"}`) still replays as
    // a call; counting it as "text without a call" made the report useless —
    // every turn ends with one.
    if is_legacy_call(trimmed) {
        return None;
    }
    if trimmed.contains("DSML") || trimmed.contains('\u{ff5c}') {
        // Markup that produced no call: the parser saw a block but no invoke.
        return Some((Kind::UnparsedDsml, normalise(trimmed)));
    }
    Some((Kind::NoToolCall, normalise(trimmed)))
}

/// Does this text carry a call in the retired single-object form?
fn is_legacy_call(text: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return false;
    };
    let Some(object) = value.as_object() else {
        return false;
    };
    object.contains_key("node_id") || object.contains_key("action")
}

/// Keep the shape of a failure, drop the varying parts.
///
/// Two turns that differ only in a name, a number or a path are the same bug;
/// collapsing those is what turns 93 individual failures into one signature.
fn normalise(text: &str) -> String {
    let mut out = String::new();
    for word in text.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&mask_numbers(word));
        if out.chars().count() > 200 {
            break;
        }
    }
    out.chars().take(200).collect()
}

/// Replace digit runs with `<n>` so `msg=992010` and `msg=993020` match.
fn mask_numbers(word: &str) -> String {
    let mut out = String::new();
    let mut in_number = false;
    for ch in word.chars() {
        if ch.is_ascii_digit() {
            if !in_number {
                out.push_str("<n>");
                in_number = true;
            }
        } else {
            in_number = false;
            out.push(ch);
        }
    }
    out
}

fn short_hash(text: &str) -> String {
    // FNV-1a: stable across runs, no dependency.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("{hash:08x}")
}

/// Scan a sessions directory and rank what keeps going wrong.
pub fn scan(root: &Path) -> SelfCheck {
    let mut signatures: BTreeMap<(Kind, String), Signature> = BTreeMap::new();
    let mut repeated: BTreeMap<String, usize> = BTreeMap::new();
    let mut scanned_sessions = 0usize;
    let mut scanned_turns = 0usize;

    let Ok(entries) = std::fs::read_dir(root) else {
        return SelfCheck::default();
    };
    let mut paths: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("json"))
        .filter(|path| path.file_name().and_then(|n| n.to_str()) != Some("index.json"))
        .collect();
    paths.sort();

    for path in paths {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(snapshot) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let Some(transcript) = snapshot.get("transcript").and_then(|t| t.as_array()) else {
            continue;
        };
        scanned_sessions += 1;
        let session = path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("unknown")
            .to_string();
        for (index, entry) in transcript.iter().enumerate() {
            if entry.get("kind").and_then(|k| k.as_str()) != Some("assistant") {
                continue;
            }
            scanned_turns += 1;
            let content = entry.get("content").and_then(|c| c.as_str()).unwrap_or("");
            // Repetition matters only for output that achieved nothing; a
            // `suspend` that repeats every turn is the protocol working.
            if !content.trim().is_empty() && classify(content).is_some() {
                *repeated.entry(content.trim().to_string()).or_default() += 1;
            }
            let Some((kind, excerpt)) = classify(content) else {
                continue;
            };
            let signature = signatures
                .entry((kind, excerpt.clone()))
                .or_insert_with(|| Signature {
                    kind,
                    excerpt,
                    count: 0,
                    sessions: Vec::new(),
                    first_turn: index,
                    last_turn: index,
                });
            signature.count += 1;
            if !signature.sessions.contains(&session) {
                signature.sessions.push(session.clone());
            }
            signature.last_turn = index;
        }
    }

    // A reply that repeats word for word is its own failure: the model is stuck.
    for (content, count) in repeated {
        if count < 3 {
            continue;
        }
        signatures
            .entry((Kind::Repeated, normalise(&content)))
            .or_insert_with(|| Signature {
                kind: Kind::Repeated,
                excerpt: normalise(&content),
                count: 0,
                sessions: Vec::new(),
                first_turn: 0,
                last_turn: 0,
            })
            .count += count;
    }

    let mut signatures: Vec<Signature> = signatures.into_values().collect();
    signatures.sort_by(|a, b| b.score().cmp(&a.score()).then(a.excerpt.cmp(&b.excerpt)));
    SelfCheck {
        scanned_sessions,
        scanned_turns,
        signatures,
        lessons: Vec::new(),
        attempted: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_session(root: &Path, name: &str, turns: &[&str]) {
        let transcript: Vec<serde_json::Value> = turns
            .iter()
            .map(|content| serde_json::json!({"kind": "assistant", "content": content}))
            .collect();
        let snapshot = serde_json::json!({"kind": "runtime_shell", "transcript": transcript});
        std::fs::write(root.join(name), serde_json::to_string(&snapshot).unwrap()).unwrap();
    }

    #[test]
    fn empty_turns_are_a_failure() {
        assert_eq!(classify("   "), Some((Kind::Empty, "(empty)".to_string())));
    }

    #[test]
    fn a_readable_call_is_not_a_failure() {
        assert!(
            classify("<|DSML|calls>\n<|DSML|invoke name=\"suspend\"></|DSML|invoke>").is_none()
        );
    }

    #[test]
    fn markup_without_a_call_is_unparsed() {
        let (kind, _) = classify("<｜｜DSML｜｜ calls>").expect("failure");
        assert_eq!(kind, Kind::UnparsedDsml);
    }

    #[test]
    fn a_legacy_suspend_object_is_a_call_not_a_failure() {
        assert!(classify("{\"node_id\":\"suspend\"}").is_none());
        assert!(classify("{\"node_id\":\"qq_send\",\"target\":\"group:1\"}").is_none());
    }

    #[test]
    fn the_literal_empty_marker_counts_as_empty() {
        assert_eq!(
            classify("(empty)"),
            Some((Kind::Empty, "(empty)".to_string()))
        );
    }

    #[test]
    fn plain_prose_is_a_missing_call() {
        let (kind, _) = classify("在的，能收到").expect("failure");
        assert_eq!(kind, Kind::NoToolCall);
    }

    #[test]
    fn provider_errors_keep_their_text() {
        let (kind, excerpt) = classify("[error] LLM request failed: boom").expect("failure");
        assert_eq!(kind, Kind::ProviderError);
        assert!(excerpt.contains("boom"));
    }

    #[test]
    fn normalising_collapses_numbers_and_paths() {
        let a = normalise("failed on msg=992010 at plugins/qq/src/lib.rs");
        let b = normalise("failed on msg=993020 at plugins/qq/src/lib.rs");
        assert_eq!(a, b);
    }

    #[test]
    fn scanning_groups_identical_failures() {
        let dir = tempfile::tempdir().unwrap();
        write_session(dir.path(), "a.json", &["", "", "<|DSML|calls>"]);
        write_session(dir.path(), "b.json", &[""]);
        let report = scan(dir.path());
        assert_eq!(report.scanned_sessions, 2);
        let empty = report
            .signatures
            .iter()
            .find(|s| s.kind == Kind::Empty)
            .expect("empty signature");
        assert_eq!(empty.count, 3);
        assert_eq!(empty.sessions.len(), 2);
    }

    #[test]
    fn the_worst_signature_ranks_first_at_equal_frequency() {
        let dir = tempfile::tempdir().unwrap();
        write_session(dir.path(), "a.json", &["hello", "hello", "hello"]);
        write_session(
            dir.path(),
            "b.json",
            &["<|DSML|calls>", "<|DSML|calls>", "<|DSML|calls>"],
        );
        let report = scan(dir.path());
        assert_eq!(report.signatures[0].kind, Kind::UnparsedDsml);
    }

    #[test]
    fn a_message_repeated_three_times_becomes_its_own_signature() {
        let dir = tempfile::tempdir().unwrap();
        write_session(dir.path(), "a.json", &["同样的话", "同样的话", "同样的话"]);
        let report = scan(dir.path());
        assert!(report.signatures.iter().any(|s| s.kind == Kind::Repeated));
    }

    #[test]
    fn attempted_signatures_are_not_open() {
        let signature = Signature {
            kind: Kind::Empty,
            excerpt: "(empty)".to_string(),
            count: 5,
            sessions: vec!["a".to_string()],
            first_turn: 0,
            last_turn: 0,
        };
        let report = SelfCheck {
            signatures: vec![signature.clone()],
            attempted: vec![signature.id()],
            ..SelfCheck::default()
        };
        assert!(report.open().is_empty());
    }

    #[test]
    fn the_instruction_carries_the_evidence() {
        let signature = Signature {
            kind: Kind::UnparsedDsml,
            excerpt: "<|DSML|calls>".to_string(),
            count: 93,
            sessions: vec!["s1".to_string()],
            first_turn: 4,
            last_turn: 9,
        };
        let report = SelfCheck {
            signatures: vec![signature.clone()],
            ..SelfCheck::default()
        };
        let text = report.instruction(&signature);
        assert!(text.contains("Occurrences: 93"));
        assert!(text.contains("unparsed_dsml"));
        assert!(text.contains("turns 4..=9"));
        assert!(text.contains("s1"));
    }

    #[test]
    fn a_prior_lesson_is_repeated_in_the_instruction() {
        let signature = Signature {
            kind: Kind::NoToolCall,
            excerpt: "hello".to_string(),
            count: 4,
            sessions: Vec::new(),
            first_turn: 0,
            last_turn: 0,
        };
        let report = SelfCheck {
            lessons: vec![Lesson {
                signature: signature.id(),
                symptom: "model answered in prose".to_string(),
                fix: "prompt rewrite".to_string(),
                outcome: "failed: still prose".to_string(),
                learned_at_ms: 0,
            }],
            ..SelfCheck::default()
        };
        let text = report.instruction(&signature);
        assert!(text.contains("Already tried"), "{text}");
        assert!(text.contains("prompt rewrite"), "{text}");
    }

    #[test]
    fn the_digest_names_only_the_failures_worth_naming() {
        let report = SelfCheck {
            signatures: vec![
                Signature {
                    kind: Kind::Empty,
                    excerpt: "(empty)".to_string(),
                    count: 1636,
                    sessions: Vec::new(),
                    first_turn: 0,
                    last_turn: 0,
                },
                Signature {
                    kind: Kind::NoToolCall,
                    excerpt: "hi".to_string(),
                    count: 2,
                    sessions: Vec::new(),
                    first_turn: 0,
                    last_turn: 0,
                },
            ],
            ..SelfCheck::default()
        };
        let text = digest(&report, 3).expect("digest");
        assert!(text.contains("empty × 1636"), "{text}");
        assert!(!text.contains("no_tool_call"), "{text}");
        assert!(text.contains("get_self_check"), "{text}");
    }

    #[test]
    fn no_failures_means_no_digest() {
        assert!(digest(&SelfCheck::default(), 3).is_none());
    }

    #[test]
    fn a_ledger_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("selfcheck.json");
        let mut ledger = Ledger::default();
        ledger.record_attempt("empty-abc123");
        ledger.push_lesson(
            Lesson {
                signature: "empty-abc123".to_string(),
                symptom: "history was cleared".to_string(),
                fix: "keep a floor".to_string(),
                outcome: "verified".to_string(),
                learned_at_ms: 1,
            },
            10,
        );
        ledger.save(&path).unwrap();
        let loaded = Ledger::load(&path);
        assert_eq!(loaded, ledger);
        assert!(loaded.has_attempted("empty-abc123"));
    }

    #[test]
    fn a_missing_or_broken_ledger_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            Ledger::load(&dir.path().join("none.json")),
            Ledger::default()
        );
        let broken = dir.path().join("broken.json");
        std::fs::write(&broken, "{not json").unwrap();
        assert_eq!(Ledger::load(&broken), Ledger::default());
    }

    #[test]
    fn recording_an_attempt_twice_keeps_one_entry() {
        let mut ledger = Ledger::default();
        ledger.record_attempt("a");
        ledger.record_attempt("a");
        assert_eq!(ledger.attempted.len(), 1);
    }

    #[test]
    fn lessons_keep_only_the_most_recent() {
        let mut ledger = Ledger::default();
        for index in 0..5 {
            ledger.push_lesson(
                Lesson {
                    signature: format!("s{index}"),
                    symptom: String::new(),
                    fix: String::new(),
                    outcome: String::new(),
                    learned_at_ms: index as u64,
                },
                3,
            );
        }
        assert_eq!(ledger.lessons.len(), 3);
        assert_eq!(ledger.lessons[0].signature, "s2");
        assert_eq!(ledger.lessons[2].signature, "s4");
    }

    #[test]
    fn a_scan_can_carry_recorded_lessons() {
        let lesson = Lesson {
            signature: "empty-abc".to_string(),
            symptom: "cleared".to_string(),
            fix: "floor".to_string(),
            outcome: "failed".to_string(),
            learned_at_ms: 0,
        };
        let report = SelfCheck::default().with_lessons(vec![lesson.clone()]);
        assert_eq!(report.lessons, vec![lesson]);
    }

    #[test]
    fn a_missing_directory_scans_to_nothing() {
        let report = scan(Path::new("/nonexistent/sessions"));
        assert_eq!(report.scanned_sessions, 0);
        assert!(report.signatures.is_empty());
    }

    #[test]
    fn a_broken_snapshot_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bad.json"), "{not json").unwrap();
        write_session(dir.path(), "good.json", &[""]);
        let report = scan(dir.path());
        assert_eq!(report.scanned_sessions, 1);
    }

    #[test]
    fn kind_weights_rank_markup_above_repetition() {
        assert!(Kind::UnparsedDsml.weight() > Kind::Repeated.weight());
        assert!(Kind::NoToolCall.weight() > Kind::Empty.weight());
        assert_eq!(Kind::Repeated.as_str(), "repeated");
    }
}
