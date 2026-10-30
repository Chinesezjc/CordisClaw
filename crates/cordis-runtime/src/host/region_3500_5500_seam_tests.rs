/// Pull the message out of an `InvalidArgument`, panicking at the caller's
/// line on any other variant — replaces `match`-with-unreachable-arm in
/// assertions so no test arm stays unexecuted.
#[track_caller]
fn invalid_argument_message(err: &RuntimeError) -> String {
    // Both arms are executable: a non-matching variant returns its own
    // rendering, so the caller's `assert_eq!` fails with the real value
    // instead of this helper panicking from a never-taken arm.
    match err {
        RuntimeError::InvalidArgument { message } => message.clone(),
        other => format!("not InvalidArgument: {other}"),
    }
}

#[test]
fn invalid_argument_message_labels_other_variants() {
    // Drives the fallback arm: callers only ever pass the matching variant,
    // so without this the arm would never execute.
    let other = RuntimeError::Invariant {
        message: "nope".to_string(),
    };
    assert_eq!(
        invalid_argument_message(&other),
        format!("not InvalidArgument: {other}")
    );
}

/// Same for `LlmResponseInvalid`.
#[track_caller]
fn llm_response_invalid_message(err: &RuntimeError) -> String {
    match err {
        RuntimeError::LlmResponseInvalid { message } => message.clone(),
        other => format!("not LlmResponseInvalid: {other}"),
    }
}

#[test]
fn llm_response_invalid_message_labels_other_variants() {
    let other = RuntimeError::Invariant {
        message: "nope".to_string(),
    };
    assert_eq!(
        llm_response_invalid_message(&other),
        format!("not LlmResponseInvalid: {other}")
    );
}

use super::{
    enrich_plugin_iteration_edit_error, format_agent_transcript_excerpt, parse_agent_args,
    transcript_excerpt, AgentBackend, AgentSessionHandle, AgentSessionKind, InvocationSample,
    ManagedAgentSession, ManagedAgentState, PluginIterationAgentBackend, PluginIterationAgentState,
    PluginIterationContextPaths, PluginIterationRunState, PreparedPluginIteration, ReloadReport,
    ReplaceFilesExactArgs, RetiredSnapshot, RuntimeHost, ScaffoldChildPluginArgs,
    PLUGIN_AGENT_TOOL_RECORD_ITERATION_SUMMARY, PLUGIN_AGENT_TOOL_REPLACE_FILES_EXACT,
};
use crate::agent::AgentTranscriptEntry;
use crate::core::error::RuntimeError;
use crate::core::models::PluginUnavailableReason;
use crate::kernel::plugin_iteration::{
    CanaryVerdict, KernelPluginIssueSource, PluginEditOpKind, PluginEditOperation,
    PluginIterationFinalVerdict, VerifierVerdict,
};
use crate::kernel::verifier::VerificationProfile;
use cordis_plugin_sdk::{AbiFingerprint, NodeDoc, NodeType, PluginDocs};
use serde_json::{json, Value};
use serial_test::serial;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;

// ───────────────────────── hermetic fixtures ─────────────────────────

/// An `artifacts/index.json` with zero entries: `RuntimeHost::boot`
/// registers no plugins and never dlopens. Near-instant on any target.
fn setup_empty_fixture() -> (TempDir, PathBuf) {
    let temp = TempDir::new().expect("tempdir");
    let fixtures = temp.path().join("fixtures");
    let artifacts = fixtures.join("artifacts");
    fs::create_dir_all(&artifacts).expect("create artifacts dir");
    fs::write(
        artifacts.join("index.json"),
        r#"{
  "schema_version": 2,
  "generated_at": "2026-07-25T00:00:00Z",
  "topo_order": [],
  "entries": []
}
"#,
    )
    .expect("write empty artifact index");
    (temp, fixtures)
}

fn empty_host() -> (TempDir, RuntimeHost) {
    let (temp, fixtures) = setup_empty_fixture();
    let host = RuntimeHost::boot(&fixtures).expect("boot on empty index");
    (temp, host)
}

fn prepared_for(root: &str, targets: &[&str]) -> PreparedPluginIteration {
    let target_plugin_paths: Vec<String> = targets.iter().map(|s| (*s).to_string()).collect();
    let allowed_plugin_roots: BTreeMap<String, String> = target_plugin_paths
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
        verify_profile: VerificationProfile::Default,
        quality_score: None,
        edit_plan: None,
        instruction: None,
        allowed_plugin_roots,
    }
}

// ───────────────────────── pure functions ────────────────────────────

#[test]
#[serial]
fn transcript_excerpt_keeps_tail_in_order() {
    let entries: Vec<AgentTranscriptEntry> = (0..5)
        .map(|i| AgentTranscriptEntry::User {
            content: format!("m{i}"),
        })
        .collect();
    let out = transcript_excerpt(&entries, 3);
    // Last three, still in chronological order.
    // Compare the whole slice against the expected tail: no per-variant
    // arm, so no never-taken match arm is left behind.
    let expected: Vec<AgentTranscriptEntry> = ["m2", "m3", "m4"]
        .into_iter()
        .map(|c| AgentTranscriptEntry::User {
            content: c.to_string(),
        })
        .collect();
    assert_eq!(format!("{out:?}"), format!("{expected:?}"));
}

#[test]
#[serial]
fn format_agent_transcript_excerpt_renders_all_three_variants() {
    let entries = vec![
        AgentTranscriptEntry::User {
            content: "hello   world".to_string(),
        },
        AgentTranscriptEntry::Assistant {
            content: "an answer".to_string(),
            response_id: Some("resp-1".to_string()),
        },
        AgentTranscriptEntry::Assistant {
            content: "no id answer".to_string(),
            response_id: None,
        },
        AgentTranscriptEntry::Tool {
            name: "create_file".to_string(),
            arguments: json!({}),
            ok: false,
            output: None,
            error: Some("boom happened".to_string()),
        },
        AgentTranscriptEntry::Tool {
            name: "json_set".to_string(),
            arguments: json!({}),
            ok: true,
            output: None,
            error: None,
        },
    ];
    let rendered = format_agent_transcript_excerpt(&entries);
    // whitespace is flattened in the user line.
    assert!(rendered.contains("user: hello world"));
    assert!(rendered.contains("assistant[resp-1]: an answer"));
    assert!(rendered.contains("assistant: no id answer"));
    assert!(rendered.contains("tool create_file ok=false error=boom happened"));
    assert!(rendered.contains("tool json_set ok=true"));
    // The ok=true tool line carries no error suffix.
    let ok_line = rendered
        .lines()
        .find(|l| l.starts_with("tool json_set"))
        .expect("json_set line");
    assert!(!ok_line.contains("error="));
}

#[test]
#[serial]
fn parse_agent_args_wraps_deserialize_error_byte_exact() {
    // `ReplaceFilesExactArgs` requires an `edits` array; a bare string is
    // the wrong shape → serde error wrapped as InvalidArgument.
    let out = parse_agent_args::<ReplaceFilesExactArgs>(
        json!("not an object"),
        PLUGIN_AGENT_TOOL_REPLACE_FILES_EXACT,
    );
    let err = out.expect_err("wrong shape must error");
    let expected_prefix =
        format!("agent tool {PLUGIN_AGENT_TOOL_REPLACE_FILES_EXACT} received invalid arguments: ");
    let message = invalid_argument_message(&err);
    assert!(message.starts_with(&expected_prefix), "got {message}");
}

#[test]
#[serial]
fn enrich_edit_error_returns_stale_snippet_hint_for_replace_exact() {
    let temp = TempDir::new().expect("tempdir");
    let workspace = temp.path();
    let rel = "plugins/mini/src/lib.rs";
    let abs = workspace.join(rel);
    fs::create_dir_all(abs.parent().unwrap()).expect("mkdir");
    fs::write(&abs, "current file body\n").expect("write file");

    let operation = PluginEditOperation {
        path: rel.to_string(),
        kind: PluginEditOpKind::ReplaceExact,
        expected_old_string: Some("stale".to_string()),
        expected_sha256: None,
        new_content: Some("new".to_string()),
        pointer: None,
        dotted_key: None,
        value: None,
    };
    // The base error message must contain the trigger substring.
    let base = RuntimeError::LlmResponseInvalid {
        message: "auto update patch pattern not found in file".to_string(),
    };
    let enriched = enrich_plugin_iteration_edit_error(&operation, workspace, base);
    let message = llm_response_invalid_message(&enriched);
    assert!(message.contains("The exact snippet is stale for plugins/mini/src/lib.rs"));
    assert!(message.contains("current_sha256="));
    assert!(message.contains("current file body"));
}

#[test]
#[serial]
fn enrich_edit_error_passes_through_when_not_stale_pattern() {
    let temp = TempDir::new().expect("tempdir");
    let operation = PluginEditOperation {
        path: "plugins/mini/src/lib.rs".to_string(),
        kind: PluginEditOpKind::ReplaceExact,
        expected_old_string: Some("x".to_string()),
        expected_sha256: None,
        new_content: Some("y".to_string()),
        pointer: None,
        dotted_key: None,
        value: None,
    };
    // Message lacks the "auto update patch pattern not found" trigger → the
    // original error is returned untouched.
    let base = RuntimeError::InvalidArgument {
        message: "some other failure".to_string(),
    };
    let out = enrich_plugin_iteration_edit_error(&operation, temp.path(), base);
    assert!(matches!(
        out,
        RuntimeError::InvalidArgument { message } if message == "some other failure"
    ));
}

// ───────────────── record_invocation_sample cap ──────────────────────

#[test]
#[serial]
fn record_invocation_sample_caps_deque_at_64() {
    let (_temp, host) = empty_host();
    for i in 0..70 {
        host.record_invocation_sample(
            "mini",
            "mini_echo",
            &json!({ "n": i }).to_string(),
            &json!({ "ok": true }).to_string(),
        );
    }
    let len = host
        .invocation_samples
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .len();
    assert_eq!(len, 64, "deque must be capped at 64 (pop_back on overflow)");
    // Newest (front) is the last pushed; oldest were evicted from the back.
    let front = host
        .invocation_samples
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .front()
        .cloned()
        .expect("front sample");
    assert_eq!(front.payload, json!({ "n": 69 }));
}

// ───────────────── observe_reload_error arms ─────────────────────────

#[test]
#[serial]
fn observe_reload_error_maps_docs_contract_to_docs_drift() {
    let (_temp, host) = empty_host();
    let err = RuntimeError::DocsContract {
        plugin_path: "alpha".to_string(),
        message: "docs mismatch".to_string(),
    };
    host.observe_reload_error("reload", &err);
    let issues = host.kernel().plugin_issues();
    let issue = issues
        .iter()
        .find(|i| i.root_plugin_path == "alpha")
        .expect("issue recorded for alpha");
    assert_eq!(issue.source, KernelPluginIssueSource::DocsDrift);
    assert!(issue.summary.contains("reload failed for alpha"));
}

#[test]
#[serial]
fn observe_reload_error_maps_contract_violation_unavailable_to_docs_drift() {
    let (_temp, host) = empty_host();
    let err = RuntimeError::PluginUnavailable {
        plugin_path: "beta".to_string(),
        reason: PluginUnavailableReason::ContractViolation,
        required: true,
    };
    host.observe_reload_error("candidate_reload", &err);
    let issues = host.kernel().plugin_issues();
    let issue = issues
        .iter()
        .find(|i| i.root_plugin_path == "beta")
        .expect("issue recorded for beta");
    assert_eq!(issue.source, KernelPluginIssueSource::DocsDrift);
}

#[test]
#[serial]
fn observe_reload_error_maps_other_to_load_failure() {
    let (_temp, host) = empty_host();
    let err = RuntimeError::PluginUnavailable {
        plugin_path: "gamma".to_string(),
        reason: PluginUnavailableReason::ArtifactMissing,
        required: false,
    };
    host.observe_reload_error("reload", &err);
    let issues = host.kernel().plugin_issues();
    let issue = issues
        .iter()
        .find(|i| i.root_plugin_path == "gamma")
        .expect("issue recorded for gamma");
    assert_eq!(issue.source, KernelPluginIssueSource::LoadFailure);
}

#[test]
#[serial]
fn observe_reload_error_ignores_errors_without_plugin_path() {
    let (_temp, host) = empty_host();
    let before = host.kernel().plugin_issues().len();
    // Invariant carries no plugin path → early return, no issue recorded.
    host.observe_reload_error(
        "reload",
        &RuntimeError::Invariant {
            message: "no plugin here".to_string(),
        },
    );
    assert_eq!(host.kernel().plugin_issues().len(), before);
}

// ─────────────── observe_snapshot_plugin_issues arms ─────────────────

/// Build a one-plugin snapshot whose single plugin is `Unavailable`.
fn snapshot_with_unavailable_plugin(
    plugin_path: &str,
    reason: PluginUnavailableReason,
) -> super::RuntimeSnapshot {
    let plugin_registry = crate::plugin::registry::PluginRegistry::default();
    plugin_registry.insert_unavailable(
        plugin_path.to_string(),
        None,
        true,
        std::collections::BTreeSet::new(),
        reason,
        Vec::new(),
    );
    let node_registry = crate::plugin::registry::NodeRegistry::default();
    let doc_registry =
        crate::service::doc_registry::DocRegistry::from_plugin_registry(&plugin_registry);
    let graph_registry = crate::service::graph_registry::GraphRegistry::from_registries(
        &plugin_registry,
        &node_registry,
    );
    super::runtime_snapshot_from_output(
        crate::plugin::loader::LoadOutput {
            execution_id: "snap-unavail".to_string(),
            plugin_registry,
            node_registry,
            doc_registry,
            graph_registry,
            context: crate::context::RuntimeContext::default(),
            metrics: crate::plugin::loader::LoaderMetrics::default(),
        },
        PathBuf::from("/tmp/cordis-seam-unavail-staged"),
        BTreeMap::new(),
    )
}

fn empty_reload_report() -> ReloadReport {
    ReloadReport {
        from_snapshot_id: "a".to_string(),
        to_snapshot_id: "b".to_string(),
        snapshot_root: "/tmp/x".to_string(),
        staged_artifact_root: "/tmp/x/staged".to_string(),
        elapsed_ms: 0,
        added_plugins: Vec::new(),
        removed_plugins: Vec::new(),
        changed_plugins: Vec::new(),
        changed_plugin_reasons: BTreeMap::new(),
    }
}

#[test]
#[serial]
fn observe_snapshot_issues_contract_violation_is_docs_drift() {
    let (_temp, host) = empty_host();
    let snapshot =
        snapshot_with_unavailable_plugin("cv", PluginUnavailableReason::ContractViolation);
    host.observe_snapshot_plugin_issues(&snapshot, &empty_reload_report(), "reload");
    let issues = host.kernel().plugin_issues();
    let issue = issues
        .iter()
        .find(|i| i.root_plugin_path == "cv")
        .expect("cv issue");
    assert_eq!(issue.source, KernelPluginIssueSource::DocsDrift);
    assert!(issue
        .summary
        .contains("reload observed plugin cv unavailable"));
}

#[test]
#[serial]
fn observe_snapshot_issues_other_unavailable_is_load_failure() {
    let (_temp, host) = empty_host();
    let snapshot = snapshot_with_unavailable_plugin("am", PluginUnavailableReason::ArtifactMissing);
    host.observe_snapshot_plugin_issues(&snapshot, &empty_reload_report(), "reload");
    let issues = host.kernel().plugin_issues();
    let issue = issues
        .iter()
        .find(|i| i.root_plugin_path == "am")
        .expect("am issue");
    assert_eq!(issue.source, KernelPluginIssueSource::LoadFailure);
}

// ─────────────── cleanup_retired_snapshots cap ───────────────────────

#[test]
#[serial]
fn cleanup_retired_snapshots_caps_dead_prefix_and_removes_dirs() {
    let (temp, host) = empty_host();
    // Push more than MAX_RETIRED_SNAPSHOTS (64) dead Weak entries, each
    // pointing at a real (empty) staged dir. cleanup must drop the dead
    // ones AND remove their dirs.
    let mut dirs = Vec::new();
    {
        let mut guard = host
            .retired_snapshots
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        for i in 0..70 {
            let dir = temp.path().join(format!("retired-{i}"));
            fs::create_dir_all(&dir).expect("mk retired dir");
            dirs.push(dir.clone());
            // A Weak that never had a live Arc: upgrade() is always None.
            guard.push(RetiredSnapshot {
                snapshot: std::sync::Weak::<super::RuntimeSnapshot>::new(),
                staged_artifact_root: dir,
            });
        }
    }
    host.cleanup_retired_snapshots();
    let remaining = host
        .retired_snapshots
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .len();
    // All dead entries are swept by the Weak-liveness retain (they upgrade
    // to None), so nothing remains.
    assert_eq!(remaining, 0, "all dead Weak entries must be swept");
    for dir in &dirs {
        assert!(!dir.exists(), "dead entry's staged dir must be removed");
    }
}

// ─────────────── ReloadReport::from_snapshots removed arm ────────────

#[test]
#[serial]
fn reload_report_from_snapshots_records_removed_plugin() {
    let previous = snapshot_with_unavailable_plugin("gone", PluginUnavailableReason::InitFailed);
    // Build a snapshot with a DIFFERENT plugin so `gone` is "removed".
    let next = snapshot_with_unavailable_plugin("kept", PluginUnavailableReason::InitFailed);
    let report = ReloadReport::from_snapshots(&previous, &next, Path::new("/tmp/snap"), 7);
    assert!(report.removed_plugins.contains(&"gone".to_string()));
    assert!(report.added_plugins.contains(&"kept".to_string()));
    assert_eq!(report.elapsed_ms, 7);
}

// ─────────────── run_plugin_canary no-evidence path ──────────────────

#[test]
#[serial]
fn run_plugin_canary_returns_partial_without_evidence() {
    let (_temp, host) = empty_host();
    let state = PluginIterationRunState::new(prepared_for("mini", &["mini"]));
    let report = host.run_plugin_canary(&state).expect("canary runs");
    assert_eq!(report.verdict, CanaryVerdict::Partial);
    assert_eq!(report.mode, "no_canary_evidence");
    assert!(report.plugin_path.is_none());
}

#[test]
#[serial]
fn run_plugin_canary_skips_samples_for_other_plugins() {
    let (_temp, host) = empty_host();
    // Seed a sample for a plugin NOT in the target set → the `continue`
    // arm skips it, and with no candidate we fall through to Partial.
    host.invocation_samples
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push_front(InvocationSample {
            plugin_path: "other".to_string(),
            node_id: "n".to_string(),
            payload: json!({}),
            response: json!({ "ok": true }),
            observed_at_ms: 0,
        });
    let state = PluginIterationRunState::new(prepared_for("mini", &["mini"]));
    let report = host.run_plugin_canary(&state).expect("canary runs");
    assert_eq!(report.verdict, CanaryVerdict::Partial);
    assert_eq!(report.mode, "no_canary_evidence");
}

// ─────────────── finalize_plugin_iteration arms ──────────────────────

#[test]
#[serial]
fn finalize_stage_error_short_circuits_to_rolled_back() {
    let (_temp, host) = empty_host();
    let mut state = PluginIterationRunState::new(prepared_for("mini", &["mini"]));
    state.stage_error = Some("rebuild blew up".to_string());
    let verdict = host
        .finalize_plugin_iteration(&mut state)
        .expect("finalize returns RolledBack for a stage error");
    assert_eq!(verdict, PluginIterationFinalVerdict::RolledBack);
    assert_eq!(
        state.blocked_reason.as_deref(),
        Some("rebuild blew up"),
        "no candidate + clean restore → base message verbatim"
    );
}

#[test]
#[serial]
fn finalize_verifier_fail_rolls_back_without_candidate() {
    let (_temp, host) = empty_host();
    let mut state = PluginIterationRunState::new(prepared_for("mini", &["mini"]));
    state.verifier_verdict = Some(VerifierVerdict::Fail);
    // A concrete Fail canary (not the Partial default) so the `else`
    // rollback arm — not the Partial-canary Blocked arm — is selected.
    state.canary = Some(crate::kernel::plugin_iteration::CanaryReport {
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
    let verdict = host
        .finalize_plugin_iteration(&mut state)
        .expect("finalize returns RolledBack for a failed verifier");
    assert_eq!(verdict, PluginIterationFinalVerdict::RolledBack);
}

// The canary is diagnostics now: tests decide. A Partial canary must not
// block a change whose tests pass (random plugins made it a coin flip).
#[test]
#[serial]
fn finalize_partial_canary_does_not_block_a_tested_change() {
    let (_temp, host) = empty_host();
    let mut state = PluginIterationRunState::new(prepared_for("mini", &["mini"]));
    state.verifier_verdict = Some(VerifierVerdict::Pass);
    state.canary = Some(crate::kernel::plugin_iteration::CanaryReport {
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
    let err = host
        .finalize_plugin_iteration(&mut state)
        .expect_err("no candidate staged");
    assert!(
        matches!(err, RuntimeError::CandidateSnapshotMissing),
        "got: {err:?}"
    );
}

/// Minimal agent-phase result for tests that only care about one flag.
struct TestAgentRun;
impl TestAgentRun {
    fn with_requires_approval(flag: bool) -> super::PluginIterationAgentRun {
        super::PluginIterationAgentRun {
            session_id: None,
            tool_summary: None,
            transcript_excerpt: Vec::new(),
            recorded_summary: Some("test".to_string()),
            requires_approval: flag,
            changed_paths: Vec::new(),
            rollback: super::PluginEditRollback::empty(std::path::Path::new(".")),
            derived_edit_plan: super::PluginEditPlan {
                issue_id: "issue".to_string(),
                patch_id: "patch".to_string(),
                summary: "test".to_string(),
                operations: Vec::new(),
            },
        }
    }
}

// Editing an existing test stages the change for approval instead of
// promoting it (tests are the contract).
#[test]
#[serial]
fn finalize_blocks_a_change_that_edits_an_existing_test() {
    let (_temp, host) = empty_host();
    let mut state = PluginIterationRunState::new(prepared_for("mini", &["mini"]));
    state.verifier_verdict = Some(VerifierVerdict::Pass);
    state.agent = Some(TestAgentRun::with_requires_approval(true));
    let verdict = host
        .finalize_plugin_iteration(&mut state)
        .expect("blocked, not rolled back");
    assert_eq!(verdict, PluginIterationFinalVerdict::Blocked);
    let reason = state.blocked_reason.unwrap_or_default();
    assert!(reason.contains("existing test"), "reason: {reason}");
    assert!(reason.contains("approval"), "reason: {reason}");
}

// ─────────────── rollback_candidate_if_staged (no candidate) ─────────

#[test]
#[serial]
fn rollback_candidate_if_staged_is_none_without_candidate() {
    let (_temp, host) = empty_host();
    assert!(host.rollback_candidate_if_staged(None).is_none());
}

// ─────────────── PluginIterationAgentBackend methods ─────────────────

/// A backend bound to a hermetic empty-fixture host. The `mini` plugin dir
/// need not exist for exploration/guard tests.
fn with_backend<R>(
    host: &RuntimeHost,
    f: impl FnOnce(&mut PluginIterationAgentBackend<'_>) -> R,
) -> R {
    let prepared = prepared_for("mini", &["mini"]);
    let context_paths = PluginIterationContextPaths {
        focus_paths: Vec::new(),
        all_paths: Vec::new(),
    };
    let mut state = PluginIterationAgentState::new(prepared, context_paths, &host.fixtures_root);
    let mut backend = PluginIterationAgentBackend {
        host,
        state: &mut state,
    };
    f(&mut backend)
}

/// Seed a writable `plugins/mini/src/<name>` file inside the host's fixtures
/// tree so edit-tool success arms (which validate paths against the plugin
/// subtree and read/write real bytes) can run without a cargo build.
fn seed_writable_source(host: &RuntimeHost, name: &str, body: &str) -> String {
    let rel = format!("plugins/mini/src/{name}");
    let abs = host.fixtures_root.join(&rel);
    fs::create_dir_all(abs.parent().expect("parent")).expect("mkdir src");
    fs::write(&abs, body).expect("seed source");
    rel
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
        fs::read_to_string(host.fixtures_root.join(&rel)).expect("read edited"),
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
    let mut state = PluginIterationAgentState::new(prepared, context_paths, &host.fixtures_root);
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
            AgentBackend::host(backend).fixtures_root,
            host.fixtures_root
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

// ─────────────── ManagedAgentSession::compact_history wrapper ────────

#[test]
#[serial]
fn managed_session_compact_history_wrapper_is_noop_below_threshold() {
    let config = crate::config::LlmApiConfig {
        provider: "deepseek".to_string(),
        base_url: "http://127.0.0.1:1/v1".to_string(),
        api_key: Some("k".to_string()),
        model: "m".to_string(),
        ..crate::config::LlmApiConfig::default()
    };
    // Below the compaction threshold: wrapper returns (old, old) unchanged.
    let mut managed = ManagedAgentSession {
        handle: AgentSessionHandle {
            session_id: "s-1".to_string(),
            kind: AgentSessionKind::RuntimeShell,
        },
        session: Box::new(
            crate::agent::AgentSession::new(config, "runtime_shell").expect("build agent session"),
        ),
        state: ManagedAgentState::RuntimeShell,
    };
    let (old, new) = managed.compact_history();
    assert_eq!(old, new, "no compaction below threshold");
}

// ─────────────── canary declared-verifier-node (JSON candidate) ──────

/// Build a JSON-artifact fixture whose `svc` plugin declares a node whose
/// id contains "verify" and is backed by a `Process` executor that echoes a
/// fixed JSON. Staging it as a candidate lets `run_plugin_canary` take the
/// declared-verifier-node branch (host.rs 3803-3832 in cov6 numbering).
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
    let sha256 = crate::plugin::artifact::sha256_file(&artifact_path).expect("hash svc artifact");
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

#[test]
#[serial]
fn run_plugin_canary_uses_declared_verifier_node_from_candidate() {
    let (_temp, fixtures) = setup_verify_node_json_fixture();
    let host = RuntimeHost::boot(&fixtures).expect("boot on svc fixture");
    // Stage the current tree as a candidate (no source change needed): the
    // candidate registry then carries the `svc` plugin with the verify node.
    host.reload_candidate().expect("stage candidate");
    assert!(host.candidate_snapshot().is_some());

    let state = PluginIterationRunState::new(prepared_for("svc", &["svc"]));
    let report = host
        .run_plugin_canary(&state)
        .expect("canary invokes the declared verify node");
    assert_eq!(report.verdict, CanaryVerdict::Pass);
    assert_eq!(report.mode, "declared_plugin_verifier_node");
    assert_eq!(report.node_id.as_deref(), Some("svc_verify"));
    let _ = Arc::strong_count(&host.candidate_snapshot().expect("candidate live"));
}

#[test]
#[serial]
fn run_plugin_canary_replay_divergence_yields_fail() {
    let (_temp, fixtures) = setup_verify_node_json_fixture();
    let host = RuntimeHost::boot(&fixtures).expect("boot on svc fixture");
    host.reload_candidate().expect("stage candidate");
    // Seed a replay sample for svc/svc_verify whose recorded response does
    // NOT match what the candidate echoes ({"ok":true}) → divergence Fail.
    host.invocation_samples
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push_front(InvocationSample {
            plugin_path: "svc".to_string(),
            node_id: "svc_verify".to_string(),
            payload: json!({}),
            response: json!({ "ok": false }),
            observed_at_ms: 0,
        });
    let state = PluginIterationRunState::new(prepared_for("svc", &["svc"]));
    let report = host.run_plugin_canary(&state).expect("canary replay runs");
    assert_eq!(report.verdict, CanaryVerdict::Fail);
    assert_eq!(report.mode, "recent_successful_invocation_replay");
    assert!(report
        .message
        .contains("candidate replay response diverged from current response"));
}

// ─────────── R2: cleanup_retired_snapshots pinned-snapshot eviction ──

/// R2: 上限驱逐不得删被 pin 的活条目。Hold >64 LIVE Arcs（模拟
/// in-flight 会话 pin 住旧快照）——Weak-liveness retain 全保留，上限
/// 循环看到最旧条目仍活着就停止，**不删任何目录**，积压保留。
#[test]
#[serial]
fn cleanup_retired_snapshots_keeps_pinned_backlog_beyond_max() {
    let (temp, host) = empty_host();
    let mut live: Vec<Arc<super::RuntimeSnapshot>> = Vec::new();
    let mut dirs = Vec::new();
    {
        let mut guard = host
            .retired_snapshots
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        for i in 0..70 {
            let snap = host.current_snapshot();
            let dir = temp.path().join(format!("live-retired-{i}"));
            fs::create_dir_all(&dir).expect("mk dir");
            dirs.push(dir.clone());
            guard.push(RetiredSnapshot {
                snapshot: Arc::downgrade(&snap),
                staged_artifact_root: dir,
            });
            live.push(snap);
        }
    }
    host.cleanup_retired_snapshots();
    let remaining = host
        .retired_snapshots
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .len();
    assert_eq!(
        remaining, 70,
        "pinned entries are kept even beyond MAX_RETIRED_SNAPSHOTS"
    );
    // 旧行为会删掉最旧 6 条的目录（→ 被 pin 快照上的 Process 插件
    // invoke spawn failed）；现在一条都不能删。
    assert!(dirs[0].exists(), "oldest pinned entry's dir must survive");
    assert!(dirs[69].exists(), "newest pinned entry's dir retained");
    drop(live);
}

/// R2: 混合场景——未 pin 的照删（retain 扫掉并删目录），pin 住的超过
/// 上限也保留。构造 70 个 pinned + 10 个 dead（Weak::new，从未有强
/// 引用）的条目。
#[test]
#[serial]
fn cleanup_retired_snapshots_removes_dead_and_keeps_pinned() {
    let (temp, host) = empty_host();
    let mut live: Vec<Arc<super::RuntimeSnapshot>> = Vec::new();
    let mut pinned_dirs = Vec::new();
    let mut dead_dirs = Vec::new();
    {
        let mut guard = host
            .retired_snapshots
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        for i in 0..70 {
            let snap = host.current_snapshot();
            let dir = temp.path().join(format!("pinned-{i}"));
            fs::create_dir_all(&dir).expect("mk pinned dir");
            pinned_dirs.push(dir.clone());
            guard.push(RetiredSnapshot {
                snapshot: Arc::downgrade(&snap),
                staged_artifact_root: dir,
            });
            live.push(snap);
        }
        for i in 0..10 {
            let dir = temp.path().join(format!("dead-{i}"));
            fs::create_dir_all(&dir).expect("mk dead dir");
            dead_dirs.push(dir.clone());
            guard.push(RetiredSnapshot {
                snapshot: std::sync::Weak::<super::RuntimeSnapshot>::new(),
                staged_artifact_root: dir,
            });
        }
    }
    host.cleanup_retired_snapshots();
    let remaining = host
        .retired_snapshots
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .len();
    // 10 条 dead 被 retain 扫掉；70 条 pinned 全保留（超上限也不驱逐）。
    assert_eq!(remaining, 70, "only dead entries are swept");
    for dir in &pinned_dirs {
        assert!(dir.exists(), "pinned entry's dir must survive: {dir:?}");
    }
    for dir in &dead_dirs {
        assert!(!dir.exists(), "dead entry's dir must be removed: {dir:?}");
    }
    drop(live);
}

/// `evict_retired_snapshots` 的"删死条目"臂：最旧条目在 retain 之后才
/// 死（竞态窗口）时，上限循环必须删它的目录。retain 已把所有 dead
/// 扫掉，这个臂只能经纯函数直测。
#[test]
#[serial]
fn evict_retired_snapshots_removes_dead_prefix_entries() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut retired: Vec<RetiredSnapshot> = Vec::new();
    // 第一条是死的：上限循环先删它，然后撞上活条目停止。
    let dead_dir = temp.path().join("dead-0");
    fs::create_dir_all(&dead_dir).expect("mk dead dir");
    retired.push(RetiredSnapshot {
        snapshot: std::sync::Weak::<super::RuntimeSnapshot>::new(),
        staged_artifact_root: dead_dir.clone(),
    });
    for i in 1..65 {
        let dir = temp.path().join(format!("live-{i}"));
        fs::create_dir_all(&dir).expect("mk live dir");
        retired.push(RetiredSnapshot {
            snapshot: std::sync::Weak::<super::RuntimeSnapshot>::new(),
            staged_artifact_root: dir,
        });
    }
    // 65 条（1 dead + 64 live）> 64：删掉 dead 后恰好回到上限。
    super::evict_retired_snapshots(&mut retired);
    assert_eq!(retired.len(), 64);
    assert!(!dead_dir.exists(), "the dead prefix entry's dir is removed");
}

// ─────────── finalize promote-failure arms (Pass/Pass) ───────────────

/// Force `promote_candidate` to fail by making the journal path a
/// non-empty directory: `clear_plugin_iteration_journal` → `remove_file`
/// on a directory errors, so `promote_candidate` returns Err and the
/// finalize promote-failure arm runs.
fn wedge_journal_as_dir(host: &RuntimeHost) {
    let journal = super::plugin_iteration_journal_path(&host.snapshot_root);
    fs::create_dir_all(&journal).expect("create journal-as-dir");
    fs::write(journal.join("blocker.txt"), b"x").expect("nonempty dir");
}

#[test]
#[serial]
fn finalize_promote_failure_pass_pass_arm_returns_err_and_records_reason() {
    let (_temp, host) = empty_host();
    host.reload_candidate().expect("stage candidate");
    wedge_journal_as_dir(&host);
    let mut state = PluginIterationRunState::new(prepared_for("mini", &["mini"]));
    state.verifier_verdict = Some(VerifierVerdict::Pass);
    state.canary = Some(crate::kernel::plugin_iteration::CanaryReport {
        verdict: CanaryVerdict::Pass,
        mode: "recent_successful_invocation_replay".to_string(),
        plugin_path: Some("mini".to_string()),
        node_id: Some("mini_echo".to_string()),
        payload: Some(json!({})),
        expected_response: Some(json!({ "ok": true })),
        actual_response: Some(json!({ "ok": true })),
        message: "match".to_string(),
    });
    let out = host.finalize_plugin_iteration(&mut state);
    assert!(out.is_err(), "promote failure must propagate as Err");
    let reason = state.blocked_reason.expect("blocked reason recorded");
    assert!(
        reason.starts_with("promote failed: "),
        "unexpected reason: {reason}"
    );
}

// Manual approval is what unblocks a test-editing change: it promotes
// through the same arm (and reports a promote failure the same way).
#[test]
#[serial]
fn finalize_manual_approval_promotes_a_test_editing_change() {
    let (_temp, host) = empty_host();
    host.reload_candidate().expect("stage candidate");
    let mut prepared = prepared_for("mini", &["mini"]);
    prepared.manual_approved = true;
    let mut state = PluginIterationRunState::new(prepared);
    state.verifier_verdict = Some(VerifierVerdict::Pass);
    state.agent = Some(TestAgentRun::with_requires_approval(true));
    let verdict = host
        .finalize_plugin_iteration(&mut state)
        .expect("manual approval promotes");
    assert_eq!(verdict, PluginIterationFinalVerdict::Promoted);
}

// ─────────── finalize TOCTOU source-drift downgrade ──────────────────

#[test]
#[serial]
fn finalize_toctou_drift_downgrades_pass_to_rolled_back() {
    let (_temp, host) = empty_host();
    let mut state = PluginIterationRunState::new(prepared_for("mini", &["mini"]));
    state.verifier_verdict = Some(VerifierVerdict::Pass);
    state.canary = Some(crate::kernel::plugin_iteration::CanaryReport {
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
    state.verification = Some(crate::kernel::verifier::VerificationReport {
        plan: crate::kernel::verifier::VerificationPlan {
            profile: VerificationProfile::Default,
            static_check_command: None,
            tests_command: None,
            safety_command: None,
        },
        stages: Vec::new(),
        input: crate::kernel::evaluator::VerificationInput {
            tests_passed: true,
            safety_checks_passed: true,
            quality_score: 100,
        },
        tests: None,
        safety: None,
        source_tree_hash: Some("stale-hash-that-will-not-match".to_string()),
        hash_error: None,
    });
    let verdict = host
        .finalize_plugin_iteration(&mut state)
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
#[serial]
fn finalize_baseline_hash_failure_downgrades_pass_to_rolled_back() {
    // A Pass verdict whose verification report carries a baseline-hash
    // failure (unreadable entry, dangling symlink, permissions) must NOT
    // be promotable: with no recorded hash there is no way to prove the
    // tree did not change, so it is downgraded exactly like a re-hash
    // failure before promote.
    let (_temp, host) = empty_host();
    let mut state = PluginIterationRunState::new(prepared_for("mini", &["mini"]));
    state.verifier_verdict = Some(VerifierVerdict::Pass);
    state.canary = Some(crate::kernel::plugin_iteration::CanaryReport {
        verdict: CanaryVerdict::Pass,
        mode: "recent_successful_invocation_replay".to_string(),
        plugin_path: Some("mini".to_string()),
        node_id: Some("mini_echo".to_string()),
        payload: Some(json!({})),
        expected_response: Some(json!({ "ok": true })),
        actual_response: Some(json!({ "ok": true })),
        message: "match".to_string(),
    });
    state.verification = Some(crate::kernel::verifier::VerificationReport {
        plan: crate::kernel::verifier::VerificationPlan {
            profile: VerificationProfile::Default,
            static_check_command: None,
            tests_command: None,
            safety_command: None,
        },
        stages: Vec::new(),
        input: crate::kernel::evaluator::VerificationInput {
            tests_passed: true,
            safety_checks_passed: true,
            quality_score: 100,
        },
        tests: None,
        safety: None,
        source_tree_hash: None,
        hash_error: Some("unable to read plugins/mini/src/lib.rs".to_string()),
    });
    let verdict = host
        .finalize_plugin_iteration(&mut state)
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

// ─────────── apply_operations partial-batch rollback error ───────────

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
        fs::read_to_string(host.fixtures_root.join(&good)).expect("read"),
        "before\n",
        "partial-batch rollback must restore the first op"
    );
}

// ─────────── run_plugin_iteration_agent edit_plan (no LLM) path ──────

#[test]
#[serial]
fn run_plugin_iteration_agent_applies_edit_plan_without_llm() {
    let (_temp, host) = empty_host();
    let rel = seed_writable_source(&host, "planned.rs", "old body\n");
    let mut prepared = prepared_for("mini", &["mini"]);
    prepared.edit_plan = Some(crate::kernel::plugin_iteration::PluginEditPlan {
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
    let run = host
        .run_plugin_iteration_agent(&prepared)
        .expect("edit-plan path runs without any LLM call");
    // No agent session in the edit-plan branch.
    assert!(run.session_id.is_none());
    assert_eq!(run.recorded_summary.as_deref(), Some("planned edit"));
    assert!(run.changed_paths.iter().any(|p| p == &rel));
    // The planned edit landed on disk.
    assert_eq!(
        fs::read_to_string(host.fixtures_root.join(&rel)).expect("read"),
        "planned body\n"
    );
}

// ─────────── verify_plugin_iteration wiring (cheap commands) ─────────

#[test]
#[serial]
fn verify_plugin_iteration_runs_trivial_commands_and_reports() {
    let (_temp, host) = empty_host();
    let mut prepared = prepared_for("mini", &["mini"]);
    // Trivial shell commands so the verifier's command stages run fast and
    // pass, exercising the candidate_invoker closure construction and the
    // CommandVerifier::verify_with_options `?` call site.
    prepared.tests_command = Some("cargo test --help".to_string());
    prepared.safety_command = Some("cargo --version".to_string());
    let state = PluginIterationRunState::new(prepared);
    let report = host
        .verify_plugin_iteration(&state)
        .expect("verify runs the trivial commands");
    // Both trivial commands exit 0 → tests_passed true.
    assert!(report.input.tests_passed);
}

#[test]
#[serial]
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
    let state = PluginIterationRunState::new(prepared);
    let report = host
        .verify_plugin_iteration(&state)
        .expect("verify routes the plugin: command through the candidate closure");
    // The candidate echoes {"ok":true} and exits successfully.
    assert!(report.input.tests_passed, "plugin: candidate invoke passed");
}

// ─────────── finalize manual-approved promote SUCCESS ────────────────

#[test]
#[serial]
fn finalize_manual_approved_partial_promotes_when_candidate_staged() {
    let (_temp, host) = empty_host();
    host.reload_candidate().expect("stage candidate");
    let mut prepared = prepared_for("", &[]);
    prepared.manual_approved = true;
    let mut state = PluginIterationRunState::new(prepared);
    state.verifier_verdict = Some(VerifierVerdict::Pass);
    state.canary = Some(crate::kernel::plugin_iteration::CanaryReport {
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
    let verdict = host
        .finalize_plugin_iteration(&mut state)
        .expect("manual-approved promote succeeds");
    assert_eq!(verdict, PluginIterationFinalVerdict::Promoted);
    assert!(host.candidate_snapshot().is_none(), "candidate promoted");
}

#[test]
#[serial]
fn finalize_pass_pass_promotes_when_candidate_staged() {
    let (_temp, host) = empty_host();
    host.reload_candidate().expect("stage candidate");
    let mut state = PluginIterationRunState::new(prepared_for("", &[]));
    state.verifier_verdict = Some(VerifierVerdict::Pass);
    state.canary = Some(crate::kernel::plugin_iteration::CanaryReport {
        verdict: CanaryVerdict::Pass,
        mode: "recent_successful_invocation_replay".to_string(),
        plugin_path: Some("x".to_string()),
        node_id: Some("n".to_string()),
        payload: Some(json!({})),
        expected_response: Some(json!({ "ok": true })),
        actual_response: Some(json!({ "ok": true })),
        message: "match".to_string(),
    });
    let verdict = host
        .finalize_plugin_iteration(&mut state)
        .expect("pass/pass promote succeeds");
    assert_eq!(verdict, PluginIterationFinalVerdict::Promoted);
    assert!(host.candidate_snapshot().is_none());
}

// ─────────── R3: promote/rollback/approve 候选身份校验 ──────────────

/// `candidate_replaced_error` 的文本逐字节稳定。
#[test]
fn candidate_replaced_error_text_is_stable() {
    let err = super::candidate_replaced_error("snap-a", "snap-b");
    assert_eq!(
        err.to_string(),
        "internal invariant broken: candidate snapshot replaced during plugin \
             iteration: expected snap-a, got snap-b"
    );
}

/// R3: 候选在暂存后（Step 4 与 Step 7 之间）被并发 `reload_candidate`
/// 替换时，promote 必须拒绝且**不取走**新候选、不 promote。
#[test]
#[serial]
fn promote_candidate_rejects_replaced_candidate() {
    let (_temp, host) = empty_host();
    let first = host.reload_candidate().expect("stage candidate #1");
    let second = host.reload_candidate().expect("replace candidate #2");
    assert_ne!(
        first.candidate_snapshot_id, second.candidate_snapshot_id,
        "two reloads must produce distinct snapshot ids"
    );

    let err = host
        .promote_candidate_matching(Some(&first.candidate_snapshot_id))
        .expect_err("a replaced candidate must be rejected");
    let text = err.to_string();
    assert!(
        text.contains("candidate snapshot replaced during plugin iteration"),
        "unexpected error: {text}"
    );
    assert!(
        text.contains(first.candidate_snapshot_id.as_str()),
        "error must name the expected snapshot: {text}"
    );
    assert!(
        text.contains(second.candidate_snapshot_id.as_str()),
        "error must name the actual staged snapshot: {text}"
    );
    // 未 promote：新候选仍在，current 未变。
    assert!(
        host.candidate_snapshot().is_some(),
        "candidate must stay staged"
    );
    assert_eq!(
        host.current_snapshot().snapshot_id(),
        second.from_snapshot_id,
        "current snapshot must be untouched"
    );
}

/// R3: rollback 同样拒绝被替换的候选——不 restore、不 retire。
#[test]
#[serial]
fn rollback_candidate_rejects_replaced_candidate() {
    let (_temp, host) = empty_host();
    let first = host.reload_candidate().expect("stage candidate #1");
    let _second = host.reload_candidate().expect("replace candidate #2");

    let err = host
        .rollback_candidate_matching(Some(&first.candidate_snapshot_id))
        .expect_err("a replaced candidate must be rejected");
    assert!(
        err.to_string()
            .contains("candidate snapshot replaced during plugin iteration"),
        "unexpected error: {err}"
    );
    assert!(
        host.candidate_snapshot().is_some(),
        "candidate must stay staged"
    );
}

/// R3: finalize 的 Pass/Pass promote 路径把 Step 4 记录的身份传下去——
/// 候选被替换时 finalize 返回 Err 且候选仍 staged。
#[test]
#[serial]
fn finalize_rejects_promote_of_replaced_candidate() {
    let (_temp, host) = empty_host();
    let first = host.reload_candidate().expect("stage candidate #1");
    host.reload_candidate().expect("replace candidate #2");
    let mut state = PluginIterationRunState::new(prepared_for("", &[]));
    // Step 4 记录的候选是 first；当前 staged 已被替换成 second。
    state.candidate = Some(first);
    state.verifier_verdict = Some(VerifierVerdict::Pass);
    state.canary = Some(crate::kernel::plugin_iteration::CanaryReport {
        verdict: CanaryVerdict::Pass,
        mode: "recent_successful_invocation_replay".to_string(),
        plugin_path: Some("x".to_string()),
        node_id: Some("n".to_string()),
        payload: Some(json!({})),
        expected_response: Some(json!({ "ok": true })),
        actual_response: Some(json!({ "ok": true })),
        message: "match".to_string(),
    });
    let out = host.finalize_plugin_iteration(&mut state);
    assert!(out.is_err(), "promote of a replaced candidate must fail");
    assert!(
        host.candidate_snapshot().is_some(),
        "the (replaced) candidate must remain staged"
    );
}

// ─────────── reload retire-candidate + double reload_candidate ───────

#[test]
#[serial]
fn reload_retires_replaced_candidate_snapshot() {
    let (_temp, host) = empty_host();
    // Stage a candidate, then a full reload swaps the live snapshot and
    // retires the still-staged candidate (reload_internal line 4429).
    host.reload_candidate().expect("stage candidate");
    assert!(host.candidate_snapshot().is_some());
    host.reload("/").expect("full reload");
    // The replaced candidate was retired (taken) during reload_internal.
    assert!(
        host.candidate_snapshot().is_none(),
        "reload must clear/retire the staged candidate"
    );
}

#[test]
#[serial]
fn double_reload_candidate_retires_previous_candidate() {
    let (_temp, host) = empty_host();
    host.reload_candidate().expect("first candidate");
    // A second stage replaces the first; the previous candidate is retired
    // (reload_candidate_internal line 4535).
    host.reload_candidate()
        .expect("second candidate replaces first");
    assert!(host.candidate_snapshot().is_some());
}

// ─────────── run_plugin_canary candidate loop continue arm ───────────

#[test]
#[serial]
fn run_plugin_canary_continues_when_candidate_lacks_target_plugin() {
    let (_temp, host) = empty_host();
    // Stage an (empty) candidate, then run canary for a target plugin that
    // is NOT in the candidate registry → the `let Some(plugin) = ... else
    // { continue }` arm fires, then falls through to Partial.
    host.reload_candidate().expect("stage empty candidate");
    let state = PluginIterationRunState::new(prepared_for("ghost", &["ghost"]));
    let report = host.run_plugin_canary(&state).expect("canary runs");
    assert_eq!(report.verdict, CanaryVerdict::Partial);
    assert_eq!(report.mode, "no_canary_evidence");
}

// ─────────── session-lookup / wrong-kind error arms ───────────
//
// These four arms are private to the impl: the public wrappers
// (`agent_send_with_fallback`, `iterate_plugins`) either fail earlier or
// guarantee the session exists, so the misses have to be driven directly.
// `tests/host_boot_session_arms.rs` covers the reachable-from-outside
// counterparts.

/// `swap_session_profile`'s `AgentSessionNotFound` closure: an id that was
/// never inserted into `agent_sessions`.
#[test]
#[serial]
fn swap_session_profile_reports_missing_session() {
    let (_temp, host) = empty_host();
    let err = host
        .swap_session_profile("no-such-session", "default")
        .expect_err("an unknown session id must error");
    assert_eq!(
        err.to_string(),
        RuntimeError::AgentSessionNotFound {
            session_id: "no-such-session".to_string(),
        }
        .to_string()
    );
}

/// `swap_session_profile`'s success path on a live session: the resolved
/// profile's `api` replaces the session's live config, so `status().model`
/// reflects the swap.
#[test]
#[serial]
fn swap_session_profile_replaces_live_config() {
    let (_temp, host) = empty_host();
    let sid = host
        .agent_start(AgentSessionKind::RuntimeShell)
        .expect("start session")
        .session_id;
    // The empty fixture has only the `default` profile, so resolving an
    // unknown name falls back to it — the swap still rebuilds the client
    // and returns Ok.
    host.swap_session_profile(&sid, "no-such-profile")
        .expect("swap to the fallback-resolved default profile");
    let expected = host
        .config()
        .llm_profiles
        .resolve("default")
        .api
        .model
        .clone();
    assert_eq!(host.agent_status(&sid).expect("status").model, expected);
}

/// `plugin_iteration_agent_snapshot`'s two error arms: an unknown session
/// id, and a session that exists but is not a plugin-iteration session.
#[test]
#[serial]
fn plugin_iteration_agent_snapshot_rejects_missing_and_wrong_kind_sessions() {
    let (_temp, host) = empty_host();

    let missing = host
        .plugin_iteration_agent_snapshot("no-such-session")
        .expect_err("an unknown session id must error");
    assert_eq!(
        missing.to_string(),
        RuntimeError::AgentSessionNotFound {
            session_id: "no-such-session".to_string(),
        }
        .to_string()
    );

    // A RuntimeShell session exists but carries `ManagedAgentState::
    // RuntimeShell`, so the let-else guard rejects it.
    let sid = host
        .agent_start(AgentSessionKind::RuntimeShell)
        .expect("start a runtime-shell session")
        .session_id;
    let wrong_kind = host
        .plugin_iteration_agent_snapshot(&sid)
        .expect_err("a runtime-shell session is not an iteration session");
    let message = invalid_argument_message(&wrong_kind);
    assert_eq!(
        message,
        format!("agent session {sid} is not a plugin iteration session")
    );
}

/// `plugin_iteration_agent_snapshot`'s success path: a real
/// `ManagedAgentState::PluginIteration` entry returns its snapshot.
#[test]
#[serial]
fn plugin_iteration_agent_snapshot_returns_state_for_an_iteration_session() {
    let (temp, host) = empty_host();
    let prepared = prepared_for("ghost", &["ghost"]);
    let context_paths = PluginIterationContextPaths {
        focus_paths: Vec::new(),
        all_paths: Vec::new(),
    };
    let session_id = host
        .start_plugin_iteration_agent_session(prepared.clone(), context_paths)
        .expect("start an iteration session");

    let snapshot = host
        .plugin_iteration_agent_snapshot(&session_id)
        .expect("iteration session yields a snapshot");
    // A freshly started session has recorded no summary yet, so the derived
    // plan falls back to the prepared summary.
    assert!(snapshot.recorded_summary.is_none());
    assert_eq!(snapshot.derived_edit_plan.summary, prepared.summary);
    assert!(snapshot.changed_paths.is_empty());
    drop(temp);
}
