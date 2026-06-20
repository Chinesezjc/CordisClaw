use cordis_runtime::agent::ShellAgentReply;
use cordis_runtime::context::ContextRegistry;
use cordis_runtime::host::{
    AgentSessionKind, KernelApplyRequest, RuntimeHost,
};
use cordis_runtime::kernel::auto_update::{
    AutoUpdatePlan, AutoUpdater, FilePatch, VerificationEnvelope,
};
use cordis_runtime::kernel::evaluator::VerificationInput;
use cordis_runtime::kernel::plugin_iteration::KernelPluginIterationRequest;
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

static mut AGENT_TRIGGER_TX: Option<std::sync::mpsc::Sender<String>> = None;

#[no_mangle]
pub extern "C" fn _cordis_agent_trigger(msg: *const std::ffi::c_char) {
    if msg.is_null() { return; }
    let s = unsafe { std::ffi::CStr::from_ptr(msg).to_string_lossy().to_string() };
    let _ = std::fs::write("/tmp/trigger_called.txt", &s);
    unsafe {
        if let Some(ref tx) = AGENT_TRIGGER_TX {
            let _ = tx.send(s.clone());
        }
    }
}

extern "C" fn sigterm_to_sigint(_sig: libc::c_int) {
    unsafe { libc::raise(libc::SIGINT); }
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
    let (root, runtime_only) = parse_root_and_runtime_only(args, "fixtures")?;
    prepare_fixtures_root(&root, runtime_only)?;
    let host = std::sync::Arc::new(
        RuntimeHost::boot(&root).map_err(|err| runtime_mode_error(err, &root, runtime_only))?,
    );
    let agent_session = host.agent_start(AgentSessionKind::RuntimeShell)?;
    let session_id = agent_session.session_id.clone();
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
            std::process::exit(0);
        })
        .ok();
    }
    // SIGTERM → SIGINT so systemctl stop triggers graceful shutdown.
    unsafe {
        libc::signal(libc::SIGTERM, sigterm_to_sigint as usize);
    }

    // ── Startup invocations ──────────────────────────────────────────────
    // Read startup_invoke.json from fixtures root and execute each
    // invocation before entering the REPL.  This is used to start
    // background services (e.g. qq_serve HTTP server).
    let startup_file = root.join("startup_invoke.json");
    if startup_file.exists() {
        match fs::read_to_string(&startup_file) {
            Ok(text) => match serde_json::from_str::<Value>(&text) {
                Ok(Value::Array(items)) => {
                    for item in &items {
                        let plugin_path = item["plugin_path"].as_str().unwrap_or("");
                        let node_id = item["node_id"].as_str().unwrap_or("");
                        let payload = item["payload"].as_object()
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
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let inject_queue = std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::<String>::new()));
        unsafe {
            AGENT_TRIGGER_TX = Some(tx);
        }
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
                // Group by group_id so replies never leak across groups.
                let mut by_group: BTreeMap<String, Vec<String>> = BTreeMap::new();
                for msg in msgs {
                    let gid = msg.strip_prefix("[QQ group from ")
                        .and_then(|rest| rest.find("]: ").map(|end| &rest[..end]))
                        .map(|prefix| prefix.split_once(" (user ").map(|(g, _)| g).unwrap_or(prefix).to_string())
                        .unwrap_or_default();
                    if !gid.is_empty() {
                        by_group.entry(gid).or_default().push(msg);
                    }
                }
                for (group_id, group_msgs) in &by_group {
                    if group_id.is_empty() || group_msgs.is_empty() { continue; }
                    let combined = group_msgs.join("\n");
                    eprintln!("inbox: [{group_id}] batch {} msgs", group_msgs.len());
                    // Push any messages that arrived while we were busy
                    // into the inject queue so the agent's respond() loop
                    // can see them via drain_inject_queue() between turns.
                    while let Ok(late) = rx.try_recv() {
                        inject_queue.lock().unwrap_or_else(|p| p.into_inner()).push_back(late);
                    }
                    let sid = sessions.entry(group_id.clone())
                        .or_insert_with(|| host.agent_start(AgentSessionKind::RuntimeShell)
                            .map(|s| s.session_id).unwrap_or_default());
                    // Process agent output: parse JSON, dispatch action, send to QQ.
                    // Returns Some(feedback) if the agent needs to retry with a corrected output.
                    let mut process = |raw: String, label: &str| -> Option<String> {
                        if raw.is_empty() { return None; }
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
                                    if already_escaped { out.push('"'); }
                                    else {
                                        let mut j = i + 1;
                                        while j < chars.len() && chars[j] == ' ' { j += 1; }
                                        let next = chars.get(j).copied();
                                        if matches!(next, Some(':') | Some(',') | Some('}') | Some(']') | None) {
                                            in_string = false; out.push('"');
                                        } else { out.push_str("\\\""); }
                                    }
                                } else { in_string = true; out.push('"'); }
                            } else if ch == '\n' && in_string { out.push_str("\\n"); }
                            else { out.push(ch); }
                            i += 1;
                        }
                        match serde_json::from_str::<Value>(&out) {
                            Ok(ref cmd) if cmd.get("action").and_then(|v| v.as_str()) == Some("suspend") => {
                                eprintln!("inbox: session suspended ({label})");
                                None
                            }
                            Ok(ref cmd) if cmd.get("action").and_then(|v| v.as_str()) == Some("respond") => {
                                let msg = cmd.get("message").and_then(|v| v.as_str()).unwrap_or("");
                                if !msg.is_empty() {
                                    eprintln!("inbox: agent reply ({label}): {}...", msg.chars().take(100).collect::<String>());
                                    let payload = serde_json::json!({
                                        "node_id": "qq_send",
                                        "target": format!("group:{}", group_id),
                                        "message": msg,
                                    });
                                    match host.invoke("qq", "qq_send", payload.to_string()) {
                                        Ok(_) => eprintln!("inbox: qq_send OK ({label})"),
                                        Err(e) => eprintln!("inbox: qq_send failed ({label}): {e}"),
                                    }
                                }
                                None
                            }
                            Ok(ref cmd) => {
                                let action = cmd.get("action").and_then(|v| v.as_str()).unwrap_or("?");
                                eprintln!("inbox: unknown JSON action={action}, dropping raw={}...", raw.chars().take(200).collect::<String>().replace('\n', " "));
                                cordis_runtime::kernel::notify::send(&host, &format!("[{group_id}] ⚠️ 回复异常（未知动作: {action}），正在重试..."));
                                Some(format!("SYSTEM: Your last output was valid JSON but had unknown action \"{action}\". Allowed actions: \"suspend\" or \"respond\". Please retry.\n\nYour raw output was:\n{raw}"))
                            }
                            Err(e) => {
                                eprintln!("inbox: JSON parse failed: {e} — raw={}... preprocessed={}...", raw.chars().take(200).collect::<String>().replace('\n', " "), out.chars().take(200).collect::<String>().replace('\n', " "));
                                cordis_runtime::kernel::notify::send(&host, &format!("[{group_id}] ⚠️ 回复格式异常，正在重试...（{e}）"));
                                Some(format!("SYSTEM: Your last output was not valid JSON and was dropped. Parse error: {e}\n\nPlease fix the JSON formatting and retry. Final output must be exactly {{\"action\":\"suspend\"}} or {{\"action\":\"respond\",\"message\":\"...\"}}.\n\nYour raw output was:\n{raw}"))
                            }
                        }
                    };
                    match host.agent_send(sid, &combined) {
                        Ok(reply) => {
                            let feedback = process(reply.content.trim().to_string(), "inbox");
                            if let Some(fb) = feedback {
                                if let Ok(reply2) = host.agent_send(sid, &fb) {
                                    process(reply2.content.trim().to_string(), "retry");
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("inbox: {e}");
                            cordis_runtime::kernel::notify::send(
                                &host,
                                &format!("[{group_id}] ⚠️ LLM 请求失败: {e}"),
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
        cordis_runtime::kernel::health::start_health_loop(health_host, 3600);

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

        match handled {
            Ok(true) => {}
            Ok(false) => break,
            Err(err) => {
                println!("serve error: {err}");
                io::stdout().flush()?;
            }
        }
        // Persist history after every command.
        let _ = rl.save_history(&history_path);
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
                println!("{}", serde_json::to_string(&result)?);
            } else if let Some(json) = command.strip_prefix("kernel iterate-plugins ") {
                let request: KernelPluginIterationRequest = serde_json::from_str(json)?;
                let result = host.iterate_plugins(request)?;
                println!("{}", serde_json::to_string(&result)?);
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
                        let reverted = host
                            .revert_interactive_changes()
                            .unwrap_or(0);
                        let saved =
                            save_draft_and_revert(host.fixtures_root(), "error");
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

/// Save the current git diff as a draft patch in `.cordis-drafts/`, then
/// revert all modified and untracked files under `plugins/`.  Used both by
/// agent-error recovery and by the Ctrl+C handler.
fn save_draft_and_revert(fixtures_root: &Path, reason: &str) -> Option<String> {
    let draft_dir = fixtures_root.join(".cordis-drafts");
    let _ = std::fs::create_dir_all(&draft_dir);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let draft_path = draft_dir.join(format!("draft-{ts}-{reason}.patch"));
    let diff = std::process::Command::new("sh")
        .arg("-c")
        .arg("git diff -- plugins/ 2>/dev/null")
        .current_dir(fixtures_root)
        .output()
        .ok()?;
    let patch = String::from_utf8_lossy(&diff.stdout);
    if patch.trim().is_empty() {
        return None;
    }
    std::fs::write(&draft_path, patch.as_bytes()).ok()?;
    // Revert modified files and remove untracked files.
    let _ = std::process::Command::new("sh")
        .arg("-c")
        .arg("git checkout -- plugins/ 2>/dev/null")
        .current_dir(fixtures_root)
        .output();
    let _ = std::process::Command::new("sh")
        .arg("-c")
        .arg("git clean -fd -- plugins/ 2>/dev/null")
        .current_dir(fixtures_root)
        .output();
    Some(format!(
        ".cordis-drafts/draft-{ts}-{reason}.patch"
    ))
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
    for (_key, trace) in &response.traces {
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
        println!("{}", serde_json::to_string_pretty(&json!({
            "dry_run": true,
            "message": "agent loop dry-run",
            "paths": paths,
            "target_plugin_paths": request.target_plugin_paths,
        }))?);
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

fn parse_root_and_runtime_only(
    args: &[String],
    default_root: &str,
) -> Result<(PathBuf, bool), Box<dyn std::error::Error>> {
    let mut root = PathBuf::from(default_root);
    let mut runtime_only = false;
    let mut seen_root = false;

    for token in args {
        if token == "--runtime-only" {
            runtime_only = true;
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

    Ok((root, runtime_only))
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
  cargo run -p cordis-runtime -- serve [fixtures_root] [--runtime-only]
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
  cargo run -p cordis-runtime -- rebuild-fixture-artifacts [fixtures_root]"
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
