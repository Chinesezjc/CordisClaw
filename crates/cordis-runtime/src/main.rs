use cordis_runtime::agent::ShellAgentReply;
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
use cordis_runtime::plugin::tooling::{
    prepare_artifacts, rebuild_fixture_artifacts, refresh_artifact_index, sync_plugin_docs,
    PrepareMode,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

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
            },
        }
    }

    fn can_reply(&self) -> bool {
        !self.source_plugin.is_empty() && !self.reply_node.is_empty()
    }

    /// Soul scope key: `{sender_id}#{conversation_kind}` so the same user
    /// can carry different personas in private vs group chats. Envelopes
    /// without identity (legacy callers) scope by session_key instead.
    fn soul_key(&self) -> String {
        if self.sender_id.is_empty() {
            self.session_key.clone()
        } else {
            format!("{}#{}", self.sender_id, self.conversation_kind)
        }
    }
}

/// M批: pending-message spill. When the LLM request exhausts every retry
/// AND the profile fallback, the user's message must not vanish — it is
/// written to `data/pending/<hash>.json` and replayed (prepended) on the
/// next message for the same session. Pure mechanical path: no LLM.
mod pending {
    use serde::{Deserialize, Serialize};
    use std::path::{Path, PathBuf};

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
        }
    }

    pub fn load(data_dir: &Path, session_key: &str) -> Option<PendingMessage> {
        let text = std::fs::read_to_string(path_for(data_dir, session_key)).ok()?;
        serde_json::from_str(&text).ok()
    }

    pub fn clear(data_dir: &Path, session_key: &str) {
        let _ = std::fs::remove_file(path_for(data_dir, session_key));
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
            save_draft_and_revert(&fixtures_root, "signal");
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
    let startup_file = root.join("startup_invoke.json");
    if no_startup_invoke {
        eprintln!("[startup] startup_invoke.json skipped (--no-startup-invoke)");
    } else if startup_file.exists() {
        match fs::read_to_string(&startup_file) {
            Ok(text) => match serde_json::from_str::<Value>(&text) {
                Ok(Value::Array(items)) => {
                    for item in &items {
                        let plugin_path = item["plugin_path"].as_str().unwrap_or("");
                        let node_id = item["node_id"].as_str().unwrap_or("");
                        let payload = item["payload"]
                            .as_object()
                            .map(|o| serde_json::to_string(o).unwrap_or_default())
                            .unwrap_or_else(|| "{}".to_string());
                        if !plugin_path.is_empty() && !node_id.is_empty() {
                            match host.invoke(plugin_path, node_id, payload) {
                                Ok(response) => {
                                    eprintln!(
                                        "[startup] invoke {plugin_path}::{node_id} ok={}",
                                        response.payload
                                    );
                                }
                                Err(err) => {
                                    eprintln!(
                                        "[startup] invoke {plugin_path}::{node_id} failed: {err}"
                                    );
                                }
                            }
                        }
                    }
                }
                Ok(_) => eprintln!("[startup] startup_invoke.json must be an array"),
                Err(e) => eprintln!("[startup] startup_invoke.json parse error: {e}"),
            },
            Err(e) => eprintln!("[startup] cannot read startup_invoke.json: {e}"),
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
        let (tx, rx) = std::sync::mpsc::sync_channel::<String>(AGENT_TRIGGER_CAPACITY);
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
            loop {
                let mut msgs: Vec<String> = Vec::new();
                match rx.recv() {
                    Ok(m) => msgs.push(m),
                    Err(_) => break,
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
                        let payload = serde_json::json!({
                            "node_id": env.reply_node,
                            "target": env.reply_target,
                            "message": message,
                        });
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
                    // Process agent output: parse JSON, dispatch action, send
                    // the reply back to the ORIGINATING plugin (route). Returns
                    // Some(feedback) if the agent needs to retry.
                    let process = |raw: String, label: &str| -> Option<String> {
                        if raw.is_empty() {
                            return None;
                        }
                        // Preprocess: escape newlines and embedded quotes inside JSON strings.
                        let chars: Vec<char> = raw.chars().collect();
                        let mut out = String::with_capacity(raw.len() + 64);
                        let mut in_string = false;
                        let mut i = 0;
                        while i < chars.len() {
                            let ch = chars[i];
                            if ch == '"' {
                                if in_string {
                                    let already_escaped = i > 0 && chars[i - 1] == '\\';
                                    if already_escaped {
                                        out.push('"');
                                    } else {
                                        let mut j = i + 1;
                                        while j < chars.len() && chars[j] == ' ' {
                                            j += 1;
                                        }
                                        let next = chars.get(j).copied();
                                        if matches!(
                                            next,
                                            Some(':') | Some(',') | Some('}') | Some(']') | None
                                        ) {
                                            in_string = false;
                                            out.push('"');
                                        } else {
                                            out.push_str("\\\"");
                                        }
                                    }
                                } else {
                                    in_string = true;
                                    out.push('"');
                                }
                            } else if ch == '\n' && in_string {
                                out.push_str("\\n");
                            } else {
                                out.push(ch);
                            }
                            i += 1;
                        }
                        match serde_json::from_str::<Value>(&out) {
                            Ok(ref cmd)
                                if cmd.get("action").and_then(|v| v.as_str())
                                    == Some("suspend") =>
                            {
                                eprintln!("inbox: session suspended ({label})");
                                None
                            }
                            Ok(ref cmd)
                                if cmd.get("action").and_then(|v| v.as_str())
                                    == Some("respond") =>
                            {
                                let msg = cmd.get("message").and_then(|v| v.as_str()).unwrap_or("");
                                if !msg.is_empty() {
                                    eprintln!(
                                        "inbox: agent reply ({label}): {}...",
                                        msg.chars().take(100).collect::<String>()
                                    );
                                    if route.can_reply() {
                                        let mut payload = serde_json::json!({
                                            "node_id": route.reply_node,
                                            "target": route.reply_target,
                                            "message": msg,
                                        });
                                        if let Some(rt) = &route.reply_to {
                                            payload["reply_to"] = serde_json::json!(rt);
                                        }
                                        match host.invoke(
                                            &route.source_plugin,
                                            &route.reply_node,
                                            payload.to_string(),
                                        ) {
                                            Ok(_) => eprintln!(
                                                "inbox: {}::{} OK ({label})",
                                                route.source_plugin, route.reply_node
                                            ),
                                            Err(e) => eprintln!(
                                                "inbox: {}::{} failed ({label}): {e}",
                                                route.source_plugin, route.reply_node
                                            ),
                                        }
                                    } else {
                                        eprintln!("inbox: no reply route for session {session_key}, dropping reply ({label})");
                                    }
                                }
                                None
                            }
                            Ok(ref cmd) => {
                                let action =
                                    cmd.get("action").and_then(|v| v.as_str()).unwrap_or("?");
                                eprintln!(
                                    "inbox: unknown JSON action={action}, dropping raw={}...",
                                    raw.chars().take(200).collect::<String>().replace('\n', " ")
                                );
                                cordis_runtime::kernel::notify::send(&host, &format!("[{session_key}] ⚠️ 回复异常（未知动作: {action}），正在重试..."));
                                Some(format!("SYSTEM: Your last output was valid JSON but had unknown action \"{action}\". Allowed actions: \"suspend\" or \"respond\". Please retry.\n\nYour raw output was:\n{raw}"))
                            }
                            Err(e) => {
                                eprintln!(
                                    "inbox: JSON parse failed: {e} — raw={}... preprocessed={}...",
                                    raw.chars().take(200).collect::<String>().replace('\n', " "),
                                    out.chars().take(200).collect::<String>().replace('\n', " ")
                                );
                                cordis_runtime::kernel::notify::send(
                                    &host,
                                    &format!("[{session_key}] ⚠️ 回复格式异常，正在重试...（{e}）"),
                                );
                                Some(format!("SYSTEM: Your last output was not valid JSON and was dropped. Parse error: {e}\n\nPlease fix the JSON formatting and retry. Final output must be exactly {{\"action\":\"suspend\"}} or {{\"action\":\"respond\",\"message\":\"...\"}}.\n\nYour raw output was:\n{raw}"))
                            }
                        }
                    };
                    match host.agent_send_with_fallback(sid, &combined) {
                        Ok(reply) => {
                            pending::clear(&data_dir, session_key);
                            let feedback = process(reply.content.trim().to_string(), "inbox");
                            if let Some(fb) = feedback {
                                if let Ok(reply2) = host.agent_send_with_fallback(sid, &fb) {
                                    process(reply2.content.trim().to_string(), "retry");
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("inbox: {e}");
                            // M批: the message must survive the outage. Spill
                            // it to disk (replayed on the next inbound batch)
                            // and tell the user via a FIXED template — this
                            // receipt path must never depend on the LLM.
                            // Only the NORMAL messages are spilled: commands
                            // were already handled above the LLM call.
                            let batch_only = normals
                                .iter()
                                .map(|e| e.display.as_str())
                                .collect::<Vec<_>>()
                                .join("\n");
                            pending::save(
                                &data_dir,
                                &pending::PendingMessage {
                                    session_key: session_key.clone(),
                                    combined: batch_only,
                                    enqueued_at_ms: std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .map(|d| d.as_millis() as u64)
                                        .unwrap_or(0),
                                },
                            );
                            if route.can_reply() {
                                let payload = serde_json::json!({
                                    "node_id": route.reply_node,
                                    "target": route.reply_target,
                                    "message": "我暂时无法思考（模型服务不可用），你的消息已收到，恢复后会回复你。",
                                });
                                if let Err(re) = host.invoke(
                                    &route.source_plugin,
                                    &route.reply_node,
                                    payload.to_string(),
                                ) {
                                    eprintln!("inbox: outage receipt failed: {re}");
                                }
                            }
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

    // spill → load → clear 往返；连续失败合并不覆盖。
    #[test]
    fn pending_roundtrip_and_merge() {
        let temp = std::env::temp_dir().join(format!("cordis-pending-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        let key = "feishu:chat:oc_x";
        assert!(pending::load(&temp, key).is_none());
        pending::save(
            &temp,
            &pending::PendingMessage {
                session_key: key.to_string(),
                combined: "第一条".to_string(),
                enqueued_at_ms: 100,
            },
        );
        pending::save(
            &temp,
            &pending::PendingMessage {
                session_key: key.to_string(),
                combined: "第二条".to_string(),
                enqueued_at_ms: 200,
            },
        );
        let loaded = pending::load(&temp, key).expect("pending should exist");
        assert_eq!(loaded.combined, "第一条\n第二条");
        assert_eq!(loaded.enqueued_at_ms, 100, "保留最早时间戳");
        pending::clear(&temp, key);
        assert!(pending::load(&temp, key).is_none());
        let _ = std::fs::remove_dir_all(&temp);
    }

    // session_key 含特殊字符不能逃出 pending 目录。
    #[test]
    fn pending_path_sanitizes_key() {
        let dir = std::path::Path::new("/data");
        let p = pending::path_for(dir, "../../etc/passwd");
        assert!(p.starts_with("/data/pending/"), "path: {}", p.display());
        assert!(!p.to_string_lossy().contains(".."), "path: {}", p.display());
    }
}

#[cfg(test)]
mod batch_tests {
    use super::{extract_user_text, partition_batch, AgentEnvelope, BatchItem};

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
        assert_eq!(route.soul_key(), "C#group");
    }
}
