//! Coverage-focused integration tests for `RuntimeHost` / `RuntimeKernel`.
//!
//! Unlike `runtime_host.rs`, these tests do NOT gate behind
//! `linux_dylib_artifacts_available()`. `ensure_fixture_artifacts` rebuilds the
//! fixture dylibs for the *current* host target (arm64 macOS included) and
//! rewrites `artifacts/index.json` with the local target triple, so a real
//! `RuntimeHost::boot` against the fixtures tree succeeds here. Read-only
//! surface is exercised against the shared real fixtures (the loader only
//! reads `artifacts/`; the snapshot root lives in the OS temp dir), while any
//! path that mutates the fixtures tree runs against a throwaway copy.

use cordis_runtime::agent::AgentToolHost;
use cordis_runtime::core::error::RuntimeError;
use cordis_runtime::core::models::NodeOutcome;
use cordis_runtime::host::{
    AgentSessionKind, AgentStartOptions, ReloadAttemptStatus, RuntimeHost, RuntimeKernel,
    WalkControl,
};
use cordis_runtime::kernel::auto_update::{AutoUpdatePlan, FilePatch};
use cordis_runtime::kernel::evaluator::VerificationInput;
use cordis_runtime::kernel::plugin_iteration::{
    KernelPluginIssueSource, KernelPluginIterationRequest, PluginEditOpKind, PluginEditOperation,
    PluginEditPlan, PluginIterationFinalVerdict,
};
use cordis_runtime::kernel::verifier::VerificationProfile;
use cordis_runtime::soul::Soul;
use serde_json::{json, Value};
use serial_test::serial;
use std::fs;
use std::path::Path;
use std::sync::OnceLock;
use tempfile::TempDir;

mod support;
use support::fixtures_root;

// ---------------------------------------------------------------------------
// Shared read-only host against the real fixtures tree.
//
// Booting is comparatively cheap (the loader only reads `artifacts/`), but we
// still only want to pay for it once for the read-only tests that never mutate
// the tree. `serial` on those tests keeps them from racing each other's use of
// the shared temp snapshot root inside the kernel.
// ---------------------------------------------------------------------------

static SHARED_HOST: OnceLock<RuntimeHost> = OnceLock::new();

fn shared_host() -> &'static RuntimeHost {
    SHARED_HOST.get_or_init(|| RuntimeHost::boot(fixtures_root()).expect("host should boot"))
}

fn copy_dir_all(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).expect("create destination");
    for entry in fs::read_dir(src).expect("read dir") {
        let entry = entry.expect("dir entry");
        let ty = entry.file_type().expect("file type");
        if ty.is_dir() && (entry.file_name() == "target") {
            continue;
        }
        let target = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).expect("copy file");
        }
    }
}

/// Copy only the `artifacts/` tree plus top-level config-ish files into a
/// fresh temp dir. This is enough for a read-only boot (the loader consumes
/// `artifacts/index.json` and the staged dylibs); it deliberately skips the
/// multi-gigabyte `plugins/` source tree that lives on the network mount.
fn setup_artifacts_only_copy() -> TempDir {
    let temp = TempDir::new().expect("tempdir");
    let root = fixtures_root();
    copy_dir_all(&root.join("artifacts"), &temp.path().join("artifacts"));
    for name in ["notify_handlers.json", "startup_invoke.json"] {
        let src = root.join(name);
        if src.exists() {
            fs::copy(&src, temp.path().join(name)).expect("copy top-level fixture file");
        }
    }
    temp
}

// ===========================================================================
// Group A — RuntimeKernel unit surface (no fixtures; tiny local temp dir).
// ===========================================================================

#[test]
fn kernel_status_reflects_default_config_and_empty_state() {
    let temp = TempDir::new().expect("tempdir");
    let config = cordis_runtime::config::RuntimeConfig::default();
    let kernel = RuntimeKernel::new(temp.path(), &config);

    let status = kernel.status();
    assert_eq!(status.workspace_root, temp.path().display().to_string());
    assert_eq!(status.iteration_total, 0);
    assert_eq!(status.iteration_promote_total, 0);
    assert_eq!(status.iteration_rollback_total, 0);
    assert_eq!(status.history_len, 0);
    assert!(status.last_change.is_none());
    assert_eq!(status.plugin_issue_count, 0);
    assert_eq!(status.blocked_iteration_count, 0);
    assert_eq!(status.plugin_iteration_total, 0);
    assert!(status.last_plugin_iteration.is_none());
    assert!(kernel.history().is_empty());
    assert!(kernel.plugin_issues().is_empty());
    assert!(kernel.plugin_history().is_empty());
    assert!(kernel.blocked_iterations().is_empty());
}

#[test]
fn kernel_run_iteration_promotes_and_records_change() {
    let temp = TempDir::new().expect("tempdir");
    let target = temp.path().join("notes.txt");
    fs::write(&target, "alpha-old-omega").expect("seed patch target");
    let config = cordis_runtime::config::RuntimeConfig::default();
    let kernel = RuntimeKernel::new(temp.path(), &config);

    let result = kernel
        .run_iteration(
            AutoUpdatePlan {
                issue_id: "issue-promote".to_string(),
                patch_id: "patch-promote".to_string(),
                manual_approved: false,
                diff_lines: 1,
                patches: vec![FilePatch::text("notes.txt", "old", "new")],
            },
            VerificationInput {
                tests_passed: true,
                safety_checks_passed: true,
                quality_score: 95,
            },
        )
        .expect("iteration should run");

    assert!(!result.rolled_back);
    assert_eq!(
        fs::read_to_string(&target).expect("read patched file"),
        "alpha-new-omega"
    );
    let status = kernel.status();
    assert_eq!(status.iteration_total, 1);
    assert_eq!(status.iteration_promote_total, 1);
    assert_eq!(status.iteration_rollback_total, 0);
    assert_eq!(status.history_len, 1);
    assert_eq!(kernel.history().len(), 1);
}

#[test]
fn kernel_run_iteration_rolls_back_on_failed_verification() {
    let temp = TempDir::new().expect("tempdir");
    let target = temp.path().join("notes.txt");
    fs::write(&target, "alpha-old-omega").expect("seed patch target");
    let config = cordis_runtime::config::RuntimeConfig::default();
    let kernel = RuntimeKernel::new(temp.path(), &config);

    let result = kernel
        .run_iteration(
            AutoUpdatePlan {
                issue_id: "issue-rollback".to_string(),
                patch_id: "patch-rollback".to_string(),
                manual_approved: false,
                diff_lines: 1,
                patches: vec![FilePatch::text("notes.txt", "old", "new")],
            },
            VerificationInput {
                tests_passed: false,
                safety_checks_passed: true,
                quality_score: 10,
            },
        )
        .expect("iteration should still return a result");

    assert!(result.rolled_back);
    // Rolled-back edits must leave the original file untouched.
    assert_eq!(
        fs::read_to_string(&target).expect("read reverted file"),
        "alpha-old-omega"
    );
    let status = kernel.status();
    assert_eq!(status.iteration_total, 1);
    assert_eq!(status.iteration_promote_total, 0);
    assert_eq!(status.iteration_rollback_total, 1);
}

#[test]
fn kernel_observe_plugin_issue_dedups_and_counts() {
    let temp = TempDir::new().expect("tempdir");
    let config = cordis_runtime::config::RuntimeConfig::default();
    let kernel = RuntimeKernel::new(temp.path(), &config);

    let first = kernel.observe_plugin_issue(
        KernelPluginIssueSource::InvokeFailure,
        "expr",
        "invoke blew up",
    );
    assert_eq!(first.observe_count, 1);
    let second = kernel.observe_plugin_issue(
        KernelPluginIssueSource::InvokeFailure,
        "expr",
        "invoke blew up again",
    );
    // Same source + root path collapses onto the same issue id.
    assert_eq!(second.issue_id, first.issue_id);
    assert_eq!(second.observe_count, 2);
    assert_eq!(second.summary, "invoke blew up again");

    // A different source produces a distinct issue.
    kernel.observe_plugin_issue(KernelPluginIssueSource::LoadFailure, "shell", "load broke");
    let issues = kernel.plugin_issues();
    assert_eq!(issues.len(), 2);
    assert!(issues
        .iter()
        .any(|issue| issue.root_plugin_path == "expr" && issue.observe_count == 2));
    assert!(issues.iter().any(|issue| issue.root_plugin_path == "shell"
        && issue.source == KernelPluginIssueSource::LoadFailure));
    assert_eq!(kernel.status().plugin_issue_count, 2);
}

#[test]
fn kernel_can_auto_iterate_plugins_follows_api_key_presence() {
    let temp = TempDir::new().expect("tempdir");
    let mut config = cordis_runtime::config::RuntimeConfig::default();
    // Default config has no inline api_key and an env var that is unset in the
    // test process, so auto-iteration is disabled.
    config.llm_api.api_key = None;
    config.llm_api.api_key_env = "CORDIS_TEST_DEFINITELY_UNSET_KEY".to_string();
    let kernel = RuntimeKernel::new(temp.path(), &config);
    assert!(!kernel.can_auto_iterate_plugins());

    let mut config2 = cordis_runtime::config::RuntimeConfig::default();
    config2.llm_api.api_key = Some("sk-inline-test".to_string());
    let kernel2 = RuntimeKernel::new(temp.path(), &config2);
    assert!(kernel2.can_auto_iterate_plugins());
}

// ===========================================================================
// Group B — read-only RuntimeHost surface against the shared real fixtures.
// ===========================================================================

#[test]
#[serial]
fn host_boots_and_reports_status() {
    let host = shared_host();
    let status = host.status();
    assert!(status.plugin_count > 0);
    assert!(status.node_count > 0);
    assert!(!status.current_snapshot_id.is_empty());
    assert_eq!(
        status.current_snapshot_id,
        host.current_snapshot().snapshot_id()
    );

    let snapshot = host.current_snapshot();
    assert!(snapshot.plugin_registry().get("expr").is_some());
    assert!(snapshot.plugin_registry().get("time").is_some());
    assert!(snapshot.plugin_registry().get("filesystem").is_some());
}

#[test]
#[serial]
fn host_invoke_expr_returns_value() {
    let host = shared_host();
    let response = host
        .invoke(
            "expr",
            "expr_entry",
            json!({ "expression": "2 + 3 * 4" }).to_string(),
        )
        .expect("expr invoke should succeed");
    let value: Value = serde_json::from_str(&response.payload).expect("expr json");
    assert_eq!(value.get("value").and_then(|v| v.as_f64()), Some(14.0));
}

#[test]
#[serial]
fn host_invoke_time_now_returns_iso_timestamp() {
    let host = shared_host();
    let response = host
        .invoke(
            "time",
            "time_now",
            json!({ "node_id": "time_now" }).to_string(),
        )
        .expect("time invoke should succeed");
    let value: Value = serde_json::from_str(&response.payload).expect("time json");
    // The time plugin echoes a formatted timestamp; assert it produced a
    // non-empty string field rather than an error object.
    assert!(
        value.is_object(),
        "time response should be a JSON object: {value}"
    );
}

#[test]
#[serial]
fn host_invoke_unknown_plugin_errors_and_records_issue() {
    let host = shared_host();
    let before = host.kernel().plugin_issues().len();
    let err = host
        .invoke("no_such_plugin", "no_node", json!({}).to_string())
        .expect_err("unknown plugin invoke should fail");
    assert!(!err.to_string().is_empty());
    // The failure is observed as a kernel plugin issue.
    let after = host.kernel().plugin_issues();
    assert!(after.len() >= before);
    assert!(after
        .iter()
        .any(|issue| issue.root_plugin_path == "no_such_plugin"));
}

#[test]
#[serial]
fn host_execute_registered_target_traces_expr() {
    let host = shared_host();
    let result = host
        .execute("expr::expr_entry", json!({ "expression": "1 + 2 * 3" }))
        .expect("execute should succeed");
    assert_eq!(result.target_node_fqn, "expr::expr_entry");
    assert_eq!(
        result.output.outcomes.get("expr::expr_entry"),
        Some(&NodeOutcome::Success)
    );
    let trace = result
        .traces
        .get("expr::expr_entry")
        .expect("expr trace should exist");
    assert_eq!(
        trace
            .response_payload
            .as_ref()
            .and_then(|value| value.get("value"))
            .and_then(|value| value.as_f64()),
        Some(7.0)
    );
}

#[test]
#[serial]
fn host_execute_unknown_target_errors() {
    let host = shared_host();
    let err = host
        .execute("expr::does_not_exist", json!({ "expression": "1" }))
        .expect_err("unknown target should fail");
    assert!(err.to_string().contains("does_not_exist") || err.to_string().contains("not found"));
}

#[test]
#[serial]
fn host_execute_rejects_non_object_payload() {
    let host = shared_host();
    let err = host
        .execute("expr::expr_entry", json!("not-an-object"))
        .expect_err("non-object payload should fail");
    assert!(err.to_string().contains("must be a JSON object"));
}

#[test]
#[serial]
fn host_reload_root_noop_keeps_snapshot_stable() {
    let host = shared_host();
    let before = host.current_snapshot().snapshot_id().to_string();
    let report = host.reload("/").expect("noop reload should succeed");
    // Nothing on disk changed, so no plugins are added/removed/changed.
    assert!(report.added_plugins.is_empty());
    assert!(report.removed_plugins.is_empty());
    assert_eq!(report.from_snapshot_id, before);
    // Reload always advances to a fresh snapshot id even when content is equal.
    assert!(!report.to_snapshot_id.is_empty());
    assert_eq!(host.current_snapshot().snapshot_id(), report.to_snapshot_id);
    let attempt = host
        .last_reload_attempt()
        .expect("last reload attempt recorded");
    assert_eq!(attempt.status, ReloadAttemptStatus::Reloaded);
}

#[test]
#[serial]
fn host_candidate_stage_and_rollback_roundtrip() {
    let host = shared_host();
    let current = host.current_snapshot().snapshot_id().to_string();
    // A no-op candidate reload still stages a snapshot (control-plane path).
    let staged = host
        .reload_candidate()
        .expect("candidate reload should stage");
    assert_eq!(staged.from_snapshot_id, current);
    assert!(host.candidate_snapshot().is_some());
    assert_eq!(host.candidate_status(), Some(staged.clone()));
    assert_eq!(
        host.last_candidate_reload_attempt()
            .expect("candidate attempt recorded")
            .status,
        ReloadAttemptStatus::Staged
    );

    // Candidate snapshot serves invokes without touching current.
    let resp = host
        .invoke_candidate(
            "expr",
            "expr_entry",
            json!({ "expression": "6 / 2" }).to_string(),
        )
        .expect("candidate invoke should succeed");
    let value: Value = serde_json::from_str(&resp.payload).expect("candidate json");
    assert_eq!(value.get("value").and_then(|v| v.as_f64()), Some(3.0));

    let rolled = host
        .rollback_candidate()
        .expect("rollback should discard candidate");
    assert_eq!(rolled, staged);
    assert!(host.candidate_snapshot().is_none());
    let err = host
        .invoke_candidate(
            "expr",
            "expr_entry",
            json!({ "expression": "1" }).to_string(),
        )
        .expect_err("invoke after rollback should fail");
    assert!(err.to_string().contains("candidate"));
}

#[test]
#[serial]
fn host_candidate_promote_switches_current() {
    let host = shared_host();
    host.reload_candidate().expect("stage candidate");
    let report = host.promote_candidate().expect("promote should succeed");
    assert!(host.candidate_snapshot().is_none());
    assert_eq!(host.current_snapshot().snapshot_id(), report.to_snapshot_id);
    assert_eq!(
        host.last_reload_attempt()
            .expect("promote records reload attempt")
            .status,
        ReloadAttemptStatus::Reloaded
    );
    // Promoted snapshot still serves invokes.
    let resp = host
        .invoke(
            "expr",
            "expr_entry",
            json!({ "expression": "10 - 4" }).to_string(),
        )
        .expect("post-promote invoke should succeed");
    let value: Value = serde_json::from_str(&resp.payload).expect("json");
    assert_eq!(value.get("value").and_then(|v| v.as_f64()), Some(6.0));
}

#[test]
#[serial]
fn host_promote_without_candidate_errors() {
    let host = shared_host();
    assert!(host.candidate_snapshot().is_none());
    let err = host
        .promote_candidate()
        .expect_err("promote without candidate should fail");
    assert!(!err.to_string().is_empty());
}

#[test]
#[serial]
fn host_security_checks_flag_sensitive_paths_and_commands() {
    let host = shared_host();
    // Sensitive path keywords are rejected.
    for bad in [".ssh/id_rsa", "config/credentials", "/etc/passwd", "a.env"] {
        assert!(
            host.check_sensitive_path(bad).is_err(),
            "expected {bad} to be rejected"
        );
    }
    assert!(host.check_sensitive_path("plugins/expr/src/lib.rs").is_ok());

    for bad in ["ssh user@host", "cat /etc/shadow", "export SECRET=1"] {
        assert!(
            host.check_sensitive_command(bad).is_err(),
            "expected {bad} to be rejected"
        );
    }
    assert!(host.check_sensitive_command("cargo build").is_ok());
}

#[test]
#[serial]
fn host_resolve_sandboxed_path_enforces_containment() {
    let host = shared_host();
    // Absolute and parent-traversal are rejected.
    assert!(host.resolve_sandboxed_path("/etc/hosts").is_err());
    assert!(host.resolve_sandboxed_path("../escape").is_err());
    // A plain relative path resolves under the fixtures root.
    let resolved = host
        .resolve_sandboxed_path("artifacts/index.json")
        .expect("relative path should resolve");
    assert!(resolved.ends_with("artifacts/index.json"));
    // data/ paths resolve against the workspace root (parent of fixtures).
    let data = host
        .resolve_sandboxed_path("data/scratch.txt")
        .expect("data path should resolve");
    assert!(data.to_string_lossy().contains("data/scratch.txt"));
}

#[test]
#[serial]
fn host_check_agent_accessible_gates_on_docs_flag() {
    let host = shared_host();
    // filesystem nodes are agent-accessible in the fixture docs.
    assert!(host.check_agent_accessible("filesystem", "fs_read").is_ok());
    // An unknown plugin is not registered.
    assert!(matches!(
        host.check_agent_accessible("ghost_plugin", "node"),
        Err(RuntimeError::PluginNotRegistered { .. })
    ));
    // A real plugin with an unknown node id is rejected as not-allowed.
    assert!(host
        .check_agent_accessible("filesystem", "no_such_node")
        .is_err());
}

#[test]
#[serial]
fn host_walk_code_files_finds_sources_and_honors_stop() {
    let host = shared_host();
    let root = fixtures_root().join("artifacts");
    let mut seen = 0usize;
    host.walk_code_files(&root, &mut |rel, abs| {
        assert!(abs.is_file());
        assert!(!rel.is_empty());
        seen += 1;
    })
    .expect("walk should succeed");
    assert!(seen > 0, "artifacts dir should contain source-like files");

    // The _ctl variant aborts immediately when the visitor returns Stop.
    let mut count = 0usize;
    host.walk_code_files_ctl(&root, &mut |_, _| {
        count += 1;
        WalkControl::Stop
    })
    .expect("walk_ctl should succeed");
    assert_eq!(count, 1, "Stop after the first hit should end the walk");

    // Non-directory root is a no-op.
    let mut never = false;
    host.walk_code_files(&root.join("index.json"), &mut |_, _| never = true)
        .expect("walk on file should be a no-op");
    assert!(!never);
}

#[test]
#[serial]
fn host_soul_roundtrip_via_file_provider() {
    let host = shared_host();
    // Empty scope key is rejected on write and yields None on read.
    assert!(host.set_soul("", &Soul::default()).is_err());
    assert!(host.get_soul("").expect("empty read ok").is_none());

    let soul_key = "host-coverage:soul-scope#private";
    let soul = Soul {
        persona: "coverage persona".to_string(),
        ..Default::default()
    };
    host.set_soul(soul_key, &soul).expect("set soul");
    let fetched = host.get_soul(soul_key).expect("get soul").expect("exists");
    assert_eq!(fetched.persona, "coverage persona");

    // AgentToolHost overlay surface reads the same persona back.
    assert_eq!(
        host.agent_soul_overlay(soul_key).as_deref(),
        Some("coverage persona")
    );
    assert!(host.agent_soul_overlay("nobody#private").is_none());
}

#[test]
#[serial]
fn host_agent_session_lifecycle_inject_status_transcript_drop() {
    let host = shared_host();
    let handle = host
        .agent_start_with(AgentSessionKind::RuntimeShell, AgentStartOptions::default())
        .expect("agent should start");
    let sid = handle.session_id.clone();
    assert_eq!(handle.kind, AgentSessionKind::RuntimeShell);

    // Inject a synthetic exchange (no LLM round-trip needed). This pushes
    // user+assistant transcript entries without advancing completed_turns
    // (which only a real respond() cycle increments).
    host.agent_inject(&sid, "hello", "hi there")
        .expect("inject should succeed");
    host.agent_status(&sid).expect("status should be queryable");
    let transcript = host.agent_transcript(&sid).expect("transcript");
    assert!(
        transcript
            .iter()
            .any(|entry| format!("{entry:?}").contains("hi there")),
        "assistant reply should be in the transcript"
    );
    assert!(
        transcript
            .iter()
            .any(|entry| format!("{entry:?}").contains("hello")),
        "user input should be in the transcript"
    );

    // refresh_session_soul re-scopes the persona overlay.
    host.refresh_session_soul(&sid, "host-coverage:refreshed#private")
        .expect("refresh soul on live session");

    // Unknown session ids error uniformly.
    assert!(matches!(
        host.agent_status("no-such"),
        Err(RuntimeError::AgentSessionNotFound { .. })
    ));
    assert!(matches!(
        host.agent_inject("no-such", "a", "b"),
        Err(RuntimeError::AgentSessionNotFound { .. })
    ));
    assert!(matches!(
        host.refresh_session_soul("no-such", "x"),
        Err(RuntimeError::AgentSessionNotFound { .. })
    ));

    host.drop_session(&sid);
    assert!(matches!(
        host.agent_status(&sid),
        Err(RuntimeError::AgentSessionNotFound { .. })
    ));
    // Dropping again is idempotent.
    host.drop_session(&sid);
}

#[test]
#[serial]
fn host_agent_start_rejects_plugin_iteration_kind() {
    let host = shared_host();
    let err = host
        .agent_start(AgentSessionKind::PluginIteration)
        .expect_err("plugin iteration sessions cannot be started directly");
    assert!(err.to_string().contains("iterate_plugins"));
}

// ===========================================================================
// Group C — fixture-mutating paths (throwaway copies; safe on network mount).
// ===========================================================================

#[test]
#[serial]
fn host_create_plugin_validates_name_and_writes_skeleton() {
    let temp = setup_artifacts_only_copy();
    let host = RuntimeHost::boot(temp.path()).expect("host should boot from artifacts copy");

    // Empty and invalid names are rejected before any filesystem work.
    assert!(host.create_plugin("", None).is_err());
    assert!(host.create_plugin("bad name!", None).is_err());
}

#[test]
#[serial]
fn host_iterate_plugins_policy_blocks_runtime_paths() {
    // A copy that lacks the plugins/ source tree still boots (loader reads
    // artifacts/), and the policy gate rejects edits outside the plugin
    // iteration surface before any source access — so no plugins/ tree needed.
    let temp = setup_artifacts_only_copy();
    let host = RuntimeHost::boot(temp.path()).expect("host should boot");

    let result = host
        .iterate_plugins(KernelPluginIterationRequest {
            issue_id: None,
            target_plugin_paths: vec!["expr".to_string()],
            instruction: Some("try to modify runtime crate".to_string()),
            edit_plan: Some(PluginEditPlan {
                issue_id: "issue-policy-blocked".to_string(),
                patch_id: "patch-policy-blocked".to_string(),
                summary: "try to modify runtime crate".to_string(),
                operations: vec![PluginEditOperation {
                    path: "crates/cordis-runtime/src/lib.rs".to_string(),
                    kind: PluginEditOpKind::ReplaceExact,
                    expected_old_string: Some("pub mod config;".to_string()),
                    expected_sha256: None,
                    new_content: Some("pub mod config;".to_string()),
                    pointer: None,
                    dotted_key: None,
                    value: None,
                }],
            }),
            manual_approved: false,
            tests_command: None,
            safety_command: None,
            verify_profile: Some(VerificationProfile::RustWorkspace),
            quality_score: Some(95),
        })
        .expect("policy-blocked iteration still returns a result");

    assert_eq!(
        result.final_verdict,
        PluginIterationFinalVerdict::RolledBack
    );
    assert!(result.changed_paths.is_empty());
    assert!(host.candidate_snapshot().is_none());
    assert!(result
        .blocked_reason
        .as_deref()
        .unwrap_or_default()
        .contains("outside the plugin iteration surface"));
    assert!(host.kernel().plugin_issues().iter().any(|issue| {
        issue.root_plugin_path == "expr" && issue.source == KernelPluginIssueSource::PolicyBlocked
    }));
}
