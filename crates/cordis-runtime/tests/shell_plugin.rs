use cordis_runtime::plugin::abi::{PluginRequest, RuntimePlugin};
use cordis_runtime::plugin::shell::{ShellPlugin, ShellPluginResponsePayload};

#[test]
fn shell_plugin_start_terminal_success() {
    let mut plugin = ShellPlugin::default();
    let request = PluginRequest {
        payload: r#"{
            "action":"start_terminal",
            "command":"echo hello"
        }"#
        .to_string(),
    };

    let response = plugin.handle(request);
    let parsed: ShellPluginResponsePayload =
        serde_json::from_str(&response.payload).expect("valid response json");
    assert!(parsed.ok);
    assert_eq!(parsed.action, "start_terminal");
    assert_eq!(parsed.exit_code, Some(0));
    assert_eq!(parsed.output.as_deref(), Some("hello"));
}

#[test]
fn shell_plugin_expr_command_outputs_value() {
    let mut plugin = ShellPlugin::default();
    let request = PluginRequest {
        payload: r#"{
            "action":"start_terminal",
            "command":"Expr 1 + 2 * 3"
        }"#
        .to_string(),
    };

    let response = plugin.handle(request);
    let parsed: ShellPluginResponsePayload =
        serde_json::from_str(&response.payload).expect("valid response json");
    assert!(parsed.ok);
    assert_eq!(parsed.exit_code, Some(0));
    assert_eq!(parsed.output.as_deref(), Some("Value: 7"));
}

#[test]
fn shell_plugin_start_terminal_non_zero_exit() {
    let mut plugin = ShellPlugin::default();
    let request = PluginRequest {
        payload: r#"{
            "action":"start_terminal",
            "command":"no_such_command"
        }"#
        .to_string(),
    };

    let response = plugin.handle(request);
    let parsed: ShellPluginResponsePayload =
        serde_json::from_str(&response.payload).expect("valid response json");
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
    let mut plugin = ShellPlugin::default();
    let request = PluginRequest {
        payload: r#"{"action":"unknown_action"}"#.to_string(),
    };

    let response = plugin.handle(request);
    let parsed: ShellPluginResponsePayload =
        serde_json::from_str(&response.payload).expect("valid response json");
    assert!(!parsed.ok);
    assert_eq!(parsed.action, "error");
    assert!(parsed.message.contains("unsupported action"));
}

#[test]
fn shell_plugin_sets_username_to_cordisclaw() {
    let mut plugin = ShellPlugin::default();
    let request = PluginRequest {
        payload: r#"{
            "action":"start_terminal",
            "command":"whoami"
        }"#
        .to_string(),
    };

    let response = plugin.handle(request);
    let parsed: ShellPluginResponsePayload =
        serde_json::from_str(&response.payload).expect("valid response json");
    assert!(parsed.ok, "expected whoami to be CordisClaw, got: {parsed:?}");
    assert_eq!(parsed.exit_code, Some(0));
    assert_eq!(parsed.output.as_deref(), Some("CordisClaw"));
}

#[test]
fn shell_plugin_rejects_external_shell_backend() {
    let mut plugin = ShellPlugin::default();
    let request = PluginRequest {
        payload: r#"{
            "action":"start_terminal",
            "shell":"/bin/bash",
            "command":"echo hi"
        }"#
        .to_string(),
    };

    let response = plugin.handle(request);
    let parsed: ShellPluginResponsePayload =
        serde_json::from_str(&response.payload).expect("valid response json");
    assert!(!parsed.ok);
    assert_eq!(parsed.action, "error");
    assert!(parsed.message.contains("only builtin shell is supported"));
}
