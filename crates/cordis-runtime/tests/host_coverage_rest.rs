//! Coverage-focused integration tests for `RuntimeHost` / `RuntimeKernel`
//! segments NOT owned by the `execute_tool` / agent-driven plugin-iteration
//! batch (w3-host-a). This file is the "rest" companion to
//! `host_coverage.rs`: same shared read-only fixtures boot plus hermetic
//! temp-dir kernels, targeting the read-only accessor surface, the kernel
//! plugin-iteration bookkeeping (`record_plugin_iteration_outcome`,
//! `plugin_iteration_status`, `take_blocked_iteration`,
//! `select_issue_for_request` via `begin`), the plugin-backed soul provider
//! (soul_store is present in the shared fixtures), reload-subtree diagnostics
//! and error arms, and `agent_send_with_fallback`.
//!
//! Like `host_coverage.rs` these do not gate on
//! `linux_dylib_artifacts_available()`; `ensure_fixture_artifacts` rebuilds
//! the fixture dylibs for the current host so a real boot succeeds on macOS.

use cordis_runtime::core::error::RuntimeError;
use cordis_runtime::core::models::NodeOutcome;
use cordis_runtime::execution::engine::{ExecutionMetrics, ExecutionOutput};
use cordis_runtime::host::{
    AgentSessionKind, AgentStartOptions, KernelPluginIterationResult, ReloadAttemptStatus,
    RuntimeHost, RuntimeKernel,
};
use cordis_runtime::kernel::plugin_iteration::{
    CanaryReport, CanaryVerdict, KernelPluginIssueSource, KernelPluginIterationRequest,
    PluginEditPlan, PluginIterationFinalVerdict, VerifierVerdict,
};
use cordis_runtime::kernel::verifier::VerificationProfile;
use cordis_runtime::soul::Soul;
use serde_json::json;
use serial_test::serial;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

mod support;
use support::fixtures_root;

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

/// Copy just the `artifacts/` tree + top-level config files into a throwaway
/// dir. Enough for a read-only boot (the loader consumes `artifacts/`).
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

// ---------------------------------------------------------------------------
// Shared read-only host against the real fixtures tree. Mirrors the helper in
// host_coverage.rs but kept local so the two files never contend for one
// OnceLock (each test binary is its own process).
// ---------------------------------------------------------------------------

use std::sync::OnceLock;
static SHARED_HOST: OnceLock<RuntimeHost> = OnceLock::new();

fn shared_host() -> &'static RuntimeHost {
    SHARED_HOST.get_or_init(|| {
        let root = fixtures_root();
        cordis_runtime::plugin::tooling::refresh_artifact_index(&root)
            .expect("refresh artifact index before shared boot");
        RuntimeHost::boot(root).expect("host should boot")
    })
}

// ===========================================================================
// Group A — RuntimeSnapshot accessor surface (read-only; skipped on mac by
// runtime_host.rs because it gates on the linux dylibs).
// ===========================================================================

#[test]
#[serial]
fn snapshot_accessors_expose_all_registries() {
    let host = shared_host();
    let snapshot = host.current_snapshot();

    // doc_registry / graph_registry / context_baseline / staged_artifact_root
    // are the four accessors the reference tests never touch on macOS.
    let docs = snapshot.doc_registry();
    // Every loaded plugin with docs contributes at least its own agent docs.
    assert!(
        snapshot.plugin_registry().get("expr").is_some(),
        "expr must be loaded for the doc registry assertion"
    );
    // doc_registry is a real registry object; formatting it must not panic.
    let _ = format!("{docs:?}");

    let graph = snapshot.graph_registry();
    // The registered net is the source the execute path walks.
    let net = graph.net();
    assert!(
        !net.nodes.is_empty(),
        "graph registry should expose registered nodes"
    );

    // context_baseline is the cloneable seed each execute run forks.
    let baseline = snapshot.context_baseline();
    let _ = format!("{baseline:?}");

    // staged_artifact_root lives under the snapshot root in the OS temp dir.
    let staged = snapshot.staged_artifact_root();
    assert!(
        !staged.as_os_str().is_empty(),
        "staged artifact root must be a real path"
    );
    assert!(
        staged.starts_with(&host.status().snapshot_root),
        "staged artifact root must live under the snapshot root"
    );
}

// ===========================================================================
// Group B — RuntimeKernel plugin-iteration bookkeeping (fully hermetic).
//
// `record_plugin_iteration_outcome` is a public entry point that feeds the
// metrics, history, blocked-map, last-iteration slot and issue-status update
// paths. Driving it directly with hand-built results exercises every match
// arm without needing a compiled plugin.
// ===========================================================================

fn make_result(
    iteration_id: &str,
    issue_id: &str,
    verdict: PluginIterationFinalVerdict,
) -> KernelPluginIterationResult {
    KernelPluginIterationResult {
        iteration_id: iteration_id.to_string(),
        issue_id: issue_id.to_string(),
        root_plugin_path: "expr".to_string(),
        target_plugin_paths: vec!["expr".to_string()],
        source: Some(KernelPluginIssueSource::InvokeFailure),
        summary: "hand-built iteration result".to_string(),
        agent_session_id: None,
        tool_execution_summary: None,
        derived_edit_plan: PluginEditPlan {
            issue_id: issue_id.to_string(),
            patch_id: format!("{iteration_id}-patch"),
            summary: "edit".to_string(),
            operations: Vec::new(),
        },
        transcript_excerpt: Vec::new(),
        changed_paths: vec!["plugins/expr/src/lib.rs".to_string()],
        rebuilt_artifacts: Vec::new(),
        candidate: None,
        verification: None,
        verifier_verdict: Some(VerifierVerdict::Pass),
        canary: Some(CanaryReport {
            verdict: CanaryVerdict::Pass,
            mode: "test".to_string(),
            plugin_path: Some("expr".to_string()),
            node_id: None,
            payload: None,
            expected_response: None,
            actual_response: None,
            message: "ok".to_string(),
        }),
        final_verdict: verdict,
        blocked_reason: match verdict {
            PluginIterationFinalVerdict::Blocked => Some("blocked for coverage".to_string()),
            _ => None,
        },
        net_output: ExecutionOutput {
            execution_id: "exec".to_string(),
            order: Vec::new(),
            outcomes: BTreeMap::new(),
            keyed_outcomes: BTreeMap::new(),
            metrics: ExecutionMetrics::default(),
        },
    }
}

#[test]
fn kernel_record_outcome_promoted_updates_metrics_history_and_status() {
    let temp = TempDir::new().expect("tempdir");
    let config = cordis_runtime::config::RuntimeConfig::default();
    let kernel = RuntimeKernel::new(temp.path(), &config);

    // Seed a matching issue so the Resolved status update has a target.
    let issue = kernel.observe_plugin_issue(
        KernelPluginIssueSource::InvokeFailure,
        "expr",
        "expr blew up",
    );

    let result = make_result(
        "iter-promoted",
        &issue.issue_id,
        PluginIterationFinalVerdict::Promoted,
    );
    kernel.record_plugin_iteration_outcome(&result);

    let status = kernel.status();
    assert_eq!(status.plugin_iteration_total, 1);
    assert_eq!(status.iteration_total, 1);
    assert_eq!(status.iteration_promote_total, 1);
    assert_eq!(status.iteration_rollback_total, 0);
    assert!(status.last_plugin_iteration.is_some());

    // History carries the promoted entry.
    let history = kernel.plugin_history();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].iteration_id, "iter-promoted");
    assert_eq!(
        history[0].final_verdict,
        PluginIterationFinalVerdict::Promoted
    );

    // Promotion resolves the issue (no longer Open) so it drops out of the
    // candidate list select_issue_for_request would return.
    assert!(kernel.blocked_iterations().is_empty());
    // last_plugin_iteration slot answers plugin_iteration_status directly.
    let queried = kernel
        .plugin_iteration_status("iter-promoted")
        .expect("promoted iteration status queryable");
    assert_eq!(queried.final_verdict, PluginIterationFinalVerdict::Promoted);
}

#[test]
fn kernel_record_outcome_blocked_then_taken_and_rolledback_metrics() {
    let temp = TempDir::new().expect("tempdir");
    let config = cordis_runtime::config::RuntimeConfig::default();
    let kernel = RuntimeKernel::new(temp.path(), &config);

    let blocked = make_result(
        "iter-blocked",
        "issue-b",
        PluginIterationFinalVerdict::Blocked,
    );
    kernel.record_plugin_iteration_outcome(&blocked);

    // Blocked verdict does not bump promote/rollback but is retained in the
    // blocked map and answerable via status.
    let status = kernel.status();
    assert_eq!(status.iteration_promote_total, 0);
    assert_eq!(status.iteration_rollback_total, 0);
    assert_eq!(status.blocked_iteration_count, 1);
    assert_eq!(kernel.blocked_iterations().len(), 1);

    let via_blocked = kernel
        .plugin_iteration_status("iter-blocked")
        .expect("blocked status queryable");
    assert_eq!(
        via_blocked.final_verdict,
        PluginIterationFinalVerdict::Blocked
    );
    assert_eq!(
        via_blocked.blocked_reason.as_deref(),
        Some("blocked for coverage")
    );

    // take_blocked_iteration removes and returns the blocked result.
    let taken = kernel
        .take_blocked_iteration("iter-blocked")
        .expect("blocked iteration takeable");
    assert_eq!(taken.iteration_id, "iter-blocked");
    assert!(kernel.blocked_iterations().is_empty());
    // Taking again errors (already removed).
    assert!(matches!(
        kernel.take_blocked_iteration("iter-blocked"),
        Err(RuntimeError::PluginIterationStatusNotFound { .. })
    ));

    // Now record a rollback for a different iteration to hit that metric arm
    // and the blocked-map removal branch.
    let rolled = make_result(
        "iter-rb",
        "issue-rb",
        PluginIterationFinalVerdict::RolledBack,
    );
    kernel.record_plugin_iteration_outcome(&rolled);
    assert_eq!(kernel.status().iteration_rollback_total, 1);

    // History now retains both blocked and rolled-back entries; querying a
    // history-only id (not last, not blocked) hits the history branch.
    let from_history = kernel
        .plugin_iteration_status("iter-blocked")
        .expect("blocked iteration still in history after take");
    assert_eq!(
        from_history.final_verdict,
        PluginIterationFinalVerdict::Blocked
    );
}

#[test]
fn kernel_take_blocked_rejects_non_blocked_and_unknown() {
    let temp = TempDir::new().expect("tempdir");
    let config = cordis_runtime::config::RuntimeConfig::default();
    let kernel = RuntimeKernel::new(temp.path(), &config);

    // Unknown id → not found.
    assert!(matches!(
        kernel.take_blocked_iteration("nope"),
        Err(RuntimeError::PluginIterationStatusNotFound { .. })
    ));

    // plugin_iteration_status for a totally unknown id also errors.
    assert!(matches!(
        kernel.plugin_iteration_status("ghost"),
        Err(RuntimeError::PluginIterationStatusNotFound { .. })
    ));
}

#[test]
fn kernel_record_outcome_updates_existing_history_entry_in_place() {
    let temp = TempDir::new().expect("tempdir");
    let config = cordis_runtime::config::RuntimeConfig::default();
    let kernel = RuntimeKernel::new(temp.path(), &config);

    // First record as Blocked, then re-record the SAME iteration id as
    // Promoted: the history entry must be updated in place (not duplicated)
    // and the blocked map entry cleared.
    let blocked = make_result("iter-x", "issue-x", PluginIterationFinalVerdict::Blocked);
    kernel.record_plugin_iteration_outcome(&blocked);
    assert_eq!(kernel.plugin_history().len(), 1);
    assert_eq!(kernel.blocked_iterations().len(), 1);

    let promoted = make_result("iter-x", "issue-x", PluginIterationFinalVerdict::Promoted);
    kernel.record_plugin_iteration_outcome(&promoted);
    let history = kernel.plugin_history();
    assert_eq!(history.len(), 1, "same iteration id must update in place");
    assert_eq!(
        history[0].final_verdict,
        PluginIterationFinalVerdict::Promoted
    );
    assert!(kernel.blocked_iterations().is_empty());
}

// ===========================================================================
// Group C — begin_plugin_iteration / select_issue_for_request via the public
// iterate_plugins entry. A policy-blocked edit plan targeting an unknown issue
// id must surface PluginIterationIssueNotFound before any filesystem work.
// ===========================================================================

#[test]
#[serial]
fn iterate_plugins_unknown_issue_id_errors() {
    let host = shared_host();
    let err = host
        .iterate_plugins(KernelPluginIterationRequest {
            issue_id: Some("no-such-issue".to_string()),
            target_plugin_paths: vec!["expr".to_string()],
            instruction: Some("x".to_string()),
            edit_plan: None,
            manual_approved: false,
            tests_command: None,
            safety_command: None,
            verify_profile: Some(VerificationProfile::RustWorkspace),
            quality_score: Some(95),
        })
        .expect_err("unknown issue id must error");
    assert!(matches!(
        err,
        RuntimeError::PluginIterationIssueNotFound { .. }
    ));
}

#[test]
#[serial]
fn iterate_plugins_unknown_target_subtree_errors() {
    let host = shared_host();
    let err = host
        .iterate_plugins(KernelPluginIterationRequest {
            issue_id: None,
            target_plugin_paths: vec!["definitely_not_a_plugin".to_string()],
            instruction: Some("x".to_string()),
            edit_plan: None,
            manual_approved: false,
            tests_command: None,
            safety_command: None,
            verify_profile: Some(VerificationProfile::RustWorkspace),
            quality_score: Some(95),
        })
        .expect_err("target that shares no loaded subtree root must error");
    assert!(matches!(err, RuntimeError::InvalidArgument { .. }));
}

// ===========================================================================
// Group D — plugin-backed soul provider. The shared fixtures include
// `soul_store`, whose `soul_get`/`soul_set` capability nodes override the
// kernel file provider. Exercising get/set through the host drives the
// PluginSoulProvider get/set arms (JSON round-trip via invoke).
// ===========================================================================

#[test]
#[serial]
fn soul_roundtrip_prefers_plugin_backed_provider() {
    let host = shared_host();
    assert!(
        host.current_snapshot()
            .plugin_registry()
            .get("soul_store")
            .is_some(),
        "soul_store must be loaded for the plugin provider path"
    );

    let soul_key = "host-rest:plugin-soul#private";
    let soul = Soul {
        persona: "plugin-backed persona".to_string(),
        ..Default::default()
    };
    // set → get through the plugin nodes (PluginSoulProvider::set / ::get).
    host.set_soul(soul_key, &soul).expect("plugin set_soul");
    let fetched = host
        .get_soul(soul_key)
        .expect("plugin get_soul")
        .expect("soul exists");
    assert_eq!(fetched.persona, "plugin-backed persona");

    // A key that was never written returns None (soul == null arm).
    assert!(host
        .get_soul("host-rest:never-written#private")
        .expect("get missing soul")
        .is_none());
}

// ===========================================================================
// Group E — reload diagnostics + subtree error arms.
// ===========================================================================

#[test]
#[serial]
fn reload_with_diagnostics_records_status_accessors() {
    let host = shared_host();
    let attempt = host.reload_with_diagnostics("/");
    assert_eq!(attempt.status, ReloadAttemptStatus::Reloaded);

    // status() folds last_reload / last_candidate_reload accessors, which are
    // uncovered by the linux-gated reference tests.
    let status = host.status();
    assert!(status.plugin_count > 0);
    assert!(status.node_count > 0);
    assert!(status.last_reload.is_some());
    // last_reload_attempt() mirrors the status field.
    let last = host.last_reload_attempt().expect("last reload recorded");
    assert_eq!(last.status, ReloadAttemptStatus::Reloaded);
}

// ===========================================================================
// Group F — execute_registered_target multi-node + error surface.
// ===========================================================================

#[test]
#[serial]
fn execute_registered_target_populates_traces_for_expr() {
    let host = shared_host();
    let snapshot = host.current_snapshot();
    // Drive execute_registered_target directly on the snapshot (the accessor
    // path RuntimeSnapshot exposes), asserting the trace + outcome plumbing.
    let result = snapshot
        .execute_registered_target("expr::expr_entry", json!({ "expression": "2 + 2" }))
        .expect("execute should succeed");
    assert_eq!(result.target_node_fqn, "expr::expr_entry");
    assert_eq!(
        result.output.outcomes.get("expr::expr_entry"),
        Some(&NodeOutcome::Success)
    );
    let trace = result
        .traces
        .get("expr::expr_entry")
        .expect("expr trace present");
    assert_eq!(trace.plugin_path, "expr");
    assert_eq!(trace.node_id, "expr_entry");
    assert!(trace.response_payload.is_some());
}

#[test]
#[serial]
fn execute_registered_target_rejects_non_object_and_unknown_node() {
    let host = shared_host();
    let snapshot = host.current_snapshot();

    // Non-object payload → InvalidArgument ("must be a JSON object").
    let err = snapshot
        .execute_registered_target("expr::expr_entry", json!("scalar"))
        .expect_err("scalar payload rejected");
    assert!(matches!(err, RuntimeError::InvalidArgument { .. }));

    // Unknown node fqn → InvalidArgument ("registered node not found").
    let err = snapshot
        .execute_registered_target("expr::missing_node", json!({}))
        .expect_err("unknown node rejected");
    let msg = err.to_string();
    assert!(msg.contains("registered node not found") || msg.contains("missing_node"));
}

// ===========================================================================
// Group G — agent_send_with_fallback: session with no fallback entry falls
// through to a plain send; unknown session id surfaces AgentSessionNotFound.
// (No LLM round-trip: we only reach the pre-send lookup arms.)
// ===========================================================================

#[test]
#[serial]
fn agent_send_with_fallback_unknown_session_errors() {
    let host = shared_host();
    // No profile_fallback entry AND no session → agent_send path returns
    // AgentSessionNotFound (the `entry is None → plain agent_send` branch).
    let err = host
        .agent_send_with_fallback("no-such-session", "hello")
        .expect_err("unknown session must error");
    assert!(matches!(err, RuntimeError::AgentSessionNotFound { .. }));
}

#[test]
#[serial]
fn agent_start_populates_profile_fallback_entry() {
    let host = shared_host();
    // Starting a session inserts a ProfileFallbackEntry; dropping removes it.
    let before = host.debug_session_map_sizes();
    let handle = host
        .agent_start_with(AgentSessionKind::RuntimeShell, AgentStartOptions::default())
        .expect("agent should start");
    let after = host.debug_session_map_sizes();
    // agent_sessions and profile_fallback both grew by one.
    assert_eq!(after.0, before.0 + 1);
    assert_eq!(after.2, before.2 + 1);
    host.drop_session(&handle.session_id);
    let cleaned = host.debug_session_map_sizes();
    assert_eq!(cleaned.0, before.0);
    assert_eq!(cleaned.2, before.2);
}

// ===========================================================================
// Group H — kernel status maps last_plugin_iteration through
// plugin_iteration_status_from_result (the map arm in status()).
// ===========================================================================

#[test]
fn kernel_status_reflects_last_plugin_iteration_summary() {
    let temp = TempDir::new().expect("tempdir");
    let config = cordis_runtime::config::RuntimeConfig::default();
    let kernel = RuntimeKernel::new(temp.path(), &config);
    let result = make_result(
        "iter-last",
        "issue-last",
        PluginIterationFinalVerdict::Promoted,
    );
    kernel.record_plugin_iteration_outcome(&result);
    let status = kernel.status();
    let last = status
        .last_plugin_iteration
        .expect("last plugin iteration present");
    assert_eq!(last.iteration_id, "iter-last");
    assert_eq!(last.root_plugin_path, "expr");
    assert_eq!(last.verifier_verdict, Some(VerifierVerdict::Pass));
    assert_eq!(last.canary_verdict, Some(CanaryVerdict::Pass));
}

// ===========================================================================
// Group I — reload_subtree branches on an isolated artifacts-only copy.
// Uses its own booted host (not the shared one) so mutating artifacts/ is
// safe. These exercise the subtree happy path (changed_plugins non-empty →
// notify + reason map), the empty-target no-op, and the missing-artifact
// error arm.
// ===========================================================================

#[test]
#[serial]
fn reload_subtree_leaf_reports_changed_plugin() {
    let temp = setup_artifacts_only_copy();
    let host = RuntimeHost::boot(temp.path()).expect("host should boot from artifacts copy");

    let before = host.current_snapshot().snapshot_id().to_string();
    let report = host
        .reload("expr")
        .expect("subtree reload of a real leaf should succeed");
    // A subtree reload marks the target as changed and advances the snapshot
    // id (distinct from/ to per P1-21).
    assert!(report.changed_plugins.iter().any(|p| p == "expr"));
    assert_eq!(report.from_snapshot_id, before);
    assert_ne!(report.to_snapshot_id, before);
    // The reload attempt is recorded as Reloaded with plugin_count set.
    let attempt = host.last_reload_attempt().expect("attempt recorded");
    assert_eq!(attempt.status, ReloadAttemptStatus::Reloaded);
    assert!(attempt.plugin_count.is_some());
}

#[test]
#[serial]
fn reload_subtree_unknown_prefix_is_noop_reloaded() {
    let temp = setup_artifacts_only_copy();
    let host = RuntimeHost::boot(temp.path()).expect("host should boot");

    let before = host.current_snapshot().snapshot_id().to_string();
    // A prefix that matches no plugin → empty targets → no-op reload that
    // keeps the same snapshot id and reports Reloaded.
    let report = host
        .reload("no_such_prefix")
        .expect("empty-target subtree reload is a no-op success");
    assert!(report.changed_plugins.is_empty());
    assert_eq!(report.from_snapshot_id, before);
    assert_eq!(report.to_snapshot_id, before);
    let attempt = host.reload_with_diagnostics("no_such_prefix");
    assert_eq!(attempt.status, ReloadAttemptStatus::Reloaded);
}

#[test]
#[serial]
fn reload_subtree_missing_artifact_errors() {
    let temp = setup_artifacts_only_copy();
    let host = RuntimeHost::boot(temp.path()).expect("host should boot");

    // Delete the whole artifact index so the subtree reload's index load /
    // per-plugin lookup fails, driving the error arm (make_failed_attempt).
    let index = temp.path().join("artifacts/index.json");
    fs::remove_file(&index).expect("remove artifact index");

    let err = host
        .reload("expr")
        .expect_err("subtree reload without an index must fail");
    assert!(!err.to_string().is_empty());
    // The failed attempt is recorded with a failure summary and Failed status.
    let attempt = host.last_reload_attempt().expect("failed attempt recorded");
    assert_eq!(attempt.status, ReloadAttemptStatus::Failed);
    assert!(attempt.failure_summary.is_some());
}

#[test]
#[serial]
fn execute_unknown_target_records_kernel_issue() {
    let temp = setup_artifacts_only_copy();
    let host = RuntimeHost::boot(temp.path()).expect("host should boot");

    let before = host.kernel().plugin_issues().len();
    let err = host
        .execute("expr::does_not_exist", json!({ "expression": "1" }))
        .expect_err("unknown target should fail");
    assert!(!err.to_string().is_empty());
    // execute()'s Err arm observes an InvokeFailure kernel issue for the
    // plugin segment of the fqn.
    let after = host.kernel().plugin_issues();
    assert!(after.len() >= before);
    assert!(after.iter().any(|issue| issue.root_plugin_path == "expr"));
}

// ===========================================================================
// Group J — kernel run_iteration records into ChangeMemory (history()).
// The reference kernel tests in host_coverage.rs assert metrics; here we also
// assert the ChangeRecord bookkeeping surfaced through history() + status
// last_change, covering the memory-record arm.
// ===========================================================================

#[test]
fn kernel_run_iteration_records_change_memory_entry() {
    use cordis_runtime::kernel::auto_update::{AutoUpdatePlan, FilePatch};
    use cordis_runtime::kernel::evaluator::VerificationInput;

    let temp = TempDir::new().expect("tempdir");
    let target = temp.path().join("notes.txt");
    fs::write(&target, "alpha-old-omega").expect("seed target");
    let config = cordis_runtime::config::RuntimeConfig::default();
    let kernel = RuntimeKernel::new(temp.path(), &config);

    kernel
        .run_iteration(
            AutoUpdatePlan {
                issue_id: "issue-mem".to_string(),
                patch_id: "patch-mem".to_string(),
                manual_approved: false,
                diff_lines: 1,
                patches: vec![FilePatch::text("notes.txt", "old", "new")],
            },
            VerificationInput {
                tests_passed: true,
                safety_checks_passed: true,
                quality_score: 90,
            },
        )
        .expect("iteration should run");

    // history() returns the recorded ChangeRecord; status.last_change mirrors
    // the most recent one.
    let history = kernel.history();
    assert_eq!(history.len(), 1);
    let status = kernel.status();
    assert_eq!(status.history_len, 1);
    assert!(status.last_change.is_some());
}
