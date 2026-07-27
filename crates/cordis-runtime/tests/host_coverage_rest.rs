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
    RuntimeHost, RuntimeKernel, WalkControl,
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
use std::path::{Path, PathBuf};
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
fn reload_whole_tree_missing_artifact_index_records_failed_attempt() {
    let temp = setup_artifacts_only_copy();
    let host = RuntimeHost::boot(temp.path()).expect("host should boot");

    // Delete the artifact index so the whole-tree snapshot rebuild inside
    // `reload_internal` fails at `build_snapshot_with_staged_root`, driving
    // the Failed-attempt construction arm (the reload_internal build-error
    // branch that returns Err((err, attempt))).
    let index = temp.path().join("artifacts/index.json");
    fs::remove_file(&index).expect("remove artifact index");

    let err = host
        .reload("/")
        .expect_err("whole-tree reload without an index must fail");
    assert!(!err.to_string().is_empty());
    let attempt = host.last_reload_attempt().expect("failed attempt recorded");
    assert_eq!(attempt.status, ReloadAttemptStatus::Failed);
    assert!(attempt.failure_summary.is_some());
    assert!(attempt.to_snapshot_id.is_none());
}

#[test]
#[serial]
fn reload_candidate_missing_artifact_index_records_failed_attempt() {
    let temp = setup_artifacts_only_copy();
    let host = RuntimeHost::boot(temp.path()).expect("host should boot");

    // Same failure injection, but through the candidate-staging path so the
    // build-error arm of `reload_candidate_internal` is exercised.
    let index = temp.path().join("artifacts/index.json");
    fs::remove_file(&index).expect("remove artifact index");

    let attempt = host.reload_candidate_with_diagnostics();
    assert_eq!(attempt.status, ReloadAttemptStatus::Failed);
    assert!(attempt.failure_summary.is_some());
    assert!(attempt.to_snapshot_id.is_none());
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
/// Copy only `artifacts/` (+ small top-level config files) into a fresh temp
/// dir and boot a host on it. The returned `TempDir` owns the tree; the
/// `PathBuf` is the fixtures root passed to `boot`.
fn boot_on_artifacts_copy() -> (TempDir, PathBuf, RuntimeHost) {
    let temp = TempDir::new().expect("tempdir");
    let root = fixtures_root();
    copy_dir_all(&root.join("artifacts"), &temp.path().join("artifacts"));
    for name in ["notify_handlers.json", "startup_invoke.json"] {
        let src = root.join(name);
        if src.exists() {
            fs::copy(&src, temp.path().join(name)).expect("copy top-level fixture file");
        }
    }
    let fixtures = temp.path().to_path_buf();
    // Flaky-guard (mirrors `host_coverage::shared_host`): concurrent suites
    // rebuild the fixture dylibs mid-run, so the copied `index.json`'s recorded
    // sha256 can be stale relative to the copied `.so`. A boot would then fail
    // with `PluginUnavailable { HashMismatch }`. Re-hash the copied artifacts
    // against the copied index before booting.
    cordis_runtime::plugin::tooling::refresh_artifact_index(&fixtures)
        .expect("refresh copied artifact index before boot");
    let host = RuntimeHost::boot(&fixtures).expect("host should boot on artifacts-only copy");
    (temp, fixtures, host)
}

// create_plugin: workspace manifest is present but not valid TOML. The skeleton
// (src dir + Cargo.toml + lib.rs) is written first, then the read succeeds but
// `toml::from_str` fails → InvalidArgument "failed to parse workspace manifest".
#[test]
#[serial]
fn create_plugin_rejects_malformed_workspace_manifest() {
    let (_temp, fixtures, host) = boot_on_artifacts_copy();
    let plugins = fixtures.join("plugins");
    fs::create_dir_all(&plugins).expect("mkdir plugins");
    fs::write(plugins.join("Cargo.toml"), "this = = not valid toml").expect("write bad manifest");

    let err = host
        .create_plugin("widget", Some("a widget"))
        .expect_err("malformed workspace manifest must abort create_plugin");
    assert!(
        matches!(&err, RuntimeError::InvalidArgument { message } if message.starts_with("failed to parse workspace manifest:")),
        "expected InvalidArgument, got {err:?}"
    );
    // The skeleton files were written before the manifest RMW failed.
    assert!(plugins.join("widget/Cargo.toml").exists());
    assert!(plugins.join("widget/src/lib.rs").exists());
}

// create_plugin: workspace manifest is entirely absent. The read arm surfaces
// as an Io error with the "failed to read workspace manifest" message.
#[test]
#[serial]
fn create_plugin_errors_when_workspace_manifest_missing() {
    let (_temp, fixtures, host) = boot_on_artifacts_copy();
    // No plugins/Cargo.toml exists (only the artifacts tree was copied).
    let err = host
        .create_plugin("gizmo", None)
        .expect_err("missing workspace manifest must abort create_plugin");
    assert!(
        matches!(&err, RuntimeError::Io { path, message } if path == &fixtures.join("plugins").join("Cargo.toml") && message.starts_with("failed to read workspace manifest:")),
        "expected Io, got {err:?}"
    );
}

// create_plugin: the workspace table has no `members` array → InvalidArgument
// "workspace.members not found".
#[test]
#[serial]
fn create_plugin_rejects_manifest_without_members() {
    let (_temp, fixtures, host) = boot_on_artifacts_copy();
    let plugins = fixtures.join("plugins");
    fs::create_dir_all(&plugins).expect("mkdir plugins");
    // Valid TOML, but no [workspace].members key.
    fs::write(
        plugins.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\n",
    )
    .expect("write manifest without members");

    let err = host
        .create_plugin("sprocket", None)
        .expect_err("manifest without members must abort create_plugin");
    assert!(
        matches!(&err, RuntimeError::InvalidArgument { message } if message == "workspace.members not found in plugins/Cargo.toml"),
        "expected InvalidArgument, got {err:?}"
    );
}

// walk_code_files: directories named target / .git / node_modules are pruned,
// so source files nested inside them are never visited, while a sibling source
// file at the tree root is.
#[test]
#[serial]
fn walk_code_files_prunes_target_git_and_node_modules() {
    let (_temp, _fixtures, host) = boot_on_artifacts_copy();
    let tree = TempDir::new().expect("walk tree");
    let root = tree.path();
    // A visible source file at the root.
    fs::write(root.join("keep.rs"), "// keep").expect("write keep.rs");
    // Source files buried in pruned directories.
    for dir in ["target", ".git", "node_modules"] {
        let d = root.join(dir);
        fs::create_dir_all(&d).expect("mkdir pruned dir");
        fs::write(d.join("hidden.rs"), "// hidden").expect("write hidden.rs");
    }

    let mut seen: Vec<String> = Vec::new();
    host.walk_code_files(root, &mut |rel, _abs| seen.push(rel.to_string()))
        .expect("walk should succeed");

    assert!(
        seen.iter().any(|r| r == "keep.rs"),
        "root source file should be visited: {seen:?}"
    );
    assert!(
        !seen.iter().any(|r| r.contains("hidden.rs")),
        "files under target/.git/node_modules must be pruned: {seen:?}"
    );
}

// walk_code_files honors an early Stop from the visitor via walk_code_files_ctl
// (the public non-ctl wrapper always Continues; this asserts the Stop return).
#[test]
#[serial]
fn walk_code_files_ctl_stops_early() {
    let (_temp, _fixtures, host) = boot_on_artifacts_copy();
    let tree = TempDir::new().expect("walk tree");
    let root = tree.path();
    for name in ["a.rs", "b.rs", "c.rs"] {
        fs::write(root.join(name), "// src").expect("write src");
    }

    let mut count = 0usize;
    host.walk_code_files_ctl(root, &mut |_, _| {
        count += 1;
        WalkControl::Stop
    })
    .expect("walk_ctl should succeed");
    assert_eq!(count, 1, "Stop after the first hit must end the walk");
}

// walk_code_files_ctl: descend into a NON-pruned nested subdirectory
// (`stack.push` + `continue` arm), skip a non-file / non-dir entry (a symlink
// whose target is a directory has a file_type that is neither `is_dir` nor
// `is_file` → the `!ft.is_file()` continue), and survive a subdirectory whose
// contents cannot be listed (a 0o000 dir makes the popped-dir `read_dir` fail
// → the `Err(_) => continue` arm). Together these cover the loop's traversal
// and IO-error branches without a real repo.
#[cfg(unix)]
#[test]
#[serial]
fn walk_code_files_descends_nested_skips_symlink_and_unreadable_dir() {
    use std::os::unix::fs::{symlink, PermissionsExt};

    let (_temp, _fixtures, host) = boot_on_artifacts_copy();
    let tree = TempDir::new().expect("walk tree");
    let root = tree.path();

    // A non-pruned nested subdir with a source file: exercises the
    // `stack.push(entry.path()); continue;` descent arm and then visits the
    // buried file on the next stack pop.
    let nested = root.join("nested");
    fs::create_dir_all(&nested).expect("mkdir nested");
    fs::write(nested.join("deep.rs"), "// deep").expect("write deep.rs");

    // A dangling-target-agnostic symlink pointing at a directory: its
    // `file_type()` reports `is_symlink()` (neither dir nor file), so the walk
    // takes the `!ft.is_file()` continue and never recurses through it.
    symlink(&nested, root.join("link_to_dir")).expect("create dir symlink");

    // A directory we then strip all permissions from: when popped off the
    // stack its `read_dir` fails, driving the `Err(_) => continue` arm.
    let locked = root.join("locked");
    fs::create_dir_all(&locked).expect("mkdir locked");
    fs::write(locked.join("secret.rs"), "// secret").expect("seed locked");
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("chmod 000");

    let mut seen: Vec<String> = Vec::new();
    let walk = host.walk_code_files(root, &mut |rel, _abs| seen.push(rel.to_string()));

    // Restore permissions so TempDir cleanup can remove the tree.
    let _ = fs::set_permissions(&locked, fs::Permissions::from_mode(0o755));
    walk.expect("walk should succeed despite the unreadable subdir");

    assert!(
        seen.iter().any(|r| r == "nested/deep.rs"),
        "nested source file must be visited via the descent arm: {seen:?}"
    );
    // The symlink-to-dir is never recursed, so its contents never appear under
    // the link name regardless of euid.
    assert!(
        !seen.iter().any(|r| r.starts_with("link_to_dir/")),
        "a directory symlink must not be recursed: {seen:?}"
    );
    // The 0o000 dir drives the `Err(_) => continue` arm only when mode bits are
    // enforced; root reads it anyway and reaches the file inside.
    // SAFETY: `geteuid` is always safe to call and cannot fail.
    let enforced = (unsafe { libc::geteuid() }) != 0;
    assert_eq!(
        seen.iter().any(|r| r.contains("secret.rs")),
        !enforced,
        "unreadable-dir contents must surface only as root (euid_enforced={enforced}): {seen:?}"
    );
}

// ===========================================================================
// Group K — reload_subtree Phase-1 strict-comparison error arms. A copied
// artifacts tree lets us tamper `index.json` so the recorded entry disagrees
// with what the live dylib reports, driving the docs-drift and ABI-drift
// mismatch branches (each returns Err + records a Failed attempt).
// ===========================================================================

// Docs drift: shrink the recorded `docs.nodes` for `expr` so the live dylib's
// node list no longer matches the index entry → reload_docs_mismatch_error.
#[test]
#[serial]
fn reload_subtree_docs_drift_records_failed_attempt() {
    let temp = setup_artifacts_only_copy();
    let host = RuntimeHost::boot(temp.path()).expect("host should boot from artifacts copy");

    let index_path = temp.path().join("artifacts/index.json");
    let raw = fs::read_to_string(&index_path).expect("read index");
    let mut index: serde_json::Value = serde_json::from_str(&raw).expect("parse index");
    let entries = index
        .get_mut("entries")
        .and_then(|v| v.as_array_mut())
        .expect("entries array");
    for entry in entries.iter_mut() {
        if entry.get("plugin_path").and_then(|v| v.as_str()) == Some("expr") {
            // Drop every recorded node so the count (0) differs from the live
            // dylib's (1). Node-count divergence trips the strict docs check.
            entry
                .get_mut("docs")
                .and_then(|d| d.get_mut("nodes"))
                .map(|n| *n = serde_json::json!([]))
                .expect("expr docs.nodes present");
        }
    }
    fs::write(&index_path, serde_json::to_string_pretty(&index).unwrap()).expect("write index");

    let err = host
        .reload("expr")
        .expect_err("docs drift must abort the subtree reload");
    assert!(
        matches!(&err, RuntimeError::AbiMismatch { .. }),
        "got {err:?}"
    );
    assert!(err.to_string().contains("docs mismatch"));
    let attempt = host.last_reload_attempt().expect("failed attempt recorded");
    assert_eq!(attempt.status, ReloadAttemptStatus::Failed);
    assert!(attempt.failure_summary.is_some());
}

// ABI drift: rewrite the recorded `abi_fingerprint.crate_hash`/`api_hash` for
// `expr` so the live dylib's fingerprint no longer matches → the
// reload_abi_fingerprint_mismatch_error arm. Docs are left intact so the docs
// check passes first and control reaches the fingerprint comparison.
#[test]
#[serial]
fn reload_subtree_abi_fingerprint_drift_records_failed_attempt() {
    let temp = setup_artifacts_only_copy();
    let host = RuntimeHost::boot(temp.path()).expect("host should boot from artifacts copy");

    let index_path = temp.path().join("artifacts/index.json");
    let raw = fs::read_to_string(&index_path).expect("read index");
    let mut index: serde_json::Value = serde_json::from_str(&raw).expect("parse index");
    let entries = index
        .get_mut("entries")
        .and_then(|v| v.as_array_mut())
        .expect("entries array");
    for entry in entries.iter_mut() {
        if entry.get("plugin_path").and_then(|v| v.as_str()) == Some("expr") {
            let fp = entry
                .get_mut("abi_fingerprint")
                .and_then(|v| v.as_object_mut())
                .expect("abi_fingerprint object");
            fp.insert(
                "crate_hash".to_string(),
                serde_json::json!("crate_expr_TAMPERED"),
            );
            fp.insert("api_hash".to_string(), serde_json::json!("api_TAMPERED"));
        }
    }
    fs::write(&index_path, serde_json::to_string_pretty(&index).unwrap()).expect("write index");

    let err = host
        .reload("expr")
        .expect_err("abi fingerprint drift must abort the subtree reload");
    assert!(
        matches!(&err, RuntimeError::AbiMismatch { .. }),
        "got {err:?}"
    );
    assert!(err.to_string().contains("expected crate="));
    let attempt = host.last_reload_attempt().expect("failed attempt recorded");
    assert_eq!(attempt.status, ReloadAttemptStatus::Failed);
    assert!(attempt.failure_summary.is_some());
}

// ===========================================================================
// Group L — small public-accessor and per-session error arms that the other
// coverage groups do not touch: workspace_root / config accessors, the
// AgentSessionNotFound arms of agent_transcript / agent_send, and
// check_agent_accessible's not-registered + not-allowed branches.
// ===========================================================================

#[test]
#[serial]
fn host_workspace_root_and_config_accessors_are_live() {
    let host = shared_host();
    // config() hands back the live RuntimeConfig (RuntimeHost accessor).
    let _ = host.config();
    // workspace_root() is the RuntimeKernel accessor; it points at the
    // workspace directory (parent of the fixtures root).
    assert!(host.kernel().workspace_root().is_dir());
}

#[test]
#[serial]
fn agent_transcript_and_send_report_unknown_session() {
    let host = shared_host();
    let err = host
        .agent_transcript("ghost-session")
        .expect_err("unknown session transcript must error");
    assert!(matches!(err, RuntimeError::AgentSessionNotFound { .. }));

    let err = host
        .agent_send("ghost-session", "hi")
        .expect_err("unknown session send must error");
    assert!(matches!(err, RuntimeError::AgentSessionNotFound { .. }));
}

#[test]
#[serial]
fn check_agent_accessible_covers_not_registered_and_not_allowed() {
    let host = shared_host();

    // Agent-accessible node → Ok (the success arm returns before the reject).
    host.check_agent_accessible("time", "time_now")
        .expect("time_now is agent-accessible");

    // Unknown plugin path → PluginNotRegistered.
    let err = host
        .check_agent_accessible("no_such_plugin", "whatever")
        .expect_err("unknown plugin must be rejected");
    assert!(matches!(err, RuntimeError::PluginNotRegistered { .. }));

    // Known plugin but unknown node id → falls through to the not-allowed
    // InvalidArgument arm (no matching node ⇒ no agent_accessible grant).
    let err = host
        .check_agent_accessible("expr", "definitely_not_a_node")
        .expect_err("unknown node must be rejected");
    assert!(
        matches!(&err, RuntimeError::InvalidArgument { message } if message.contains("not allowed")),
        "got {err:?}"
    );
}

// reload_with_diagnostics error arm: a subtree reload of a real plugin whose
// artifact index has been deleted fails, and reload_with_diagnostics returns a
// Failed ReloadAttemptReport (the `Err((err, attempt))` branch distinct from
// the `reload()` wrapper's error arm).
#[test]
#[serial]
fn reload_with_diagnostics_reports_failed_attempt_on_missing_index() {
    let temp = setup_artifacts_only_copy();
    let host = RuntimeHost::boot(temp.path()).expect("host should boot");

    let index = temp.path().join("artifacts/index.json");
    fs::remove_file(&index).expect("remove artifact index");

    let attempt = host.reload_with_diagnostics("expr");
    assert_eq!(attempt.status, ReloadAttemptStatus::Failed);
    assert!(attempt.failure_summary.is_some());
}

// ===========================================================================
// Group M — kernel plugin_iteration_status resolves a blocked (non-last)
// iteration through the blocked-map arm. Recording a Blocked outcome and then
// a DIFFERENT Promoted outcome makes the Promoted one the `last_plugin_
// iteration` slot, so querying the blocked id skips the last-slot fast path
// and hits the blocked-map lookup branch.
// ===========================================================================

#[test]
fn plugin_iteration_status_reads_blocked_map_when_not_last() {
    let temp = TempDir::new().expect("tempdir");
    let config = cordis_runtime::config::RuntimeConfig::default();
    let kernel = RuntimeKernel::new(temp.path(), &config);

    let blocked = make_result(
        "iter-blk",
        "issue-blk",
        PluginIterationFinalVerdict::Blocked,
    );
    kernel.record_plugin_iteration_outcome(&blocked);
    // A later Promoted iteration takes over the `last_plugin_iteration` slot.
    let promoted = make_result(
        "iter-prm",
        "issue-prm",
        PluginIterationFinalVerdict::Promoted,
    );
    kernel.record_plugin_iteration_outcome(&promoted);

    // Querying the still-blocked id: the last-slot filter rejects it (last is
    // iter-prm) so the lookup falls through to the blocked-map arm.
    let status = kernel
        .plugin_iteration_status("iter-blk")
        .expect("blocked iteration still queryable via the blocked map");
    assert_eq!(status.final_verdict, PluginIterationFinalVerdict::Blocked);
}
