//! N批: bypass-LLM command router.
//!
//! Messages starting with `/` are dispatched here by the inbox loop
//! BEFORE any agent/LLM involvement, and the reply goes back through the
//! envelope's existing reply route (`host.invoke(source_plugin,
//! reply_node, ...)`). This path is deliberately dumb — exact-match
//! command names, positional args, no fuzzy fallback to the LLM — so it
//! keeps working when the LLM API is completely down. It doubles as the
//! no-LLM management surface (/status works during a total outage).
//!
//! Kernel/Plugin boundary: builtins (status/help/soul) live here; plugins
//! join by declaring `command_name` in their docs plus a conventional
//! `command_entry` node — `/{command_name} args...` is routed to it.
//! Permission model (phase 1): channel policy IS the permission — anything
//! that reached the inbox already passed the channel plugin's policy gate,
//! and mutating builtins only touch the caller's own session/soul (scoped
//! by sender identity). An admin allowlist can layer on later.

use crate::host::RuntimeHost;
use serde_json::json;

/// Identity/scope context for a command invocation, extracted from the
/// message envelope by the inbox loop.
#[derive(Debug, Clone, Default)]
pub struct CommandContext {
    pub session_key: String,
    pub sender_id: String,
    pub conversation_kind: String,
    pub soul_key: String,
}

/// Outcome of dispatching a `/command` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandOutcome {
    /// Text to send back through the envelope's reply route.
    Reply(String),
    /// The caller's session should be reset (inbox owns the session map,
    /// so the actual reset happens there); the string is the reply text.
    ResetSession(String),
}

/// Split "/status extra args" into ("status", "extra args").
fn split_command(input: &str) -> (String, String) {
    let trimmed = input.trim().trim_start_matches('/');
    match trimmed.split_once(char::is_whitespace) {
        Some((name, rest)) => (name.to_lowercase(), rest.trim().to_string()),
        None => (trimmed.to_lowercase(), String::new()),
    }
}

/// List plugin-declared commands: plugins whose docs set `command_name`
/// AND expose a `command_entry` node. Returns (command_name, plugin_path).
fn plugin_commands(host: &RuntimeHost) -> Vec<(String, String)> {
    let snapshot = host.current_snapshot();
    let mut commands = Vec::new();
    for (plugin_path, plugin) in snapshot.plugin_registry().iter() {
        let Some(docs) = &plugin.docs else { continue };
        let Some(name) = &docs.command_name else {
            continue;
        };
        if name.trim().is_empty() {
            continue;
        }
        let has_entry = docs.nodes.iter().any(|n| n.id == "command_entry");
        if has_entry {
            commands.push((name.trim().to_lowercase(), plugin_path.clone()));
        }
    }
    commands.sort();
    commands
}

/// Dispatch a `/`-prefixed message. Never touches the LLM.
pub fn dispatch(host: &RuntimeHost, ctx: &CommandContext, input: &str) -> CommandOutcome {
    let (name, args) = split_command(input);
    match name.as_str() {
        "status" => CommandOutcome::Reply(status_text(host)),
        "help" => CommandOutcome::Reply(help_text(host)),
        "reset" => CommandOutcome::ResetSession(
            "会话已重置：历史已清空，下一条消息将开始新的对话。".to_string(),
        ),
        "soul" => CommandOutcome::Reply(soul_text(host, ctx)),
        "" => CommandOutcome::Reply(help_text(host)),
        _ => {
            // Plugin-declared command?
            for (cmd, plugin_path) in plugin_commands(host) {
                if cmd == name {
                    return dispatch_plugin_command(host, ctx, &plugin_path, &args);
                }
            }
            CommandOutcome::Reply(format!("未知指令 /{name}。输入 /help 查看可用指令。"))
        }
    }
}

fn dispatch_plugin_command(
    host: &RuntimeHost,
    ctx: &CommandContext,
    plugin_path: &str,
    args: &str,
) -> CommandOutcome {
    let payload = json!({
        "node_id": "command_entry",
        "action": "command",
        "payload": {
            "args": args,
            "session_key": ctx.session_key,
            "sender_id": ctx.sender_id,
            "conversation_kind": ctx.conversation_kind,
        },
    });
    match host.invoke(plugin_path, "command_entry", payload.to_string()) {
        Ok(response) => {
            // Prefer a human-readable `message` field; fall back to raw.
            let text = serde_json::from_str::<serde_json::Value>(&response.payload)
                .ok()
                .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(String::from))
                .unwrap_or(response.payload);
            CommandOutcome::Reply(text)
        }
        Err(e) => CommandOutcome::Reply(format!("指令执行失败: {e}")),
    }
}

fn status_text(host: &RuntimeHost) -> String {
    let status = host.status();
    let kernel = host.kernel().status();
    let issues = host.kernel().plugin_issues();
    let open_issues = issues.len();
    format!(
        "运行时状态\n\
         - snapshot: {}\n\
         - plugins: {} / nodes: {}\n\
         - kernel issues: {}\n\
         - plugin iterations: {}\n\
         (此回复不经 LLM，模型故障时也可用)",
        status.current_snapshot_id,
        status.plugin_count,
        status.node_count,
        open_issues,
        kernel.plugin_iteration_total,
    )
}

fn help_text(host: &RuntimeHost) -> String {
    let mut lines = vec![
        "可用指令（不经 LLM，直接执行）:".to_string(),
        "/status — 运行时状态".to_string(),
        "/reset — 重置当前会话历史".to_string(),
        "/soul — 查看当前会话的人格设置".to_string(),
        "/help — 本列表".to_string(),
    ];
    for (cmd, plugin_path) in plugin_commands(host) {
        lines.push(format!("/{cmd} — 插件指令（{plugin_path}）"));
    }
    lines.join("\n")
}

fn soul_text(host: &RuntimeHost, ctx: &CommandContext) -> String {
    if ctx.soul_key.is_empty() {
        return "当前会话没有身份信息，无法定位 soul。".to_string();
    }
    match host.get_soul(&ctx.soul_key) {
        Ok(Some(soul)) => {
            let persona_preview: String = soul.persona.chars().take(200).collect();
            let profile = soul.profile.as_deref().unwrap_or("default");
            format!(
                "当前 soul（作用域 {}）\n- persona: {}\n- LLM profile: {}\n(对 agent 说\"帮我修改人格设定\"即可更新；变更在 /reset 后的新会话生效)",
                ctx.soul_key,
                if persona_preview.is_empty() { "(未设置)" } else { &persona_preview },
                profile,
            )
        }
        Ok(None) => format!(
            "当前 soul（作用域 {}）尚未设置，使用默认人格。\n(对 agent 说\"帮我设置人格\"即可创建)",
            ctx.soul_key
        ),
        Err(e) => format!("读取 soul 失败: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::soul::Soul;

    #[test]
    fn split_command_variants() {
        assert_eq!(split_command("/status"), ("status".into(), String::new()));
        assert_eq!(
            split_command("/soul  set  foo"),
            ("soul".into(), "set  foo".into())
        );
        assert_eq!(split_command("/STATUS"), ("status".into(), String::new()));
        assert_eq!(split_command("/"), ("".into(), String::new()));
    }

    /// Boot a real `RuntimeHost` against an empty fixtures workspace (no
    /// dylibs, so it works on every host — unlike the fixture-backed
    /// integration test which is x86_64-linux only). Only the built-in
    /// kernel plugin is present, which is all the builtin commands need.
    fn boot_minimal() -> (tempfile::TempDir, RuntimeHost) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let fixtures = tmp.path().join("fixtures");
        std::fs::create_dir_all(fixtures.join("artifacts")).unwrap();
        std::fs::write(
            fixtures.join("artifacts/index.json"),
            r#"{"generated_at":"0","entries":[]}"#,
        )
        .unwrap();
        let host = RuntimeHost::boot(&fixtures).expect("boot minimal host");
        (tmp, host)
    }

    fn ctx_with_soul(key: &str) -> CommandContext {
        CommandContext {
            session_key: "sess".to_string(),
            sender_id: "sender".to_string(),
            conversation_kind: "private".to_string(),
            soul_key: key.to_string(),
        }
    }

    #[test]
    fn dispatch_status_reports_runtime_state() {
        let (_t, host) = boot_minimal();
        match dispatch(&host, &CommandContext::default(), "/status") {
            CommandOutcome::Reply(text) => {
                assert!(text.contains("运行时状态"), "text: {text}");
                assert!(text.contains("snapshot"), "text: {text}");
                assert!(text.contains("plugins"), "text: {text}");
                assert!(text.contains("不经 LLM"), "text: {text}");
            }
            other => panic!("expected Reply, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_help_lists_builtins() {
        let (_t, host) = boot_minimal();
        // "/help" and the empty-command fallback both yield the help text.
        for input in ["/help", "/"] {
            match dispatch(&host, &CommandContext::default(), input) {
                CommandOutcome::Reply(text) => {
                    assert!(text.contains("可用指令"), "text: {text}");
                    assert!(text.contains("/status"), "text: {text}");
                    assert!(text.contains("/reset"), "text: {text}");
                    assert!(text.contains("/soul"), "text: {text}");
                    assert!(text.contains("/help"), "text: {text}");
                }
                other => panic!("expected Reply for {input}, got {other:?}"),
            }
        }
    }

    #[test]
    fn dispatch_reset_yields_reset_session() {
        let (_t, host) = boot_minimal();
        match dispatch(&host, &CommandContext::default(), "/reset") {
            CommandOutcome::ResetSession(text) => {
                assert!(text.contains("会话已重置"), "text: {text}");
            }
            other => panic!("expected ResetSession, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_unknown_command_is_reply() {
        let (_t, host) = boot_minimal();
        match dispatch(&host, &CommandContext::default(), "/frobnicate now") {
            CommandOutcome::Reply(text) => {
                assert!(text.contains("未知指令"), "text: {text}");
                assert!(text.contains("/frobnicate"), "text: {text}");
                assert!(text.contains("/help"), "text: {text}");
            }
            other => panic!("expected Reply, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_soul_without_identity_explains_missing() {
        let (_t, host) = boot_minimal();
        // Empty soul_key → identity-less session branch.
        match dispatch(&host, &CommandContext::default(), "/soul") {
            CommandOutcome::Reply(text) => {
                assert!(
                    text.contains("没有身份") || text.contains("无法定位"),
                    "text: {text}"
                );
            }
            other => panic!("expected Reply, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_soul_unset_scope_reports_default() {
        let (_t, host) = boot_minimal();
        let ctx = ctx_with_soul("scope_never_set#private");
        match dispatch(&host, &ctx, "/soul") {
            CommandOutcome::Reply(text) => {
                assert!(text.contains("尚未设置"), "text: {text}");
                assert!(text.contains("默认人格"), "text: {text}");
            }
            other => panic!("expected Reply, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_soul_reads_persona_and_profile() {
        let (_t, host) = boot_minimal();
        let key = "scope_with_soul#private";
        host.set_soul(
            key,
            &Soul {
                persona: "运维值班助手".to_string(),
                profile: Some("fast".to_string()),
                updated_at_ms: 1,
                updated_by: "test".to_string(),
            },
        )
        .unwrap();
        match dispatch(&host, &ctx_with_soul(key), "/soul") {
            CommandOutcome::Reply(text) => {
                assert!(text.contains("运维值班助手"), "text: {text}");
                assert!(text.contains("fast"), "text: {text}");
                assert!(text.contains(key), "text: {text}");
            }
            other => panic!("expected Reply, got {other:?}"),
        }
    }

    #[test]
    fn dispatch_soul_empty_persona_shows_placeholder() {
        let (_t, host) = boot_minimal();
        let key = "scope_empty_persona#private";
        // Stored soul exists but persona is blank → "(未设置)" placeholder,
        // and profile None falls back to "default".
        host.set_soul(key, &Soul::default()).unwrap();
        match dispatch(&host, &ctx_with_soul(key), "/soul") {
            CommandOutcome::Reply(text) => {
                assert!(text.contains("(未设置)"), "text: {text}");
                assert!(text.contains("default"), "text: {text}");
            }
            other => panic!("expected Reply, got {other:?}"),
        }
    }

    #[test]
    fn plugin_commands_empty_without_declared_commands() {
        let (_t, host) = boot_minimal();
        // No fixture plugins loaded → no plugin-declared commands, and
        // /help therefore lists only the builtins.
        assert!(plugin_commands(&host).is_empty());
    }

    /// Read-error branch of `/soul`: a soul_key whose on-disk file is a
    /// directory makes `FileSoulProvider::get` return a non-NotFound Io
    /// error, which `soul_text` renders as "读取 soul 失败".
    #[test]
    fn dispatch_soul_read_error_is_reported() {
        let (_t, host) = boot_minimal();
        let key = "scope_read_err#private";
        // Place a directory exactly where the provider would read the soul
        // JSON file, forcing a read I/O error (not NotFound).
        let souls = host.data_dir().join("souls");
        std::fs::create_dir_all(&souls).unwrap();
        std::fs::create_dir_all(
            souls.join(format!("{}.json", crate::soul::sanitize_soul_key(key))),
        )
        .unwrap();
        match dispatch(&host, &ctx_with_soul(key), "/soul") {
            CommandOutcome::Reply(text) => {
                assert!(text.contains("读取 soul 失败"), "text: {text}");
            }
            other => panic!("expected Reply, got {other:?}"),
        }
    }

    /// Register a synthetic Loaded plugin that declares a `command_name` plus
    /// a conventional `command_entry` node, backed by a JSON artifact whose
    /// execution is a stock system binary. This is the only way to exercise
    /// `plugin_commands` / `dispatch_plugin_command` without an x86_64-linux
    /// dylib, since no cross-platform fixture ships a `command_entry` node.
    fn register_command_plugin(
        host: &RuntimeHost,
        tmp: &std::path::Path,
        plugin_path: &str,
        command_name: &str,
        command: &str,
        args: &[&str],
    ) {
        use crate::core::models::{ArtifactKind, PluginExecution};
        use cordis_plugin_sdk::{AbiFingerprint, NodeDoc, NodeType, PluginDocs};

        let artifact = tmp.join(format!("{}.json", command_name.trim().to_lowercase()));
        std::fs::write(&artifact, "{}").unwrap();

        let docs = PluginDocs {
            plugin_id: plugin_path.replace('/', "_"),
            plugin_path: plugin_path.to_string(),
            plugin_version: "0.1.0".to_string(),
            abi_version: 2,
            command_name: Some(command_name.to_string()),
            nodes: vec![NodeDoc {
                id: "command_entry".to_string(),
                summary: "bypass-LLM command entry".to_string(),
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: serde_json::json!({"type": "object"}),
                side_effects: vec![],
                failure_modes: vec![],
                node_type: NodeType::Router,
                agent_accessible: true,
            }],
            system_hint: None,
        };

        host.current_snapshot().plugin_registry().insert_loaded(
            plugin_path.to_string(),
            None,
            true,
            std::collections::BTreeSet::new(),
            docs,
            artifact,
            ArtifactKind::Json,
            AbiFingerprint::current_build("crate_cmd_v1", "api_v2"),
            Some(PluginExecution::Process {
                command: command.to_string(),
                args: args.iter().map(|s| s.to_string()).collect(),
            }),
        );
    }

    // Plugin-declared command dispatch: the plugin echoes a JSON body with a
    // `message` field, and `dispatch_plugin_command` prefers that field.
    #[test]
    fn dispatch_plugin_command_prefers_message_field() {
        let (tmp, host) = boot_minimal();
        // `printf` writes a fixed JSON payload to stdout regardless of stdin,
        // giving a deterministic `{"message": ...}` reply.
        register_command_plugin(
            &host,
            tmp.path(),
            "plugins/echocmd",
            "Echo",
            "/usr/bin/printf",
            &[r#"{"message":"插件已处理"}"#],
        );

        // The command name is matched case-insensitively (/echo -> "echo").
        match dispatch(&host, &CommandContext::default(), "/echo hello world") {
            CommandOutcome::Reply(text) => {
                assert_eq!(text, "插件已处理", "should surface the message field");
            }
            other => panic!("expected Reply, got {other:?}"),
        }

        // The declared command also appears in /help alongside the builtins.
        match dispatch(&host, &CommandContext::default(), "/help") {
            CommandOutcome::Reply(text) => {
                assert!(
                    text.contains("/echo"),
                    "help should list plugin cmd: {text}"
                );
                assert!(
                    text.contains("plugins/echocmd"),
                    "help should name the plugin path: {text}"
                );
            }
            other => panic!("expected Reply, got {other:?}"),
        }

        // plugin_commands surfaces exactly the registered command.
        let cmds = plugin_commands(&host);
        assert_eq!(
            cmds,
            vec![("echo".to_string(), "plugins/echocmd".to_string())]
        );
    }

    // When the plugin reply is not a JSON object with a `message` field, the
    // raw payload is returned verbatim.
    #[test]
    fn dispatch_plugin_command_falls_back_to_raw_payload() {
        let (tmp, host) = boot_minimal();
        register_command_plugin(
            &host,
            tmp.path(),
            "plugins/rawcmd",
            "Raw",
            "/usr/bin/printf",
            &["just text, not json"],
        );
        match dispatch(&host, &CommandContext::default(), "/raw") {
            CommandOutcome::Reply(text) => {
                assert_eq!(text, "just text, not json");
            }
            other => panic!("expected Reply, got {other:?}"),
        }
    }

    // Plugin command whose backing process fails to spawn → the invoke error
    // is rendered through the "指令执行失败" branch.
    #[test]
    fn dispatch_plugin_command_invoke_failure_is_reported() {
        let (tmp, host) = boot_minimal();
        register_command_plugin(
            &host,
            tmp.path(),
            "plugins/brokencmd",
            "Broken",
            "/nonexistent/binary/xyzzy",
            &[],
        );
        match dispatch(&host, &CommandContext::default(), "/broken") {
            CommandOutcome::Reply(text) => {
                assert!(text.contains("指令执行失败"), "text: {text}");
            }
            other => panic!("expected Reply, got {other:?}"),
        }
    }

    // A plugin that sets an empty/whitespace command_name is skipped by
    // `plugin_commands` (the `name.trim().is_empty()` guard).
    #[test]
    fn plugin_commands_skips_blank_command_name() {
        let (tmp, host) = boot_minimal();
        register_command_plugin(
            &host,
            tmp.path(),
            "plugins/blankcmd",
            "   ",
            "/usr/bin/printf",
            &["{}"],
        );
        assert!(
            plugin_commands(&host).is_empty(),
            "blank command_name must be ignored"
        );
    }
}
