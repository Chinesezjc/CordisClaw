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
use std::path::{Path, PathBuf};

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

/// What a change touches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeTarget {
    Persona,
    Policy,
    Memory,
    Code,
    /// Something the bot could not do for lack of a tool or system ability.
    /// Never applied by the data lane: it is a todo item, not an edit.
    Capability,
}

impl ChangeTarget {
    pub fn as_str(self) -> &'static str {
        match self {
            ChangeTarget::Persona => "persona",
            ChangeTarget::Policy => "policy",
            ChangeTarget::Memory => "memory",
            ChangeTarget::Code => "code",
            ChangeTarget::Capability => "capability",
        }
    }

    /// Whether this change is applied only through the gate, never directly.
    pub fn needs_gate(self) -> bool {
        matches!(self, ChangeTarget::Code | ChangeTarget::Capability)
    }
}

/// Where an applied change stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeState {
    /// Applied and waiting for the next review to judge it.
    Applied,
    /// The next review saw the expected improvement.
    Kept,
    /// The next review saw no improvement, and the old value was restored.
    Reverted,
}

impl ChangeState {
    pub fn as_str(self) -> &'static str {
        match self {
            ChangeState::Applied => "applied",
            ChangeState::Kept => "kept",
            ChangeState::Reverted => "reverted",
        }
    }
}

/// One thing the bot changed about itself, with the evidence needed to undo it.
///
/// A review that only writes prose cannot be checked: the next hour has no idea
/// what the previous one claimed it would do. This record is the missing link —
/// it carries the expectation that makes "did it work?" answerable, and the old
/// value that makes a wrong answer reversible.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Change {
    /// Stable id, e.g. `persona-qq_group_996980275-1a2b3c4d`.
    pub id: String,
    pub target: ChangeTarget,
    /// One line: what was changed.
    pub summary: String,
    /// What should be observable if it worked: "fewer complaints in 6 hours".
    pub expectation: String,
    pub applied_at_ms: u64,
    /// Old-value snapshot, enough to restore the previous state.
    pub rollback: serde_json::Value,
    pub state: ChangeState,
}

impl Change {
    /// Load `data/changes.json`; a missing or broken file is an empty list.
    pub fn load(path: &Path) -> Vec<Change> {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Persist the whole list atomically.
    pub fn save(path: &Path, changes: &[Change]) -> Result<(), String> {
        write_json_atomic(path, &changes)
    }

    /// Changes still waiting for a verdict. Only these are shown to the next
    /// review: a kept or reverted change is settled and asking again would
    /// make the prompt grow without bound.
    pub fn pending(changes: &[Change]) -> Vec<&Change> {
        changes
            .iter()
            .filter(|change| change.state == ChangeState::Applied)
            .collect()
    }
}

/// What the model proposed in a review, before it has been applied.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ProposedChange {
    pub target: ChangeTarget,
    pub summary: String,
    #[serde(default)]
    pub expectation: String,
    /// New value: a persona sentence, a reply policy, a memory note, or (for
    /// `code` and `capability`) a one-line description of the patch.
    #[serde(default)]
    pub value: String,
    /// Persona lane: the exact sentences this change removes. A persona value
    /// is the complete new text, so every sentence that vanishes from the old
    /// text must be named here or the change is refused.
    #[serde(default)]
    pub drops: Vec<String>,
    /// Which room/soul this applies to: a group id, a `qq:group:<id>` key, or
    /// a full soul key. Empty is an error for the data lanes.
    #[serde(default)]
    pub scope: String,
    /// Code and capability lanes: cargo package and exact test name the gate
    /// runs.
    #[serde(default)]
    pub package: String,
    #[serde(default)]
    pub test: String,
    /// Code lane only: unified diff text.
    #[serde(default)]
    pub patch: String,
}

/// The model's verdict on a change that was applied earlier.
#[derive(Debug, Clone, PartialEq)]
pub struct Judgement {
    pub id: String,
    pub state: ChangeState,
    pub reason: String,
}

/// One parsed review: new changes to apply and verdicts on old ones.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Review {
    pub changes: Vec<ProposedChange>,
    pub verdicts: Vec<Judgement>,
}

impl Review {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty() && self.verdicts.is_empty()
    }
}

/// Where a persona change is written.
///
/// The kernel resolves the authoritative store per call: a loaded
/// `soul_store` plugin (SQLite) wins, otherwise `data/souls/*.json`. The CLI
/// deliberately does not know which one is active — it hands this sink to
/// `RuntimeHost::set_soul`, so no SQLite driver is linked into the binary.
pub trait PersonaSink {
    fn persona(&self, soul_key: &str) -> Result<Option<String>, String>;
    fn set_persona(&self, soul_key: &str, persona: &str) -> Result<(), String>;
}

/// Reverses a code patch in the working tree.
///
/// Kept as a trait for the same reason as [`PersonaSink`]: the decision logic
/// lives here and is testable, while the only impure part (git) stays at the
/// process edge.
pub trait PatchReverter {
    fn revert(&self, patch: &Path) -> Result<(), String>;
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

/// One reply and what came back after it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Exchange {
    pub session: String,
    /// The soul whose persona shaped the reply. Empty for snapshots written
    /// before the field existed; those cannot contribute a persona.
    #[serde(default)]
    pub soul_key: String,
    /// What the bot said, as the room saw it.
    pub reply: String,
    /// The first message(s) after it, verbatim.
    pub after: Vec<String>,
}

/// The most recent exchanges across all sessions, oldest first.
///
/// There is no classifier here on purpose. Whether a reply landed badly is a
/// judgement about meaning, and a list of words cannot make it: the same phrase
/// is friendly in one room and insulting in another. The recent conversations
/// are handed to the model to judge instead of being pre-sorted by a
/// hand-written vocabulary.
pub fn recent_exchanges(root: &Path, max: usize) -> Vec<Exchange> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut paths: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("json"))
        .filter(|path| path.file_name().and_then(|n| n.to_str()) != Some("index.json"))
        .collect();
    paths.sort_by_key(|path| {
        path.metadata()
            .and_then(|meta| meta.modified())
            .unwrap_or(std::time::UNIX_EPOCH)
    });

    let mut exchanges = Vec::new();
    for path in paths {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(snapshot) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let Some(history) = snapshot.get("history").and_then(|value| value.as_array()) else {
            continue;
        };
        let soul_key = snapshot
            .get("soul_key")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string();
        let session = path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("unknown")
            .to_string();
        for (index, message) in history.iter().enumerate() {
            if message.get("role").and_then(|role| role.as_str()) != Some("assistant") {
                continue;
            }
            let reply = message
                .get("content")
                .and_then(|c| c.as_str())
                .unwrap_or("");
            // Only the spoken text: the `[tool] …` records are machinery.
            let spoken: String = reply
                .lines()
                .filter(|line| !line.trim_start().starts_with("[tool]"))
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_string();
            if spoken.is_empty() {
                continue;
            }
            let after: Vec<String> = history
                .iter()
                .skip(index + 1)
                .take(3)
                .filter(|next| next.get("role").and_then(|role| role.as_str()) == Some("user"))
                .filter_map(|next| next.get("content").and_then(|c| c.as_str()))
                .map(str::to_string)
                .collect();
            exchanges.push(Exchange {
                session: session.clone(),
                soul_key: soul_key.clone(),
                reply: spoken,
                after,
            });
        }
    }
    let skip = exchanges.len().saturating_sub(max);
    exchanges.split_off(skip)
}

/// The conclusions of its own last reviews, newest first.
///
/// Without them every review starts from nothing: the model writes "next time
/// say half a sentence first", and the next hour it has no idea it ever said
/// that. The notes are its own words, so they are cheap to carry.
pub fn previous_reviews(dir: &Path, max: usize) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|extension| extension.to_str()) == Some("md"))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .rev()
        .take(max)
        .filter_map(|path| std::fs::read_to_string(path).ok())
        .map(|text| unwrap_envelope(&text))
        .filter(|text| !text.trim().is_empty())
        .collect()
}

/// The text of a review, whether or not the model wrapped it in the retired
/// `{"action":"respond","message":…}` envelope.
fn unwrap_envelope(text: &str) -> String {
    let trimmed = text.trim();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if let Some(message) = value.get("message").and_then(|message| message.as_str()) {
            return message.to_string();
        }
    }
    trimmed.to_string()
}

/// A prompt that asks the model to judge its own recent conversations.
///
/// The question is deliberately open: "which of these landed badly" is not a
/// property of words, so it cannot be answered by matching them.
pub fn review_prompt(exchanges: &[Exchange]) -> String {
    review_prompt_after(exchanges, &[], &[], None)
}

/// The review prompt, with the model's own earlier conclusions in front.
///
/// `changes` are the still-unjudged changes from the previous review. Carrying
/// them turns "write another note" into a closed loop: the model is told what
/// it changed, what that was supposed to achieve, and asked whether it did.
///
/// `current_persona` is the persona the review may replace, in full. Without
/// it the model invents a persona from nothing and every review erases the
/// rules earlier reviews accumulated.
pub fn review_prompt_after(
    exchanges: &[Exchange],
    previous: &[String],
    changes: &[Change],
    current_persona: Option<&str>,
) -> String {
    let mut text = String::new();
    if !previous.is_empty() {
        text.push_str(
            "These are your own conclusions from earlier reviews, newest first. \
             First judge whether you actually did them; say plainly if you did not.\n\n",
        );
        for (index, note) in previous.iter().enumerate() {
            text.push_str(&format!(
                "--- 上次复盘 {} ---\n{}\n\n",
                index + 1,
                note.trim()
            ));
        }
    }
    if let Some(persona) = current_persona {
        text.push_str(&persona_section(persona));
    }
    text.push_str(&change_section(changes));
    text.push_str(&review_body(exchanges));
    text
}

/// The block that shows the model the persona a change will replace.
///
/// A persona value is a complete replacement, so this text is the difference
/// between "add one rule" and "keep every rule that still holds". An empty
/// persona adds no section at all.
fn persona_section(persona: &str) -> String {
    let persona = persona.trim();
    if persona.is_empty() {
        return String::new();
    }
    format!(
        "--- 你现在的人格（全文） ---\n{persona}\n\n\
         A `persona` change replaces this text in full: write the **complete new \
         version**, keeping every sentence that is still true. To remove a \
         sentence, put its exact original text in the change's `drops` array. \
         A sentence that disappears without a `drops` entry makes the whole \
         change refused.\n\n"
    )
}

/// The current text of several rooms, labelled by soul key.
///
/// A review reads conversations from more than one room, and a persona change
/// is validated against the room's own old text. Showing only one room would
/// make the model erase the others, so each room it read gets its own section.
pub fn persona_overview(personas: &[(String, String)]) -> Option<String> {
    let sections: Vec<String> = personas
        .iter()
        .filter(|(_, persona)| !persona.trim().is_empty())
        .map(|(soul_key, persona)| format!("[{soul_key}]\n{}", persona.trim()))
        .collect();
    if sections.is_empty() {
        return None;
    }
    Some(format!(
        "Each section starts with its soul key in [brackets]; use that key as `scope`.\n\n{}",
        sections.join("\n\n")
    ))
}

/// The block that asks the model to judge what it changed last time.
fn change_section(changes: &[Change]) -> String {
    if changes.is_empty() {
        return String::new();
    }
    let mut text = String::from(
        "--- 上次应用的改动（还没判定）---\n\n\
         For each change below, answer whether it actually happened. \
         `kept` means the expectation came true; `reverted` means it did not, \
         and the old value will be restored.\n\n",
    );
    for change in changes {
        text.push_str(&format!(
            "- id: {}\n  target: {}\n  changed: {}\n  expectation: {}\n",
            change.id,
            change.target.as_str(),
            change.summary,
            change.expectation,
        ));
    }
    text.push('\n');
    text
}

fn review_body(exchanges: &[Exchange]) -> String {
    let mut text = String::from(
        "Answer in prose, then put **exactly one** ```json fenced block at the very \\
         end. Do not wrap anything in `{\"action\":\"respond\"}` or any other \\
         envelope: that protocol no longer exists.\n\n\
         另外指出：这一轮里有哪些请求你**因为缺少工具或能力而没能服务**\
         （例如“看视频”“读某个站点的图”）。对每一条说明：他们要什么、\
         你缺什么、值不值得补。值得补就给出 `target:\"capability\"` 的改动，\
         并在 `summary` 里写清要造什么。\n\n\
         Below are your recent replies, each followed by what people said next. \
         Judge them as a reader would, not as a log analyser:\n\n\
         - Which replies landed badly — irritated someone, read as lecturing, \
           answered a question nobody asked, or were right but rude?\n\
         - What was the moment that turned? Quote it.\n\
         - What would you say differently, in one sentence of persona?\n\n\
         Do not look for failure codes; there are none. This is about how you talk.\n\n",
    );
    for exchange in exchanges {
        text.push_str("---\n[你说] ");
        text.push_str(exchange.reply.trim());
        text.push('\n');
        for after in &exchange.after {
            text.push_str("[对方] ");
            text.push_str(after.trim());
            text.push('\n');
        }
    }
    text.push_str(
        "\nEnd your reply with exactly one ```json block of this shape:\n\
         {\"changes\":[{\"target\":\"persona|policy|memory|code|capability\",\
         \"scope\":\"<group id or soul key>\",\"summary\":\"one line\",\
         \"expectation\":\"an outcome that can be checked next time\",\
         \"value\":\"the complete new text\",\
         \"drops\":[\"exact original sentence you removed\"]}],\
         \"verdicts\":[{\"id\":\"<id shown above>\",\"state\":\"kept|reverted\",\
         \"reason\":\"one sentence\"}]}\n\
         `drops` is required whenever a persona sentence is removed; an \
         undeclared removal is refused. \
         Use `\"changes\":[]` or `\"verdicts\":[]` when you have none. \
         A `code` change is not applied by you: it needs \"package\", \"test\" \
         and a unified \"patch\" diff; without those it is not machine-checkable \
         and is recorded as such. A `capability` change names something you \
         cannot do for lack of a tool or system ability: it is not applied by \
         you either, so give \"package\", \"test\" and a \"patch\" to have it \
         built, or it is recorded as an open gap. If nothing landed badly and \
         nothing needs a verdict, say so and stop.",
    );
    text
}

// ---------------------------------------------------------------------------
// Structured changes: parse, apply, back up, revert.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RawReview {
    #[serde(default)]
    changes: Vec<ProposedChange>,
    #[serde(default)]
    verdicts: Vec<RawJudgement>,
}

#[derive(Debug, Deserialize)]
struct RawJudgement {
    id: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    reason: String,
}

impl RawJudgement {
    fn into_judgement(self) -> Option<Judgement> {
        let state = match self.state.trim() {
            "kept" => ChangeState::Kept,
            "reverted" => ChangeState::Reverted,
            _ => return None,
        };
        Some(Judgement {
            id: self.id,
            state,
            reason: self.reason,
        })
    }
}

/// Pull every ```json fenced block out of the model's reply.
fn json_blocks(text: &str) -> Vec<&str> {
    let mut blocks = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find("```") {
        let after_open = &rest[open + 3..];
        let Some(language_end) = after_open.find('\n') else {
            break;
        };
        let language = after_open[..language_end].trim();
        let body = &after_open[language_end + 1..];
        let Some(close) = body.find("```") else {
            break;
        };
        if language.eq_ignore_ascii_case("json") {
            blocks.push(&body[..close]);
        }
        rest = &body[close + 3..];
    }
    blocks
}

/// Parse the structured part of a review. Pure, so the "no block", "broken
/// JSON" and "several changes" cases are direct unit tests.
///
/// `None` means the reply carried nothing machine-readable; the caller still
/// keeps the raw review, so a talkative model loses nothing but its edit.
pub fn parse_review(reply: &str) -> Option<Review> {
    let mut review = Review::default();
    for block in json_blocks(reply) {
        let Ok(raw) = serde_json::from_str::<RawReview>(block) else {
            continue;
        };
        review.changes.extend(raw.changes);
        review.verdicts.extend(
            raw.verdicts
                .into_iter()
                .filter_map(RawJudgement::into_judgement),
        );
    }
    if review.is_empty() {
        return None;
    }
    Some(review)
}

/// Replace the record with the same id, or append it.
pub fn upsert_change(changes: &mut Vec<Change>, change: Change) {
    if let Some(slot) = changes.iter_mut().find(|existing| existing.id == change.id) {
        *slot = change;
    } else {
        changes.push(change);
    }
}

/// Stable id for a proposal: `persona-qq_group_996980275-1a2b3c4d`.
pub fn change_id(target: ChangeTarget, scope: &str, summary: &str, value: &str) -> String {
    let digest = short_hash(&format!("{summary}\u{0}{value}"));
    format!(
        "{}-{}-{digest}",
        target.as_str(),
        sanitize_id_part(scope.trim())
    )
}

fn sanitize_id_part(text: &str) -> String {
    text.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn temp_path(path: &Path) -> Result<PathBuf, String> {
    let Some(name) = path.file_name() else {
        return Err(format!("{} has no file name", path.display()));
    };
    let mut owned = name.to_os_string();
    owned.push(".tmp");
    Ok(path.with_file_name(owned))
}

fn write_error(action: &str, path: &Path, error: &std::io::Error) -> String {
    format!("{action} {}: {error}", path.display())
}

fn json_encode_error(path: &Path, error: &serde_json::Error) -> String {
    format!("encode {}: {error}", path.display())
}

/// Write text through a sibling temp file and a rename, so a crash never
/// leaves a half-written JSON document behind.
fn atomic_write_text(path: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(|error| write_error("create", parent, &error))?;
    }
    let tmp = temp_path(path)?;
    std::fs::write(&tmp, text).map_err(|error| write_error("write", &tmp, &error))?;
    if let Err(error) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(write_error("rename", path, &error));
    }
    Ok(())
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let text =
        serde_json::to_string_pretty(value).map_err(|error| json_encode_error(path, &error))?;
    atomic_write_text(path, &text)
}

/// Read a JSON object, treating a missing file as `{}` and broken JSON as an
/// error. Refusing to overwrite a corrupt file is deliberate: clobbering it
/// would destroy the only copy of the old value the rollback needs.
fn read_json_object(path: &Path, label: &str) -> Result<serde_json::Value, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let value: serde_json::Value = serde_json::from_str(&text)
                .map_err(|error| format!("parse {}: {error}", path.display()))?;
            if value.is_object() {
                Ok(value)
            } else {
                Err(format!(
                    "{label} at {} is not a JSON object",
                    path.display()
                ))
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(serde_json::json!({})),
        Err(error) => Err(format!("read {}: {error}", path.display())),
    }
}

/// Snapshot the old value before mutating anything.
fn write_backup(
    data_dir: &Path,
    id: &str,
    applied_at_ms: u64,
    rollback: &serde_json::Value,
) -> Result<PathBuf, String> {
    let path = data_dir
        .join("changes-backup")
        .join(format!("{}-{applied_at_ms}.json", sanitize_id_part(id)));
    write_json_atomic(&path, rollback)?;
    Ok(path)
}

fn rollback_str<'a>(rollback: &'a serde_json::Value, key: &str) -> Result<&'a str, String> {
    rollback
        .get(key)
        .and_then(|value| value.as_str())
        .ok_or_else(|| format!("rollback has no `{key}`"))
}

/// A group id from a scope, accepting both `996980275` and `qq:group:996980275`.
fn group_scope(scope: &str, target: ChangeTarget) -> Result<String, String> {
    let scope = scope.trim();
    let scope = scope.strip_prefix("qq:group:").unwrap_or(scope);
    if scope.is_empty() {
        return Err(format!(
            "{} change has no scope (expected a group id)",
            target.as_str()
        ));
    }
    Ok(scope.to_string())
}

/// The soul key a persona change targets.
///
/// A bare group id becomes `qq:group:<id>`, the key the QQ channel asks for.
/// Anything that already looks like a soul key is left alone: the same change
/// can be aimed at a Feishu chat or a private user.
fn persona_soul_key(scope: &str) -> Result<String, String> {
    let scope = scope.trim();
    if scope.is_empty() {
        return Err("persona change has no scope (expected a group id or soul key)".to_string());
    }
    if scope.contains('#') || scope.contains(':') {
        return Ok(scope.to_string());
    }
    if scope.chars().all(|ch| ch.is_ascii_digit()) {
        return Ok(format!("qq:group:{scope}"));
    }
    Ok(scope.to_string())
}

/// Sentences that vanished from `old` without being declared in `drops`.
///
/// A persona change is the complete new text, so a rule that is absent from
/// `new` is either an intentional removal (named in `drops`) or a regression
/// that would silently delete what earlier reviews learned. Sentences are the
/// unit because that is how the persona is written: split on `。`, `！`, `？`
/// and newlines, keeping the punctuation.
///
/// A `drops` entry matches when it and the sentence are substrings of each
/// other after whitespace is removed, so `"别顶回去"` declares `"别顶回去。"`
/// and a line re-wrapped in the new text still matches.
pub fn dropped_without_leave(old: &str, new: &str, drops: &[String]) -> Vec<String> {
    // An empty declaration would otherwise be a substring of every sentence
    // and silently declare all removals.
    let declared: Vec<String> = drops
        .iter()
        .map(|drop| without_whitespace(drop))
        .filter(|drop| !drop.is_empty())
        .collect();
    split_sentences(old)
        .into_iter()
        .filter(|sentence| !new.contains(sentence))
        .filter(|sentence| {
            let sentence = without_whitespace(sentence);
            !declared
                .iter()
                .any(|drop| sentence.contains(drop) || drop.contains(&sentence))
        })
        .collect()
}

/// The sentences of a persona, punctuation kept and empty lines dropped.
fn split_sentences(text: &str) -> Vec<String> {
    let mut sentences = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        current.push(ch);
        if matches!(ch, '。' | '！' | '？' | '\n') {
            let sentence = current.trim();
            if !sentence.is_empty() {
                sentences.push(sentence.to_string());
            }
            current.clear();
        }
    }
    let sentence = current.trim();
    if !sentence.is_empty() {
        sentences.push(sentence.to_string());
    }
    sentences
}

fn without_whitespace(text: &str) -> String {
    text.chars().filter(|ch| !ch.is_whitespace()).collect()
}

fn apply_persona(
    data_dir: &Path,
    proposal: &ProposedChange,
    id: &str,
    now_ms: u64,
    souls: &dyn PersonaSink,
) -> Result<serde_json::Value, String> {
    let soul_key = persona_soul_key(&proposal.scope)?;
    let old = souls.persona(&soul_key)?;
    let dropped = dropped_without_leave(
        old.as_deref().unwrap_or(""),
        &proposal.value,
        &proposal.drops,
    );
    if !dropped.is_empty() {
        return Err(format!(
            "persona change {id} drops rules without saying so: {}",
            dropped.join(" | ")
        ));
    }
    let rollback = serde_json::json!({"kind": "persona", "soul_key": &soul_key, "persona": old});
    write_backup(data_dir, id, now_ms, &rollback)?;
    souls.set_persona(&soul_key, &proposal.value)?;
    Ok(rollback)
}

fn apply_policy(
    data_dir: &Path,
    proposal: &ProposedChange,
    id: &str,
    now_ms: u64,
) -> Result<serde_json::Value, String> {
    let group = group_scope(&proposal.scope, proposal.target)?;
    let path = data_dir.join("qq_reply_policies.json");
    let mut store = read_json_object(&path, "reply policies")?;
    let root = store
        .as_object_mut()
        .ok_or_else(|| format!("reply policies at {} is not an object", path.display()))?;
    let groups = root
        .entry("groups".to_string())
        .or_insert_with(|| serde_json::json!({}));
    let groups = groups
        .as_object_mut()
        .ok_or_else(|| format!("`groups` in {} is not an object", path.display()))?;
    let existed = groups.contains_key(&group);
    let entry = groups
        .entry(group.clone())
        .or_insert_with(|| serde_json::json!({}));
    let entry = entry
        .as_object_mut()
        .ok_or_else(|| format!("group {group} in {} is not an object", path.display()))?;
    let old = entry
        .get("policy")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    let rollback =
        serde_json::json!({"kind": "policy", "scope": &group, "policy": old, "existed": existed});
    write_backup(data_dir, id, now_ms, &rollback)?;
    // `enabled` is deliberately untouched: a review may adjust what the bot is
    // told, never whether it is allowed to speak.
    entry.insert(
        "policy".to_string(),
        serde_json::Value::String(proposal.value.clone()),
    );
    entry.insert("updated_at_ms".to_string(), serde_json::json!(now_ms));
    entry.insert(
        "updated_by".to_string(),
        serde_json::Value::String("selfcheck".to_string()),
    );
    write_json_atomic(&path, &store)?;
    Ok(rollback)
}

fn apply_memory(
    data_dir: &Path,
    proposal: &ProposedChange,
    id: &str,
    now_ms: u64,
) -> Result<serde_json::Value, String> {
    let group = group_scope(&proposal.scope, proposal.target)?;
    let path = data_dir.join("qq_group_memory.json");
    let mut store = read_json_object(&path, "group memory")?;
    let root = store
        .as_object_mut()
        .ok_or_else(|| format!("group memory at {} is not an object", path.display()))?;
    let existed = root.contains_key(&group);
    let entry = root
        .entry(group.clone())
        .or_insert_with(|| serde_json::json!({}));
    let entry = entry
        .as_object_mut()
        .ok_or_else(|| format!("group {group} in {} is not an object", path.display()))?;
    let old = entry
        .get("memory")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .to_string();
    let memory = if old.trim().is_empty() {
        proposal.value.clone()
    } else {
        format!("{old}\n{}", proposal.value)
    };
    let rollback =
        serde_json::json!({"kind": "memory", "scope": &group, "memory": old, "existed": existed});
    write_backup(data_dir, id, now_ms, &rollback)?;
    entry.insert("memory".to_string(), serde_json::Value::String(memory));
    entry.insert("updated_at_ms".to_string(), serde_json::json!(now_ms));
    entry.insert(
        "updated_by".to_string(),
        serde_json::Value::String("selfcheck".to_string()),
    );
    write_json_atomic(&path, &store)?;
    Ok(rollback)
}

/// Apply one proposal to the live data, after backing up what it replaces.
pub fn apply_proposed(
    data_dir: &Path,
    proposal: &ProposedChange,
    now_ms: u64,
    souls: &dyn PersonaSink,
) -> Result<Change, String> {
    let id = change_id(
        proposal.target,
        &proposal.scope,
        &proposal.summary,
        &proposal.value,
    );
    let rollback = match proposal.target {
        ChangeTarget::Persona => apply_persona(data_dir, proposal, &id, now_ms, souls)?,
        ChangeTarget::Policy => apply_policy(data_dir, proposal, &id, now_ms)?,
        ChangeTarget::Memory => apply_memory(data_dir, proposal, &id, now_ms)?,
        _ => {
            // `code` and `capability` are applied only by the gate, never here.
            return Err(format!(
                "{} change {id} must pass the gate before it is applied",
                proposal.target.as_str()
            ));
        }
    };
    Ok(Change {
        id,
        target: proposal.target,
        summary: proposal.summary.clone(),
        expectation: proposal.expectation.clone(),
        applied_at_ms: now_ms,
        rollback,
        state: ChangeState::Applied,
    })
}

/// What applying a review's data-lane changes produced.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AppliedReview {
    pub applied: Vec<Change>,
    pub notes: Vec<String>,
}

/// Apply every non-code proposal. A failure is a note, not a panic: one bad
/// change must not discard the rest of the review.
pub fn apply_review_data(
    data_dir: &Path,
    proposals: &[ProposedChange],
    now_ms: u64,
    souls: &dyn PersonaSink,
) -> AppliedReview {
    let mut outcome = AppliedReview::default();
    for proposal in proposals {
        if proposal.target.needs_gate() {
            outcome.notes.push(format!(
                "{} change {:?} is left to the gate",
                proposal.target.as_str(),
                proposal.summary
            ));
            continue;
        }
        match apply_proposed(data_dir, proposal, now_ms, souls) {
            Ok(change) => outcome.applied.push(change),
            Err(error) => outcome.notes.push(format!(
                "skipped {} change: {error}",
                proposal.target.as_str()
            )),
        }
    }
    outcome
}

fn revert_persona(rollback: &serde_json::Value, souls: &dyn PersonaSink) -> Result<(), String> {
    let soul_key = rollback_str(rollback, "soul_key")?;
    let persona = rollback
        .get("persona")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    souls.set_persona(soul_key, persona)
}

fn restore_policy(data_dir: &Path, rollback: &serde_json::Value) -> Result<(), String> {
    let group = rollback_str(rollback, "scope")?;
    let old = rollback_str(rollback, "policy")?;
    let existed = rollback
        .get("existed")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let path = data_dir.join("qq_reply_policies.json");
    let mut store = read_json_object(&path, "reply policies")?;
    let root = store
        .as_object_mut()
        .ok_or_else(|| format!("reply policies at {} is not an object", path.display()))?;
    let groups = root
        .get_mut("groups")
        .and_then(|groups| groups.as_object_mut())
        .ok_or_else(|| format!("`groups` in {} is not an object", path.display()))?;
    if existed {
        let entry = groups
            .get_mut(group)
            .and_then(|entry| entry.as_object_mut())
            .ok_or_else(|| format!("group {group} in {} is not an object", path.display()))?;
        entry.insert(
            "policy".to_string(),
            serde_json::Value::String(old.to_string()),
        );
    } else {
        groups.remove(group);
    }
    write_json_atomic(&path, &store)
}

fn restore_memory(data_dir: &Path, rollback: &serde_json::Value) -> Result<(), String> {
    let group = rollback_str(rollback, "scope")?;
    let old = rollback_str(rollback, "memory")?;
    let existed = rollback
        .get("existed")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let path = data_dir.join("qq_group_memory.json");
    let mut store = read_json_object(&path, "group memory")?;
    let root = store
        .as_object_mut()
        .ok_or_else(|| format!("group memory at {} is not an object", path.display()))?;
    if existed {
        let entry = root
            .get_mut(group)
            .and_then(|entry| entry.as_object_mut())
            .ok_or_else(|| format!("group {group} in {} is not an object", path.display()))?;
        entry.insert(
            "memory".to_string(),
            serde_json::Value::String(old.to_string()),
        );
    } else {
        root.remove(group);
    }
    write_json_atomic(&path, &store)
}

/// Restore the old value recorded in `change.rollback`.
pub fn revert_change(
    data_dir: &Path,
    change: &Change,
    souls: &dyn PersonaSink,
    patches: &dyn PatchReverter,
) -> Result<(), String> {
    match change.target {
        ChangeTarget::Persona => revert_persona(&change.rollback, souls),
        ChangeTarget::Policy => restore_policy(data_dir, &change.rollback),
        ChangeTarget::Memory => restore_memory(data_dir, &change.rollback),
        ChangeTarget::Code => {
            let patch = rollback_str(&change.rollback, "patch")?;
            let patch = Path::new(patch);
            let patch = if patch.is_absolute() {
                patch.to_path_buf()
            } else {
                data_dir.join(patch)
            };
            patches.revert(&patch)
        }
        // A capability record is a todo item, not an edit to the live data:
        // `reverted` means "not worth building", so nothing is restored.
        ChangeTarget::Capability => Ok(()),
    }
}

/// Apply the next review's kept/reverted verdicts.
///
/// A change with no verdict stays `Applied` and is asked about again next
/// time; a reverted one is restored before its state changes, so a failed
/// restore leaves it visibly applied instead of silently claiming success.
pub fn reconcile_verdicts(
    data_dir: &Path,
    changes: &mut [Change],
    judgements: &[Judgement],
    souls: &dyn PersonaSink,
    patches: &dyn PatchReverter,
) -> Vec<String> {
    let mut notes = Vec::new();
    for judgement in judgements {
        let Some(change) = changes.iter_mut().find(|change| change.id == judgement.id) else {
            notes.push(format!("verdict for unknown change {}", judgement.id));
            continue;
        };
        if change.state != ChangeState::Applied {
            notes.push(format!(
                "change {} is already {}",
                change.id,
                change.state.as_str()
            ));
            continue;
        }
        match judgement.state {
            ChangeState::Kept => change.state = ChangeState::Kept,
            ChangeState::Reverted => match revert_change(data_dir, change, souls, patches) {
                Ok(()) => change.state = ChangeState::Reverted,
                Err(error) => notes.push(format!("could not revert {}: {error}", change.id)),
            },
            ChangeState::Applied => notes.push(format!(
                "verdict for {} is neither kept nor reverted",
                change.id
            )),
        }
    }
    notes
}

/// Write a gate-ready proposal (a `code` or `capability` change) in the shape
/// the existing gate reads.
///
/// Refusing a patch-less proposal here (rather than letting the gate fail on a
/// missing file) is what makes "no machine-checkable proposal" a recorded fact
/// instead of an ambiguous gate error.
pub fn write_code_proposal(
    data_dir: &Path,
    proposal: &ProposedChange,
    id: &str,
) -> Result<PathBuf, String> {
    if proposal.patch.trim().is_empty() {
        return Err(format!("no machine-checkable proposal for {id}: no patch"));
    }
    if proposal.package.trim().is_empty() || proposal.test.trim().is_empty() {
        return Err(format!(
            "no machine-checkable proposal for {id}: no package or test"
        ));
    }
    let dir = data_dir.join("proposals").join(sanitize_id_part(id));
    std::fs::create_dir_all(&dir).map_err(|error| format!("create {}: {error}", dir.display()))?;
    atomic_write_text(&dir.join("patch.diff"), &proposal.patch)?;
    let document = serde_json::json!({
        "signature": id,
        "summary": proposal.summary,
        "package": proposal.package,
        "test": proposal.test,
        "patch": "patch.diff",
    });
    write_json_atomic(&dir.join("proposal.json"), &document)?;
    Ok(dir)
}

/// Record a code change that the gate verified.
pub fn code_change(id: &str, proposal: &ProposedChange, now_ms: u64) -> Change {
    Change {
        id: id.to_string(),
        target: ChangeTarget::Code,
        summary: proposal.summary.clone(),
        expectation: proposal.expectation.clone(),
        applied_at_ms: now_ms,
        rollback: serde_json::json!({
            "kind": "code",
            "patch": format!("proposals/{}/patch.diff", sanitize_id_part(id)),
        }),
        state: ChangeState::Applied,
    }
}

/// What staging a capability proposal produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StagedCapability {
    /// Package + test + patch: ready for the gate to judge.
    Proposal(PathBuf),
    /// No machine-checkable proposal: only a note, so the gap stays visible.
    Report(PathBuf),
}

/// Stage a capability proposal in the directory the gate reads.
///
/// A complete proposal goes to the gate. An incomplete one becomes a
/// `report.md` note instead of vanishing: "I cannot do this" has to survive
/// into the next review as an open gap, or the bot forgets its own limits.
pub fn stage_capability(
    data_dir: &Path,
    proposal: &ProposedChange,
    id: &str,
) -> Result<StagedCapability, String> {
    match write_code_proposal(data_dir, proposal, id) {
        Ok(dir) => Ok(StagedCapability::Proposal(dir)),
        Err(error) => {
            let dir = data_dir.join("proposals").join(sanitize_id_part(id));
            std::fs::create_dir_all(&dir).map_err(|error| write_error("create", &dir, &error))?;
            let mut note = format!("# {id}\n\n尚无实现：{error}\n\n");
            note.push_str("- summary: ");
            note.push_str(&proposal.summary);
            note.push_str("\n- expectation: ");
            note.push_str(&proposal.expectation);
            note.push('\n');
            atomic_write_text(&dir.join("report.md"), &note)?;
            Ok(StagedCapability::Report(dir))
        }
    }
}

/// Record a capability gap as a todo item.
///
/// There is nothing to roll back: this is a note that the bot cannot do
/// something, not an edit to the live data. Keeping it in `changes.json` is
/// what lets the next review ask "is this gap still worth closing?".
pub fn capability_change(id: &str, proposal: &ProposedChange, now_ms: u64) -> Change {
    Change {
        id: id.to_string(),
        target: ChangeTarget::Capability,
        summary: proposal.summary.clone(),
        expectation: proposal.expectation.clone(),
        applied_at_ms: now_ms,
        rollback: serde_json::json!({}),
        state: ChangeState::Applied,
    }
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
    // A call the parser reads is not a failure, whatever else is in the text.
    // The parser folds the spellings the model mixes — the fullwidth pipe
    // (U+FF5C), a doubled bar and the space after the tag name — so ask the
    // same question. Matching the ASCII spelling alone reported every such
    // call as dropped markup even though it ran and the turn answered.
    if has_readable_call(trimmed) {
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

/// Does this text carry a call the DSML parser can actually read?
///
/// The parser folds the spellings the model mixes before it looks for a tag:
/// the fullwidth pipe (U+FF5C) becomes ASCII, a doubled bar becomes one, and
/// the space after the tag name is dropped. Folding only the whitespace here
/// would still miss the first two, which is what made a readable call look
/// like dropped markup.
fn has_readable_call(text: &str) -> bool {
    let mut folded = String::new();
    for ch in text.to_ascii_lowercase().chars() {
        let ch = if ch as u32 == 0xff5c { '|' } else { ch };
        if ch.is_whitespace() {
            continue;
        }
        if ch == '|' && folded.ends_with('|') {
            continue;
        }
        folded.push(ch);
    }
    let needle: String = ['|', 'd', 's', 'm', 'l', '|', 'i', 'n', 'v', 'o', 'k', 'e']
        .iter()
        .collect();
    folded.contains(&needle)
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
            // A channel turn speaks through a tool, so its content is empty
            // even when it answered. Without this check every such turn was
            // counted as "produced nothing", and the top failure signature was
            // an artefact of the protocol rather than a defect.
            // Entries written before `tools` existed cannot tell speech from
            // silence, so they are not evidence either way. Counting them made
            // "empty" the top signature with thousands of turns that had in
            // fact answered through `qq_send`.
            let Some(tools) = entry.get("tools").and_then(|value| value.as_array()) else {
                continue;
            };
            if !tools.is_empty() {
                continue;
            }
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
            .map(
                |content| serde_json::json!({"kind": "assistant", "content": content, "tools": []}),
            )
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

    /// A call the parser reads is never an unparsed_dsml failure.
    ///
    /// The parser folds the fullwidth pipe (U+FF5C), a doubled tag bar and the
    /// space after the tag name; this check matched the ASCII spelling only, so
    /// a call written the other way was reported as dropped markup even though
    /// its call ran and the turn answered.
    #[test]
    fn a_call_the_parser_reads_is_not_an_unparsed_failure() {
        let lt = char::from_u32(0x3c).unwrap();
        let gt = char::from_u32(0x3e).unwrap();
        let q = char::from_u32(0x22).unwrap();
        let fw = char::from_u32(0xff5c).unwrap();
        let nl = char::from_u32(0x0a).unwrap();
        let fullwidth = format!(
            "{lt}{fw}{fw}DSML{fw}{fw} calls{gt}{nl}{lt}{fw}{fw}DSML{fw}{fw} invoke name={q}suspend{q}{gt}",
        );
        let spaced = format!("{lt}|DSML|calls{gt}{nl}{lt}|DSML|invoke name={q}suspend{q}{gt}",);
        for call in [fullwidth, spaced] {
            assert!(
                classify(&call).is_none(),
                "a call the parser reads is not a failure: {call}"
            );
        }
        let broken = format!("{lt}|DSML|calls{gt}{nl}{lt}|DSML|call name={q}suspend{q}{gt}",);
        assert_eq!(classify(&broken).expect("failure").0, Kind::UnparsedDsml);
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

    /// A turn that answered through a tool has empty content and is not a
    /// failure; counting it made "empty" the top signature by a wide margin.
    #[test]
    fn a_turn_that_ran_a_tool_is_not_an_empty_failure() {
        let dir = tempfile::tempdir().unwrap();
        let transcript = serde_json::json!([
            {"kind": "assistant", "content": "", "tools": ["qq_send"]},
            {"kind": "assistant", "content": "", "tools": []},
            {"kind": "assistant", "content": ""}
        ]);
        std::fs::write(
            dir.path().join("a.json"),
            serde_json::to_string(&serde_json::json!({"transcript": transcript})).unwrap(),
        )
        .unwrap();
        let report = scan(dir.path());
        let empty = report
            .signatures
            .iter()
            .find(|signature| signature.kind == Kind::Empty)
            .expect("one genuine empty turn");
        assert_eq!(empty.count, 1);
    }

    #[test]
    fn recent_exchanges_pair_a_reply_with_what_followed_it() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = serde_json::json!({
            "soul_key": "qq:group:1",
            "history": [
                {"role": "user", "content": "你怎么看"},
                {"role": "assistant", "content": "先当假的看\n\n[tool] qq_send {}"},
                {"role": "user", "content": "你能不能换个说法"}
            ]
        });
        std::fs::write(
            dir.path().join("a.json"),
            serde_json::to_string(&snapshot).unwrap(),
        )
        .unwrap();
        let exchanges = recent_exchanges(dir.path(), 10);
        assert_eq!(exchanges.len(), 1);
        assert_eq!(exchanges[0].soul_key, "qq:group:1");
        assert_eq!(exchanges[0].reply, "先当假的看");
        assert_eq!(exchanges[0].after, vec!["你能不能换个说法".to_string()]);
    }

    #[test]
    fn a_review_prompt_carries_the_conversation_and_asks_for_judgement() {
        let exchanges = vec![Exchange {
            session: "s".to_string(),
            soul_key: "qq:group:1".to_string(),
            reply: "先当假的看".to_string(),
            after: vec!["换个说法".to_string()],
        }];
        let prompt = review_prompt(&exchanges);
        assert!(prompt.contains("先当假的看"), "{prompt}");
        assert!(prompt.contains("换个说法"), "{prompt}");
        assert!(prompt.contains("persona"), "{prompt}");
        // No classifier vocabulary: the question is about judgement.
        assert!(prompt.contains("Judge"), "{prompt}");
    }

    #[test]
    fn exchanges_without_speech_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = serde_json::json!({
            "history": [
                {"role": "assistant", "content": "[tool] qq_send {}"},
                {"role": "user", "content": "嗯"}
            ]
        });
        std::fs::write(
            dir.path().join("a.json"),
            serde_json::to_string(&snapshot).unwrap(),
        )
        .unwrap();
        assert!(recent_exchanges(dir.path(), 10).is_empty());
    }

    #[test]
    fn previous_reviews_are_read_newest_first_and_unwrapped() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("1.md"), "第一条结论").unwrap();
        std::fs::write(
            dir.path().join("2.md"),
            r#"{"action":"respond","message":"第二条结论"}"#,
        )
        .unwrap();
        // The marker file is not a review.
        std::fs::write(dir.path().join("last"), "").unwrap();
        let notes = previous_reviews(dir.path(), 2);
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0], "第二条结论");
        assert_eq!(notes[1], "第一条结论");
    }

    #[test]
    fn a_review_prompt_carries_the_earlier_conclusions() {
        let exchanges = vec![Exchange {
            session: "s".to_string(),
            soul_key: "qq:group:1".to_string(),
            reply: "嗯".to_string(),
            after: vec!["你怎么不说话".to_string()],
        }];
        let prompt = review_prompt_after(&exchanges, &["下次先接半句".to_string()], &[], None);
        assert!(prompt.contains("下次先接半句"), "{prompt}");
        assert!(
            prompt.contains("judge whether you actually did them"),
            "{prompt}"
        );
        // The conversation is still there.
        assert!(prompt.contains("你怎么不说话"), "{prompt}");
        // The structured contract is stated once, at the end.
        assert!(prompt.contains("```json"), "{prompt}");
        assert!(prompt.contains("\"verdicts\""), "{prompt}");
    }

    #[test]
    fn a_review_prompt_carries_the_current_persona_in_full() {
        let persona = "别人说气，先接住。\n不顶回去。";
        let prompt = review_prompt_after(&[], &[], &[], Some(persona));
        assert!(prompt.contains("你现在的人格（全文）"), "{prompt}");
        assert!(prompt.contains(persona), "{prompt}");
        // The contract the validator enforces is stated in the prompt.
        assert!(prompt.contains("drops"), "{prompt}");
        assert!(prompt.contains("complete new version"), "{prompt}");
        // An absent or blank persona adds no empty section.
        for none in [None, Some("   ")] {
            let prompt = review_prompt_after(&[], &[], &[], none);
            let present = prompt.contains("你现在的人格（全文）");
            assert!(!present, "{prompt}");
        }
    }

    #[test]
    fn a_multi_room_review_lists_each_rooms_persona() {
        let rooms = vec![
            ("qq:group:1".to_string(), "甲。".to_string()),
            ("qq:group:2".to_string(), "  ".to_string()),
            ("qq:group:3".to_string(), "乙。".to_string()),
        ];
        let overview = persona_overview(&rooms).expect("two rooms");
        assert!(overview.contains("use that key as `scope`"), "{overview}");
        assert!(overview.contains("[qq:group:1]\n甲。"), "{overview}");
        assert!(overview.contains("[qq:group:3]\n乙。"), "{overview}");
        // A room with no persona contributes nothing.
        assert!(!overview.contains("qq:group:2"), "{overview}");
        assert!(persona_overview(&[]).is_none());
        assert!(persona_overview(&[("k".to_string(), " ".to_string())]).is_none());
    }

    #[test]
    fn a_review_prompt_carries_pending_changes_and_asks_for_a_verdict() {
        let change = Change {
            id: "persona-qq_group_1-abc".to_string(),
            target: ChangeTarget::Persona,
            summary: "先接半句".to_string(),
            expectation: "6 小时内同类抱怨减少".to_string(),
            applied_at_ms: 7,
            rollback: serde_json::json!({"persona": "旧句子"}),
            state: ChangeState::Applied,
        };
        let prompt = review_prompt_after(&[], &[], &[change], None);
        assert!(prompt.contains("persona-qq_group_1-abc"), "{prompt}");
        assert!(prompt.contains("先接半句"), "{prompt}");
        assert!(prompt.contains("6 小时内同类抱怨减少"), "{prompt}");
        assert!(prompt.contains("kept"), "{prompt}");
        assert!(prompt.contains("reverted"), "{prompt}");
    }

    #[test]
    fn a_review_prompt_asks_which_requests_were_out_of_reach() {
        let exchanges = vec![Exchange {
            session: "s".to_string(),
            soul_key: "qq:group:1".to_string(),
            reply: "我没有工具看视频".to_string(),
            after: vec!["那你能看这个图吗".to_string()],
        }];
        let prompt = review_prompt(&exchanges);
        assert!(prompt.contains("因为缺少工具或能力而没能服务"), "{prompt}");
        assert!(prompt.contains("target:\"capability\""), "{prompt}");
        assert!(prompt.contains("值不值得补"), "{prompt}");
        // It is part of the opening instructions, before the conversation.
        let asked = prompt.find("因为缺少工具或能力而没能服务").unwrap();
        let conversation = prompt.find("Below are your recent replies").unwrap();
        assert!(asked < conversation, "{prompt}");
    }

    #[test]
    fn no_pending_change_means_no_verdict_section() {
        assert!(change_section(&[]).is_empty());
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

    // -----------------------------------------------------------------------
    // Structured changes
    // -----------------------------------------------------------------------

    #[derive(Default)]
    struct TestSouls {
        inner: std::cell::RefCell<std::collections::BTreeMap<String, String>>,
    }

    impl PersonaSink for TestSouls {
        fn persona(&self, soul_key: &str) -> Result<Option<String>, String> {
            Ok(self.inner.borrow().get(soul_key).cloned())
        }

        fn set_persona(&self, soul_key: &str, persona: &str) -> Result<(), String> {
            self.inner
                .borrow_mut()
                .insert(soul_key.to_string(), persona.to_string());
            Ok(())
        }
    }

    struct BrokenSouls;

    impl PersonaSink for BrokenSouls {
        fn persona(&self, _soul_key: &str) -> Result<Option<String>, String> {
            Err("read failed".to_string())
        }

        fn set_persona(&self, _soul_key: &str, _persona: &str) -> Result<(), String> {
            Err("write failed".to_string())
        }
    }

    #[derive(Default)]
    struct RecordingReverter {
        seen: std::cell::RefCell<Vec<PathBuf>>,
        fail: bool,
    }

    impl PatchReverter for RecordingReverter {
        fn revert(&self, patch: &Path) -> Result<(), String> {
            self.seen.borrow_mut().push(patch.to_path_buf());
            if self.fail {
                return Err("git failed".to_string());
            }
            Ok(())
        }
    }

    fn proposed(target: ChangeTarget, scope: &str, value: &str) -> ProposedChange {
        ProposedChange {
            target,
            summary: "一句话".to_string(),
            expectation: "六小时内变好".to_string(),
            value: value.to_string(),
            drops: Vec::new(),
            scope: scope.to_string(),
            package: String::new(),
            test: String::new(),
            patch: String::new(),
        }
    }

    fn change(target: ChangeTarget, rollback: serde_json::Value) -> Change {
        Change {
            id: format!("{}-scope-abc", target.as_str()),
            target,
            summary: "一句话".to_string(),
            expectation: "六小时内变好".to_string(),
            applied_at_ms: 9,
            rollback,
            state: ChangeState::Applied,
        }
    }

    #[test]
    fn target_and_state_names_cover_every_variant() {
        assert_eq!(ChangeTarget::Persona.as_str(), "persona");
        assert_eq!(ChangeTarget::Policy.as_str(), "policy");
        assert_eq!(ChangeTarget::Memory.as_str(), "memory");
        assert_eq!(ChangeTarget::Code.as_str(), "code");
        assert_eq!(ChangeTarget::Capability.as_str(), "capability");
        assert!(ChangeTarget::Code.needs_gate());
        assert!(ChangeTarget::Capability.needs_gate());
        assert!(!ChangeTarget::Persona.needs_gate());
        assert_eq!(ChangeState::Applied.as_str(), "applied");
        assert_eq!(ChangeState::Kept.as_str(), "kept");
        assert_eq!(ChangeState::Reverted.as_str(), "reverted");
    }

    #[test]
    fn every_target_and_state_round_trips_through_serde_and_debug() {
        for target in [
            ChangeTarget::Persona,
            ChangeTarget::Policy,
            ChangeTarget::Memory,
            ChangeTarget::Code,
            ChangeTarget::Capability,
        ] {
            let text = serde_json::to_string(&target).unwrap();
            assert_eq!(serde_json::from_str::<ChangeTarget>(&text).unwrap(), target);
            assert_eq!(target.as_str(), text.trim_matches('"'));
            // Formatting executes the derived `Debug` arms; without this the
            // only reference is the cold panic path of an assertion.
            assert!(!format!("{target:?}").is_empty());
        }
        for state in [
            ChangeState::Applied,
            ChangeState::Kept,
            ChangeState::Reverted,
        ] {
            let text = serde_json::to_string(&state).unwrap();
            assert_eq!(serde_json::from_str::<ChangeState>(&text).unwrap(), state);
            assert_eq!(state.as_str(), text.trim_matches('"'));
            assert!(!format!("{state:?}").is_empty());
        }
        let item = change(ChangeTarget::Persona, serde_json::json!({}));
        assert!(!format!("{item:?}").is_empty());
        let proposal = proposed(ChangeTarget::Policy, "1", "x");
        assert!(!format!("{proposal:?}").is_empty());
        let judgement = Judgement {
            id: "a".to_string(),
            state: ChangeState::Kept,
            reason: "好".to_string(),
        };
        assert!(!format!("{judgement:?}").is_empty());
        let review = Review {
            changes: vec![proposal],
            verdicts: vec![judgement],
        };
        assert!(!format!("{review:?}").is_empty());
        let applied = AppliedReview {
            applied: vec![item],
            notes: vec!["n".to_string()],
        };
        assert!(!format!("{applied:?}").is_empty());
        assert_eq!(applied, applied.clone());
    }

    #[test]
    fn changes_round_trip_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("changes.json");
        let targets = [
            ChangeTarget::Persona,
            ChangeTarget::Policy,
            ChangeTarget::Memory,
            ChangeTarget::Code,
            ChangeTarget::Capability,
        ];
        let states = [
            ChangeState::Applied,
            ChangeState::Kept,
            ChangeState::Reverted,
            ChangeState::Applied,
            ChangeState::Applied,
        ];
        let items: Vec<Change> = targets
            .into_iter()
            .zip(states)
            .enumerate()
            .map(|(index, (target, state))| {
                let mut item = change(target, serde_json::json!({"old": index}));
                item.id = format!("{}-{index}", target.as_str());
                item.state = state;
                item
            })
            .collect();
        Change::save(&path, &items).unwrap();
        assert_eq!(Change::load(&path), items);
    }

    #[test]
    fn a_missing_or_broken_change_list_starts_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Change::load(&dir.path().join("none.json")).is_empty());
        let broken = dir.path().join("broken.json");
        std::fs::write(&broken, "{not json").unwrap();
        assert!(Change::load(&broken).is_empty());
    }

    #[test]
    fn a_change_list_that_cannot_be_written_reports_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("blocked");
        std::fs::write(&blocked, "x").unwrap();
        let error = Change::save(&blocked.join("changes.json"), &[]).unwrap_err();
        assert!(error.contains("create"), "{error}");
    }

    #[test]
    fn only_applied_changes_are_pending() {
        let mut applied = change(ChangeTarget::Persona, serde_json::json!({}));
        let mut kept = applied.clone();
        kept.id = "kept".to_string();
        kept.state = ChangeState::Kept;
        let mut reverted = applied.clone();
        reverted.id = "reverted".to_string();
        reverted.state = ChangeState::Reverted;
        applied.id = "applied".to_string();
        let all = [applied.clone(), kept, reverted];
        let pending: Vec<&Change> = Change::pending(&all);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "applied");
    }

    #[test]
    fn a_reply_without_a_json_block_has_no_structured_review() {
        assert!(parse_review("只是普通的复盘文字").is_none());
    }

    #[test]
    fn broken_json_inside_a_block_is_ignored() {
        assert!(parse_review("```json\n{\"changes\": [}\n```").is_none());
    }

    #[test]
    fn several_changes_in_one_block_are_all_parsed() {
        let reply = "```json\n{\"changes\":[\
            {\"target\":\"persona\",\"summary\":\"先说半句\",\"expectation\":\"少抱怨\",\"value\":\"嗯，我在。\",\"scope\":\"996980275\"},\
            {\"target\":\"policy\",\"summary\":\"少插话\",\"expectation\":\"安静\",\"value\":\"只在被问时说\",\"scope\":\"qq:group:1\"},\
            {\"target\":\"code\",\"summary\":\"修测试\",\"package\":\"cordis-agent\",\"test\":\"tests::x\",\"patch\":\"diff\"}]}\n```";
        let review = parse_review(reply).expect("review");
        assert_eq!(review.changes.len(), 3);
        assert_eq!(review.changes[0].target, ChangeTarget::Persona);
        assert_eq!(review.changes[0].scope, "996980275");
        assert_eq!(review.changes[1].target, ChangeTarget::Policy);
        assert_eq!(review.changes[2].target, ChangeTarget::Code);
        assert_eq!(review.changes[2].package, "cordis-agent");
        assert_eq!(review.changes[2].test, "tests::x");
        assert_eq!(review.changes[2].patch, "diff");
        assert!(review.verdicts.is_empty());
    }

    #[test]
    fn a_proposed_change_carries_its_declared_drops() {
        let with = serde_json::from_str::<ProposedChange>(
            r#"{"target":"persona","summary":"s","value":"v","drops":["旧句。"]}"#,
        )
        .unwrap();
        assert_eq!(with.drops, vec!["旧句。".to_string()]);
        // Reviews written before `drops` existed still parse.
        let without = serde_json::from_str::<ProposedChange>(
            r#"{"target":"persona","summary":"s","value":"v"}"#,
        )
        .unwrap();
        assert!(without.drops.is_empty());
    }

    #[test]
    fn several_json_blocks_are_merged_and_non_json_fences_ignored() {
        let reply = "\
```text\nnot json\n```\n\
```json\n{\"changes\":[{\"target\":\"memory\",\"summary\":\"a\",\"value\":\"x\",\"scope\":\"1\"}]}\n```\n\
```JSON\n{\"verdicts\":[{\"id\":\"m-1\",\"state\":\"kept\",\"reason\":\"好\"}]}\n```";
        let review = parse_review(reply).expect("review");
        assert_eq!(review.changes.len(), 1);
        assert_eq!(review.changes[0].target, ChangeTarget::Memory);
        assert_eq!(review.verdicts.len(), 1);
        assert_eq!(review.verdicts[0].state, ChangeState::Kept);
    }

    #[test]
    fn an_unclosed_fence_stops_parsing_without_panicking() {
        assert!(json_blocks("```json\n{\"changes\":[]}").is_empty());
        assert!(json_blocks("```json").is_empty());
    }

    #[test]
    fn an_empty_review_object_is_not_a_review() {
        assert!(parse_review("```json\n{}\n```").is_none());
    }

    #[test]
    fn only_kept_and_reverted_verdicts_survive_parsing() {
        let reply = "```json\n{\"verdicts\":[\
            {\"id\":\"a\",\"state\":\"reverted\",\"reason\":\"没做到\"},\
            {\"id\":\"b\",\"state\":\"applied\",\"reason\":\"不认识\"},\
            {\"id\":\"c\",\"state\":\"KEPT\",\"reason\":\"大小写\"}]}\n```";
        let review = parse_review(reply).expect("review");
        assert_eq!(review.verdicts.len(), 1);
        assert_eq!(review.verdicts[0].id, "a");
        assert_eq!(review.verdicts[0].state, ChangeState::Reverted);
        assert_eq!(review.verdicts[0].reason, "没做到");
    }

    #[test]
    fn change_ids_are_stable_and_sanitized() {
        let a = change_id(
            ChangeTarget::Persona,
            "qq:group:996980275",
            "先说半句",
            "嗯，我在。",
        );
        let b = change_id(
            ChangeTarget::Persona,
            "qq:group:996980275",
            "先说半句",
            "嗯，我在。",
        );
        assert_eq!(a, b);
        assert!(a.starts_with("persona-qq_group_996980275-"), "{a}");
        let different = change_id(
            ChangeTarget::Persona,
            "qq:group:996980275",
            "先说半句",
            "换一句",
        );
        assert_ne!(a, different);
    }

    #[test]
    fn upsert_replaces_the_same_id_and_appends_a_new_one() {
        let mut changes = vec![change(ChangeTarget::Persona, serde_json::json!({}))];
        let mut replacement = changes[0].clone();
        replacement.summary = "新的".to_string();
        upsert_change(&mut changes, replacement);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].summary, "新的");
        let other = change(ChangeTarget::Memory, serde_json::json!({}));
        upsert_change(&mut changes, other);
        assert_eq!(changes.len(), 2);
    }

    #[test]
    fn scope_normalisation_accepts_ids_and_keys() {
        assert_eq!(persona_soul_key("996980275").unwrap(), "qq:group:996980275");
        assert_eq!(persona_soul_key("qq:group:1").unwrap(), "qq:group:1");
        assert_eq!(
            persona_soul_key("feishu:chat:oc_x").unwrap(),
            "feishu:chat:oc_x"
        );
        assert_eq!(
            persona_soul_key("ou_abc#private").unwrap(),
            "ou_abc#private"
        );
        assert_eq!(persona_soul_key("plain").unwrap(), "plain");
        assert!(persona_soul_key(" ").unwrap_err().contains("no scope"));
        assert_eq!(group_scope(" 12 ", ChangeTarget::Policy).unwrap(), "12");
        assert_eq!(
            group_scope("qq:group:12", ChangeTarget::Memory).unwrap(),
            "12"
        );
        assert!(group_scope("", ChangeTarget::Memory)
            .unwrap_err()
            .contains("no scope"));
    }

    #[test]
    fn temp_paths_and_error_texts_are_directly_testable() {
        assert_eq!(
            temp_path(Path::new("/tmp/x.json")).unwrap(),
            PathBuf::from("/tmp/x.json.tmp")
        );
        assert!(temp_path(Path::new("/"))
            .unwrap_err()
            .contains("no file name"));
        let io = std::io::Error::other("boom");
        assert_eq!(
            write_error("write", Path::new("/tmp/x"), &io),
            "write /tmp/x: boom"
        );
        let json = serde_json::from_str::<serde_json::Value>("{").unwrap_err();
        assert!(json_encode_error(Path::new("/tmp/x"), &json).contains("encode /tmp/x"));
    }

    #[test]
    fn atomic_writes_leave_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("x.json");
        atomic_write_text(&path, "hello").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
        assert!(!path.with_file_name("x.json.tmp").exists());
    }

    #[test]
    fn atomic_write_reports_each_failure_stage() {
        let dir = tempfile::tempdir().unwrap();

        let blocked = dir.path().join("blocked");
        std::fs::write(&blocked, "x").unwrap();
        let create = atomic_write_text(&blocked.join("x.json"), "x").unwrap_err();
        assert!(create.contains("create"), "{create}");

        let target = dir.path().join("occupied.json");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("keep"), "x").unwrap();
        let rename = atomic_write_text(&target, "x").unwrap_err();
        assert!(rename.contains("rename"), "{rename}");
        assert!(!dir.path().join("occupied.json.tmp").exists());

        let as_dir = dir.path().join("as-dir.json");
        std::fs::create_dir(&as_dir).unwrap();
        std::fs::write(as_dir.join("keep"), "x").unwrap();
        let write = atomic_write_text(&as_dir, "x").unwrap_err();
        assert!(write.contains("rename"), "{write}");

        let tmp_blocked = dir.path().join("tmp-blocked.json");
        std::fs::create_dir(dir.path().join("tmp-blocked.json.tmp")).unwrap();
        let write = atomic_write_text(&tmp_blocked, "x").unwrap_err();
        assert!(write.contains("write"), "{write}");
    }

    #[test]
    fn atomic_write_handles_a_path_with_no_filename() {
        assert!(atomic_write_text(Path::new("/"), "x")
            .unwrap_err()
            .contains("no file name"));
    }

    #[test]
    fn reading_a_json_object_distinguishes_missing_broken_and_wrong_shape() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            read_json_object(&dir.path().join("none.json"), "x").unwrap(),
            serde_json::json!({})
        );
        let broken = dir.path().join("broken.json");
        std::fs::write(&broken, "{").unwrap();
        assert!(read_json_object(&broken, "x")
            .unwrap_err()
            .contains("parse"));
        let array = dir.path().join("array.json");
        std::fs::write(&array, "[]").unwrap();
        assert!(read_json_object(&array, "x")
            .unwrap_err()
            .contains("not a JSON object"));
        let directory = dir.path().join("dir");
        std::fs::create_dir(&directory).unwrap();
        assert!(read_json_object(&directory, "x")
            .unwrap_err()
            .contains("read"));
    }

    #[test]
    fn a_persona_sentence_that_vanishes_without_a_drop_is_reported() {
        // Everything still present: nothing to declare.
        assert!(dropped_without_leave("甲。乙！", "甲。乙！丙。", &[]).is_empty());
        // One sentence gone and not declared: it is reported verbatim.
        assert_eq!(
            dropped_without_leave("甲。乙。", "甲。", &[]),
            vec!["乙。".to_string()]
        );
        // Declared without its punctuation: the drop is a substring of the
        // sentence, so it counts as declared.
        assert!(
            dropped_without_leave("别顶回去。看截图。", "看截图。", &["别顶回去".to_string()])
                .is_empty()
        );
        // The declaration may also be longer than the sentence.
        assert!(dropped_without_leave("乙。", "甲。", &["甲。乙。".to_string()]).is_empty());
        // Whitespace differences do not matter.
        assert!(dropped_without_leave("甲。 乙。", "甲。", &[" 乙 ".to_string()]).is_empty());
        // An empty declaration cannot silently cover every sentence.
        assert_eq!(
            dropped_without_leave("甲。", "乙。", &["".to_string(), "  ".to_string()]),
            vec!["甲。".to_string()]
        );
        // No old persona means no rule can be lost.
        assert!(dropped_without_leave("", "甲。", &[]).is_empty());
        // An empty new persona drops every old sentence.
        assert_eq!(
            dropped_without_leave("甲。乙。", "", &[]),
            vec!["甲。".to_string(), "乙。".to_string()]
        );
        // All three sentence separators plus newlines, and a final sentence
        // without punctuation.
        assert_eq!(
            dropped_without_leave("甲！乙？丙\n丁", "", &[]),
            vec![
                "甲！".to_string(),
                "乙？".to_string(),
                "丙".to_string(),
                "丁".to_string()
            ]
        );
        // Blank lines between sentences are not sentences of their own.
        assert_eq!(
            dropped_without_leave("\n甲。\n\n", "", &[]),
            vec!["甲。".to_string()]
        );
    }

    #[test]
    fn persona_changes_are_backed_up_before_they_are_written() {
        let dir = tempfile::tempdir().unwrap();
        let souls = TestSouls::default();
        souls
            .set_persona("qq:group:1", "旧句子。仍然成立。")
            .unwrap();
        let change = apply_proposed(
            dir.path(),
            &proposed(ChangeTarget::Persona, "1", "旧句子。仍然成立。新句子。"),
            5,
            &souls,
        )
        .unwrap();
        assert_eq!(change.state, ChangeState::Applied);
        assert_eq!(
            souls.persona("qq:group:1").unwrap().unwrap(),
            "旧句子。仍然成立。新句子。"
        );
        let rollback = &change.rollback;
        assert_eq!(rollback["persona"], "旧句子。仍然成立。");
        let backup = dir
            .path()
            .join("changes-backup")
            .join(format!("{}-5.json", change.id));
        assert!(std::fs::read_to_string(backup)
            .unwrap()
            .contains("旧句子。仍然成立。"));
    }

    #[test]
    fn a_declared_persona_deletion_is_applied() {
        let dir = tempfile::tempdir().unwrap();
        let souls = TestSouls::default();
        souls
            .set_persona("qq:group:1", "别顶回去。看截图。")
            .unwrap();
        let mut proposal = proposed(ChangeTarget::Persona, "1", "看截图。");
        proposal.drops = vec!["别顶回去".to_string()];
        apply_proposed(dir.path(), &proposal, 5, &souls).unwrap();
        assert_eq!(souls.persona("qq:group:1").unwrap().unwrap(), "看截图。");
    }

    #[test]
    fn an_undeclared_persona_deletion_is_refused_without_writing_anything() {
        let dir = tempfile::tempdir().unwrap();
        let souls = TestSouls::default();
        souls
            .set_persona("qq:group:1", "别顶回去。看截图。")
            .unwrap();
        let proposal = proposed(ChangeTarget::Persona, "1", "看截图。");
        let id = change_id(
            proposal.target,
            &proposal.scope,
            &proposal.summary,
            &proposal.value,
        );
        let error = apply_proposed(dir.path(), &proposal, 5, &souls).unwrap_err();
        assert_eq!(
            error,
            format!("persona change {id} drops rules without saying so: 别顶回去。")
        );
        // Refused before the backup: nothing was written and the persona is
        // exactly what it was.
        assert!(!dir.path().join("changes-backup").exists());
        assert_eq!(
            souls.persona("qq:group:1").unwrap().unwrap(),
            "别顶回去。看截图。"
        );
    }

    #[test]
    fn an_empty_persona_value_deletes_everything_only_when_declared() {
        let dir = tempfile::tempdir().unwrap();
        let souls = TestSouls::default();
        souls.set_persona("qq:group:1", "甲。乙！").unwrap();
        let mut proposal = proposed(ChangeTarget::Persona, "1", "");
        proposal.drops = vec!["甲。".to_string(), "乙！".to_string()];
        apply_proposed(dir.path(), &proposal, 5, &souls).unwrap();
        assert_eq!(souls.persona("qq:group:1").unwrap().unwrap(), "");
    }

    #[test]
    fn a_persona_change_without_a_scope_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let souls = TestSouls::default();
        let error = apply_proposed(
            dir.path(),
            &proposed(ChangeTarget::Persona, " ", "x"),
            5,
            &souls,
        )
        .unwrap_err();
        assert!(error.contains("no scope"), "{error}");
    }

    #[test]
    fn persona_read_and_write_failures_bubble_up() {
        let dir = tempfile::tempdir().unwrap();
        let error = apply_proposed(
            dir.path(),
            &proposed(ChangeTarget::Persona, "1", "x"),
            5,
            &BrokenSouls,
        )
        .unwrap_err();
        assert_eq!(error, "read failed");

        struct ReadOnly(TestSouls);
        impl PersonaSink for ReadOnly {
            fn persona(&self, key: &str) -> Result<Option<String>, String> {
                self.0.persona(key)
            }

            fn set_persona(&self, _key: &str, _persona: &str) -> Result<(), String> {
                Err("write failed".to_string())
            }
        }
        let error = apply_proposed(
            dir.path(),
            &proposed(ChangeTarget::Persona, "1", "x"),
            5,
            &ReadOnly(TestSouls::default()),
        )
        .unwrap_err();
        assert_eq!(error, "write failed");
    }

    #[test]
    fn a_persona_backup_failure_happens_before_the_soul_is_touched() {
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("blocked");
        std::fs::write(&blocked, "x").unwrap();
        let souls = TestSouls::default();
        let error = apply_proposed(
            &blocked,
            &proposed(ChangeTarget::Persona, "1", "x"),
            5,
            &souls,
        )
        .unwrap_err();
        assert!(error.contains("create"), "{error}");
        assert!(souls.persona("qq:group:1").unwrap().is_none());
    }

    #[test]
    fn policy_changes_touch_only_the_policy_field() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("qq_reply_policies.json");
        std::fs::write(
            &path,
            r#"{"default":{"enabled":false,"policy":""},"groups":{"12":{"enabled":true,"policy":"旧策略","updated_by":"operator"}}}"#,
        )
        .unwrap();
        let change = apply_proposed(
            dir.path(),
            &proposed(ChangeTarget::Policy, "qq:group:12", "新策略"),
            7,
            &TestSouls::default(),
        )
        .unwrap();
        let stored: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(stored["groups"]["12"]["policy"], "新策略");
        assert_eq!(stored["groups"]["12"]["enabled"], true);
        assert_eq!(stored["groups"]["12"]["updated_by"], "selfcheck");
        assert_eq!(change.rollback["policy"], "旧策略");
    }

    #[test]
    fn a_policy_change_creates_a_missing_group_and_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let change = apply_proposed(
            dir.path(),
            &proposed(ChangeTarget::Policy, "12", "新策略"),
            7,
            &TestSouls::default(),
        )
        .unwrap();
        assert_eq!(change.rollback["existed"], false);
        let stored: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("qq_reply_policies.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(stored["groups"]["12"]["policy"], "新策略");
    }

    #[test]
    fn a_policy_change_refuses_corrupt_or_misshapen_files() {
        for (name, body) in [
            ("broken.json", "{"),
            ("array.json", "[]"),
            ("groups.json", r#"{"groups":[]}"#),
            ("entry.json", r#"{"groups":{"12":[]}}"#),
        ] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("qq_reply_policies.json"), body).unwrap();
            let error = apply_proposed(
                dir.path(),
                &proposed(ChangeTarget::Policy, "12", "x"),
                1,
                &TestSouls::default(),
            )
            .unwrap_err();
            assert!(!error.is_empty(), "{name} should fail");
        }
    }

    #[test]
    fn memory_changes_append_to_the_group_note() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("qq_group_memory.json");
        std::fs::write(
            &path,
            r#"{"12":{"memory":"旧记忆","updated_at_ms":1,"updated_by":"agent"}}"#,
        )
        .unwrap();
        let change = apply_proposed(
            dir.path(),
            &proposed(ChangeTarget::Memory, "12", "新记忆"),
            8,
            &TestSouls::default(),
        )
        .unwrap();
        let stored: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(stored["12"]["memory"], "旧记忆\n新记忆");
        assert_eq!(stored["12"]["updated_by"], "selfcheck");
        assert_eq!(change.rollback["memory"], "旧记忆");
    }

    #[test]
    fn memory_changes_create_a_missing_group_and_handle_an_empty_note() {
        let dir = tempfile::tempdir().unwrap();
        let change = apply_proposed(
            dir.path(),
            &proposed(ChangeTarget::Memory, "12", "第一条"),
            8,
            &TestSouls::default(),
        )
        .unwrap();
        assert_eq!(change.rollback["existed"], false);
        let stored: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("qq_group_memory.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(stored["12"]["memory"], "第一条");
    }

    #[test]
    fn memory_changes_refuse_corrupt_or_misshapen_files() {
        for body in ["{", "[]", r#"{"12":[]}"#] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("qq_group_memory.json"), body).unwrap();
            let error = apply_proposed(
                dir.path(),
                &proposed(ChangeTarget::Memory, "12", "x"),
                1,
                &TestSouls::default(),
            )
            .unwrap_err();
            assert!(!error.is_empty());
        }
    }

    #[test]
    fn a_code_change_is_never_applied_without_the_gate() {
        let dir = tempfile::tempdir().unwrap();
        let error = apply_proposed(
            dir.path(),
            &proposed(ChangeTarget::Code, "1", "x"),
            1,
            &TestSouls::default(),
        )
        .unwrap_err();
        assert!(error.contains("gate"), "{error}");
    }

    #[test]
    fn applying_a_review_keeps_going_after_one_bad_change() {
        let dir = tempfile::tempdir().unwrap();
        let proposals = vec![
            proposed(ChangeTarget::Policy, "1", "新"),
            proposed(ChangeTarget::Memory, " ", "坏"),
            proposed(ChangeTarget::Code, "1", "x"),
            proposed(ChangeTarget::Capability, "", "看视频"),
        ];
        let outcome = apply_review_data(dir.path(), &proposals, 3, &TestSouls::default());
        assert_eq!(outcome.applied.len(), 1);
        assert_eq!(outcome.notes.len(), 3);
        assert!(outcome.notes[0].contains("no scope"), "{:?}", outcome.notes);
        assert!(outcome.notes[1].contains("gate"), "{:?}", outcome.notes);
        assert!(
            outcome.notes[2].contains("capability change"),
            "{:?}",
            outcome.notes
        );
    }

    #[test]
    fn reverting_a_persona_restores_the_old_sentence() {
        let souls = TestSouls::default();
        souls.set_persona("qq:group:1", "新句子").unwrap();
        revert_change(
            Path::new("/tmp"),
            &change(
                ChangeTarget::Persona,
                serde_json::json!({"soul_key": "qq:group:1", "persona": "旧句子"}),
            ),
            &souls,
            &RecordingReverter::default(),
        )
        .unwrap();
        assert_eq!(souls.persona("qq:group:1").unwrap().unwrap(), "旧句子");
    }

    #[test]
    fn a_persona_rollback_without_a_key_is_an_error() {
        let error = revert_change(
            Path::new("/tmp"),
            &change(ChangeTarget::Persona, serde_json::json!({})),
            &TestSouls::default(),
            &RecordingReverter::default(),
        )
        .unwrap_err();
        assert!(error.contains("soul_key"), "{error}");
    }

    #[test]
    fn reverting_policy_restores_existing_and_removes_new_groups() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("qq_reply_policies.json");
        std::fs::write(
            &path,
            r#"{"default":{"enabled":true},"groups":{"12":{"enabled":true,"policy":"新策略"},"13":{"enabled":true,"policy":"新策略"}}}"#,
        )
        .unwrap();
        revert_change(
            dir.path(),
            &change(
                ChangeTarget::Policy,
                serde_json::json!({"scope": "12", "policy": "旧策略", "existed": true}),
            ),
            &TestSouls::default(),
            &RecordingReverter::default(),
        )
        .unwrap();
        revert_change(
            dir.path(),
            &change(
                ChangeTarget::Policy,
                serde_json::json!({"scope": "13", "policy": "", "existed": false}),
            ),
            &TestSouls::default(),
            &RecordingReverter::default(),
        )
        .unwrap();
        let stored: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(stored["groups"]["12"]["policy"], "旧策略");
        assert!(stored["groups"].get("13").is_none());
    }

    #[test]
    fn a_policy_rollback_reports_missing_or_misshapen_groups() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("qq_reply_policies.json");
        std::fs::write(&path, r#"{"groups":[]}"#).unwrap();
        assert!(revert_change(
            dir.path(),
            &change(
                ChangeTarget::Policy,
                serde_json::json!({"scope": "12", "policy": "旧", "existed": true})
            ),
            &TestSouls::default(),
            &RecordingReverter::default(),
        )
        .unwrap_err()
        .contains("not an object"));

        std::fs::write(&path, r#"{}"#).unwrap();
        assert!(revert_change(
            dir.path(),
            &change(
                ChangeTarget::Policy,
                serde_json::json!({"scope": "12", "policy": "旧", "existed": true})
            ),
            &TestSouls::default(),
            &RecordingReverter::default(),
        )
        .unwrap_err()
        .contains("not an object"));

        std::fs::write(&path, r#"{"groups":{"12":[]}}"#).unwrap();
        assert!(revert_change(
            dir.path(),
            &change(
                ChangeTarget::Policy,
                serde_json::json!({"scope": "12", "policy": "旧", "existed": true})
            ),
            &TestSouls::default(),
            &RecordingReverter::default(),
        )
        .unwrap_err()
        .contains("not an object"));

        std::fs::write(&path, "[]").unwrap();
        assert!(revert_change(
            dir.path(),
            &change(
                ChangeTarget::Policy,
                serde_json::json!({"scope": "12", "policy": "旧", "existed": true})
            ),
            &TestSouls::default(),
            &RecordingReverter::default(),
        )
        .unwrap_err()
        .contains("not a JSON object"));

        let missing = revert_change(
            dir.path(),
            &change(
                ChangeTarget::Policy,
                serde_json::json!({"scope": "12", "existed": true}),
            ),
            &TestSouls::default(),
            &RecordingReverter::default(),
        )
        .unwrap_err();
        assert!(missing.contains("policy"), "{missing}");
    }

    #[test]
    fn reverting_memory_restores_existing_and_removes_new_groups() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("qq_group_memory.json");
        std::fs::write(
            &path,
            r#"{"12":{"memory":"新记忆"},"13":{"memory":"新记忆"}}"#,
        )
        .unwrap();
        revert_change(
            dir.path(),
            &change(
                ChangeTarget::Memory,
                serde_json::json!({"scope": "12", "memory": "旧记忆", "existed": true}),
            ),
            &TestSouls::default(),
            &RecordingReverter::default(),
        )
        .unwrap();
        revert_change(
            dir.path(),
            &change(
                ChangeTarget::Memory,
                serde_json::json!({"scope": "13", "memory": "", "existed": false}),
            ),
            &TestSouls::default(),
            &RecordingReverter::default(),
        )
        .unwrap();
        let stored: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(stored["12"]["memory"], "旧记忆");
        assert!(stored.get("13").is_none());
    }

    #[test]
    fn a_memory_rollback_reports_misshapen_groups() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("qq_group_memory.json");
        std::fs::write(&path, r#"{"12":[]}"#).unwrap();
        assert!(revert_change(
            dir.path(),
            &change(
                ChangeTarget::Memory,
                serde_json::json!({"scope": "12", "memory": "旧", "existed": true})
            ),
            &TestSouls::default(),
            &RecordingReverter::default(),
        )
        .unwrap_err()
        .contains("not an object"));

        std::fs::write(&path, "[]").unwrap();
        assert!(revert_change(
            dir.path(),
            &change(
                ChangeTarget::Memory,
                serde_json::json!({"scope": "12", "memory": "旧", "existed": true})
            ),
            &TestSouls::default(),
            &RecordingReverter::default(),
        )
        .unwrap_err()
        .contains("not a JSON object"));

        let missing = revert_change(
            dir.path(),
            &change(
                ChangeTarget::Memory,
                serde_json::json!({"scope": "12", "existed": true}),
            ),
            &TestSouls::default(),
            &RecordingReverter::default(),
        )
        .unwrap_err();
        assert!(missing.contains("memory"), "{missing}");
    }

    #[test]
    fn reverting_code_applies_the_patch_reverse() {
        let reverter = RecordingReverter::default();
        let item = change(
            ChangeTarget::Code,
            serde_json::json!({"patch": "proposals/code-1-abc/patch.diff"}),
        );
        revert_change(Path::new("/data"), &item, &TestSouls::default(), &reverter).unwrap();
        assert_eq!(
            reverter.seen.borrow().as_slice(),
            &[PathBuf::from("/data/proposals/code-1-abc/patch.diff")]
        );

        let reverter = RecordingReverter::default();
        let absolute = change(
            ChangeTarget::Code,
            serde_json::json!({"patch": "/tmp/patch.diff"}),
        );
        revert_change(
            Path::new("/data"),
            &absolute,
            &TestSouls::default(),
            &reverter,
        )
        .unwrap();
        assert_eq!(
            reverter.seen.borrow().as_slice(),
            &[PathBuf::from("/tmp/patch.diff")]
        );
    }

    #[test]
    fn a_code_rollback_without_a_patch_is_an_error() {
        let error = revert_change(
            Path::new("/data"),
            &change(ChangeTarget::Code, serde_json::json!({})),
            &TestSouls::default(),
            &RecordingReverter::default(),
        )
        .unwrap_err();
        assert!(error.contains("patch"), "{error}");
    }

    #[test]
    fn kept_verdicts_mark_the_change_without_reverting() {
        let souls = TestSouls::default();
        souls.set_persona("qq:group:1", "新句子").unwrap();
        let mut changes = vec![change(
            ChangeTarget::Persona,
            serde_json::json!({"soul_key": "qq:group:1", "persona": "旧句子"}),
        )];
        let id = changes[0].id.clone();
        let notes = reconcile_verdicts(
            Path::new("/tmp"),
            &mut changes,
            &[Judgement {
                id,
                state: ChangeState::Kept,
                reason: "好".to_string(),
            }],
            &souls,
            &RecordingReverter::default(),
        );
        assert!(notes.is_empty());
        assert_eq!(changes[0].state, ChangeState::Kept);
        assert_eq!(souls.persona("qq:group:1").unwrap().unwrap(), "新句子");
    }

    #[test]
    fn reverted_verdicts_restore_and_mark_each_data_lane() {
        let dir = tempfile::tempdir().unwrap();
        let souls = TestSouls::default();
        souls.set_persona("qq:group:1", "新句子").unwrap();
        std::fs::write(
            dir.path().join("qq_reply_policies.json"),
            r#"{"groups":{"1":{"policy":"新策略"}}}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("qq_group_memory.json"),
            r#"{"1":{"memory":"新记忆"}}"#,
        )
        .unwrap();
        let mut changes = vec![
            change(
                ChangeTarget::Persona,
                serde_json::json!({"soul_key": "qq:group:1", "persona": "旧句子"}),
            ),
            change(
                ChangeTarget::Policy,
                serde_json::json!({"scope": "1", "policy": "旧策略", "existed": true}),
            ),
            change(
                ChangeTarget::Memory,
                serde_json::json!({"scope": "1", "memory": "旧记忆", "existed": true}),
            ),
        ];
        changes[0].id = "persona-1".to_string();
        changes[1].id = "policy-1".to_string();
        changes[2].id = "memory-1".to_string();
        let judgements: Vec<Judgement> = changes
            .iter()
            .map(|item| Judgement {
                id: item.id.clone(),
                state: ChangeState::Reverted,
                reason: "没做到".to_string(),
            })
            .collect();
        let notes = reconcile_verdicts(
            dir.path(),
            &mut changes,
            &judgements,
            &souls,
            &RecordingReverter::default(),
        );
        assert!(notes.is_empty(), "{notes:?}");
        assert!(changes
            .iter()
            .all(|item| item.state == ChangeState::Reverted));
        assert_eq!(souls.persona("qq:group:1").unwrap().unwrap(), "旧句子");
        let policies: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("qq_reply_policies.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(policies["groups"]["1"]["policy"], "旧策略");
        let memory: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(dir.path().join("qq_group_memory.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(memory["1"]["memory"], "旧记忆");
    }

    #[test]
    fn unknown_already_settled_and_sentinel_verdicts_are_noted_not_applied() {
        let dir = tempfile::tempdir().unwrap();
        let mut changes = vec![change(
            ChangeTarget::Persona,
            serde_json::json!({"soul_key": "qq:group:1", "persona": "旧"}),
        )];
        changes[0].id = "a".to_string();
        let unknown = Judgement {
            id: "missing".to_string(),
            state: ChangeState::Kept,
            reason: String::new(),
        };
        let mut settled = changes[0].clone();
        settled.state = ChangeState::Kept;
        let mut settled_changes = vec![settled];
        let notes = reconcile_verdicts(
            dir.path(),
            &mut settled_changes,
            &[Judgement {
                id: "a".to_string(),
                state: ChangeState::Kept,
                reason: String::new(),
            }],
            &TestSouls::default(),
            &RecordingReverter::default(),
        );
        assert!(notes[0].contains("already kept"), "{notes:?}");

        let notes = reconcile_verdicts(
            dir.path(),
            &mut changes,
            &[
                unknown,
                Judgement {
                    id: "a".to_string(),
                    state: ChangeState::Applied,
                    reason: String::new(),
                },
            ],
            &TestSouls::default(),
            &RecordingReverter::default(),
        );
        assert_eq!(notes.len(), 2);
        assert!(notes[0].contains("unknown change"), "{notes:?}");
        assert!(notes[1].contains("neither kept nor reverted"), "{notes:?}");
    }

    #[test]
    fn a_failed_restore_leaves_the_change_applied() {
        let dir = tempfile::tempdir().unwrap();
        let mut changes = vec![change(
            ChangeTarget::Persona,
            serde_json::json!({"soul_key": "qq:group:1", "persona": "旧"}),
        )];
        changes[0].id = "a".to_string();
        let notes = reconcile_verdicts(
            dir.path(),
            &mut changes,
            &[Judgement {
                id: "a".to_string(),
                state: ChangeState::Reverted,
                reason: String::new(),
            }],
            &BrokenSouls,
            &RecordingReverter::default(),
        );
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("could not revert"), "{notes:?}");
        assert_eq!(changes[0].state, ChangeState::Applied);
    }

    #[test]
    fn a_failed_code_revert_is_noted() {
        let dir = tempfile::tempdir().unwrap();
        let mut changes = vec![change(
            ChangeTarget::Code,
            serde_json::json!({"patch": "proposals/code-1-abc/patch.diff"}),
        )];
        changes[0].id = "c1".to_string();
        let notes = reconcile_verdicts(
            dir.path(),
            &mut changes,
            &[Judgement {
                id: "c1".to_string(),
                state: ChangeState::Reverted,
                reason: String::new(),
            }],
            &TestSouls::default(),
            &RecordingReverter {
                seen: std::cell::RefCell::new(Vec::new()),
                fail: true,
            },
        );
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("git failed"), "{notes:?}");
    }

    #[test]
    fn a_code_proposal_needs_a_patch_and_a_test() {
        let dir = tempfile::tempdir().unwrap();
        let mut proposal = proposed(ChangeTarget::Code, "1", "x");
        assert!(write_code_proposal(dir.path(), &proposal, "id")
            .unwrap_err()
            .contains("no patch"));
        proposal.patch = "diff --git a/x b/x".to_string();
        assert!(write_code_proposal(dir.path(), &proposal, "id")
            .unwrap_err()
            .contains("no package or test"));
        proposal.package = "cordis-agent".to_string();
        proposal.test = "tests::x".to_string();
        let written = write_code_proposal(dir.path(), &proposal, "id").unwrap();
        assert_eq!(written, dir.path().join("proposals").join("id"));
        assert!(written.join("patch.diff").is_file());
        let document: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(written.join("proposal.json")).unwrap())
                .unwrap();
        assert_eq!(document["signature"], "id");
        assert_eq!(document["patch"], "patch.diff");
        assert_eq!(document["test"], "tests::x");

        let blocked = dir.path().join("blocked");
        std::fs::write(&blocked, "x").unwrap();
        assert!(write_code_proposal(&blocked, &proposal, "id2")
            .unwrap_err()
            .contains("create"));
    }

    #[test]
    fn a_verified_code_change_records_where_its_patch_lives() {
        let proposal = proposed(ChangeTarget::Code, "1", "x");
        let item = code_change("code-1-abc", &proposal, 11);
        assert_eq!(item.target, ChangeTarget::Code);
        assert_eq!(item.state, ChangeState::Applied);
        assert_eq!(item.rollback["patch"], "proposals/code-1-abc/patch.diff");
    }

    #[test]
    fn a_capability_change_is_parsed_as_its_own_target() {
        let reply = "```json\n{\"changes\":[{\"target\":\"capability\",\
            \"summary\":\"能读视频\",\"expectation\":\"看懂短视频\",\
            \"value\":\"抽帧+转写\",\"scope\":\"\"}]}\n```";
        let review = parse_review(reply).expect("capability change");
        assert_eq!(review.changes.len(), 1);
        assert_eq!(review.changes[0].target, ChangeTarget::Capability);
        assert_eq!(review.changes[0].summary, "能读视频");
        assert_eq!(review.changes[0].expectation, "看懂短视频");
    }

    #[test]
    fn a_capability_without_a_patch_gets_a_report_and_a_todo() {
        let dir = tempfile::tempdir().unwrap();
        let proposal = proposed(ChangeTarget::Capability, "", "抽帧+转写");
        let id = change_id(
            ChangeTarget::Capability,
            &proposal.scope,
            &proposal.summary,
            &proposal.value,
        );
        assert!(id.starts_with("capability--"), "{id}");
        let expected = dir.path().join("proposals").join(&id);
        let staged = stage_capability(dir.path(), &proposal, &id).unwrap();
        // Formatting executes the derived `Debug` arms.
        assert!(!format!("{staged:?}").is_empty());
        assert_eq!(staged, StagedCapability::Report(expected.clone()));
        let note = std::fs::read_to_string(expected.join("report.md")).unwrap();
        assert!(note.contains("尚无实现"), "{note}");
        assert!(note.contains("一句话"), "{note}");
        assert!(note.contains("六小时内变好"), "{note}");
        assert!(!expected.join("proposal.json").exists());
        let item = capability_change(&id, &proposal, 5);
        assert_eq!(item.target, ChangeTarget::Capability);
        assert_eq!(item.state, ChangeState::Applied);
        assert_eq!(item.rollback, serde_json::json!({}));
        assert_eq!(item.applied_at_ms, 5);
        assert_eq!(item.summary, "一句话");
    }

    #[test]
    fn a_capability_with_a_patch_is_staged_for_the_gate() {
        let dir = tempfile::tempdir().unwrap();
        let mut proposal = proposed(ChangeTarget::Capability, "", "抽帧+转写");
        proposal.package = "cordis-runtime".to_string();
        proposal.test = "tests::x".to_string();
        proposal.patch = "diff --git a/x b/x".to_string();
        let id = change_id(
            ChangeTarget::Capability,
            "",
            &proposal.summary,
            &proposal.value,
        );
        let expected = dir.path().join("proposals").join(&id);
        assert_eq!(
            stage_capability(dir.path(), &proposal, &id).unwrap(),
            StagedCapability::Proposal(expected.clone())
        );
        assert!(expected.join("patch.diff").is_file());
        assert!(expected.join("proposal.json").is_file());
        assert!(!expected.join("report.md").exists());
    }

    #[test]
    fn a_capability_report_that_cannot_be_written_reports_the_path() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("proposals"), "x").unwrap();
        let error = stage_capability(
            dir.path(),
            &proposed(ChangeTarget::Capability, "", "x"),
            "capability--abc",
        )
        .unwrap_err();
        assert!(error.contains("create"), "{error}");
    }

    #[test]
    fn a_capability_change_is_never_applied_by_the_data_lane() {
        let dir = tempfile::tempdir().unwrap();
        let error = apply_proposed(
            dir.path(),
            &proposed(ChangeTarget::Capability, "1", "看视频"),
            1,
            &TestSouls::default(),
        )
        .unwrap_err();
        assert!(error.contains("capability change"), "{error}");
        assert!(error.contains("gate"), "{error}");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_reverted_capability_restores_nothing_and_a_kept_one_is_settled() {
        let dir = tempfile::tempdir().unwrap();
        let reverter = RecordingReverter::default();
        let mut changes = vec![change(ChangeTarget::Capability, serde_json::json!({}))];
        changes[0].id = "cap-reverted".to_string();
        let notes = reconcile_verdicts(
            dir.path(),
            &mut changes,
            &[Judgement {
                id: "cap-reverted".to_string(),
                state: ChangeState::Reverted,
                reason: "不值得".to_string(),
            }],
            &TestSouls::default(),
            &reverter,
        );
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(changes[0].state, ChangeState::Reverted);
        assert!(reverter.seen.borrow().is_empty());
        // Nothing was restored, so the data directory stayed empty.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);

        let mut kept = vec![change(ChangeTarget::Capability, serde_json::json!({}))];
        kept[0].id = "cap-kept".to_string();
        let notes = reconcile_verdicts(
            dir.path(),
            &mut kept,
            &[Judgement {
                id: "cap-kept".to_string(),
                state: ChangeState::Kept,
                reason: "值得".to_string(),
            }],
            &TestSouls::default(),
            &reverter,
        );
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(kept[0].state, ChangeState::Kept);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    /// The acceptance demo: one review applies a policy change, the next one
    /// says it did not work, and the old value comes back.
    #[test]
    fn a_review_applies_a_policy_change_and_a_later_verdict_rolls_it_back() {
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path();
        let policies = data_dir.join("qq_reply_policies.json");
        std::fs::write(
            &policies,
            r#"{"groups":{"42":{"enabled":true,"policy":"旧策略"}}}"#,
        )
        .unwrap();
        let changes_path = data_dir.join("changes.json");
        let reply = "复盘结论：说得太多。\n```json\n\
            {\"changes\":[{\"target\":\"policy\",\"scope\":\"42\",\
            \"summary\":\"少插话\",\"expectation\":\"接下来 6 小时同类抱怨减少\",\
            \"value\":\"只在被问时说\"}]}\n```";
        let review = parse_review(reply).expect("changes");
        let outcome = apply_review_data(data_dir, &review.changes, 100, &TestSouls::default());
        assert!(outcome.notes.is_empty(), "{:?}", outcome.notes);
        let mut changes = Change::load(&changes_path);
        for item in outcome.applied {
            upsert_change(&mut changes, item);
        }
        Change::save(&changes_path, &changes).unwrap();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].state, ChangeState::Applied);
        assert!(changes[0].id.starts_with("policy-42-"), "{}", changes[0].id);
        let backup = data_dir
            .join("changes-backup")
            .join(format!("{}-100.json", changes[0].id));
        assert!(backup.is_file(), "backup {}", backup.display());
        let stored: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&policies).unwrap()).unwrap();
        assert_eq!(stored["groups"]["42"]["policy"], "只在被问时说");
        assert_eq!(stored["groups"]["42"]["enabled"], true);

        // Next review: the expectation did not hold.
        let id = changes[0].id.clone();
        let reply = format!(
            "```json\n{{\"verdicts\":[{{\"id\":\"{id}\",\"state\":\"reverted\",\
             \"reason\":\"还是插话\"}}]}}\n```"
        );
        let review = parse_review(&reply).expect("verdict");
        let mut changes = Change::load(&changes_path);
        let notes = reconcile_verdicts(
            data_dir,
            &mut changes,
            &review.verdicts,
            &TestSouls::default(),
            &RecordingReverter::default(),
        );
        assert!(notes.is_empty(), "{notes:?}");
        assert_eq!(changes[0].state, ChangeState::Reverted);
        Change::save(&changes_path, &changes).unwrap();
        assert_eq!(Change::load(&changes_path)[0].state, ChangeState::Reverted);
        let stored: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&policies).unwrap()).unwrap();
        assert_eq!(stored["groups"]["42"]["policy"], "旧策略");
        assert_eq!(stored["groups"]["42"]["enabled"], true);
    }

    // -----------------------------------------------------------------------
    // The observer's own edges: every kind, every unreadable shape, every
    // truncation. Each of these paths has a caller in production, but only on
    // input a throwing-together test never produces.
    // -----------------------------------------------------------------------

    /// Both lookups exist for every kind; `provider_error` in particular only
    /// ever came from a real provider failure, so neither arm had a caller.
    #[test]
    fn every_kind_names_and_weighs_itself() {
        let named: Vec<(&str, usize)> = [
            Kind::Empty,
            Kind::NoToolCall,
            Kind::UnparsedDsml,
            Kind::ProviderError,
            Kind::Repeated,
        ]
        .iter()
        .map(|kind| (kind.as_str(), kind.weight()))
        .collect();
        assert_eq!(
            named,
            vec![
                ("empty", 2),
                ("no_tool_call", 4),
                ("unparsed_dsml", 5),
                ("provider_error", 3),
                ("repeated", 1),
            ]
        );
    }

    /// The digest explains each kind in prose. Only `empty` had ever been
    /// formatted, so the other four wordings were dead letters.
    #[test]
    fn the_digest_names_every_kind_it_counts() {
        let signatures: Vec<Signature> = [
            Kind::Empty,
            Kind::NoToolCall,
            Kind::UnparsedDsml,
            Kind::ProviderError,
            Kind::Repeated,
        ]
        .iter()
        .map(|kind| Signature {
            kind: *kind,
            excerpt: format!("{kind:?}"),
            count: 5,
            sessions: vec!["s".to_string()],
            first_turn: 0,
            last_turn: 4,
        })
        .collect();
        let report = SelfCheck {
            scanned_sessions: 1,
            scanned_turns: 5,
            signatures,
            lessons: Vec::new(),
            attempted: Vec::new(),
        };
        let text = digest(&report, 5).expect("a digest");
        for wording in [
            "turns that produced nothing at all",
            "replies with no tool call, so nothing was sent",
            "turns of tool markup that did not parse",
            "provider failures recorded as turns",
            "the same reply repeated verbatim",
        ] {
            assert!(text.contains(wording), "{wording} missing from {text}");
        }
    }

    #[test]
    fn a_missing_exchange_directory_reads_to_nothing() {
        assert!(recent_exchanges(Path::new("/nonexistent/selfcheck-exchanges"), 10).is_empty());
    }

    /// Every shape a sessions directory can contain: a subdirectory that looks
    /// like a snapshot, a truncated json file, and json that is not a snapshot.
    /// The two readable sessions still come back, so one bad entry cannot hide
    /// the rest.
    #[test]
    fn exchange_scanning_survives_every_unreadable_shape() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a.json", "b.json"] {
            let snapshot = serde_json::json!({
                "history": [{"role": "assistant", "content": "先说一句"}]
            });
            std::fs::write(
                dir.path().join(name),
                serde_json::to_string(&snapshot).unwrap(),
            )
            .unwrap();
        }
        // A directory whose name ends in `.json`: reading it fails.
        std::fs::create_dir(dir.path().join("c.json")).unwrap();
        std::fs::write(dir.path().join("d.json"), "{not json").unwrap();
        std::fs::write(dir.path().join("e.json"), "{}").unwrap();
        let exchanges = recent_exchanges(dir.path(), 10);
        assert_eq!(exchanges.len(), 2);
        for exchange in &exchanges {
            assert_eq!(exchange.reply, "先说一句");
        }
    }

    #[test]
    fn a_missing_review_directory_reads_to_nothing() {
        assert!(previous_reviews(Path::new("/nonexistent/selfcheck-reviews"), 5).is_empty());
    }

    /// A review that parses as json but carries no `message` key keeps its
    /// literal text: `{"action":"respond"}` is a body, not an envelope.
    #[test]
    fn a_json_review_without_a_message_is_kept_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("1.md"), r#"{"action":"respond"}"#).unwrap();
        assert_eq!(
            previous_reviews(dir.path(), 1),
            vec![r#"{"action":"respond"}"#.to_string()]
        );
    }

    /// Parses as json but is not an object, so it is not a retired call either.
    #[test]
    fn a_json_scalar_is_not_a_legacy_call() {
        assert_eq!(
            classify("123").map(|(kind, _)| kind),
            Some(Kind::NoToolCall)
        );
    }

    /// An excerpt stops growing at the limit, so one enormous failure cannot
    /// carry the whole transcript into the instruction.
    #[test]
    fn a_very_long_failure_stops_at_the_excerpt_limit() {
        let excerpt = normalise(&"a".repeat(300));
        assert_eq!(excerpt.chars().count(), 200);
    }

    /// One scan over a directory holding only shapes it must skip: a directory
    /// named like a snapshot, json without a transcript, and a transcript whose
    /// assistant turn is a call (not a failure).
    #[test]
    fn scanning_skips_what_it_cannot_use() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("a.json")).unwrap();
        std::fs::write(dir.path().join("b.json"), "{}").unwrap();
        let snapshot = serde_json::json!({
            "transcript": [
                {"kind": "user", "content": "在吗", "tools": []},
                {
                    "kind": "assistant",
                    "content": "<|DSML|calls><|DSML|invoke name=\"suspend\">",
                    "tools": [],
                },
            ]
        });
        std::fs::write(
            dir.path().join("c.json"),
            serde_json::to_string(&snapshot).unwrap(),
        )
        .unwrap();
        let report = scan(dir.path());
        assert_eq!(report.scanned_sessions, 1);
        assert_eq!(report.scanned_turns, 1);
        assert!(report.signatures.is_empty());
    }
}
