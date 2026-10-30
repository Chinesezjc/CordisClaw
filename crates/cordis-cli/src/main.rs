use cordis_runtime::agent::{AgentToolEvent, ShellAgentReply};
use cordis_runtime::context::ContextRegistry;
use cordis_runtime::host::{
    AgentSessionKind, KernelApplyRequest, KernelPluginIterationResult, RuntimeHost,
};
use cordis_runtime::kernel::auto_update::{
    AutoUpdatePlan, AutoUpdater, FilePatch, VerificationEnvelope,
};
use cordis_runtime::kernel::evaluator::VerificationInput;
use cordis_runtime::kernel::plugin_iteration::{
    KernelPluginIterationRequest, PluginIterationFinalVerdict,
};
use cordis_runtime::kernel::verifier::VerificationProfile;
use cordis_runtime::plugin::invoke::PluginInvoker;
use cordis_runtime::plugin::loader::{default_loader_config, Loader};
use cordis_tooling::{
    prepare_artifacts, rebuild_fixture_artifacts, refresh_artifact_index, sync_plugin_docs,
    PrepareMode,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Structured message envelope carried over `agent_trigger`.
///
/// Kernel/Plugin boundary: the runtime inbox must NOT know about any
/// specific protocol (QQ, Feishu, …). Source plugins encode routing
/// metadata here so the inbox can (a) shard sessions by `session_key`
/// and (b) dispatch the agent's reply back to the ORIGINATING plugin's
/// send node — without the runtime hard-coding `qq_send` or `group:`.
///
/// Parsing is tolerant: a plain (non-JSON) string from a legacy caller
/// is wrapped as `{ display = <string>, session_key = <string> }` with
/// no reply routing, so it still reaches the agent (just can't reply).
#[derive(Debug, Clone, serde::Deserialize)]
struct AgentEnvelope {
    /// Plugin to invoke for the reply, e.g. "feishu". Empty = no reply.
    #[serde(default)]
    source_plugin: String,
    /// Reply node on that plugin, e.g. "feishu_send". Empty = no reply.
    #[serde(default)]
    reply_node: String,
    /// Session sharding key, unique across sources, e.g. "feishu:chat:oc_x".
    #[serde(default)]
    session_key: String,
    /// Human-readable text fed to the agent.
    #[serde(default)]
    display: String,
    /// `target` passed to the reply node; plugin self-parses (e.g. "chat:oc_x").
    #[serde(default)]
    reply_target: String,
    /// Optional original message id for quote-reply.
    #[serde(default)]
    reply_to: Option<String>,
    /// Stable sender identity, e.g. "feishu:ou_xxx". Empty = unknown
    /// (legacy caller) — soul scoping falls back to `session_key`.
    #[serde(default)]
    sender_id: String,
    /// Conversation dimension: "private" | "group". Empty = unknown.
    #[serde(default)]
    conversation_kind: String,
    /// P4: sender's role in a group room ("owner" | "admin" | "member").
    /// Empty = the channel cannot report it (treated as a plain member).
    #[serde(default)]
    sender_role: String,
    /// Channel-provided fact: may this speaker approve a staged iteration?
    /// The plugin decides (operator allowlist / room owner / admin); the kernel
    /// only enforces it, so approval policy stays in the plugin layer.
    #[serde(default)]
    caller_can_approve: bool,
    /// 本批消息里带的图片地址（渠道中立）。kernel 自己下载并内联成
    /// `image_url` content part（见 `cordis_runtime::image_input`），所以插件只需
    /// 把图片 URL 原样交出来，不必理解多模态 wire format。
    #[serde(default)]
    images: Vec<String>,
    /// 渠道给的**一次性**上下文（如 qq 的 `[reply policy: …]` /
    /// `[group memory: …]` 行）。只随本次请求发给模型，**不进 history**：
    /// 每轮重发同一份会白烧几十万字节上下文，还会盖住真正的对话。
    #[serde(default)]
    notes: String,
}

impl AgentEnvelope {
    /// Parse a trigger payload. Falls back to treating the whole string
    /// as `display` + `session_key` when it isn't a valid envelope, so
    /// un-migrated callers still function (without reply routing).
    fn parse(raw: &str) -> Self {
        match serde_json::from_str::<AgentEnvelope>(raw) {
            Ok(env) if !env.session_key.is_empty() || !env.display.is_empty() => env,
            _ => AgentEnvelope {
                source_plugin: String::new(),
                reply_node: String::new(),
                session_key: raw.to_string(),
                display: raw.to_string(),
                reply_target: String::new(),
                reply_to: None,
                sender_id: String::new(),
                conversation_kind: String::new(),
                sender_role: String::new(),
                caller_can_approve: false,
                images: Vec::new(),
                notes: String::new(),
            },
        }
    }

    fn can_reply(&self) -> bool {
        !self.source_plugin.is_empty() && !self.reply_node.is_empty()
    }

    /// Soul scope key. Private chats are per-user (`{sender_id}#private`);
    /// group chats are per-room (the whole `session_key`), so every speaker
    /// in one room meets the same persona. Envelopes without identity
    /// (legacy callers) fall back to `session_key`.
    ///
    /// A group room has one bot character, not one per member: keying by
    /// `{sender_id}#group` made the persona flip with whoever spoke last.
    fn soul_key(&self) -> String {
        if self.sender_id.is_empty() {
            return self.session_key.clone();
        }
        if self.conversation_kind == "group" && !self.session_key.is_empty() {
            return self.session_key.clone();
        }
        format!("{}#{}", self.sender_id, self.conversation_kind)
    }

    /// Only a group owner/admin may change a room persona; an empty role
    /// (channel cannot report it) counts as a plain member.
    fn caller_can_approve_iteration(&self) -> bool {
        self.caller_can_approve
    }

    fn caller_is_admin(&self) -> bool {
        matches!(
            self.sender_role.to_ascii_lowercase().as_str(),
            "owner" | "admin"
        )
    }
}

/// M批: pending-message spill. When the LLM request exhausts every retry
/// AND the profile fallback, the user's message must not vanish — it is
/// written to `data/pending/<hash>.json` and replayed (prepended) on the
/// next message for the same session. Pure mechanical path: no LLM.
mod pending {
    use serde::{Deserialize, Serialize};
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime};

    /// WS-3: a spill older than this is treated as non-existent when it
    /// would be replayed — a message from weeks ago must never be answered
    /// as if it were current. The file is NOT deleted: `archive_stale`
    /// relocates it out of the live `pending/` directory at startup.
    pub const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

    const MS_PER_DAY: u64 = 24 * 60 * 60 * 1000;

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct PendingMessage {
        pub session_key: String,
        /// The combined user text that failed to get a response.
        pub combined: String,
        pub enqueued_at_ms: u64,
    }

    fn sanitize(key: &str) -> String {
        key.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    }

    pub fn path_for(data_dir: &Path, session_key: &str) -> PathBuf {
        data_dir
            .join("pending")
            .join(format!("{}.json", sanitize(session_key)))
    }

    /// Wall-clock milliseconds since the Unix epoch — the on-disk timestamp
    /// unit for `enqueued_at_ms`. 0 if the clock sits before the epoch.
    pub fn now_ms() -> u64 {
        to_ms(SystemTime::now())
    }

    fn to_ms(at: SystemTime) -> u64 {
        at.duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    pub fn save(data_dir: &Path, msg: &PendingMessage) {
        let path = path_for(data_dir, &msg.session_key);
        if let Some(dir) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                eprintln!("pending: create dir failed: {e}");
                return;
            }
        }
        // Merge with an already-pending message so consecutive failures
        // accumulate instead of overwriting the older text.
        let merged = match load(data_dir, &msg.session_key) {
            Some(prev) => PendingMessage {
                session_key: msg.session_key.clone(),
                combined: format!("{}\n{}", prev.combined, msg.combined),
                enqueued_at_ms: prev.enqueued_at_ms,
            },
            None => msg.clone(),
        };
        // Atomic tmp+rename, same discipline as session auto-save.
        let tmp = path.with_extension("json.tmp");
        let bytes = match serde_json::to_vec(&merged) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("pending: serialize failed: {e}");
                return;
            }
        };
        if let Err(e) = std::fs::write(&tmp, &bytes).and_then(|_| std::fs::rename(&tmp, &path)) {
            eprintln!("pending: write failed: {e}");
            return;
        }
        // WS-3: a spill used to be invisible in the logs — the 10-20 outage
        // left 700+ dead letters nobody knew about. Log every successful one
        // (chars = what actually landed on disk, i.e. after merge).
        eprintln!(
            "pending: spilled session={} chars={} path={}",
            merged.session_key,
            merged.combined.chars().count(),
            path.display()
        );
    }

    /// Load the pending spill for `session_key`.
    ///
    /// WS-3: a spill older than [`MAX_AGE`] is reported as absent (and
    /// logged) so a long-dead message can never be replayed into a fresh
    /// turn. The file is left on disk untouched — `archive_stale` owns
    /// relocation, this function never deletes.
    pub fn load(data_dir: &Path, session_key: &str) -> Option<PendingMessage> {
        let text = std::fs::read_to_string(path_for(data_dir, session_key)).ok()?;
        let msg: PendingMessage = serde_json::from_str(&text).ok()?;
        let age_ms = now_ms().saturating_sub(msg.enqueued_at_ms);
        if age_ms > MAX_AGE.as_millis() as u64 {
            eprintln!(
                "pending: ignoring stale spill session={} age_days={}",
                session_key,
                age_ms / MS_PER_DAY
            );
            return None;
        }
        Some(msg)
    }

    pub fn clear(data_dir: &Path, session_key: &str) {
        // Only a real removal logs: a successful turn with no spill in place
        // (the common case) hits NotFound and stays silent.
        if std::fs::remove_file(path_for(data_dir, session_key)).is_ok() {
            eprintln!("pending: cleared session={session_key}");
        }
    }

    /// WS-3: relocate dead letters out of the live `pending/` directory.
    ///
    /// Every `pending/*.json` whose `enqueued_at_ms` is older than
    /// `now - max_age` — plus every entry whose JSON cannot be parsed, since
    /// its age is unknowable — is `fs::rename`d into `data/pending.archive/`
    /// (created on first use). **Nothing is ever deleted**: a file whose
    /// rename fails is logged and left where it is.
    ///
    /// Destination collisions are resolved by appending a numeric suffix
    /// (`x.json` → `x.1.json` → `x.2.json` …) instead of the implicit
    /// overwrite `rename(2)` performs on Unix, so an already-archived dead
    /// letter is never displaced by a newer one.
    ///
    /// Returns the number of files archived.
    pub fn archive_stale(data_dir: &Path, max_age: Duration, now: SystemTime) -> usize {
        let pending_dir = data_dir.join("pending");
        let archive_dir = data_dir.join("pending.archive");
        let cutoff = to_ms(now).saturating_sub(max_age.as_millis() as u64);
        let entries = match std::fs::read_dir(&pending_dir) {
            Ok(entries) => entries,
            // No spill directory yet: nothing to archive.
            Err(_) => return 0,
        };
        let mut archive_dir_ready = false;
        let mut archived = 0usize;
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let stale = match std::fs::read_to_string(&path) {
                Ok(text) => match serde_json::from_str::<PendingMessage>(&text) {
                    Ok(msg) => msg.enqueued_at_ms < cutoff,
                    // Unparseable spill: age is unknowable, archive it.
                    Err(_) => true,
                },
                // Unreadable spill: archive it rather than delete it.
                Err(_) => true,
            };
            if !stale {
                continue;
            }
            if !archive_dir_ready {
                match std::fs::create_dir_all(&archive_dir) {
                    Ok(()) => archive_dir_ready = true,
                    Err(e) => {
                        eprintln!("pending: create archive dir failed: {e}");
                        break;
                    }
                }
            }
            let Some(dest) = unique_dest(&archive_dir, &path) else {
                eprintln!("pending: no free archive name for {}", path.display());
                continue;
            };
            match std::fs::rename(&path, &dest) {
                Ok(()) => archived += 1,
                Err(e) => eprintln!("pending: archive {} failed: {e}", path.display()),
            }
        }
        archived
    }

    /// First non-existing destination name for `src` inside `archive_dir`.
    /// `None` when even the numeric-suffix fallbacks are all taken.
    fn unique_dest(archive_dir: &Path, src: &Path) -> Option<PathBuf> {
        let direct = archive_dir.join(src.file_name()?);
        if !direct.exists() {
            return Some(direct);
        }
        let stem = src.file_stem()?.to_string_lossy().into_owned();
        (1..=1000)
            .map(|n| archive_dir.join(format!("{stem}.{n}.json")))
            .find(|candidate| !candidate.exists())
    }
}

/// WS-3: fixed-template notice sent to the batch's origin after an LLM
/// failure has spilled the message. Deliberately LLM-free — this is the one
/// path that must still work when the model does not.
const OUTAGE_NOTICE: &str = "我这边刚才调用模型失败了，你的消息我记下了，稍后会自动重试。";

/// WS-3: at most one outage notice per session per window — a flapping
/// provider must not turn a burst of user messages into a burst of replies.
const OUTAGE_NOTIFY_WINDOW: Duration = Duration::from_secs(10 * 60);

/// WS-3: rate-limit gate for [`OUTAGE_NOTICE`]. Returns `true` when `key`
/// has not been notified inside `window` before `now`, recording `now` so
/// later calls inside the window are throttled. Pure apart from the map, so
/// the throttle policy is unit-testable without an inbox.
fn should_notify(
    last_notified: &mut HashMap<String, Instant>,
    key: &str,
    now: Instant,
    window: Duration,
) -> bool {
    match last_notified.get(key) {
        Some(prev) if now.duration_since(*prev) < window => false,
        _ => {
            last_notified.insert(key.to_string(), now);
            true
        }
    }
}

/// Strip a channel-plugin source tag (e.g. `[feishu (user 张三)]: `) from a
/// display line, returning the trimmed user text. Falls back to the trimmed
/// original when no `]: ` marker is present (raw / legacy callers).
fn extract_user_text(display: &str) -> &str {
    display
        .rsplit_once("]: ")
        .map(|(_, t)| t)
        .unwrap_or(display)
        .trim()
}

/// The one in-band way to say "do not post anything".
///
/// "Answer with nothing" is not expressible through a chat-completions
/// provider: asked for silence, a model emits a placeholder like `(empty)`.
/// One literal token beats an envelope — the inbox recognises only this exact
/// string, and everything else is the message.
const SILENCE_TOKEN: &str = "[silence]";

/// Text one tool-call event actually delivered (the `message` it sent).
fn event_message(event: &AgentToolEvent) -> Option<&str> {
    if event.name != "invoke_plugin" {
        return None;
    }
    event
        .arguments
        .pointer("/payload_json/message")
        .and_then(Value::as_str)
        .or_else(|| event.arguments.get("message").and_then(Value::as_str))
        .map(str::trim)
        .filter(|message| !message.is_empty())
}

/// P6: collect the text the channel actually sent during this agent turn.
///
/// A reply leaves the process through a tool call
/// (`invoke_plugin(qq, qq_send, {message})`) or through the assistant's final
/// text. The inbox records it into history so the next turn can see what the
/// bot told the room.
fn outbound_replies(content: &str, tool_events: &[AgentToolEvent]) -> Vec<String> {
    let mut replies: Vec<String> = tool_events
        .iter()
        .filter_map(event_message)
        .map(str::to_string)
        .collect();
    let text = content.trim();
    if !text.is_empty() && !replies.iter().any(|sent| sent == text) {
        replies.push(text.to_string());
    }
    replies
}

/// Was this exact text already delivered through `reply_node` in this turn?
///
/// The model can send an image (or an @ message) with a tool call and then
/// answer in text; only a byte-identical repeat is suppressed, so a caption is
/// still delivered.
fn already_sent(text: &str, events: &[AgentToolEvent], reply_node: &str) -> bool {
    events.iter().any(|event| {
        event.ok
            && event.arguments.get("node_id").and_then(Value::as_str) == Some(reply_node)
            && event_message(event) == Some(text)
    })
}

/// A batch envelope classified by whether its user text is a `/command`.
/// Commands bypass the LLM; normals are combined and sent to the agent.
enum BatchItem {
    Command(AgentEnvelope),
    Normal(AgentEnvelope),
}

/// Classify each envelope in a batch (order preserved): `/`-prefixed user
/// text becomes a `Command`, everything else a `Normal`. M2 fix — a command
/// anywhere in a mixed batch no longer discards the batch's normal messages.
fn partition_batch(envs: Vec<AgentEnvelope>) -> Vec<BatchItem> {
    envs.into_iter()
        .map(|env| {
            if extract_user_text(&env.display).starts_with('/') {
                BatchItem::Command(env)
            } else {
                BatchItem::Normal(env)
            }
        })
        .collect()
}

/// Wrap the combined inbox batch in explicit START/END markers plus a
/// scope instruction, so the agent treats only these lines as the current
/// turn's new events (older chat history stays context-only).
///
/// Channel-neutral by design: the runtime inbox must NOT know about any
/// specific protocol (see the `AgentEnvelope` doc comment on the
/// Kernel/Plugin boundary). The marker tokens and prose therefore say
/// "INCOMING_BATCH" rather than naming QQ/Feishu/any source — a source
/// plugin's identity never leaks into runtime-level framing.
///
/// The caller wraps the FINAL combined string (after any outage spill has
/// been prepended). Storage paths (pending spill, logs) keep the raw
/// unwrapped `combined`; the wrap is applied only at the send site, so a
/// replayed message is wrapped exactly once and never nested.
fn wrap_inbox_batch(combined: &str) -> String {
    format!(
        "CURRENT_INCOMING_BATCH_START\n{combined}\nCURRENT_INCOMING_BATCH_END\n\n\
         The messages between START and END are the new events for this turn. \
         Use the earlier conversation as the basis for judging them: who has been talking to whom, \
         whether this continues a thread you are part of, and whether a reply would actually help. \
         Do not re-answer an older message as if it were a new request, \
         and do not claim someone is asking you a question unless the question is in this batch or in the thread it continues. \
         Whether to answer is decided by the `[reply policy: ...]` line (see the plugin instructions) plus that context — \
         if the message is clearly between other people, answer with exactly `[silence]` and nothing else."
    )
}

/// Parse only the FIRST JSON value from `s`, ignoring any trailing content
/// (e.g. DSML XML or stray prose the model appended after its JSON verdict).
/// `serde_json::from_str` rejects trailing bytes, so a `Deserializer` that
/// consumes a single value is used instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServeMode {
    Command,
    AgentChat,
    ShellConsole,
}

struct ServeState {
    agent_session_id: String,
    mode: ServeMode,
}

/// P1-12: `static mut` is a data race waiting to happen (unsynchronised
/// concurrent access from the plugin thread that raises the trigger and
/// the main-loop thread that reads the receiver end). `OnceLock` gives us
/// initialise-once + safe concurrent read semantics without unsafe.
///
/// Bounded (`SyncSender`, capacity below): the inbox consumer blocks on
/// LLM round-trips, so an unbounded channel would grow without limit
/// whenever DeepSeek is slower than the QQ message rate. On overflow we
/// drop the NEWEST message (try_send fails) and count it — the qq plugin
/// already deduplicates and batches upstream, and a bounded loss under
/// sustained overload beats unbounded memory growth.
static AGENT_TRIGGER_TX: std::sync::OnceLock<std::sync::mpsc::SyncSender<String>> =
    std::sync::OnceLock::new();
const AGENT_TRIGGER_CAPACITY: usize = 256;
static AGENT_TRIGGER_DROPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// C ABI entry point plugins call to push a message into the agent inbox.
///
/// # Safety
///
/// The caller must pass either a null pointer (handled here as a no-op) or a
/// pointer to a valid, NUL-terminated C string that stays live for the
/// duration of this call. Passing a dangling, misaligned, or non-NUL-terminated
/// pointer is undefined behaviour.
#[no_mangle]
pub unsafe extern "C" fn _cordis_agent_trigger(msg: *const std::ffi::c_char) {
    if msg.is_null() {
        return;
    }
    let s = unsafe { std::ffi::CStr::from_ptr(msg).to_string_lossy().to_string() };
    if let Some(tx) = AGENT_TRIGGER_TX.get() {
        if tx.try_send(s).is_err() {
            let dropped =
                AGENT_TRIGGER_DROPPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            // Log every drop at first, then only every 50th so a sustained
            // flood doesn't turn stderr into its own overload problem.
            if dropped <= 5 || dropped.is_multiple_of(50) {
                eprintln!(
                    "agent-trigger: inbox full ({AGENT_TRIGGER_CAPACITY}), \
                     dropped {dropped} message(s) total"
                );
            }
        }
    }
}

extern "C" fn sigterm_to_sigint(_sig: libc::c_int) {
    unsafe {
        libc::raise(libc::SIGINT);
    }
}

/// P1-12: install SIGTERM → SIGINT forwarding using `sigaction(2)`, the
/// modern POSIX signal API. The old `libc::signal(SIGTERM, handler)` uses
/// the legacy System V / BSD-divergent `signal(2)`, whose interaction with
/// multithreaded processes is implementation-defined. `sigaction` gives us
/// a portable, well-specified install.
fn install_sigterm_handler() {
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = sigterm_to_sigint as *const () as usize;
        libc::sigemptyset(&mut action.sa_mask);
        action.sa_flags = libc::SA_RESTART;
        let _ = libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut());
    }
}

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();

    // Install the build backend before anything boots. The runtime only holds
    // the `PluginBuilder` seam (`runtime` does not link build tooling), while
    // `serve` / `auto-update` reach that seam at any time: iteration stage 3,
    // the `rebuild_plugin_workspace` agent tool, and crash-recovery restore.
    // `--runtime-only` installs too — self-iteration may need it later. A
    // duplicate install only returns `false`, never a failure, so ignore it.
    let _ = cordis_runtime::plugin::builder::install_plugin_builder(Box::new(
        cordis_tooling::CargoBuilder,
    ));

    if args.first().map(|x| x.as_str()) == Some("auto-update") {
        if let Err(err) = run_auto_update(&args[1..]) {
            eprintln!("auto-update failed: {err}");
            eprintln!("{}", usage());
            std::process::exit(1);
        }
        return;
    }
    if args.first().map(|x| x.as_str()) == Some("serve") {
        if let Err(err) = run_serve(&args[1..]) {
            eprintln!("serve failed: {err}");
            eprintln!("{}", usage());
            std::process::exit(1);
        }
        return;
    }
    if args.first().map(|x| x.as_str()) == Some("invoke") {
        if let Err(err) = run_invoke(&args[1..]) {
            eprintln!("invoke failed: {err}");
            eprintln!("{}", usage());
            std::process::exit(1);
        }
        return;
    }
    if args.first().map(|x| x.as_str()) == Some("execute") {
        if let Err(err) = run_execute(&args[1..]) {
            eprintln!("execute failed: {err}");
            eprintln!("{}", usage());
            std::process::exit(1);
        }
        return;
    }
    if args.first().map(|x| x.as_str()) == Some("reload") {
        if let Err(err) = run_reload_command(&args[1..]) {
            eprintln!("reload failed: {err}");
            std::process::exit(1);
        }
        return;
    }
    if args.first().map(|x| x.as_str()) == Some("gc") {
        if let Err(err) = run_gc(&args[1..]) {
            eprintln!("gc failed: {err}");
            eprintln!("{}", usage());
            std::process::exit(1);
        }
        return;
    }
    if args.first().map(|x| x.as_str()) == Some("llm-auto-update") {
        if let Err(err) = run_llm_auto_update(&args[1..]) {
            eprintln!("llm-auto-update failed: {err}");
            eprintln!("{}", usage());
            std::process::exit(1);
        }
        return;
    }
    if args.first().map(|x| x.as_str()) == Some("graph-html") {
        if let Err(err) = run_graph_html(&args[1..]) {
            eprintln!("graph-html failed: {err}");
            eprintln!("{}", usage());
            std::process::exit(1);
        }
        return;
    }
    if args.first().map(|x| x.as_str()) == Some("net-html") {
        if let Err(err) = run_net_html(&args[1..]) {
            eprintln!("net-html failed: {err}");
            eprintln!("{}", usage());
            std::process::exit(1);
        }
        return;
    }
    if args.first().map(|x| x.as_str()) == Some("sync-plugin-docs") {
        if let Err(err) = run_sync_plugin_docs(&args[1..]) {
            eprintln!("sync-plugin-docs failed: {err}");
            eprintln!("{}", usage());
            std::process::exit(1);
        }
        return;
    }
    if args.first().map(|x| x.as_str()) == Some("refresh-artifact-index") {
        if let Err(err) = run_refresh_artifact_index(&args[1..]) {
            eprintln!("refresh-artifact-index failed: {err}");
            eprintln!("{}", usage());
            std::process::exit(1);
        }
        return;
    }
    if args.first().map(|x| x.as_str()) == Some("rebuild-fixture-artifacts") {
        if let Err(err) = run_rebuild_fixture_artifacts(&args[1..]) {
            eprintln!("rebuild-fixture-artifacts failed: {err}");
            eprintln!("{}", usage());
            std::process::exit(1);
        }
        return;
    }
    if args.first().map(|x| x.as_str()) == Some("prepare-artifacts") {
        if let Err(err) = run_prepare_artifacts(&args[1..]) {
            eprintln!("prepare-artifacts failed: {err}");
            eprintln!("{}", usage());
            std::process::exit(1);
        }
        return;
    }

    if let Err(err) = run_loader(args.first().map(PathBuf::from)) {
        eprintln!("load failed: {err}");
        std::process::exit(1);
    }
}

fn run_loader(root: Option<PathBuf>) -> Result<(), Box<dyn std::error::Error>> {
    let root = root.unwrap_or_else(|| PathBuf::from("fixtures"));
    prepare_fixtures_root(&root, false)?;
    let config = default_loader_config(&root);
    let loader = Loader::new(config);
    let output = loader.load()?;

    println!("execution_id: {}", output.execution_id);
    println!("loaded plugins:");
    for (path, plugin) in output.plugin_registry.iter() {
        println!("- {path}: {:?}", plugin.load_result);
    }
    println!("registered nodes: {}", output.node_registry.len());
    let net = output.graph_registry.net();
    if !net.diagnostics.is_empty() {
        println!("net diagnostics ({}):", net.diagnostics.len());
        for diagnostic in &net.diagnostics {
            println!("  - {diagnostic}");
        }
    }
    println!(
        "metrics: abi_mismatch={}, no_fallback={}, unavailable={}",
        output.metrics.dylib_abi_mismatch_total,
        output.metrics.dylib_no_fallback_total,
        output.metrics.plugin_unavailable_total
    );

    if let Ok(service) = output.context.inject::<String>("root/child", "service.db") {
        println!("inject(root/child, service.db) -> {}", service.as_str());
    }

    Ok(())
}

fn run_serve(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let (root, runtime_only, no_startup_invoke) = parse_serve_args(args, "fixtures")?;
    prepare_fixtures_root(&root, runtime_only)?;
    let host = std::sync::Arc::new(
        RuntimeHost::boot(&root).map_err(|err| runtime_mode_error(err, &root, runtime_only))?,
    );
    let agent_session = host.agent_start(AgentSessionKind::RuntimeShell)?;
    let mut state = ServeState {
        agent_session_id: agent_session.session_id,
        mode: ServeMode::Command,
    };
    println!(
        "serve ready snapshot_id={}",
        host.current_snapshot().snapshot_id()
    );
    io::stdout().flush()?;

    // Signal handler: save draft + revert + shutdown memory, then exit.
    // ctrlc crate handles SIGINT; we convert SIGTERM to SIGINT so both
    // paths go through the same graceful shutdown logic.
    let interrupted = Arc::new(AtomicBool::new(false));
    let fixtures_root = host.fixtures_root().to_path_buf();
    let shutdown_host = Arc::clone(&host);
    {
        let interrupted = Arc::clone(&interrupted);
        ctrlc::set_handler(move || {
            if interrupted.swap(true, Ordering::SeqCst) {
                eprintln!("\nforced exit");
                std::process::exit(1);
            }
            eprint!("\n⏸ interrupted, saving...");
            let _ = std::io::stderr().flush();
            // Save only, never revert: a restart must not destroy uncommitted
            // `plugins/` work (that used to wipe every pending edit whenever
            // systemd stopped the service).
            if let Some(path) = save_draft(&fixtures_root, "signal") {
                eprintln!("draft saved: {path} (working tree left untouched)");
            }
            shutdown_host.write_shutdown_memory();
            // `exit` 绕过所有 Drop，`RuntimeHost::drop` 的 staged root 回收
            // 不会触发，故信号路径必须显式清一次（该方法幂等）。
            shutdown_host.cleanup_live_snapshot();
            std::process::exit(0);
        })
        .ok();
    }
    // SIGTERM → SIGINT so systemctl stop triggers graceful shutdown.
    install_sigterm_handler();

    // ── Startup invocations ──────────────────────────────────────────────
    // Read startup_invoke.json from fixtures root and execute each
    // invocation before entering the REPL.  This is used to start
    // background services (e.g. qq_serve HTTP server).
    if no_startup_invoke {
        eprintln!("[startup] startup_invoke.json skipped (--no-startup-invoke)");
    } else {
        let invocations = load_startup_invocations(&root);
        // Hand the list to the host so a reload can restart the services it
        // had to stop (agent-triggered and control-channel reloads alike).
        host.set_startup_invocations(
            invocations
                .iter()
                .map(|invocation| cordis_runtime::host::StartupInvocation {
                    plugin_path: invocation.plugin_path.clone(),
                    node_id: invocation.node_id.clone(),
                    payload: invocation.payload.clone(),
                })
                .collect(),
        );
        for invocation in &invocations {
            eprintln!(
                "[startup] invoke {}::{} {}",
                invocation.plugin_path,
                invocation.node_id,
                match run_startup_invocation(&host, invocation) {
                    Ok(payload) => format!("ok={payload}"),
                    Err(err) => format!("failed: {err}"),
                }
            );
        }
    }

    // Use rustyline for readline-like editing: history, cursor movement, etc.
    let mut rl = rustyline::DefaultEditor::new()?;
    // Persist history so it survives restarts.
    let history_path = host
        .fixtures_root()
        .join(".cordis-drafts")
        .join("repl-history.txt");
    let _ = rl.load_history(&history_path);

    // In runtime-only mode, park the main thread instead of entering the
    // REPL.  Background services (HTTP servers, etc.) keep running because
    // they were spawned as detached threads during startup invocations.
    if runtime_only {
        eprintln!("runtime-only: inbox started");
        // WS-3: one-shot sweep of dead letters left behind by earlier
        // outages (the 10-20 incident left 700+ files, oldest from
        // September). Never deletes: stale — and unparseable — spills move
        // to `data/pending.archive/`, where they are kept but never replayed.
        let archived = pending::archive_stale(
            &host.data_dir(),
            pending::MAX_AGE,
            std::time::SystemTime::now(),
        );
        if archived > 0 {
            eprintln!("pending: archived {archived} stale spill(s)");
        }
        let (tx, rx) = std::sync::mpsc::sync_channel::<String>(AGENT_TRIGGER_CAPACITY);
        // Ops channel: `cordis-runtime reload` talks to this process instead of
        // restarting it (a restart drops every channel connection and reloads
        // every plugin).
        let (control_tx, control_rx) = std::sync::mpsc::channel::<ControlJob>();
        let control_socket = control_socket_path(host.fixtures_root());
        match start_control_socket(&control_socket, control_tx) {
            Ok(()) => eprintln!("control: listening on {}", control_socket.display()),
            Err(e) => eprintln!("control: disabled ({}): {e}", control_socket.display()),
        }
        let inject_queue = std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::VecDeque::<String>::new(),
        ));
        // OnceLock::set returns Err if already set; runtime-only branch is
        // entered at most once so the first-set path always wins.
        let _ = AGENT_TRIGGER_TX.set(tx);
        cordis_runtime::agent::set_agent_inject_queue(inject_queue.clone());
        let health_host = std::sync::Arc::clone(&host);
        let park_host = std::sync::Arc::clone(&host);
        let mut sessions: BTreeMap<String, String> = BTreeMap::new();
        std::thread::spawn(move || {
            // WS-3: per-session outage-notice throttle. The inbox loop is
            // single-threaded, so a plain HashMap needs no lock.
            let mut outage_notified: HashMap<String, Instant> = HashMap::new();
            loop {
                // Ops requests run on THIS thread: it is the one that runs
                // agent turns, so a reload cannot race one.
                while let Ok(job) = control_rx.try_recv() {
                    run_control_job(&host, job);
                }
                let mut msgs: Vec<String> = Vec::new();
                match rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(m) => msgs.push(m),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
                while let Ok(m) = rx.try_recv() {
                    msgs.push(m);
                }
                // Parse each trigger payload into an envelope and shard by
                // session_key so replies never leak across sources/chats.
                // The envelope also carries reply routing (which plugin +
                // node + target to invoke for the agent's response) — the
                // runtime stays protocol-agnostic (no hard-coded qq_send).
                let mut by_session: BTreeMap<String, Vec<AgentEnvelope>> = BTreeMap::new();
                for msg in msgs {
                    let env = AgentEnvelope::parse(&msg);
                    if !env.session_key.is_empty() {
                        by_session
                            .entry(env.session_key.clone())
                            .or_default()
                            .push(env);
                    }
                }
                for (session_key, envs) in &by_session {
                    if session_key.is_empty() || envs.is_empty() {
                        continue;
                    }
                    let data_dir = host.data_dir();
                    // Fixed-template reply through a GIVEN envelope's route —
                    // the no-LLM send path shared by command replies and
                    // outage receipts. Parameterised by env so each command
                    // in a mixed batch answers its own sender/target.
                    let send_via = |env: &AgentEnvelope, message: &str| {
                        if !env.can_reply() {
                            eprintln!(
                                "inbox: no reply route for {session_key}, direct reply dropped"
                            );
                            return;
                        }
                        let mut payload = serde_json::json!({
                            "node_id": env.reply_node,
                            "target": env.reply_target,
                            "message": message,
                        });
                        // Quote the message being answered when the channel
                        // gave us one (the plugin renders it as a CQ reply).
                        if let Some(reply_to) = env.reply_to.as_ref() {
                            payload["reply_to"] = serde_json::json!(reply_to);
                        }
                        if let Err(e) =
                            host.invoke(&env.source_plugin, &env.reply_node, payload.to_string())
                        {
                            eprintln!("inbox: direct reply failed: {e}");
                        }
                    };
                    // M2: classify the batch so a `/command` anywhere no
                    // longer discards the batch's normal messages. Commands
                    // (N批 bypass-LLM) run FIRST, each with its own envelope
                    // for identity + reply routing; normals then go to the LLM.
                    let mut normals: Vec<AgentEnvelope> = Vec::new();
                    for item in partition_batch(envs.clone()) {
                        match item {
                            BatchItem::Command(env) => {
                                let user_text = extract_user_text(&env.display);
                                let ctx = cordis_runtime::command_router::CommandContext {
                                    session_key: session_key.clone(),
                                    sender_id: env.sender_id.clone(),
                                    conversation_kind: env.conversation_kind.clone(),
                                    soul_key: env.soul_key(),
                                };
                                match cordis_runtime::command_router::dispatch(&host, &ctx, user_text) {
                                    cordis_runtime::command_router::CommandOutcome::Reply(text) => {
                                        send_via(&env, &text);
                                    }
                                    cordis_runtime::command_router::CommandOutcome::ResetSession(text) => {
                                        if let Some(old_sid) = sessions.remove(session_key) {
                                            host.drop_session(&old_sid);
                                            eprintln!("inbox: [{session_key}] session {old_sid} reset by /reset");
                                        }
                                        pending::clear(&data_dir, session_key);
                                        send_via(&env, &text);
                                    }
                                }
                            }
                            BatchItem::Normal(env) => normals.push(env),
                        }
                    }
                    // Pure-command batch: nothing for the LLM, and pending is
                    // left untouched (a command must not consume a spill).
                    if normals.is_empty() {
                        continue;
                    }
                    // Combine only the NORMAL messages; each display keeps its
                    // source-tag prefix (e.g. "[feishu (user X)]: ...").
                    let mut combined = normals
                        .iter()
                        .map(|e| e.display.as_str())
                        .collect::<Vec<_>>()
                        .join("\n");
                    // M批: replay any message that failed during an earlier
                    // LLM outage by prepending it to this batch. Cleared
                    // only after a successful send below.
                    let replaying = pending::load(&data_dir, session_key);
                    if let Some(p) = &replaying {
                        eprintln!(
                            "inbox: [{session_key}] replaying pending message from outage ({} chars)",
                            p.combined.chars().count()
                        );
                        combined = format!("{}\n{}", p.combined, combined);
                    }
                    // Reply routing / soul come from the LAST NORMAL envelope
                    // (all share session_key, so same source/target).
                    let route = normals.last().cloned().unwrap();
                    eprintln!(
                        "inbox: [{session_key}] (soul {}) batch {} msgs: {}",
                        route.soul_key(),
                        normals.len(),
                        combined
                    );
                    // P5: deterministic guards, applied before any LLM work.
                    // A crisis phrase always gets the fixed helpline reply.
                    if let Some(reply) = cordis_runtime::guard::crisis_reply(&combined) {
                        eprintln!("inbox: [{session_key}] crisis phrase → fixed reply");
                        send_via(&route, reply);
                        continue;
                    }
                    // P1-53: don't drain rx.try_recv() here — messages on the
                    // channel are from ALL sessions; the outer loop shards
                    // them correctly next iteration.
                    // P1-52: evict oldest before inserting a new session. Cap
                    // session map at MAX_SESSIONS; evicted key re-creates on
                    // next message via agent_start.
                    const MAX_SESSIONS: usize = 512;
                    if !sessions.contains_key(session_key) && sessions.len() >= MAX_SESSIONS {
                        if let Some(evict_key) = sessions.keys().next().cloned() {
                            if let Some(evicted_sid) = sessions.remove(&evict_key) {
                                eprintln!(
                                    "inbox: evicting oldest session for {} (session {})",
                                    evict_key, evicted_sid
                                );
                                host.drop_session(&evicted_sid);
                            }
                        }
                    }
                    // O批: resolve the soul BEFORE creating the session so
                    // its profile reference picks the LLM config and its
                    // persona overlays the system prompt. Unknown profile
                    // names fall back to default inside resolve().
                    let soul_key = route.soul_key();
                    let sid = sessions.entry(session_key.clone()).or_insert_with(|| {
                        let soul = host.get_soul(&soul_key).ok().flatten();
                        let options = cordis_runtime::host::AgentStartOptions {
                            profile: soul.as_ref().and_then(|s| s.profile.clone()),
                            soul_key: soul_key.clone(),
                        };
                        host.agent_start_with(AgentSessionKind::RuntimeShell, options)
                            .map(|s| s.session_id)
                            .unwrap_or_default()
                    });
                    // H1: re-scope the soul to THIS batch's speaker before
                    // sending. Idempotent — a cheap no-op when unchanged, and
                    // also covers the just-created session. Errors ignored
                    // (a missing session surfaces on agent_send instead).
                    let _ = host.refresh_session_soul(sid, &route.soul_key());
                    let is_group = route.conversation_kind == "group";
                    let _ = host.refresh_session_caller(
                        sid,
                        is_group,
                        route.caller_is_admin(),
                        route.caller_can_approve_iteration(),
                    );
                    // Process agent output: parse JSON, dispatch action, send
                    // the reply back to the ORIGINATING plugin (route). Returns
                    // Some(feedback) if the agent needs to retry.
                    // B1: the assistant's final text IS the reply. There is no
                    // `{"action":...}` envelope any more — an empty answer means
                    // silence, and a tool call is an action (image/@/admin),
                    // never the reply channel. This deletes the whole class of
                    // "valid JSON, unknown action" failures.
                    let process = |content: &str, label: &str, tool_events: &[AgentToolEvent]| {
                        let text = content.trim();
                        if text.is_empty() || text == SILENCE_TOKEN {
                            eprintln!("inbox: no reply text ({label}); staying silent");
                            return;
                        }
                        if already_sent(text, tool_events, &route.reply_node) {
                            eprintln!(
                                "inbox: text already delivered via {} this turn ({label}); not repeating",
                                route.reply_node
                            );
                            return;
                        }
                        send_via(&route, text);
                    };
                    // Commands were already handled above the LLM call.
                    let spill_and_notify = |notified: &mut HashMap<String, Instant>| {
                        pending::save(
                            &data_dir,
                            &pending::PendingMessage {
                                session_key: session_key.clone(),
                                combined: normals
                                    .iter()
                                    .map(|e| e.display.as_str())
                                    .collect::<Vec<_>>()
                                    .join("\n"),
                                enqueued_at_ms: pending::now_ms(),
                            },
                        );
                        if should_notify(
                            notified,
                            session_key,
                            Instant::now(),
                            OUTAGE_NOTIFY_WINDOW,
                        ) {
                            send_via(&route, OUTAGE_NOTICE);
                        }
                    };
                    // Wrap the FINAL combined batch (spill already prepended)
                    // in START/END markers at the send site only. The raw
                    // `combined` remains unwrapped for the spill/log paths
                    // below, so a replayed message is wrapped exactly once.
                    let agent_input = wrap_inbox_batch(&combined);
                    // 图片只属于本批：合并各条 normal 消息里的图片地址，
                    // 由 host 按 runtime.images 预算下载 + 内联。
                    let batch_images: Vec<String> = normals
                        .iter()
                        .flat_map(|e| e.images.iter().cloned())
                        .collect();
                    // 一次性上下文只取最后一条（策略/记忆是按群的，整批一致）。
                    let batch_notes = normals.last().map(|e| e.notes.clone()).unwrap_or_default();
                    // 可信身份：kernel 把它注入每次 invoke 的 payload.__meta，
                    // 插件据此判权；模型自己传的同名字段会被 kernel 覆盖。
                    let mut turn_meta = std::collections::BTreeMap::new();
                    turn_meta.insert("actor".to_string(), route.sender_id.clone());
                    turn_meta.insert("conversation".to_string(), route.conversation_kind.clone());
                    turn_meta.insert("target".to_string(), route.reply_target.clone());
                    match host.agent_send_with_fallback_turn_meta(
                        sid,
                        &agent_input,
                        &batch_images,
                        &batch_notes,
                        &turn_meta,
                    ) {
                        Ok(reply) => {
                            pending::clear(&data_dir, session_key);
                            process(reply.content.trim(), "inbox", &reply.tool_events);
                            // P6: history must hold what the room actually saw
                            // (tool-sent text and/or the answer text), not the
                            // raw model output.
                            let sent = outbound_replies(reply.content.trim(), &reply.tool_events);
                            if !sent.is_empty() {
                                if let Err(e) =
                                    host.agent_record_outbound_reply(sid, &sent.join("\n"))
                                {
                                    eprintln!("inbox: record outbound reply failed: {e}");
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("inbox: {e}");
                            // WS-3: failure exit #1 — the first send failed
                            // (provider exhausted retries + profile fallback).
                            spill_and_notify(&mut outage_notified);
                            cordis_runtime::kernel::notify::send(
                                &host,
                                &format!(
                                    "[{session_key}] ⚠️ LLM 请求失败（消息已暂存待重放）: {e}"
                                ),
                            );
                        }
                    }
                }
            }
        });
        // Load notification handlers from config.
        if let Ok(handlers) = cordis_runtime::kernel::notify::load_handlers(&root) {
            for (plugin_path, node_id) in &handlers {
                cordis_runtime::kernel::notify::register(plugin_path, node_id);
            }
        }

        // Start health check loop after all services are ready.
        // P1-11: handle is intentionally leaked into the process — this
        // branch is the runtime-only long-running path and shutdown flows
        // through process exit rather than a graceful stop of the handle.
        // Leaking (via mem::forget) prevents Drop from firing at scope
        // end, so the loop keeps ticking; a future shutdown-orchestration
        // pass can replace this with `handle.stop()` at the exit sink.
        let health_handle = cordis_runtime::kernel::health::start_health_loop(health_host, 3600);
        std::mem::forget(health_handle);

        // Park — background threads keep running.
        // Periodically check whether stdin is still open: when the parent
        // process dies (e.g. a test runner that spawned us), stdin gets
        // a hangup.  Exiting cleanly prevents orphaned zombie processes
        // with their own health-check loops from piling up.
        loop {
            std::thread::sleep(std::time::Duration::from_secs(30));
            let mut pfd = libc::pollfd {
                fd: libc::STDIN_FILENO,
                events: 0,
                revents: 0,
            };
            let ret = unsafe { libc::poll(&mut pfd, 1, 0) };
            if ret > 0 && (pfd.revents & libc::POLLHUP) != 0 {
                eprintln!("runtime-only: stdin hangup (parent exited), shutting down");
                park_host.write_shutdown_memory();
                std::process::exit(0);
            }
        }
    }

    loop {
        let prompt = match state.mode {
            ServeMode::AgentChat => ">> ",
            ServeMode::ShellConsole => "$ ",
            ServeMode::Command => "> ",
        };
        let line = match rl.readline(prompt) {
            Ok(line) => {
                rl.add_history_entry(&line)?;
                line
            }
            Err(rustyline::error::ReadlineError::Interrupted) => {
                // Ctrl+C: if we get here, the ctrlc handler didn't fire
                // (e.g. rustyline caught it). Treat as exit request.
                println!("^C");
                continue;
            }
            Err(rustyline::error::ReadlineError::Eof) => {
                println!();
                break;
            }
            Err(err) => {
                println!("read error: {err}");
                continue;
            }
        };

        let line = line.trim();
        let handled = match state.mode {
            ServeMode::AgentChat => handle_agent_chat_line(&host, &mut state, line),
            ServeMode::ShellConsole => handle_shell_line(&host, &mut state, line),
            ServeMode::Command => handle_serve_command(&host, &mut state, line),
        };

        let keep_going = match handled {
            Ok(true) => true,
            Ok(false) => false,
            Err(err) => {
                println!("serve error: {err}");
                true
            }
        };
        // Flush stdout AFTER the command fully finished and BEFORE the next
        // queued stdin line is consumed. In batch mode (`cat cmds | serve`)
        // a trailing `quit` used to break the loop while the previous
        // command's output was still buffered — truncating large results
        // such as the `kernel iterate-plugins` JSON. Flushing here on EVERY
        // path (including the quit/exit path, which returns Ok(false) before
        // reaching the per-command flush) guarantees complete output no
        // matter how the loop exits.
        io::stdout().flush()?;
        // Persist history after every command.
        let _ = rl.save_history(&history_path);
        if !keep_going {
            break;
        }
    }

    let _ = rl.save_history(&history_path);
    Ok(())
}

fn handle_serve_command(
    host: &RuntimeHost,
    state: &mut ServeState,
    command: &str,
) -> Result<bool, Box<dyn std::error::Error>> {
    if command.is_empty() {
        return Ok(true);
    }

    match command {
        "help" => {
            println!("{}", serve_usage());
        }
        "agent" => {
            state.mode = ServeMode::AgentChat;
            println!("agent chat mode (>> prompt). /exit to leave, /reset to clear.");
        }
        "shell" => {
            state.mode = ServeMode::ShellConsole;
            println!("shell console ($ prompt). Type commands directly. /exit to leave.");
        }
        "agent status" => {
            println!(
                "{}",
                serde_json::to_string(&host.agent_status(&state.agent_session_id)?)?
            );
        }
        "agent reset" => {
            let session = host.agent_start(AgentSessionKind::RuntimeShell)?;
            state.agent_session_id = session.session_id;
            println!("agent session reset");
        }
        "agent start" => {
            let session = host.agent_start(AgentSessionKind::RuntimeShell)?;
            println!("{}", serde_json::to_string(&session)?);
        }
        "plugins" => {
            let snapshot = host.current_snapshot();
            println!("snapshot_id={}", snapshot.snapshot_id());
            for (plugin_path, plugin) in snapshot.plugin_registry().iter() {
                println!("{plugin_path} {:?}", plugin.load_result);
            }
        }
        "status" => {
            println!("{}", serde_json::to_string(&host.status())?);
        }
        "reload" => {
            let report = host.reload_with_diagnostics("/");
            println!("{}", serde_json::to_string(&report)?);
        }
        "candidate status" => {
            println!("{}", serde_json::to_string(&host.candidate_status())?);
        }
        "candidate reload" => {
            let report = host.reload_candidate_with_diagnostics();
            println!("{}", serde_json::to_string(&report)?);
        }
        "candidate promote" => {
            let report = host.promote_candidate()?;
            println!("{}", serde_json::to_string(&report)?);
        }
        "candidate rollback" => {
            let report = host.rollback_candidate()?;
            println!("{}", serde_json::to_string(&report)?);
        }
        "kernel status" => {
            println!("{}", serde_json::to_string(&host.kernel().status())?);
        }
        "kernel history" => {
            println!(
                "{}",
                serde_json::to_string(&host.kernel().plugin_history())?
            );
        }
        "kernel issues" => {
            println!("{}", serde_json::to_string(&host.kernel().plugin_issues())?);
        }
        "kernel blocked" => {
            println!(
                "{}",
                serde_json::to_string(&host.kernel().blocked_iterations())?
            );
        }
        "exit" | "quit" => return Ok(false),
        _ => {
            if let Some(rest) = command.strip_prefix("agent send ") {
                let (session_id, message) =
                    split_first_token(rest).ok_or("missing session_id/message for agent send")?;
                let reply = host.agent_send(session_id, message)?;
                emit_agent_reply(&reply)?;
            } else if let Some(session_id) = command.strip_prefix("agent status ") {
                let status = host.agent_status(session_id.trim())?;
                println!("{}", serde_json::to_string(&status)?);
            } else if let Some(session_id) = command.strip_prefix("agent transcript ") {
                let transcript = host.agent_transcript(session_id.trim())?;
                println!("{}", serde_json::to_string(&transcript)?);
            } else if let Some(rest) = command.strip_prefix("agent ") {
                let reply = host.agent_send(&state.agent_session_id, rest)?;
                emit_agent_reply(&reply)?;
            } else if let Some(rest) = command.strip_prefix("invoke ") {
                let (plugin_path, remainder) =
                    split_first_token(rest).ok_or("missing plugin_path for invoke")?;
                let (node_id, payload_json) =
                    split_first_token(remainder).ok_or("missing node_id/payload for invoke")?;
                let response = host.invoke(plugin_path, node_id, payload_json.to_string())?;
                emit_invoke_response(&response.payload)?;
            } else if let Some(rest) = command.strip_prefix("execute ") {
                let (target_node_fqn, payload_json) =
                    split_first_token(rest).ok_or("missing node_fqn/payload for execute")?;
                let payload = serde_json::from_str::<Value>(payload_json)?;
                let response = host.execute(target_node_fqn, payload)?;
                println!("{}", serde_json::to_string(&response)?);
            } else if let Some(rest) = command.strip_prefix("candidate invoke ") {
                let (plugin_path, remainder) =
                    split_first_token(rest).ok_or("missing plugin_path for candidate invoke")?;
                let (node_id, payload_json) = split_first_token(remainder)
                    .ok_or("missing node_id/payload for candidate invoke")?;
                let response =
                    host.invoke_candidate(plugin_path, node_id, payload_json.to_string())?;
                emit_invoke_response(&response.payload)?;
            } else if let Some(rest) = command.strip_prefix("candidate execute ") {
                let (target_node_fqn, payload_json) = split_first_token(rest)
                    .ok_or("missing node_fqn/payload for candidate execute")?;
                let payload = serde_json::from_str::<Value>(payload_json)?;
                let response = host.execute_candidate(target_node_fqn, payload)?;
                println!("{}", serde_json::to_string(&response)?);
            } else if let Some(json) = command.strip_prefix("kernel apply-plan ") {
                let request: KernelApplyRequest = serde_json::from_str(json)?;
                let result = host
                    .kernel()
                    .run_iteration(request.plan, request.verification)?;
                println!("{}", serde_json::to_string(&result)?);
            } else if let Some(json) = command.strip_prefix("kernel plan-apply ") {
                let request: KernelPluginIterationRequest = serde_json::from_str(json)?;
                let result = host.iterate_plugins(request)?;
                emit_plugin_iteration_result(&result)?;
            } else if let Some(json) = command.strip_prefix("kernel iterate-plugins ") {
                let request: KernelPluginIterationRequest = serde_json::from_str(json)?;
                let result = host.iterate_plugins(request)?;
                emit_plugin_iteration_result(&result)?;
            } else if let Some(iteration_id) = command.strip_prefix("kernel iteration-status ") {
                let result = host.kernel().plugin_iteration_status(iteration_id.trim())?;
                println!("{}", serde_json::to_string(&result)?);
            } else if let Some(iteration_id) = command.strip_prefix("kernel approve ") {
                let result = host.approve_blocked_iteration(iteration_id.trim())?;
                println!("{}", serde_json::to_string(&result)?);
            } else if command.contains("::") || command.starts_with('/') {
                let input = if let Some(rest) = command.strip_prefix('/') {
                    rest.trim()
                } else {
                    command
                };
                match invoke_shortcut(host, input) {
                    Ok(_) => {}
                    Err(err) => println!("invoke failed: {err}"),
                }
            } else {
                println!("unknown serve command: {command}");
            }
        }
    }

    io::stdout().flush()?;
    Ok(true)
}

fn handle_agent_chat_line(
    host: &RuntimeHost,
    state: &mut ServeState,
    line: &str,
) -> Result<bool, Box<dyn std::error::Error>> {
    if line.is_empty() {
        return Ok(true);
    }

    match line {
        "/help" => println!("{}", agent_chat_usage()),
        "/status" => {
            println!(
                "{}",
                serde_json::to_string(&host.agent_status(&state.agent_session_id)?)?
            );
        }
        "/reset" => {
            let session = host.agent_start(AgentSessionKind::RuntimeShell)?;
            state.agent_session_id = session.session_id;
            println!("agent session reset");
        }
        "/exit" | "/quit" => {
            state.mode = ServeMode::Command;
            println!("back to serve commands (> prompt).");
        }
        _ => {
            // `/node_fqn args...` — direct plugin invocation, bypass agent.
            if let Some(rest) = line.strip_prefix('/') {
                let rest = rest.trim();
                if rest.is_empty() {
                    println!("usage: /<node_fqn> [args...]");
                    return Ok(true);
                }
                match invoke_shortcut(host, rest) {
                    Ok(result) => {
                        // Let the agent know what just happened.
                        let _ = host.agent_inject(
                            &state.agent_session_id,
                            &format!("[direct] /{rest}"),
                            &result,
                        );
                    }
                    Err(err) => println!("invoke failed: {err}"),
                }
            } else {
                match host.agent_send(&state.agent_session_id, line) {
                    Ok(reply) => {
                        emit_agent_reply(&reply)?;
                    }
                    Err(err) => {
                        // Save partial changes as draft, revert workspace.
                        let reverted = host.revert_interactive_changes().unwrap_or(0);
                        let saved = save_draft_and_revert(host.fixtures_root(), "error");
                        if let Some(path) = saved {
                            println!(
                                "\n💾 draft saved: {path} ({n} file(s) reverted)\n   replay: cd fixtures && git apply {path}",
                                n = reverted
                            );
                        } else if reverted > 0 {
                            println!(
                                "\n⚠ agent error, reverted {n} file(s) back to original state.",
                                n = reverted
                            );
                        }
                        return Err(err.into());
                    }
                }
            }
        }
    }

    io::stdout().flush()?;
    Ok(true)
}

fn handle_shell_line(
    host: &RuntimeHost,
    state: &mut ServeState,
    line: &str,
) -> Result<bool, Box<dyn std::error::Error>> {
    if line.is_empty() {
        return Ok(true);
    }
    if line == "/exit" || line == "/quit" {
        state.mode = ServeMode::Command;
        println!("back to serve commands (> prompt).");
        return Ok(true);
    }
    // Route through the Shell plugin: start_terminal with a single command.
    let payload = json!({"action": "start_terminal", "command": line});
    match host.invoke("shell", "shell_entry", serde_json::to_string(&payload)?) {
        Ok(response) => {
            let value: Value = serde_json::from_str(&response.payload)
                .unwrap_or(Value::String(response.payload.clone()));
            if let Some(output) = value.get("output").and_then(Value::as_str) {
                if output.ends_with('\n') {
                    print!("{output}");
                } else {
                    println!("{output}");
                }
            } else if let Some(msg) = value.get("message").and_then(Value::as_str) {
                println!("{msg}");
            }
        }
        Err(err) => println!("shell error: {err}"),
    }
    io::stdout().flush()?;
    Ok(true)
}

fn run_invoke(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.len() < 2 {
        return Err("missing required args: <plugin_path> <node_id>".into());
    }

    let plugin_path = args[0].clone();
    let node_id = args[1].clone();
    let mut fixtures_root: Option<PathBuf> = None;
    let mut payload_json: Option<String> = None;
    let mut runtime_only = false;

    for token in &args[2..] {
        if let Some(value) = token.strip_prefix("--fixtures-root=") {
            fixtures_root = Some(PathBuf::from(value));
            continue;
        }
        if let Some(value) = token.strip_prefix("--payload-json=") {
            payload_json = Some(value.to_string());
            continue;
        }
        if token == "--runtime-only" {
            runtime_only = true;
            continue;
        }
        return Err(format!("unknown flag: {token}").into());
    }

    let payload = payload_json.ok_or("missing required flag: --payload-json=<json>")?;
    let fixtures_root = fixtures_root.unwrap_or_else(PluginInvoker::default_fixtures_root);
    prepare_fixtures_root(&fixtures_root, runtime_only)?;
    let invoker = PluginInvoker::load(&fixtures_root)
        .map_err(|err| runtime_mode_error(err, &fixtures_root, runtime_only))?;
    let response = invoker.invoke(&plugin_path, &node_id, payload)?;
    emit_invoke_response(&response.payload)
}

fn run_execute(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.is_empty() {
        return Err("missing required args: <node_fqn>".into());
    }

    let mut root = PathBuf::from("fixtures");
    let mut target_node_fqn: Option<String> = None;
    let mut payload_json: Option<String> = None;
    let mut runtime_only = false;

    for token in args {
        if let Some(value) = token.strip_prefix("--fixtures-root=") {
            root = PathBuf::from(value);
            continue;
        }
        if let Some(value) = token.strip_prefix("--payload-json=") {
            payload_json = Some(value.to_string());
            continue;
        }
        if token == "--runtime-only" {
            runtime_only = true;
            continue;
        }
        if token.starts_with("--") {
            return Err(format!("unknown flag: {token}").into());
        }
        if target_node_fqn.is_none() {
            target_node_fqn = Some(token.clone());
            continue;
        }
        return Err(format!("unexpected extra arg: {token}").into());
    }

    let target_node_fqn = target_node_fqn.ok_or("missing required arg: <node_fqn>")?;
    let payload = payload_json.ok_or("missing required flag: --payload-json=<json>")?;
    prepare_fixtures_root(&root, runtime_only)?;
    let host =
        RuntimeHost::boot(&root).map_err(|err| runtime_mode_error(err, &root, runtime_only))?;
    let payload = serde_json::from_str::<Value>(&payload)?;
    let result = host.execute(&target_node_fqn, payload)?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

fn emit_invoke_response(payload: &str) -> Result<(), Box<dyn std::error::Error>> {
    let value = match serde_json::from_str::<Value>(payload) {
        Ok(value) => value,
        Err(_) => {
            println!("{payload}");
            return Ok(());
        }
    };

    let Some(object) = value.as_object() else {
        println!("{payload}");
        return Ok(());
    };

    if let Some(output) = object.get("output").and_then(|v| v.as_str()) {
        if !output.is_empty() {
            println!("{output}");
        }
    }

    if let Some(ok) = object.get("ok").and_then(|v| v.as_bool()) {
        let exit_code = object.get("exit_code").cloned().unwrap_or(Value::Null);
        let message = object
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        println!(
            "invoke ok={} exit_code={} message={}",
            ok,
            format_scalar(&exit_code),
            message
        );
        if ok {
            return Ok(());
        }
        return Err(message.to_string().into());
    }

    println!("{payload}");
    Ok(())
}

/// Save the current git diff (tracked *and* untracked) as a draft patch in
/// `.cordis-drafts/`, then revert modified files. Untracked files are moved
/// into `.cordis-drafts/untracked-<ts>-<reason>/` rather than deleted (P0-26).
///
/// Previous behaviour ran `git clean -fd -- plugins/` unconditionally, which
/// deleted every untracked file under `plugins/` on every agent-error path.
/// Users editing a brand-new plugin file that hadn't been `git add`ed yet
/// lost that work on the first error the agent produced. The new flow
/// preserves untracked files under `.cordis-drafts/` and never runs
/// `git clean` outright.
/// Snapshot the tracked `plugins/` diff into `.cordis-drafts/` **without
/// touching the working tree**.
///
/// Used on SIGTERM/SIGINT: the operator's uncommitted plugin edits have to
/// survive a restart. The destructive variant below (`save_draft_and_revert`)
/// stays for the agent-error path, where discarding the agent's own half-done
/// edits is the point.
fn save_draft(fixtures_root: &Path, reason: &str) -> Option<String> {
    let diff = std::process::Command::new("git")
        .args(["diff", "--", "plugins/"])
        .current_dir(fixtures_root)
        .output()
        .ok()?;
    let patch = String::from_utf8_lossy(&diff.stdout).into_owned();
    if patch.trim().is_empty() {
        return None;
    }
    let draft_dir = fixtures_root.join(".cordis-drafts");
    std::fs::create_dir_all(&draft_dir).ok()?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let name = format!("draft-{ts}-{reason}.patch");
    std::fs::write(draft_dir.join(&name), patch.as_bytes()).ok()?;
    Some(format!(".cordis-drafts/{name}"))
}

fn save_draft_and_revert(fixtures_root: &Path, reason: &str) -> Option<String> {
    let draft_dir = fixtures_root.join(".cordis-drafts");
    let _ = std::fs::create_dir_all(&draft_dir);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let draft_path = draft_dir.join(format!("draft-{ts}-{reason}.patch"));

    // 1. Snapshot tracked changes (git diff of tracked files).
    let diff = std::process::Command::new("git")
        .args(["diff", "--", "plugins/"])
        .current_dir(fixtures_root)
        .output()
        .ok()?;
    let patch = String::from_utf8_lossy(&diff.stdout).into_owned();

    // 2. Snapshot untracked file *names* under plugins/ (so we can preserve them).
    let untracked = std::process::Command::new("git")
        .args([
            "ls-files",
            "--others",
            "--exclude-standard",
            "--",
            "plugins/",
        ])
        .current_dir(fixtures_root)
        .output()
        .ok()?;
    let untracked_paths: Vec<String> = String::from_utf8_lossy(&untracked.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.to_string())
        .collect();

    if patch.trim().is_empty() && untracked_paths.is_empty() {
        return None;
    }

    // 3. Persist the tracked diff, if any.
    if !patch.trim().is_empty() {
        std::fs::write(&draft_path, patch.as_bytes()).ok()?;
    }

    // 4. Preserve untracked files by moving them (not deleting) into the
    //    draft dir. If the move fails, LEAVE the file in place rather than
    //    deleting it — losing user work is worse than a slightly noisy
    //    working tree.
    if !untracked_paths.is_empty() {
        let stash_root = draft_dir.join(format!("untracked-{ts}-{reason}"));
        let _ = std::fs::create_dir_all(&stash_root);
        for rel in &untracked_paths {
            let src = fixtures_root.join(rel);
            let dest = stash_root.join(rel);
            if let Some(parent) = dest.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::rename(&src, &dest);
        }
    }

    // 5. Revert tracked modifications only. NEVER `git clean -fd` — that
    //    would clobber untracked files (see above) and any directory the
    //    user just created.
    let _ = std::process::Command::new("git")
        .args(["checkout", "--", "plugins/"])
        .current_dir(fixtures_root)
        .output();

    Some(format!(".cordis-drafts/draft-{ts}-{reason}.patch"))
}

/// Handle `/node_fqn args...` or `/ShellCommand args...` shortcuts in agent chat.
/// Parses the input, looks up the target node, and executes it directly.
/// Returns the formatted result line for injection into agent history.
fn invoke_shortcut(host: &RuntimeHost, input: &str) -> Result<String, Box<dyn std::error::Error>> {
    let (first, rest) = split_first_token(input).unwrap_or((input, ""));
    let node_fqn = if first.contains("::") {
        // Already a fully-qualified node name.
        first.to_string()
    } else {
        // Look up by shell command name in the plugin registry.
        let snapshot = host.current_snapshot();
        let mut found: Option<String> = None;
        for (plugin_path, plugin) in snapshot.plugin_registry().iter() {
            if let Some(docs) = &plugin.docs {
                if docs
                    .command_name
                    .as_ref()
                    .is_some_and(|cmd| cmd.eq_ignore_ascii_case(first))
                {
                    // Use the first declared node for this plugin.
                    if let Some(node) = docs.nodes.first() {
                        found = Some(format!("{plugin_path}::{node_id}", node_id = node.id));
                        break;
                    }
                }
            }
        }
        found.unwrap_or_else(|| first.to_string())
    };

    // Build payload: try JSON first, fall back to wrapping as expression.
    let payload: Value = if rest.is_empty() {
        json!({})
    } else {
        match serde_json::from_str::<Value>(rest) {
            Ok(v) => v,
            Err(_) => {
                // Single number or plain expression: wrap as {"expression": "..."}
                // or try numeric parsing.
                if let Ok(n) = rest.parse::<f64>() {
                    json!({"expression": n.to_string()})
                } else {
                    json!({"expression": rest})
                }
            }
        }
    };

    let response = host.execute(&node_fqn, payload)?;
    let mut lines = Vec::new();
    for trace in response.traces.values() {
        let outcome = match trace.outcome {
            Some(cordis_runtime::core::models::NodeOutcome::Success) => "ok",
            Some(_) => "fail",
            None => "?",
        };
        let payload = trace
            .response_payload
            .as_ref()
            .map(|p| serde_json::to_string(p).unwrap_or_else(|_| "?".to_string()))
            .unwrap_or_else(|| "null".to_string());
        let error = trace.error.as_deref().unwrap_or("");
        let line = if error.is_empty() {
            format!("→ {outcome}: {payload}")
        } else {
            format!("→ {outcome}: {error}")
        };
        println!("{line}");
        lines.push(line);
    }
    Ok(lines.join("\n"))
}

fn emit_agent_reply(reply: &ShellAgentReply) -> Result<(), Box<dyn std::error::Error>> {
    // Tool calls are already announced in real-time during agent execution.
    // Content is already streamed in real-time.
    if reply.tool_events.is_empty() && reply.content.trim().is_empty() {
        println!("(agent returned an empty response)");
    } else {
        // Ensure a trailing newline after streamed content so the next
        // input prompt starts on a fresh line.
        println!();
    }
    Ok(())
}

/// Emit a plugin-iteration result: a human-readable one-line summary first
/// (final_verdict + changed_paths + blocked_reason), then the full JSON on
/// the LAST line so `tail -1 | jq` still parses a complete object. Reading a
/// 450-line JSON blob by eye to find the verdict was the motivating pain.
fn emit_plugin_iteration_result(
    result: &KernelPluginIterationResult,
) -> Result<(), Box<dyn std::error::Error>> {
    let verdict = match result.final_verdict {
        PluginIterationFinalVerdict::Promoted => "promoted",
        PluginIterationFinalVerdict::RolledBack => "rolled_back",
        PluginIterationFinalVerdict::Blocked => "blocked",
        PluginIterationFinalVerdict::InfrastructureFailure => "infrastructure_failure",
    };
    let changed = if result.changed_paths.is_empty() {
        "-".to_string()
    } else {
        result.changed_paths.join(", ")
    };
    let blocked = result.blocked_reason.as_deref().unwrap_or("-");
    println!(
        "iterate-plugins: final_verdict={verdict} changed_paths=[{changed}] blocked_reason={blocked}"
    );
    println!("{}", serde_json::to_string(result)?);
    Ok(())
}

fn format_scalar(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => text.clone(),
        _ => value.to_string(),
    }
}

fn run_auto_update(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.len() < 4 {
        return Err("missing required args".into());
    }

    let workspace_root = PathBuf::from(&args[0]);
    let patch_path = args[1].clone();
    let find = args[2].clone();
    let replace = args[3].clone();

    let mut manual_approved = false;
    let mut tests_passed = true;
    let mut safety_checks_passed = true;
    let mut quality_score = 90_u32;
    let mut diff_lines = 1_usize;

    for token in &args[4..] {
        if token == "--manual-approved" {
            manual_approved = true;
            continue;
        }
        if let Some(value) = token.strip_prefix("--tests-passed=") {
            tests_passed = parse_bool_flag(value)?;
            continue;
        }
        if let Some(value) = token.strip_prefix("--safety-checks-passed=") {
            safety_checks_passed = parse_bool_flag(value)?;
            continue;
        }
        if let Some(value) = token.strip_prefix("--quality-score=") {
            quality_score = value.parse::<u32>()?;
            continue;
        }
        if let Some(value) = token.strip_prefix("--diff-lines=") {
            diff_lines = value.parse::<usize>()?;
            continue;
        }
        return Err(format!("unknown flag: {token}").into());
    }

    let updater = AutoUpdater::new(&workspace_root);
    let result = updater.execute(
        AutoUpdatePlan {
            issue_id: "cli-issue".to_string(),
            patch_id: "cli-patch".to_string(),
            manual_approved,
            diff_lines,
            patches: vec![FilePatch::text(patch_path, find, replace)],
        },
        |_| {
            Ok(VerificationEnvelope::from(VerificationInput {
                tests_passed,
                safety_checks_passed,
                quality_score,
            }))
        },
    )?;

    println!("auto_update verdict: {}", result.verdict);
    println!("rolled_back: {}", result.rolled_back);
    println!("changed_paths: {:?}", result.changed_paths);
    Ok(())
}

fn run_llm_auto_update(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if args.is_empty() {
        return Err("missing required args: <workspace_root>".into());
    }

    let workspace_root = PathBuf::from(&args[0]);
    let mut instruction: Option<String> = None;
    let mut issue_id: Option<String> = None;
    let mut _patch_id: Option<String> = None;
    let mut paths = Vec::new();
    let mut manual_approved = false;
    let mut tests_command: Option<String> = None;
    let mut safety_command: Option<String> = None;
    let mut verify_profile: Option<VerificationProfile> = None;
    let mut quality_score: Option<u32> = None;
    let mut dry_run = false;

    for token in &args[1..] {
        if let Some(value) = token.strip_prefix("--instruction=") {
            instruction = Some(value.to_string());
            continue;
        }
        if let Some(value) = token.strip_prefix("--issue-id=") {
            issue_id = Some(value.to_string());
            continue;
        }
        if let Some(value) = token.strip_prefix("--patch-id=") {
            _patch_id = Some(value.to_string());
            continue;
        }
        if let Some(value) = token.strip_prefix("--path=") {
            paths.push(value.to_string());
            continue;
        }
        if let Some(value) = token.strip_prefix("--tests-command=") {
            tests_command = Some(value.to_string());
            continue;
        }
        if let Some(value) = token.strip_prefix("--safety-command=") {
            safety_command = Some(value.to_string());
            continue;
        }
        if let Some(value) = token.strip_prefix("--verify-profile=") {
            verify_profile = Some(parse_verify_profile_flag(value)?);
            continue;
        }
        if let Some(value) = token.strip_prefix("--quality-score=") {
            quality_score = Some(value.parse::<u32>()?);
            continue;
        }
        if token == "--manual-approved" {
            manual_approved = true;
            continue;
        }
        if token == "--dry-run" {
            dry_run = true;
            continue;
        }
        return Err(format!("unknown flag: {token}").into());
    }

    let instruction = instruction.ok_or("missing required flag: --instruction=<text>")?;
    if paths.is_empty() {
        return Err("missing required flag: --path=<relative_path>".into());
    }

    // Derive target plugin paths from the file paths: "plugins/expr/lexer/src/core.rs" -> "expr/lexer"
    let target_plugin_paths: Vec<String> = paths
        .iter()
        .filter_map(|path| {
            let stripped = path.strip_prefix("plugins/")?;
            if stripped.contains("/src/") {
                Some(stripped.split("/src/").next()?.to_string())
            } else if stripped.contains("/tests/") {
                Some(stripped.split("/tests/").next()?.to_string())
            } else if stripped.ends_with("/Cargo.toml") {
                Some(stripped.strip_suffix("/Cargo.toml")?.to_string())
            } else {
                None
            }
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    let host = RuntimeHost::boot(&workspace_root)?;
    let request = KernelPluginIterationRequest {
        issue_id,
        target_plugin_paths,
        instruction: Some(instruction),
        edit_plan: None,
        manual_approved,
        tests_command,
        safety_command,
        verify_profile,
        quality_score,
    };

    if dry_run {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "dry_run": true,
                "message": "agent loop dry-run",
                "paths": paths,
                "target_plugin_paths": request.target_plugin_paths,
            }))?
        );
        return Ok(());
    }

    let result = host.iterate_plugins(request)?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

fn parse_bool_flag(value: &str) -> Result<bool, Box<dyn std::error::Error>> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        other => Err(format!("invalid bool: {other} (expected true/false)").into()),
    }
}

fn parse_verify_profile_flag(
    value: &str,
) -> Result<VerificationProfile, Box<dyn std::error::Error>> {
    match value {
        "default" => Ok(VerificationProfile::Default),
        "rust-workspace" | "rust_workspace" => Ok(VerificationProfile::RustWorkspace),
        other => {
            Err(format!("invalid verify profile: {other} (expected default|rust-workspace)").into())
        }
    }
}

/// `gc` 子命令的解析结果。
#[derive(Debug, PartialEq, Eq)]
struct GcOptions {
    dry_run: bool,
    max_age: std::time::Duration,
}

/// 解析 `gc` 的 flag。`--max-age-hours=0` 合法，表示所有目录立即过期
/// （与 `RuntimeConfig::snapshot_retention` 的 `Some(0)` 语义一致）。
fn parse_gc_args(args: &[String]) -> Result<GcOptions, Box<dyn std::error::Error>> {
    const DEFAULT_MAX_AGE_HOURS: u64 = 24;
    let mut dry_run = false;
    let mut max_age_hours = DEFAULT_MAX_AGE_HOURS;

    for token in args {
        if token == "--dry-run" {
            dry_run = true;
            continue;
        }
        if let Some(value) = token.strip_prefix("--max-age-hours=") {
            max_age_hours = value
                .parse::<u64>()
                .map_err(|err| format!("invalid --max-age-hours={value}: {err}"))?;
            continue;
        }
        return Err(format!("unknown flag: {token}").into());
    }

    Ok(GcOptions {
        dry_run,
        max_age: std::time::Duration::from_secs(max_age_hours.saturating_mul(3_600)),
    })
}

/// 回收 `{temp_dir}/cordis-runtime-host/` 下已无人认领的 snapshot 目录。
///
/// 每次 boot / reload / plugin-iteration attempt 都会 stage 一份插件工件
/// （约 120-250 MB），而回收此前只覆盖"同一 hash 目录内已死进程的残留"与
/// "reload 时退休的快照"，跨 hash 目录的孤儿无人清理。本命令是运维兜底入口。
fn run_gc(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let options = parse_gc_args(args)?;
    let host_root = cordis_runtime::host::default_host_snapshot_dir();
    if !host_root.exists() {
        println!("nothing to collect: {} does not exist", host_root.display());
        return Ok(());
    }

    let report = cordis_runtime::host::cleanup_orphaned_snapshot_roots(
        &host_root,
        options.max_age,
        None,
        options.dry_run,
    );

    let mib = report.bytes_reclaimed as f64 / (1024.0 * 1024.0);
    let verb = if options.dry_run {
        "would reclaim"
    } else {
        "reclaimed"
    };
    println!("snapshot gc at {}", host_root.display());
    println!("  scanned:         {}", report.scanned);
    println!("  {verb}: {} dir(s), {mib:.1} MiB", report.removed);
    println!("  skipped (live):     {}", report.skipped_live);
    println!("  skipped (journal):  {}", report.skipped_journal);
    println!("  skipped (recent):   {}", report.skipped_recent);
    if report.skipped_journal > 0 {
        println!(
            "  note: dirs holding an unreplayed plugin-iteration journal are kept \
             for manual inspection"
        );
    }
    Ok(())
}

fn run_graph_html(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut root: Option<PathBuf> = None;
    let mut output_path = PathBuf::from("registered-nodes.html");

    for token in args {
        if let Some(value) = token.strip_prefix("--output=") {
            output_path = PathBuf::from(value);
            continue;
        }
        if token.starts_with("--") {
            return Err(format!("unknown flag: {token}").into());
        }
        if root.is_none() {
            root = Some(PathBuf::from(token));
            continue;
        }
        return Err(format!("unexpected extra arg: {token}").into());
    }

    let root = root.unwrap_or_else(|| PathBuf::from("fixtures"));
    prepare_fixtures_root(&root, false)?;
    let loader = Loader::new(default_loader_config(&root));
    let output = loader.load()?;
    let html = output
        .graph_registry
        .handle_get_html("/graphs/registered-nodes.html")?;
    fs::write(&output_path, html)?;

    let absolute = if output_path.is_absolute() {
        output_path
    } else {
        std::env::current_dir()?.join(output_path)
    };
    println!("graph_html written to {}", absolute.display());
    println!(
        "plugins={} nodes={}",
        output.graph_registry.graph().plugins.len(),
        output.graph_registry.graph().nodes.len()
    );
    Ok(())
}

fn split_first_token(input: &str) -> Option<(&str, &str)> {
    let trimmed = input.trim_start();
    if trimmed.is_empty() {
        return None;
    }

    let mut split_index = trimmed.len();
    for (idx, ch) in trimmed.char_indices() {
        if ch.is_whitespace() {
            split_index = idx;
            break;
        }
    }

    let token = &trimmed[..split_index];
    let remainder = trimmed[split_index..].trim_start();
    Some((token, remainder))
}

fn run_net_html(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut root: Option<PathBuf> = None;
    let mut output_path = PathBuf::from("registered-net.html");

    for token in args {
        if let Some(value) = token.strip_prefix("--output=") {
            output_path = PathBuf::from(value);
            continue;
        }
        if token.starts_with("--") {
            return Err(format!("unknown flag: {token}").into());
        }
        if root.is_none() {
            root = Some(PathBuf::from(token));
            continue;
        }
        return Err(format!("unexpected extra arg: {token}").into());
    }

    let root = root.unwrap_or_else(|| PathBuf::from("fixtures"));
    prepare_fixtures_root(&root, false)?;
    let loader = Loader::new(default_loader_config(&root));
    let output = loader.load()?;
    let html = output
        .graph_registry
        .handle_get_html("/graphs/registered-net.html")?;
    fs::write(&output_path, html)?;

    let absolute = if output_path.is_absolute() {
        output_path
    } else {
        std::env::current_dir()?.join(output_path)
    };
    println!("net_html written to {}", absolute.display());
    let net = output.graph_registry.net();
    println!(
        "nodes={} edges={} diagnostics={}",
        net.nodes.len(),
        net.edges.len(),
        net.diagnostics.len()
    );
    for diagnostic in &net.diagnostics {
        println!("  net diagnostic: {diagnostic}");
    }
    Ok(())
}

fn run_sync_plugin_docs(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let root = parse_optional_root_arg(args, "fixtures")?;
    prepare_fixtures_root(&root, false)?;
    let written = sync_plugin_docs(&root)?;
    println!("synced_plugin_docs={}", written.len());
    for path in written {
        println!("{}", path.display());
    }
    Ok(())
}

fn run_refresh_artifact_index(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let root = parse_optional_root_arg(args, "fixtures")?;
    prepare_fixtures_root(&root, false)?;
    let refreshed = refresh_artifact_index(&root)?;
    println!("refreshed_artifact_entries={}", refreshed.len());
    for (plugin_path, hash) in refreshed {
        println!("{plugin_path} {hash}");
    }
    Ok(())
}

fn run_rebuild_fixture_artifacts(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let root = parse_optional_root_arg(args, "fixtures")?;
    let rebuilt = rebuild_fixture_artifacts(&root)?;
    println!("rebuilt_artifact_entries={}", rebuilt.len());
    for (plugin_path, hash) in rebuilt {
        println!("{plugin_path} {hash}");
    }
    Ok(())
}

fn run_prepare_artifacts(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut root = PathBuf::from("fixtures");
    let mut mode = PrepareMode::Incremental;

    for token in args {
        if token == "--full" {
            mode = PrepareMode::Full;
            continue;
        }
        if token.starts_with("--") {
            return Err(format!("unknown flag: {token}").into());
        }
        root = PathBuf::from(token);
    }

    let report = prepare_artifacts(&root, mode)?;
    println!(
        "prepared_artifacts rebuilt={} reused={} full_rebuild={}",
        report.rebuilt.len(),
        report.reused.len(),
        report.full_rebuild
    );
    for (plugin_path, hash) in report.rebuilt {
        println!("{plugin_path} {hash}");
    }
    Ok(())
}

fn prepare_fixtures_root(
    root: &Path,
    runtime_only: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if runtime_only {
        return Ok(());
    }
    let report = prepare_artifacts(root, PrepareMode::Incremental)?;
    if !report.rebuilt.is_empty() {
        println!(
            "prepared fixture artifacts under {} rebuilt={} reused={} full_rebuild={}",
            root.display(),
            report.rebuilt.len(),
            report.reused.len(),
            report.full_rebuild
        );
    }
    Ok(())
}

fn parse_optional_root_arg(
    args: &[String],
    default_root: &str,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    match args {
        [] => Ok(PathBuf::from(default_root)),
        [root] if !root.starts_with("--") => Ok(PathBuf::from(root)),
        [other] => Err(format!("unknown flag: {other}").into()),
        _ => Err("too many arguments".into()),
    }
}

// 本地测试与线上实例共用外部协议凭证：startup_invoke.json 会连真实
// 飞书 WSS / 起 qq HTTP，测试实例会把线上消息抢走并在退出时丢掉
// pending 状态。`--no-startup-invoke`（或 CORDIS_NO_STARTUP_INVOKE
// 环境变量非空）跳过启动段的全部自动 invoke，插件加载与 REPL 功能
// 不受影响。
fn parse_serve_args(
    args: &[String],
    default_root: &str,
) -> Result<(PathBuf, bool, bool), Box<dyn std::error::Error>> {
    let env_no_startup_invoke = std::env::var("CORDIS_NO_STARTUP_INVOKE")
        .map(|v| !v.is_empty())
        .unwrap_or(false);
    parse_serve_args_with_env(args, default_root, env_no_startup_invoke)
}

fn parse_serve_args_with_env(
    args: &[String],
    default_root: &str,
    env_no_startup_invoke: bool,
) -> Result<(PathBuf, bool, bool), Box<dyn std::error::Error>> {
    let mut root = PathBuf::from(default_root);
    let mut runtime_only = false;
    let mut no_startup_invoke = env_no_startup_invoke;
    let mut seen_root = false;

    for token in args {
        if token == "--runtime-only" {
            runtime_only = true;
            continue;
        }
        if token == "--no-startup-invoke" {
            no_startup_invoke = true;
            continue;
        }
        if token.starts_with("--") {
            return Err(format!("unknown flag: {token}").into());
        }
        if seen_root {
            return Err(format!("unexpected extra arg: {token}").into());
        }
        root = PathBuf::from(token);
        seen_root = true;
    }

    Ok((root, runtime_only, no_startup_invoke))
}

fn runtime_mode_error(
    err: cordis_runtime::core::error::RuntimeError,
    root: &Path,
    runtime_only: bool,
) -> Box<dyn std::error::Error> {
    if runtime_only {
        return format!(
            "{err}; bundle is runtime-only, run `cargo run -p cordis-runtime -- prepare-artifacts {}` to rebuild artifacts",
            root.display()
        )
        .into();
    }
    Box::new(err)
}

fn usage() -> String {
    "Usage:
  cargo run -p cordis-runtime -- <fixtures_root>
  cargo run -p cordis-runtime -- serve [fixtures_root] [--runtime-only] [--no-startup-invoke]
  cargo run -p cordis-runtime -- reload [plugin_path] [--fixtures-root=<dir>]   (reload in the RUNNING host)
  cargo run -p cordis-runtime -- invoke <plugin_path> <node_id> --payload-json=<json> [--fixtures-root=fixtures] [--runtime-only]
  cargo run -p cordis-runtime -- execute <node_fqn> --payload-json=<json> [--fixtures-root=fixtures] [--runtime-only]
  cargo run -p cordis-runtime -- llm-auto-update <workspace_root> --instruction=<text> --path=<relative_path> [--path=<relative_path> ...] [--issue-id=<id>] [--patch-id=<id>] [--manual-approved] [--tests-command=<shell>] [--safety-command=<shell>] [--verify-profile=<default|rust-workspace>] [--quality-score=<u32>] [--dry-run]
    tests/safety commands also accept plugin:{\"plugin_path\":\"<plugin_path>\",\"node_id\":\"<node_id>\",\"payload_json\":{},\"expect_substring\":\"<expected text>\",\"fixtures_root\":\"<optional fixtures root>\"}
  cargo run -p cordis-runtime -- auto-update <workspace_root> <relative_path> <find> <replace> [--manual-approved] [--tests-passed=true|false] [--safety-checks-passed=true|false] [--quality-score=<u32>] [--diff-lines=<usize>]
  cargo run -p cordis-runtime -- graph-html [fixtures_root] [--output=registered-nodes.html]
  cargo run -p cordis-runtime -- net-html [fixtures_root] [--output=registered-net.html]
  cargo run -p cordis-runtime -- prepare-artifacts [fixtures_root] [--full]
  cargo run -p cordis-runtime -- sync-plugin-docs [fixtures_root]
  cargo run -p cordis-runtime -- refresh-artifact-index [fixtures_root]
  cargo run -p cordis-runtime -- rebuild-fixture-artifacts [fixtures_root]
  cargo run -p cordis-runtime -- gc [--dry-run] [--max-age-hours=<u64>]
    reclaims orphaned snapshot staging dirs under {temp_dir}/cordis-runtime-host (default max age 24h; 0 expires everything)"
        .to_string()
}

fn serve_usage() -> &'static str {
    "serve commands:
  help
  agent
  shell
  agent <message>
  agent start
  agent send <session-id> <message>
  agent status
  agent status <session-id>
  agent reset
  agent transcript <session-id>
  status
  plugins
  reload
  candidate status
  candidate reload
  candidate promote
  candidate rollback
  invoke <plugin_path> <node_id> <payload-json>
  execute <node_fqn> <payload-json>
  candidate invoke <plugin_path> <node_id> <payload-json>
  candidate execute <node_fqn> <payload-json>
  kernel status
  kernel history
  kernel issues
  kernel blocked
  kernel apply-plan <json>
  kernel plan-apply <json>
  kernel iterate-plugins <json>
  kernel iteration-status <iteration-id>
  kernel approve <iteration-id>
  exit"
}

fn agent_chat_usage() -> &'static str {
    "agent chat mode:
  Type any message to talk with the agent.
  /status  show the current shared agent session status
  /reset   start a fresh shared agent session
  /exit    leave agent chat mode and return to serve commands"
}

#[cfg(test)]
mod gc_args_tests {
    use super::parse_gc_args;
    use std::time::Duration;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn defaults_to_24h_without_flags() {
        let options = parse_gc_args(&args(&[])).unwrap();
        assert!(!options.dry_run);
        assert_eq!(options.max_age, Duration::from_secs(24 * 3600));
    }

    #[test]
    fn parses_dry_run_and_max_age() {
        let options = parse_gc_args(&args(&["--dry-run", "--max-age-hours=6"])).unwrap();
        assert!(options.dry_run);
        assert_eq!(options.max_age, Duration::from_secs(6 * 3600));
    }

    #[test]
    fn zero_max_age_expires_everything() {
        // 与 config 的 snapshot_retention_hours: 0 语义一致，不回落默认值。
        let options = parse_gc_args(&args(&["--max-age-hours=0"])).unwrap();
        assert_eq!(options.max_age, Duration::ZERO);
    }

    #[test]
    fn huge_max_age_saturates_without_panic() {
        let options = parse_gc_args(&args(&["--max-age-hours=18446744073709551615"])).unwrap();
        assert_eq!(options.max_age, Duration::from_secs(u64::MAX));
    }

    #[test]
    fn rejects_unknown_flag_and_non_numeric_age() {
        assert!(parse_gc_args(&args(&["--nope"])).is_err());
        assert!(parse_gc_args(&args(&["--max-age-hours=abc"])).is_err());
        // 位置参数也不接受：gc 不吃 fixtures_root。
        assert!(parse_gc_args(&args(&["fixtures"])).is_err());
    }
}

#[cfg(test)]
mod serve_args_tests {
    use super::parse_serve_args_with_env;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn defaults_without_flags() {
        let (root, runtime_only, no_startup) =
            parse_serve_args_with_env(&args(&[]), "fixtures", false).unwrap();
        assert_eq!(root, std::path::PathBuf::from("fixtures"));
        assert!(!runtime_only);
        assert!(!no_startup);
    }

    #[test]
    fn no_startup_invoke_flag_alone_and_combined() {
        let (_, runtime_only, no_startup) =
            parse_serve_args_with_env(&args(&["--no-startup-invoke"]), "fixtures", false).unwrap();
        assert!(!runtime_only);
        assert!(no_startup);

        let (root, runtime_only, no_startup) = parse_serve_args_with_env(
            &args(&["myroot", "--runtime-only", "--no-startup-invoke"]),
            "fixtures",
            false,
        )
        .unwrap();
        assert_eq!(root, std::path::PathBuf::from("myroot"));
        assert!(runtime_only);
        assert!(no_startup);
    }

    #[test]
    fn env_var_enables_skip_and_flag_still_works_on_top() {
        // CORDIS_NO_STARTUP_INVOKE 生效路径（env 读取在 parse_serve_args
        // 外壳完成，这里注入解析后的值，避免测试间 env 竞争）。
        let (_, _, no_startup) = parse_serve_args_with_env(&args(&[]), "fixtures", true).unwrap();
        assert!(no_startup);
        let (_, _, no_startup) =
            parse_serve_args_with_env(&args(&["--no-startup-invoke"]), "fixtures", true).unwrap();
        assert!(no_startup);
    }

    #[test]
    fn unknown_flag_still_rejected() {
        assert!(parse_serve_args_with_env(&args(&["--bogus"]), "fixtures", false).is_err());
        assert!(
            parse_serve_args_with_env(&args(&["a", "b"]), "fixtures", false).is_err(),
            "extra positional arg must still error"
        );
    }
}

#[cfg(test)]
mod envelope_tests {
    use super::AgentEnvelope;

    // 旧 envelope（无身份字段）必须照常解析，soul_key 回落 session_key。
    #[test]
    fn envelope_parse_backfills_identity() {
        let raw = r#"{"source_plugin":"feishu","reply_node":"feishu_send","session_key":"feishu:chat:oc_x","display":"hi","reply_target":"chat:oc_x"}"#;
        let env = AgentEnvelope::parse(raw);
        assert_eq!(env.sender_id, "");
        assert_eq!(env.conversation_kind, "");
        assert_eq!(env.soul_key(), "feishu:chat:oc_x");
    }

    #[test]
    fn envelope_parse_reads_identity() {
        let raw = r#"{"session_key":"feishu:chat:oc_x","display":"hi","sender_id":"feishu:ou_abc","conversation_kind":"private"}"#;
        let env = AgentEnvelope::parse(raw);
        assert_eq!(env.sender_id, "feishu:ou_abc");
        assert_eq!(env.soul_key(), "feishu:ou_abc#private");
    }

    // 一次性上下文（策略/记忆行）在 notes 里；缺省是空串。
    #[test]
    fn envelope_parse_reads_one_shot_notes() {
        let raw =
            r#"{"session_key":"qq:group:1","display":"hi","notes":"[reply policy: quiet — x]"}"#;
        let env = AgentEnvelope::parse(raw);
        assert_eq!(env.notes, "[reply policy: quiet — x]");
        let raw = r#"{"session_key":"qq:group:1","display":"hi"}"#;
        let env = AgentEnvelope::parse(raw);
        assert!(env.notes.is_empty());
    }

    // 入站图片：envelope 的 images 原样带出，交给 host 下载 + 内联。
    #[test]
    fn envelope_parse_reads_image_urls() {
        let raw = r#"{"session_key":"qq:group:1","display":"看 [image: https://a/1.png]","images":["https://a/1.png"]}"#;
        let env = AgentEnvelope::parse(raw);
        assert_eq!(env.images, vec!["https://a/1.png".to_string()]);
    }

    // 老 envelope 没有 images 字段：解析成空 vec（不能因此丢掉整条消息）。
    #[test]
    fn envelope_parse_defaults_images_to_empty() {
        let raw = r#"{"session_key":"qq:group:1","display":"hi"}"#;
        let env = AgentEnvelope::parse(raw);
        assert!(env.images.is_empty());
    }

    // 非 JSON 的 legacy 纯文本仍能进 agent（无路由、soul_key = 原文）。
    #[test]
    fn envelope_parse_plain_text_fallback() {
        let env = AgentEnvelope::parse("plain text trigger");
        assert_eq!(env.display, "plain text trigger");
        assert!(!env.can_reply());
        assert_eq!(env.soul_key(), "plain text trigger");
    }
}

#[cfg(test)]
mod pending_tests {
    use super::pending;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime};

    /// Unique per-test temp dir that removes itself on drop (tests run in
    /// parallel inside one process, so the pid alone is not unique).
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let nanos = SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let path = std::env::temp_dir().join(format!(
                "cordis-pending-{tag}-{}-{seq}-{nanos}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Forge a spill file directly so tests can control content + timestamp.
    fn write_spill(data_dir: &Path, name: &str, body: &str) {
        let dir = data_dir.join("pending");
        std::fs::create_dir_all(&dir).expect("create pending dir");
        std::fs::write(dir.join(name), body).expect("write spill");
    }

    fn spill_json(key: &str, combined: &str, enqueued_at_ms: u64) -> String {
        serde_json::json!({
            "session_key": key,
            "combined": combined,
            "enqueued_at_ms": enqueued_at_ms,
        })
        .to_string()
    }

    const WEEK: Duration = Duration::from_secs(7 * 24 * 60 * 60);
    const DAY_MS: u64 = 24 * 60 * 60 * 1000;

    // spill → load → clear 往返；连续失败合并不覆盖。
    #[test]
    fn pending_roundtrip_and_merge() {
        let temp = TempDir::new("roundtrip");
        let key = "feishu:chat:oc_x";
        // WS-3 made `load` age-aware, so the forged timestamps must be
        // fresh: a 1970-era `enqueued_at_ms` now counts as a stale spill.
        let first = pending::now_ms();
        assert!(pending::load(temp.path(), key).is_none());
        pending::save(
            temp.path(),
            &pending::PendingMessage {
                session_key: key.to_string(),
                combined: "第一条".to_string(),
                enqueued_at_ms: first,
            },
        );
        pending::save(
            temp.path(),
            &pending::PendingMessage {
                session_key: key.to_string(),
                combined: "第二条".to_string(),
                enqueued_at_ms: first + 200,
            },
        );
        let loaded = pending::load(temp.path(), key).expect("pending should exist");
        assert_eq!(loaded.combined, "第一条\n第二条");
        assert_eq!(loaded.enqueued_at_ms, first, "保留最早时间戳");
        pending::clear(temp.path(), key);
        assert!(pending::load(temp.path(), key).is_none());
    }

    // session_key 含特殊字符不能逃出 pending 目录。
    #[test]
    fn pending_path_sanitizes_key() {
        let dir = std::path::Path::new("/data");
        let p = pending::path_for(dir, "../../etc/passwd");
        assert!(p.starts_with("/data/pending/"), "path: {}", p.display());
        assert!(!p.to_string_lossy().contains(".."), "path: {}", p.display());
    }

    // archive_stale：过期 → 移入 archive、新鲜 → 原地保留、坏 JSON → 也移入
    // （不删），archive 目录在首次归档时由该调用创建，非 .json 不动。
    #[test]
    fn archive_stale_moves_old_and_unparseable() {
        let temp = TempDir::new("archive-mixed");
        let now_ms = pending::now_ms();
        write_spill(
            temp.path(),
            "fresh.json",
            &spill_json("fresh", "新", now_ms),
        );
        write_spill(
            temp.path(),
            "stale.json",
            &spill_json("stale", "旧", now_ms.saturating_sub(8 * DAY_MS)),
        );
        write_spill(temp.path(), "broken.json", "{ this is not json");
        write_spill(temp.path(), "keep.txt", "not a spill");
        assert!(!temp.path().join("pending.archive").exists());

        let archived = pending::archive_stale(temp.path(), WEEK, SystemTime::now());
        assert_eq!(archived, 2);
        // Fresh spill stays live.
        assert!(temp.path().join("pending/fresh.json").exists());
        // Stale + unparseable spill moved out of the live directory, kept.
        assert!(!temp.path().join("pending/stale.json").exists());
        assert!(!temp.path().join("pending/broken.json").exists());
        assert!(temp.path().join("pending.archive/stale.json").exists());
        assert!(temp.path().join("pending.archive/broken.json").exists());
        // Archive dir created on first use; non-json files untouched.
        assert!(temp.path().join("pending.archive").is_dir());
        assert!(temp.path().join("pending/keep.txt").exists());
        // Byte-identical move: the bad payload is preserved, not deleted.
        let kept = std::fs::read_to_string(temp.path().join("pending.archive/broken.json"))
            .expect("archived broken spill is readable");
        assert_eq!(kept, "{ this is not json");
    }

    // archive 目标同名已存在 → 追加数字后缀，绝不覆盖已归档的死信。
    #[test]
    fn archive_stale_never_clobbers_existing_entry() {
        let temp = TempDir::new("archive-collision");
        let stale = pending::now_ms().saturating_sub(30 * DAY_MS);
        write_spill(temp.path(), "dup.json", &spill_json("dup", "旧", stale));
        let archive_dir = temp.path().join("pending.archive");
        std::fs::create_dir_all(&archive_dir).expect("create archive dir");
        std::fs::write(archive_dir.join("dup.json"), "archived-earlier").expect("seed archive");

        let archived = pending::archive_stale(temp.path(), WEEK, SystemTime::now());
        assert_eq!(archived, 1);
        // The older archive entry is intact and the new one landed beside it.
        assert_eq!(
            std::fs::read_to_string(archive_dir.join("dup.json")).ok(),
            Some("archived-earlier".to_string())
        );
        assert!(archive_dir.join("dup.1.json").exists());
        assert!(!temp.path().join("pending/dup.json").exists());
    }

    // 没有 pending 目录（从未发生 spill）→ 0，且不创建 archive 目录。
    #[test]
    fn archive_stale_without_pending_dir_is_noop() {
        let temp = TempDir::new("archive-missing");
        let archived = pending::archive_stale(temp.path(), WEEK, SystemTime::now());
        assert_eq!(archived, 0);
        assert!(!temp.path().join("pending.archive").exists());
    }

    // load：超过 7 天的 spill 视为不存在（文件保留，留给 archive_stale），
    // 新鲜的仍能读回。
    #[test]
    fn load_ignores_stale_spill() {
        let temp = TempDir::new("load-age");
        let key = "feishu:chat:oc_old";
        pending::save(
            temp.path(),
            &pending::PendingMessage {
                session_key: key.to_string(),
                combined: "陈年死信".to_string(),
                enqueued_at_ms: pending::now_ms().saturating_sub(8 * DAY_MS),
            },
        );
        assert!(pending::load(temp.path(), key).is_none());
        assert!(
            pending::path_for(temp.path(), key).exists(),
            "过期 spill 只能忽略，不能删除"
        );

        pending::save(
            temp.path(),
            &pending::PendingMessage {
                session_key: key.to_string(),
                combined: "新鲜的".to_string(),
                enqueued_at_ms: pending::now_ms(),
            },
        );
        let loaded = pending::load(temp.path(), key).expect("fresh spill loads");
        assert_eq!(loaded.combined, "新鲜的");
    }
}

#[cfg(test)]
mod notify_tests {
    use super::should_notify;
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    const WINDOW: Duration = Duration::from_secs(10 * 60);

    // 窗口内第二次被限流，窗口边界（满 10 分钟）重新放行。
    #[test]
    fn should_notify_throttles_within_window() {
        let mut last = HashMap::new();
        let t0 = Instant::now();
        assert!(should_notify(&mut last, "k", t0, WINDOW));
        assert!(!should_notify(
            &mut last,
            "k",
            t0 + Duration::from_secs(1),
            WINDOW
        ));
        assert!(!should_notify(
            &mut last,
            "k",
            t0 + WINDOW - Duration::from_secs(1),
            WINDOW
        ));
        assert!(should_notify(&mut last, "k", t0 + WINDOW, WINDOW));
    }

    // 不同 session_key 各计各的，互不影响。
    #[test]
    fn should_notify_is_per_key() {
        let mut last = HashMap::new();
        let t0 = Instant::now();
        assert!(should_notify(&mut last, "a", t0, WINDOW));
        assert!(should_notify(&mut last, "b", t0, WINDOW));
        assert!(!should_notify(
            &mut last,
            "a",
            t0 + Duration::from_secs(60),
            WINDOW
        ));
        assert!(!should_notify(
            &mut last,
            "b",
            t0 + Duration::from_secs(60),
            WINDOW
        ));
        // A brand-new session is never throttled by other sessions' notices.
        assert!(should_notify(&mut last, "c", t0, WINDOW));
    }
}

#[cfg(test)]
mod batch_tests {
    use super::{extract_user_text, outbound_replies, partition_batch, AgentEnvelope, BatchItem};
    use cordis_runtime::agent::AgentToolEvent;
    use serde_json::{json, Value};

    // Build an envelope with a given display + sender via the JSON parser
    // (no public struct literal outside the module).
    fn env(display: &str, sender_id: &str) -> AgentEnvelope {
        let raw = serde_json::json!({
            "session_key": "feishu:chat:oc_x",
            "display": display,
            "sender_id": sender_id,
            "conversation_kind": "group",
        })
        .to_string();
        AgentEnvelope::parse(&raw)
    }

    // 混合批（普通A / 命令B / 普通C，不同 sender）→ 2 Normal + 1 Command，
    // 顺序保持，Command env 的 sender_id 是 B 的。
    #[test]
    fn partition_batch_splits_commands_and_normals() {
        let batch = vec![
            env("[feishu (user A)]: 你好", "A"),
            env("[feishu (user B)]: /status", "B"),
            env("[feishu (user C)]: 在吗", "C"),
        ];
        let items = partition_batch(batch);
        assert_eq!(items.len(), 3);
        // First and last are Normal (A then C), middle is Command (B).
        assert!(
            matches!(&items[0], BatchItem::Normal(e) if e.sender_id == "A"),
            "item 0 should be Normal(A)"
        );
        assert!(
            matches!(&items[1], BatchItem::Command(e) if e.sender_id == "B"),
            "item 1 should be Command(B)"
        );
        assert!(
            matches!(&items[2], BatchItem::Normal(e) if e.sender_id == "C"),
            "item 2 should be Normal(C)"
        );
        let normals = items
            .iter()
            .filter(|i| matches!(i, BatchItem::Normal(_)))
            .count();
        let commands = items
            .iter()
            .filter(|i| matches!(i, BatchItem::Command(_)))
            .count();
        assert_eq!(normals, 2);
        assert_eq!(commands, 1);
    }

    // 全命令批 → 0 Normal。
    #[test]
    fn partition_batch_all_commands_no_normal() {
        let batch = vec![
            env("[feishu (user A)]: /status", "A"),
            env("[feishu (user B)]: /reset", "B"),
        ];
        let items = partition_batch(batch);
        let normals = items
            .iter()
            .filter(|i| matches!(i, BatchItem::Normal(_)))
            .count();
        assert_eq!(normals, 0);
        assert!(items.iter().all(|i| matches!(i, BatchItem::Command(_))));
    }

    // 带 `]: ` 前缀提取、无前缀回落原文、两侧 trim。
    #[test]
    fn extract_user_text_variants() {
        assert_eq!(
            extract_user_text("[feishu (user 张三)]: /status"),
            "/status"
        );
        assert_eq!(extract_user_text("[qq (user 42)]:   在吗  "), "在吗");
        // No "]: " marker → whole string, trimmed.
        assert_eq!(extract_user_text("  plain text  "), "plain text");
        assert_eq!(extract_user_text("/help"), "/help");
    }

    // 混合批过滤 Normal 后 last 的 soul_key() 是最后一条普通消息发送者的
    // （直接对 partition 结果断言，不经 host）。
    #[test]
    fn batch_route_is_last_normal_sender() {
        let batch = vec![
            env("[feishu (user A)]: 你好", "A"),
            env("[feishu (user C)]: 在吗", "C"),
            env("[feishu (user B)]: /status", "B"),
        ];
        let normals: Vec<AgentEnvelope> = partition_batch(batch)
            .into_iter()
            .filter_map(|i| match i {
                BatchItem::Normal(e) => Some(e),
                BatchItem::Command(_) => None,
            })
            .collect();
        let route = normals.last().expect("at least one normal");
        // Last normal is C (the command B trails but is filtered out).
        assert_eq!(route.sender_id, "C");
        // Group persona is room-scoped: the key is the session, not the sender.
        assert_eq!(route.soul_key(), "feishu:chat:oc_x");
    }

    /// 同一房间的不同发言人共用一个 soul（房间级人格）。
    #[test]
    fn group_soul_key_is_shared_by_speakers() {
        let a = env("[feishu (user A)]: 你好", "A");
        let b = env("[feishu (user B)]: 在吗", "B");
        assert_eq!(a.soul_key(), "feishu:chat:oc_x");
        assert_eq!(b.soul_key(), "feishu:chat:oc_x");
        assert_eq!(a.soul_key(), b.soul_key());
    }

    /// 群聊但缺 session_key 时回落成 per-speaker key，不能塌成空串。
    #[test]
    fn group_soul_key_falls_back_without_session_key() {
        let raw = serde_json::json!({
            "session_key": "",
            "display": "hi",
            "sender_id": "A",
            "conversation_kind": "group",
        })
        .to_string();
        let env = AgentEnvelope::parse(&raw);
        assert_eq!(env.session_key, "");
        assert_eq!(env.soul_key(), "A#group");
    }

    /// 私聊仍按人作用域。
    #[test]
    fn private_soul_key_stays_per_user() {
        let envelope = |sender: &str| {
            let raw = serde_json::json!({
                "session_key": "feishu:ou_abc",
                "display": "hi",
                "sender_id": sender,
                "conversation_kind": "private",
            })
            .to_string();
            AgentEnvelope::parse(&raw)
        };
        assert_eq!(
            envelope("feishu:ou_abc").soul_key(),
            "feishu:ou_abc#private"
        );
        assert_eq!(
            envelope("feishu:ou_xyz").soul_key(),
            "feishu:ou_xyz#private"
        );
    }

    /// P4: role 解析——owner/admin 才可改群人格，缺省视为普通成员。
    #[test]
    fn sender_role_marks_admins() {
        let with_role = |role: &str| {
            let raw = serde_json::json!({
                "session_key": "feishu:chat:oc_x",
                "display": "hi",
                "sender_id": "A",
                "conversation_kind": "group",
                "sender_role": role,
            })
            .to_string();
            AgentEnvelope::parse(&raw)
        };
        assert!(with_role("admin").caller_is_admin());
        assert!(with_role("Owner").caller_is_admin());
        assert!(!with_role("member").caller_is_admin());
        assert!(!with_role("").caller_is_admin());
    }

    /// P6: 从工具调用和助手正文里取出真正发给渠道的文本。
    #[test]
    fn outbound_replies_reads_tool_events_and_answer_text() {
        let event = |name: &str, arguments: Value| AgentToolEvent {
            name: name.to_string(),
            arguments,
            ok: true,
            output: None,
            error: None,
        };
        let qq_send = event(
            "invoke_plugin",
            json!({
                "plugin_path": "qq",
                "node_id": "qq_send",
                "payload_json": {"node_id": "qq_send", "target": "group:1", "message": "你好"}
            }),
        );
        // A tool-sent message and a spoken answer both belong in history.
        assert_eq!(
            outbound_replies("回答", std::slice::from_ref(&qq_send)),
            vec!["你好".to_string(), "回答".to_string()]
        );
        let flat = event("invoke_plugin", json!({"message": "平铺"}));
        assert_eq!(outbound_replies("", &[flat]), vec!["平铺".to_string()]);
        let other = event("read_file", json!({"message": "不是回复"}));
        assert!(outbound_replies("", &[other]).is_empty());
        // An answer identical to what a tool already sent is recorded once.
        assert_eq!(
            outbound_replies("你好", &[qq_send]),
            vec!["你好".to_string()]
        );
        // Whitespace-only text is silence, not a reply.
        assert!(outbound_replies("   \n", &[]).is_empty());
    }
}

#[cfg(test)]
mod inbox_framing_tests {
    use super::wrap_inbox_batch;

    // Channel-neutral markers frame a plain single-line batch, with the
    // scope instruction and the silence-in-plain-words rule present.
    #[test]
    fn wrap_plain_batch() {
        let wrapped = wrap_inbox_batch("[user A]: 你好");
        assert!(wrapped.starts_with("CURRENT_INCOMING_BATCH_START\n"));
        assert!(wrapped.contains("\n[user A]: 你好\n"));
        assert!(wrapped.contains("CURRENT_INCOMING_BATCH_END\n"));
        assert!(wrapped.contains("answer with exactly `[silence]`"));
        // Channel-neutral: no protocol name leaks into runtime framing.
        assert!(!wrapped.contains("QQ"));
        assert!(!wrapped.contains("Feishu"));
    }

    // 判断是否接话必须以对话历史为依据；不能再有"不是对你说的就 suspend"这种
    // 无条件沉默指令（5.2.44）。
    #[test]
    fn wrap_tells_the_model_to_judge_from_history() {
        let wrapped = wrap_inbox_batch("[user A]: 你好");
        let tells_to_use_history = wrapped.contains("Use the earlier conversation as the basis");
        assert!(tells_to_use_history, "{wrapped}");
        let orders_silence = wrapped.contains("Not directed at you");
        assert!(!orders_silence, "must not order silence: {wrapped}");
        let defers_to_policy = wrapped.contains("[reply policy:");
        assert!(defers_to_policy, "{wrapped}");
        let keeps_anti_hallucination =
            wrapped.contains("do not claim someone is asking you a question");
        assert!(keeps_anti_hallucination, "{wrapped}");
    }

    // A multi-line combined batch (outage spill prepended to fresh messages)
    // is wrapped exactly once: a single START and a single END, with the
    // spilled line and the new line both inside.
    #[test]
    fn wrap_multiline_batch_with_spill_no_nesting() {
        let combined = "[user A]: 昨天没发出去的\n[user B]: 今天新的";
        let wrapped = wrap_inbox_batch(combined);
        assert_eq!(
            wrapped.matches("CURRENT_INCOMING_BATCH_START").count(),
            1,
            "exactly one START marker (no nesting)"
        );
        assert_eq!(
            wrapped.matches("CURRENT_INCOMING_BATCH_END").count(),
            1,
            "exactly one END marker (no nesting)"
        );
        assert!(wrapped.contains("昨天没发出去的"));
        assert!(wrapped.contains("今天新的"));
    }

    // Re-wrapping an already-wrapped string would nest markers; confirm the
    // helper output only nests if called twice (guarding the send-site rule
    // that storage keeps raw combined, so replay wraps exactly once).
    #[test]
    fn double_wrap_would_nest() {
        let once = wrap_inbox_batch("hi");
        let twice = wrap_inbox_batch(&once);
        assert_eq!(twice.matches("CURRENT_INCOMING_BATCH_START").count(), 2);
    }
}

#[cfg(test)]
mod draft_tests {
    use super::*;

    /// Minimal temp dir (the CLI crate has no `tempfile` dev-dependency).
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("cordis-cli-draft-test-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .expect("git must be installed for this test");
        assert!(status.success(), "git {args:?} failed");
    }

    /// A shutdown must never discard uncommitted plugin edits: `save_draft`
    /// only writes the patch, it must leave the working tree as it was.
    #[test]
    fn save_draft_leaves_working_tree_untouched() {
        let temp = TempDir::new();
        let root = temp.path();
        git(root, &["init", "-q"]);
        std::fs::create_dir_all(root.join("plugins/demo/src")).expect("mkdir");
        std::fs::write(root.join("plugins/demo/src/a.rs"), "ORIGINAL\n").expect("write");
        git(root, &["add", "."]);
        git(
            root,
            &[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-q",
                "-m",
                "init",
            ],
        );

        std::fs::write(root.join("plugins/demo/src/a.rs"), "EDITED\n").expect("edit");
        let draft = save_draft(root, "signal").expect("a tracked diff must produce a draft");
        assert!(
            root.join(&draft).exists(),
            "draft patch must be written next to the fixtures root"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("plugins/demo/src/a.rs")).expect("read"),
            "EDITED\n",
            "save_draft must not revert the working tree"
        );
        let patch = std::fs::read_to_string(root.join(&draft)).expect("read patch");
        assert!(patch.contains("EDITED"), "draft must carry the change");

        // No diff → no draft, and nothing is written.
        git(root, &["checkout", "--", "plugins/"]);
        assert!(save_draft(root, "signal").is_none());
    }
}

// ---------------------------------------------------------------------------
// Control channel (unix socket) — lets an operator trigger a plugin reload in
// the *running* host without restarting it.
//
// `systemctl restart` drops every connection (QQ/Feishu) and reloads every
// plugin; a plugin-only change only needs `reload /<plugin>`. The socket is
// private to the invoking user (dir 0700, socket 0600, peer uid checked) and
// accepts exactly two actions: `reload` and `status`.
// ---------------------------------------------------------------------------

/// Cap on a single request line: this is a control channel, not a data pipe.
const CONTROL_MAX_LINE: usize = 64 * 1024;

/// Per-user directory for the control sockets.
fn control_socket_path(fixtures_root: &Path) -> PathBuf {
    let root = fixtures_root
        .canonicalize()
        .unwrap_or_else(|_| fixtures_root.to_path_buf());
    // FNV-1a over the canonical root: stable across processes (no RandomState)
    // and distinct per checkout, so two runtimes on one box never share a
    // socket.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in root.to_string_lossy().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    // SAFETY: geteuid has no preconditions and cannot fail.
    let uid = unsafe { libc::geteuid() };
    control_socket_dir(uid).join(format!("{hash:016x}.sock"))
}

/// Where the control sockets live: `/run` when it is writable (shared with the
/// service's mount namespace), else the temp dir (tests, non-root CI).
fn control_socket_dir(uid: libc::uid_t) -> PathBuf {
    let run = PathBuf::from(format!("/run/cordis-runtime-{uid}"));
    if fs::create_dir_all(&run).is_ok() {
        return run;
    }
    std::env::temp_dir().join(format!("cordis-runtime-ctl-{uid}"))
}

/// One control request, with the channel used to hand the response back.
struct ControlJob {
    action: ControlAction,
    reply: std::sync::mpsc::SyncSender<String>,
}

enum ControlAction {
    Reload { plugin_path: String },
    Status,
}

fn parse_control_request(line: &str) -> Result<ControlAction, String> {
    let value: Value = serde_json::from_str(line).map_err(|e| format!("invalid json: {e}"))?;
    let action = value
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing \"action\"".to_string())?;
    match action {
        "reload" => Ok(ControlAction::Reload {
            plugin_path: value
                .get("plugin_path")
                .and_then(Value::as_str)
                .unwrap_or("/")
                .to_string(),
        }),
        "status" => Ok(ControlAction::Status),
        other => Err(format!("unknown action: {other}")),
    }
}

fn control_error(message: &str) -> String {
    json!({ "ok": false, "error": message }).to_string()
}

/// Bind the control socket and serve it from a detached thread.
///
/// Bound synchronously so the caller knows the socket is ready before the
/// Serve loop starts accepting jobs.
fn start_control_socket(
    path: &Path,
    tx: std::sync::mpsc::Sender<ControlJob>,
) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "socket has no dir")
    })?;
    fs::create_dir_all(dir)?;
    // Only this user may enter the directory or talk to the socket.
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    // A stale socket from a crashed run would make bind() fail; remove it.
    let _ = fs::remove_file(path);
    let listener = std::os::unix::net::UnixListener::bind(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let tx = tx.clone();
            std::thread::spawn(move || serve_control_connection(stream, &tx));
        }
    });
    Ok(())
}

/// Reflect the request line back to the client after the inbox thread answers,
/// or immediately for malformed input.
fn serve_control_connection(
    stream: std::os::unix::net::UnixStream,
    tx: &std::sync::mpsc::Sender<ControlJob>,
) {
    use std::io::{BufRead, BufReader, Read as _};
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
    if !control_peer_is_us(&stream) {
        let _ = write_control_response(&stream, &control_error("peer uid mismatch"));
        return;
    }
    let mut reader = BufReader::new(&stream).take((CONTROL_MAX_LINE + 1) as u64);
    let mut line = String::new();
    if let Err(e) = reader.read_line(&mut line) {
        let _ = write_control_response(&stream, &control_error(&format!("read failed: {e}")));
        return;
    }
    if line.len() > CONTROL_MAX_LINE {
        let _ = write_control_response(&stream, &control_error("request line too long"));
        return;
    }
    let response = match parse_control_request(line.trim()) {
        Ok(action) => {
            let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel::<String>(1);
            if tx
                .send(ControlJob {
                    action,
                    reply: reply_tx,
                })
                .is_err()
            {
                control_error("inbox is not running (control channel unavailable)")
            } else {
                // A reload rebuilds a snapshot: give it room, but never hang
                // the client forever.
                reply_rx
                    .recv_timeout(Duration::from_secs(120))
                    .unwrap_or_else(|_| control_error("timed out waiting for the host"))
            }
        }
        Err(e) => control_error(&e),
    };
    let _ = write_control_response(&stream, &response);
}

fn write_control_response(
    stream: &std::os::unix::net::UnixStream,
    response: &str,
) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut stream = stream;
    stream.write_all(response.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()
}

/// Only the owning user may drive the control channel (defence in depth on top
/// of the 0700/0600 permissions).
fn control_peer_is_us(stream: &std::os::unix::net::UnixStream) -> bool {
    use std::os::unix::io::AsRawFd;
    let mut cred: libc::ucred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred`/`len` are valid for the syscall's documented sizes and the
    // fd comes from a live UnixStream.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    rc == 0 && cred.uid == unsafe { libc::geteuid() }
}

/// Handle one control job on the inbox thread, so a reload never races an
/// in-flight agent turn.
fn run_control_job(host: &RuntimeHost, job: ControlJob) {
    let response = match job.action {
        ControlAction::Reload { plugin_path } => {
            let report = host.reload_with_diagnostics(&plugin_path);
            let ok = report.status != cordis_runtime::host::ReloadAttemptStatus::Failed;
            // The host restarts the Task services this reload stopped (it owns
            // the startup-invocation list), so the response only carries the
            // report.
            json!({ "ok": ok, "action": "reload", "report": report }).to_string()
        }
        ControlAction::Status => {
            json!({ "ok": true, "action": "status", "status": host.status() }).to_string()
        }
    };
    let _ = job.reply.send(response);
}

/// One entry from `fixtures/startup_invoke.json`: how to start a Task service.
#[derive(Debug, Clone)]
struct StartupInvocation {
    plugin_path: String,
    node_id: String,
    payload: String,
}

/// Parse `fixtures/startup_invoke.json`. A missing/broken file yields an empty
/// list (the service simply starts nothing), matching the boot behaviour.
fn load_startup_invocations(root: &Path) -> Vec<StartupInvocation> {
    let path = root.join("startup_invoke.json");
    let Ok(text) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&text) else {
        eprintln!("[startup] startup_invoke.json must be an array");
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let plugin_path = item["plugin_path"].as_str().unwrap_or("");
            let node_id = item["node_id"].as_str().unwrap_or("");
            if plugin_path.is_empty() || node_id.is_empty() {
                return None;
            }
            let payload = item["payload"]
                .as_object()
                .map(|o| serde_json::to_string(o).unwrap_or_default())
                .unwrap_or_else(|| "{}".to_string());
            Some(StartupInvocation {
                plugin_path: plugin_path.to_string(),
                node_id: node_id.to_string(),
                payload,
            })
        })
        .collect()
}

fn run_startup_invocation(
    host: &RuntimeHost,
    invocation: &StartupInvocation,
) -> Result<String, String> {
    host.invoke(
        &invocation.plugin_path,
        &invocation.node_id,
        invocation.payload.clone(),
    )
    .map(|response| response.payload)
    .map_err(|e| e.to_string())
}

/// `cordis-runtime reload [plugin_path] [--fixtures-root=<dir>]`
fn run_reload_command(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut plugin_path = "/".to_string();
    let mut fixtures_root = PluginInvoker::default_fixtures_root();
    let mut have_plugin = false;
    for token in args {
        if let Some(value) = token.strip_prefix("--fixtures-root=") {
            fixtures_root = PathBuf::from(value);
            continue;
        }
        if token.starts_with("--") {
            return Err(format!("unknown flag: {token}").into());
        }
        if have_plugin {
            return Err(format!("unexpected extra arg: {token}").into());
        }
        plugin_path = token.clone();
        have_plugin = true;
    }
    let socket = control_socket_path(&fixtures_root);
    let request = json!({ "action": "reload", "plugin_path": plugin_path }).to_string();
    let response = request_control(&socket, &request)?;
    println!("{response}");
    let parsed: Value = serde_json::from_str(&response)?;
    if parsed.get("ok").and_then(Value::as_bool) == Some(false) {
        return Err("reload failed (see the response above)".into());
    }
    Ok(())
}

/// One request/response round trip with the running host.
fn request_control(socket: &Path, request: &str) -> Result<String, String> {
    use std::io::{BufRead, BufReader, Write as _};
    let mut stream = std::os::unix::net::UnixStream::connect(socket)
        .map_err(|e| format!("cannot reach the running host at {}: {e}", socket.display()))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(130)))
        .map_err(|e| format!("socket timeout: {e}"))?;
    stream
        .write_all(request.as_bytes())
        .and_then(|_| stream.write_all(b"\n"))
        .map_err(|e| format!("write failed: {e}"))?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("read failed: {e}"))?;
    if line.trim().is_empty() {
        return Err("host closed the control socket without a response".into());
    }
    Ok(line.trim().to_string())
}

#[cfg(test)]
mod control_tests {
    use super::*;

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("cordis-ctl-{tag}-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    /// Bind a socket under the temp dir and answer every job with `reply`.
    fn serve_with(tag: &str, reply: &'static str) -> (TempDir, PathBuf) {
        let temp = TempDir::new(tag);
        let socket = temp.path().join("control.sock");
        let (tx, rx) = std::sync::mpsc::channel::<ControlJob>();
        start_control_socket(&socket, tx).expect("bind control socket");
        std::thread::spawn(move || {
            while let Ok(job) = rx.recv() {
                // Exercise the real job shape without a host.
                let _ = match job.action {
                    ControlAction::Reload { plugin_path } => plugin_path,
                    ControlAction::Status => "status".to_string(),
                };
                let _ = job.reply.send(reply.to_string());
            }
        });
        (temp, socket)
    }

    #[test]
    fn socket_path_is_stable_and_per_checkout() {
        let a = control_socket_path(Path::new("/tmp/checkout-a/fixtures"));
        let b = control_socket_path(Path::new("/tmp/checkout-a/fixtures"));
        let c = control_socket_path(Path::new("/tmp/checkout-b/fixtures"));
        assert_eq!(a, b, "same root must map to the same socket");
        assert_ne!(a, c, "different checkouts must not share a socket");
        assert_eq!(
            a.extension().and_then(|e| e.to_str()),
            Some("sock"),
            "socket path: {}",
            a.display()
        );
        // Per-user directory: another user must not be able to reach it.
        let uid = unsafe { libc::geteuid() };
        assert!(
            a.parent()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .is_some_and(|name| name.ends_with(&uid.to_string())),
            "socket dir must be per-uid: {}",
            a.display()
        );
    }

    #[test]
    fn control_request_round_trips() {
        let (_temp, socket) = serve_with("roundtrip", r#"{"ok":true,"action":"status"}"#);
        let response = request_control(&socket, r#"{"action":"status"}"#).expect("round trip");
        assert_eq!(response, r#"{"ok":true,"action":"status"}"#);
    }

    #[test]
    fn control_rejects_unknown_action_bad_json_and_long_lines() {
        let (_temp, socket) = serve_with("reject", r#"{"ok":true}"#);
        // Unknown action is rejected before it ever reaches the inbox thread.
        let response = request_control(&socket, r#"{"action":"shutdown"}"#).expect("response");
        assert!(
            response.contains("unknown action: shutdown"),
            "response: {response}"
        );
        // Malformed JSON likewise.
        let response = request_control(&socket, "{not json").expect("response");
        assert!(response.contains("invalid json"), "response: {response}");
        // Missing action.
        let response = request_control(&socket, r#"{"plugin_path":"/qq"}"#).expect("response");
        assert!(response.contains("missing"), "response: {response}");
        // Oversized line is refused instead of buffering without bound.
        let big = format!(
            r#"{{"action":"status","pad":"{}"}}"#,
            "x".repeat(CONTROL_MAX_LINE)
        );
        let response = request_control(&socket, &big).expect("response");
        assert!(response.contains("too long"), "response: {response}");
    }

    #[test]
    fn control_reports_missing_host() {
        let temp = TempDir::new("missing");
        let socket = temp.path().join("nothing.sock");
        let err = request_control(&socket, r#"{"action":"status"}"#).expect_err("no host");
        assert!(err.contains("cannot reach the running host"), "{err}");
    }

    #[test]
    fn parse_control_request_defaults_plugin_path_to_root() {
        match parse_control_request(r#"{"action":"reload"}"#).expect("parse") {
            ControlAction::Reload { plugin_path } => assert_eq!(plugin_path, "/"),
            ControlAction::Status => panic!("expected reload"),
        }
        match parse_control_request(r#"{"action":"reload","plugin_path":"/qq"}"#).expect("parse") {
            ControlAction::Reload { plugin_path } => assert_eq!(plugin_path, "/qq"),
            ControlAction::Status => panic!("expected reload"),
        }
        assert!(matches!(
            parse_control_request(r#"{"action":"status"}"#).expect("parse"),
            ControlAction::Status
        ));
    }

    #[test]
    fn control_peer_is_us_accepts_own_connection() {
        let temp = TempDir::new("peer");
        let socket = temp.path().join("peer.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).expect("bind");
        let client = std::os::unix::net::UnixStream::connect(&socket).expect("connect");
        let (server, _) = listener.accept().expect("accept");
        assert!(control_peer_is_us(&server));
        drop(client);
    }
}

#[cfg(test)]
mod startup_invocation_tests {
    use super::*;

    fn write_startup(root: &Path, body: &str) {
        std::fs::write(root.join("startup_invoke.json"), body).expect("write startup file");
    }

    #[test]
    fn startup_invocations_parse_and_filter() {
        let temp = std::env::temp_dir().join(format!("cordis-startup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).expect("mkdir");
        write_startup(
            &temp,
            r#"[{"plugin_path":"qq","node_id":"qq_serve","payload":{"port":8099}},
                {"plugin_path":"feishu","node_id":"feishu_serve","payload":{"mode":"ws"}},
                {"plugin_path":"","node_id":"ignored"},
                {"plugin_path":"nocontent"}]"#,
        );
        let invocations = load_startup_invocations(&temp);
        assert_eq!(invocations.len(), 2, "malformed entries are dropped");
        assert_eq!(invocations[0].plugin_path, "qq");
        assert_eq!(invocations[0].payload, r#"{"port":8099}"#);

        // Broken / non-array files start nothing instead of panicking.
        write_startup(&temp, "not json");
        assert!(load_startup_invocations(&temp).is_empty());
        write_startup(&temp, r#"{"plugin_path":"qq"}"#);
        assert!(load_startup_invocations(&temp).is_empty());
        std::fs::remove_file(temp.join("startup_invoke.json")).expect("rm");
        assert!(load_startup_invocations(&temp).is_empty());
        let _ = std::fs::remove_dir_all(&temp);
    }
}

#[cfg(test)]
#[cfg(test)]
mod silence_token_tests {
    use super::SILENCE_TOKEN;

    // Silence has exactly one spelling. A placeholder the model invents
    // (`(empty)`) is just text and must still be distinguishable, so the
    // inbox only suppresses the literal token.
    #[test]
    fn only_the_exact_token_is_silence() {
        let is_silence = |content: &str| content.trim() == SILENCE_TOKEN;
        assert!(is_silence("  [silence]\n"));
        assert!(!is_silence("(empty)"));
        assert!(!is_silence("[silence] 好"));
        assert!(!is_silence(""));
    }

    #[test]
    fn the_token_is_documented_for_the_model() {
        let hint = include_str!("../../../fixtures/plugins/qq/src/lib.rs");
        assert!(hint.contains("exactly `[silence]`"), "qq hint must name it");
    }
}

#[cfg(test)]
mod reply_dedupe_tests {
    use super::already_sent;
    use cordis_runtime::agent::AgentToolEvent;
    use serde_json::json;

    fn event(node_id: &str, message: &str, ok: bool) -> AgentToolEvent {
        AgentToolEvent {
            name: "invoke_plugin".to_string(),
            arguments: json!({
                "node_id": node_id,
                "plugin_path": "qq",
                "payload_json": { "node_id": node_id, "message": message },
            }),
            ok,
            output: None,
            error: None,
        }
    }

    // The answer text is the reply; a byte-identical repeat of what a tool
    // already sent this turn is suppressed, while a caption still goes out.
    #[test]
    fn only_an_identical_repeat_is_suppressed() {
        let sent = event("qq_send", "你好", true);
        assert!(already_sent("你好", std::slice::from_ref(&sent), "qq_send"));
        assert!(!already_sent(
            "你好，图在这",
            std::slice::from_ref(&sent),
            "qq_send"
        ));
        assert!(!already_sent(
            "你好",
            &[event("qq_send", "你好", false)],
            "qq_send"
        ));
        assert!(!already_sent(
            "你好",
            &[event("qq_get_group_info", "你好", true)],
            "qq_send"
        ));
        assert!(!already_sent("你好", &[], "qq_send"));
        assert!(!already_sent("你好", &[sent], ""));
    }
}
