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
    assert!(
        parsed
            .output
            .as_deref()
            .unwrap_or_default()
            .contains("command not found"),
    );
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
    assert!(parsed.ok, "expected whoami to be CordisClaw, got: {parsed:?}");
    assert_eq!(parsed.exit_code, Some(0));
    assert_eq!(parsed.output.as_deref(), Some("CordisClaw"));
}

#[test]
fn shell_plugin_rejects_external_shell_backend() {
    let parsed = invoke_shell(
        r#"{"action":"start_terminal","shell":"/bin/bash","command":"echo hi"}"#,
    );
    assert!(!parsed.ok);
    assert_eq!(parsed.action, "error");
    assert!(parsed.message.contains("only builtin shell is supported"));
}

#[test]
fn invoke_cli_runs_interactive_shell_session() {
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
    assert!(output.status.success(), "stderr: {}", String::from_utf8_lossy(&output.stderr));

    let stdout = String::from_utf8(output.stdout).expect("stdout utf-8");
    assert!(stdout.contains("CordisClaw@runtime:"));
    assert!(stdout.contains("CordisClaw"));
    assert!(stdout.contains("invoke ok=true"));
}
