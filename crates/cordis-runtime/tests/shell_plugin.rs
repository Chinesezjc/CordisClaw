use cordis_runtime::core::models::PluginLoadResult;
use cordis_runtime::plugin::invoke::PluginInvoker;
use serde::Deserialize;
use std::io::Write;
use std::process::{Command, Stdio};

mod support;

use support::fixtures_root;

#[derive(Debug, Deserialize)]
struct ShellResponse {
    ok: bool,
    action: String,
    shell: Option<String>,
    exit_code: Option<i32>,
    message: String,
    #[serde(default)]
    output: Option<String>,
}

fn invoke_shell(payload: &str) -> ShellResponse {
    let invoker = PluginInvoker::load(fixtures_root()).expect("fixtures should load");
    let plugin = invoker
        .plugin_registry()
        .get("shell")
        .expect("shell plugin should exist");
    assert!(matches!(plugin.load_result, PluginLoadResult::Loaded));

    let response = invoker
        .invoke("shell", "shell_entry", payload.to_string())
        .expect("shell invoke should succeed");
    serde_json::from_str(&response.payload).expect("valid shell response")
}

#[test]
fn shell_plugin_is_loaded_externally() {
    let invoker = PluginInvoker::load(fixtures_root()).expect("fixtures should load");
    let plugin = invoker
        .plugin_registry()
        .get("shell")
        .expect("shell plugin should be registered");
    assert!(matches!(plugin.load_result, PluginLoadResult::Loaded));
    assert!(plugin.docs.is_some());
}

#[test]
fn shell_plugin_start_terminal_success() {
    let parsed = invoke_shell(r#"{"action":"start_terminal","command":"echo hello"}"#);
    assert!(parsed.ok);
    assert_eq!(parsed.action, "start_terminal");
    assert_eq!(parsed.shell.as_deref(), Some("cordis"));
    assert_eq!(parsed.exit_code, Some(0));
    assert_eq!(parsed.output.as_deref(), Some("hello"));
}

#[test]
fn shell_plugin_expr_command_outputs_value() {
    let parsed = invoke_shell(r#"{"action":"start_terminal","command":"Expr 1 + 2 * 3"}"#);
    assert!(parsed.ok);
    assert_eq!(parsed.exit_code, Some(0));
    assert_eq!(parsed.output.as_deref(), Some("Value: 7"));
}

#[test]
fn shell_plugin_start_terminal_non_zero_exit() {
    let parsed = invoke_shell(r#"{"action":"start_terminal","command":"no_such_command"}"#);
    assert!(!parsed.ok);
    assert_eq!(parsed.action, "start_terminal");
    assert_eq!(parsed.exit_code, Some(127));
    assert!(parsed
        .output
        .as_deref()
        .unwrap_or_default()
        .contains("command not found"),);
}

#[test]
fn shell_plugin_rejects_unknown_action() {
    let parsed = invoke_shell(r#"{"action":"unknown_action"}"#);
    assert!(!parsed.ok);
    assert_eq!(parsed.action, "error");
    assert!(parsed.message.contains("unsupported action"));
}

#[test]
fn shell_plugin_sets_username_to_cordisclaw() {
    let parsed = invoke_shell(r#"{"action":"start_terminal","command":"whoami"}"#);
    assert!(
        parsed.ok,
        "expected whoami to be CordisClaw, got: {parsed:?}"
    );
    assert_eq!(parsed.exit_code, Some(0));
    assert_eq!(parsed.output.as_deref(), Some("CordisClaw"));
}

#[test]
fn shell_plugin_rejects_external_shell_backend() {
    let parsed =
        invoke_shell(r#"{"action":"start_terminal","shell":"/bin/bash","command":"echo hi"}"#);
    assert!(!parsed.ok);
    assert_eq!(parsed.action, "error");
    assert!(parsed.message.contains("only builtin shell is supported"));
}

#[test]
fn invoke_cli_shell_repl_refuses_non_tty_stdin() {
    // P2-37: `shell_run_repl` now requires an interactive TTY on stdin so a
    // headless host can't hang waiting on a REPL that will never receive
    // keystrokes. This test drives `invoke shell shell_entry` with a PIPED
    // stdin (never a TTY under `cargo test`), so the plugin must fail fast
    // with a clear diagnostic rather than block or pretend to run a session.
    // (Was `invoke_cli_runs_interactive_shell_session`, which asserted the
    // pre-P2-37 behaviour of driving the REPL over a pipe — impossible now.)
    let bin = env!("CARGO_BIN_EXE_cordis-runtime");
    let mut child = Command::new(bin)
        .args([
            "invoke",
            "shell",
            "shell_entry",
            r#"--payload-json={"action":"start_terminal"}"#,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn invoke cli");

    let stdin = child.stdin.as_mut().expect("stdin pipe");
    stdin
        .write_all(b"whoami\nexit\n")
        .expect("write repl commands");

    let output = child.wait_with_output().expect("wait for invoke cli");
    assert!(
        !output.status.success(),
        "non-TTY stdin must make `invoke shell` exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("requires an interactive TTY"),
        "expected TTY-refusal diagnostic, got stderr: {stderr}"
    );
}
