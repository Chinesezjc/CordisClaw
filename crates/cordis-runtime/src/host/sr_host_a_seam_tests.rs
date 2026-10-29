use super::{
    missing_registry_trace, reload_abi_fingerprint_mismatch_error, reload_docs_mismatch_error,
    PendingSessionAction, RuntimeHost,
};
use crate::core::error::RuntimeError;
use crate::core::models::NodeOutcome;
use cordis_plugin_sdk::AbiFingerprint;
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

// ── Pure-fn: execute-path failure traces ─────────────────────────────

#[test]
fn missing_registry_trace_carries_transition_id_and_blank_plugin() {
    let trace = missing_registry_trace("expr::add", 2);
    assert_eq!(trace.node_fqn, "expr::add");
    assert_eq!(trace.plugin_path, "");
    assert_eq!(trace.node_id, "");
    assert_eq!(trace.attempt, 2);
    assert_eq!(trace.outcome, Some(NodeOutcome::Failure));
    assert_eq!(trace.request_payload, None);
    assert_eq!(trace.response_payload, None);
    assert_eq!(trace.error.as_deref(), Some("node missing from registry"));
}

/// Unwrap an `AbiMismatch` error into its fields, or `None` on any other
/// variant. Returning an `Option` (rather than panicking in a never-taken
/// arm) keeps both arms executable; callers `expect` the value, so a wrong
/// variant still fails the test at the call site.
fn abi_mismatch_fields(
    err: RuntimeError,
) -> Option<(String, AbiFingerprint, AbiFingerprint, Vec<String>)> {
    match err {
        RuntimeError::AbiMismatch {
            plugin_path,
            expected,
            actual,
            fingerprint_diff,
        } => Some((plugin_path, *expected, *actual, fingerprint_diff)),
        _ => None,
    }
}

#[test]
fn abi_mismatch_fields_returns_none_for_other_variants() {
    // Drives the non-`AbiMismatch` arm, which the reload tests never take.
    let other = RuntimeError::Invariant {
        message: "not an abi mismatch".to_string(),
    };
    assert!(abi_mismatch_fields(other).is_none());
}

// ── Pure-fn: reload_subtree Phase-1 AbiMismatch reports ──────────────

#[test]
fn reload_docs_mismatch_error_uses_entry_fingerprint_for_both_sides() {
    let fp = AbiFingerprint::current_build("crate_h", "api_h");
    let err = reload_docs_mismatch_error("qq", &fp, 3, 5);
    let (plugin_path, expected, actual, fingerprint_diff) =
        abi_mismatch_fields(err).expect("reload phase-1 must report AbiMismatch");
    assert_eq!(plugin_path, "qq");
    // Docs drifted, not the ABI hash → both sides are the entry fp.
    assert_eq!(expected, fp);
    assert_eq!(actual, fp);
    assert_eq!(
        fingerprint_diff,
        vec!["docs mismatch: expected 3 nodes, got 5".to_string()]
    );
}

#[test]
fn reload_abi_fingerprint_mismatch_error_reports_both_hashes() {
    let expected_fp = AbiFingerprint::current_build("crate_old", "api_old");
    let actual_fp = AbiFingerprint::current_build("crate_new", "api_new");
    let err = reload_abi_fingerprint_mismatch_error("svc", &expected_fp, actual_fp.clone());
    let (plugin_path, expected, actual, fingerprint_diff) =
        abi_mismatch_fields(err).expect("reload phase-1 must report AbiMismatch");
    assert_eq!(plugin_path, "svc");
    assert_eq!(expected, expected_fp);
    assert_eq!(actual, actual_fp);
    assert_eq!(
        fingerprint_diff,
        vec![format!(
            "expected crate={} api={}, got crate={} api={}",
            expected_fp.crate_hash, expected_fp.api_hash, actual_fp.crate_hash, actual_fp.api_hash,
        )]
    );
}

// ── pub(crate) session-map accessors on a cheap empty-index host ─────

/// A near-instant, cross-platform fixtures tree: an `artifacts/index.json`
/// with zero entries. Boot registers no plugins and touches no dylib.
fn setup_empty_fixture() -> (TempDir, PathBuf) {
    let temp = TempDir::new().expect("tempdir");
    let fixtures = temp.path().join("fixtures");
    let artifacts = fixtures.join("artifacts");
    fs::create_dir_all(&artifacts).expect("create artifacts dir");
    fs::write(
        artifacts.join("index.json"),
        r#"{
  "schema_version": 2,
  "generated_at": "2026-07-24T00:00:00Z",
  "topo_order": [],
  "entries": []
}
"#,
    )
    .expect("write empty artifact index");
    (temp, fixtures)
}

#[test]
fn session_accessors_start_empty_and_queue_action_is_noop_on_fresh_host() {
    let (_temp, fixtures) = setup_empty_fixture();
    let host = RuntimeHost::boot(&fixtures).expect("boot on empty index");

    // agent_sessions_mut: fresh boot registers no interactive sessions.
    assert!(
        host.agent_sessions_mut().is_empty(),
        "a freshly booted host must have no agent sessions"
    );

    // revert_interactive_changes: nothing was edited → zero restored.
    assert_eq!(
        host.revert_interactive_changes()
            .expect("revert on a clean rollback log succeeds"),
        0
    );

    // queue_session_action: enqueues without touching agent_sessions. There
    // is no in-crate reader for the pending map, so we assert the observable
    // invariant (agent_sessions untouched) and that the call is total.
    host.queue_session_action("session-xyz", PendingSessionAction::CompactHistory);
    host.queue_session_action("session-xyz", PendingSessionAction::CompactHistory);
    assert!(
        host.agent_sessions_mut().is_empty(),
        "queue_session_action must not create an agent session entry"
    );
}
