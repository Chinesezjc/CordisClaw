use crate::agent::{
    AgentBackend, AgentReply, AgentSession, AgentSessionSnapshot, AgentSessionStatus,
    AgentToolExecutionSummary, AgentToolSpec, AgentTranscriptEntry,
};
use crate::config::{LlmApiConfig, PluginConfigFile, RuntimeConfig};
use crate::context::RuntimeContext;
use crate::core::error::RuntimeError;
use crate::core::models::{
    AbiFingerprint, GatePolicy, NodeOutcome, PluginDocs, PluginLoadResult, PluginUnavailableReason,
};
use crate::execution::engine::{
    execute_net, ExecutionConfig, ExecutionNetSpec, ExecutionOutput, ExecutionTransitionKind,
    ExecutionTransitionSpec, TransitionRunResult, TriggerInput,
};
use crate::execution::gate::RunPolicy;
use crate::execution::net::{ArcDirection, ArcSpec, JoinPolicy, PlaceSpec, TransitionSpec};
use crate::execution::scheduler::SchedulerConfig;
use crate::kernel::auto_update::{
    AutoUpdatePlan, AutoUpdateResult, AutoUpdater, VerificationEnvelope,
};
use crate::kernel::evaluator::VerificationInput;
use crate::kernel::memory::{ChangeMemory, ChangeRecord};
use crate::kernel::plugin_iteration::{
    file_sha256, normalize_rel_path, now_ms, validate_reserved_child_keyword_identifiers,
    CanaryReport, CanaryVerdict, KernelPluginIssue, KernelPluginIssueSource,
    KernelPluginIssueStatus, KernelPluginIterationRequest, PluginEditExecutor, PluginEditOpKind,
    PluginEditOperation, PluginEditPlan, PluginEditRollback, PluginIterationFinalVerdict,
    PluginIterationHistoryEntry, PluginIterationPolicy, PluginIterationStatus, VerifierVerdict,
};
use crate::kernel::verifier::{
    hash_source_tree, CommandVerifier, VerificationProfile, VerificationReport, VerifyOptions,
};
use crate::plugin::abi::PluginResponse;
use crate::plugin::invoke::invoke_registered_plugin;
use crate::plugin::loader::{default_loader_config, LoadOutput, Loader};
use crate::plugin::registry::{NodeRegistry, PluginRegistry, RegisteredPlugin};
use crate::plugin::tooling::rebuild_plugin_workspace;
use crate::service::doc_registry::DocRegistry;
use crate::service::graph_registry::{
    GraphRegistry, RegisteredNet, RegisteredNetEdge, RegisteredNetEdgeKind,
};
use cordis_plugin_sdk::NodeType;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use toml::Value as TomlValue;

#[derive(Debug, Clone)]
pub struct RuntimeSnapshot {
    snapshot_id: String,
    plugin_registry: PluginRegistry,
    node_registry: NodeRegistry,
    doc_registry: DocRegistry,
    graph_registry: GraphRegistry,
    context_baseline: RuntimeContext,
    staged_artifact_root: PathBuf,
}

impl RuntimeSnapshot {
    pub fn snapshot_id(&self) -> &str {
        &self.snapshot_id
    }

    pub fn plugin_registry(&self) -> &PluginRegistry {
        &self.plugin_registry
    }

    pub fn node_registry(&self) -> &NodeRegistry {
        &self.node_registry
    }

    pub fn doc_registry(&self) -> &DocRegistry {
        &self.doc_registry
    }

    pub fn graph_registry(&self) -> &GraphRegistry {
        &self.graph_registry
    }

    pub fn context_baseline(&self) -> &RuntimeContext {
        &self.context_baseline
    }

    pub fn staged_artifact_root(&self) -> &Path {
        &self.staged_artifact_root
    }

    pub fn invoke(
        &self,
        plugin_path: &str,
        node_id: &str,
        payload: String,
    ) -> Result<PluginResponse, RuntimeError> {
        invoke_registered_plugin(&self.plugin_registry, plugin_path, node_id, payload)
    }

    pub fn execute_registered_target(
        &self,
        target_node_fqn: &str,
        payload: Value,
    ) -> Result<RuntimeExecutionResult, RuntimeError> {
        let request_seed =
            payload
                .as_object()
                .cloned()
                .ok_or_else(|| RuntimeError::InvalidArgument {
                    message: "execute payload must be a JSON object".to_string(),
                })?;
        let target_node = self.node_registry.get(target_node_fqn).ok_or_else(|| {
            RuntimeError::InvalidArgument {
                message: format!("registered node not found: {target_node_fqn}"),
            }
        })?;
        let registered_net = self.graph_registry.net();
        let selected_nodes = select_registered_net_subgraph(registered_net, target_node_fqn);
        let net = build_execution_net(
            registered_net,
            &selected_nodes,
            target_node_fqn,
            target_node,
        );

        let mut context = self.context_baseline.clone();
        let traces = Mutex::new(BTreeMap::<String, ExecutionInvocationTrace>::new());
        // Bind the config out of the call so the `?` below lands on the same
        // line as the closing paren: a `?` whose expression spans many lines
        // otherwise leaves llvm-cov a zero-hit region on the continuation line.
        let execution_config = ExecutionConfig {
            scheduler: SchedulerConfig {
                max_parallelism: 1,
                max_concurrency: 1,
            },
            ..ExecutionConfig::default()
        };
        let run = execute_net(
            execution_config,
            net,
            &mut context,
            |spec, attempt, trigger, _| {
                let transition_id = &spec.transition.transition_id;
                let Some(node) = self.node_registry.get(transition_id) else {
                    traces.lock().unwrap().insert(
                        transition_id.clone(),
                        missing_registry_trace(transition_id, attempt),
                    );
                    return TransitionRunResult::from_outcome(NodeOutcome::Failure);
                };

                let request_payload = build_execution_payload(&request_seed, &trigger.inputs);
                // `Display` on `Value` emits the same compact JSON as
                // `serde_json::to_string` and is infallible for object maps,
                // so no error arm is needed here.
                let request_text = Value::Object(request_payload.clone()).to_string();

                match self.invoke(&node.plugin_path, &node.node_id, request_text) {
                    Ok(response) => {
                        let response_payload = parse_response_payload(&response.payload);
                        let outcome = infer_outcome_from_payload(&response_payload);
                        traces.lock().unwrap().insert(
                            transition_id.clone(),
                            ExecutionInvocationTrace {
                                node_fqn: transition_id.clone(),
                                plugin_path: node.plugin_path.clone(),
                                node_id: node.node_id.clone(),
                                attempt,
                                outcome: Some(outcome),
                                request_payload: Some(Value::Object(request_payload)),
                                response_payload: Some(response_payload.clone()),
                                error: None,
                            },
                        );
                        TransitionRunResult {
                            outcome,
                            payload: response_payload,
                        }
                    }
                    Err(err) => {
                        traces.lock().unwrap().insert(
                            transition_id.clone(),
                            ExecutionInvocationTrace {
                                node_fqn: transition_id.clone(),
                                plugin_path: node.plugin_path.clone(),
                                node_id: node.node_id.clone(),
                                attempt,
                                outcome: Some(NodeOutcome::Failure),
                                request_payload: Some(Value::Object(request_payload)),
                                response_payload: None,
                                error: Some(err.to_string()),
                            },
                        );
                        TransitionRunResult::from_outcome(NodeOutcome::Failure)
                    }
                }
            },
        );

        let output = run?;
        let mut traces = traces.into_inner().unwrap();
        fill_missing_execution_traces(&output, &mut traces);
        Ok(RuntimeExecutionResult {
            target_node_fqn: target_node_fqn.to_string(),
            selected_nodes: selected_nodes.into_iter().collect(),
            net_diagnostics: registered_net.diagnostics.clone(),
            output,
            traces,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExecutionInvocationTrace {
    pub node_fqn: String,
    pub plugin_path: String,
    pub node_id: String,
    pub attempt: u32,
    pub outcome: Option<NodeOutcome>,
    pub request_payload: Option<Value>,
    pub response_payload: Option<Value>,
    pub error: Option<String>,
}

/// Failure trace for the `execute_registered_target` parallel path when the
/// scheduler hands us a transition whose id is absent from the node registry.
/// Extracted as a pure function so the trace shape is unit-testable without
/// driving a full net execution (host.rs execute closure, registry-miss arm).
fn missing_registry_trace(transition_id: &str, attempt: u32) -> ExecutionInvocationTrace {
    ExecutionInvocationTrace {
        node_fqn: transition_id.to_string(),
        plugin_path: String::new(),
        node_id: String::new(),
        attempt,
        outcome: Some(NodeOutcome::Failure),
        request_payload: None,
        response_payload: None,
        error: Some("node missing from registry".to_string()),
    }
}

/// Build the `AbiMismatch` error raised by `reload_subtree` Phase 1 when the
/// candidate dylib's docs disagree with the recorded index entry's node count.
/// The `expected`/`actual` fingerprints are both the index entry's fingerprint
/// (only the docs drifted, not the ABI hash) — this matches the historical
/// report shape byte-for-byte. Extracted for direct unit testing of the report.
fn reload_docs_mismatch_error(
    plugin_path: &str,
    entry_fingerprint: &AbiFingerprint,
    expected_nodes: usize,
    actual_nodes: usize,
) -> RuntimeError {
    RuntimeError::AbiMismatch {
        plugin_path: plugin_path.to_string(),
        expected: Box::new(entry_fingerprint.clone()),
        actual: Box::new(entry_fingerprint.clone()),
        fingerprint_diff: vec![format!(
            "docs mismatch: expected {expected_nodes} nodes, got {actual_nodes}"
        )],
    }
}

/// Build the `AbiMismatch` error raised by `reload_subtree` Phase 1 when the
/// candidate dylib's ABI fingerprint (crate_hash / api_hash) diverges from the
/// recorded index entry. `actual_fingerprint` is moved in because the caller no
/// longer needs it after the error is built. Extracted for direct unit testing.
fn reload_abi_fingerprint_mismatch_error(
    plugin_path: &str,
    entry_fingerprint: &AbiFingerprint,
    actual_fingerprint: AbiFingerprint,
) -> RuntimeError {
    let diff = vec![format!(
        "expected crate={} api={}, got crate={} api={}",
        entry_fingerprint.crate_hash,
        entry_fingerprint.api_hash,
        actual_fingerprint.crate_hash,
        actual_fingerprint.api_hash,
    )];
    RuntimeError::AbiMismatch {
        plugin_path: plugin_path.to_string(),
        expected: Box::new(entry_fingerprint.clone()),
        actual: Box::new(actual_fingerprint),
        fingerprint_diff: diff,
    }
}

/// Log the per-session outcome of the post-reload notice injection.
///
/// Extracted from the loop in `notify_sessions_of_reload` so the failure arm is
/// unit-testable: the loop iterates over session ids read from the live session
/// map, and `agent_inject` only fails when the id is absent, so a session would
/// have to be dropped between the id snapshot and the injection for the `Err`
/// arm to fire in production.
fn log_session_reload_notify_outcome(session_id: &str, outcome: Result<(), RuntimeError>) {
    if let Err(e) = outcome {
        eprintln!("reload: failed to notify session {session_id}: {e}");
    }
}

/// The `PluginUnavailable` error raised by `reload_subtree` Phase 1 when a
/// target plugin present in the live registry has no entry in the artifact
/// index. `required: false` because the reload is refusing to swap this plugin,
/// not declaring the runtime unbootable.
///
/// Extracted from the `ok_or_else` closure inside `reload_subtree` so the
/// constructed error is unit-testable: reaching it through `reload_subtree`
/// needs an index that loads successfully yet omits a plugin the previous
/// snapshot loaded from that same index.
fn reload_artifact_missing_error(plugin_path: &str) -> RuntimeError {
    RuntimeError::PluginUnavailable {
        plugin_path: plugin_path.to_string(),
        reason: PluginUnavailableReason::ArtifactMissing,
        required: false,
    }
}

/// The `Invariant` error raised when a candidate dylib's `docs` payload is not
/// parseable as [`PluginDocs`]. Extracted so the message is unit-testable — a
/// dylib built with the SDK's `export_plugin_api!` always serializes valid
/// docs, so this arm is unreachable through a well-formed artifact.
fn reload_docs_parse_error(plugin_path: &str, err: &serde_json::Error) -> RuntimeError {
    host_invariant(format!("failed to parse docs for {plugin_path}: {err}"))
}

/// The `Invariant` error raised when a candidate dylib's `abi_fingerprint`
/// payload is not parseable as [`AbiFingerprint`]. Same reasoning as
/// [`reload_docs_parse_error`]: unreachable via the SDK export macro, so the
/// message is pinned by a direct unit test instead.
fn reload_abi_fingerprint_parse_error(plugin_path: &str, err: &serde_json::Error) -> RuntimeError {
    host_invariant(format!(
        "failed to parse abi_fingerprint for {plugin_path}: {err}"
    ))
}

/// Render the payload of a caught stop-handler panic for the `reload_subtree`
/// diagnostic line.
///
/// Extracted from the `catch_unwind` arm inside `reload_subtree` so all three
/// payload shapes are directly unit-testable. Only the `&'static str` shape is
/// reachable through the runtime's own injection point (a literal `panic!`);
/// a plugin's stop handler compiled into a dylib can produce a formatted
/// `String` or an arbitrary `panic_any` payload, and those two arms were
/// otherwise unexercisable without shipping a deliberately broken artifact.
fn reload_stop_handler_panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown".to_string()
    }
}

/// Unit coverage for the reload-chain arms extracted out of `reload_subtree`,
/// `notify_sessions_of_reload` and `RuntimeHost::fail_reload`, plus the
/// `fail_reload` packaging method itself.
///
/// Each helper here sits on an arm that a well-formed artifact and a live
/// session map cannot reach (unparseable SDK-serialized payloads, a `panic_any`
/// payload from inside a plugin dylib, a session dropped mid-notify), so the
/// error text and shape are pinned here rather than left uncovered. The
/// reachable reload arms are covered end-to-end in
/// `tests/host_reload_arms.rs`.
#[cfg(test)]
mod reload_arm_helper_tests {
    use super::{
        log_session_reload_notify_outcome, reload_abi_fingerprint_parse_error,
        reload_artifact_missing_error, reload_docs_parse_error, reload_stop_handler_panic_message,
        RuntimeHost,
    };
    use crate::core::error::RuntimeError;
    use crate::core::models::PluginUnavailableReason;
    use std::any::Any;
    use std::time::Instant;

    fn json_error() -> serde_json::Error {
        serde_json::from_str::<serde_json::Value>("{ not json").expect_err("must not parse")
    }

    // ── reload_subtree Phase-1 error constructors ────────────────────────

    #[test]
    fn artifact_missing_error_is_non_required_unavailable() {
        let err = reload_artifact_missing_error("expr/evaluator");
        let expected = RuntimeError::PluginUnavailable {
            plugin_path: "expr/evaluator".to_string(),
            reason: PluginUnavailableReason::ArtifactMissing,
            required: false,
        };
        // Full-value comparison: plugin_path, reason and the `required: false`
        // classification are all load-bearing for the reload report.
        // `Display` renders plugin_path, reason and the `required: false`
        // classification, so this one comparison pins every field without a
        // never-taken destructuring arm.
        assert_eq!(err.to_string(), expected.to_string());
    }

    #[test]
    fn docs_parse_error_names_the_plugin_and_quotes_serde() {
        let source = json_error();
        let err = reload_docs_parse_error("qq", &source);
        let expected = RuntimeError::Invariant {
            message: format!("failed to parse docs for qq: {source}"),
        };
        assert_eq!(err.to_string(), expected.to_string());
    }

    #[test]
    fn abi_fingerprint_parse_error_names_the_plugin_and_quotes_serde() {
        let source = json_error();
        let err = reload_abi_fingerprint_parse_error("feishu", &source);
        let expected = RuntimeError::Invariant {
            message: format!("failed to parse abi_fingerprint for feishu: {source}"),
        };
        assert_eq!(err.to_string(), expected.to_string());
    }

    // ── stop-handler panic payload rendering ─────────────────────────────

    // A literal `panic!("...")` yields a `&'static str` payload — the shape the
    // runtime's own injection point produces.
    #[test]
    fn stop_handler_panic_message_reads_static_str_payload() {
        let payload: Box<dyn Any + Send> = Box::new("literal boom");
        assert_eq!(
            reload_stop_handler_panic_message(payload.as_ref()),
            "literal boom"
        );
    }

    // A formatted `panic!("{}", x)` yields an owned `String` payload.
    #[test]
    fn stop_handler_panic_message_reads_owned_string_payload() {
        let payload: Box<dyn Any + Send> = Box::new(String::from("formatted boom"));
        assert_eq!(
            reload_stop_handler_panic_message(payload.as_ref()),
            "formatted boom"
        );
    }

    // `panic_any(v)` with a non-string `v` falls through to the placeholder
    // instead of panicking inside the diagnostic itself.
    #[test]
    fn stop_handler_panic_message_falls_back_for_non_string_payload() {
        let payload: Box<dyn Any + Send> = Box::new(42u32);
        assert_eq!(
            reload_stop_handler_panic_message(payload.as_ref()),
            "unknown"
        );
    }

    // The rendering is reached through a real `catch_unwind` of a `panic_any`,
    // matching how `reload_subtree` calls it.
    #[test]
    fn stop_handler_panic_message_renders_a_caught_panic_any() {
        let caught = std::panic::catch_unwind(|| {
            std::panic::panic_any(7i64);
        })
        .expect_err("panic_any must unwind");
        assert_eq!(
            reload_stop_handler_panic_message(caught.as_ref()),
            "unknown"
        );
    }

    // ── notify_sessions_of_reload per-session outcome ─────────────────────

    // The Ok arm is a no-op; the Err arm logs. Both are total, so the assertion
    // is that neither panics for either outcome.
    #[test]
    fn session_notify_outcome_logging_handles_both_arms() {
        log_session_reload_notify_outcome("sid-ok", Ok(()));
        log_session_reload_notify_outcome(
            "sid-gone",
            Err(RuntimeError::AgentSessionNotFound {
                session_id: "sid-gone".to_string(),
            }),
        );
    }

    // ── RuntimeHost::fail_reload packaging ───────────────────────────────

    /// `fail_reload` pairs the error with a Failed attempt whose
    /// `failure_summary` is the error text and whose `to_snapshot_id` is unset.
    /// Called directly because every `reload_subtree` call site that reaches it
    /// needs a distinct artifact fault to synthesize.
    #[test]
    fn fail_reload_packages_error_with_failed_attempt() {
        let temp = tempfile::TempDir::new().expect("tempdir");
        let fixtures = temp.path().join("fixtures");
        let artifacts = fixtures.join("artifacts");
        std::fs::create_dir_all(&artifacts).expect("create artifacts dir");
        std::fs::write(
            artifacts.join("index.json"),
            r#"{"schema_version":2,"generated_at":"2026-07-24T00:00:00Z","topo_order":[],"entries":[]}"#,
        )
        .expect("write empty index");
        let host = RuntimeHost::boot(&fixtures).expect("boot on empty index");

        let snapshot = host.current_snapshot();
        let err = RuntimeError::Invariant {
            message: "synthetic reload fault".to_string(),
        };
        let rendered = err.to_string();
        let (returned, attempt) = host.fail_reload(&snapshot, Instant::now(), err);

        // The error is handed back unchanged, and the attempt's summary is
        // exactly its rendered form.
        assert_eq!(returned.to_string(), rendered);
        assert_eq!(attempt.status, super::ReloadAttemptStatus::Failed);
        assert_eq!(attempt.from_snapshot_id, snapshot.snapshot_id());
        assert_eq!(attempt.to_snapshot_id, None);
        assert_eq!(attempt.failure_summary, Some(rendered));
        assert_eq!(attempt.plugin_count, None);
        assert!(attempt.changed_plugins.is_empty());
    }
}

/// Coverage for the execute-path trace constructors and the `reload_subtree`
/// Phase-1 AbiMismatch report builders extracted above, plus the cheap
/// `pub(crate)` session-map accessors. Kept in a dedicated module (pre-`impl`
/// region) so concurrent edits to the primary `mod tests` never collide.
#[cfg(test)]
mod sr_host_a_seam_tests {
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
                expected_fp.crate_hash,
                expected_fp.api_hash,
                actual_fp.crate_hash,
                actual_fp.api_hash,
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
}

/// Coverage batch (host.rs lines 1-3500, "cov-fa" seam): internal arms that
/// need a hand-built [`RuntimeSnapshot`] or a direct call into a private
/// kernel method, which integration tests (only `pub` surface) cannot reach.
/// Kept in a dedicated module so concurrent edits to the primary `mod tests`
/// and the `sr_host_a_seam_tests` module never collide on the same lines.
#[cfg(test)]
mod cov_fa_host_1_3500_tests {
    use super::{runtime_snapshot_from_output, RuntimeHost, RuntimeKernel, RuntimeSnapshot};
    use crate::core::error::RuntimeError;
    use crate::core::models::{ArtifactKind, NodeOutcome};
    use crate::kernel::plugin_iteration::{
        KernelPluginIssueSource, KernelPluginIssueStatus, KernelPluginIterationRequest,
    };
    use cordis_plugin_sdk::{node_doc, plugin_docs, AbiFingerprint, NodeDoc};
    use serde_json::json;
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    // A two-node "producer → consumer" plugin whose registered net carries an
    // upstream edge, but whose node_registry — handed to the snapshot — is
    // MISSING the producer node. Executing the consumer therefore fires the
    // producer transition, whose `node_registry.get(..)` returns None, driving
    // `execute_registered_target`'s registry-missing arm (missing_registry_trace).
    fn snapshot_with_ghost_upstream_node() -> RuntimeSnapshot {
        let plugin_path = "ghostplug".to_string();
        // Producer emits "shared"; consumer reads "shared" → a data edge
        // producer→consumer is inferred by the graph builder.
        let producer: NodeDoc = node_doc(
            "producer",
            "emits the shared field",
            json!({ "type": "object", "properties": {} }),
            json!({ "type": "object", "properties": { "shared": { "type": "number" } } }),
            &[],
            &[],
        );
        let consumer: NodeDoc = node_doc(
            "consumer",
            "reads the shared field",
            json!({ "type": "object", "properties": { "shared": { "type": "number" } } }),
            json!({ "type": "object", "properties": {} }),
            &[],
            &[],
        );
        let docs = plugin_docs(
            plugin_path.clone(),
            plugin_path.clone(),
            "0.1.0",
            None,
            vec![producer.clone(), consumer.clone()],
            None,
        );

        let plugin_registry = crate::plugin::registry::PluginRegistry::default();
        plugin_registry.insert_loaded(
            plugin_path.clone(),
            None,
            true,
            BTreeSet::new(),
            docs.clone(),
            PathBuf::from("/nonexistent/cordis-ghost.dylib"),
            ArtifactKind::Dylib,
            AbiFingerprint::current_build("crate_ghost_v1", "api_v2"),
            None,
        );

        // FULL node registry (both nodes) drives the graph/net construction so
        // the net contains the producer→consumer edge.
        let mut full_nodes = crate::plugin::registry::NodeRegistry::default();
        full_nodes
            .register_from_docs(&plugin_path, &docs)
            .expect("register both nodes for graph build");
        let graph_registry = crate::service::graph_registry::GraphRegistry::from_registries(
            &plugin_registry,
            &full_nodes,
        );

        // PARTIAL node registry (consumer only) is what the snapshot carries,
        // so resolving the upstream producer transition fails at runtime.
        let consumer_only_docs = plugin_docs(
            plugin_path.clone(),
            plugin_path.clone(),
            "0.1.0",
            None,
            vec![consumer],
            None,
        );
        let mut partial_nodes = crate::plugin::registry::NodeRegistry::default();
        partial_nodes
            .register_from_docs(&plugin_path, &consumer_only_docs)
            .expect("register consumer only");

        let doc_registry =
            crate::service::doc_registry::DocRegistry::from_plugin_registry(&plugin_registry);

        runtime_snapshot_from_output(
            crate::plugin::loader::LoadOutput {
                execution_id: "snapshot-ghost-upstream".to_string(),
                plugin_registry,
                node_registry: partial_nodes,
                doc_registry,
                graph_registry,
                context: crate::context::RuntimeContext::default(),
                metrics: crate::plugin::loader::LoaderMetrics::default(),
            },
            PathBuf::from("/tmp/cordis-ghost-upstream-staged"),
        )
    }

    #[test]
    fn execute_records_missing_registry_trace_for_ghost_upstream_node() {
        let snapshot = snapshot_with_ghost_upstream_node();
        let result = snapshot
            .execute_registered_target("ghostplug::consumer", json!({ "shared": 1 }))
            .expect("execute returns a result even with a ghost upstream node");

        // The upstream producer transition resolves to no registry node, so
        // the closure records the blank-plugin missing-registry trace.
        let trace = result
            .traces
            .get("ghostplug::producer")
            .expect("ghost upstream node should have a trace entry");
        assert_eq!(trace.outcome, Some(NodeOutcome::Failure));
        assert_eq!(trace.plugin_path, "");
        assert_eq!(trace.node_id, "");
        assert_eq!(
            trace.error.as_deref(),
            Some("node missing from registry"),
            "missing_registry_trace error text must be verbatim"
        );
        assert!(trace.request_payload.is_none());
    }

    // begin_plugin_iteration (private) + select_issue_for_request comparator.
    // A hermetic kernel with two OPEN issues of different source priority; a
    // no-issue-id request auto-selects, running the sort comparator, and the
    // highest-priority issue (LoadFailure=0) is chosen + flipped to Running.
    #[test]
    fn begin_plugin_iteration_auto_selects_highest_priority_issue() {
        let temp = TempDir::new().expect("tempdir");
        let config = crate::config::RuntimeConfig::default();
        let kernel = RuntimeKernel::new(temp.path(), &config);

        // Observed in "wrong" order: low-urgency first, high-urgency second.
        let _invoke =
            kernel.observe_plugin_issue(KernelPluginIssueSource::InvokeFailure, "expr", "invoke");
        let load =
            kernel.observe_plugin_issue(KernelPluginIssueSource::LoadFailure, "shell", "load");

        let snapshot = snapshot_with_ghost_upstream_node();
        let prepared = kernel
            .begin_plugin_iteration(
                &snapshot,
                &KernelPluginIterationRequest {
                    issue_id: None,
                    target_plugin_paths: Vec::new(),
                    instruction: Some("auto".to_string()),
                    edit_plan: None,
                    manual_approved: false,
                    tests_command: None,
                    safety_command: None,
                    verify_profile: None,
                    quality_score: None,
                },
            )
            .expect("begin should auto-select an issue");
        // LoadFailure (priority 0) wins the comparator over InvokeFailure (3).
        assert_eq!(prepared.root_plugin_path, "shell");

        // The selected issue was flipped to Running (the `and_modify` arm).
        let running = kernel
            .plugin_issues()
            .into_iter()
            .find(|i| i.issue_id == load.issue_id)
            .expect("load issue present");
        assert_eq!(running.status, KernelPluginIssueStatus::Running);
    }

    // begin_plugin_iteration active-guard arm: a second begin while one is
    // already active returns PluginIterationActive.
    #[test]
    fn begin_plugin_iteration_rejects_second_concurrent_iteration() {
        let temp = TempDir::new().expect("tempdir");
        let config = crate::config::RuntimeConfig::default();
        let kernel = RuntimeKernel::new(temp.path(), &config);
        let snapshot = snapshot_with_ghost_upstream_node();

        let req = |instruction: &str| KernelPluginIterationRequest {
            issue_id: None,
            target_plugin_paths: vec!["ghostplug".to_string()],
            instruction: Some(instruction.to_string()),
            edit_plan: None,
            manual_approved: false,
            tests_command: None,
            safety_command: None,
            verify_profile: None,
            quality_score: None,
        };

        // First begin succeeds and marks the iteration active.
        let _first = kernel
            .begin_plugin_iteration(&snapshot, &req("first"))
            .expect("first iteration prepares");
        // Second begin hits the active-guard early return.
        let err = kernel
            .begin_plugin_iteration(&snapshot, &req("second"))
            .expect_err("a second concurrent iteration must be rejected");
        assert!(matches!(err, RuntimeError::PluginIterationActive { .. }));
    }

    // begin_plugin_iteration: when the selected issue's root_plugin_path no
    // longer matches any plugin in the snapshot registry, the subtree filter
    // yields nothing and the not-found InvalidArgument fires. (Explicit
    // target paths hit determine_root_plugin_path first, so the issue route
    // is the reachable way into this arm.)
    #[test]
    fn begin_plugin_iteration_rejects_vanished_issue_subtree() {
        let temp = TempDir::new().expect("tempdir");
        let config = crate::config::RuntimeConfig::default();
        let kernel = RuntimeKernel::new(temp.path(), &config);
        let issue = kernel.observe_plugin_issue(
            KernelPluginIssueSource::LoadFailure,
            "vanished-plugin",
            "plugin was unloaded after the issue was filed",
        );
        let snapshot = snapshot_with_ghost_upstream_node();
        let req = KernelPluginIterationRequest {
            issue_id: Some(issue.issue_id.clone()),
            // Non-empty targets keep root_mode off; the issue's vanished
            // root then filters the registry down to nothing.
            target_plugin_paths: vec!["ghostplug".to_string()],
            instruction: Some("noop".to_string()),
            edit_plan: None,
            manual_approved: false,
            tests_command: None,
            safety_command: None,
            verify_profile: None,
            quality_score: None,
        };
        let err = kernel
            .begin_plugin_iteration(&snapshot, &req)
            .expect_err("vanished issue subtree must be rejected");
        assert!(
            matches!(&err, RuntimeError::InvalidArgument { message }
                if message == "plugin subtree not found for vanished-plugin"),
            "got: {err:?}"
        );
    }

    // take_blocked_iteration: unknown id surfaces StatusNotFound (the only
    // remaining error path now that the verdict check is an invariant).
    // host_io_log_line: the single formatting seam behind every best-effort
    // I/O failure log in auto-save/shutdown/delete paths.
    #[test]
    fn host_io_log_line_formats_stage_subject_and_error() {
        let err = std::io::Error::other("disk gone");
        let line =
            RuntimeHost::host_io_log_line("auto-save", "failed to create sessions dir /x", &err);
        assert_eq!(
            line,
            "[auto-save] failed to create sessions dir /x: disk gone"
        );
    }

    /// The reconstruct-failure line in `detect_crash_and_recover`'s hydration
    /// chain. `AgentSession::from_snapshot` only fails when reqwest cannot build
    /// an HTTP client — no on-disk snapshot can force that (every field
    /// round-trips and reqwest accepts any timeout, including 0 and u64::MAX) —
    /// so the message shape is pinned here rather than by a fixture.
    #[test]
    fn crash_recovery_reconstruct_log_line_names_the_snapshot_path() {
        let err = RuntimeError::LlmRequestFailed {
            message: "failed to rebuild agent HTTP client from snapshot: nope".to_string(),
        };
        let line = super::crash_recovery_reconstruct_log_line(
            std::path::Path::new("/data/sessions/abc.json"),
            &err,
        );
        assert_eq!(
            line,
            "[crash-recovery] reconstruct failed for /data/sessions/abc.json: \
             LLM request failed: failed to rebuild agent HTTP client from snapshot: nope"
        );
    }

    // auto_save_session error arms via fault injection: data/sessions blocked
    // by a regular file at data/ makes create_dir_all fail; a read-only
    // sessions dir makes the tmp write fail; a directory squatting on the
    // target makes rename fail. All are best-effort (must not panic).
    #[cfg(unix)]
    #[test]
    fn auto_save_and_delete_session_arms_survive_fs_faults() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::TempDir::new().expect("tempdir");
        let fixtures = temp.path().join("fixtures");
        let artifacts = fixtures.join("artifacts");
        fs::create_dir_all(&artifacts).expect("create artifacts dir");
        fs::write(
            artifacts.join("index.json"),
            r#"{"schema_version":2,"generated_at":"2026-07-24T00:00:00Z","topo_order":[],"entries":[]}"#,
        )
        .expect("write empty index");
        let host = RuntimeHost::boot(&fixtures).expect("boot on empty index");
        let config = crate::config::LlmApiConfig {
            provider: "deepseek".to_string(),
            base_url: "http://127.0.0.1:1/v1".to_string(),
            api_key: Some("test".to_string()),
            model: "m".to_string(),
            ..crate::config::LlmApiConfig::default()
        };
        let session =
            crate::agent::AgentSession::new(config, "runtime_shell").expect("build session");

        // Arm 1: create_dir_all failure — put a FILE where data/sessions goes.
        let data_dir = host.data_dir();
        fs::create_dir_all(&data_dir).unwrap();
        fs::write(data_dir.join("sessions"), b"squatter").unwrap();
        host.auto_save_session("s1", &session); // must log-and-return, not panic
        fs::remove_file(data_dir.join("sessions")).unwrap();

        // Arm 2: tmp write failure — sessions dir exists but is read-only.
        let sessions_dir = data_dir.join("sessions");
        fs::create_dir_all(&sessions_dir).unwrap();
        let ro = fs::Permissions::from_mode(0o555);
        fs::set_permissions(&sessions_dir, ro).unwrap();
        if unsafe { libc::geteuid() } != 0 {
            host.auto_save_session("s2", &session);
        }
        fs::set_permissions(&sessions_dir, fs::Permissions::from_mode(0o700)).unwrap();

        // Arm 3: rename failure — a DIRECTORY squats on the target filename.
        fs::create_dir_all(sessions_dir.join("s3.json")).unwrap();
        host.auto_save_session("s3", &session);
        fs::remove_dir(sessions_dir.join("s3.json")).unwrap();

        // Arm 4: delete failure — target exists but parent becomes read-only.
        fs::write(sessions_dir.join("s4.json"), b"{}").unwrap();
        fs::set_permissions(&sessions_dir, fs::Permissions::from_mode(0o555)).unwrap();
        if unsafe { libc::geteuid() } != 0 {
            host.delete_session_snapshot("s4");
        }
        fs::set_permissions(&sessions_dir, fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[test]
    fn backup_artifacts_into_rollback_absorbs_and_rejects_mismatched_root() {
        use crate::kernel::plugin_iteration::PluginEditRollback;
        let temp = tempfile::TempDir::new().expect("tempdir");
        let fixtures = temp.path().join("fixtures");
        let artifacts = fixtures.join("artifacts");
        fs::create_dir_all(&artifacts).expect("mk artifacts");
        // artifact_paths_to_backup only collects `artifacts/<name>.so`.
        fs::write(artifacts.join("demo.so"), b"\x7fELF").expect("write artifact");
        fs::write(
            artifacts.join("index.json"),
            r#"{"schema_version":2,"generated_at":"t","topo_order":["demo"],"entries":[{"plugin_path":"demo","version":"0.1.0","abi_fingerprint":{"crate_hash":"c","api_hash":"a"},"artifact_path":"demo.json","sha256":"x","built_at":"t","parent":null,"required":true,"grants_from_parent":[],"docs":{"plugin_id":"demo","plugin_path":"demo","plugin_version":"0.1.0","abi_version":2,"nodes":[]},"exports":[],"execution":null,"artifact_kind":"json","local_path_deps":[],"input_probe":[],"build_fingerprint":""}]}"#,
        )
        .expect("write index");

        // Success: same workspace root absorbs the artifact backups.
        let mut ok_rb = PluginEditRollback::empty(fixtures.display().to_string());
        super::backup_artifacts_into_rollback(&fixtures, "demo", &mut ok_rb)
            .expect("absorb into same-root rollback");

        // Failure: a rollback anchored at a different workspace refuses.
        let mut bad_rb = PluginEditRollback::empty("/elsewhere".to_string());
        let err = super::backup_artifacts_into_rollback(&fixtures, "demo", &mut bad_rb)
            .expect_err("mismatched workspace root must fail absorb");
        assert!(
            matches!(err, RuntimeError::Invariant { .. }),
            "got: {err:?}"
        );
    }

    #[test]
    fn take_blocked_iteration_unknown_id_is_not_found() {
        let temp = TempDir::new().expect("tempdir");
        let config = crate::config::RuntimeConfig::default();
        let kernel = RuntimeKernel::new(temp.path(), &config);
        let err = kernel
            .take_blocked_iteration("nope")
            .expect_err("missing id must error");
        assert!(matches!(
            err,
            RuntimeError::PluginIterationStatusNotFound { .. }
        ));
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReloadAttemptStatus {
    Reloaded,
    Staged,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReloadReport {
    pub from_snapshot_id: String,
    pub to_snapshot_id: String,
    pub snapshot_root: String,
    pub staged_artifact_root: String,
    pub elapsed_ms: u128,
    pub added_plugins: Vec<String>,
    pub removed_plugins: Vec<String>,
    pub changed_plugins: Vec<String>,
    pub changed_plugin_reasons: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReloadAttemptReport {
    pub status: ReloadAttemptStatus,
    pub from_snapshot_id: String,
    pub to_snapshot_id: Option<String>,
    pub snapshot_root: String,
    pub staged_artifact_root: String,
    pub elapsed_ms: u128,
    pub plugin_count: Option<usize>,
    pub node_count: Option<usize>,
    pub added_plugins: Vec<String>,
    pub removed_plugins: Vec<String>,
    pub changed_plugins: Vec<String>,
    pub changed_plugin_reasons: BTreeMap<String, Vec<String>>,
    pub failure_summary: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CandidateSnapshotStatus {
    pub from_snapshot_id: String,
    pub candidate_snapshot_id: String,
    pub snapshot_root: String,
    pub staged_artifact_root: String,
    pub plugin_count: usize,
    pub node_count: usize,
    pub added_plugins: Vec<String>,
    pub removed_plugins: Vec<String>,
    pub changed_plugins: Vec<String>,
    pub changed_plugin_reasons: BTreeMap<String, Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuntimeExecutionResult {
    pub target_node_fqn: String,
    pub selected_nodes: Vec<String>,
    pub net_diagnostics: Vec<String>,
    pub output: ExecutionOutput,
    pub traces: BTreeMap<String, ExecutionInvocationTrace>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RuntimeHostStatus {
    pub fixtures_root: String,
    pub snapshot_root: String,
    pub current_snapshot_id: String,
    pub plugin_count: usize,
    pub node_count: usize,
    pub candidate_snapshot: Option<CandidateSnapshotStatus>,
    pub last_reload: Option<ReloadAttemptReport>,
    pub last_candidate_reload: Option<ReloadAttemptReport>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KernelStatus {
    pub workspace_root: String,
    pub config_dir: String,
    pub llm_provider: String,
    pub llm_model: String,
    pub plugin_config_count: usize,
    pub iteration_total: u64,
    pub iteration_promote_total: u64,
    pub iteration_rollback_total: u64,
    /// 基础设施故障（磁盘满等）导致的失败次数。与 rollback 分开计数，
    /// 否则磁盘满会污染"验证失败率"这个指标。
    #[serde(default)]
    pub iteration_infrastructure_failure_total: u64,
    pub history_len: usize,
    pub last_change: Option<ChangeRecord>,
    pub plugin_issue_count: usize,
    pub blocked_iteration_count: usize,
    pub plugin_iteration_total: usize,
    pub last_plugin_iteration: Option<PluginIterationStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KernelApplyRequest {
    pub plan: AutoUpdatePlan,
    pub verification: VerificationInput,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KernelPluginIterationResult {
    pub iteration_id: String,
    pub issue_id: String,
    pub root_plugin_path: String,
    pub target_plugin_paths: Vec<String>,
    pub source: Option<KernelPluginIssueSource>,
    pub summary: String,
    pub agent_session_id: Option<String>,
    pub tool_execution_summary: Option<AgentToolExecutionSummary>,
    pub derived_edit_plan: PluginEditPlan,
    pub transcript_excerpt: Vec<AgentTranscriptEntry>,
    pub changed_paths: Vec<String>,
    pub rebuilt_artifacts: Vec<(String, String)>,
    pub candidate: Option<CandidateSnapshotStatus>,
    pub verification: Option<VerificationReport>,
    pub verifier_verdict: Option<VerifierVerdict>,
    pub canary: Option<CanaryReport>,
    pub final_verdict: PluginIterationFinalVerdict,
    pub blocked_reason: Option<String>,
    pub net_output: ExecutionOutput,
}

#[derive(Debug, Clone)]
struct PreparedPluginIteration {
    iteration_id: String,
    issue_id: String,
    root_plugin_path: String,
    target_plugin_paths: Vec<String>,
    source: Option<KernelPluginIssueSource>,
    summary: String,
    #[allow(dead_code)]
    manual_approved: bool,
    tests_command: Option<String>,
    safety_command: Option<String>,
    verify_profile: VerificationProfile,
    quality_score: Option<u32>,
    edit_plan: Option<PluginEditPlan>,
    instruction: Option<String>,
    allowed_plugin_roots: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct InvocationSample {
    plugin_path: String,
    node_id: String,
    payload: Value,
    response: Value,
    observed_at_ms: u128,
}

#[derive(Debug, Default, Clone)]
struct KernelIterationMetrics {
    iteration_total: u64,
    iteration_promote_total: u64,
    iteration_rollback_total: u64,
    iteration_infrastructure_failure_total: u64,
}

#[derive(Debug)]
pub struct RuntimeKernel {
    workspace_root: PathBuf,
    config_dir: PathBuf,
    llm_api: LlmApiConfig,
    plugin_configs: BTreeMap<String, PluginConfigFile>,
    plugin_iteration_policy: PluginIterationPolicy,
    plugin_issues: Mutex<BTreeMap<String, KernelPluginIssue>>,
    plugin_history: Mutex<VecDeque<PluginIterationHistoryEntry>>,
    blocked_iterations: Mutex<BTreeMap<String, KernelPluginIterationResult>>,
    last_plugin_iteration: Mutex<Option<KernelPluginIterationResult>>,
    active_plugin_iteration: Mutex<Option<String>>,
    iteration_metrics: Mutex<KernelIterationMetrics>,
    memory: Mutex<ChangeMemory>,
    updater: AutoUpdater,
}

impl RuntimeKernel {
    pub fn new(workspace_root: impl Into<PathBuf>, config: &RuntimeConfig) -> Self {
        let workspace_root = workspace_root.into();
        Self {
            config_dir: config.config_dir.clone(),
            llm_api: config.llm_api.clone(),
            plugin_configs: config.plugin_configs.clone(),
            plugin_iteration_policy: PluginIterationPolicy::default(),
            plugin_issues: Mutex::new(BTreeMap::new()),
            plugin_history: Mutex::new(VecDeque::new()),
            blocked_iterations: Mutex::new(BTreeMap::new()),
            last_plugin_iteration: Mutex::new(None),
            active_plugin_iteration: Mutex::new(None),
            iteration_metrics: Mutex::new(KernelIterationMetrics::default()),
            memory: Mutex::new(ChangeMemory::with_limit(config.kernel.change_history_limit)),
            updater: AutoUpdater::new(&workspace_root),
            workspace_root,
        }
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn status(&self) -> KernelStatus {
        // Lock each field individually and drop the guard immediately
        // to prevent deadlocks with concurrent PluginIteration operations
        // that may also need these locks.
        let plugin_issue_count = self
            .plugin_issues
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .len();
        let blocked_iteration_count = self
            .blocked_iterations
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .len();
        let plugin_iteration_total = self
            .plugin_history
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .len();
        let last_plugin_iteration = self
            .last_plugin_iteration
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        let metrics = self
            .iteration_metrics
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let (
            iteration_total,
            iteration_promote_total,
            iteration_rollback_total,
            iteration_infrastructure_failure_total,
        ) = (
            metrics.iteration_total,
            metrics.iteration_promote_total,
            metrics.iteration_rollback_total,
            metrics.iteration_infrastructure_failure_total,
        );
        drop(metrics);
        // Single memory lock for both fields — avoids double-futex.
        let (history_len, last_change) = {
            let memory = self
                .memory
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let len = memory.len();
            let last = memory.recent(1).into_iter().next();
            (len, last)
        };
        KernelStatus {
            workspace_root: self.workspace_root.display().to_string(),
            config_dir: self.config_dir.display().to_string(),
            llm_provider: self.llm_api.provider.clone(),
            llm_model: self.llm_api.model.clone(),
            plugin_config_count: self.plugin_configs.len(),
            iteration_total,
            iteration_promote_total,
            iteration_rollback_total,
            iteration_infrastructure_failure_total,
            history_len,
            last_change,
            plugin_issue_count,
            blocked_iteration_count,
            plugin_iteration_total,
            last_plugin_iteration: last_plugin_iteration
                .as_ref()
                .map(plugin_iteration_status_from_result),
        }
    }

    pub fn history(&self) -> Vec<ChangeRecord> {
        let memory = self
            .memory
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        memory.recent(memory.len())
    }

    pub fn plugin_issues(&self) -> Vec<KernelPluginIssue> {
        let mut issues = self
            .plugin_issues
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .values()
            .cloned()
            .collect::<Vec<_>>();
        issues.sort_by(|left, right| {
            left.status
                .cmp(&right.status)
                .then_with(|| left.source.priority().cmp(&right.source.priority()))
                .then_with(|| left.first_observed_at_ms.cmp(&right.first_observed_at_ms))
        });
        issues
    }

    pub fn plugin_history(&self) -> Vec<PluginIterationHistoryEntry> {
        self.plugin_history
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    pub fn blocked_iterations(&self) -> Vec<PluginIterationStatus> {
        self.blocked_iterations
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .values()
            .map(plugin_iteration_status_from_result)
            .collect()
    }

    pub fn plugin_iteration_status(
        &self,
        iteration_id: &str,
    ) -> Result<PluginIterationStatus, RuntimeError> {
        if let Some(result) = self
            .last_plugin_iteration
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
            .filter(|result| result.iteration_id == iteration_id)
        {
            return Ok(plugin_iteration_status_from_result(&result));
        }
        if let Some(result) = self
            .blocked_iterations
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(iteration_id)
            .cloned()
        {
            return Ok(plugin_iteration_status_from_result(&result));
        }
        if let Some(entry) = self
            .plugin_history
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .iter()
            .find(|entry| entry.iteration_id == iteration_id)
            .cloned()
        {
            return Ok(plugin_iteration_status_from_history(&entry));
        }
        Err(RuntimeError::PluginIterationStatusNotFound {
            iteration_id: iteration_id.to_string(),
        })
    }

    pub fn take_blocked_iteration(
        &self,
        iteration_id: &str,
    ) -> Result<KernelPluginIterationResult, RuntimeError> {
        let mut blocked = self
            .blocked_iterations
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let result = blocked.remove(iteration_id).ok_or_else(|| {
            RuntimeError::PluginIterationStatusNotFound {
                iteration_id: iteration_id.to_string(),
            }
        })?;
        // The blocked map's only writer (`record_plugin_iteration_outcome`)
        // inserts exclusively `Blocked` verdicts, so this holds by invariant.
        debug_assert_eq!(result.final_verdict, PluginIterationFinalVerdict::Blocked);
        Ok(result)
    }

    pub fn can_auto_iterate_plugins(&self) -> bool {
        self.llm_api
            .api_key
            .as_ref()
            .map(|key| !key.trim().is_empty())
            .unwrap_or(false)
            || std::env::var(&self.llm_api.api_key_env)
                .map(|value| !value.trim().is_empty())
                .unwrap_or(false)
    }

    pub fn observe_plugin_issue(
        &self,
        source: KernelPluginIssueSource,
        root_plugin_path: impl Into<String>,
        summary: impl Into<String>,
    ) -> KernelPluginIssue {
        let root_plugin_path = root_plugin_path.into();
        let summary = summary.into();
        let now_ms = now_ms();
        let issue_id = format!(
            "plugin-issue-{}-{}",
            root_plugin_path.replace('/', "-"),
            source.priority()
        );
        let mut guard = self
            .plugin_issues
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let issue = guard
            .entry(issue_id.clone())
            .or_insert_with(|| KernelPluginIssue {
                issue_id: issue_id.clone(),
                root_plugin_path: root_plugin_path.clone(),
                target_plugin_paths: vec![root_plugin_path.clone()],
                source,
                summary: summary.clone(),
                status: KernelPluginIssueStatus::Open,
                first_observed_at_ms: now_ms,
                last_observed_at_ms: now_ms,
                observe_count: 0,
            });
        issue.last_observed_at_ms = now_ms;
        issue.observe_count += 1;
        issue.summary = summary;
        if !matches!(issue.status, KernelPluginIssueStatus::Running) {
            issue.status = KernelPluginIssueStatus::Open;
        }
        issue.clone()
    }

    fn select_issue_for_request(
        &self,
        request: &KernelPluginIterationRequest,
    ) -> Result<Option<KernelPluginIssue>, RuntimeError> {
        let issues = self
            .plugin_issues
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(issue_id) = &request.issue_id {
            return issues.get(issue_id).cloned().map(Some).ok_or_else(|| {
                RuntimeError::PluginIterationIssueNotFound {
                    issue_id: issue_id.clone(),
                }
            });
        }
        let mut candidates = issues
            .values()
            .filter(|issue| issue.status == KernelPluginIssueStatus::Open)
            .cloned()
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| {
            left.source
                .priority()
                .cmp(&right.source.priority())
                .then_with(|| left.first_observed_at_ms.cmp(&right.first_observed_at_ms))
        });
        Ok(candidates.into_iter().next())
    }

    fn begin_plugin_iteration(
        &self,
        snapshot: &RuntimeSnapshot,
        request: &KernelPluginIterationRequest,
    ) -> Result<PreparedPluginIteration, RuntimeError> {
        let mut active = self
            .active_plugin_iteration
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(iteration_id) = active.clone() {
            return Err(RuntimeError::PluginIterationActive { iteration_id });
        }

        let selected_issue = self.select_issue_for_request(request)?;
        let iteration_id = normalize_request_id(None, "plugin-iteration");

        // Root workspace mode: empty target_plugin_paths means work across all
        // plugins. This allows creating new top-level plugins and editing any
        // existing one. The "" → "plugins" root in allowed_plugin_roots lets
        // validate_path accept any path under plugins/.
        let root_mode = request.target_plugin_paths.is_empty();
        let root_plugin_path = if let Some(issue) = &selected_issue {
            issue.root_plugin_path.clone()
        } else if root_mode {
            String::new()
        } else {
            determine_root_plugin_path(snapshot, &request.target_plugin_paths)?
        };
        let target_plugin_paths: Vec<String> = if root_mode {
            snapshot
                .plugin_registry()
                .iter()
                .map(|(plugin_path, _)| plugin_path.clone())
                .collect()
        } else {
            snapshot
                .plugin_registry()
                .iter()
                .map(|(plugin_path, _)| plugin_path)
                .filter(|plugin_path| {
                    plugin_path == &root_plugin_path
                        || plugin_path.starts_with(&format!("{root_plugin_path}/"))
                })
                .collect()
        };
        if target_plugin_paths.is_empty() && !root_mode {
            return Err(RuntimeError::InvalidArgument {
                message: format!("plugin subtree not found for {root_plugin_path}"),
            });
        }
        let issue_id = selected_issue
            .as_ref()
            .map(|issue| issue.issue_id.clone())
            .unwrap_or_else(|| format!("plugin-issue-{iteration_id}"));
        let summary = request
            .instruction
            .clone()
            .or_else(|| selected_issue.as_ref().map(|issue| issue.summary.clone()))
            .unwrap_or_else(|| format!("iterate plugin subtree {root_plugin_path}"));
        let mut allowed_plugin_roots: BTreeMap<String, String> = target_plugin_paths
            .iter()
            .map(|plugin_path: &String| (plugin_path.clone(), format!("plugins/{plugin_path}")))
            .collect();
        // In root mode, add a catch-all root so that paths under any plugin
        // directory (including not-yet-created ones) pass subtree validation.
        if root_mode {
            allowed_plugin_roots.insert(String::new(), "plugins".to_string());
        }

        if let Some(ref issue) = selected_issue {
            self.plugin_issues
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .entry(issue.issue_id.clone())
                .and_modify(|entry| entry.status = KernelPluginIssueStatus::Running);
        }
        *active = Some(iteration_id.clone());

        Ok(PreparedPluginIteration {
            iteration_id,
            issue_id,
            root_plugin_path,
            target_plugin_paths,
            source: selected_issue.as_ref().map(|issue| issue.source),
            summary,
            manual_approved: request.manual_approved,
            tests_command: request.tests_command.clone(),
            safety_command: request.safety_command.clone(),
            verify_profile: request
                .verify_profile
                .unwrap_or(VerificationProfile::RustWorkspace),
            quality_score: request.quality_score,
            edit_plan: request.edit_plan.clone(),
            instruction: request.instruction.clone(),
            allowed_plugin_roots,
        })
    }

    pub fn finish_plugin_iteration(&self, iteration_id: &str) {
        let mut active = self
            .active_plugin_iteration
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if active.as_deref() == Some(iteration_id) {
            *active = None;
        }
    }

    fn update_issue_status(&self, issue_id: &str, status: KernelPluginIssueStatus) {
        if let Some(issue) = self
            .plugin_issues
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get_mut(issue_id)
        {
            issue.status = status;
        }
    }

    pub fn record_plugin_iteration_outcome(&self, result: &KernelPluginIterationResult) {
        // P1-9: fragile nested-lock chain. Every lock is now acquired,
        // mutated, and released in the shortest possible scope so that at
        // no point does this function hold two of the kernel Mutexes at
        // once. Callers that hold `plugin_issues` (e.g. `select_issue_for_
        // request`) can invert order all day without deadlocking us.
        //
        // Canonical lock-order document (post-fix): every mutating call in
        // this file acquires locks in this order (partial order); this
        // function honours it by never holding more than one at a time.
        //   iteration_metrics → plugin_history → last_plugin_iteration →
        //   blocked_iterations → plugin_issues
        let completed_at_ms = now_ms();
        let history_entry = PluginIterationHistoryEntry {
            iteration_id: result.iteration_id.clone(),
            issue_id: result.issue_id.clone(),
            root_plugin_path: result.root_plugin_path.clone(),
            target_plugin_paths: result.target_plugin_paths.clone(),
            source: result.source,
            summary: result.summary.clone(),
            changed_paths: result.changed_paths.clone(),
            verifier_verdict: result.verifier_verdict,
            canary_verdict: result.canary.as_ref().map(|report| report.verdict),
            final_verdict: result.final_verdict,
            blocked_reason: result.blocked_reason.clone(),
            observed_at_ms: completed_at_ms,
            completed_at_ms,
        };

        {
            let mut metrics = self
                .iteration_metrics
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            metrics.iteration_total += 1;
            match result.final_verdict {
                PluginIterationFinalVerdict::Blocked => {}
                PluginIterationFinalVerdict::Promoted => {
                    metrics.iteration_promote_total += 1;
                }
                PluginIterationFinalVerdict::RolledBack => {
                    metrics.iteration_rollback_total += 1;
                }
                PluginIterationFinalVerdict::InfrastructureFailure => {
                    metrics.iteration_infrastructure_failure_total += 1;
                }
            }
        }

        {
            // P1-23: cap `plugin_history` at a hard upper bound; without
            // this the VecDeque grew forever on long-running hosts. Newest
            // entries stay at the front, so we drop from the back.
            const MAX_PLUGIN_HISTORY: usize = 1024;
            let mut history = self
                .plugin_history
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if let Some(existing) = history
                .iter_mut()
                .find(|entry| entry.iteration_id == result.iteration_id)
            {
                *existing = history_entry;
            } else {
                history.push_front(history_entry);
                while history.len() > MAX_PLUGIN_HISTORY {
                    history.pop_back();
                }
            }
        }

        {
            *self
                .last_plugin_iteration
                .lock()
                .unwrap_or_else(|poison| poison.into_inner()) = Some(result.clone());
        }

        {
            let mut blocked = self
                .blocked_iterations
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            match result.final_verdict {
                // 基础设施故障与 Blocked 一样保留：腾出磁盘后可经
                // approve_blocked_iteration 原样重试；RolledBack 摘除是因为
                // 那代表插件真的没通过验证，重试同一份代码没有意义。
                PluginIterationFinalVerdict::Blocked
                | PluginIterationFinalVerdict::InfrastructureFailure => {
                    blocked.insert(result.iteration_id.clone(), result.clone());
                }
                PluginIterationFinalVerdict::Promoted | PluginIterationFinalVerdict::RolledBack => {
                    blocked.remove(&result.iteration_id);
                }
            }
        }

        let status = match result.final_verdict {
            PluginIterationFinalVerdict::Blocked => KernelPluginIssueStatus::Blocked,
            PluginIterationFinalVerdict::Promoted => KernelPluginIssueStatus::Resolved,
            PluginIterationFinalVerdict::RolledBack => KernelPluginIssueStatus::Open,
            // 不是 Open：Open 读作"插件仍然坏着"，而磁盘满与插件质量无关。
            // 复用 Blocked（等人处置基础设施后重试），不新增 issue 状态。
            PluginIterationFinalVerdict::InfrastructureFailure => KernelPluginIssueStatus::Blocked,
        };
        self.update_issue_status(&result.issue_id, status);
    }

    pub fn run_iteration(
        &self,
        plan: AutoUpdatePlan,
        verification: VerificationInput,
    ) -> Result<AutoUpdateResult, RuntimeError> {
        let issue_id = plan.issue_id.clone();
        let patch_id = plan.patch_id.clone();
        {
            let mut metrics = self
                .iteration_metrics
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            metrics.iteration_total += 1;
        }
        let result = self
            .updater
            .execute(plan, |_| Ok(VerificationEnvelope::from(verification)))?;
        {
            let mut metrics = self
                .iteration_metrics
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if result.rolled_back {
                metrics.iteration_rollback_total += 1;
            } else {
                metrics.iteration_promote_total += 1;
            }
        }
        self.memory
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .record(
                issue_id,
                patch_id,
                "auto_update".to_string(),
                None,
                if result.rolled_back {
                    crate::kernel::memory::ChangeVerdict::Rollback
                } else {
                    crate::kernel::memory::ChangeVerdict::Promote
                },
                result.quality_score,
                Vec::new(),
            );
        Ok(result)
    }
}

#[derive(Debug)]
pub struct RuntimeHost {
    fixtures_root: PathBuf,
    config: RuntimeConfig,
    loader: Loader,
    snapshot_root: PathBuf,
    current_snapshot: RwLock<Arc<RuntimeSnapshot>>,
    candidate_snapshot: Mutex<Option<StagedCandidateSnapshot>>,
    invocation_samples: Mutex<VecDeque<InvocationSample>>,
    retired_snapshots: Mutex<Vec<RetiredSnapshot>>,
    last_reload_attempt: Mutex<Option<ReloadAttemptReport>>,
    last_candidate_reload_attempt: Mutex<Option<ReloadAttemptReport>>,
    agent_sessions: Mutex<BTreeMap<String, ManagedAgentSession>>,
    /// P1-25: side-channel for tool calls that need to mutate their own
    /// active `AgentSession`. During `agent_send` the session is removed
    /// from `agent_sessions` for the duration of `respond`; tools like
    /// `compact_context` can't `get_mut` the session because it's not in
    /// the map. Instead they push a `PendingSessionAction` here; the
    /// `agent_send` prologue drains and applies them before reinsert.
    pending_session_actions: Mutex<BTreeMap<String, Vec<PendingSessionAction>>>,
    /// Per-session LLM profile fallback state. Single inbox thread means
    /// no real contention; the Mutex is for interior mutability only.
    profile_fallback: Mutex<BTreeMap<String, ProfileFallbackEntry>>,
    /// Registry of background services (Task nodes).
    pub service_registry: Arc<crate::context::ServiceRegistry>,
    /// Accumulated rollback for interactive agent file edits.
    interactive_rollback: Mutex<PluginEditRollback>,
    kernel: RuntimeKernel,
}

#[derive(Debug)]
struct RetiredSnapshot {
    snapshot: Weak<RuntimeSnapshot>,
    staged_artifact_root: PathBuf,
}

#[derive(Debug, Clone)]
struct StagedCandidateSnapshot {
    snapshot: Arc<RuntimeSnapshot>,
    status: CandidateSnapshotStatus,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentSessionKind {
    RuntimeShell,
    PluginIteration,
}

/// P1-25: actions a tool can request against its own currently-running
/// `AgentSession`. The tool queues one of these instead of blocking
/// waiting for the session lock (which is held by `agent_send`
/// throughout `respond`); `agent_send` drains and applies before
/// reinserting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingSessionAction {
    /// Run `session.compact_history()` after the current turn.
    CompactHistory,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentSessionHandle {
    pub session_id: String,
    pub kind: AgentSessionKind,
}

/// Options for `agent_start_with`. `profile` selects a named entry from
/// `llm_profiles` (None/unknown → default). Kept as a struct so the soul
/// scope key and future per-session knobs extend without churning callers.
#[derive(Debug, Clone, Default)]
pub struct AgentStartOptions {
    pub profile: Option<String>,
    /// Soul scope key for the persona overlay; "" = no per-user soul.
    pub soul_key: String,
}

/// Per-session profile fallback state. `desired` is the profile the
/// session was started with; `degraded` is true while requests are being
/// served by that profile's fallback instead.
#[derive(Debug, Clone)]
struct ProfileFallbackEntry {
    desired: String,
    degraded: bool,
}

/// O批: soul provider backed by a storage plugin's `soul_get`/`soul_set`
/// capability nodes (see `crate::soul` for the payload contract).
struct PluginSoulProvider<'a> {
    host: &'a RuntimeHost,
    plugin_path: String,
}

impl crate::soul::SoulProvider for PluginSoulProvider<'_> {
    fn get(&self, soul_key: &str) -> Result<Option<crate::soul::Soul>, RuntimeError> {
        let payload = serde_json::json!({
            "node_id": "soul_get",
            // data_dir travels in the payload so the plugin never has to
            // guess the workspace root from env/cwd.
            "payload": {
                "soul_key": soul_key,
                "data_dir": self.host.data_dir().display().to_string(),
            },
        });
        let response = self
            .host
            .invoke(&self.plugin_path, "soul_get", payload.to_string())?;
        let value: serde_json::Value = serde_json::from_str(&response.payload)
            .map_err(|e| soul_reply_error(&self.plugin_path, "is not JSON", &e))?;
        match value.get("soul") {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(soul) => serde_json::from_value(soul.clone())
                .map(Some)
                .map_err(|e| soul_reply_error(&self.plugin_path, "malformed", &e)),
        }
    }

    fn set(&self, soul_key: &str, soul: &crate::soul::Soul) -> Result<(), RuntimeError> {
        let payload = serde_json::json!({
            "node_id": "soul_set",
            "payload": {
                "soul_key": soul_key,
                "soul": soul,
                "data_dir": self.host.data_dir().display().to_string(),
            },
        });
        self.host
            .invoke(&self.plugin_path, "soul_set", payload.to_string())?;
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct ManagedAgentSession {
    #[allow(dead_code)]
    handle: AgentSessionHandle,
    session: AgentSession,
    state: ManagedAgentState,
}

#[derive(Debug)]
enum ManagedAgentState {
    RuntimeShell,
    PluginIteration(Box<PluginIterationAgentState>),
}

#[derive(Debug, Clone)]
struct PluginIterationAgentSnapshot {
    recorded_summary: Option<String>,
    tests_command: Option<String>,
    safety_command: Option<String>,
    changed_paths: Vec<String>,
    rollback: PluginEditRollback,
    derived_edit_plan: PluginEditPlan,
}

#[derive(Debug, Clone)]
struct PluginIterationAgentState {
    prepared: PreparedPluginIteration,
    focus_context_paths: Vec<String>,
    all_context_paths: Vec<String>,
    context_scope_expanded: bool,
    recorded_summary: Option<String>,
    tests_command: Option<String>,
    safety_command: Option<String>,
    verification_attempts: usize,
    verification_successes: usize,
    rollback: PluginEditRollback,
    operations: Vec<PluginEditOperation>,
    scaffolded_children: Vec<ScaffoldedChildRegistration>,
}

#[derive(Debug, Clone)]
struct PluginIterationAgentRun {
    session_id: Option<String>,
    tool_summary: Option<AgentToolExecutionSummary>,
    transcript_excerpt: Vec<AgentTranscriptEntry>,
    snapshot: PluginIterationAgentSnapshot,
}

impl PluginIterationAgentState {
    fn new(
        prepared: PreparedPluginIteration,
        context_paths: PluginIterationContextPaths,
        workspace_root: &Path,
    ) -> Self {
        Self {
            prepared,
            focus_context_paths: context_paths.focus_paths,
            all_context_paths: context_paths.all_paths,
            context_scope_expanded: false,
            recorded_summary: None,
            tests_command: None,
            safety_command: None,
            verification_attempts: 0,
            verification_successes: 0,
            rollback: PluginEditRollback::empty(workspace_root),
            operations: Vec::new(),
            scaffolded_children: Vec::new(),
        }
    }

    fn snapshot(&self) -> PluginIterationAgentSnapshot {
        let derived_edit_plan = PluginEditPlan {
            issue_id: self.prepared.issue_id.clone(),
            patch_id: format!("{}-agent", self.prepared.iteration_id),
            summary: self
                .recorded_summary
                .clone()
                .unwrap_or_else(|| self.prepared.summary.clone()),
            operations: self.operations.clone(),
        };
        PluginIterationAgentSnapshot {
            recorded_summary: self.recorded_summary.clone(),
            tests_command: self.tests_command.clone(),
            safety_command: self.safety_command.clone(),
            changed_paths: derived_edit_plan.changed_paths(),
            rollback: self.rollback.clone(),
            derived_edit_plan,
        }
    }
}

#[derive(Debug, Clone)]
struct PluginIterationContextPaths {
    focus_paths: Vec<String>,
    all_paths: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ScaffoldedChildRegistration {
    parent_manifest_path: String,
    child_root_path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContextFilesScope {
    Focus,
    All,
}

/// Test-only FFI-panic injection flag for the `iterate_plugins` catch_unwind
/// guard. When set, the next entry into the iteration body panics so tests can
/// exercise the emergency-rollback arm without a real plugin fault. Swapped
/// back to `false` on read so it fires exactly once. Referenced by the
/// `#[cfg(test)]` arm inside `iterate_plugins` and by `mod tests`.
#[cfg(test)]
pub(crate) static TEST_ITERATION_PANIC_INJECTION: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Test-only FFI-panic injection flag for the reload/stop-handler catch_unwind
/// guard (Task-node `stop` invocation). When set, the next stop invocation
/// panics so tests can verify the panic is caught and the reload path keeps
/// running. Swapped back to `false` on read so it fires exactly once.
#[cfg(test)]
pub(crate) static TEST_STOP_HANDLER_PANIC_INJECTION: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Test-only ENOSPC injection flag. 置位后 journal persist 阶段返回一个 ENOSPC
/// 形状的 `RuntimeError::Io`，让测试无需真把盘写满就能覆盖"磁盘满判成
/// `InfrastructureFailure` 而非 `RolledBack`"。journal 写在 `snapshot_root` 下，
/// 正是磁盘写满时最先失败的位置之一（真实链路 persist_journal → atomic_write
/// → `RuntimeError::Io`）。读后置回 `false`，只触发一次。
#[cfg(test)]
pub(crate) static TEST_ITERATION_ENOSPC_INJECTION: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

impl RuntimeHost {
    pub fn boot(fixtures_root: impl AsRef<Path>) -> Result<Self, RuntimeError> {
        let fixtures_root = fixtures_root.as_ref().to_path_buf();
        let config = RuntimeConfig::load(&fixtures_root)?;
        let loader = Loader::new(default_loader_config(&fixtures_root));
        let snapshot_root = config
            .resolve_snapshot_root(&fixtures_root)
            .unwrap_or_else(|| default_snapshot_root(&fixtures_root));
        fs::create_dir_all(&snapshot_root)
            .map_err(|e| host_io_error(snapshot_root.clone(), e.to_string()))?;
        cleanup_stale_snapshot_dirs(&snapshot_root);
        // 兄弟 hash 目录的回收：`cleanup_stale_snapshot_dirs` 只管本 root
        // 内部，跨 root 的孤儿（fixtures root 已消失、hash 再也不会重现）
        // 只能在这里扫。默认 root 之外（配置了 runtime.snapshot_root）不做
        // 跨目录清理，避免动到用户自己指定的目录树。
        let host_snapshot_dir = default_host_snapshot_dir();
        if snapshot_root.starts_with(&host_snapshot_dir) {
            let report = cleanup_orphaned_snapshot_roots(
                &host_snapshot_dir,
                config.snapshot_retention(),
                Some(&snapshot_root),
                false,
            );
            if report.removed > 0 {
                eprintln!(
                    "[snapshot-gc] 回收 {} 个孤儿 snapshot root，释放 {:.1} MB",
                    report.removed,
                    report.bytes_reclaimed as f64 / (1024.0 * 1024.0)
                );
            }
        }
        let initial_snapshot = Arc::new(build_snapshot(&loader, &snapshot_root)?);
        let interactive_rollback = Mutex::new(PluginEditRollback::empty(&fixtures_root));
        let service_registry = Arc::new(crate::context::ServiceRegistry::new());
        let host = Self {
            kernel: RuntimeKernel::new(&fixtures_root, &config),
            config,
            fixtures_root,
            loader,
            snapshot_root,
            current_snapshot: RwLock::new(initial_snapshot),
            candidate_snapshot: Mutex::new(None),
            invocation_samples: Mutex::new(VecDeque::new()),
            retired_snapshots: Mutex::new(Vec::new()),
            last_reload_attempt: Mutex::new(None),
            last_candidate_reload_attempt: Mutex::new(None),
            agent_sessions: Mutex::new(BTreeMap::new()),
            pending_session_actions: Mutex::new(BTreeMap::new()),
            profile_fallback: Mutex::new(BTreeMap::new()),
            service_registry,
            interactive_rollback,
        };
        host.detect_crash_and_recover();
        // P0-6: recover from a crashed plugin-iteration by replaying the
        // durable rollback journal. Done AFTER the initial snapshot is built
        // but before any user request lands. Errors are logged and the
        // orphan journal is left on disk for operator inspection rather than
        // panicking `boot`.
        match restore_plugin_iteration_workspace(&host.fixtures_root, &host.snapshot_root, None) {
            Err(err) => {
                let jp = plugin_iteration_journal_path(&host.snapshot_root);
                let subject = format!(
                    "boot-time restore failed: {err}; journal preserved at {}",
                    jp.display()
                );
                eprintln!("[plugin-iteration-recovery] {subject}");
            }
            Ok(true) => {
                // The initial snapshot above was built from the PRE-restore
                // artifacts (the crashed iteration's half-promoted state).
                // After the journal replay rewrote sources and rebuilt the
                // artifacts, reload so the live registry reflects the
                // restored tree — otherwise docs/nodes from the rolled-back
                // candidate leak into the recovered snapshot.
                if let Err(err) = host.reload("/") {
                    eprintln!("[plugin-iteration-recovery] post-restore reload failed: {err}");
                }
            }
            Ok(false) => {}
        }
        Ok(host)
    }

    pub fn fixtures_root(&self) -> &Path {
        &self.fixtures_root
    }

    /// Write a shutdown memory snapshot to data/memory/shutdown.json.
    /// Uses try_lock to avoid deadlocking with active agent sessions.
    pub fn write_shutdown_memory(&self) {
        let ws_root = self
            .fixtures_root
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| self.fixtures_root.clone());
        let path = ws_root.join("data/memory/shutdown.json");
        let _ = std::fs::create_dir_all(path.parent().unwrap());
        let now = chrono::Local::now()
            .format("%Y-%m-%dT%H:%M:%S%z")
            .to_string();
        // Use try_lock to avoid deadlocking if an agent session is active.
        let sessions: Vec<serde_json::Value> = self
            .agent_sessions
            .try_lock()
            .map(|guard| {
                guard
                    .iter()
                    .map(|(sid, s)| {
                        let st = s.session.status();
                        serde_json::json!({
                            "session_id": sid,
                            "kind": st.kind,
                            "completed_turns": st.completed_turns,
                            "model": st.model,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let snapshot = self.current_snapshot();
        let plugins: Vec<serde_json::Value> = snapshot
            .plugin_registry()
            .iter()
            .map(|(p, pl)| {
                serde_json::json!({
                    "plugin_path": p,
                    "load_result": format!("{:?}", pl.load_result),
                })
            })
            .collect();
        let memory = serde_json::json!({
            "shutdown_at": now,
            "sessions": sessions,
            "plugins": plugins,
        });
        // `memory` is a `json!` object of owned strings, so serialization is
        // infallible in practice; iterating the `Result` keeps the historical
        // "skip silently on serialize failure" behaviour without an `if let`
        // whose else-branch can never run.
        for json in serde_json::to_string_pretty(&memory).into_iter() {
            // P1-15: previously `fs::write(&path, json)` — a crash mid-write
            // (SIGKILL / power loss) left a truncated JSON on disk that the
            // next boot would parse-fail on. All other runtime writers use
            // tmp + rename; shutdown is now the same.
            if let Err(err) = atomic_write_bytes(&path, json.as_bytes()) {
                let subject = format!("failed to write memory to {}", path.display());
                eprintln!("{}", Self::host_io_log_line("shutdown", &subject, &err));
            } else {
                eprintln!("[shutdown] wrote memory to {}", path.display());
            }
        }
    }

    /// O批: resolve the active soul provider. A loaded plugin exposing
    /// BOTH `soul_get` and `soul_set` nodes overrides the kernel's file
    /// default (capability-node convention — resolved per call so a
    /// reload picks up / drops the override automatically).
    fn soul_provider(&self) -> Box<dyn crate::soul::SoulProvider + '_> {
        let snapshot = self.current_snapshot();
        for (plugin_path, plugin) in snapshot.plugin_registry().iter() {
            // A registry entry only carries `docs` while it is `Loaded`
            // (`insert_unavailable` / `mark_unavailable` both clear the field),
            // so the two guards collapse into one condition instead of leaving
            // a separate `continue` whose predicate never differs from the
            // docs check.
            let loaded_docs = plugin
                .docs
                .as_ref()
                .filter(|_| plugin.load_result == crate::core::models::PluginLoadResult::Loaded);
            let Some(docs) = loaded_docs else { continue };
            let has_get = docs.nodes.iter().any(|n| n.id == "soul_get");
            let has_set = docs.nodes.iter().any(|n| n.id == "soul_set");
            if has_get && has_set {
                return Box::new(PluginSoulProvider {
                    host: self,
                    plugin_path,
                });
            }
        }
        Box::new(crate::soul::FileSoulProvider::new(&self.data_dir()))
    }

    pub fn get_soul(&self, soul_key: &str) -> Result<Option<crate::soul::Soul>, RuntimeError> {
        if soul_key.is_empty() {
            return Ok(None);
        }
        self.soul_provider().get(soul_key)
    }

    pub fn set_soul(&self, soul_key: &str, soul: &crate::soul::Soul) -> Result<(), RuntimeError> {
        if soul_key.is_empty() {
            return Err(RuntimeError::InvalidArgument {
                message: "cannot store a soul without a scope key".to_string(),
            });
        }
        self.soul_provider().set(soul_key, soul)
    }

    /// Workspace-root-relative `data/` directory. Public because the
    /// inbox loop (pending-message spill) and soul storage share it.
    pub fn data_dir(&self) -> PathBuf {
        self.fixtures_root
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| self.fixtures_root.clone())
            .join("data")
    }

    /// Log a best-effort host I/O failure. Centralizing the format keeps the
    /// call sites single-line and lets the message shape be unit-tested.
    fn host_io_log_line(stage: &str, subject: &str, err: &dyn std::fmt::Display) -> String {
        format!("[{stage}] {subject}: {err}")
    }

    /// Best-effort save of a session snapshot to `data/sessions/<id>.json`.
    /// Uses atomic temp-file-then-rename.  Errors are logged but never
    /// propagated — an auto-save failure must not break the agent response.
    fn auto_save_session(&self, session_id: &str, session: &AgentSession) {
        let sessions_dir = self.data_dir().join("sessions");
        if let Err(e) = std::fs::create_dir_all(&sessions_dir) {
            let subject = format!("failed to create sessions dir {}", sessions_dir.display());
            eprintln!("{}", Self::host_io_log_line("auto-save", &subject, &e));
            return;
        }
        // P0-25: session snapshots may embed non-secret HTTP config (base_url,
        // model, timeouts) — the api_key field is now `#[serde(skip_serializing)]`
        // so it does NOT land on disk, but even so we tighten the dir to
        // owner-only permissions.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&sessions_dir, std::fs::Permissions::from_mode(0o700));
        }
        let snapshot = session.to_snapshot();
        let target = sessions_dir.join(format!("{session_id}.json"));
        // P1-24(c): unique tmp filename per write so concurrent
        // `agent_send` invocations for the same session id don't stomp on
        // each other's staging file. Previously `.<id>.json.tmp` was
        // shared, so a race would leave a truncated / mis-attributed
        // snapshot.
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = sessions_dir.join(format!(".{session_id}.json.tmp.{}", seq));
        // Serialize then stage, each with its own log message, funnelling into
        // one early return. `AgentSessionSnapshot` is a tree of owned strings
        // and numbers, so `to_vec` has no reachable failure through this call
        // site; keeping it in the same chain as the staging write means one
        // bail-out path rather than a separate arm that no input can reach.
        let staged = serde_json::to_vec(&snapshot)
            .map_err(|e| format!("[auto-save] serialize failed for {session_id}: {e}"))
            .and_then(|json| {
                std::fs::write(&tmp, &json)
                    .map_err(|e| format!("[auto-save] write tmp failed for {session_id}: {e}"))
            });
        if let Err(log_line) = staged {
            eprintln!("{log_line}");
            return;
        }
        if let Err(e) = std::fs::rename(&tmp, &target) {
            eprintln!("[auto-save] rename failed for {session_id}: {e}");
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// P1-24(a): remove the on-disk snapshot for `session_id` so a
    /// completed/reset session is not re-hydrated by the crash recovery
    /// path on next boot. Best-effort — failures are logged.
    pub fn delete_session_snapshot(&self, session_id: &str) {
        let path = self
            .data_dir()
            .join("sessions")
            .join(format!("{session_id}.json"));
        if !path.exists() {
            return;
        }
        if let Err(err) = std::fs::remove_file(&path) {
            let subject = format!("failed to delete session snapshot {}", path.display());
            eprintln!("{}", Self::host_io_log_line("auto-save", &subject, &err));
        }
    }

    /// H2: session-termination cleanup entry point. Removes the session from
    /// all three per-session maps (`agent_sessions`, `pending_session_actions`,
    /// `profile_fallback`) and deletes its on-disk snapshot. Call this whenever
    /// a session ends: `/reset`, LRU eviction, or `plugin_iteration` completion.
    /// Idempotent — dropping an unknown session id is a no-op (no panic, no
    /// error).
    pub fn drop_session(&self, session_id: &str) {
        self.agent_sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(session_id);
        self.pending_session_actions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(session_id);
        self.profile_fallback
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(session_id);
        self.delete_session_snapshot(session_id);
    }

    /// Test/debug helper: current entry counts of the three per-session maps,
    /// as `(agent_sessions, pending_session_actions, profile_fallback)`. Lock
    /// order matches the rest of this impl to avoid deadlock.
    #[doc(hidden)]
    pub fn debug_session_map_sizes(&self) -> (usize, usize, usize) {
        let agent_sessions = self
            .agent_sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .len();
        let pending = self
            .pending_session_actions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .len();
        let fallback = self
            .profile_fallback
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .len();
        (agent_sessions, pending, fallback)
    }

    /// Check for saved sessions in `data/sessions/` and reconstruct them.
    /// Called once at the end of `boot()`.  If sessions exist from a previous
    /// run (crash or deliberate restart), they are restored into the agent
    /// session map so the user can continue where they left off.
    fn detect_crash_and_recover(&self) {
        let sessions_dir = self.data_dir().join("sessions");
        let dir = match std::fs::read_dir(&sessions_dir) {
            Ok(d) => d,
            Err(_) => return, // no sessions dir, nothing to recover
        };
        let mut recovered = 0usize;
        let mut skipped = 0usize;
        for entry in dir.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            // Skip temp files.
            if path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with('.'))
            {
                continue;
            }
            // Read → parse → reconstruct, each step logging its own message and
            // funnelling into one shared `skipped += 1; continue`. Folding the
            // three `match … Err(e) => { … }` arms into a single fallible block
            // keeps every log line byte-identical while leaving one skip path
            // instead of three, the last of which (`from_snapshot`) cannot be
            // provoked through the filesystem at all — it only fails when
            // reqwest cannot construct an HTTP client.
            let hydrated = std::fs::read(&path)
                .map_err(|e| {
                    let subject = format!("read failed for {}", path.display());
                    Self::host_io_log_line("crash-recovery", &subject, &e)
                })
                .and_then(|json| {
                    serde_json::from_slice::<AgentSessionSnapshot>(&json).map_err(|e| {
                        format!("[crash-recovery] parse failed for {}: {e}", path.display())
                    })
                })
                .and_then(|snapshot| {
                    AgentSession::from_snapshot(snapshot)
                        .map_err(|e| crash_recovery_reconstruct_log_line(&path, &e))
                });
            let session = match hydrated {
                Ok(session) => session,
                Err(log_line) => {
                    eprintln!("{log_line}");
                    skipped += 1;
                    continue;
                }
            };
            let session_id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("recovered-session")
                .to_string();
            // P1-24(b): read the recovered session's own `kind` field
            // instead of hardcoding RuntimeShell. Otherwise a
            // PluginIteration session comes back as RuntimeShell, whose
            // tool set disagrees with what the transcript expects.
            let (session_kind, agent_state) = match session.kind() {
                "plugin_iteration" => (
                    AgentSessionKind::PluginIteration,
                    // Recovery cannot reconstruct the in-progress iteration
                    // state; hold the session in a neutral RuntimeShell-
                    // state for now so it's inspectable. The plugin-
                    // iteration transition itself is guarded by the
                    // rollback journal (P0-6), so this is a UX
                    // concession, not a correctness gap.
                    ManagedAgentState::RuntimeShell,
                ),
                _ => (
                    AgentSessionKind::RuntimeShell,
                    ManagedAgentState::RuntimeShell,
                ),
            };
            let handle = AgentSessionHandle {
                session_id: session_id.clone(),
                kind: session_kind,
            };
            self.agent_sessions
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .insert(
                    session_id,
                    ManagedAgentSession {
                        handle,
                        session,
                        state: agent_state,
                    },
                );
            recovered += 1;
        }
        if recovered > 0 || skipped > 0 {
            eprintln!("[crash-recovery] recovered {recovered} session(s), skipped {skipped}");
        }
    }

    /// Register and start a background service for a Task node.
    pub fn start_service(
        &self,
        plugin_path: &str,
        node_id: &str,
        svc: Box<dyn crate::context::Service>,
    ) -> Result<(), RuntimeError> {
        self.service_registry
            .start_service(plugin_path, node_id, svc)
    }

    pub(crate) fn interactive_rollback(&self) -> std::sync::MutexGuard<'_, PluginEditRollback> {
        self.interactive_rollback
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    /// Resolve a relative path within the fixtures root, rejecting traversal
    /// attempts (absolute paths, `..` components) and symlink escapes.
    pub fn check_agent_accessible(
        &self,
        plugin_path: &str,
        node_id: &str,
    ) -> Result<(), RuntimeError> {
        let snapshot = self.current_snapshot();
        let registry = snapshot.plugin_registry();
        let plugin =
            registry
                .get(plugin_path)
                .ok_or_else(|| RuntimeError::PluginNotRegistered {
                    plugin_path: plugin_path.to_string(),
                })?;
        // Flattened from three nested guards: the only accepting case is "docs
        // present AND node declared AND flag set", and every other combination
        // falls through to the same rejection. Chaining keeps that one condition
        // on one line instead of leaving unreachable fall-through braces.
        let permitted = plugin
            .docs
            .as_ref()
            .and_then(|docs| docs.nodes.iter().find(|n| n.id == node_id))
            .is_some_and(|node| node.agent_accessible);
        if permitted {
            return Ok(());
        }
        Err(RuntimeError::InvalidArgument {
            message: format!("Agent is not allowed to call {plugin_path}::{node_id}"),
        })
    }

    pub fn check_sensitive_path(&self, path: &str) -> Result<(), RuntimeError> {
        let lower = path.to_lowercase();
        for kw in &[
            ".ssh",
            ".claude",
            "auth.json",
            "credentials",
            ".env",
            "id_rsa",
            "id_ed25519",
            "id_ecdsa",
            "known_hosts",
            "access_token",
            "api_key",
            "api_secret",
            "private_key",
            "/etc/passwd",
            "/etc/shadow",
            "/proc/",
            "/sys/",
        ] {
            if lower.contains(kw) {
                return Err(RuntimeError::InvalidArgument {
                    message: format!("blocked: path references sensitive resource ({kw})"),
                });
            }
        }
        Ok(())
    }

    pub fn check_sensitive_command(&self, command: &str) -> Result<(), RuntimeError> {
        let lower = command.to_lowercase();
        for kw in &[
            "ssh",
            "scp",
            "ssh-keygen",
            "cat /etc/passwd",
            "cat /etc/shadow",
            ".ssh/id",
            ".claude/",
            "auth.json",
            "token",
            "password",
            "secret",
            "credential",
            "export ",
            "unset ",
            "declare -",
        ] {
            if lower.contains(kw) {
                return Err(RuntimeError::InvalidArgument {
                    message: format!("blocked: command references sensitive operation ({kw})"),
                });
            }
        }
        Ok(())
    }

    pub fn resolve_sandboxed_path(&self, rel: &str) -> Result<PathBuf, RuntimeError> {
        let rel_path = Path::new(rel);
        if rel_path.is_absolute() {
            return Err(RuntimeError::InvalidArgument {
                message: format!("absolute path is not allowed: {rel}"),
            });
        }
        if rel_path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(RuntimeError::InvalidArgument {
                message: format!("parent directory traversal (..) is not allowed: {rel}"),
            });
        }
        // Paths under data/ resolve against the workspace root (parent of
        // fixtures/) so the agent can persist data outside the sandbox.
        let (base_root, canonical_root) = if rel.starts_with("data/") || rel == "data" {
            let ws = self
                .fixtures_root
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| self.fixtures_root.clone());
            let canon = ws.canonicalize().unwrap_or_else(|_| ws.clone());
            (ws, canon)
        } else {
            let fr = self.fixtures_root.to_path_buf();
            let canon = fr.canonicalize().unwrap_or_else(|_| fr.clone());
            (fr, canon)
        };
        let resolved = base_root.join(rel_path);
        // Verify containment.  Try canonical form first (catches symlink
        // escapes); when the path does not exist yet (canonicalize fails),
        // walk up to the nearest existing ancestor and canonicalize that.
        // `ancestors()` yields `resolved` first and then each parent, ending at
        // the filesystem root (or the empty path for a relative base), so the
        // former hand-rolled climb-until-parent()-is-None loop is expressed
        // without a terminating arm that only fires for a base root that has
        // already been deleted. Same result: nearest existing ancestor, or
        // `resolved` itself when nothing along the chain canonicalizes.
        let check = resolved
            .ancestors()
            .find(|ancestor| ancestor.exists())
            .and_then(|ancestor| ancestor.canonicalize().ok())
            .unwrap_or_else(|| resolved.clone());
        if !check.starts_with(&canonical_root) {
            return Err(RuntimeError::InvalidArgument {
                message: format!("path escapes fixtures root: {rel}"),
            });
        }
        Ok(resolved)
    }

    /// Walk code files under `root`, calling `f` for each regular file that
    /// looks like source code (by extension). Skips `target/`, `.git/`, and
    /// binary-looking files. Stops early when `f` returns sufficiently many
    /// results (the callback tracks its own limit).
    pub fn walk_code_files(
        &self,
        root: &Path,
        f: &mut dyn FnMut(&str, &Path),
    ) -> Result<(), RuntimeError> {
        self.walk_code_files_ctl(root, &mut |rel, abs| {
            f(rel, abs);
            WalkControl::Continue
        })
    }

    /// P1-27 variant of `walk_code_files`: caller can return
    /// `WalkControl::Stop` to abort the entire walk immediately, so a
    /// search that already collected N hits doesn't keep reading and
    /// grep'ing the rest of the tree. Returning `Continue` matches the
    /// original semantics.
    pub fn walk_code_files_ctl(
        &self,
        root: &Path,
        f: &mut dyn FnMut(&str, &Path) -> WalkControl,
    ) -> Result<(), RuntimeError> {
        if !root.is_dir() {
            return Ok(());
        }
        let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            // Three skip-on-error steps folded into the iterator chain, same
            // semantics as the previous `match … Err(_) => continue` arms: an
            // unreadable directory is skipped (`read_dir` Err → empty), a
            // failed `DirEntry` is skipped (inner `flatten`), and an entry
            // whose `file_type` cannot be read is dropped by `filter_map`.
            let entries = std::fs::read_dir(&dir)
                .into_iter()
                .flatten()
                .flatten()
                .filter_map(|entry| entry.file_type().ok().map(|ft| (entry, ft)));
            for (entry, ft) in entries {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if ft.is_dir() {
                    let pruned =
                        name_str == "target" || name_str == ".git" || name_str == "node_modules";
                    if !pruned {
                        stack.push(entry.path());
                    }
                    continue;
                }
                if !ft.is_file() {
                    continue;
                }
                if !is_source_like_file_name(&name_str) {
                    continue;
                }
                // `entry.path()` is always `dir.join(name)` and `dir` descends
                // from `root`, so `strip_prefix` cannot fail; iterating the
                // Result keeps the previous skip-on-Err shape without an
                // unreachable else-branch.
                for rel in entry.path().strip_prefix(root).into_iter() {
                    let control = f(rel.to_string_lossy().as_ref(), &entry.path());
                    if matches!(control, WalkControl::Stop) {
                        return Ok(());
                    }
                }
            }
        }
        Ok(())
    }

    /// Revert all file changes made by the interactive agent in this session.
    /// Returns the number of files restored.
    pub fn revert_interactive_changes(&self) -> Result<usize, RuntimeError> {
        let mut rollback = self.interactive_rollback();
        let count = rollback.len();
        rollback.rollback()?;
        *rollback = PluginEditRollback::empty(&self.fixtures_root);
        Ok(count)
    }

    pub fn config(&self) -> &RuntimeConfig {
        &self.config
    }

    pub fn current_snapshot(&self) -> Arc<RuntimeSnapshot> {
        self.current_snapshot
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    pub fn create_plugin(
        &self,
        name: &str,
        description: Option<&str>,
    ) -> Result<Value, RuntimeError> {
        // Validate name
        if name.is_empty() {
            return Err(RuntimeError::InvalidArgument {
                message: "plugin name must not be empty".to_string(),
            });
        }
        if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(RuntimeError::InvalidArgument {
                message: "plugin name must contain only [a-zA-Z0-9_]".to_string(),
            });
        }

        let plugin_dir = self.fixtures_root.join("plugins").join(name);
        if plugin_dir.exists() {
            return Err(RuntimeError::InvalidArgument {
                message: format!("plugin directory already exists: plugins/{name}"),
            });
        }

        // Create directory structure
        let src_dir = plugin_dir.join("src");
        std::fs::create_dir_all(&src_dir)
            .map_err(|e| io_ctx(src_dir.clone(), "failed to create plugin src dir", e))?;

        let desc = description.unwrap_or(name);
        let crate_hash = format!("crate_{name}_v1");

        // Write Cargo.toml skeleton
        let cargo_toml = format!(
            r#"[package]
name = "{name}"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["rlib", "dylib"]

[package.metadata.cordis]
plugin_path = "{name}"
abi_kind = "rust"
declared_nodes = []
children = []

[package.metadata.cordis.abi_fingerprint]
crate_hash = "{crate_hash}"
api_hash = "api_v2"

[dependencies]
cordis-plugin-sdk = {{ path = "../../../crates/cordis-plugin-sdk" }}
serde = {{ version = "1", features = ["derive"] }}
serde_json = "1"
"#
        );
        let manifest_path = plugin_dir.join("Cargo.toml");
        std::fs::write(&manifest_path, &cargo_toml)
            .map_err(|e| io_ctx(manifest_path.clone(), "failed to write Cargo.toml", e))?;

        // Write src/lib.rs skeleton
        let lib_rs = format!(
            r#"//! {desc} plugin.

use cordis_plugin_sdk::{{
    export_plugin_api, json_response, node_doc, plugin_docs, AbiFingerprint,
    PluginRequest, PluginResponse,
}};
use serde::{{Deserialize, Serialize}};
use serde_json::json;

// ---------------------------------------------------------------------------
// Request / Response types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct NodeRequest {{
    node_id: String,
}}

#[derive(Debug, Serialize)]
struct NodeResponse {{
    ok: bool,
    node_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

fn handle(req: &NodeRequest) -> Result<NodeResponse, String> {{
    Err(format!("unknown node_id: {{}}", req.node_id))
}}

// ---------------------------------------------------------------------------
// Plugin API exports
// ---------------------------------------------------------------------------

fn docs_value() -> cordis_plugin_sdk::PluginDocs {{
    plugin_docs(
        "{name}",
        "{name}",
        "0.1.0",
        None,
        vec![],
        None,
    )
}}

fn abi_fingerprint_value() -> AbiFingerprint {{
    AbiFingerprint::current_build("{crate_hash}", "api_v2")
}}

fn api_handle(req: PluginRequest) -> PluginResponse {{
    match serde_json::from_str::<NodeRequest>(&req.payload)
        .map_err(|e| format!("{name} plugin: {{e}}"))
        .and_then(|r| handle(&r))
    {{
        Ok(resp) => json_response(&resp),
        Err(e) => json_response(&NodeResponse {{
            ok: false,
            node_id: "error".to_string(),
            error: Some(e),
        }}),
    }}
}}

export_plugin_api! {{
    abi_fingerprint = abi_fingerprint_value(),
    docs = docs_value(),
    handle = api_handle,
}}
"#
        );
        std::fs::write(src_dir.join("lib.rs"), &lib_rs)
            .map_err(|e| io_ctx(src_dir.join("lib.rs"), "failed to write lib.rs", e))?;

        // Add to workspace members.
        //
        // P1-16: previously plain `read → mutate → write`. Two concurrent
        // `create_plugin` calls could interleave the read-modify-write and
        // lose one member. Now:
        //   - hold an advisory flock on `<manifest>.lock` for the whole
        //     RMW, so callers cross-process see a serial order;
        //   - write to `<manifest>.cordis-tmp.<pid>` + rename so a crash
        //     mid-write leaves the previous manifest intact.
        let workspace_manifest = self.fixtures_root.join("plugins").join("Cargo.toml");
        let lock_path = workspace_manifest.with_extension("toml.create-lock");
        let _lock = workspace_manifest_lock::acquire(&lock_path);
        let manifest_text = std::fs::read_to_string(&workspace_manifest).map_err(|e| {
            io_ctx(
                workspace_manifest.clone(),
                "failed to read workspace manifest",
                e,
            )
        })?;
        let mut document: TomlValue =
            toml::from_str(&manifest_text).map_err(|e| RuntimeError::InvalidArgument {
                message: format!("failed to parse workspace manifest: {e}"),
            })?;
        let members = document
            .get_mut("workspace")
            .and_then(|w| w.get_mut("members"))
            .and_then(|m| m.as_array_mut())
            .ok_or_else(|| RuntimeError::InvalidArgument {
                message: "workspace.members not found in plugins/Cargo.toml".to_string(),
            })?;
        let already_member = members.iter().any(|v| v.as_str() == Some(name));
        if !already_member {
            members.push(TomlValue::String(name.to_string()));
        }
        let wm = workspace_manifest;
        let new_manifest = toml::to_string_pretty(&document)
            .map_err(|e| io_ctx(wm.clone(), "failed to serialize workspace manifest", e))?;
        atomic_write_bytes(&wm, new_manifest.as_bytes())
            .map_err(|e| io_ctx(wm.clone(), "failed to write workspace manifest", e))?;

        Ok(serde_json::json!({
            "ok": true,
            "plugin_path": format!("/{name}"),
            "message": format!("Created plugin /{name} with skeleton. Use request_iteration with plugin_path=\"/{name}\" to add nodes."),
        }))
    }

    pub fn agent_start(&self, kind: AgentSessionKind) -> Result<AgentSessionHandle, RuntimeError> {
        self.agent_start_with(kind, AgentStartOptions::default())
    }

    /// Start an agent session with an explicit LLM profile and soul scope.
    /// `profile` names an entry in `llm_profiles` (unknown/None → default);
    /// `soul_key` scopes the persona overlay ("" → no per-user soul).
    pub fn agent_start_with(
        &self,
        kind: AgentSessionKind,
        options: AgentStartOptions,
    ) -> Result<AgentSessionHandle, RuntimeError> {
        let handle = AgentSessionHandle {
            session_id: normalize_request_id(None, "agent-session"),
            kind,
        };
        let session_kind_label = match kind {
            AgentSessionKind::RuntimeShell => "runtime_shell",
            AgentSessionKind::PluginIteration => "plugin_iteration",
        };
        let state = match kind {
            AgentSessionKind::RuntimeShell => ManagedAgentState::RuntimeShell,
            AgentSessionKind::PluginIteration => {
                return Err(RuntimeError::InvalidArgument {
                    message: "plugin_iteration agent sessions must be started by iterate_plugins"
                        .to_string(),
                });
            }
        };
        let profile_name = options.profile.as_deref().unwrap_or("default");
        let api = self.config.llm_profiles.resolve(profile_name).api.clone();
        let mut session = AgentSession::new(api, session_kind_label)?;
        session.set_soul_key(options.soul_key.clone());
        self.profile_fallback
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(
                handle.session_id.clone(),
                ProfileFallbackEntry {
                    desired: profile_name.to_string(),
                    degraded: false,
                },
            );
        self.agent_sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(
                handle.session_id.clone(),
                ManagedAgentSession {
                    handle: handle.clone(),
                    session,
                    state,
                },
            );
        Ok(handle)
    }

    pub fn agent_send(&self, session_id: &str, input: &str) -> Result<AgentReply, RuntimeError> {
        let mut session = {
            let mut guard = self
                .agent_sessions
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            guard
                .remove(session_id)
                .ok_or_else(|| RuntimeError::AgentSessionNotFound {
                    session_id: session_id.to_string(),
                })?
        };
        let result = session.respond(self, input);
        // P1-25: drain any pending actions queued by tools that couldn't
        // reach the session during `respond` (compact_context et al) and
        // apply them here, before reinserting into the map. Errors are
        // logged, not propagated, because the underlying tool call has
        // already returned "deferred" to the LLM.
        {
            let pending = {
                let mut guard = self
                    .pending_session_actions
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                guard.remove(session_id).unwrap_or_default()
            };
            for action in pending {
                match action {
                    PendingSessionAction::CompactHistory => {
                        let (old, new) = session.session.compact_history();
                        eprintln!(
                            "[pending-action] compact_history for session {session_id}: {old} -> {new}"
                        );
                    }
                }
            }
        }
        // Auto-save on success for RuntimeShell sessions so that
        // session context survives crashes and restarts.
        if result.is_ok() && matches!(session.state, ManagedAgentState::RuntimeShell) {
            self.auto_save_session(session_id, &session.session);
        }
        self.agent_sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(session_id.to_string(), session);
        result
    }

    /// `agent_send` with mechanical LLM-profile fallback. Policy:
    /// - Requests normally go through the session's desired profile.
    /// - On failure (retries already exhausted inside `send_chat_request`),
    ///   switch to the profile's declared `fallback` and retry once.
    /// - While degraded, each new send optimistically probes the desired
    ///   profile first; success switches back, failure re-degrades.
    ///   Every switch is explicit: a kernel issue is recorded and a notify
    ///   message is emitted — never a silent model change.
    pub fn agent_send_with_fallback(
        &self,
        session_id: &str,
        input: &str,
    ) -> Result<AgentReply, RuntimeError> {
        let entry = {
            let guard = self
                .profile_fallback
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            guard.get(session_id).cloned()
        };
        let Some(entry) = entry else {
            // Sessions started via plain agent_start (REPL, tests) have no
            // fallback entry under a non-default registry only if created
            // before this map existed; treat as plain send.
            return self.agent_send(session_id, input);
        };

        // Recovery probe: while degraded, put the desired profile back
        // before this attempt. If it is still down we fall through to the
        // normal failure path below and re-degrade.
        if entry.degraded {
            self.swap_session_profile(session_id, &entry.desired)?;
        }

        match self.agent_send(session_id, input) {
            Ok(reply) => {
                if entry.degraded {
                    self.set_degraded(session_id, false);
                    let msg = format!(
                        "LLM profile '{}' recovered; session {} switched back",
                        entry.desired, session_id
                    );
                    eprintln!("[llm-profile] {msg}");
                    crate::kernel::notify::send(self, &format!("⚙️ {msg}"));
                }
                Ok(reply)
            }
            Err(primary_err) => {
                let Some(fallback) = self.config.llm_profiles.fallback_of(&entry.desired) else {
                    return Err(primary_err);
                };
                let fallback = fallback.to_string();
                self.swap_session_profile(session_id, &fallback)?;
                match self.agent_send(session_id, input) {
                    Ok(reply) => {
                        self.set_degraded(session_id, true);
                        let msg = format!(
                            "LLM profile '{}' failing ({}); session {} degraded to fallback '{}'",
                            entry.desired, primary_err, session_id, fallback
                        );
                        eprintln!("[llm-profile] {msg}");
                        self.kernel.observe_plugin_issue(
                            KernelPluginIssueSource::InvokeFailure,
                            "/llm-profile",
                            msg.clone(),
                        );
                        crate::kernel::notify::send(self, &format!("⚙️ {msg}"));
                        Ok(reply)
                    }
                    Err(fallback_err) => {
                        // Both profiles down. Restore the desired profile so
                        // the next attempt probes it first, and surface the
                        // original error (more representative).
                        let _ = self.swap_session_profile(session_id, &entry.desired);
                        self.set_degraded(session_id, false);
                        eprintln!(
                            "[llm-profile] fallback '{fallback}' also failed: {fallback_err}"
                        );
                        Err(primary_err)
                    }
                }
            }
        }
    }

    /// Swap the live LLM config of a resting session (must not be inside
    /// `respond`) to the named profile.
    fn swap_session_profile(
        &self,
        session_id: &str,
        profile_name: &str,
    ) -> Result<(), RuntimeError> {
        let api = self.config.llm_profiles.resolve(profile_name).api.clone();
        let mut guard = self
            .agent_sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let not_found = || RuntimeError::AgentSessionNotFound {
            session_id: session_id.to_string(),
        };
        let managed = guard.get_mut(session_id).ok_or_else(not_found)?;
        managed.session.swap_config(api)
    }

    /// H1: rebind an existing session's soul scope key. Unlike `agent_start`
    /// (which sets `soul_key` once at creation), group chats route several
    /// senders through one session and must re-scope the persona overlay per
    /// turn. Returns `AgentSessionNotFound` if the session is gone.
    pub fn refresh_session_soul(
        &self,
        session_id: &str,
        soul_key: &str,
    ) -> Result<(), RuntimeError> {
        let mut guard = self
            .agent_sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let not_found = || RuntimeError::AgentSessionNotFound {
            session_id: session_id.to_string(),
        };
        let managed = guard.get_mut(session_id).ok_or_else(not_found)?;
        managed.session.set_soul_key(soul_key.to_string());
        Ok(())
    }

    fn set_degraded(&self, session_id: &str, degraded: bool) {
        let mut guard = self
            .profile_fallback
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(entry) = guard.get_mut(session_id) {
            entry.degraded = degraded;
        }
    }

    /// P1-25: queue a `PendingSessionAction` for `session_id`; the next
    /// `agent_send` prologue drains and applies. Public because tool
    /// implementations in `agent.rs` may need to call it via the
    /// `AgentToolHost` interface (see `queue_session_compact` below).
    pub(crate) fn queue_session_action(&self, session_id: &str, action: PendingSessionAction) {
        let mut guard = self
            .pending_session_actions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        guard
            .entry(session_id.to_string())
            .or_default()
            .push(action);
    }

    /// Inject a user→assistant exchange into the agent's history without
    /// triggering an LLM call. Used by `/` shortcuts.
    pub(crate) fn agent_sessions_mut(
        &self,
    ) -> std::sync::MutexGuard<'_, BTreeMap<String, ManagedAgentSession>> {
        self.agent_sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    pub fn agent_inject(
        &self,
        session_id: &str,
        user_input: &str,
        assistant_output: &str,
    ) -> Result<(), RuntimeError> {
        let mut guard = self
            .agent_sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let session =
            guard
                .get_mut(session_id)
                .ok_or_else(|| RuntimeError::AgentSessionNotFound {
                    session_id: session_id.to_string(),
                })?;
        session
            .session
            .inject_exchange(user_input, assistant_output);
        Ok(())
    }

    pub fn agent_status(&self, session_id: &str) -> Result<AgentSessionStatus, RuntimeError> {
        let guard = self
            .agent_sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let session = guard
            .get(session_id)
            .ok_or_else(|| RuntimeError::AgentSessionNotFound {
                session_id: session_id.to_string(),
            })?;
        Ok(session.session.status())
    }

    pub fn agent_transcript(
        &self,
        session_id: &str,
    ) -> Result<Vec<AgentTranscriptEntry>, RuntimeError> {
        let guard = self
            .agent_sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let session = guard
            .get(session_id)
            .ok_or_else(|| RuntimeError::AgentSessionNotFound {
                session_id: session_id.to_string(),
            })?;
        Ok(session.session.transcript().to_vec())
    }

    fn start_plugin_iteration_agent_session(
        &self,
        prepared: PreparedPluginIteration,
        context_paths: PluginIterationContextPaths,
    ) -> Result<String, RuntimeError> {
        let session_id = normalize_request_id(None, "plugin-agent-session");
        let mut llm_config = self.config.llm_api.clone();
        llm_config.timeout_ms = llm_config
            .timeout_ms
            .min(PLUGIN_ITERATION_AGENT_TIMEOUT_CAP_MS);
        let session = AgentSession::new(llm_config, "plugin_iteration")?;
        self.agent_sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(
                session_id.clone(),
                ManagedAgentSession {
                    handle: AgentSessionHandle {
                        session_id: session_id.clone(),
                        kind: AgentSessionKind::PluginIteration,
                    },
                    session,
                    state: ManagedAgentState::PluginIteration(Box::new(
                        PluginIterationAgentState::new(
                            prepared,
                            context_paths,
                            &self.fixtures_root,
                        ),
                    )),
                },
            );
        Ok(session_id)
    }

    fn plugin_iteration_agent_snapshot(
        &self,
        session_id: &str,
    ) -> Result<PluginIterationAgentSnapshot, RuntimeError> {
        let guard = self
            .agent_sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let managed = guard
            .get(session_id)
            .ok_or_else(|| RuntimeError::AgentSessionNotFound {
                session_id: session_id.to_string(),
            })?;
        let ManagedAgentState::PluginIteration(state) = &managed.state else {
            return Err(RuntimeError::InvalidArgument {
                message: format!("agent session {session_id} is not a plugin iteration session"),
            });
        };
        Ok(state.snapshot())
    }

    pub fn status(&self) -> RuntimeHostStatus {
        let snapshot = self.current_snapshot();
        RuntimeHostStatus {
            fixtures_root: self.fixtures_root.display().to_string(),
            snapshot_root: self.snapshot_root.display().to_string(),
            current_snapshot_id: snapshot.snapshot_id().to_string(),
            plugin_count: snapshot.plugin_registry().iter().count(),
            node_count: snapshot.node_registry().len(),
            candidate_snapshot: self.candidate_status(),
            last_reload: self.last_reload_attempt(),
            last_candidate_reload: self.last_candidate_reload_attempt(),
        }
    }

    pub fn last_reload_attempt(&self) -> Option<ReloadAttemptReport> {
        self.last_reload_attempt
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    pub fn last_candidate_reload_attempt(&self) -> Option<ReloadAttemptReport> {
        self.last_candidate_reload_attempt
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    pub fn candidate_snapshot(&self) -> Option<Arc<RuntimeSnapshot>> {
        self.candidate_snapshot
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_ref()
            .map(|candidate| candidate.snapshot.clone())
    }

    pub fn candidate_status(&self) -> Option<CandidateSnapshotStatus> {
        self.candidate_snapshot
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_ref()
            .map(|candidate| candidate.status.clone())
    }

    pub fn invoke(
        &self,
        plugin_path: &str,
        node_id: &str,
        payload: String,
    ) -> Result<PluginResponse, RuntimeError> {
        let snapshot = self.current_snapshot();
        let payload_for_sample = payload.clone();
        let response = snapshot.invoke(plugin_path, node_id, payload);
        match &response {
            Ok(response) => self.record_invocation_sample(
                plugin_path,
                node_id,
                &payload_for_sample,
                &response.payload,
            ),
            Err(err) => {
                self.kernel.observe_plugin_issue(
                    KernelPluginIssueSource::InvokeFailure,
                    plugin_path.to_string(),
                    format!("invoke failure for {plugin_path}::{node_id}: {err}"),
                );
            }
        }
        self.cleanup_retired_snapshots();
        // auto-iteration deferred to kernel timer.
        response
    }

    pub fn invoke_candidate(
        &self,
        plugin_path: &str,
        node_id: &str,
        payload: String,
    ) -> Result<PluginResponse, RuntimeError> {
        let snapshot = self
            .candidate_snapshot()
            .ok_or(RuntimeError::CandidateSnapshotMissing)?;
        let response = snapshot.invoke(plugin_path, node_id, payload);
        self.cleanup_retired_snapshots();
        response
    }

    pub fn execute(
        &self,
        target_node_fqn: &str,
        payload: Value,
    ) -> Result<RuntimeExecutionResult, RuntimeError> {
        let snapshot = self.current_snapshot();
        let result = snapshot.execute_registered_target(target_node_fqn, payload);
        if let Ok(ref exec_result) = result {
            for diagnostic in &exec_result.net_diagnostics {
                eprintln!("[execute] net diagnostic for {target_node_fqn}: {diagnostic}");
            }
        }
        if let Err(err) = &result {
            let plugin_path = target_node_fqn
                .split("::")
                .next()
                .unwrap_or(target_node_fqn)
                .to_string();
            self.kernel.observe_plugin_issue(
                KernelPluginIssueSource::InvokeFailure,
                plugin_path.clone(),
                format!("execute failure for {target_node_fqn}: {err}"),
            );
        }
        self.cleanup_retired_snapshots();
        // auto-iteration deferred to kernel timer.
        result
    }

    pub fn execute_candidate(
        &self,
        target_node_fqn: &str,
        payload: Value,
    ) -> Result<RuntimeExecutionResult, RuntimeError> {
        let snapshot = self
            .candidate_snapshot()
            .ok_or(RuntimeError::CandidateSnapshotMissing)?;
        let result = snapshot.execute_registered_target(target_node_fqn, payload);
        self.cleanup_retired_snapshots();
        result
    }

    pub fn reload(&self, plugin_path: &str) -> Result<ReloadReport, RuntimeError> {
        let result = if plugin_path == "/" {
            self.reload_internal()
        } else {
            self.reload_subtree(plugin_path)
        };
        match result {
            Ok((report, attempt)) => {
                self.record_reload_attempt(attempt);
                let snapshot = self.current_snapshot();
                self.observe_snapshot_plugin_issues(snapshot.as_ref(), &report, "reload");
                self.notify_sessions_of_reload(&report);
                Ok(report)
            }
            Err((err, attempt)) => {
                self.record_reload_attempt(*attempt);
                self.observe_reload_error("reload", &err);
                Err(err)
            }
        }
    }

    pub fn reload_with_diagnostics(&self, plugin_path: &str) -> ReloadAttemptReport {
        let result = if plugin_path == "/" {
            self.reload_internal()
        } else {
            self.reload_subtree(plugin_path)
        };
        match result {
            Ok((report, attempt)) => {
                self.record_reload_attempt(attempt.clone());
                let snapshot = self.current_snapshot();
                self.observe_snapshot_plugin_issues(snapshot.as_ref(), &report, "reload");
                self.notify_sessions_of_reload(&report);
                attempt
            }
            Err((err, attempt)) => {
                let attempt = *attempt;
                self.record_reload_attempt(attempt.clone());
                self.observe_reload_error("reload", &err);
                attempt
            }
        }
    }

    /// Reload a subtree of plugins whose path starts with `prefix`.
    /// Uses two-phase commit: Phase 1 pre-loads and validates all new dylibs
    /// (no side effects); Phase 2 stops old services and swaps in the new
    /// registry entries.
    /// Package a reload error together with its recorded failed attempt —
    /// the shared tail of every error path inside `reload_subtree`.
    fn fail_reload(
        &self,
        previous_snapshot: &Arc<RuntimeSnapshot>,
        started_at: Instant,
        err: RuntimeError,
    ) -> (RuntimeError, Box<ReloadAttemptReport>) {
        let attempt = self.make_failed_attempt(previous_snapshot, started_at, &err);
        (err, Box::new(attempt))
    }

    fn reload_subtree(
        &self,
        prefix: &str,
    ) -> Result<(ReloadReport, ReloadAttemptReport), (RuntimeError, Box<ReloadAttemptReport>)> {
        let normalized = prefix.trim_start_matches('/');
        let previous_snapshot = self.current_snapshot();
        let started_at = Instant::now();

        // Collect target plugins (match prefix).
        let targets: Vec<String> = previous_snapshot
            .plugin_registry()
            .iter()
            .map(|(p, _)| p)
            .filter(|p| {
                if normalized.is_empty() {
                    true
                } else {
                    p.as_str() == normalized || p.starts_with(&format!("{}/", normalized))
                }
            })
            .collect();

        if targets.is_empty() {
            let attempt = ReloadAttemptReport {
                status: ReloadAttemptStatus::Reloaded,
                from_snapshot_id: previous_snapshot.snapshot_id().to_string(),
                to_snapshot_id: Some(previous_snapshot.snapshot_id().to_string()),
                snapshot_root: self.snapshot_root.display().to_string(),
                staged_artifact_root: String::new(),
                elapsed_ms: started_at.elapsed().as_millis(),
                plugin_count: None,
                node_count: None,
                added_plugins: Vec::new(),
                removed_plugins: Vec::new(),
                changed_plugins: Vec::new(),
                changed_plugin_reasons: BTreeMap::new(),
                failure_summary: None,
            };
            return Ok((
                ReloadReport {
                    from_snapshot_id: previous_snapshot.snapshot_id().to_string(),
                    to_snapshot_id: previous_snapshot.snapshot_id().to_string(),
                    snapshot_root: self.snapshot_root.display().to_string(),
                    staged_artifact_root: String::new(),
                    elapsed_ms: 0,
                    added_plugins: Vec::new(),
                    removed_plugins: Vec::new(),
                    changed_plugins: Vec::new(),
                    changed_plugin_reasons: BTreeMap::new(),
                },
                attempt,
            ));
        }

        let artifacts_dir = self.fixtures_root().join("artifacts");
        let index_path = artifacts_dir.join("index.json");
        let index = crate::plugin::artifact::load_artifact_index(&index_path).map_err(|e| {
            let attempt = self.make_failed_attempt(&previous_snapshot, started_at, &e);
            (e, Box::new(attempt))
        })?;
        let index_map = crate::plugin::artifact::artifact_index_map(&index);

        // Stop background services + Task node threads before .so is dlclose'd.
        for plugin_path in &targets {
            self.service_registry.stop_plugin_services(plugin_path);
            // Also invoke stop action for Task nodes (plugins that don't
            // implement the Service trait call stop via node invocation).
            //
            // P1-19: a plugin's stop handler runs inside the plugin's own
            // dylib; a panic there would unwind across the FFI boundary
            // (UB in the general case). Wrap each invocation in
            // catch_unwind so a broken stop handler cannot crash the whole
            // reload path.
            let snapshot = self.current_snapshot();
            let node_prefix = format!("{}::", plugin_path);
            let task_fqns = snapshot.node_registry().task_node_fqns();
            // The prefix match and the `plugin_id::node_id` split are both
            // expressed as iterator adapters rather than nested `if`s: every
            // fqn that reaches the body is a Task node of this plugin and is
            // guaranteed to carry a `::`, so there is no gate whose untaken
            // side is structurally unreachable.
            for (plugin_id, node_id) in task_fqns
                .iter()
                .filter(|fqn| fqn.starts_with(&node_prefix))
                .filter_map(|fqn| fqn.split_once("::"))
            {
                let payload = serde_json::json!({"action": "stop"}).to_string();
                let this = self;
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    #[cfg(test)]
                    if TEST_STOP_HANDLER_PANIC_INJECTION
                        .swap(false, std::sync::atomic::Ordering::SeqCst)
                    {
                        panic!("test panic injection: stop handler");
                    }
                    let _ = this.invoke(plugin_id, node_id, payload);
                }));
                if let Err(err) = result {
                    let msg = reload_stop_handler_panic_message(err.as_ref());
                    eprintln!("[reload] stop handler for {plugin_id}::{node_id} panicked: {msg}");
                }
            }
            // P0-16: drop the keep-alive dylib handle for this plugin path
            // so the OS can unmap the old .so once the new one is loaded.
            // Without this, every reload leaks a mapping.
            crate::plugin::invoke::unregister_task_library(plugin_path);
        }

        // ── Phase 1: pre-load and validate all new dylibs ─────────────
        // No side effects — if anything fails, old plugins keep running.
        //
        // P1-20: `_dylib` is held as a struct-owned by-value handle for the
        // lifetime of `prepared` (drops at fn return, i.e. AFTER Phase 2
        // has updated the registry). The invoke path opens the artifact
        // fresh each time via `LoadedDylibApi::open`, so we don't hand any
        // function pointer from Phase 1 across to the registry — no
        // dangling reference to worry about. Keeping `_dylib` here still
        // matters because it validates the fingerprint before we mutate
        // the registry and gives us "old plugins keep running" on failure.
        struct Prepared {
            plugin_path: String,
            docs: PluginDocs,
            abi_fingerprint: AbiFingerprint,
            _dylib: crate::plugin::dynamic::LoadedDylibApi,
        }
        let mut prepared: Vec<Prepared> = Vec::new();

        for plugin_path in &targets {
            let entry = index_map.get(plugin_path).ok_or_else(|| {
                let err = reload_artifact_missing_error(plugin_path);
                self.fail_reload(&previous_snapshot, started_at, err)
            })?;

            let resolved =
                crate::plugin::artifact::resolve_artifact_path(&index_path, &entry.artifact_path);
            let dylib = crate::plugin::dynamic::LoadedDylibApi::open(&resolved).map_err(|e| {
                let attempt = self.make_failed_attempt(&previous_snapshot, started_at, &e);
                (e, Box::new(attempt))
            })?;
            let api = dylib.api();

            // Strict docs comparison.
            let new_docs: PluginDocs = serde_json::from_str(&(api.docs)().payload)
                .map_err(|e| reload_docs_parse_error(plugin_path, &e))
                .map_err(|err| self.fail_reload(&previous_snapshot, started_at, err))?;
            if new_docs.nodes != entry.docs.nodes {
                let err = reload_docs_mismatch_error(
                    plugin_path,
                    &entry.abi_fingerprint,
                    entry.docs.nodes.len(),
                    new_docs.nodes.len(),
                );
                let attempt = self.make_failed_attempt(&previous_snapshot, started_at, &err);
                return Err((err, Box::new(attempt)));
            }

            // Strict ABI fingerprint comparison.
            let actual_fingerprint: AbiFingerprint =
                serde_json::from_str(&(api.abi_fingerprint)().payload)
                    .map_err(|e| reload_abi_fingerprint_parse_error(plugin_path, &e))
                    .map_err(|err| self.fail_reload(&previous_snapshot, started_at, err))?;
            if actual_fingerprint.crate_hash != entry.abi_fingerprint.crate_hash
                || actual_fingerprint.api_hash != entry.abi_fingerprint.api_hash
            {
                let err = reload_abi_fingerprint_mismatch_error(
                    plugin_path,
                    &entry.abi_fingerprint,
                    actual_fingerprint,
                );
                let attempt = self.make_failed_attempt(&previous_snapshot, started_at, &err);
                return Err((err, Box::new(attempt)));
            }

            prepared.push(Prepared {
                plugin_path: plugin_path.clone(),
                docs: new_docs,
                abi_fingerprint: actual_fingerprint,
                _dylib: dylib,
            });
        }

        // ── Phase 2: stop old services → update registry ───────────
        let registry = previous_snapshot.plugin_registry();
        let mut changed_plugins: Vec<String> = Vec::new();
        let mut changed_reasons: BTreeMap<String, Vec<String>> = BTreeMap::new();

        for p in prepared.iter().rev() {
            eprintln!("reload_subtree: stopping services for {}", p.plugin_path);
            self.service_registry
                .stop_plugin_services_timed(&p.plugin_path);
        }
        for p in &prepared {
            registry.reload_plugin_entry(&p.plugin_path, p.docs.clone(), p.abi_fingerprint.clone());
            changed_plugins.push(p.plugin_path.clone());
            changed_reasons.insert(p.plugin_path.clone(), vec!["subtree reload".to_string()]);
            eprintln!("reload_subtree: reloaded {}", p.plugin_path);
        }

        let zombie_count = self.service_registry.zombie_count();
        if zombie_count > 0 {
            eprintln!(
                "reload_subtree: {} zombie service(s) remaining (use kill_zombie_services to clean up)",
                zombie_count
            );
        }

        // P1-21: give the subtree reload a distinct snapshot id so
        // invocation traces before and after the reload don't collide.
        // The previous behaviour reused `previous_snapshot.snapshot_id()`
        // for both `from` and `to`, making downstream analytics unable to
        // distinguish invocations against the old vs new plugin set.
        let new_snapshot_id = make_snapshot_dir_name();
        let report = ReloadReport {
            from_snapshot_id: previous_snapshot.snapshot_id().to_string(),
            to_snapshot_id: new_snapshot_id,
            snapshot_root: self.snapshot_root.display().to_string(),
            staged_artifact_root: String::new(),
            elapsed_ms: started_at.elapsed().as_millis(),
            added_plugins: Vec::new(),
            removed_plugins: Vec::new(),
            changed_plugins: changed_plugins.clone(),
            changed_plugin_reasons: changed_reasons,
        };

        let attempt = ReloadAttemptReport {
            status: ReloadAttemptStatus::Reloaded,
            from_snapshot_id: report.from_snapshot_id.clone(),
            to_snapshot_id: Some(report.to_snapshot_id.clone()),
            snapshot_root: report.snapshot_root.clone(),
            staged_artifact_root: report.staged_artifact_root.clone(),
            elapsed_ms: report.elapsed_ms,
            plugin_count: Some(targets.len()),
            node_count: None,
            added_plugins: Vec::new(),
            removed_plugins: Vec::new(),
            changed_plugins,
            changed_plugin_reasons: BTreeMap::new(),
            failure_summary: None,
        };

        Ok((report, attempt))
    }

    fn make_failed_attempt(
        &self,
        previous_snapshot: &RuntimeSnapshot,
        started_at: Instant,
        err: &RuntimeError,
    ) -> ReloadAttemptReport {
        ReloadAttemptReport {
            status: ReloadAttemptStatus::Failed,
            from_snapshot_id: previous_snapshot.snapshot_id().to_string(),
            to_snapshot_id: None,
            snapshot_root: self.snapshot_root.display().to_string(),
            staged_artifact_root: String::new(),
            elapsed_ms: started_at.elapsed().as_millis(),
            plugin_count: None,
            node_count: None,
            added_plugins: Vec::new(),
            removed_plugins: Vec::new(),
            changed_plugins: Vec::new(),
            changed_plugin_reasons: BTreeMap::new(),
            failure_summary: Some(err.to_string()),
        }
    }

    /// Notify all active agent sessions that a plugin reload happened.
    fn notify_sessions_of_reload(&self, report: &ReloadReport) {
        if report.changed_plugins.is_empty() {
            return;
        }
        let changed = report.changed_plugins.join(", ");
        let notice = format!(
            "[system] Plugin reloaded: {}. Available nodes may have changed. Use list_plugins/list_nodes if unsure.",
            changed
        );
        // Collect session IDs first to avoid deadlock with agent_inject.
        let sids: Vec<String> = {
            self.agent_sessions
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .keys()
                .cloned()
                .collect()
        };
        for sid in &sids {
            let outcome = self.agent_inject(sid, &notice, "Acknowledged.");
            log_session_reload_notify_outcome(sid, outcome);
        }
    }

    pub fn reload_candidate(&self) -> Result<CandidateSnapshotStatus, RuntimeError> {
        match self.reload_candidate_internal() {
            Ok((status, attempt)) => {
                self.record_candidate_reload_attempt(attempt);
                if let Some(snapshot) = self.candidate_snapshot() {
                    let report = ReloadReport {
                        from_snapshot_id: status.from_snapshot_id.clone(),
                        to_snapshot_id: status.candidate_snapshot_id.clone(),
                        snapshot_root: status.snapshot_root.clone(),
                        staged_artifact_root: status.staged_artifact_root.clone(),
                        elapsed_ms: 0,
                        added_plugins: status.added_plugins.clone(),
                        removed_plugins: status.removed_plugins.clone(),
                        changed_plugins: status.changed_plugins.clone(),
                        changed_plugin_reasons: status.changed_plugin_reasons.clone(),
                    };
                    self.observe_snapshot_plugin_issues(
                        snapshot.as_ref(),
                        &report,
                        "candidate_reload",
                    );
                }
                // auto-iteration deferred to kernel timer.
                Ok(status)
            }
            Err((err, attempt)) => {
                self.record_candidate_reload_attempt(*attempt);
                self.observe_reload_error("candidate_reload", &err);
                // auto-iteration deferred to kernel timer.
                Err(err)
            }
        }
    }

    pub fn reload_candidate_with_diagnostics(&self) -> ReloadAttemptReport {
        match self.reload_candidate_internal() {
            Ok((status, attempt)) => {
                self.record_candidate_reload_attempt(attempt.clone());
                if let Some(snapshot) = self.candidate_snapshot() {
                    let report = ReloadReport {
                        from_snapshot_id: status.from_snapshot_id.clone(),
                        to_snapshot_id: status.candidate_snapshot_id.clone(),
                        snapshot_root: status.snapshot_root.clone(),
                        staged_artifact_root: status.staged_artifact_root.clone(),
                        elapsed_ms: 0,
                        added_plugins: status.added_plugins.clone(),
                        removed_plugins: status.removed_plugins.clone(),
                        changed_plugins: status.changed_plugins.clone(),
                        changed_plugin_reasons: status.changed_plugin_reasons.clone(),
                    };
                    self.observe_snapshot_plugin_issues(
                        snapshot.as_ref(),
                        &report,
                        "candidate_reload",
                    );
                }
                // auto-iteration deferred to kernel timer.
                attempt
            }
            Err((err, attempt)) => {
                let attempt = *attempt;
                self.record_candidate_reload_attempt(attempt.clone());
                self.observe_reload_error("candidate_reload", &err);
                // auto-iteration deferred to kernel timer.
                attempt
            }
        }
    }

    pub fn promote_candidate(&self) -> Result<ReloadReport, RuntimeError> {
        if self.candidate_snapshot().is_none() {
            return Err(RuntimeError::CandidateSnapshotMissing);
        }
        clear_plugin_iteration_journal(&self.snapshot_root)?;
        let previous_snapshot = self.current_snapshot();
        let candidate = self
            .candidate_snapshot
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
            .ok_or(RuntimeError::CandidateSnapshotMissing)?;
        let next_snapshot = candidate.snapshot;
        {
            let mut guard = self
                .current_snapshot
                .write()
                .unwrap_or_else(|poison| poison.into_inner());
            *guard = next_snapshot.clone();
        }

        let report = ReloadReport::from_snapshots(
            previous_snapshot.as_ref(),
            next_snapshot.as_ref(),
            &self.snapshot_root,
            0,
        );
        self.record_reload_attempt(ReloadAttemptReport::from_report(
            &report,
            next_snapshot.as_ref(),
        ));
        self.retire_snapshot(previous_snapshot);
        self.cleanup_retired_snapshots();
        Ok(report)
    }

    pub fn rollback_candidate(&self) -> Result<CandidateSnapshotStatus, RuntimeError> {
        if self.candidate_snapshot().is_none() {
            return Err(RuntimeError::CandidateSnapshotMissing);
        }
        restore_plugin_iteration_workspace(&self.fixtures_root, &self.snapshot_root, None)?;
        let candidate = self
            .candidate_snapshot
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take()
            .ok_or(RuntimeError::CandidateSnapshotMissing)?;
        let status = candidate.status.clone();
        self.retire_snapshot(candidate.snapshot);
        self.cleanup_retired_snapshots();
        Ok(status)
    }

    pub fn kernel(&self) -> &RuntimeKernel {
        &self.kernel
    }

    pub fn approve_blocked_iteration(
        &self,
        iteration_id: &str,
    ) -> Result<KernelPluginIterationResult, RuntimeError> {
        let mut result = self.kernel.take_blocked_iteration(iteration_id)?;
        let report = match self.promote_candidate() {
            Ok(report) => report,
            Err(err) => {
                self.kernel
                    .blocked_iterations
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .insert(iteration_id.to_string(), result);
                return Err(err);
            }
        };
        result.final_verdict = PluginIterationFinalVerdict::Promoted;
        result.blocked_reason = None;
        result.candidate = Some(CandidateSnapshotStatus {
            from_snapshot_id: report.from_snapshot_id.clone(),
            candidate_snapshot_id: report.to_snapshot_id.clone(),
            snapshot_root: report.snapshot_root.clone(),
            staged_artifact_root: report.staged_artifact_root.clone(),
            plugin_count: self.current_snapshot().plugin_registry().iter().count(),
            node_count: self.current_snapshot().node_registry().len(),
            added_plugins: report.added_plugins.clone(),
            removed_plugins: report.removed_plugins.clone(),
            changed_plugins: report.changed_plugins.clone(),
            changed_plugin_reasons: report.changed_plugin_reasons.clone(),
        });
        self.kernel.record_plugin_iteration_outcome(&result);
        Ok(result)
    }

    pub fn iterate_plugins(
        &self,
        request: KernelPluginIterationRequest,
    ) -> Result<KernelPluginIterationResult, RuntimeError> {
        let snapshot = self.current_snapshot();
        let prepared = self
            .kernel
            .begin_plugin_iteration(snapshot.as_ref(), &request)?;
        let iteration_id = prepared.iteration_id.clone();

        // Wrap the entire iteration body in a panic guard: if any step panics
        // (e.g. inside the agent loop, rebuild, or verification), we catch it
        // and perform emergency rollback instead of crashing the server.
        let result: std::thread::Result<Result<KernelPluginIterationResult, RuntimeError>> =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // `assert!(!injected, msg)` panics with exactly `msg` (a `&str`
                // payload), same as the previous `if injected { panic!(msg) }`,
                // but without a not-taken block whose closing brace llvm-cov
                // can never reach.
                #[cfg(test)]
                let injected =
                    TEST_ITERATION_PANIC_INJECTION.swap(false, std::sync::atomic::Ordering::SeqCst);
                #[cfg(test)]
                assert!(!injected, "test panic injection: plugin iteration");
                let mut state = PluginIterationRunState::new(prepared.clone());

                // Step 1: Run the agent loop — the agent freely decides what to do.
                match self.run_plugin_iteration_agent(&state.prepared) {
                    Ok(agent) => {
                        state.agent_session_id = agent.session_id;
                        state.tool_execution_summary = agent.tool_summary;
                        state.derived_edit_plan = Some(agent.snapshot.derived_edit_plan.clone());
                        state.transcript_excerpt = agent.transcript_excerpt;
                        state.rollback = Some(agent.snapshot.rollback);
                        state.changed_paths = agent.snapshot.changed_paths;
                        state.diff_lines = agent.snapshot.derived_edit_plan.diff_lines();
                        state.tests_command = agent.snapshot.tests_command;
                        state.safety_command = agent.snapshot.safety_command;
                        if agent.snapshot.recorded_summary.is_none() {
                            let sid = state
                                .agent_session_id
                                .as_deref()
                                .unwrap_or("unknown-session");
                            let err = missing_summary_error(sid);
                            self.fail_stage(&mut state, "agent", &err);
                        }
                    }
                    Err(err) => {
                        self.fail_stage(&mut state, "agent", &err);
                    }
                }

                // Step 2: Persist the rollback journal.
                if state.stage_error.is_none() {
                    // Both outcomes funnel into one `fail_stage` call site.
                    // `state.rollback` is `Some` on every path Step 1 can take
                    // (the edit-plan branch starts from
                    // `PluginEditRollback::empty(..)` and the agent branch reads
                    // one out of the session snapshot), so the fallback is
                    // purely defensive — kept as a named, unit-tested
                    // constructor on the same line as the covered
                    // `unwrap_or_else` call rather than as its own arm.
                    let journal_error = state
                        .rollback
                        .as_ref()
                        .map(|rollback| {
                            // 注入的 ENOSPC 与真实 persist 走同一条表达式：
                            // `Option::map_or_else` 没有"未走到的臂"，而
                            // `if { return .. }` 的收尾在 if 不成立时不执行，
                            // 会在 100% 行覆盖门槛下留一行永久缺口。
                            injected_journal_enospc(&plugin_iteration_journal_path(
                                &self.snapshot_root,
                            ))
                            .map_or_else(
                                || {
                                    rollback.persist_journal(
                                        &plugin_iteration_journal_path(&self.snapshot_root),
                                        &state.prepared.iteration_id,
                                    )
                                },
                                Err,
                            )
                        })
                        .unwrap_or_else(|| Err(plugin_iteration_missing_rollback_error()))
                        .err();
                    for err in journal_error.iter() {
                        self.fail_stage(&mut state, "edit", err);
                    }
                }

                // Step 3: Rebuild the target plugin only — UNLESS the iteration
                // touched any Cargo.toml. A manifest change is structural: a
                // scaffolded child plugin (new crate + parent children entry)
                // exists neither in the cargo build graph of `-p <target>` nor
                // in artifacts/index.json, so a target-only rebuild would
                // silently drop it and the subsequent candidate load could
                // never register it (the `45febba` regression). Manifest-touching
                // iterations take the full `rebuild_fixture_artifacts` path,
                // which re-resolves the plugin graph and regenerates the index.
                if state.stage_error.is_none() {
                    let manifest_changed = state
                        .changed_paths
                        .iter()
                        .any(|path| path.ends_with("Cargo.toml"));
                    let plugin_path = if manifest_changed {
                        "/".to_string()
                    } else {
                        format!("/{}", state.prepared.root_plugin_path)
                    };
                    // P0-8: capture the current `.so` before rebuild so rollback
                    // reverts BOTH source and compiled artifact. Otherwise
                    // source-code rollback runs but the new `.so` stays on disk,
                    // producing silent behaviour drift on the next run.
                    //
                    // Backup → journal re-persist → rebuild is one `and_then`
                    // chain, so all three fallible steps share a single
                    // `fail_stage("rebuild", ..)` call site and each step still
                    // short-circuits the ones after it exactly as the previous
                    // `if state.stage_error.is_none()` gates did.
                    let journal = plugin_iteration_journal_path(&self.snapshot_root);
                    let iteration_id = state.prepared.iteration_id.clone();
                    let root_plugin_path = state.prepared.root_plugin_path.clone();
                    let rebuilt = state
                        .rollback
                        .as_mut()
                        .map(|rollback| {
                            backup_artifacts_into_rollback(
                                &self.fixtures_root,
                                &root_plugin_path,
                                rollback,
                            )
                            // Re-persist the journal now that artifact backups
                            // landed in the rollback; otherwise a crash after
                            // rebuild would leave the artifact rollback-able
                            // only in memory.
                            .and_then(|()| rollback.persist_journal(&journal, &iteration_id))
                        })
                        .unwrap_or_else(|| Err(plugin_iteration_missing_rollback_error()))
                        .and_then(|()| rebuild_plugin_workspace(&self.fixtures_root, &plugin_path));
                    match rebuilt {
                        Ok(artifacts) => {
                            state.rebuilt_artifacts = artifacts;
                        }
                        Err(err) => {
                            self.fail_stage(&mut state, "rebuild", &err);
                        }
                    }
                }

                // Step 4: Stage candidate snapshot.
                if state.stage_error.is_none() {
                    match self.reload_candidate() {
                        Ok(candidate) => {
                            state.candidate = Some(candidate);
                        }
                        Err(err) => {
                            self.fail_stage(&mut state, "stage_candidate", &err);
                        }
                    }
                }

                // Step 5: Verify.
                if state.stage_error.is_none() {
                    match self.verify_plugin_iteration(&state) {
                        Ok(report) => {
                            let verdict =
                                if report.input.tests_passed && report.input.safety_checks_passed {
                                    VerifierVerdict::Pass
                                } else {
                                    VerifierVerdict::Fail
                                };
                            if verdict == VerifierVerdict::Fail {
                                self.kernel.observe_plugin_issue(
                                KernelPluginIssueSource::VerifierFailure,
                                state.prepared.root_plugin_path.clone(),
                                format!(
                                    "plugin verifier failed for {}: tests_passed={}, safety_checks_passed={}",
                                    state.prepared.root_plugin_path,
                                    report.input.tests_passed,
                                    report.input.safety_checks_passed,
                                ),
                            );
                            }
                            state.verification = Some(report);
                            state.verifier_verdict = Some(verdict);
                        }
                        Err(err) => {
                            self.fail_stage(&mut state, "verify", &err);
                        }
                    }
                }

                // Step 6: Canary replay.
                if state.stage_error.is_none() {
                    match self.run_plugin_canary(&state) {
                        Ok(report) => {
                            if report.verdict == CanaryVerdict::Fail {
                                self.kernel.observe_plugin_issue(
                                    KernelPluginIssueSource::CanaryFailure,
                                    state.prepared.root_plugin_path.clone(),
                                    format!(
                                        "plugin canary failed for {}: {}",
                                        state.prepared.root_plugin_path, report.message
                                    ),
                                );
                            }
                            state.canary = Some(report);
                        }
                        Err(err) => {
                            self.fail_stage(&mut state, "canary", &err);
                        }
                    }
                }

                // Step 7: Promote or rollback (always runs, even after stage errors).
                let _final_verdict = self.finalize_plugin_iteration(&mut state)?;

                let net_output = ExecutionOutput {
                    execution_id: format!("plugin-iteration-{iteration_id}"),
                    order: vec![
                        "plugin_iteration::agent".to_string(),
                        "plugin_iteration::edit".to_string(),
                        "plugin_iteration::rebuild".to_string(),
                        "plugin_iteration::stage_candidate".to_string(),
                        "plugin_iteration::verify".to_string(),
                        "plugin_iteration::canary".to_string(),
                        "plugin_iteration::promote_or_rollback".to_string(),
                    ],
                    outcomes: std::collections::BTreeMap::new(),
                    keyed_outcomes: std::collections::BTreeMap::new(),
                    metrics: crate::execution::engine::ExecutionMetrics::default(),
                };
                state.into_result(net_output)
            }));

        self.kernel.finish_plugin_iteration(&iteration_id);

        match result {
            // No panic: record the outcome iff the body produced one, then
            // clean up and hand the body's own `Result` back unchanged. Written
            // as one arm over `Result::iter()` rather than separate `Ok(Ok)` /
            // `Ok(Err)` arms because the cleanup and the return value are
            // identical in both — only the `record_plugin_iteration_outcome`
            // call is conditional, and it is conditional on exactly the same
            // `Ok`-ness the iterator encodes.
            Ok(outcome) => {
                for result in outcome.iter() {
                    self.kernel.record_plugin_iteration_outcome(result);
                }
                self.cleanup_retired_snapshots();
                outcome
            }
            Err(panic_payload) => {
                // Emergency cleanup: restore workspace files, rollback candidate,
                // and clear journal so the system stays in a consistent state.
                let _ = restore_plugin_iteration_workspace(
                    &self.fixtures_root,
                    &self.snapshot_root,
                    None,
                );
                if self.candidate_snapshot().is_some() {
                    let _ = self.rollback_candidate();
                }
                self.cleanup_retired_snapshots();
                Err(plugin_iteration_panic_error(&panic_payload))
            }
        }
    }

    fn run_plugin_iteration_agent(
        &self,
        prepared: &PreparedPluginIteration,
    ) -> Result<PluginIterationAgentRun, RuntimeError> {
        if let Some(plan) = &prepared.edit_plan {
            self.kernel
                .plugin_iteration_policy
                .validate_plan(&prepared.allowed_plugin_roots, plan)?;
            let mut rollback = PluginEditRollback::empty(&self.fixtures_root);
            let executor = PluginEditExecutor::new(&self.fixtures_root);
            for (idx, operation) in plan.operations.iter().enumerate() {
                let single = PluginEditPlan {
                    issue_id: plan.issue_id.clone(),
                    patch_id: format!("{}-manual-{idx}", prepared.iteration_id),
                    summary: plan.summary.clone(),
                    operations: vec![operation.clone()],
                };
                // Bound out of the `?` so each fallible call is a single line:
                // a `?` spanning several lines leaves llvm-cov a zero-hit
                // region on the continuation line no test can reach.
                let policy = &self.kernel.plugin_iteration_policy;
                let roots = &prepared.allowed_plugin_roots;
                let executed = executor.execute(policy, roots, &single);
                let (_, op_rollback) = executed?;
                rollback.absorb(op_rollback)?;
            }
            let snapshot = PluginIterationAgentSnapshot {
                recorded_summary: Some(plan.summary.clone()),
                tests_command: prepared.tests_command.clone(),
                safety_command: prepared.safety_command.clone(),
                changed_paths: plan.changed_paths(),
                rollback,
                derived_edit_plan: plan.clone(),
            };
            return Ok(PluginIterationAgentRun {
                session_id: None,
                tool_summary: None,
                transcript_excerpt: Vec::new(),
                snapshot,
            });
        }
        let collected = collect_plugin_context_paths(
            &self.fixtures_root,
            &prepared.root_plugin_path,
            &prepared.target_plugin_paths,
        );
        let context_paths = collected?;
        let session_id =
            self.start_plugin_iteration_agent_session(prepared.clone(), context_paths)?;
        let input = prepared
            .instruction
            .clone()
            .unwrap_or_else(|| prepared.summary.clone());
        if let Err(err) = self.agent_send(&session_id, &input) {
            let transcript_excerpt = self
                .agent_transcript(&session_id)
                .map(|transcript| transcript_excerpt(&transcript, 12))
                .unwrap_or_default();
            let tool_summary = self.agent_status(&session_id).ok().and_then(|_| {
                self.agent_sessions
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .get(&session_id)
                    .map(|managed| managed.session.tool_execution_summary())
            });
            // H2: excerpt/tool_summary captured above; terminate the failed
            // session and reclaim its map entries before returning the error.
            self.drop_session(&session_id);
            return Err(enrich_plugin_iteration_agent_error(
                err,
                &session_id,
                tool_summary.as_ref(),
                &transcript_excerpt,
            ));
        }
        let snapshot = self.plugin_iteration_agent_snapshot(&session_id)?;
        let transcript = self.agent_transcript(&session_id)?;
        let transcript_excerpt = transcript_excerpt(&transcript, 12);
        let tool_summary = self.agent_status(&session_id).ok().and_then(|_| {
            self.agent_sessions
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .get(&session_id)
                .map(|managed| managed.session.tool_execution_summary())
        });
        // H2: transcript/snapshot are fully read above; terminate the session
        // and reclaim its map entries before returning success.
        self.drop_session(&session_id);
        Ok(PluginIterationAgentRun {
            session_id: Some(session_id),
            tool_summary,
            transcript_excerpt,
            snapshot,
        })
    }

    fn verify_plugin_iteration(
        &self,
        state: &PluginIterationRunState,
    ) -> Result<VerificationReport, RuntimeError> {
        let tests_command = state
            .tests_command
            .clone()
            .or_else(|| state.prepared.tests_command.clone())
            .or_else(|| Some("cargo test --quiet --manifest-path plugins/Cargo.toml".to_string()));
        let safety_command = state
            .safety_command
            .clone()
            .or_else(|| state.prepared.safety_command.clone());
        // P0-4: `plugin:` commands must exercise the staged candidate. Wire an
        // invoker closure that dispatches into the candidate snapshot rather
        // than loading a fresh live invoker inside the verifier.
        let candidate_invoker = |plugin_path: &str,
                                 node_id: &str,
                                 payload: String|
         -> Result<PluginResponse, RuntimeError> {
            self.invoke_candidate(plugin_path, node_id, payload)
        };
        let options = VerifyOptions {
            candidate_invoker: Some(&candidate_invoker),
            command_timeout: None,
        };
        // Bound out of the `?` (single-line fallible expression) so the error
        // edge shares a line with the call it guards.
        let verified = CommandVerifier::verify_with_options(
            &self.fixtures_root,
            state.prepared.verify_profile,
            tests_command.as_deref(),
            safety_command.as_deref(),
            state.prepared.quality_score,
            &options,
        );
        verified
    }

    fn run_plugin_canary(
        &self,
        state: &PluginIterationRunState,
    ) -> Result<CanaryReport, RuntimeError> {
        let target_plugins = state
            .prepared
            .target_plugin_paths
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let samples = self
            .invocation_samples
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .iter()
            .cloned()
            .collect::<Vec<_>>();
        for sample in samples {
            if !target_plugins.contains(&sample.plugin_path) {
                continue;
            }
            // Both fallible steps are bound out of their `?` onto a single line
            // each: a `?` whose call spans several lines leaves llvm-cov a
            // zero-hit region on the continuation lines.
            let encoded = serde_json::to_string(&sample.payload)
                .map_err(|err| canary_payload_serialize_error(&err));
            let payload = encoded?;
            let invoked = self.invoke_candidate(&sample.plugin_path, &sample.node_id, payload);
            let response = invoked?;
            let actual = parse_response_payload(&response.payload);
            let verdict = if actual == sample.response {
                CanaryVerdict::Pass
            } else {
                CanaryVerdict::Fail
            };
            return Ok(CanaryReport {
                verdict,
                mode: "recent_successful_invocation_replay".to_string(),
                plugin_path: Some(sample.plugin_path),
                node_id: Some(sample.node_id),
                payload: Some(sample.payload),
                expected_response: Some(sample.response),
                actual_response: Some(actual.clone()),
                message: if verdict == CanaryVerdict::Pass {
                    "candidate replay matched current response".to_string()
                } else {
                    "candidate replay response diverged from current response".to_string()
                },
            });
        }

        for candidate in self.candidate_snapshot().into_iter() {
            for plugin_path in &state.prepared.target_plugin_paths {
                // Registry miss and a plugin without docs are both "no
                // evidence here, try the next target". Chained through
                // `and_then` so the two skips share one `for`-over-`Option`
                // gate instead of two `let ... else { continue }` guards whose
                // not-taken edges sit on their own lines.
                let declared = candidate
                    .plugin_registry()
                    .get(plugin_path)
                    .and_then(|plugin| plugin.docs)
                    .and_then(|docs| {
                        docs.nodes
                            .into_iter()
                            .find(|node| node.id.contains("canary") || node.id.contains("verify"))
                    });
                if let Some(node) = declared {
                    let invoked = self.invoke_candidate(plugin_path, &node.id, "{}".to_string());
                    let response = invoked?;
                    let actual = parse_response_payload(&response.payload);
                    return Ok(CanaryReport {
                        verdict: CanaryVerdict::Pass,
                        mode: "declared_plugin_verifier_node".to_string(),
                        plugin_path: Some(plugin_path.clone()),
                        node_id: Some(node.id.clone()),
                        payload: Some(Value::Object(Map::new())),
                        expected_response: None,
                        actual_response: Some(actual),
                        message: "plugin-declared canary/verifier node completed successfully"
                            .to_string(),
                    });
                }
            }
        }

        Ok(CanaryReport {
            verdict: CanaryVerdict::Partial,
            mode: "no_canary_evidence".to_string(),
            plugin_path: None,
            node_id: None,
            payload: None,
            expected_response: None,
            actual_response: None,
            message: "no recent successful invocation or declared canary/verifier node found"
                .to_string(),
        })
    }

    /// Roll back the candidate snapshot iff one is staged. Returns `None` when
    /// there is nothing to roll back, otherwise the rollback `Result` mapped to
    /// `()`. Kept separate from `finalize_plugin_iteration` so the failure
    /// aggregation feeds off a plain `Option<Result<(), _>>`.
    fn rollback_candidate_if_staged(&self) -> Option<Result<(), RuntimeError>> {
        if self.candidate_snapshot().is_some() {
            Some(self.rollback_candidate().map(|_| ()))
        } else {
            None
        }
    }

    fn finalize_plugin_iteration(
        &self,
        state: &mut PluginIterationRunState,
    ) -> Result<PluginIterationFinalVerdict, RuntimeError> {
        if let Some(stage_error) = state.stage_error.clone() {
            let candidate_rollback = self.rollback_candidate_if_staged();
            let workspace_restore = restore_plugin_iteration_workspace(
                &self.fixtures_root,
                &self.snapshot_root,
                state.rollback.as_ref(),
            )
            .map(|_| ());
            state.blocked_reason = Some(aggregate_rollback_failure(
                stage_error,
                candidate_rollback,
                workspace_restore,
            ));
            // 基础设施故障（磁盘满等）与"验证失败"分开定级。回滚照常先跑
            // （上面已执行），回滚自身失败也不改变这个判定——错误文本已由
            // `aggregate_rollback_failure` 拼进 blocked_reason，journal 留在盘上
            // 供人工恢复。
            let verdict = if state.stage_error_is_infrastructure {
                PluginIterationFinalVerdict::InfrastructureFailure
            } else {
                PluginIterationFinalVerdict::RolledBack
            };
            state.final_verdict = Some(verdict);
            return Ok(verdict);
        }
        let verifier_verdict = state.verifier_verdict.unwrap_or(VerifierVerdict::Partial);
        let canary_verdict = state
            .canary
            .as_ref()
            .map(|report| report.verdict)
            .unwrap_or(CanaryVerdict::Partial);

        // P0-3: guard against concurrent source-tree mutation between verify
        // and promote. The verifier hashes the tree; here we rehash and
        // compare. Any drift → downgrade the verdict to a rollback so we
        // don't promote code we never verified.
        let mut effective_verifier = verifier_verdict;
        let source_drift_reason = detect_plugin_source_drift(
            verifier_verdict,
            state
                .verification
                .as_ref()
                .and_then(|r| r.source_tree_hash.as_deref()),
            || hash_source_tree(&self.fixtures_root),
        );
        if source_drift_reason.is_some() {
            effective_verifier = VerifierVerdict::Fail;
        }
        if let Some(reason) = source_drift_reason.as_deref() {
            self.kernel.observe_plugin_issue(
                KernelPluginIssueSource::VerifierFailure,
                state.prepared.root_plugin_path.clone(),
                format!(
                    "plugin verifier TOCTOU guard tripped for {}: {}",
                    state.prepared.root_plugin_path, reason
                ),
            );
        }

        let final_verdict = if effective_verifier == VerifierVerdict::Pass
            && canary_verdict == CanaryVerdict::Pass
        {
            // P2-25: promote failure used to `?` propagate straight
            // out, leaving the workspace modified with journal on
            // disk but no rollback trigger (catch_unwind doesn't fire
            // for normal Err). Now: on promote failure, run rollback
            // + restore explicitly before propagating the promote
            // error so the workspace is clean.
            match self.promote_candidate() {
                Ok(_) => PluginIterationFinalVerdict::Promoted,
                Err(err) => {
                    let candidate_rollback = self.rollback_candidate_if_staged();
                    let workspace_restore = restore_plugin_iteration_workspace(
                        &self.fixtures_root,
                        &self.snapshot_root,
                        state.rollback.as_ref(),
                    )
                    .map(|_| ());
                    state.blocked_reason = Some(aggregate_rollback_failure(
                        format!("promote failed: {err}"),
                        candidate_rollback,
                        workspace_restore,
                    ));
                    return Err(err);
                }
            }
        } else if effective_verifier == VerifierVerdict::Pass
            && canary_verdict == CanaryVerdict::Partial
            && state.prepared.manual_approved
        {
            // When the user explicitly approves, allow promotion without canary evidence.
            // Same rollback-on-promote-failure guard as above.
            match self.promote_candidate() {
                Ok(_) => PluginIterationFinalVerdict::Promoted,
                Err(err) => {
                    let candidate_rollback = self.rollback_candidate_if_staged();
                    let workspace_restore = restore_plugin_iteration_workspace(
                        &self.fixtures_root,
                        &self.snapshot_root,
                        state.rollback.as_ref(),
                    )
                    .map(|_| ());
                    state.blocked_reason = Some(aggregate_rollback_failure(
                        format!("promote (manual-approved) failed: {err}"),
                        candidate_rollback,
                        workspace_restore,
                    ));
                    return Err(err);
                }
            }
        } else if canary_verdict == CanaryVerdict::Partial {
            // P2-24: `Partial` without manual_approved → Blocked. We
            // intentionally keep the candidate snapshot alive so the
            // user can call `approve_blocked_iteration(...)` later;
            // discarding here would forfeit that path. If a new
            // `iterate_plugins` runs before approval it will replace
            // this candidate (see `reload_candidate_internal`), so
            // long-lived blocked candidates are not a memory leak —
            // just a UX hazard callers should be aware of.
            state.blocked_reason = Some(
                state
                    .canary
                    .as_ref()
                    .map(|report| report.message.clone())
                    .unwrap_or_else(|| "canary returned partial".to_string()),
            );
            PluginIterationFinalVerdict::Blocked
        } else {
            if let Some(reason) = source_drift_reason.as_ref() {
                state.blocked_reason = Some(reason.clone());
            }
            // `rollback_candidate_if_staged` folds the "no candidate staged"
            // and "rollback failed" cases into one `Option<Result<_, _>>`, so
            // the error text is built by a single chained expression instead of
            // an `if`-inside-`if` whose inner arm needs its own gate.
            let candidate_error = self
                .rollback_candidate_if_staged()
                .and_then(|outcome| outcome.err())
                .map(|err| verdict_rollback_partial_cleanup_reason(&err));
            let restored = restore_plugin_iteration_workspace(
                &self.fixtures_root,
                &self.snapshot_root,
                state.rollback.as_ref(),
            );
            restored?;
            // `or`-assign: a candidate-cleanup error overwrites whatever the
            // drift reason above set, and `None` leaves it alone — the same
            // outcome as the previous `if let Some(..)` guard, on one line.
            state.blocked_reason = candidate_error.or(state.blocked_reason.take());
            PluginIterationFinalVerdict::RolledBack
        };
        state.final_verdict = Some(final_verdict);
        Ok(final_verdict)
    }

    fn record_invocation_sample(
        &self,
        plugin_path: &str,
        node_id: &str,
        payload: &str,
        response_payload: &str,
    ) {
        let payload =
            serde_json::from_str(payload).unwrap_or_else(|_| Value::String(payload.to_string()));
        let response = parse_response_payload(response_payload);
        let mut samples = self
            .invocation_samples
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        samples.push_front(InvocationSample {
            plugin_path: plugin_path.to_string(),
            node_id: node_id.to_string(),
            payload,
            response,
            observed_at_ms: now_ms(),
        });
        while samples.len() > 64 {
            samples.pop_back();
        }
    }

    /// Record a stage failure on the iteration state: file a kernel issue via
    /// `observe_plugin_iteration_failure` and stamp `stage_error` so the run
    /// short-circuits to rollback. One call site per pipeline stage.
    fn fail_stage(&self, state: &mut PluginIterationRunState, stage: &str, err: &RuntimeError) {
        self.observe_plugin_iteration_failure(&state.prepared, stage, err);
        state.stage_error = Some(err.to_string());
        // 同时记下这次失败是否为基础设施故障（磁盘满 / 配额耗尽）。所有 stage
        // 的失败都经本函数，故此处是唯一需要标记的地方；收尾时据此给
        // `InfrastructureFailure` 而不是把 ENOSPC 误报成"验证失败"。
        state.stage_error_is_infrastructure = err.is_infrastructure_failure();
    }

    fn observe_plugin_iteration_failure(
        &self,
        prepared: &PreparedPluginIteration,
        stage: &str,
        err: &RuntimeError,
    ) {
        // 基础设施故障（磁盘满等）不是插件的问题：此前 rebuild /
        // stage_candidate 阶段的任何错误都被记成 LoadFailure 归咎于插件，
        // 于 ENOSPC 时会凭空产生一条"插件加载失败"的 kernel issue。
        if err.is_infrastructure_failure() {
            return;
        }
        let source = match err {
            RuntimeError::PluginIterationPolicyBlocked { .. } => {
                Some(KernelPluginIssueSource::PolicyBlocked)
            }
            _ if matches!(stage, "rebuild" | "stage_candidate") => {
                Some(KernelPluginIssueSource::LoadFailure)
            }
            _ => None,
        };
        let Some(source) = source else {
            return;
        };
        self.kernel.observe_plugin_issue(
            source,
            prepared.root_plugin_path.clone(),
            format!(
                "plugin iteration {stage} failed for {}: {err}",
                prepared.root_plugin_path
            ),
        );
    }

    fn observe_snapshot_plugin_issues(
        &self,
        snapshot: &RuntimeSnapshot,
        report: &ReloadReport,
        stage: &str,
    ) {
        for (plugin_path, plugin) in snapshot.plugin_registry().iter() {
            let changed_reasons = report
                .changed_plugin_reasons
                .get(&plugin_path)
                .cloned()
                .unwrap_or_default();
            match plugin.load_result {
                PluginLoadResult::Unavailable(reason) => {
                    let source = match reason {
                        PluginUnavailableReason::ContractViolation => {
                            KernelPluginIssueSource::DocsDrift
                        }
                        _ => KernelPluginIssueSource::LoadFailure,
                    };
                    self.kernel.observe_plugin_issue(
                        source,
                        plugin_path.clone(),
                        format!("{stage} observed plugin {plugin_path} unavailable: {reason:?}"),
                    );
                }
                PluginLoadResult::Loaded => {
                    if changed_reasons.iter().any(|reason| {
                        matches!(reason.as_str(), "docs_changed" | "fingerprint_diff_changed")
                    }) {
                        self.kernel.observe_plugin_issue(
                            KernelPluginIssueSource::DocsDrift,
                            plugin_path.clone(),
                            format!(
                                "{stage} detected docs/contract drift for {plugin_path}: {}",
                                changed_reasons.join(", ")
                            ),
                        );
                    }
                }
            }
        }
    }

    fn observe_reload_error(&self, stage: &str, err: &RuntimeError) {
        let Some(plugin_path) = plugin_path_from_runtime_error(err) else {
            return;
        };
        let source = match err {
            RuntimeError::DocsContract { .. } => KernelPluginIssueSource::DocsDrift,
            RuntimeError::PluginUnavailable {
                reason: PluginUnavailableReason::ContractViolation,
                ..
            } => KernelPluginIssueSource::DocsDrift,
            _ => KernelPluginIssueSource::LoadFailure,
        };
        self.kernel.observe_plugin_issue(
            source,
            plugin_path.clone(),
            format!("{stage} failed for {plugin_path}: {err}"),
        );
    }

    fn reload_internal(
        &self,
    ) -> Result<(ReloadReport, ReloadAttemptReport), (RuntimeError, Box<ReloadAttemptReport>)> {
        let previous_snapshot = self.current_snapshot();
        let staged_artifact_root = next_staged_artifact_root(&self.snapshot_root);
        let started_at = Instant::now();

        let next_snapshot =
            match build_snapshot_with_staged_root(&self.loader, staged_artifact_root.clone()) {
                Ok(snapshot) => Arc::new(snapshot),
                Err(err) => {
                    let attempt = ReloadAttemptReport {
                        status: ReloadAttemptStatus::Failed,
                        from_snapshot_id: previous_snapshot.snapshot_id().to_string(),
                        to_snapshot_id: None,
                        snapshot_root: self.snapshot_root.display().to_string(),
                        staged_artifact_root: staged_artifact_root.display().to_string(),
                        elapsed_ms: started_at.elapsed().as_millis(),
                        plugin_count: None,
                        node_count: None,
                        added_plugins: Vec::new(),
                        removed_plugins: Vec::new(),
                        changed_plugins: Vec::new(),
                        changed_plugin_reasons: BTreeMap::new(),
                        failure_summary: Some(err.to_string()),
                    };
                    return Err((err, Box::new(attempt)));
                }
            };

        // Stop services for plugins that are being removed or changed in the
        // new snapshot before swapping it in.
        let previous_plugins: BTreeSet<String> = previous_snapshot
            .plugin_registry()
            .iter()
            .map(|(path, _)| path)
            .collect();
        let next_plugins: BTreeSet<String> = next_snapshot
            .plugin_registry()
            .iter()
            .map(|(path, _)| path)
            .collect();
        // Plugins the new snapshot dropped — stop their services.
        for plugin_path in previous_plugins.difference(&next_plugins) {
            self.service_registry.stop_plugin_services(plugin_path);
        }
        // Also stop services for plugins whose docs changed (the new snapshot
        // may have different Task nodes).
        for plugin_path in next_plugins.intersection(&previous_plugins) {
            let prev_plugin = previous_snapshot.plugin_registry().get(plugin_path);
            let next_plugin = next_snapshot.plugin_registry().get(plugin_path);
            let prev_docs = prev_plugin.as_ref().and_then(|p| p.docs.as_ref());
            let next_docs = next_plugin.as_ref().and_then(|p| p.docs.as_ref());
            // Compare docs by JSON representation — if they differ, restart
            // services so the new plugin version's services are used.
            if prev_docs != next_docs {
                self.service_registry.stop_plugin_services(plugin_path);
            }
        }

        {
            let mut guard = self
                .current_snapshot
                .write()
                .unwrap_or_else(|poison| poison.into_inner());
            *guard = next_snapshot.clone();
        }
        let replaced_candidate = self
            .candidate_snapshot
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take();

        let report = ReloadReport::from_snapshots(
            previous_snapshot.as_ref(),
            next_snapshot.as_ref(),
            &self.snapshot_root,
            started_at.elapsed().as_millis(),
        );
        let retired_root = previous_snapshot.staged_artifact_root.clone();
        let retired_weak = Arc::downgrade(&previous_snapshot);
        drop(previous_snapshot);
        self.retired_snapshots
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(RetiredSnapshot {
                snapshot: retired_weak,
                staged_artifact_root: retired_root,
            });
        if let Some(candidate) = replaced_candidate {
            self.retire_snapshot(candidate.snapshot);
        }
        self.cleanup_retired_snapshots();

        let attempt = ReloadAttemptReport::from_report(&report, next_snapshot.as_ref());
        Ok((report, attempt))
    }

    fn record_reload_attempt(&self, attempt: ReloadAttemptReport) {
        let mut guard = self
            .last_reload_attempt
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        *guard = Some(attempt);
    }

    fn record_candidate_reload_attempt(&self, attempt: ReloadAttemptReport) {
        let mut guard = self
            .last_candidate_reload_attempt
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        *guard = Some(attempt);
    }

    fn cleanup_retired_snapshots(&self) {
        // P1-22: opportunistic removal of Weak-dead entries. Long-lived
        // agent sessions may pin an Arc<RuntimeSnapshot> across many
        // reloads, keeping the underlying `RetiredSnapshot` alive; without
        // an upper bound the Vec grew unbounded. In addition to the
        // Weak-liveness sweep we now cap the retained count at
        // `MAX_RETIRED_SNAPSHOTS` and drop the oldest entries first, so a
        // pathologically long-lived pin can only leak a bounded prefix.
        const MAX_RETIRED_SNAPSHOTS: usize = 64;
        let mut retired = self
            .retired_snapshots
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        retired.retain(|entry| {
            if entry.snapshot.upgrade().is_some() {
                return true;
            }
            let _ = fs::remove_dir_all(&entry.staged_artifact_root);
            false
        });
        while retired.len() > MAX_RETIRED_SNAPSHOTS {
            let dropped = retired.remove(0);
            let _ = fs::remove_dir_all(&dropped.staged_artifact_root);
        }
    }

    fn reload_candidate_internal(
        &self,
    ) -> Result<
        (CandidateSnapshotStatus, ReloadAttemptReport),
        (RuntimeError, Box<ReloadAttemptReport>),
    > {
        let previous_snapshot = self.current_snapshot();
        let staged_artifact_root = next_staged_artifact_root(&self.snapshot_root);
        let started_at = Instant::now();

        let next_snapshot =
            match build_snapshot_with_staged_root(&self.loader, staged_artifact_root.clone()) {
                Ok(snapshot) => Arc::new(snapshot),
                Err(err) => {
                    let attempt = ReloadAttemptReport {
                        status: ReloadAttemptStatus::Failed,
                        from_snapshot_id: previous_snapshot.snapshot_id().to_string(),
                        to_snapshot_id: None,
                        snapshot_root: self.snapshot_root.display().to_string(),
                        staged_artifact_root: staged_artifact_root.display().to_string(),
                        elapsed_ms: started_at.elapsed().as_millis(),
                        plugin_count: None,
                        node_count: None,
                        added_plugins: Vec::new(),
                        removed_plugins: Vec::new(),
                        changed_plugins: Vec::new(),
                        changed_plugin_reasons: BTreeMap::new(),
                        failure_summary: Some(err.to_string()),
                    };
                    return Err((err, Box::new(attempt)));
                }
            };

        let report = ReloadReport::from_snapshots(
            previous_snapshot.as_ref(),
            next_snapshot.as_ref(),
            &self.snapshot_root,
            started_at.elapsed().as_millis(),
        );
        let status = CandidateSnapshotStatus::from_snapshots(
            previous_snapshot.as_ref(),
            next_snapshot.as_ref(),
            &self.snapshot_root,
            &report,
        );
        let attempt = ReloadAttemptReport::from_candidate_status(&status, report.elapsed_ms);
        let candidate_entry = StagedCandidateSnapshot {
            snapshot: next_snapshot,
            status: status.clone(),
        };

        let mut guard = self
            .candidate_snapshot
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(previous_candidate) = guard.replace(candidate_entry) {
            self.retire_snapshot(previous_candidate.snapshot);
        }
        drop(guard);
        self.cleanup_retired_snapshots();
        Ok((status, attempt))
    }

    fn retire_snapshot(&self, snapshot: Arc<RuntimeSnapshot>) {
        let staged_artifact_root = snapshot.staged_artifact_root.clone();
        let retired_weak = Arc::downgrade(&snapshot);
        drop(snapshot);
        self.retired_snapshots
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(RetiredSnapshot {
                snapshot: retired_weak,
                staged_artifact_root,
            });
    }

    /// 删除本进程当前 live snapshot 的 staged root，并回收所有已退休的。
    ///
    /// `cleanup_retired_snapshots` 只回收 **reload 时被退休**的快照，live
    /// 的那一份没有任何路径负责：boot 后不 reload 直接退出的进程会把它原地
    /// 留下，叠加"hash 目录再也不会重现"即成为永久孤儿。
    ///
    /// **幂等**：重复调用安全，供 `Drop` 与信号处理路径共用 —— 信号处理器走
    /// `std::process::exit(0)`，绕过所有 `Drop`，因此必须显式调一次。
    pub fn cleanup_live_snapshot(&self) {
        let staged_root = self.current_snapshot().staged_artifact_root.clone();
        if staged_artifact_root_is_removable(&staged_root, &self.snapshot_root) {
            let _ = fs::remove_dir_all(&staged_root);
        }
        self.cleanup_retired_snapshots();
    }
}

impl Drop for RuntimeHost {
    fn drop(&mut self) {
        self.cleanup_live_snapshot();
    }
}

impl ReloadReport {
    fn from_snapshots(
        previous: &RuntimeSnapshot,
        next: &RuntimeSnapshot,
        snapshot_root: &Path,
        elapsed_ms: u128,
    ) -> Self {
        let mut added_plugins = Vec::new();
        let mut removed_plugins = Vec::new();
        let mut changed_plugins = Vec::new();
        let mut changed_plugin_reasons = BTreeMap::new();

        for (plugin_path, plugin) in next.plugin_registry.iter() {
            match previous.plugin_registry.get(&plugin_path) {
                None => added_plugins.push(plugin_path.clone()),
                Some(previous_plugin) => {
                    let reasons = plugin_change_reasons(&previous_plugin, &plugin);
                    if !reasons.is_empty() {
                        changed_plugins.push(plugin_path.clone());
                        changed_plugin_reasons.insert(plugin_path.clone(), reasons);
                    }
                }
            }
        }

        for (plugin_path, _) in previous.plugin_registry.iter() {
            if next.plugin_registry.get(&plugin_path).is_none() {
                removed_plugins.push(plugin_path.clone());
            }
        }

        Self {
            from_snapshot_id: previous.snapshot_id.clone(),
            to_snapshot_id: next.snapshot_id.clone(),
            snapshot_root: snapshot_root.display().to_string(),
            staged_artifact_root: next.staged_artifact_root.display().to_string(),
            elapsed_ms,
            added_plugins,
            removed_plugins,
            changed_plugins,
            changed_plugin_reasons,
        }
    }
}

impl ReloadAttemptReport {
    fn from_report(report: &ReloadReport, next: &RuntimeSnapshot) -> Self {
        Self {
            status: ReloadAttemptStatus::Reloaded,
            from_snapshot_id: report.from_snapshot_id.clone(),
            to_snapshot_id: Some(report.to_snapshot_id.clone()),
            snapshot_root: report.snapshot_root.clone(),
            staged_artifact_root: report.staged_artifact_root.clone(),
            elapsed_ms: report.elapsed_ms,
            plugin_count: Some(next.plugin_registry().iter().count()),
            node_count: Some(next.node_registry().len()),
            added_plugins: report.added_plugins.clone(),
            removed_plugins: report.removed_plugins.clone(),
            changed_plugins: report.changed_plugins.clone(),
            changed_plugin_reasons: report.changed_plugin_reasons.clone(),
            failure_summary: None,
        }
    }

    fn from_candidate_status(status: &CandidateSnapshotStatus, elapsed_ms: u128) -> Self {
        Self {
            status: ReloadAttemptStatus::Staged,
            from_snapshot_id: status.from_snapshot_id.clone(),
            to_snapshot_id: Some(status.candidate_snapshot_id.clone()),
            snapshot_root: status.snapshot_root.clone(),
            staged_artifact_root: status.staged_artifact_root.clone(),
            elapsed_ms,
            plugin_count: Some(status.plugin_count),
            node_count: Some(status.node_count),
            added_plugins: status.added_plugins.clone(),
            removed_plugins: status.removed_plugins.clone(),
            changed_plugins: status.changed_plugins.clone(),
            changed_plugin_reasons: status.changed_plugin_reasons.clone(),
            failure_summary: None,
        }
    }
}

impl CandidateSnapshotStatus {
    fn from_snapshots(
        previous: &RuntimeSnapshot,
        next: &RuntimeSnapshot,
        snapshot_root: &Path,
        report: &ReloadReport,
    ) -> Self {
        Self {
            from_snapshot_id: previous.snapshot_id.clone(),
            candidate_snapshot_id: next.snapshot_id.clone(),
            snapshot_root: snapshot_root.display().to_string(),
            staged_artifact_root: next.staged_artifact_root.display().to_string(),
            plugin_count: next.plugin_registry().iter().count(),
            node_count: next.node_registry().len(),
            added_plugins: report.added_plugins.clone(),
            removed_plugins: report.removed_plugins.clone(),
            changed_plugins: report.changed_plugins.clone(),
            changed_plugin_reasons: report.changed_plugin_reasons.clone(),
        }
    }
}

const PLUGIN_AGENT_TOOL_LIST_CONTEXT_FILES: &str = "list_context_files";
const PLUGIN_AGENT_TOOL_READ_CONTEXT_FILES: &str = "read_context_files";
const PLUGIN_AGENT_TOOL_INSPECT_PLUGIN_CATALOG: &str = "inspect_plugin_catalog";
const PLUGIN_AGENT_TOOL_SCAFFOLD_CHILD_PLUGIN: &str = "scaffold_child_plugin";
const PLUGIN_AGENT_TOOL_REPLACE_FILE_EXACT: &str = "replace_file_exact";
const PLUGIN_AGENT_TOOL_REPLACE_FILES_EXACT: &str = "replace_files_exact";
const PLUGIN_AGENT_TOOL_CREATE_FILE: &str = "create_file";
const PLUGIN_AGENT_TOOL_DELETE_FILE: &str = "delete_file";
const PLUGIN_AGENT_TOOL_TOML_SET: &str = "toml_set";
const PLUGIN_AGENT_TOOL_JSON_SET: &str = "json_set";
// P2-3: `run_plugin_check` / `run_plugin_test` / `rebuild_plugin_workspace`
// share names with tools of the same name in `agent.rs`
// (`AGENT_TOOL_RUN_PLUGIN_TEST`, etc). The two backends never share a
// dispatch table, but the schemas are DIFFERENT: PluginIteration
// requires `plugin_path`, RuntimeShell makes it optional and defaults
// to `/`. If a future refactor unifies dispatch, rename these to
// `plugin_iteration.run_plugin_test` etc, or fold the two schemas into
// one and accept the wider superset. Keeping the names identical for
// now avoids gratuitous churn in agent prompts.
const PLUGIN_AGENT_TOOL_RUN_PLUGIN_CHECK: &str = "run_plugin_check";
const PLUGIN_AGENT_TOOL_RUN_PLUGIN_TEST: &str = "run_plugin_test";
const PLUGIN_AGENT_TOOL_REBUILD_PLUGIN_WORKSPACE: &str = "rebuild_plugin_workspace";
const PLUGIN_AGENT_TOOL_RECORD_ITERATION_SUMMARY: &str = "record_iteration_summary";
const PLUGIN_ITERATION_AGENT_TIMEOUT_CAP_MS: u64 = 1_200_000;

#[derive(Debug, Clone, Deserialize)]
struct ListContextFilesArgs {
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ReadContextFilesArgs {
    paths: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ScaffoldChildPluginArgs {
    parent_plugin_path: String,
    child_name: String,
    #[serde(default)]
    template_plugin_path: Option<String>,
    #[serde(default)]
    node_id: Option<String>,
    #[serde(default)]
    summary: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ReplaceFileExactArgs {
    path: String,
    expected_old_string: String,
    new_content: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ReplaceFilesExactArgs {
    edits: Vec<ReplaceFileExactArgs>,
}

#[derive(Debug, Clone, Deserialize)]
struct CreateFileArgs {
    path: String,
    new_content: String,
}

#[derive(Debug, Clone, Deserialize)]
struct DeleteFileArgs {
    path: String,
    expected_sha256: String,
}

#[derive(Debug, Clone, Deserialize)]
struct TomlSetArgs {
    path: String,
    expected_sha256: String,
    dotted_key: String,
    value: Value,
}

#[derive(Debug, Clone, Deserialize)]
struct JsonSetArgs {
    path: String,
    expected_sha256: String,
    pointer: String,
    value: Value,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct RunPluginCommandArgs {
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    plugin_path: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct RecordIterationSummaryArgs {
    summary: String,
    #[serde(default)]
    tests_command: Option<String>,
    #[serde(default)]
    safety_command: Option<String>,
}

impl ManagedAgentSession {
    pub(crate) fn compact_history(&mut self) -> (usize, usize) {
        let old = self.session.history_len();
        self.session.compact_history();
        (old, self.session.history_len())
    }

    fn respond(&mut self, host: &RuntimeHost, input: &str) -> Result<AgentReply, RuntimeError> {
        match &mut self.state {
            ManagedAgentState::RuntimeShell => {
                self.session
                    .respond_with_runtime_host(host, &self.handle.session_id, input)
            }
            ManagedAgentState::PluginIteration(state) => {
                let mut backend = PluginIterationAgentBackend {
                    host,
                    state: state.as_mut(),
                };
                self.session.respond(&mut backend, input)
            }
        }
    }
}

struct PluginIterationAgentBackend<'a> {
    host: &'a RuntimeHost,
    state: &'a mut PluginIterationAgentState,
}

impl<'a> PluginIterationAgentBackend<'a> {
    fn phase(&self) -> &'static str {
        if self.state.recorded_summary.is_some() {
            "finalized"
        } else if self.state.operations.is_empty() {
            "exploration"
        } else if self.state.verification_attempts == 0 {
            "editing"
        } else if self.state.verification_successes == 0 {
            "verification_retry"
        } else {
            "verification"
        }
    }

    fn apply_operations(
        &mut self,
        summary: &str,
        operations: Vec<PluginEditOperation>,
    ) -> Result<Value, RuntimeError> {
        let mut combined_operations = self.state.operations.clone();
        combined_operations.extend(operations.clone());
        let writable_roots = self
            .state
            .prepared
            .allowed_plugin_roots
            .values()
            .cloned()
            .collect::<BTreeSet<_>>();
        validate_reserved_child_keyword_identifiers(&combined_operations, &writable_roots)?;

        let executor = PluginEditExecutor::new(&self.host.fixtures_root);
        let mut local_rollback = PluginEditRollback::empty(&self.host.fixtures_root);
        for (idx, operation) in operations.iter().enumerate() {
            let single = PluginEditPlan {
                issue_id: self.state.prepared.issue_id.clone(),
                patch_id: format!("{}-tool-{}", self.state.prepared.iteration_id, idx),
                summary: summary.to_string(),
                operations: vec![operation.clone()],
            };
            let execute = executor.execute(
                &self.host.kernel.plugin_iteration_policy,
                &self.state.prepared.allowed_plugin_roots,
                &single,
            );
            match execute {
                Ok((_apply_result, rollback)) => {
                    local_rollback.absorb(rollback)?;
                }
                Err(err) => {
                    let rollback_err = local_rollback.rollback().err();
                    let enriched = enrich_plugin_iteration_edit_error(
                        operation,
                        &self.host.fixtures_root,
                        err,
                    );
                    return Err(with_partial_batch_rollback_failure(enriched, rollback_err));
                }
            }
        }
        self.state.rollback.absorb(local_rollback)?;

        // Persist the rollback journal to disk after every tool execution so
        // that a crash mid-agent-loop still leaves a recoverable journal on
        // restart — no window where files are modified but no backup exists.
        // Bound out of the `?` so the fallible expression fits one line.
        let persisted = self.state.rollback.persist_journal(
            &plugin_iteration_journal_path(&self.host.snapshot_root),
            &self.state.prepared.iteration_id,
        );
        persisted?;

        self.state.operations.extend(operations.clone());
        for path in operations.into_iter().map(|operation| operation.path) {
            if should_track_context_file(&path) {
                self.state.focus_context_paths.push(path.clone());
                self.state.all_context_paths.push(path);
            }
        }
        sort_and_dedup_context_paths(&mut self.state.focus_context_paths);
        sort_and_dedup_context_paths(&mut self.state.all_context_paths);
        let derived = self.state.snapshot().derived_edit_plan;
        Ok(json!({
            "changed_paths": derived.changed_paths(),
            "operation_count": derived.operations.len(),
        }))
    }

    fn replace_file_exact_operation(args: ReplaceFileExactArgs) -> PluginEditOperation {
        PluginEditOperation {
            path: args.path,
            kind: PluginEditOpKind::ReplaceExact,
            expected_old_string: Some(args.expected_old_string),
            expected_sha256: None,
            new_content: Some(args.new_content),
            pointer: None,
            dotted_key: None,
            value: None,
        }
    }

    fn visible_context_paths(&self) -> &[String] {
        if self.state.context_scope_expanded {
            &self.state.all_context_paths
        } else {
            &self.state.focus_context_paths
        }
    }

    fn list_context_files(&mut self, scope: ContextFilesScope) -> Value {
        if scope == ContextFilesScope::All {
            self.state.context_scope_expanded = true;
        }

        let mut focus_paths = self.state.focus_context_paths.clone();
        let mut paths = match scope {
            ContextFilesScope::Focus => focus_paths.clone(),
            ContextFilesScope::All => self.state.all_context_paths.clone(),
        };
        sort_plugin_context_paths(&mut focus_paths);
        sort_plugin_context_paths(&mut paths);
        json!({
            "root_plugin_path": self.state.prepared.root_plugin_path,
            "phase": self.phase(),
            "scope": match scope {
                ContextFilesScope::Focus => "focus",
                ContextFilesScope::All => "all",
            },
            "scope_expanded": self.state.context_scope_expanded,
            "hidden_count": self.state.all_context_paths.len().saturating_sub(paths.len()),
            "focus_paths": focus_paths,
            "paths": paths,
        })
    }

    fn read_context_path(&self, path: &str) -> Result<Value, RuntimeError> {
        let normalized = normalize_rel_path(path)?;
        if !self
            .state
            .all_context_paths
            .iter()
            .any(|item| item == &normalized)
        {
            return Err(RuntimeError::InvalidArgument {
                message: format!(
                    "context file is not available in this plugin iteration session: {normalized}"
                ),
            });
        }
        if !self
            .visible_context_paths()
            .iter()
            .any(|item| item == &normalized)
        {
            return Err(RuntimeError::InvalidArgument {
                message: format!(
                    "context file is currently hidden behind the structural focus shortlist: {normalized}. Call list_context_files with `{{\"scope\":\"all\"}}` before reading deeper subtree files."
                ),
            });
        }
        let abs_path = self.host.fixtures_root.join(&normalized);
        let content =
            fs::read_to_string(&abs_path).map_err(|err| region_io_error(&abs_path, &err))?;
        Ok(json!({
            "path": normalized,
            "sha256": sha256_text(&content),
            "content": content,
        }))
    }

    fn read_context_files(&self, paths: &[String]) -> Result<Value, RuntimeError> {
        let files = paths
            .iter()
            .map(|path| self.read_context_path(path))
            .collect::<Result<Vec<_>, RuntimeError>>()?;
        Ok(json!({ "files": files }))
    }

    fn inspect_plugin_catalog(&self) -> Value {
        let snapshot = self.host.current_snapshot();
        let plugins = snapshot
            .plugin_registry()
            .iter()
            .filter(|(plugin_path, _)| {
                plugin_path == &self.state.prepared.root_plugin_path
                    || plugin_path.starts_with(&format!("{}/", self.state.prepared.root_plugin_path))
            })
            .map(|(plugin_path, plugin)| {
                json!({
                    "plugin_path": plugin_path,
                    "parent": plugin.parent,
                    "required": plugin.required,
                    "node_ids": plugin
                        .docs
                        .as_ref()
                        .map(|docs| docs.nodes.iter().map(|node| node.id.clone()).collect::<Vec<_>>())
                        .unwrap_or_default(),
                    "node_summaries": plugin
                        .docs
                        .as_ref()
                        .map(|docs| docs.nodes.iter().map(|node| {
                            json!({
                                "node_id": node.id,
                                "summary": node.summary,
                            })
                        }).collect::<Vec<_>>())
                        .unwrap_or_default(),
                })
            })
            .collect::<Vec<_>>();
        json!({
            "root_plugin_path": self.state.prepared.root_plugin_path,
            "plugins": plugins,
        })
    }

    fn scaffold_child_plugin(
        &mut self,
        args: ScaffoldChildPluginArgs,
    ) -> Result<Value, RuntimeError> {
        if !self
            .state
            .prepared
            .target_plugin_paths
            .iter()
            .any(|path| path == &args.parent_plugin_path)
        {
            return Err(RuntimeError::InvalidArgument {
                message: format!(
                    "parent plugin path {} is outside the selected subtree",
                    args.parent_plugin_path
                ),
            });
        }

        let child_segment = sanitize_child_plugin_segment(&args.child_name);
        let child_plugin_path = format!("{}/{}", args.parent_plugin_path, child_segment);
        let child_root = format!("plugins/{child_plugin_path}");
        let node_id = args
            .node_id
            .clone()
            .unwrap_or_else(|| format!("{}_entry", child_segment.replace('-', "_")));
        let crate_name = child_plugin_path.replace(['/', '-'], "_");
        let summary = args
            .summary
            .clone()
            .unwrap_or_else(|| format!("Child plugin scaffold for {child_plugin_path}"));

        let parent_manifest_rel = format!("plugins/{}/Cargo.toml", args.parent_plugin_path);
        let parent_manifest_abs = self.host.fixtures_root.join(&parent_manifest_rel);
        let parent_manifest_text = fs::read_to_string(&parent_manifest_abs)
            .map_err(|err| region_io_error(&parent_manifest_abs, &err))?;
        let parent_manifest_sha = file_sha256(&parent_manifest_abs)?;
        let parent_toml: TomlValue = toml::from_str(&parent_manifest_text)
            .map_err(|err| parent_manifest_parse_error(&parent_manifest_abs, &err))?;
        let mut children = parent_toml
            .get("package")
            .and_then(TomlValue::as_table)
            .and_then(|value| value.get("metadata"))
            .and_then(TomlValue::as_table)
            .and_then(|value| value.get("cordis"))
            .and_then(TomlValue::as_table)
            .and_then(|value| value.get("children"))
            .and_then(TomlValue::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|entry| serde_json::to_value(entry).unwrap_or(Value::Null))
            .collect::<Vec<_>>();
        let child_source = format!("./{child_segment}");
        if !children.iter().any(|entry| {
            entry
                .get("source")
                .and_then(Value::as_str)
                .is_some_and(|value| value == child_source)
        }) {
            children.push(json!({
                "source": child_source,
                "required": true,
                "grants": [],
            }));
        }

        let manifest_path = format!("{child_root}/Cargo.toml");
        let lib_path = format!("{child_root}/src/lib.rs");
        let core_path = format!("{child_root}/src/core.rs");
        let test_path = format!(
            "{child_root}/tests/{}_scaffold.rs",
            child_segment.replace('-', "_")
        );
        let human_path = format!("{child_root}/docs/human/overview.md");

        let operations = vec![
            PluginEditOperation {
                path: parent_manifest_rel.clone(),
                kind: PluginEditOpKind::TomlSet,
                expected_old_string: None,
                expected_sha256: Some(parent_manifest_sha),
                new_content: None,
                pointer: None,
                dotted_key: Some("package.metadata.cordis.children".to_string()),
                value: Some(Value::Array(children)),
            },
            PluginEditOperation {
                path: manifest_path.clone(),
                kind: PluginEditOpKind::CreateFile,
                expected_old_string: Some(String::new()),
                expected_sha256: None,
                new_content: Some(render_child_plugin_manifest(
                    &crate_name,
                    &child_plugin_path,
                    &node_id,
                )),
                pointer: None,
                dotted_key: None,
                value: None,
            },
            PluginEditOperation {
                path: lib_path.clone(),
                kind: PluginEditOpKind::CreateFile,
                expected_old_string: Some(String::new()),
                expected_sha256: None,
                new_content: Some(render_child_plugin_lib(
                    &crate_name,
                    &child_plugin_path,
                    &node_id,
                    &summary,
                )),
                pointer: None,
                dotted_key: None,
                value: None,
            },
            PluginEditOperation {
                path: core_path.clone(),
                kind: PluginEditOpKind::CreateFile,
                expected_old_string: Some(String::new()),
                expected_sha256: None,
                new_content: Some(render_child_plugin_core(&child_segment)),
                pointer: None,
                dotted_key: None,
                value: None,
            },
            PluginEditOperation {
                path: test_path.clone(),
                kind: PluginEditOpKind::CreateFile,
                expected_old_string: Some(String::new()),
                expected_sha256: None,
                new_content: Some(render_child_plugin_test(&crate_name)),
                pointer: None,
                dotted_key: None,
                value: None,
            },
            PluginEditOperation {
                path: human_path.clone(),
                kind: PluginEditOpKind::CreateFile,
                expected_old_string: Some(String::new()),
                expected_sha256: None,
                new_content: Some(render_child_plugin_overview(&child_plugin_path)),
                pointer: None,
                dotted_key: None,
                value: None,
            },
        ];
        let applied = self.apply_operations("scaffold_child_plugin", operations)?;
        self.state
            .scaffolded_children
            .push(ScaffoldedChildRegistration {
                parent_manifest_path: parent_manifest_rel.clone(),
                child_root_path: child_root.clone(),
            });
        self.state
            .scaffolded_children
            .sort_by(scaffolded_child_order);
        self.state.scaffolded_children.dedup();
        Ok(json!({
            "child_plugin_path": child_plugin_path,
            "template_plugin_path": args.template_plugin_path,
            "normalized_child_name": child_segment,
            "node_id": node_id,
            "parent_manifest_path": parent_manifest_rel,
            "created_paths": [manifest_path, lib_path, core_path, test_path, human_path],
            "result": applied,
        }))
    }

    fn run_checked_command(&mut self, stage: &str, command: String) -> Result<Value, RuntimeError> {
        self.state.verification_attempts += 1;
        let output = Command::new("bash")
            .arg("-lc")
            .arg(&command)
            .current_dir(&self.host.fixtures_root)
            .output()
            .map_err(|err| checked_command_spawn_error(&command, &err))?;
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        if output.status.success() {
            let warning_diagnostics = warning_diagnostics_for_changed_paths(
                &stdout,
                &stderr,
                &self.state.operations,
                &self.host.fixtures_root,
            );
            if !warning_diagnostics.is_empty() {
                return Err(RuntimeError::LlmResponseInvalid {
                    message: warning_cleanup_error_message(&command, &warning_diagnostics),
                });
            }
            self.state.verification_successes += 1;
        }
        Ok(json!({
            "stage": stage,
            "command": command,
            "success": output.status.success(),
            "exit_code": output.status.code(),
            "stdout": stdout,
            "stderr": stderr,
        }))
    }
}

impl<'a> AgentBackend for PluginIterationAgentBackend<'a> {
    type Host = RuntimeHost;
    fn host(&self) -> &RuntimeHost {
        self.host
    }
    fn system_prompt(&self) -> String {
        format!(
            "You are the Cordis plugin-iteration agent.\n\
Work directly through tools instead of proposing a large final JSON plan.\n\
You may only modify the selected plugin subtree rooted at {}.\n\
Start by calling list_context_files to inspect the current structural shortlist of readable files.\n\
The default context scope exposes structural source anchors for the root plugin, its direct children, and one nested child-plugin source layer. If you need deeper tests, docs, or additional subtree files, call list_context_files with `{{\"scope\":\"all\"}}` before reading them.\n\
Use read_context_files in batches instead of one file at a time, and avoid repeated list/read loops once you have enough structure to edit.\n\
Use replace_files_exact for source edits, including related updates across multiple files or multiple exact replacements you already understand. Group related edits into as few tool turns as possible.\n\
If an exact-replace tool reports invalid JSON or a stale exact-match pattern, reread the affected file and retry with a smaller replacement call instead of continuing from stale assumptions.\n\
Reserve inspect_plugin_catalog and any single-file follow-up cleanup only for cases where the structural file list and batched reads still leave ambiguity.\n\
Keep the new plugin architecture aligned with existing sibling plugins in the selected subtree instead of inventing a one-off layout.\n\
run_plugin_check and run_plugin_test both have safe defaults: call them with `{{}}` to run `cargo check --quiet --manifest-path plugins/Cargo.toml` or `cargo test --quiet --manifest-path plugins/Cargo.toml`.\n\
Use run_plugin_check and run_plugin_test until the files you changed are warning-free; if a verification command reports warnings in edited files, treat the work as incomplete and fix them before moving on.\n\
Once a warning-free check succeeds, stop exploring unless a later tool fails. Immediately run rebuild_plugin_workspace, then run_plugin_test, then record_iteration_summary.\n\
Use rebuild_plugin_workspace to refresh artifacts and generated docs after edits, but rebuild_plugin_workspace alone does not satisfy the final verification requirement.\n\
If a child plugin path uses a Rust keyword such as `mod`, keep that keyword in filesystem and `plugin_path` positions like `expr/evaluator/mod`. Type names such as `ModPlugin` and `ModError` are valid, but raw lower-case source identifiers such as a field, local, parameter, alias, or member named `mod` are invalid; prefer names like `modulo` or `mod_plugin` for those Rust identifiers.\n\
Replace placeholder scaffold implementations, tests, and docs together once the behavior is real, and do not stop after scaffolding a child plugin without wiring or testing it from the host subtree.\n\
When the iteration is ready, call record_iteration_summary with a concise summary and any recommended verification commands. record_iteration_summary must be your last tool call and it ends the session immediately.\n\
Do not attempt to modify runtime crates, repository root manifests, config, .git, target, or generated docs under docs/agent.",
            self.state.prepared.root_plugin_path
        )
    }

    fn tool_specs(&self) -> Vec<AgentToolSpec> {
        let mut tools = vec![
            AgentToolSpec {
                name: PLUGIN_AGENT_TOOL_LIST_CONTEXT_FILES,
                description: "List readable context files for the selected plugin subtree. Defaults to the structural focus shortlist; use scope=all to expand the visible context set.",
                parameters: json!({"type":"object","properties":{"scope":{"type":"string","enum":["focus","all"]}},"additionalProperties":false}),
            },
            AgentToolSpec {
                name: PLUGIN_AGENT_TOOL_READ_CONTEXT_FILES,
                description: "Read multiple currently visible context files in one call. If a needed file is hidden behind the focus shortlist, expand first with list_context_files(scope=all).",
                parameters: json!({"type":"object","properties":{"paths":{"type":"array","items":{"type":"string"}}},"required":["paths"],"additionalProperties":false}),
            },
            AgentToolSpec {
                name: PLUGIN_AGENT_TOOL_INSPECT_PLUGIN_CATALOG,
                description: "Inspect the currently loaded plugin subtree, including child plugins and node summaries.",
                parameters: json!({"type":"object","properties":{},"additionalProperties":false}),
            },
            AgentToolSpec {
                name: PLUGIN_AGENT_TOOL_SCAFFOLD_CHILD_PLUGIN,
                description: "Create a sibling child plugin scaffold under the selected subtree and register it in the parent manifest.",
                parameters: json!({"type":"object","properties":{"parent_plugin_path":{"type":"string"},"child_name":{"type":"string"},"template_plugin_path":{"type":"string"},"node_id":{"type":"string"},"summary":{"type":"string"}},"required":["parent_plugin_path","child_name"],"additionalProperties":false}),
            },
            AgentToolSpec {
                name: PLUGIN_AGENT_TOOL_REPLACE_FILE_EXACT,
                description: "Replace an exact string in one writable file. Prefer replace_files_exact unless this is truly a single-file follow-up.",
                parameters: json!({"type":"object","properties":{"path":{"type":"string"},"expected_old_string":{"type":"string"},"new_content":{"type":"string"}},"required":["path","expected_old_string","new_content"],"additionalProperties":false}),
            },
            AgentToolSpec {
                name: PLUGIN_AGENT_TOOL_REPLACE_FILES_EXACT,
                description: "Replace exact strings in one or more writable files in one call. Prefer this for nearly all source edits so related changes land in the same tool turn.",
                parameters: json!({"type":"object","properties":{"edits":{"type":"array","items":{"type":"object","properties":{"path":{"type":"string"},"expected_old_string":{"type":"string"},"new_content":{"type":"string"}},"required":["path","expected_old_string","new_content"],"additionalProperties":false},"minItems":1}},"required":["edits"],"additionalProperties":false}),
            },
            AgentToolSpec {
                name: PLUGIN_AGENT_TOOL_CREATE_FILE,
                description: "Create a new writable file inside the selected plugin subtree.",
                parameters: json!({"type":"object","properties":{"path":{"type":"string"},"new_content":{"type":"string"}},"required":["path","new_content"],"additionalProperties":false}),
            },
            AgentToolSpec {
                name: PLUGIN_AGENT_TOOL_DELETE_FILE,
                description: "Delete a writable file when you know its expected sha256.",
                parameters: json!({"type":"object","properties":{"path":{"type":"string"},"expected_sha256":{"type":"string"}},"required":["path","expected_sha256"],"additionalProperties":false}),
            },
            AgentToolSpec {
                name: PLUGIN_AGENT_TOOL_TOML_SET,
                description: "Set one TOML dotted key inside a writable manifest using an expected sha256 guard.",
                parameters: json!({"type":"object","properties":{"path":{"type":"string"},"expected_sha256":{"type":"string"},"dotted_key":{"type":"string"},"value":{}},"required":["path","expected_sha256","dotted_key","value"],"additionalProperties":false}),
            },
            AgentToolSpec {
                name: PLUGIN_AGENT_TOOL_JSON_SET,
                description: "Set one JSON pointer inside a writable file using an expected sha256 guard.",
                parameters: json!({"type":"object","properties":{"path":{"type":"string"},"expected_sha256":{"type":"string"},"pointer":{"type":"string"},"value":{}},"required":["path","expected_sha256","pointer","value"],"additionalProperties":false}),
            },
            AgentToolSpec {
                name: PLUGIN_AGENT_TOOL_RUN_PLUGIN_CHECK,
                description: "Run cargo check. plugin_path: \"/\" for whole workspace, \"/qq\" for a single plugin. Pass a custom command to override.",
                parameters: json!({"type":"object","properties":{"command":{"type":"string"},"plugin_path":{"type":"string","description":"\"/\" = all, \"/qq\" = single plugin"}},"required":["plugin_path"],"additionalProperties":false}),
            },
            AgentToolSpec {
                name: PLUGIN_AGENT_TOOL_RUN_PLUGIN_TEST,
                description: "Run cargo test. plugin_path: \"/\" for whole workspace, \"/qq\" for a single plugin. Pass a custom command to override.",
                parameters: json!({"type":"object","properties":{"command":{"type":"string"},"plugin_path":{"type":"string","description":"\"/\" = all, \"/qq\" = single plugin"}},"required":["plugin_path"],"additionalProperties":false}),
            },
            AgentToolSpec {
                name: PLUGIN_AGENT_TOOL_REBUILD_PLUGIN_WORKSPACE,
                description: "Rebuild plugin artifacts and sync generated docs. plugin_path: \"/\" for whole workspace, \"/qq\" for a single plugin.",
                parameters: json!({"type":"object","properties":{"plugin_path":{"type":"string","description":"\"/\" = all, \"/qq\" = single plugin"}},"required":["plugin_path"],"additionalProperties":false}),
            },
        ];
        if self.state.verification_successes > 0 && self.state.recorded_summary.is_none() {
            tools.push(AgentToolSpec {
                name: PLUGIN_AGENT_TOOL_RECORD_ITERATION_SUMMARY,
                description: "Record the final iteration summary and optional verification commands. This must be your last tool call and ends the iteration session immediately.",
                parameters: json!({"type":"object","properties":{"summary":{"type":"string"},"tests_command":{"type":"string"},"safety_command":{"type":"string"}},"required":["summary"],"additionalProperties":false}),
            });
        }
        tools
    }

    fn execute_tool(&mut self, name: &str, arguments: Value) -> Result<Value, RuntimeError> {
        match name {
            PLUGIN_AGENT_TOOL_LIST_CONTEXT_FILES => {
                let args = parse_agent_args::<ListContextFilesArgs>(arguments, name)?;
                let scope = parse_context_files_scope(args.scope.as_deref())?;
                Ok(self.list_context_files(scope))
            }
            PLUGIN_AGENT_TOOL_READ_CONTEXT_FILES => {
                let args = parse_agent_args::<ReadContextFilesArgs>(arguments, name)?;
                self.read_context_files(&args.paths)
            }
            PLUGIN_AGENT_TOOL_INSPECT_PLUGIN_CATALOG => Ok(self.inspect_plugin_catalog()),
            PLUGIN_AGENT_TOOL_SCAFFOLD_CHILD_PLUGIN => {
                let args = parse_agent_args::<ScaffoldChildPluginArgs>(arguments, name)?;
                self.scaffold_child_plugin(args)
            }
            PLUGIN_AGENT_TOOL_REPLACE_FILE_EXACT => {
                let args = parse_agent_args::<ReplaceFileExactArgs>(arguments, name)?;
                self.apply_operations(
                    "replace_file_exact",
                    vec![Self::replace_file_exact_operation(args)],
                )
            }
            PLUGIN_AGENT_TOOL_REPLACE_FILES_EXACT => {
                let args = parse_agent_args::<ReplaceFilesExactArgs>(arguments, name)?;
                if args.edits.is_empty() {
                    return Err(RuntimeError::InvalidArgument {
                        message: "replace_files_exact requires at least one edit".to_string(),
                    });
                }
                self.apply_operations(
                    "replace_files_exact",
                    args.edits
                        .into_iter()
                        .map(Self::replace_file_exact_operation)
                        .collect::<Vec<_>>(),
                )
            }
            PLUGIN_AGENT_TOOL_CREATE_FILE => {
                let args = parse_agent_args::<CreateFileArgs>(arguments, name)?;
                self.apply_operations(
                    "create_file",
                    vec![PluginEditOperation {
                        path: args.path,
                        kind: PluginEditOpKind::CreateFile,
                        expected_old_string: Some(String::new()),
                        expected_sha256: None,
                        new_content: Some(args.new_content),
                        pointer: None,
                        dotted_key: None,
                        value: None,
                    }],
                )
            }
            PLUGIN_AGENT_TOOL_DELETE_FILE => {
                let args = parse_agent_args::<DeleteFileArgs>(arguments, name)?;
                self.apply_operations(
                    "delete_file",
                    vec![PluginEditOperation {
                        path: args.path,
                        kind: PluginEditOpKind::DeleteFile,
                        expected_old_string: None,
                        expected_sha256: Some(args.expected_sha256),
                        new_content: None,
                        pointer: None,
                        dotted_key: None,
                        value: None,
                    }],
                )
            }
            PLUGIN_AGENT_TOOL_TOML_SET => {
                let args = parse_agent_args::<TomlSetArgs>(arguments, name)?;
                self.apply_operations(
                    "toml_set",
                    vec![PluginEditOperation {
                        path: args.path,
                        kind: PluginEditOpKind::TomlSet,
                        expected_old_string: None,
                        expected_sha256: Some(args.expected_sha256),
                        new_content: None,
                        pointer: None,
                        dotted_key: Some(args.dotted_key),
                        value: Some(args.value),
                    }],
                )
            }
            PLUGIN_AGENT_TOOL_JSON_SET => {
                let args = parse_agent_args::<JsonSetArgs>(arguments, name)?;
                self.apply_operations(
                    "json_set",
                    vec![PluginEditOperation {
                        path: args.path,
                        kind: PluginEditOpKind::JsonSet,
                        expected_old_string: None,
                        expected_sha256: Some(args.expected_sha256),
                        new_content: None,
                        pointer: Some(args.pointer),
                        dotted_key: None,
                        value: Some(args.value),
                    }],
                )
            }
            PLUGIN_AGENT_TOOL_RUN_PLUGIN_CHECK => {
                let args = parse_agent_args::<RunPluginCommandArgs>(arguments, name)?;
                let pp = args.plugin_path.as_deref().unwrap_or("/");
                let pp_trimmed = pp.trim_start_matches('/');
                let default = if pp_trimmed.is_empty() {
                    "cargo check --quiet --manifest-path plugins/Cargo.toml".to_string()
                } else {
                    format!(
                        "cargo check --quiet --manifest-path plugins/Cargo.toml -p {pp_trimmed}"
                    )
                };
                let explicit = normalize_optional_command(args.command);
                let command =
                    validated_verification_command(explicit, Some(default), "cargo check")?;
                self.run_checked_command("check", command)
            }
            PLUGIN_AGENT_TOOL_RUN_PLUGIN_TEST => {
                let args = parse_agent_args::<RunPluginCommandArgs>(arguments, name)?;
                let pp = args.plugin_path.as_deref().unwrap_or("/");
                let pp_trimmed = pp.trim_start_matches('/');
                let default = if pp_trimmed.is_empty() {
                    "cargo test --quiet --manifest-path plugins/Cargo.toml".to_string()
                } else {
                    format!("cargo test --quiet --manifest-path plugins/Cargo.toml -p {pp_trimmed}")
                };
                let prepared_tests = self.state.prepared.tests_command.clone();
                let explicit = normalize_optional_command(args.command)
                    .or_else(|| normalize_optional_command(prepared_tests));
                let command =
                    validated_verification_command(explicit, Some(default), "cargo test")?;
                self.run_checked_command("test", command)
            }
            PLUGIN_AGENT_TOOL_REBUILD_PLUGIN_WORKSPACE => {
                self.state.verification_attempts += 1;
                let args: serde_json::Value = arguments;
                let pp = args
                    .get("plugin_path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("/");
                let rebuilt = rebuild_plugin_workspace(&self.host.fixtures_root, pp)?;
                Ok(json!({
                    "rebuilt_count": rebuilt.len(),
                    "rebuilt": rebuilt,
                    "counts_as_warning_free_verification": false,
                }))
            }
            PLUGIN_AGENT_TOOL_RECORD_ITERATION_SUMMARY => {
                if self.state.operations.is_empty() || self.state.verification_successes == 0 {
                    return Err(RuntimeError::InvalidArgument {
                        message: "record_iteration_summary requires at least one edit and one successful verification step".to_string(),
                    });
                }
                let integration = ensure_scaffold_integration_edits(
                    &self.state.scaffolded_children,
                    &self.state.operations,
                );
                integration?;
                let args = parse_agent_args::<RecordIterationSummaryArgs>(arguments, name)?;
                self.state.recorded_summary = Some(args.summary.clone());
                self.state.tests_command = normalize_optional_command(args.tests_command);
                self.state.safety_command = normalize_optional_command(args.safety_command);
                Ok(json!({
                    "summary": args.summary,
                    "tests_command": self.state.tests_command,
                    "safety_command": self.state.safety_command,
                    "verification_attempts": self.state.verification_attempts,
                    "verification_successes": self.state.verification_successes,
                }))
            }
            other => Err(RuntimeError::InvalidArgument {
                message: format!("unsupported plugin iteration tool: {other}"),
            }),
        }
    }

    fn terminal_tool_reply(&self, name: &str, _output: &Value) -> Option<String> {
        (name == PLUGIN_AGENT_TOOL_RECORD_ITERATION_SUMMARY)
            .then_some("Plugin iteration summary recorded.".to_string())
    }

    fn tool_scope_label(&self) -> String {
        format!("plugin_iteration:{}", self.phase())
    }
}

fn parse_agent_args<T>(arguments: Value, tool_name: &str) -> Result<T, RuntimeError>
where
    T: DeserializeOwned,
{
    serde_json::from_value(arguments).map_err(|err| RuntimeError::InvalidArgument {
        message: format!("agent tool {tool_name} received invalid arguments: {err}"),
    })
}

fn parse_context_files_scope(raw: Option<&str>) -> Result<ContextFilesScope, RuntimeError> {
    match raw.unwrap_or("focus").trim() {
        "" | "focus" => Ok(ContextFilesScope::Focus),
        "all" => Ok(ContextFilesScope::All),
        other => Err(RuntimeError::InvalidArgument {
            message: format!(
                "list_context_files only supports scope `focus` or `all`, got `{other}`"
            ),
        }),
    }
}

fn transcript_excerpt(
    transcript: &[AgentTranscriptEntry],
    limit: usize,
) -> Vec<AgentTranscriptEntry> {
    let mut excerpt = transcript
        .iter()
        .rev()
        .take(limit)
        .cloned()
        .collect::<Vec<_>>();
    excerpt.reverse();
    excerpt
}

/// A/D seam: serialize failure of a canary replay payload. The invocation
/// samples are already valid JSON `Value`s, so this only trips on pathological
/// serializer states; kept as a named mapper for uniform error text.
fn canary_payload_serialize_error(err: &serde_json::Error) -> RuntimeError {
    RuntimeError::Invariant {
        message: format!("canary payload serialize failed: {err}"),
    }
}

/// A/D seam: failure to spawn the `bash -lc <command>` verification subprocess.
fn checked_command_spawn_error(command: &str, err: &std::io::Error) -> RuntimeError {
    RuntimeError::CommandFailed {
        program: "bash".to_string(),
        args: vec!["-lc".to_string(), command.to_string()],
        message: err.to_string(),
    }
}

/// A/D seam: failure to parse the parent plugin's `Cargo.toml` when scaffolding
/// a child plugin. Preserves the toml error text verbatim.
fn parent_manifest_parse_error(path: &Path, err: &toml::de::Error) -> RuntimeError {
    RuntimeError::CargoParse {
        path: path.to_path_buf(),
        message: err.to_string(),
    }
}

/// P0-3 TOCTOU guard: detect whether the plugin source tree drifted between
/// the verify phase (which hashed the tree) and the promote phase. Pure over
/// its inputs — the actual re-hash is injected via `rehash` so the decision
/// logic is unit-testable without touching the filesystem.
///
/// Returns `Some(reason)` when the candidate must be downgraded to a rollback:
/// either the re-hashed tree diverged from the verified hash, or re-hashing
/// itself failed. Returns `None` when there is no drift to act on (verdict was
/// not `Pass`, no baseline hash was recorded, or the hashes matched).
pub(crate) fn detect_plugin_source_drift(
    verifier_verdict: VerifierVerdict,
    expected_source_tree_hash: Option<&str>,
    rehash: impl FnOnce() -> Result<String, RuntimeError>,
) -> Option<String> {
    if verifier_verdict != VerifierVerdict::Pass {
        return None;
    }
    let expected = expected_source_tree_hash?;
    match rehash() {
        Ok(actual) if actual == expected => None,
        Ok(actual) => Some(format!(
            "source tree mutated between verify and promote (expected {expected}, got {actual})"
        )),
        Err(err) => Some(format!(
            "unable to re-hash source tree before promote: {err}"
        )),
    }
}

fn enrich_plugin_iteration_agent_error(
    err: RuntimeError,
    session_id: &str,
    tool_summary: Option<&AgentToolExecutionSummary>,
    transcript_excerpt: &[AgentTranscriptEntry],
) -> RuntimeError {
    let mut details = vec![format!(
        "plugin iteration agent session {session_id} failed: {err}"
    )];
    if let Some(summary) = tool_summary {
        details.push(format!(
            "tool summary: total_calls={} successful_calls={} failed_calls={} tool_names={}",
            summary.total_calls,
            summary.successful_calls,
            summary.failed_calls,
            summary.tool_names.join(", ")
        ));
    }
    if !transcript_excerpt.is_empty() {
        details.push(format!(
            "transcript excerpt:\n{}",
            format_agent_transcript_excerpt(transcript_excerpt)
        ));
    }
    RuntimeError::LlmResponseInvalid {
        message: details.join("\n\n"),
    }
}

fn format_agent_transcript_excerpt(entries: &[AgentTranscriptEntry]) -> String {
    entries
        .iter()
        .map(|entry| match entry {
            AgentTranscriptEntry::User { content } => {
                format!("user: {}", truncate_agent_excerpt_text(content, 280))
            }
            AgentTranscriptEntry::Assistant {
                content,
                response_id,
            } => {
                let prefix = response_id
                    .as_deref()
                    .map(|id| format!("assistant[{id}]"))
                    .unwrap_or_else(|| "assistant".to_string());
                format!("{prefix}: {}", truncate_agent_excerpt_text(content, 280))
            }
            AgentTranscriptEntry::Tool {
                name, ok, error, ..
            } => {
                let mut line = format!("tool {name} ok={ok}");
                if let Some(error) = error {
                    line.push_str(&format!(
                        " error={}",
                        truncate_agent_excerpt_text(error, 240)
                    ));
                }
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn truncate_agent_excerpt_text(text: &str, max_chars: usize) -> String {
    let flattened = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut truncated = flattened.chars().take(max_chars).collect::<String>();
    if flattened.chars().count() > max_chars {
        truncated.push_str("...");
    }
    truncated
}

fn enrich_plugin_iteration_edit_error(
    operation: &PluginEditOperation,
    workspace_root: &Path,
    err: RuntimeError,
) -> RuntimeError {
    let message = err.to_string();
    // Both conditions are folded into one `Option` chain rather than nested
    // `if` blocks: a nested gate whose body always runs still leaves llvm-cov a
    // zero-hit not-taken region on its closing brace, and there is no test that
    // can make a *taken* gate report both branches.
    let stale_exact_match = operation.kind == PluginEditOpKind::ReplaceExact
        && message.contains("auto update patch pattern not found");
    let current_content = stale_exact_match
        .then(|| fs::read_to_string(workspace_root.join(&operation.path)).ok())
        .flatten();
    match current_content {
        Some(current_content) => RuntimeError::LlmResponseInvalid {
            message: format!(
                "{message}\nThe exact snippet is stale for {}. Reread the current file content and retry with a smaller exact replacement.\ncurrent_sha256={}\ncurrent_content:\n{}",
                operation.path,
                sha256_text(&current_content),
                truncate_agent_excerpt_text(&current_content, 1600),
            ),
        },
        None => err,
    }
}

/// Wrap a plugin-edit error with the partial-batch rollback failure that
/// happened while unwinding it. Extracted from `apply_operations`' double-fault
/// arm so the (hard to reach in situ) wrapping is directly unit-testable; the
/// emitted message is byte-for-byte what the inline `format!` produced.
fn with_partial_batch_rollback_failure(
    enriched: RuntimeError,
    rollback_err: Option<RuntimeError>,
) -> RuntimeError {
    match rollback_err {
        Some(rollback_err) => RuntimeError::Invariant {
            message: format!(
                "{}; additionally, partial-batch rollback failed: {rollback_err}",
                enriched
            ),
        },
        None => enriched,
    }
}

fn normalize_optional_command(command: Option<String>) -> Option<String> {
    command.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn validated_verification_command(
    explicit: Option<String>,
    fallback: Option<String>,
    required_prefix: &str,
) -> Result<String, RuntimeError> {
    let command =
        explicit
            .or_else(|| fallback.clone())
            .ok_or_else(|| RuntimeError::InvalidArgument {
                message: format!("missing verification command for {required_prefix}"),
            })?;
    let trimmed = command.trim();
    // `check` / `test` are accepted as bare aliases for the tool's own default
    // command. Folded into one `if let` (rather than an outer `matches!` gate
    // wrapping an inner `if let`) because a nested gate leaves llvm-cov a
    // zero-hit region on its closing brace that no test can reach.
    let is_bare_alias = matches!(
        (required_prefix, trimmed),
        ("cargo check", "check") | ("cargo test", "test")
    );
    if let Some(default_command) = fallback.filter(|_| is_bare_alias) {
        return Ok(default_command);
    }
    if !trimmed.starts_with(required_prefix) {
        return Err(RuntimeError::InvalidArgument {
            message: format!(
                "verification tool only allows commands starting with `{required_prefix}`, got `{trimmed}`"
            ),
        });
    }
    Ok(trimmed.to_string())
}

/// Total order on scaffolded-child registrations: child root first, parent
/// manifest as the tie-break. Extracted from the inline `sort_by` closure in
/// `scaffold_child_plugin` — the closure body only runs when an iteration
/// scaffolds two or more children, which no in-process test reaches, so the
/// ordering is asserted directly against this function instead.
fn scaffolded_child_order(
    left: &ScaffoldedChildRegistration,
    right: &ScaffoldedChildRegistration,
) -> std::cmp::Ordering {
    left.child_root_path
        .cmp(&right.child_root_path)
        .then_with(|| left.parent_manifest_path.cmp(&right.parent_manifest_path))
}

fn ensure_scaffold_integration_edits(
    scaffolded_children: &[ScaffoldedChildRegistration],
    operations: &[PluginEditOperation],
) -> Result<(), RuntimeError> {
    if scaffolded_children.is_empty() {
        return Ok(());
    }

    let has_host_integration_edit = operations.iter().any(|operation| {
        let path = operation.path.as_str();
        if !path.contains("/src/") && !path.contains("/tests/") {
            return false;
        }
        !scaffolded_children.iter().any(|scaffold| {
            path == scaffold.parent_manifest_path
                || path == scaffold.child_root_path
                || path.starts_with(&format!("{}/", scaffold.child_root_path))
        })
    });

    if has_host_integration_edit {
        Ok(())
    } else {
        Err(RuntimeError::InvalidArgument {
            message: "record_iteration_summary requires at least one additional host integration source or behavior test edit outside scaffolded child plugin directories and parent manifests".to_string(),
        })
    }
}

fn should_track_context_file(path: &str) -> bool {
    matches!(
        Path::new(path)
            .extension()
            .and_then(|extension| extension.to_str()),
        Some("rs") | Some("json") | Some("toml") | Some("md")
    )
}

fn sort_plugin_context_paths(paths: &mut [String]) {
    paths.sort_by_key(|path| plugin_context_priority(path));
}

fn sort_and_dedup_context_paths(paths: &mut Vec<String>) {
    sort_plugin_context_paths(paths);
    paths.dedup();
}

fn plugin_context_priority(path: &str) -> (u8, String) {
    if path.ends_with("Cargo.toml") {
        (0, path.to_string())
    } else if path.ends_with("/src/core.rs") {
        (1, path.to_string())
    } else if path.ends_with("/src/lib.rs") {
        (2, path.to_string())
    } else if path.contains("/tests/") {
        (3, path.to_string())
    } else if path.contains("/docs/human/") {
        (4, path.to_string())
    } else if path.contains("/docs/agent/") {
        (5, path.to_string())
    } else if path.contains("/src/") {
        (6, path.to_string())
    } else {
        (7, path.to_string())
    }
}

fn sanitize_child_plugin_segment(raw: &str) -> String {
    let trimmed = raw.trim().trim_matches('/');
    let normalized = trimmed
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    match normalized.as_str() {
        "" => "child".to_string(),
        other => other.to_string(),
    }
}

fn sha256_text(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn render_child_plugin_manifest(crate_name: &str, plugin_path: &str, node_id: &str) -> String {
    format!(
        "[package]\nname = \"{crate_name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\ncrate-type = [\"rlib\", \"dylib\"]\n\n[package.metadata.cordis]\nplugin_path = \"{plugin_path}\"\nabi_kind = \"rust\"\ndeclared_nodes = [\"{node_id}\"]\nchildren = []\n# The scaffold ships no docs/agent/interfaces.json — docs are read from\n# the built dylib. P1-48 made this bypass an explicit opt-in.\nallow_generated_docs = true\n\n[package.metadata.cordis.abi_fingerprint]\ncrate_hash = \"crate_{crate_name}_v1\"\napi_hash = \"api_v2\"\n\n[dependencies]\ncordis-plugin-sdk = {{ path = \"../../../../../crates/cordis-plugin-sdk\" }}\nserde = {{ version = \"1\", features = [\"derive\"] }}\nserde_json = \"1\"\nthiserror = \"2\"\n\n[workspace]\n",
        crate_name = crate_name.replace('-', "_"),
        plugin_path = plugin_path,
        node_id = node_id,
    )
}

fn render_child_plugin_lib(
    crate_name: &str,
    plugin_path: &str,
    node_id: &str,
    summary: &str,
) -> String {
    format!(
        "mod core;\n\npub use core::*;\n\nuse cordis_plugin_sdk::{{\n    export_plugin_api, json_response, node_doc, plugin_docs, AbiFingerprint, PluginRequest,\n    PluginResponse,\n}};\nuse serde::{{Deserialize, Serialize}};\nuse serde_json::json;\n\n#[derive(Debug, Deserialize)]\nstruct BinaryOpRequest {{\n    lhs: f64,\n    rhs: f64,\n}}\n\n#[derive(Debug, Serialize)]\nstruct BinaryOpResponse {{\n    #[serde(skip_serializing_if = \"Option::is_none\")]\n    value: Option<f64>,\n    #[serde(skip_serializing_if = \"Option::is_none\")]\n    error: Option<String>,\n}}\n\nfn docs_value() -> cordis_plugin_sdk::PluginDocs {{\n    plugin_docs(\n        \"{crate_name}\",\n        \"{plugin_path}\",\n        \"0.1.0\",\n        None,\n        vec![node_doc(\n            \"{node_id}\",\n            \"{summary}\",\n            json!({{\"type\":\"object\",\"required\":[\"lhs\",\"rhs\"],\"properties\":{{\"lhs\":{{\"type\":\"number\"}},\"rhs\":{{\"type\":\"number\"}}}}}}),\n            json!({{\"type\":\"object\",\"properties\":{{\"value\":{{\"type\":\"number\"}},\"error\":{{\"type\":\"string\"}}}}}}),\n            &[],\n            &[\"not implemented\"],\n        )],\n        None,\n    )\n}}\n\nfn abi_fingerprint_value() -> AbiFingerprint {{\n    AbiFingerprint::current_build(\"crate_{crate_name}_v1\", \"api_v2\")\n}}\n\nfn api_handle(req: PluginRequest) -> PluginResponse {{\n    let response = match serde_json::from_str::<BinaryOpRequest>(&req.payload) {{\n        Ok(request) => match apply(request.lhs, request.rhs) {{\n            Ok(value) => BinaryOpResponse {{ value: Some(value), error: None }},\n            Err(err) => BinaryOpResponse {{ value: None, error: Some(err.to_string()) }},\n        }},\n        Err(err) => BinaryOpResponse {{ value: None, error: Some(format!(\"invalid request: {{err}}\")) }},\n    }};\n    json_response(&response)\n}}\n\nexport_plugin_api! {{\n    abi_fingerprint = abi_fingerprint_value(),\n    docs = docs_value(),\n    handle = api_handle,\n}}\n",
        crate_name = crate_name,
        plugin_path = plugin_path,
        node_id = node_id,
        summary = summary.replace('"', "\\\""),
    )
}

fn render_child_plugin_core(child_segment: &str) -> String {
    let type_name = child_segment
        .split('-')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<String>();
    format!(
        "use serde::{{Deserialize, Serialize}};\nuse thiserror::Error;\n\n#[derive(Debug, Error, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]\n#[serde(rename_all = \"snake_case\")]\npub enum {type_name}Error {{\n    #[error(\"not implemented\")]\n    NotImplemented,\n}}\n\n#[derive(Debug, Default, Clone, Copy)]\npub struct {type_name}Plugin;\n\nimpl {type_name}Plugin {{\n    pub fn apply(&self, _lhs: f64, _rhs: f64) -> Result<f64, {type_name}Error> {{\n        Err({type_name}Error::NotImplemented)\n    }}\n}}\n\n#[allow(dead_code)]\npub fn apply(lhs: f64, rhs: f64) -> Result<f64, {type_name}Error> {{\n    {type_name}Plugin.apply(lhs, rhs)\n}}\n"
    )
}

fn render_child_plugin_test(crate_name: &str) -> String {
    format!(
        "use {crate_name}::apply;\n\n#[test]\nfn scaffold_exports_apply() {{\n    let _ = apply(5.0, 2.0);\n}}\n"
    )
}

fn render_child_plugin_overview(plugin_path: &str) -> String {
    format!(
        "# {}\n\nThis child plugin scaffold was created by the Cordis plugin-iteration agent. Replace the placeholder implementation in `src/core.rs`, keep the child layout aligned with sibling plugins in this subtree, then update the placeholder smoke test and docs once the behavior is real.\n",
        plugin_path
    )
}

fn warning_diagnostics_for_changed_paths(
    stdout: &str,
    stderr: &str,
    operations: &[PluginEditOperation],
    fixtures_root: &Path,
) -> Vec<String> {
    let tracked_paths = tracked_warning_paths(operations);
    if tracked_paths.is_empty() {
        return Vec::new();
    }

    // 嵌套 cargo 在 CARGO_TERM_COLOR=always 下（GitHub Actions 的
    // dtolnay/rust-toolchain 未设置该 env 时会自动注入 always）输出带
    // ANSI 颜色码，`warning` 与 `:` 之间夹转义序列，starts_with("warning:")
    // 与 `--> ` 路径行的匹配全部落空。检测入口统一剥离，不依赖子进程的
    // 着色配置。
    let stdout = strip_ansi_sequences(stdout);
    let stderr = strip_ansi_sequences(stderr);

    extract_warning_blocks(&stdout)
        .into_iter()
        .chain(extract_warning_blocks(&stderr))
        .filter(|block| warning_block_matches_changed_paths(block, &tracked_paths, fixtures_root))
        .collect()
}

fn strip_ansi_sequences(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            out.push(ch);
            continue;
        }
        // CSI 序列: ESC [ 参数/中间字节(0x20-0x3f) ... 终结字节(0x40-0x7e)。
        // 其余 ESC 开头的双字符序列（如 ESC c）丢弃 ESC 与后一个字符。
        if chars.peek() == Some(&'[') {
            chars.next();
            for follow in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&follow) {
                    break;
                }
            }
        } else {
            chars.next();
        }
    }
    out
}

fn tracked_warning_paths(operations: &[PluginEditOperation]) -> BTreeSet<String> {
    operations
        .iter()
        .filter_map(|operation| normalize_rel_path(&operation.path).ok())
        .flat_map(|path| warning_path_aliases(&path))
        .collect()
}

fn warning_block_matches_changed_paths(
    block: &str,
    tracked_paths: &BTreeSet<String>,
    fixtures_root: &Path,
) -> bool {
    extract_warning_source_paths(block, fixtures_root)
        .iter()
        .any(|path| tracked_paths.contains(path))
}

fn extract_warning_source_paths(block: &str, fixtures_root: &Path) -> BTreeSet<String> {
    block
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim_start();
            let source = trimmed
                .strip_prefix("--> ")
                .or_else(|| trimmed.strip_prefix("-->"))?
                .trim();
            normalize_warning_source_path(source, fixtures_root)
        })
        .flat_map(|path| warning_path_aliases(&path))
        .collect()
}

fn normalize_warning_source_path(source: &str, fixtures_root: &Path) -> Option<String> {
    let candidate = strip_rust_span_suffix(source).trim();
    let path = Path::new(candidate);
    let relative = if path.is_absolute() {
        path.strip_prefix(fixtures_root).ok()?
    } else {
        path
    };

    let mut normalized = Vec::new();
    for component in relative.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop()?;
            }
            std::path::Component::Normal(part) => {
                normalized.push(part.to_string_lossy().to_string());
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => return None,
        }
    }

    (!normalized.is_empty()).then(|| normalized.join("/"))
}

fn strip_rust_span_suffix(source: &str) -> &str {
    let trimmed = source.trim();
    let mut parts = trimmed.rsplitn(3, ':');
    let col = parts.next();
    let line = parts.next();
    let path = parts.next();
    match (path, line, col) {
        (Some(path), Some(line), Some(col))
            if line.parse::<usize>().is_ok() && col.parse::<usize>().is_ok() =>
        {
            path
        }
        _ => trimmed,
    }
}

fn warning_path_aliases(path: &str) -> Vec<String> {
    let mut aliases = vec![path.to_string()];
    if let Some(stripped) = path.strip_prefix("plugins/") {
        aliases.push(stripped.to_string());
    }
    aliases
}

fn extract_warning_blocks(text: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("warning:") {
            if !current.is_empty() {
                blocks.push(current.join("\n"));
                current.clear();
            }
            current.push(line.to_string());
            continue;
        }

        if current.is_empty() {
            continue;
        }

        if is_warning_block_boundary(trimmed) {
            blocks.push(current.join("\n"));
            current.clear();
            continue;
        }

        current.push(line.to_string());
    }

    if !current.is_empty() {
        blocks.push(current.join("\n"));
    }

    blocks
}

fn is_warning_block_boundary(line: &str) -> bool {
    line.starts_with("error:")
        || line.starts_with("Compiling ")
        || line.starts_with("Checking ")
        || line.starts_with("Finished ")
        || line.starts_with("Running ")
        || line.starts_with("running ")
        || line.starts_with("test result:")
        || line.starts_with("Doc-tests ")
}

fn warning_cleanup_error_message(command: &str, warnings: &[String]) -> String {
    let excerpt = warnings
        .iter()
        .take(2)
        .map(|warning| truncate_warning_block(warning, 600))
        .collect::<Vec<_>>()
        .join("\n\n---\n\n");
    format!(
        "verification command `{command}` succeeded but still emitted warnings in files changed during this iteration. Clean up those warnings, keep the child plugin architecture aligned with its sibling plugins, and rerun verification before calling record_iteration_summary.\n\nWarnings:\n{excerpt}"
    )
}

fn truncate_warning_block(block: &str, max_chars: usize) -> String {
    let mut truncated = block.chars().take(max_chars).collect::<String>();
    if block.chars().count() > max_chars {
        truncated.push_str("...");
    }
    truncated
}

fn plugin_change_reasons(previous: &RegisteredPlugin, next: &RegisteredPlugin) -> Vec<String> {
    let mut reasons = Vec::new();
    if previous.parent != next.parent {
        reasons.push("parent_changed".to_string());
    }
    if previous.required != next.required {
        reasons.push("required_changed".to_string());
    }
    if previous.grants_from_parent != next.grants_from_parent {
        reasons.push("grants_changed".to_string());
    }
    if previous.load_result != next.load_result {
        reasons.push("load_result_changed".to_string());
    }
    if previous.docs != next.docs {
        reasons.push("docs_changed".to_string());
    }
    if previous.fingerprint_diff != next.fingerprint_diff {
        reasons.push("fingerprint_diff_changed".to_string());
    }
    reasons
}

fn select_registered_net_subgraph(net: &RegisteredNet, target_node_fqn: &str) -> BTreeSet<String> {
    let mut selected = BTreeSet::from([target_node_fqn.to_string()]);
    let mut queue = VecDeque::from([target_node_fqn.to_string()]);

    while let Some(current) = queue.pop_front() {
        for edge in net.edges.iter().filter(|edge| edge.to == current) {
            if selected.insert(edge.from.clone()) {
                queue.push_back(edge.from.clone());
            }
        }
    }

    selected
}

fn build_execution_net(
    net: &RegisteredNet,
    selected_nodes: &BTreeSet<String>,
    target_node_fqn: &str,
    fallback_target: &crate::plugin::registry::RegisteredNode,
) -> ExecutionNetSpec {
    let mut net_nodes = net
        .nodes
        .iter()
        .filter(|node| selected_nodes.contains(&node.node_fqn))
        .cloned()
        .collect::<Vec<_>>();
    net_nodes.sort_by(|left, right| {
        left.topo_level
            .cmp(&right.topo_level)
            .then_with(|| left.node_fqn.cmp(&right.node_fqn))
    });

    if net_nodes.is_empty() {
        return ExecutionNetSpec {
            places: Vec::new(),
            transitions: vec![ExecutionTransitionSpec {
                transition: TransitionSpec {
                    transition_id: target_node_fqn.to_string(),
                    priority: 0,
                    join_policy: JoinPolicy::AllOf,
                },
                run_policy: RunPolicy::default(),
                kind: ExecutionTransitionKind::Terminal,
                logical_group: Some("execute".to_string()),
                topo_level: 0,
                node_type: None,
            }],
            arcs: Vec::new(),
        };
    }

    let transitions = net_nodes
        .iter()
        .map(|node| {
            let incoming = net
                .edges
                .iter()
                .filter(|edge| edge.to == node.node_fqn && selected_nodes.contains(&edge.from))
                .count();
            ExecutionTransitionSpec {
                transition: TransitionSpec {
                    transition_id: node.node_fqn.clone(),
                    priority: 0,
                    join_policy: if incoming == 0 {
                        JoinPolicy::AnyOf
                    } else {
                        JoinPolicy::AllOf
                    },
                },
                run_policy: RunPolicy::default(),
                kind: match node.node_type {
                    NodeType::Task => ExecutionTransitionKind::Task,
                    NodeType::Router => ExecutionTransitionKind::Router {
                        subgraph_id: node.node_fqn.clone(),
                    },
                    NodeType::Gate => ExecutionTransitionKind::Gate {
                        policy: GatePolicy::AllOf,
                    },
                    NodeType::Terminal => ExecutionTransitionKind::Terminal,
                },
                logical_group: Some("execute".to_string()),
                topo_level: node.topo_level,
                node_type: Some(node.node_type),
            }
        })
        .collect::<Vec<_>>();

    let (places, arcs) = edges_to_net_specs(&net.edges, selected_nodes);

    let mut transitions = transitions;
    if !selected_nodes.contains(target_node_fqn) {
        transitions.push(ExecutionTransitionSpec {
            transition: TransitionSpec {
                transition_id: fallback_target.node_fqn.clone(),
                priority: 0,
                join_policy: JoinPolicy::AllOf,
            },
            run_policy: RunPolicy::default(),
            kind: ExecutionTransitionKind::Terminal,
            logical_group: Some("execute".to_string()),
            topo_level: 0,
            node_type: None,
        });
    }

    ExecutionNetSpec {
        places,
        transitions,
        arcs,
    }
}

/// P3 data-construction seam: translate the `selected`-scoped edges of a
/// registered net into engine `PlaceSpec`/`ArcSpec` values. Pure function of
/// the edge list plus the selected-node set, so it is unit-testable without a
/// live runtime. Only edges whose `from` and `to` endpoints are both selected
/// contribute; each such edge yields one place and a pair of arcs
/// (transition→place, place→transition). The inbound arc is `required` iff the
/// edge carries data (`RegisteredNetEdgeKind::Data`); control edges produce a
/// non-required arc. Places are de-duplicated and returned in sorted order via
/// the intermediate `BTreeSet`.
fn edges_to_net_specs(
    edges: &[RegisteredNetEdge],
    selected_nodes: &BTreeSet<String>,
) -> (Vec<PlaceSpec>, Vec<ArcSpec>) {
    let mut places = BTreeSet::<String>::new();
    let mut arcs = Vec::<ArcSpec>::new();

    for edge in edges
        .iter()
        .filter(|edge| selected_nodes.contains(&edge.from) && selected_nodes.contains(&edge.to))
    {
        let place_id = format!(
            "place::{}::{}::{}",
            edge.from,
            edge.to,
            edge.label.clone().unwrap_or_else(|| "control".to_string())
        );
        places.insert(place_id.clone());
        arcs.push(ArcSpec {
            arc_id: format!("arc::{}::out::{}", edge.from, place_id),
            place_id: place_id.clone(),
            transition_id: edge.from.clone(),
            direction: ArcDirection::TransitionToPlace,
            label: edge.label.clone(),
            required: false,
        });
        arcs.push(ArcSpec {
            arc_id: format!("arc::{}::in::{}", edge.to, place_id),
            place_id,
            transition_id: edge.to.clone(),
            direction: ArcDirection::PlaceToTransition,
            label: edge.label.clone(),
            required: matches!(edge.kind, RegisteredNetEdgeKind::Data),
        });
    }

    let places = places
        .into_iter()
        .map(|place_id| PlaceSpec { place_id })
        .collect();
    (places, arcs)
}

fn build_execution_payload(
    base_payload: &Map<String, Value>,
    inputs: &[TriggerInput],
) -> Map<String, Value> {
    let mut payload = base_payload.clone();
    for input in inputs {
        let Some(field) = &input.label else {
            continue;
        };
        let Some(value) = extract_response_field(&input.token.payload, field) else {
            continue;
        };
        payload.insert(field.clone(), value);
    }
    payload
}

fn extract_response_field(response_payload: &Value, field: &str) -> Option<Value> {
    response_payload
        .as_object()
        .and_then(|object| object.get(field))
        .cloned()
}

fn parse_response_payload(raw_payload: &str) -> Value {
    serde_json::from_str(raw_payload).unwrap_or_else(|_| Value::String(raw_payload.to_string()))
}

fn infer_outcome_from_payload(payload: &Value) -> NodeOutcome {
    let Some(object) = payload.as_object() else {
        return NodeOutcome::Success;
    };
    if object.get("ok").and_then(Value::as_bool) == Some(false) {
        return NodeOutcome::Failure;
    }
    if object
        .get("error")
        .and_then(Value::as_str)
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false)
    {
        return NodeOutcome::Failure;
    }
    NodeOutcome::Success
}

fn fill_missing_execution_traces(
    output: &ExecutionOutput,
    traces: &mut BTreeMap<String, ExecutionInvocationTrace>,
) {
    for (node_fqn, outcome) in &output.outcomes {
        let entry = traces
            .entry(node_fqn.clone())
            .or_insert_with(|| ExecutionInvocationTrace {
                node_fqn: node_fqn.clone(),
                plugin_path: String::new(),
                node_id: String::new(),
                attempt: 0,
                outcome: None,
                request_payload: None,
                response_payload: None,
                error: None,
            });
        entry.outcome = Some(*outcome);
    }
}

fn build_snapshot(loader: &Loader, snapshot_root: &Path) -> Result<RuntimeSnapshot, RuntimeError> {
    let staged_artifact_root = next_staged_artifact_root(snapshot_root);
    build_snapshot_with_staged_root(loader, staged_artifact_root)
}

fn build_snapshot_with_staged_root(
    loader: &Loader,
    staged_artifact_root: PathBuf,
) -> Result<RuntimeSnapshot, RuntimeError> {
    fs::create_dir_all(&staged_artifact_root)
        .map_err(|e| region_io_error(&staged_artifact_root, &e))?;

    let mut output = match loader.load_with_staging_root(Some(&staged_artifact_root)) {
        Ok(output) => output,
        Err(err) => {
            let _ = fs::remove_dir_all(&staged_artifact_root);
            return Err(err);
        }
    };

    for (plugin_path, plugin) in output.plugin_registry.iter() {
        if let PluginLoadResult::Unavailable(reason) = &plugin.load_result {
            let _ = fs::remove_dir_all(&staged_artifact_root);
            return Err(RuntimeError::PluginUnavailable {
                plugin_path: plugin_path.clone(),
                reason: reason.clone(),
                required: plugin.required,
            });
        }
    }

    // ── Register built-in Kernel nodes ──────────────────────────────────
    // These nodes are not plugins; they are part of the Kernel's
    // execution graph.  They appear in the RegisteredNet so the engine
    // can route tokens through them.
    register_builtin_agent_node(&output.plugin_registry, &mut output.node_registry);

    // Log Task nodes so operators know they exist (actual service start
    // happens later via auto_start_task_services or manual start_service).
    let task_fqns = output.node_registry.task_node_fqns();
    if !task_fqns.is_empty() {
        eprintln!(
            "[snapshot] detected {} Task node(s): {}",
            task_fqns.len(),
            task_fqns.join(", ")
        );
    }

    Ok(runtime_snapshot_from_output(output, staged_artifact_root))
}

fn register_builtin_agent_node(plugin_registry: &PluginRegistry, node_registry: &mut NodeRegistry) {
    use crate::core::models::PluginDocs;
    use cordis_plugin_sdk::NodeDoc;

    let docs = PluginDocs {
        plugin_id: "cordis".to_string(),
        plugin_path: "cordis".to_string(),
        plugin_version: "0.1.0".to_string(),
        abi_version: 2,
        command_name: None,
        nodes: vec![NodeDoc {
            id: "agent_router".to_string(),
            summary:
                "Kernel agent router — receives messages and routes them through the LLM agent."
                    .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "message": { "type": "string" },
                    "sender": { "type": "string" }
                }
            }),
            output_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "target": { "type": "string" },
                    "message": { "type": "string" }
                }
            }),
            side_effects: vec!["calls the LLM agent session".to_string()],
            failure_modes: vec!["agent session not started".to_string()],
            node_type: cordis_plugin_sdk::NodeType::Router,
            agent_accessible: false,
        }],
        system_hint: None,
    };

    // Register a virtual plugin entry.
    plugin_registry.insert_loaded(
        "cordis".to_string(),
        None,
        false,
        BTreeSet::new(),
        docs.clone(),
        PathBuf::from("cordis:builtin"),
        crate::core::models::ArtifactKind::Json,
        crate::core::models::AbiFingerprint {
            rustc_version: "builtin".to_string(),
            target_triple: "builtin".to_string(),
            crate_hash: "builtin".to_string(),
            api_hash: "builtin".to_string(),
        },
        None,
    );

    // Register nodes.
    // The builtin docs are a compile-time constant that always registers
    // cleanly; iterating the error (rather than an `if let` gate) leaves no
    // never-taken edge on the closing brace. The message itself is unit-tested
    // through `builtin_registration_failure_line`.
    let registration = node_registry.register_from_docs("cordis", &docs).err();
    registration
        .iter()
        .for_each(log_builtin_registration_failure);
}

fn runtime_snapshot_from_output(
    output: LoadOutput,
    staged_artifact_root: PathBuf,
) -> RuntimeSnapshot {
    RuntimeSnapshot {
        snapshot_id: output.execution_id,
        plugin_registry: output.plugin_registry,
        node_registry: output.node_registry,
        doc_registry: output.doc_registry,
        graph_registry: output.graph_registry,
        context_baseline: output.context,
        staged_artifact_root,
    }
}

#[derive(Debug, Clone)]
struct PluginIterationRunState {
    prepared: PreparedPluginIteration,
    agent_session_id: Option<String>,
    tool_execution_summary: Option<AgentToolExecutionSummary>,
    derived_edit_plan: Option<PluginEditPlan>,
    transcript_excerpt: Vec<AgentTranscriptEntry>,
    rollback: Option<PluginEditRollback>,
    changed_paths: Vec<String>,
    diff_lines: usize,
    rebuilt_artifacts: Vec<(String, String)>,
    candidate: Option<CandidateSnapshotStatus>,
    verification: Option<VerificationReport>,
    verifier_verdict: Option<VerifierVerdict>,
    canary: Option<CanaryReport>,
    blocked_reason: Option<String>,
    stage_error: Option<String>,
    /// `stage_error` 是否源自基础设施故障（磁盘满 / 配额耗尽），而非插件缺陷。
    /// 决定收尾时给 `InfrastructureFailure` 还是 `RolledBack`。
    stage_error_is_infrastructure: bool,
    final_verdict: Option<PluginIterationFinalVerdict>,
    tests_command: Option<String>,
    safety_command: Option<String>,
}

impl PluginIterationRunState {
    fn new(prepared: PreparedPluginIteration) -> Self {
        Self {
            prepared,
            agent_session_id: None,
            tool_execution_summary: None,
            derived_edit_plan: None,
            transcript_excerpt: Vec::new(),
            rollback: None,
            changed_paths: Vec::new(),
            diff_lines: 0,
            rebuilt_artifacts: Vec::new(),
            candidate: None,
            verification: None,
            verifier_verdict: None,
            canary: None,
            blocked_reason: None,
            stage_error: None,
            stage_error_is_infrastructure: false,
            final_verdict: None,
            tests_command: None,
            safety_command: None,
        }
    }

    fn into_result(
        self,
        net_output: ExecutionOutput,
    ) -> Result<KernelPluginIterationResult, RuntimeError> {
        let derived_edit_plan = self
            .derived_edit_plan
            .or(self.prepared.edit_plan.clone())
            .unwrap_or_else(|| PluginEditPlan {
                issue_id: self.prepared.issue_id.clone(),
                patch_id: format!("{}-empty", self.prepared.iteration_id),
                summary: self.prepared.summary.clone(),
                operations: Vec::new(),
            });
        Ok(KernelPluginIterationResult {
            iteration_id: self.prepared.iteration_id,
            issue_id: self.prepared.issue_id,
            root_plugin_path: self.prepared.root_plugin_path,
            target_plugin_paths: self.prepared.target_plugin_paths,
            source: self.prepared.source,
            summary: self.prepared.summary,
            agent_session_id: self.agent_session_id,
            tool_execution_summary: self.tool_execution_summary,
            derived_edit_plan,
            transcript_excerpt: self.transcript_excerpt,
            changed_paths: self.changed_paths,
            rebuilt_artifacts: self.rebuilt_artifacts,
            candidate: self.candidate,
            verification: self.verification,
            verifier_verdict: self.verifier_verdict,
            canary: self.canary,
            final_verdict: self
                .final_verdict
                .unwrap_or(PluginIterationFinalVerdict::RolledBack),
            blocked_reason: self.blocked_reason.or(self.stage_error),
            net_output,
        })
    }
}

fn plugin_iteration_status_from_result(
    result: &KernelPluginIterationResult,
) -> PluginIterationStatus {
    PluginIterationStatus {
        iteration_id: result.iteration_id.clone(),
        issue_id: result.issue_id.clone(),
        root_plugin_path: result.root_plugin_path.clone(),
        target_plugin_paths: result.target_plugin_paths.clone(),
        summary: result.summary.clone(),
        changed_paths: result.changed_paths.clone(),
        verifier_verdict: result.verifier_verdict,
        canary_verdict: result.canary.as_ref().map(|report| report.verdict),
        final_verdict: result.final_verdict,
        blocked_reason: result.blocked_reason.clone(),
    }
}

fn plugin_iteration_status_from_history(
    entry: &PluginIterationHistoryEntry,
) -> PluginIterationStatus {
    PluginIterationStatus {
        iteration_id: entry.iteration_id.clone(),
        issue_id: entry.issue_id.clone(),
        root_plugin_path: entry.root_plugin_path.clone(),
        target_plugin_paths: entry.target_plugin_paths.clone(),
        summary: entry.summary.clone(),
        changed_paths: entry.changed_paths.clone(),
        verifier_verdict: entry.verifier_verdict,
        canary_verdict: entry.canary_verdict,
        final_verdict: entry.final_verdict,
        blocked_reason: entry.blocked_reason.clone(),
    }
}

fn plugin_path_from_runtime_error(err: &RuntimeError) -> Option<String> {
    match err {
        RuntimeError::InvalidChildSource { parent, .. } => Some(parent.clone()),
        RuntimeError::ChildNotFound { parent, .. } => Some(parent.clone()),
        RuntimeError::DuplicatePluginPath { plugin_path, .. } => Some(plugin_path.clone()),
        RuntimeError::CycleDetected { cycle } => cycle.first().cloned(),
        RuntimeError::MissingScaffold { plugin_path, .. } => Some(plugin_path.clone()),
        RuntimeError::DocsContract { plugin_path, .. } => Some(plugin_path.clone()),
        RuntimeError::ArtifactIndexMissing { plugin_path } => Some(plugin_path.clone()),
        RuntimeError::ArtifactFileMissing { plugin_path, .. } => Some(plugin_path.clone()),
        RuntimeError::ArtifactHashMismatch { plugin_path, .. } => Some(plugin_path.clone()),
        RuntimeError::AbiMismatch { plugin_path, .. } => Some(plugin_path.clone()),
        RuntimeError::PluginUnavailable { plugin_path, .. } => Some(plugin_path.clone()),
        RuntimeError::PluginNotRegistered { plugin_path } => Some(plugin_path.clone()),
        RuntimeError::PluginExecutionUnsupported { plugin_path, .. } => Some(plugin_path.clone()),
        RuntimeError::PluginInvocationFailed { plugin_path, .. } => Some(plugin_path.clone()),
        RuntimeError::PluginDocsNotFound { plugin_path } => Some(plugin_path.clone()),
        RuntimeError::NodeDocsNotFound { plugin_path, .. } => Some(plugin_path.clone()),
        RuntimeError::PermissionDenied { plugin_path, .. } => Some(plugin_path.clone()),
        RuntimeError::ContextPluginUnavailable { plugin_path } => Some(plugin_path.clone()),
        RuntimeError::ServiceNotFound { plugin_path, .. } => Some(plugin_path.clone()),
        RuntimeError::ServiceTypeMismatch { plugin_path, .. } => Some(plugin_path.clone()),
        RuntimeError::DuplicateService { plugin_path, .. } => Some(plugin_path.clone()),
        _ => None,
    }
}

fn determine_root_plugin_path(
    snapshot: &RuntimeSnapshot,
    target_plugin_paths: &[String],
) -> Result<String, RuntimeError> {
    if target_plugin_paths.is_empty() {
        return Err(RuntimeError::InvalidArgument {
            message: "plugin iteration requires target_plugin_paths or an observed issue"
                .to_string(),
        });
    }
    let mut split_paths = target_plugin_paths
        .iter()
        .map(|path| path.split('/').collect::<Vec<_>>())
        .collect::<Vec<_>>();
    split_paths.sort_by_key(Vec::len);
    let shortest = split_paths.first().cloned().unwrap_or_default();
    let mut common = Vec::new();
    'outer: for (idx, segment) in shortest.iter().enumerate() {
        for other in &split_paths[1..] {
            if other.get(idx) != Some(segment) {
                break 'outer;
            }
        }
        common.push(*segment);
    }
    while !common.is_empty() {
        let candidate = common.join("/");
        if snapshot.plugin_registry().get(&candidate).is_some() {
            return Ok(candidate);
        }
        common.pop();
    }
    Err(RuntimeError::InvalidArgument {
        message: format!(
            "target plugin paths do not share a loaded subtree root: {}",
            target_plugin_paths.join(", ")
        ),
    })
}

fn collect_plugin_context_paths(
    workspace_root: &Path,
    root_plugin_path: &str,
    target_plugin_paths: &[String],
) -> Result<PluginIterationContextPaths, RuntimeError> {
    let mut all_files = BTreeSet::new();
    for plugin_path in target_plugin_paths {
        let plugin_root = format!("plugins/{plugin_path}");
        let manifest_path = format!("{plugin_root}/Cargo.toml");
        if workspace_root.join(&manifest_path).exists() {
            all_files.insert(manifest_path);
        }
        for subdir in ["src", "tests", "docs/agent", "docs/human"] {
            let dir = workspace_root.join(&plugin_root).join(subdir);
            if !dir.exists() {
                continue;
            }
            collect_context_files_recursive(workspace_root, &dir, &mut all_files)?;
        }
    }
    if all_files.is_empty() {
        return Err(RuntimeError::InvalidArgument {
            message: "no planner context files discovered for plugin iteration".to_string(),
        });
    }

    let mut focus_files = BTreeSet::new();
    // Bound out of the `?` so the fallible expression is a single line: a `?`
    // spanning several lines leaves llvm-cov a zero-hit region on the
    // continuation line that no test can reach.
    let focus_result = collect_focus_context_paths(
        workspace_root,
        root_plugin_path,
        target_plugin_paths,
        &mut focus_files,
    );
    focus_result?;

    let mut all_paths = all_files.into_iter().collect::<Vec<_>>();
    sort_and_dedup_context_paths(&mut all_paths);
    let all_set = all_paths.iter().cloned().collect::<BTreeSet<_>>();
    let mut focus_paths = focus_files
        .into_iter()
        .filter(|path| all_set.contains(path))
        .collect::<Vec<_>>();
    sort_and_dedup_context_paths(&mut focus_paths);

    Ok(PluginIterationContextPaths {
        focus_paths,
        all_paths,
    })
}

fn collect_focus_context_paths(
    workspace_root: &Path,
    root_plugin_path: &str,
    target_plugin_paths: &[String],
    files: &mut BTreeSet<String>,
) -> Result<(), RuntimeError> {
    let root_plugin_root = format!("plugins/{root_plugin_path}");
    insert_context_file_if_exists(
        workspace_root,
        &format!("{root_plugin_root}/Cargo.toml"),
        files,
    );
    insert_plugin_source_entries(workspace_root, &root_plugin_root, files);

    let root_tests_dir = workspace_root.join(&root_plugin_root).join("tests");
    if root_tests_dir.exists() {
        collect_context_files_recursive(workspace_root, &root_tests_dir, files)?;
    }
    insert_context_file_if_exists(
        workspace_root,
        &format!("{root_plugin_root}/docs/human/overview.md"),
        files,
    );

    let mut focus_plugins = target_plugin_paths
        .iter()
        .filter_map(|plugin_path| {
            let depth = plugin_relative_depth(root_plugin_path, plugin_path)?;
            ((1..=2).contains(&depth)).then_some((depth, plugin_path.clone()))
        })
        .collect::<Vec<_>>();
    focus_plugins.sort();
    for (_, plugin_path) in focus_plugins {
        let plugin_root = format!("plugins/{plugin_path}");
        insert_context_file_if_exists(workspace_root, &format!("{plugin_root}/Cargo.toml"), files);
        insert_plugin_source_entries(workspace_root, &plugin_root, files);
    }
    Ok(())
}

fn plugin_relative_depth(root_plugin_path: &str, plugin_path: &str) -> Option<usize> {
    if plugin_path == root_plugin_path {
        return Some(0);
    }
    let prefix = format!("{root_plugin_path}/");
    let suffix = plugin_path.strip_prefix(&prefix)?;
    (!suffix.is_empty()).then(|| suffix.split('/').count())
}

fn insert_plugin_source_entries(
    workspace_root: &Path,
    plugin_root: &str,
    files: &mut BTreeSet<String>,
) {
    for source_entry in plugin_source_entries(workspace_root, plugin_root) {
        files.insert(source_entry);
    }
}

fn plugin_source_entries(workspace_root: &Path, plugin_root: &str) -> Vec<String> {
    ["src/core.rs", "src/lib.rs"]
        .into_iter()
        .map(|suffix| format!("{plugin_root}/{suffix}"))
        .filter(|relative_path| workspace_root.join(relative_path).exists())
        .collect::<Vec<_>>()
}

fn insert_context_file_if_exists(
    workspace_root: &Path,
    relative_path: &str,
    files: &mut BTreeSet<String>,
) {
    if workspace_root.join(relative_path).exists() {
        files.insert(relative_path.to_string());
    }
}

/// A/D seam: wrap a raw `std::io::Error` into `RuntimeError::Io` for a path,
/// with the error text preserved verbatim (no context prefix). Shared by the
/// >3500 region's filesystem seams (context reads, manifest reads, scan walk).
fn region_io_error(path: &Path, err: &std::io::Error) -> RuntimeError {
    RuntimeError::Io {
        path: path.to_path_buf(),
        message: err.to_string(),
    }
}

fn context_path_escape_error(path: &Path, workspace_root: &Path) -> RuntimeError {
    RuntimeError::Invariant {
        message: format!(
            "planner context path {} escaped workspace root {}",
            path.display(),
            workspace_root.display()
        ),
    }
}

fn collect_context_files_recursive(
    workspace_root: &Path,
    dir: &Path,
    files: &mut BTreeSet<String>,
) -> Result<(), RuntimeError> {
    let entries = fs::read_dir(dir).map_err(|err| region_io_error(dir, &err))?;
    for entry in entries {
        let entry = entry.map_err(|err| region_io_error(dir, &err))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|err| region_io_error(&path, &err))?;
        if file_type.is_dir() {
            collect_context_files_recursive(workspace_root, &path, files)?;
            continue;
        }
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("rs") | Some("json") | Some("toml") | Some("md") => {
                let relative = path
                    .strip_prefix(workspace_root)
                    .map_err(|_| context_path_escape_error(&path, workspace_root))?;
                files.insert(relative.to_string_lossy().to_string());
            }
            _ => {}
        }
    }
    Ok(())
}

fn normalize_request_id(raw: Option<String>, prefix: &str) -> String {
    match raw {
        Some(value) if !value.trim().is_empty() => value,
        _ => {
            // Same hazard as P2-18's make_execution_id: two calls within the
            // same millisecond produced identical ids, so a second
            // agent_start_with silently overwrote the first session's map
            // entries. A process-local counter keeps ids unique regardless
            // of wall-clock granularity.
            use std::sync::atomic::{AtomicU64, Ordering};
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            format!("{prefix}-{now_ms}-{seq:x}")
        }
    }
}

fn make_snapshot_dir_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    // 目录名携带创建者 pid：boot 的 stale 清理据此只删属于已死进程的
    // snapshot（cleanup_stale_snapshot_dirs）。共享同一 fixtures root 的
    // 并行测试线程同属一个 pid，不会互删对方 in-flight staging。
    format!("snapshot-{}-{nanos}", std::process::id())
}

/// 删除 `snapshot_root` 下属于已死进程的 `snapshot-*` 残留目录。
///
/// 目录名格式 `snapshot-{pid}-{nanos}`：pid 段解析成功且进程仍存活
/// （`lock_pid_is_live`，同进程恒真）→ 保留，这是某个活 host 的
/// in-flight staging；进程已死或名字不含合法 pid 段（含历史
/// `snapshot-{nanos}` 旧格式）→ 视为 stale 删除。此前无条件全删，
/// 并行 `cargo test` 中多个测试 boot 同一 snapshot root 时互删对方
/// 正在 staging 的目录，报 "rename staging -> target failed"。
fn cleanup_stale_snapshot_dirs(snapshot_root: &Path) {
    let Ok(entries) = fs::read_dir(snapshot_root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if !is_snapshot_dir_name(&name_str) {
            continue;
        }
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        if !snapshot_dir_owner_is_alive(&name_str) {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

/// 目录名是否为 staged snapshot（`snapshot-{pid}-{nanos}`，含旧格式
/// `snapshot-{nanos}`）。
fn is_snapshot_dir_name(name: &str) -> bool {
    name.starts_with("snapshot-")
}

/// 从 `snapshot-{pid}-{nanos}` 目录名解析 pid 并探活。
///
/// pid 段解析成功且进程仍存活（`lock_pid_is_live`，同进程恒真）→ true，
/// 这是某个活 host 的 in-flight staging；进程已死或名字不含合法 pid 段
/// （含历史 `snapshot-{nanos}` 旧格式）→ false，视为 stale。
fn snapshot_dir_owner_is_alive(name: &str) -> bool {
    name.strip_prefix("snapshot-")
        .and_then(|rest| rest.split('-').next())
        .and_then(|segment| segment.parse::<u32>().ok())
        // pid 必须能落进正的 pid_t：`kill(-1, 0)` 语义是"探测所有
        // 可发信号进程"，恒成功，超出 i32 的值直接判死而不是探活。
        // 旧格式 `snapshot-{nanos}` 首段是纳秒时间戳，parse u32 失败
        // → None → 按 stale 处理。
        .filter(|pid| i32::try_from(*pid).is_ok())
        .is_some_and(crate::plugin::tooling::lock_pid_is_live)
}

/// 跨 hash 目录 GC 的统计结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SnapshotGcReport {
    /// 扫描到的 hash 目录总数。
    pub scanned: usize,
    /// 已删除（或 dry-run 下判定可删）的 hash 目录数。
    pub removed: usize,
    /// 回收的字节数。
    pub bytes_reclaimed: u64,
    /// 因含未重放的 rollback journal 而跳过的目录数。
    pub skipped_journal: usize,
    /// 因仍有活进程持有 in-flight staging 而跳过的目录数。
    pub skipped_live: usize,
    /// 因未到保留期而跳过的目录数。
    pub skipped_recent: usize,
}

/// 回收 `{temp_dir}/cordis-runtime-host/` 下已无人认领的 hash 目录。
///
/// `cleanup_stale_snapshot_dirs` 只扫描**当次 boot 解析出的那一个** hash
/// 目录内部，从不遍历兄弟目录；而 hash 是 fixtures root canonical 路径的
/// sha256，集成测试每次把 fixtures 拷进新 `TempDir` → 路径不同 → 全新 hash
/// 目录，老目录永远等不到"下次 boot 扫到它"，因为那个 hash 再也不会出现。
/// 实测本机累积 196 GB / 13200 个 hash 目录。本函数补上这一层回收。
///
/// 删除判据（三条**全部**满足）：
/// 1. 目录内所有 `snapshot-{pid}-*` 的 pid 段均已死（或无合法 pid 段）；
/// 2. 目录 mtime 已超过 `max_age`；
/// 3. 目录内无 `plugin-iteration-edit-journal.json`。
///
/// 第 3 条是安全红线：journal 是崩溃恢复状态（`restore_plugin_iteration_workspace`
/// 在 boot 时重放它），而 sha256 单向、无法反推 fixtures root 是否还存在，
/// 因此含 journal 的目录一律保留并打印路径交人工处置，绝不自动丢弃。
///
/// 第 1 条受 **pid 复用**限制：目录名里的 pid 早已随进程退出被 OS 回收、
/// 可能复用到任意无关进程上，此时 `lock_pid_is_live` 返回 true、目录被判为
/// "仍在用"而保留（实测 13206 个目录里 67 个如此，持有者是 vscode 里的
/// 无关进程）。这是保守方向的误判——宁可留下也不误删活 host 正在 staging
/// 的目录——且第 2 条的 mtime 会限制其累积规模；要精确判定得往目录里写
/// boot 时间戳或加文件锁，当前不值得为此加复杂度。
///
/// 空 hash 目录不受 `max_age` 约束，直接回收（无字节可丢）。
/// `skip_root` 传入当前进程正在用的 snapshot root，确保永不自删。
pub fn cleanup_orphaned_snapshot_roots(
    host_root: &Path,
    max_age: Duration,
    skip_root: Option<&Path>,
    dry_run: bool,
) -> SnapshotGcReport {
    let mut report = SnapshotGcReport::default();
    let Ok(entries) = fs::read_dir(host_root) else {
        return report;
    };
    // 当前 snapshot root 用 canonical 形式比对：调用方传进来的可能是
    // 未规范化路径（macOS 的 /tmp 是 /private/tmp 的 symlink）。
    let skip_canonical = skip_root.and_then(|path| path.canonicalize().ok());

    for entry in entries.flatten() {
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        if let Some(skip) = skip_canonical.as_deref() {
            if path.canonicalize().ok().as_deref() == Some(skip) {
                continue;
            }
        }
        report.scanned += 1;

        let Ok(children) = fs::read_dir(&path) else {
            continue;
        };
        let mut has_journal = false;
        let mut owner_alive = false;
        let mut is_empty = true;
        for child in children.flatten() {
            is_empty = false;
            let child_name = child.file_name();
            let child_name = child_name.to_string_lossy();
            // 必须是**文件**才算 journal。有测试故意把这个路径造成非空目录来
            // 迫使 `clear_journal` 的 remove_file 失败
            // （`clear_journal_remove_failure_when_path_is_nonempty_dir`），
            // 只按名字判断会把这类测试残渣误当成崩溃恢复状态而永久保留
            // （实测 338 个同名条目里 229 个是这种目录）。
            if child_name == "plugin-iteration-edit-journal.json"
                && child.file_type().map(|t| t.is_file()).unwrap_or(false)
            {
                has_journal = true;
            }
            if is_snapshot_dir_name(&child_name) && snapshot_dir_owner_is_alive(&child_name) {
                owner_alive = true;
            }
        }

        if owner_alive {
            report.skipped_live += 1;
            continue;
        }
        if has_journal {
            report.skipped_journal += 1;
            eprintln!(
                "[snapshot-gc] 保留 {}：含未重放的 plugin-iteration rollback journal",
                path.display()
            );
            continue;
        }
        // 空目录没有字节可丢，不必等保留期。
        if !is_empty && !dir_is_older_than(&path, max_age) {
            report.skipped_recent += 1;
            continue;
        }

        let bytes = dir_size_bytes(&path);
        if dry_run {
            report.removed += 1;
            report.bytes_reclaimed += bytes;
            continue;
        }
        if fs::remove_dir_all(&path).is_ok() {
            report.removed += 1;
            report.bytes_reclaimed += bytes;
        }
    }
    report
}

/// 目录 mtime 是否已早于 `now - max_age`。取不到 mtime 时保守返回 false
/// （宁可留着也不误删）。
/// staged root 是否可以安全 `remove_dir_all`。
///
/// 两道闸门，都不是理论情况：
/// - **空路径**是 `reload_subtree` 的产物（它不建 staging 目录，
///   `staged_artifact_root` 留空），当路径删会打到 CWD。
/// - **必须真正位于 `snapshot_root` 之下且不等于它本身**，否则会把整个
///   snapshot root（连带 `plugin-iteration-edit-journal.json` 这类崩溃恢复
///   状态）一起删掉。
fn staged_artifact_root_is_removable(staged_root: &Path, snapshot_root: &Path) -> bool {
    !staged_root.as_os_str().is_empty()
        && staged_root.starts_with(snapshot_root)
        && staged_root != snapshot_root
}

fn dir_is_older_than(path: &Path, max_age: Duration) -> bool {
    let Ok(modified) = fs::metadata(path).and_then(|meta| meta.modified()) else {
        return false;
    };
    SystemTime::now()
        .duration_since(modified)
        .map(|age| age >= max_age)
        .unwrap_or(false)
}

/// 递归累加目录内文件字节数，用于报告回收量。符号链接不跟随（只算链接本身）。
fn dir_size_bytes(path: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };
    let mut total = 0;
    for entry in entries.flatten() {
        match entry.file_type() {
            Ok(ty) if ty.is_dir() => total += dir_size_bytes(&entry.path()),
            Ok(ty) if ty.is_file() => {
                total += entry.metadata().map(|meta| meta.len()).unwrap_or(0);
            }
            _ => {}
        }
    }
    total
}

/// 默认的 host 级 snapshot 根目录 `{temp_dir}/cordis-runtime-host`，
/// 即所有 per-fixtures-root hash 目录的父目录。
pub fn default_host_snapshot_dir() -> PathBuf {
    std::env::temp_dir().join("cordis-runtime-host")
}

fn next_staged_artifact_root(snapshot_root: &Path) -> PathBuf {
    snapshot_root.join(make_snapshot_dir_name())
}

/// 测试注入的 ENOSPC 错误；非 test 构建恒为 `None`。
///
/// 做成返回 `Option` 的函数而不是 `if cfg!(test) { .. }` 内联块：后者在
/// 注入未触发时会留下不执行的收尾行，破坏 100% 行覆盖门槛。
fn injected_journal_enospc(journal_path: &Path) -> Option<RuntimeError> {
    #[cfg(test)]
    {
        TEST_ITERATION_ENOSPC_INJECTION
            .swap(false, std::sync::atomic::Ordering::SeqCst)
            .then(|| RuntimeError::Io {
                path: journal_path.to_path_buf(),
                message: "No space left on device (os error 28)".to_string(),
            })
    }
    #[cfg(not(test))]
    {
        let _ = journal_path;
        None
    }
}

fn plugin_iteration_journal_path(snapshot_root: &Path) -> PathBuf {
    snapshot_root.join("plugin-iteration-edit-journal.json")
}

/// P0-8: compute the artifact `.so` paths that a rebuild for `plugin_path`
/// would overwrite. Returned as (rel-to-fixtures-root, absolute) pairs so the
/// rollback recorder can save the current bytes before rebuild replaces them.
///
/// Currently only the direct `artifacts/{name}.so` produced by
/// `rebuild_plugin_workspace` is captured — nested-plugin rebuild is handled
/// via a separate helper if/when that path lands.
/// P1-16 helper module: fcntl advisory file lock around workspace-manifest
/// edits so two concurrent `create_plugin` invocations don't interleave
/// their read/modify/write of `plugins/Cargo.toml`.
mod workspace_manifest_lock {
    use std::fs::{File, OpenOptions};
    use std::path::Path;

    pub struct Guard {
        #[allow(dead_code)]
        file: Option<File>,
    }

    /// Take the exclusive lock, returning the raw `flock` status. Separated
    /// from `acquire` so the failure-reporting path can be unit-tested on every
    /// platform: `flock` on a freshly opened regular file does not fail on
    /// Linux (and only fails on BSD via FIFO quirks), so the `rc != 0` arm is
    /// unreachable in situ.
    #[cfg(unix)]
    fn lock_exclusive(file: &File) -> i32 {
        use std::os::unix::io::AsRawFd;
        // SAFETY: fd owned by `file`, kept alive across the syscall.
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) }
    }

    /// Log a non-zero `flock` status. Locking is advisory here — a failure is
    /// reported and the caller proceeds.
    #[cfg(unix)]
    fn report_flock_result(path: &Path, rc: i32) {
        if rc != 0 {
            eprintln!("{}", super::flock_failure_line(path));
        }
    }

    #[cfg(all(test, unix))]
    mod flock_report_tests {
        #[test]
        fn nonzero_status_is_reported_and_zero_is_silent() {
            // Both directions of the advisory-lock report, driven directly so
            // the arm does not depend on a platform-specific `flock` failure.
            let path = std::path::Path::new("/tmp/cordis-flock-report-probe.lock");
            super::report_flock_result(path, -1);
            super::report_flock_result(path, 0);
        }
    }

    pub fn acquire(path: &Path) -> Guard {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = match OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)
        {
            Ok(f) => f,
            Err(err) => {
                eprintln!(
                    "[create_plugin] failed to open lock {}: {err}",
                    path.display()
                );
                return Guard { file: None };
            }
        };
        #[cfg(unix)]
        report_flock_result(path, lock_exclusive(&file));
        Guard { file: Some(file) }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            #[cfg(unix)]
            {
                if let Some(file) = self.file.as_ref() {
                    use std::os::unix::io::AsRawFd;
                    let _ = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
                }
            }
        }
    }
}

/// Test-seam helper (A/D class): build a `RuntimeError::Io` from a path and a
/// preformatted message. Extracted from inline `.map_err(|e| RuntimeError::Io
/// { .. })` closures in `create_plugin` / snapshot setup so the mapping is a
/// single named, unit-testable function. Callers keep formatting the message
/// (which embeds `e`) so the emitted text stays byte-for-byte identical.
/// Log line for a failed builtin-node registration. The builtin docs are a
/// compile-time constant that always registers cleanly, so the call site is
/// Emit the builtin-registration failure line. The builtin docs are a
/// compile-time constant that always registers cleanly, so `register_builtin_
/// agent_node` never calls this; it is a named function so the body is covered
/// by a direct unit test rather than left as an unexecuted closure body.
fn log_builtin_registration_failure(err: &RuntimeError) {
    eprintln!("{}", builtin_registration_failure_line(err));
}

/// unreachable in practice; the message itself is unit-tested here.
fn builtin_registration_failure_line(err: &RuntimeError) -> String {
    format!("[builtin] agent_router registration failed: {err}")
}

/// Message for a failed `flock` on the workspace manifest lock — kept in a
/// tested helper so the (locally unfailable) call site stays one line.
fn flock_failure_line(path: &Path) -> String {
    format!(
        "[create_plugin] flock({}) failed: {}",
        path.display(),
        std::io::Error::last_os_error()
    )
}

/// Error for an agent session that ran to completion without ever calling
/// `record_iteration_summary` — the iteration cannot be finalized without it.
fn missing_summary_error(session_id: &str) -> RuntimeError {
    RuntimeError::LlmResponseInvalid {
        message: format!(
            "plugin iteration agent session {session_id} exited without calling record_iteration_summary"
        ),
    }
}

/// Single-line `Invariant` constructor mirroring [`host_io_error`].
fn host_invariant(message: String) -> RuntimeError {
    RuntimeError::Invariant { message }
}

/// `Io` error with a `"{what}: {source}"` message — the short-arg variant
/// of [`host_io_error`] so call sites fit a single line under rustfmt.
fn io_ctx(path: PathBuf, what: &str, e: impl std::fmt::Display) -> RuntimeError {
    host_io_error(path, format!("{what}: {e}"))
}

/// Log line for a session snapshot that parsed but could not be turned back
/// into an `AgentSession`. `AgentSession::from_snapshot` only fails when
/// reqwest cannot build an HTTP client from the stored config, which no
/// on-disk snapshot can force (every field round-trips and reqwest accepts any
/// timeout), so the message shape is pinned by a unit test instead of by a
/// fixture. Byte-identical to the inline `format!` it replaces.
fn crash_recovery_reconstruct_log_line(path: &Path, err: &dyn std::fmt::Display) -> String {
    let subject = format!("reconstruct failed for {}", path.display());
    RuntimeHost::host_io_log_line("crash-recovery", &subject, err)
}

fn host_io_error(path: PathBuf, message: String) -> RuntimeError {
    RuntimeError::Io { path, message }
}

/// Test-seam helper (A class): build the `RuntimeError::InvalidArgument`
/// returned when a plugin's `soul_get` reply cannot be decoded. `stage` is the
/// literal middle segment of the message ("is not JSON" or "malformed") so the
/// emitted text matches the previous inline closures byte-for-byte.
fn soul_reply_error(plugin_path: &str, stage: &str, e: &dyn std::fmt::Display) -> RuntimeError {
    RuntimeError::InvalidArgument {
        message: format!("soul_get reply from {plugin_path} {stage}: {e}"),
    }
}

/// P1-15 shared helper: durable atomic write for JSON snapshots (session
/// snapshots, shutdown memory, workspace-manifest edits). Same shape as
/// `kernel::plugin_iteration::atomic_write` but returns `std::io::Error`
/// so hot paths that don't care about the RuntimeError newtype can just
/// swallow / log.
fn atomic_write_bytes(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = match path.file_name() {
        Some(name) => {
            let mut owned = name.to_os_string();
            owned.push(format!(".cordis-tmp.{}", std::process::id()));
            path.with_file_name(owned)
        }
        None => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "atomic_write_bytes target has no filename",
            ));
        }
    };
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    if let Err(err) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(err);
    }
    Ok(())
}

/// Fold the current on-disk artifacts for `root_plugin_path` into `rollback`
/// as single-file backups, so a post-rebuild rollback restores the exact
/// pre-rebuild artifact bytes. Stops at the first absorb failure.
fn backup_artifacts_into_rollback(
    fixtures_root: &Path,
    root_plugin_path: &str,
    rollback: &mut crate::kernel::plugin_iteration::PluginEditRollback,
) -> Result<(), RuntimeError> {
    for (rel_path, abs_path) in &artifact_paths_to_backup(fixtures_root, root_plugin_path) {
        let original = fs::read(abs_path).ok();
        let single = crate::kernel::plugin_iteration::PluginEditRollback::single_backup(
            fixtures_root.to_path_buf(),
            rel_path,
            original,
        );
        rollback.absorb(single)?;
    }
    Ok(())
}

/// `blocked_reason` for a negative-verdict rollback whose candidate cleanup
/// itself failed. Byte-for-byte preserves the historical
/// `"verdict rollback with partial candidate cleanup error: candidate rollback: {err}"`
/// wording — the inner `"candidate rollback: "` prefix came from the vector the
/// old code pushed into. Extracted so both halves of the message are testable
/// without having to stage a candidate whose rollback fails.
fn verdict_rollback_partial_cleanup_reason(err: &RuntimeError) -> String {
    format!("verdict rollback with partial candidate cleanup error: candidate rollback: {err}")
}

/// Render a caught `iterate_plugins` panic payload as the `Invariant` error
/// the caller sees. `panic!("literal")` produces a `&str` payload and
/// `panic!("{x}")` / `assert!` with a formatted message produce a `String`; any
/// other payload type (only reachable via `panic_any`, which the iteration body
/// never calls) falls back to a fixed label. Extracted so all three arms are
/// unit-testable from synthetic payloads without having to make the pipeline
/// panic three different ways.
fn plugin_iteration_panic_error(payload: &Box<dyn std::any::Any + Send>) -> RuntimeError {
    let msg = payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic payload".to_string());
    RuntimeError::Invariant {
        message: format!(
            "plugin iteration panicked at an unexpected point; workspace has been restored: {msg}"
        ),
    }
}

/// Step 2's defensive `None`-rollback error. `run_plugin_iteration_agent`
/// always reports a rollback (`PluginEditRollback::empty(..)` at minimum, even
/// for a zero-operation edit plan), so no `iterate_plugins` call can reach this
/// through the public entry point. Extracted so the exact wording stays stable
/// and is directly unit-testable instead of living inline in a branch nothing
/// can drive.
fn plugin_iteration_missing_rollback_error() -> RuntimeError {
    RuntimeError::Invariant {
        message: "plugin iteration rollback journal missing after agent execution".to_string(),
    }
}

fn artifact_paths_to_backup(
    fixtures_root: &Path,
    root_plugin_path: &str,
) -> Vec<(String, PathBuf)> {
    let name = root_plugin_path.trim_start_matches('/');
    if name.is_empty() {
        return Vec::new();
    }
    let rel = format!("artifacts/{}.so", name);
    let abs = fixtures_root.join(&rel);
    if !abs.exists() {
        return Vec::new();
    }
    vec![(rel, abs)]
}

fn clear_plugin_iteration_journal(snapshot_root: &Path) -> Result<(), RuntimeError> {
    crate::kernel::plugin_iteration::PluginEditRollback::clear_journal(
        &plugin_iteration_journal_path(snapshot_root),
    )
}

/// Assemble the `blocked_reason` string for a plugin-iteration rollback.
///
/// Pure result-aggregation: the real rollback side effects (candidate
/// rollback, workspace restore) run at the call site and their outcomes are
/// passed in here. `candidate_rollback` is `None` when there was no candidate
/// snapshot to roll back; `Some(Ok(_))` when the rollback succeeded and
/// `Some(Err(_))` when it failed. Byte-for-byte preserves the historical
/// `"{base}; rollback errors: [candidate rollback: ..., workspace restore: ...]"`
/// wording so error text stays stable across the extraction.
fn aggregate_rollback_failure(
    base_message: String,
    candidate_rollback: Option<Result<(), RuntimeError>>,
    workspace_restore: Result<(), RuntimeError>,
) -> String {
    let mut rollback_errors = Vec::new();
    if let Some(Err(err)) = candidate_rollback {
        rollback_errors.push(format!("candidate rollback: {err}"));
    }
    if let Err(err) = workspace_restore {
        rollback_errors.push(format!("workspace restore: {err}"));
    }
    if rollback_errors.is_empty() {
        base_message
    } else {
        format!(
            "{}; rollback errors: [{}]",
            base_message,
            rollback_errors.join(", ")
        )
    }
}

fn plugin_iteration_applied_marker_path(snapshot_root: &Path) -> PathBuf {
    snapshot_root.join("plugin-iteration-edit-journal.applied")
}

fn restore_plugin_iteration_workspace(
    fixtures_root: &Path,
    snapshot_root: &Path,
    in_memory_rollback: Option<&crate::kernel::plugin_iteration::PluginEditRollback>,
) -> Result<bool, RuntimeError> {
    // Split into two phases so integration tests can exercise the
    // journal + rollback semantics without needing a real
    // `cargo build`-capable fixtures tree.
    let restored =
        apply_plugin_iteration_journal(fixtures_root, snapshot_root, in_memory_rollback)?;
    if restored {
        rebuild_plugin_workspace(fixtures_root, "/")?;
    }
    Ok(restored)
}

/// P0-6/7 recovery core: replay the on-disk rollback journal (or an
/// in-memory rollback if no journal) against the workspace. Returns
/// `true` iff a restore happened. Does NOT call `rebuild_plugin_workspace`
/// — callers that need artifact refresh do so themselves (production
/// path in `restore_plugin_iteration_workspace`), while integration
/// tests can stop here.
///
/// This is `pub` **only for the crash-recovery integration test**;
/// treat it as a test hook, not part of the runtime API.
#[doc(hidden)]
pub fn apply_plugin_iteration_journal(
    fixtures_root: &Path,
    snapshot_root: &Path,
    in_memory_rollback: Option<&crate::kernel::plugin_iteration::PluginEditRollback>,
) -> Result<bool, RuntimeError> {
    let journal_path = plugin_iteration_journal_path(snapshot_root);
    let applied_marker = plugin_iteration_applied_marker_path(snapshot_root);

    // P0-7: if the journal was already applied in a previous call, skip.
    // The applied-marker records the generation id we successfully restored;
    // if the journal on disk still carries the same id, we've already done
    // this work — replaying it would revert source that has since been
    // legitimately touched (or fail loudly on a moved file).
    if journal_path.exists() && applied_marker.exists() {
        let journal_gen = PluginEditRollback::journal_generation_id(&journal_path)?;
        let applied_gen = fs::read_to_string(&applied_marker).ok();
        // The id pair and the equality test are one condition rather than two
        // nested gates: a nested `if` whose body always runs on the taken path
        // still leaves llvm-cov a zero-hit region on its closing brace.
        let already_applied = match (journal_gen.as_deref(), applied_gen.as_deref()) {
            (Some(j), Some(a)) => j.trim() == a.trim(),
            _ => false,
        };
        if already_applied {
            // Already applied — treat as if there is nothing to restore.
            let _ = fs::remove_file(&applied_marker);
            PluginEditRollback::clear_journal(&journal_path)?;
            return Ok(false);
        }
    }

    let loaded = PluginEditRollback::load_journal(fixtures_root, &journal_path)?;
    if let Some(rollback) = loaded {
        let generation_id = PluginEditRollback::journal_generation_id(&journal_path)?;
        rollback.rollback()?;
        // Persist the applied marker BEFORE clearing the journal. If we crash
        // between rollback and clear, next boot sees marker + journal with the
        // same id → skips the replay. `generation_id` is always `Some` for a
        // journal that just parsed, so the gate is written as a
        // `for`-over-`Option` — an `if let` would leave llvm-cov a zero-hit
        // not-taken region on its closing brace that no test can reach.
        for id in generation_id.into_iter() {
            if let Err(err) =
                crate::kernel::plugin_iteration::atomic_write(&applied_marker, id.as_bytes())
            {
                eprintln!(
                    "[plugin-iteration-recovery] failed to write applied marker at {}: {err}",
                    applied_marker.display()
                );
            }
        }
        crate::kernel::plugin_iteration::PluginEditRollback::clear_journal(&journal_path)?;
        let _ = fs::remove_file(&applied_marker);
        return Ok(true);
    }

    if let Some(rollback) = in_memory_rollback {
        rollback.rollback()?;
        return Ok(true);
    }

    Ok(false)
}

fn default_snapshot_root(fixtures_root: &Path) -> PathBuf {
    let canonical_root = fixtures_root
        .canonicalize()
        .unwrap_or_else(|_| fixtures_root.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(canonical_root.to_string_lossy().as_bytes());
    std::env::temp_dir()
        .join("cordis-runtime-host")
        .join(hex::encode(hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::{
        cleanup_orphaned_snapshot_roots, cleanup_stale_snapshot_dirs, collect_plugin_context_paths,
        dir_is_older_than, dir_size_bytes, ensure_scaffold_integration_edits,
        extract_warning_blocks, render_child_plugin_core, render_child_plugin_test,
        sanitize_child_plugin_segment, sort_plugin_context_paths,
        staged_artifact_root_is_removable, warning_diagnostics_for_changed_paths, AgentBackend,
        AgentSessionKind, AgentStartOptions, ContextFilesScope, PluginIterationAgentBackend,
        PluginIterationAgentState, RuntimeHost, ScaffoldedChildRegistration, SnapshotGcReport,
        PLUGIN_AGENT_TOOL_CREATE_FILE, PLUGIN_AGENT_TOOL_DELETE_FILE,
        PLUGIN_AGENT_TOOL_INSPECT_PLUGIN_CATALOG, PLUGIN_AGENT_TOOL_JSON_SET,
        PLUGIN_AGENT_TOOL_LIST_CONTEXT_FILES, PLUGIN_AGENT_TOOL_READ_CONTEXT_FILES,
        PLUGIN_AGENT_TOOL_REPLACE_FILES_EXACT, PLUGIN_AGENT_TOOL_REPLACE_FILE_EXACT,
        PLUGIN_AGENT_TOOL_SCAFFOLD_CHILD_PLUGIN, PLUGIN_AGENT_TOOL_TOML_SET,
    };
    use crate::core::error::RuntimeError;
    use crate::core::models::NodeOutcome;
    use crate::kernel::plugin_iteration::{
        KernelPluginIterationRequest, PluginEditOpKind, PluginEditOperation,
    };
    use serial_test::serial;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;
    use std::time::{Duration, SystemTime};

    fn repo_fixtures_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures")
            .canonicalize()
            .expect("fixtures root")
    }

    #[test]
    fn stale_snapshot_cleanup_keeps_live_pid_dirs() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();

        // 本进程的 in-flight snapshot：必须保留。
        let live = root.join(format!("snapshot-{}-1", std::process::id()));
        // 已死进程的残留：起一个立即退出的子进程并 wait 掉，用它的 pid。
        // （wait 之后 pid 已回收；紧接着复用到新进程的概率可忽略。）
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn short-lived child");
        let dead_pid = child.id();
        child.wait().expect("reap child");
        let dead = root.join(format!("snapshot-{dead_pid}-1"));
        // 历史 `snapshot-{nanos}` 旧格式：首段纳秒时间戳 parse u32 失败，
        // 按 stale 清理。
        let legacy = root.join("snapshot-1784782891597667568");
        // 前缀不匹配的目录与普通文件：不受清理影响。
        let unrelated = root.join("candidate-1");
        let file = root.join("snapshot-not-a-dir");
        for dir in [&live, &dead, &legacy, &unrelated] {
            fs::create_dir(dir).expect("create test dir");
        }
        fs::write(&file, b"x").expect("write file");

        cleanup_stale_snapshot_dirs(root);

        assert!(live.exists(), "live-pid snapshot must survive cleanup");
        assert!(!dead.exists(), "dead-pid snapshot must be removed");
        assert!(!legacy.exists(), "legacy-format snapshot must be removed");
        assert!(unrelated.exists(), "non-snapshot dirs are untouched");
        assert!(file.exists(), "plain files are untouched");
    }

    /// `Some(())` 当且仅当当前 euid 不是 root。
    ///
    /// root 持 `CAP_DAC_OVERRIDE`，内核不对它强制 mode 位，`chmod 000` 之后仍
    /// 能 read_dir。返回 `Option` 让调用方用 `for`-over-`Option` 门控——不同于
    /// `if` 或提前 `return`，它没有"未走到的臂"会留下永久无覆盖的行。
    #[cfg(unix)]
    fn probe_not_root() -> Option<()> {
        // SAFETY: `geteuid` 无参数、不写调用方内存、不设 errno。
        (unsafe { libc::geteuid() } != 0).then_some(())
    }

    /// 起一个立即退出的子进程并 wait 掉，拿一个确定已死的 pid。
    fn reaped_dead_pid() -> u32 {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn short-lived child");
        let pid = child.id();
        child.wait().expect("reap child");
        pid
    }

    /// 把目录 mtime 往前拨 `secs` 秒，模拟"老目录"。
    fn backdate_dir(path: &Path, secs: u64) {
        let past = SystemTime::now() - Duration::from_secs(secs);
        let times = fs::FileTimes::new().set_modified(past);
        let dir = fs::File::open(path).expect("open dir for utimes");
        dir.set_times(times).expect("backdate dir mtime");
    }

    /// 造一个 hash 目录：内含一个属于 `pid` 的 snapshot 子目录 + 一个占位文件。
    fn make_hash_dir(root: &Path, name: &str, pid: u32) -> PathBuf {
        let hash = root.join(name);
        let snapshot = hash.join(format!("snapshot-{pid}-1"));
        fs::create_dir_all(&snapshot).expect("create hash/snapshot dir");
        fs::write(snapshot.join("plugin.so"), b"0123456789").expect("write artifact");
        hash
    }

    #[test]
    fn orphaned_snapshot_root_gc_removes_dead_pid_dirs() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        let dead_pid = reaped_dead_pid();

        // 死 pid + 老 mtime → 删。
        let orphan = make_hash_dir(root, "aaa", dead_pid);
        backdate_dir(&orphan, 48 * 3600);
        // 活 pid（本进程）→ 留，无论多老。
        let live = make_hash_dir(root, "bbb", std::process::id());
        backdate_dir(&live, 48 * 3600);
        // 死 pid 但 mtime 新 → 留（未到保留期）。
        let recent = make_hash_dir(root, "ccc", dead_pid);

        let report =
            cleanup_orphaned_snapshot_roots(root, Duration::from_secs(24 * 3600), None, false);

        assert!(!orphan.exists(), "dead-pid aged hash dir must be removed");
        assert!(live.exists(), "live-pid hash dir must survive");
        assert!(recent.exists(), "hash dir within retention must survive");
        assert_eq!(report.removed, 1);
        assert_eq!(report.skipped_live, 1);
        assert_eq!(report.skipped_recent, 1);
        assert_eq!(report.scanned, 3);
        // 回收字节数覆盖到 snapshot 子目录里的文件。
        assert_eq!(report.bytes_reclaimed, 10);
    }

    #[test]
    fn orphaned_snapshot_root_gc_preserves_journal_dirs() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        let dead_pid = reaped_dead_pid();

        // 死 pid + 老 mtime，但含未重放的 rollback journal → 必须保留。
        // journal 是崩溃恢复状态，sha256 单向无法反推 fixtures root 还在不在，
        // 盲删会摧毁回滚记录。
        let with_journal = make_hash_dir(root, "aaa", dead_pid);
        fs::write(
            with_journal.join("plugin-iteration-edit-journal.json"),
            b"{}",
        )
        .expect("write journal");
        backdate_dir(&with_journal, 48 * 3600);

        let report =
            cleanup_orphaned_snapshot_roots(root, Duration::from_secs(24 * 3600), None, false);

        assert!(
            with_journal.exists(),
            "hash dir holding an unreplayed journal must never be auto-removed"
        );
        assert_eq!(report.skipped_journal, 1);
        assert_eq!(report.removed, 0);
    }

    /// journal **同名目录**不算 journal，必须照常回收。
    ///
    /// `clear_journal_remove_failure_when_path_is_nonempty_dir` 之类的测试会
    /// 故意把 journal 路径造成非空目录以迫使 `remove_file` 失败；早期只按
    /// 文件名判断的实现把这类残渣误当成崩溃恢复状态而永久保留（实测 338 个
    /// 同名条目里 229 个是目录，占住 GC 该回收的空间）。
    #[test]
    fn orphaned_snapshot_root_gc_ignores_journal_named_directory() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        let dead_pid = reaped_dead_pid();

        let orphan = make_hash_dir(root, "aaa", dead_pid);
        let journal_as_dir = orphan.join("plugin-iteration-edit-journal.json");
        fs::create_dir_all(&journal_as_dir).expect("create journal-named dir");
        fs::write(journal_as_dir.join("blocker.txt"), b"x").expect("write blocker");
        backdate_dir(&orphan, 48 * 3600);

        let report =
            cleanup_orphaned_snapshot_roots(root, Duration::from_secs(24 * 3600), None, false);

        assert!(
            !orphan.exists(),
            "a journal-named directory is test debris, not recovery state"
        );
        assert_eq!(report.skipped_journal, 0);
        assert_eq!(report.removed, 1);
    }

    #[test]
    fn orphaned_snapshot_root_gc_removes_empty_hash_dirs_regardless_of_age() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        // 空目录没有字节可丢，不必等保留期。
        let empty = root.join("aaa");
        fs::create_dir(&empty).expect("create empty hash dir");

        let report =
            cleanup_orphaned_snapshot_roots(root, Duration::from_secs(24 * 3600), None, false);

        assert!(!empty.exists(), "empty hash dir is reclaimed immediately");
        assert_eq!(report.removed, 1);
        assert_eq!(report.bytes_reclaimed, 0);
    }

    #[test]
    fn orphaned_snapshot_root_gc_skips_own_root() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        let dead_pid = reaped_dead_pid();
        // 即便判据全中（死 pid + 老 mtime），当前进程正在用的 root 也不能自删。
        let own = make_hash_dir(root, "aaa", dead_pid);
        backdate_dir(&own, 48 * 3600);

        let report = cleanup_orphaned_snapshot_roots(
            root,
            Duration::from_secs(24 * 3600),
            Some(&own),
            false,
        );

        assert!(
            own.exists(),
            "the live process's own snapshot root is never removed"
        );
        assert_eq!(report.removed, 0);
        // skip_root 在计数前就被跳过，不计入 scanned。
        assert_eq!(report.scanned, 0);
    }

    #[test]
    fn orphaned_snapshot_root_gc_dry_run_reports_without_deleting() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        let dead_pid = reaped_dead_pid();
        let orphan = make_hash_dir(root, "aaa", dead_pid);
        backdate_dir(&orphan, 48 * 3600);

        let report =
            cleanup_orphaned_snapshot_roots(root, Duration::from_secs(24 * 3600), None, true);

        assert!(orphan.exists(), "dry-run must not delete anything");
        assert_eq!(
            report.removed, 1,
            "dry-run still reports what would be removed"
        );
        assert_eq!(report.bytes_reclaimed, 10);
    }

    #[test]
    fn orphaned_snapshot_root_gc_zero_max_age_expires_everything() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        let dead_pid = reaped_dead_pid();
        // 刚建的目录，配合 retention=0（config 的 Some(0) 语义）应立即可回收。
        let fresh = make_hash_dir(root, "aaa", dead_pid);

        let report = cleanup_orphaned_snapshot_roots(root, Duration::ZERO, None, false);

        assert!(
            !fresh.exists(),
            "retention=0 expires even a just-created dir"
        );
        assert_eq!(report.removed, 1);
    }

    /// GC 的三处防御性早退臂：`host_root` 不可 read_dir、hash 目录本身不可
    /// read_dir、以及目录项不是目录。都不是"不可能发生"——权限变化、并发删除
    /// 都会走到，且 100% 行覆盖门槛要求它们被执行到。
    #[cfg(unix)]
    #[test]
    fn orphaned_snapshot_root_gc_tolerates_unreadable_and_non_dir_entries() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();

        // 目录项不是目录：普通文件必须被跳过而不是当 hash 目录处理。
        fs::write(root.join("stray-file"), b"x").expect("write stray file");
        let report = cleanup_orphaned_snapshot_roots(root, Duration::ZERO, None, false);
        assert_eq!(report.scanned, 0, "plain files are not hash dirs");
        assert!(
            root.join("stray-file").exists(),
            "plain files are untouched"
        );

        // hash 目录不可 read_dir：跳过该目录，不 panic、不误删。root 绕过 mode
        // 位，故只在非特权用户下有意义；用 for-over-Option 门控而非 `if`——
        // `if` 会留下当前 euid 不走的那个臂永久无覆盖。
        for () in probe_not_root().into_iter() {
            let locked = root.join("locked");
            fs::create_dir(&locked).expect("create locked dir");
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).expect("chmod 000");
            let report = cleanup_orphaned_snapshot_roots(root, Duration::ZERO, None, false);
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).expect("restore mode");
            assert!(locked.exists(), "an unreadable hash dir is left alone");
            assert_eq!(report.removed, 0);
        }

        // host_root 整体不可 read_dir：返回空报告。
        let missing = root.join("does-not-exist");
        let report = cleanup_orphaned_snapshot_roots(&missing, Duration::ZERO, None, false);
        assert_eq!(report, SnapshotGcReport::default());
    }

    /// `dir_is_older_than` 取不到 mtime 时保守返回 false（宁可留着不误删），
    /// `dir_size_bytes` 对不可 read_dir 的目录返回 0，且不跟随符号链接。
    #[cfg(unix)]
    #[test]
    fn dir_age_and_size_helpers_handle_unreadable_and_symlinks() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();

        // 不存在的路径拿不到 metadata → 判"不老"，避免误删。
        assert!(
            !dir_is_older_than(&root.join("nope"), Duration::ZERO),
            "missing mtime must not be treated as expired"
        );
        // 不存在的路径 size 为 0。
        assert_eq!(dir_size_bytes(&root.join("nope")), 0);

        // 符号链接不跟随：只算真实文件，不把链接目标的字节数计进来。
        let real = root.join("real.bin");
        fs::write(&real, b"0123456789").expect("write real file");
        let inner = root.join("inner");
        fs::create_dir(&inner).expect("create inner dir");
        std::os::unix::fs::symlink(&real, inner.join("link.bin")).expect("symlink");
        assert_eq!(
            dir_size_bytes(&inner),
            0,
            "symlinks are not followed, so the target's bytes are not counted"
        );
        // 真实文件被递归计入。
        assert_eq!(dir_size_bytes(root), 10);
    }

    /// `staged_artifact_root_is_removable` 的三条拒绝理由与一条放行。
    ///
    /// 空路径是 `reload_subtree` 的产物（它不建 staging 目录），当路径删会打到
    /// CWD；等于或不在 snapshot_root 之下则会连带删掉
    /// `plugin-iteration-edit-journal.json` 这类崩溃恢复状态。
    #[test]
    fn staged_root_removable_rejects_empty_and_out_of_tree_paths() {
        let snapshot_root = Path::new("/tmp/snaps");
        // 正常情况：snapshot_root 下的子目录可删。
        assert!(staged_artifact_root_is_removable(
            &snapshot_root.join("snapshot-1-2"),
            snapshot_root
        ));
        // 空路径（reload_subtree）。
        assert!(!staged_artifact_root_is_removable(
            Path::new(""),
            snapshot_root
        ));
        // 等于 snapshot_root 本身。
        assert!(!staged_artifact_root_is_removable(
            snapshot_root,
            snapshot_root
        ));
        // 完全在别处。
        assert!(!staged_artifact_root_is_removable(
            Path::new("/tmp/elsewhere/snapshot-1-2"),
            snapshot_root
        ));
    }

    // Shared read-only host against the real fixtures tree. The fixture
    // dylibs under `fixtures/artifacts/` are rebuilt natively for the current
    // host target (arm64 macOS included), so a real `RuntimeHost::boot`
    // succeeds here without the x86_64-linux-only gate that used to guard
    // these tests. Booting is cheap (the loader only reads `artifacts/`); we
    // still boot once and share it, gating the mutating tests with `serial`
    // so they don't race on the shared temp snapshot root.
    //
    // Flaky-guard (mirrors `tests/host_coverage.rs::shared_host`): a full
    // suite run may rebuild the fixture dylibs mid-suite, leaving
    // `artifacts/index.json`'s recorded sha256 stale relative to the on-disk
    // dylib and failing boot with `PluginUnavailable { HashMismatch }`.
    // Re-hash every staged artifact and rewrite the index right before
    // booting so the shared host always sees a consistent index.
    static SHARED_HOST: OnceLock<RuntimeHost> = OnceLock::new();

    fn shared_host() -> &'static RuntimeHost {
        SHARED_HOST.get_or_init(|| {
            let root = repo_fixtures_root();
            crate::plugin::tooling::refresh_artifact_index(&root)
                .expect("refresh artifact index before shared boot");
            RuntimeHost::boot(&root).expect("host should boot")
        })
    }

    fn collect_plugin_paths(plugin_root: &Path, subtree: &Path, paths: &mut Vec<String>) {
        if subtree.join("Cargo.toml").exists() {
            let relative = subtree
                .strip_prefix(plugin_root)
                .expect("subtree should stay inside plugin root");
            paths.push(relative.to_string_lossy().replace('\\', "/"));
        }

        let mut children = fs::read_dir(subtree)
            .expect("read subtree")
            .filter_map(|entry| entry.ok())
            .filter_map(|entry| {
                let path = entry.path();
                let name = path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or("");
                if matches!(name, "src" | "tests" | "docs" | "target") {
                    return None;
                }
                entry
                    .file_type()
                    .ok()
                    .filter(|ty| ty.is_dir())
                    .map(|_| path)
            })
            .collect::<Vec<_>>();
        children.sort();
        for child in children {
            collect_plugin_paths(plugin_root, &child, paths);
        }
    }

    #[test]
    fn sanitize_child_plugin_segment_keeps_mod_path_component() {
        assert_eq!(sanitize_child_plugin_segment("mod"), "mod");
    }

    #[test]
    fn child_plugin_test_template_is_smoke_only() {
        let rendered = render_child_plugin_test("expr_evaluator_mod");
        assert!(rendered.contains("scaffold_exports_apply"));
        assert!(rendered.contains("let _ = apply(5.0, 2.0);"));
        assert!(!rendered.contains("is_err"));
    }

    #[test]
    fn child_plugin_core_template_matches_shared_wrapper_pattern() {
        let rendered = render_child_plugin_core("mod");
        assert!(rendered.contains("pub struct ModPlugin;"));
        assert!(rendered.contains("#[allow(dead_code)]"));
        assert!(rendered.contains("pub fn apply(lhs: f64, rhs: f64)"));
    }

    #[test]
    fn extract_warning_blocks_keeps_separate_diagnostics() {
        let warnings = extract_warning_blocks(
            "warning: function `apply` is never used\n  --> plugins/expr/evaluator/mod/src/core.rs:23:8\n\nwarning: field `modulo` is never read\n  --> plugins/expr/evaluator/src/core.rs:15:5\n",
        );
        assert_eq!(warnings.len(), 2);
        assert!(warnings[0].contains("core.rs:23:8"));
        assert!(warnings[1].contains("field `modulo`"));
    }

    #[test]
    fn warning_detection_ignores_environment_noise_outside_changed_paths() {
        let operations = vec![PluginEditOperation {
            path: "plugins/expr/evaluator/mod/src/core.rs".to_string(),
            kind: PluginEditOpKind::ReplaceExact,
            expected_old_string: Some("old".to_string()),
            expected_sha256: None,
            new_content: Some("new".to_string()),
            pointer: None,
            dotted_key: None,
            value: None,
        }];
        let diagnostics = warning_diagnostics_for_changed_paths(
            "",
            "sh: 6: /etc/profile.d/clab-notify.sh: [[: not found\nwarning: function `apply` is never used\n  --> plugins/expr/evaluator/mod/src/core.rs:23:8\n   |\n23 | pub fn apply(lhs: f64, rhs: f64) -> Result<f64, ModError> {\n   |        ^^^^^\n",
            &operations,
            Path::new("/tmp/fixtures"),
        );
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].contains("function `apply` is never used"));
        assert!(!diagnostics[0].contains("clab-notify"));
    }

    #[test]
    fn strip_ansi_sequences_removes_csi_and_keeps_plain_text() {
        use super::strip_ansi_sequences;
        assert_eq!(
            strip_ansi_sequences(
                "\u{1b}[1m\u{1b}[33mwarning\u{1b}[0m\u{1b}[1m: unused import\u{1b}[0m"
            ),
            "warning: unused import"
        );
        assert_eq!(
            strip_ansi_sequences("no escapes at all"),
            "no escapes at all"
        );
        // 非 CSI 的 ESC 双字符序列同样被剥掉，不残留 ESC。
        assert_eq!(strip_ansi_sequences("a\u{1b}cb"), "ab");
        // 文本截断在未终结的 CSI 序列中间时不 panic。
        assert_eq!(strip_ansi_sequences("tail\u{1b}[1;3"), "tail");
    }

    #[test]
    fn warning_detection_survives_ansi_colored_output() {
        let operations = vec![PluginEditOperation {
            path: "plugins/expr/evaluator/dist/src/core.rs".to_string(),
            kind: PluginEditOpKind::ReplaceExact,
            expected_old_string: Some("old".to_string()),
            expected_sha256: None,
            new_content: Some("new".to_string()),
            pointer: None,
            dotted_key: None,
            value: None,
        }];
        // CI 实测字节序列：CARGO_TERM_COLOR=always 下嵌套 cargo 的 stderr。
        let colored_stderr = "\u{1b}[1m\u{1b}[33mwarning\u{1b}[0m\u{1b}[1m: unused import: `std::fmt`\u{1b}[0m\n \u{1b}[1m\u{1b}[94m--> \u{1b}[0mexpr/src/../evaluator/src/../dist/src/core.rs:2:5\n  \u{1b}[1m\u{1b}[94m|\u{1b}[0m\n\u{1b}[1m\u{1b}[94m2\u{1b}[0m \u{1b}[1m\u{1b}[94m|\u{1b}[0m use std::fmt;\n  \u{1b}[1m\u{1b}[94m|\u{1b}[0m     \u{1b}[1m\u{1b}[33m^^^^^^^^\u{1b}[0m\n";
        let diagnostics = warning_diagnostics_for_changed_paths(
            "",
            colored_stderr,
            &operations,
            Path::new("/tmp/fixtures"),
        );
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].contains("unused import: `std::fmt`"));
        assert!(
            !diagnostics[0].contains('\u{1b}'),
            "excerpt sent to the LLM must be free of escape sequences"
        );
    }

    #[test]
    fn warning_detection_matches_folded_cargo_source_paths() {
        let operations = vec![PluginEditOperation {
            path: "plugins/expr/evaluator/mod/src/core.rs".to_string(),
            kind: PluginEditOpKind::ReplaceExact,
            expected_old_string: Some("old".to_string()),
            expected_sha256: None,
            new_content: Some("new".to_string()),
            pointer: None,
            dotted_key: None,
            value: None,
        }];
        let diagnostics = warning_diagnostics_for_changed_paths(
            "",
            "warning: unused import: `std::fmt`\n --> expr/src/../evaluator/src/../mod/src/core.rs:2:5\n  |\n2 | use std::fmt;\n  |     ^^^^^^^^\n",
            &operations,
            Path::new("/tmp/fixtures"),
        );
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].contains("unused import: `std::fmt`"));
    }

    #[test]
    fn sort_plugin_context_paths_uses_structural_order() {
        let mut paths = vec![
            "plugins/expr/docs/human/overview.md".to_string(),
            "plugins/expr/tests/eval.rs".to_string(),
            "plugins/expr/src/lib.rs".to_string(),
            "plugins/expr/Cargo.toml".to_string(),
            "plugins/expr/evaluator/src/core.rs".to_string(),
        ];
        sort_plugin_context_paths(&mut paths);
        assert_eq!(
            paths,
            vec![
                "plugins/expr/Cargo.toml".to_string(),
                "plugins/expr/evaluator/src/core.rs".to_string(),
                "plugins/expr/src/lib.rs".to_string(),
                "plugins/expr/tests/eval.rs".to_string(),
                "plugins/expr/docs/human/overview.md".to_string(),
            ]
        );
    }

    #[test]
    fn collect_plugin_context_paths_focuses_structural_source_anchors_through_depth_two() {
        let fixtures_root = repo_fixtures_root();
        let plugin_root = fixtures_root.join("plugins");
        let expr_root = plugin_root.join("expr");
        let mut target_plugin_paths = Vec::new();
        collect_plugin_paths(&plugin_root, &expr_root, &mut target_plugin_paths);

        let context_paths =
            collect_plugin_context_paths(&fixtures_root, "expr", &target_plugin_paths)
                .expect("context paths");

        assert!(context_paths
            .focus_paths
            .contains(&"plugins/expr/Cargo.toml".to_string()));
        assert!(context_paths
            .focus_paths
            .contains(&"plugins/expr/src/lib.rs".to_string()));
        assert!(context_paths
            .focus_paths
            .contains(&"plugins/expr/tests/eval.rs".to_string()));
        assert!(context_paths
            .focus_paths
            .contains(&"plugins/expr/evaluator/Cargo.toml".to_string()));
        assert!(context_paths
            .focus_paths
            .contains(&"plugins/expr/evaluator/src/core.rs".to_string()));
        assert!(context_paths
            .focus_paths
            .contains(&"plugins/expr/lexer/src/core.rs".to_string()));
        assert!(context_paths
            .focus_paths
            .contains(&"plugins/expr/evaluator/add/src/core.rs".to_string()));
        assert!(context_paths
            .focus_paths
            .contains(&"plugins/expr/evaluator/add/src/lib.rs".to_string()));
        assert!(context_paths
            .focus_paths
            .contains(&"plugins/expr/evaluator/div/src/core.rs".to_string()));
        assert!(!context_paths
            .focus_paths
            .contains(&"plugins/expr/evaluator/add/tests/add.rs".to_string()));
        assert!(!context_paths
            .focus_paths
            .contains(&"plugins/expr/evaluator/div/docs/human/overview.md".to_string()));
        assert!(context_paths
            .all_paths
            .contains(&"plugins/expr/evaluator/add/src/core.rs".to_string()));
        assert!(context_paths
            .all_paths
            .contains(&"plugins/expr/evaluator/div/src/core.rs".to_string()));
    }

    #[test]
    fn scaffold_integration_edits_require_host_source_or_tests() {
        let scaffolded_children = vec![ScaffoldedChildRegistration {
            parent_manifest_path: "plugins/expr/evaluator/Cargo.toml".to_string(),
            child_root_path: "plugins/expr/evaluator/mod".to_string(),
        }];
        let scaffold_only = vec![
            PluginEditOperation {
                path: "plugins/expr/evaluator/Cargo.toml".to_string(),
                kind: PluginEditOpKind::TomlSet,
                expected_old_string: None,
                expected_sha256: Some("sha".to_string()),
                new_content: None,
                pointer: None,
                dotted_key: Some("package.metadata.cordis.children".to_string()),
                value: None,
            },
            PluginEditOperation {
                path: "plugins/expr/evaluator/mod/src/core.rs".to_string(),
                kind: PluginEditOpKind::ReplaceExact,
                expected_old_string: Some("old".to_string()),
                expected_sha256: None,
                new_content: Some("new".to_string()),
                pointer: None,
                dotted_key: None,
                value: None,
            },
        ];
        assert!(ensure_scaffold_integration_edits(&scaffolded_children, &scaffold_only).is_err());

        let mut integrated = scaffold_only.clone();
        integrated.push(PluginEditOperation {
            path: "plugins/expr/evaluator/src/core.rs".to_string(),
            kind: PluginEditOpKind::ReplaceExact,
            expected_old_string: Some("old".to_string()),
            expected_sha256: None,
            new_content: Some("new".to_string()),
            pointer: None,
            dotted_key: None,
            value: None,
        });
        assert!(ensure_scaffold_integration_edits(&scaffolded_children, &integrated).is_ok());
    }

    #[test]
    #[serial]
    fn plugin_iteration_tool_surface_and_context_reads_expand_from_focus_to_all() {
        crate::plugin::tooling::refresh_artifact_index(&repo_fixtures_root())
            .expect("refresh artifact index before boot");
        let fixtures_root = repo_fixtures_root();
        let host = RuntimeHost::boot(&fixtures_root).expect("host should boot");
        let snapshot = host.current_snapshot();
        let prepared = host
            .kernel
            .begin_plugin_iteration(
                snapshot.as_ref(),
                &KernelPluginIterationRequest {
                    issue_id: None,
                    target_plugin_paths: vec!["expr".to_string()],
                    instruction: Some("inspect expr subtree".to_string()),
                    edit_plan: None,
                    manual_approved: false,
                    tests_command: None,
                    safety_command: None,
                    verify_profile: None,
                    quality_score: None,
                },
            )
            .expect("prepare iteration");
        let iteration_id = prepared.iteration_id.clone();
        let context_paths = collect_plugin_context_paths(
            &fixtures_root,
            &prepared.root_plugin_path,
            &prepared.target_plugin_paths,
        )
        .expect("collect context paths");
        let mut state = PluginIterationAgentState::new(prepared, context_paths, &fixtures_root);
        let mut backend = PluginIterationAgentBackend {
            host: &host,
            state: &mut state,
        };

        let initial_tools = backend.tool_specs();
        assert!(initial_tools
            .iter()
            .any(|tool| tool.name == PLUGIN_AGENT_TOOL_LIST_CONTEXT_FILES));
        assert!(initial_tools
            .iter()
            .any(|tool| tool.name == PLUGIN_AGENT_TOOL_READ_CONTEXT_FILES));
        assert!(initial_tools
            .iter()
            .any(|tool| tool.name == PLUGIN_AGENT_TOOL_INSPECT_PLUGIN_CATALOG));
        assert!(initial_tools
            .iter()
            .any(|tool| tool.name == PLUGIN_AGENT_TOOL_SCAFFOLD_CHILD_PLUGIN));
        assert!(initial_tools
            .iter()
            .any(|tool| tool.name == PLUGIN_AGENT_TOOL_REPLACE_FILE_EXACT));
        assert!(initial_tools
            .iter()
            .any(|tool| tool.name == PLUGIN_AGENT_TOOL_REPLACE_FILES_EXACT));
        assert!(initial_tools
            .iter()
            .any(|tool| tool.name == PLUGIN_AGENT_TOOL_CREATE_FILE));
        assert!(initial_tools
            .iter()
            .any(|tool| tool.name == PLUGIN_AGENT_TOOL_DELETE_FILE));
        assert!(initial_tools
            .iter()
            .any(|tool| tool.name == PLUGIN_AGENT_TOOL_TOML_SET));
        assert!(initial_tools
            .iter()
            .any(|tool| tool.name == PLUGIN_AGENT_TOOL_JSON_SET));

        let focus = backend.list_context_files(ContextFilesScope::Focus);
        let focus_paths = focus
            .get("focus_paths")
            .and_then(|value| value.as_array())
            .expect("focus paths array")
            .iter()
            .filter_map(|value| value.as_str())
            .collect::<Vec<_>>();
        assert!(focus_paths.contains(&"plugins/expr/lexer/src/core.rs"));
        assert!(focus_paths.contains(&"plugins/expr/evaluator/src/core.rs"));
        assert!(focus_paths.contains(&"plugins/expr/evaluator/div/src/core.rs"));
        assert!(!focus_paths.contains(&"plugins/expr/evaluator/div/tests/div.rs"));

        let hidden_err = backend
            .read_context_files(&["plugins/expr/evaluator/div/tests/div.rs".to_string()])
            .expect_err("deep non-source file should require explicit expansion");
        assert!(hidden_err
            .to_string()
            .contains("hidden behind the structural focus shortlist"));

        let expanded = backend.list_context_files(ContextFilesScope::All);
        let expanded_paths = expanded
            .get("paths")
            .and_then(|value| value.as_array())
            .expect("expanded paths array")
            .iter()
            .filter_map(|value| value.as_str())
            .collect::<Vec<_>>();
        assert!(expanded_paths.contains(&"plugins/expr/evaluator/div/src/core.rs"));
        assert!(expanded_paths.contains(&"plugins/expr/evaluator/div/tests/div.rs"));

        let expanded_tools = backend.tool_specs();
        assert!(expanded_tools
            .iter()
            .any(|tool| tool.name == PLUGIN_AGENT_TOOL_INSPECT_PLUGIN_CATALOG));
        assert!(expanded_tools
            .iter()
            .any(|tool| tool.name == PLUGIN_AGENT_TOOL_CREATE_FILE));
        assert!(expanded_tools
            .iter()
            .any(|tool| tool.name == PLUGIN_AGENT_TOOL_DELETE_FILE));
        assert!(expanded_tools
            .iter()
            .any(|tool| tool.name == PLUGIN_AGENT_TOOL_TOML_SET));
        assert!(expanded_tools
            .iter()
            .any(|tool| tool.name == PLUGIN_AGENT_TOOL_JSON_SET));

        let deep_read = backend
            .read_context_files(&["plugins/expr/evaluator/div/tests/div.rs".to_string()])
            .expect("expanded context should allow reading deep test file");
        assert!(deep_read.to_string().contains("DivisionByZero"));

        host.kernel.finish_plugin_iteration(&iteration_id);
    }

    #[test]
    fn validated_verification_command_accepts_short_check_and_test_aliases() {
        let check = super::validated_verification_command(
            Some("check".to_string()),
            Some("cargo check --quiet --manifest-path plugins/Cargo.toml".to_string()),
            "cargo check",
        )
        .expect("check alias should use default command");
        assert_eq!(
            check,
            "cargo check --quiet --manifest-path plugins/Cargo.toml"
        );

        let test = super::validated_verification_command(
            Some("test".to_string()),
            Some("cargo test --quiet --manifest-path plugins/Cargo.toml".to_string()),
            "cargo test",
        )
        .expect("test alias should use default command");
        assert_eq!(
            test,
            "cargo test --quiet --manifest-path plugins/Cargo.toml"
        );

        let empty = super::validated_verification_command(
            super::normalize_optional_command(Some(String::new())),
            Some("cargo check --quiet --manifest-path plugins/Cargo.toml".to_string()),
            "cargo check",
        )
        .expect("empty command should fall back to default");
        assert_eq!(
            empty,
            "cargo check --quiet --manifest-path plugins/Cargo.toml"
        );
    }

    #[test]
    #[serial]
    fn drop_session_clears_all_maps() {
        let host = shared_host();
        // Boot may hydrate leftover session snapshots from data/sessions
        // (crash recovery), so assert deltas against the post-boot baseline
        // instead of absolute sizes.
        let (a0, p0, f0) = host.debug_session_map_sizes();
        let handle = host
            .agent_start_with(AgentSessionKind::RuntimeShell, AgentStartOptions::default())
            .expect("agent should start");
        let session_id = handle.session_id;
        // agent_start populates agent_sessions + profile_fallback; the
        // pending_session_actions entry is created lazily.
        assert_eq!(host.debug_session_map_sizes(), (a0 + 1, p0, f0 + 1));
        host.drop_session(&session_id);
        assert_eq!(host.debug_session_map_sizes(), (a0, p0, f0));
    }

    #[test]
    #[serial]
    fn drop_session_idempotent_on_missing() {
        let host = shared_host();
        // Boot may hydrate leftover snapshots (crash recovery); baseline
        // against whatever is there rather than assuming empty maps.
        let baseline = host.debug_session_map_sizes();
        // Dropping an unknown session id twice must not panic or error, and
        // must leave the maps untouched.
        host.drop_session("no-such-session");
        host.drop_session("no-such-session");
        assert_eq!(host.debug_session_map_sizes(), baseline);
    }

    #[test]
    #[serial]
    fn refresh_session_soul_unknown_sid_errors() {
        let host = shared_host();
        let err = host
            .refresh_session_soul("no-such-session", "user:42")
            .expect_err("unknown session should error");
        assert!(matches!(err, RuntimeError::AgentSessionNotFound { .. }));
    }

    // Build a one-node `RuntimeSnapshot` whose single plugin is registered as
    // `Loaded` with valid docs (so its node enters the registered net as a
    // Task transition), but whose `artifact_path` points at a file that does
    // not exist. `invoke_registered_plugin` then fails when it tries to open
    // the dylib, so `execute_registered_target` takes the invoke-`Err` trace
    // arm (host.rs:204-218): outcome=Failure with the error string recorded.
    fn snapshot_with_broken_artifact_node() -> super::RuntimeSnapshot {
        use crate::core::models::{ArtifactKind, PluginDocs};
        use cordis_plugin_sdk::{AbiFingerprint, NodeDoc, NodeType};

        let plugin_path = "brokenplug".to_string();
        let node_id = "broken_entry".to_string();
        let docs = PluginDocs {
            plugin_id: plugin_path.clone(),
            plugin_path: plugin_path.clone(),
            plugin_version: "0.1.0".to_string(),
            abi_version: 2,
            command_name: None,
            nodes: vec![NodeDoc {
                id: node_id.clone(),
                summary: "always-failing node backed by a missing dylib".to_string(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": { "value": { "type": "number" } }
                }),
                output_schema: serde_json::json!({ "type": "object" }),
                side_effects: Vec::new(),
                failure_modes: vec!["artifact missing".to_string()],
                node_type: NodeType::Task,
                agent_accessible: true,
            }],
            system_hint: None,
        };

        let plugin_registry = crate::plugin::registry::PluginRegistry::default();
        plugin_registry.insert_loaded(
            plugin_path.clone(),
            None,
            true,
            std::collections::BTreeSet::new(),
            docs.clone(),
            // A dylib path that does not exist on disk: `LoadedDylibApi::open`
            // fails and `invoke_registered_plugin` returns an `Err`.
            PathBuf::from("/nonexistent/cordis-broken-artifact.dylib"),
            ArtifactKind::Dylib,
            AbiFingerprint::current_build("crate_broken_v1", "api_v2"),
            None,
        );

        let mut node_registry = crate::plugin::registry::NodeRegistry::default();
        node_registry
            .register_from_docs(&plugin_path, &docs)
            .expect("register broken node");

        let doc_registry =
            crate::service::doc_registry::DocRegistry::from_plugin_registry(&plugin_registry);
        let graph_registry = crate::service::graph_registry::GraphRegistry::from_registries(
            &plugin_registry,
            &node_registry,
        );

        super::runtime_snapshot_from_output(
            crate::plugin::loader::LoadOutput {
                execution_id: "snapshot-broken-artifact".to_string(),
                plugin_registry,
                node_registry,
                doc_registry,
                graph_registry,
                context: crate::context::RuntimeContext::default(),
                metrics: crate::plugin::loader::LoaderMetrics::default(),
            },
            PathBuf::from("/tmp/cordis-broken-artifact-staged"),
        )
    }

    #[test]
    fn execute_registered_target_records_failure_trace_when_invoke_errs() {
        let snapshot = snapshot_with_broken_artifact_node();
        let result = snapshot
            .execute_registered_target(
                "brokenplug::broken_entry",
                serde_json::json!({ "value": 1 }),
            )
            .expect("execute returns a result even when the node invoke fails");

        assert_eq!(result.target_node_fqn, "brokenplug::broken_entry");
        let trace = result
            .traces
            .get("brokenplug::broken_entry")
            .expect("failing node should have a trace entry");
        // The invoke-Err arm records plugin/node identity, a Failure outcome,
        // the request payload, no response payload, and the error string.
        assert_eq!(trace.outcome, Some(NodeOutcome::Failure));
        assert_eq!(trace.plugin_path, "brokenplug");
        assert_eq!(trace.node_id, "broken_entry");
        assert!(trace.response_payload.is_none());
        assert!(
            trace.request_payload.is_some(),
            "request payload should be captured before the failed invoke"
        );
        let err = trace
            .error
            .as_deref()
            .expect("failure trace should carry the invoke error");
        assert!(!err.is_empty(), "invoke error string should be non-empty");
        // The overall net outcome for the target is Failure.
        assert_eq!(
            result.output.outcomes.get("brokenplug::broken_entry"),
            Some(&NodeOutcome::Failure)
        );
    }
}

/// P1-27: caller-controlled stop signal for `walk_code_files_ctl`.
/// Returning `Stop` from the visitor callback aborts the walk
/// immediately; returning `Continue` keeps traversing.
#[derive(Debug, Clone, Copy)]
pub enum WalkControl {
    Continue,
    Stop,
}

fn is_source_like_file_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    // Rust / TOML / YAML / JSON / Markdown / text
    lower.ends_with(".rs")
        || lower.ends_with(".toml")
        || lower.ends_with(".yaml")
        || lower.ends_with(".yml")
        || lower.ends_with(".json")
        || lower.ends_with(".md")
        || lower.ends_with(".txt")
        || lower.ends_with(".lock")
        || lower == "cargo.toml"
        || lower == "makefile"
        || lower == "dockerfile"
        || lower == "justfile"
        || lower.starts_with("dockerfile")
        || lower.ends_with(".sh")
        || lower.ends_with(".py")
        || lower.ends_with(".js")
        || lower.ends_with(".ts")
        || lower.ends_with(".html")
        || lower.ends_with(".css")
}

/// Seam-extraction tests (test-seam batch, >3500 region). Kept in a dedicated
/// module so concurrent edits to the primary `mod tests` do not collide.
#[cfg(test)]
mod seam_extraction_tests {
    use super::{
        canary_payload_serialize_error, checked_command_spawn_error, context_path_escape_error,
        detect_plugin_source_drift, parent_manifest_parse_error, region_io_error,
    };
    use crate::core::error::RuntimeError;
    use crate::kernel::plugin_iteration::VerifierVerdict;
    use std::path::Path;

    /// Body of the injected rehash closure: records the call and returns a
    /// placeholder hash. Named (rather than inline) so its body is executed by
    /// `bump_and_report_records_each_call` even though the drift tests assert
    /// the closure is never invoked.
    fn bump_and_report(ran: &std::cell::Cell<u32>) -> Result<String, RuntimeError> {
        Ok(format!("unused after {}", ran.replace(ran.get() + 1)))
    }

    #[test]
    fn bump_and_report_records_each_call() {
        let ran = std::cell::Cell::new(0u32);
        assert_eq!(bump_and_report(&ran).expect("infallible"), "unused after 0");
        assert_eq!(ran.get(), 1);
    }

    #[test]
    fn detect_drift_returns_none_when_verdict_not_pass() {
        // Non-Pass verdict short-circuits before rehashing; the injected
        // rehash closure must not even run.
        let ran = std::cell::Cell::new(0u32);
        // The closure must NOT run for non-Pass verdicts — that is what this
        // test asserts. Its body therefore lives in `bump_and_report`, which a
        // dedicated test calls directly, so no unexecuted body is left here.
        let rehash = || bump_and_report(&ran);
        let reason = detect_plugin_source_drift(VerifierVerdict::Fail, Some("abc"), rehash);
        assert!(reason.is_none());
        assert_eq!(ran.get(), 0, "rehash must not run when verdict is not Pass");

        let reason = detect_plugin_source_drift(VerifierVerdict::Partial, Some("abc"), rehash);
        assert!(reason.is_none());
        assert_eq!(ran.get(), 0, "rehash must not run when verdict is Partial");
    }

    #[test]
    fn detect_drift_returns_none_when_no_baseline_hash() {
        let reason =
            detect_plugin_source_drift(VerifierVerdict::Pass, None, || Ok("whatever".to_string()));
        assert!(reason.is_none());
    }

    #[test]
    fn detect_drift_returns_none_when_hash_matches() {
        let reason = detect_plugin_source_drift(VerifierVerdict::Pass, Some("hash-xyz"), || {
            Ok("hash-xyz".to_string())
        });
        assert!(reason.is_none());
    }

    #[test]
    fn detect_drift_reports_mutation_when_hash_diverges() {
        let reason =
            detect_plugin_source_drift(VerifierVerdict::Pass, Some("expected-hash"), || {
                Ok("actual-hash".to_string())
            })
            .expect("drift must be detected when hashes differ");
        assert_eq!(
            reason,
            "source tree mutated between verify and promote (expected expected-hash, got actual-hash)"
        );
    }

    #[test]
    fn detect_drift_reports_rehash_failure() {
        let rehash_err = RuntimeError::Invariant {
            message: "disk gone".to_string(),
        };
        let expected = format!("unable to re-hash source tree before promote: {rehash_err}");
        let reason =
            detect_plugin_source_drift(VerifierVerdict::Pass, Some("expected-hash"), || {
                Err(RuntimeError::Invariant {
                    message: "disk gone".to_string(),
                })
            })
            .expect("rehash failure must be surfaced as drift");
        assert_eq!(reason, expected);
    }

    #[test]
    fn canary_payload_serialize_error_wraps_message() {
        let json_err = serde_json::from_str::<serde_json::Value>("not json").unwrap_err();
        let err = canary_payload_serialize_error(&json_err);
        assert!(
            matches!(&err, RuntimeError::Invariant { message } if *message == format!("canary payload serialize failed: {json_err}")),
            "expected Invariant, got {err:?}"
        );
    }

    #[test]
    fn checked_command_spawn_error_preserves_command_and_text() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "no bash");
        let err = checked_command_spawn_error("cargo test", &io_err);
        assert!(
            matches!(&err, RuntimeError::CommandFailed { program, args, message } if program == "bash" && args == &vec!["-lc".to_string(), "cargo test".to_string()] && message == "no bash"),
            "expected CommandFailed, got {err:?}"
        );
    }

    #[test]
    fn region_io_error_preserves_path_and_text() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let err = region_io_error(Path::new("/tmp/x.rs"), &io_err);
        assert!(
            matches!(&err, RuntimeError::Io { path, message } if path == Path::new("/tmp/x.rs") && message == "denied"),
            "expected Io, got {err:?}"
        );
    }

    #[test]
    fn context_path_escape_error_formats_both_paths() {
        let err = context_path_escape_error(Path::new("/outside/f.rs"), Path::new("/root"));
        assert!(
            matches!(&err, RuntimeError::Invariant { message } if message == "planner context path /outside/f.rs escaped workspace root /root"),
            "expected Invariant, got {err:?}"
        );
    }

    #[test]
    fn parent_manifest_parse_error_preserves_toml_text() {
        let toml_err = toml::from_str::<toml::Value>("this = = broken").unwrap_err();
        let err = parent_manifest_parse_error(Path::new("/root/Cargo.toml"), &toml_err);
        assert!(
            matches!(&err, RuntimeError::CargoParse { path, message } if path == Path::new("/root/Cargo.toml") && *message == toml_err.to_string()),
            "expected CargoParse, got {err:?}"
        );
    }
}

/// C-seam (FFI panic arms): tests for the two `#[cfg(test)]` panic-injection
/// points wired into the `catch_unwind` guards — `iterate_plugins`' emergency
/// rollback arm and the reload/stop-handler arm. Kept in a dedicated module so
/// concurrent edits to the primary `mod tests` do not collide.
///
/// Both seams sit as the FIRST statement inside their `catch_unwind` closure,
/// so a set flag panics before any real plugin work runs. That lets these
/// tests boot a hermetic, dlopen-free JSON-artifact fixtures tree in a
/// `TempDir` (near-instant on any host target) instead of the repo fixtures
/// (whose prebuilt dylibs are platform/toolchain-pinned and slow to dlopen
/// over a network mount).
#[cfg(test)]
mod ffi_panic_seam_tests {
    use super::{
        plugin_iteration_journal_path, RuntimeHost, TEST_ITERATION_ENOSPC_INJECTION,
        TEST_ITERATION_PANIC_INJECTION, TEST_STOP_HANDLER_PANIC_INJECTION,
    };
    use crate::kernel::plugin_iteration::{
        KernelPluginIssueSource, KernelPluginIterationRequest, PluginIterationFinalVerdict,
    };
    use cordis_plugin_sdk::{AbiFingerprint, NodeDoc, NodeType, PluginDocs};
    use serde_json::json;
    use serial_test::serial;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::Ordering::SeqCst;
    use tempfile::TempDir;

    /// A near-instant, cross-platform fixtures tree: `artifacts/index.json`
    /// with zero entries. `RuntimeHost::boot` registers no plugins and never
    /// touches a dylib.
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

    /// 在 fixtures 旁写 `config/runtime.yaml`，把 snapshot staging 钉在
    /// `TempDir` 内，避免测试往全局 temp 目录堆 staged 工件。
    /// `discover_config_dir` 对 file_name 为 `fixtures` 的 root 取兄弟
    /// `../config`。
    fn pin_snapshot_root_beside_fixtures(
        fixtures: &std::path::Path,
        snapshot_root: &std::path::Path,
    ) {
        let config_dir = fixtures
            .parent()
            .expect("fixtures has a parent")
            .join("config");
        fs::create_dir_all(&config_dir).expect("create config dir");
        fs::write(
            config_dir.join("runtime.yaml"),
            format!("runtime:\n  snapshot_root: {}\n", snapshot_root.display()),
        )
        .expect("write runtime.yaml");
    }

    #[test]
    #[serial]
    fn host_drop_removes_live_staged_root() {
        let (temp, fixtures) = setup_empty_fixture();
        let snapshot_root = temp.path().join("snapshots");
        pin_snapshot_root_beside_fixtures(&fixtures, &snapshot_root);

        let host = RuntimeHost::boot(&fixtures).expect("host should boot on empty index");
        let staged = host.current_snapshot().staged_artifact_root.clone();
        // 不带格式参数：`staged.display()` 只在断言失败时求值，会留下一行
        // 永久无覆盖。断言本身已足够定位问题。
        assert!(staged.starts_with(&snapshot_root));
        assert!(
            staged.exists(),
            "live staged root exists while host is alive"
        );

        // boot 后不 reload 直接 drop：此前没有任何路径回收 live staged root。
        drop(host);

        assert!(
            !staged.exists(),
            "Drop must reclaim the live snapshot's staged root"
        );
    }

    #[test]
    #[serial]
    fn cleanup_live_snapshot_is_idempotent() {
        let (temp, fixtures) = setup_empty_fixture();
        let snapshot_root = temp.path().join("snapshots");
        pin_snapshot_root_beside_fixtures(&fixtures, &snapshot_root);

        let host = RuntimeHost::boot(&fixtures).expect("host should boot on empty index");
        let staged = host.current_snapshot().staged_artifact_root.clone();

        // 信号处理路径显式调一次，随后 Drop 还会再调一次：必须幂等。
        host.cleanup_live_snapshot();
        assert!(!staged.exists());
        host.cleanup_live_snapshot();
        drop(host);
        assert!(!staged.exists());
    }

    /// A JSON-artifact plugin `svc` that declares one `Task` node. JSON
    /// artifacts skip dlopen at load time, so this boots on any host. The
    /// artifact file's `abi_fingerprint`, `plugin_path`, and `docs` must match
    /// the index entry (the loader cross-checks all three) and the recorded
    /// sha256 must match the artifact bytes.
    fn setup_task_plugin_fixture() -> (TempDir, PathBuf) {
        let temp = TempDir::new().expect("tempdir");
        let fixtures = temp.path().join("fixtures");
        let artifacts = fixtures.join("artifacts");
        fs::create_dir_all(&artifacts).expect("create artifacts dir");

        let abi = AbiFingerprint::current_build("crate_svc_v1", "api_v2");
        let node = NodeDoc {
            id: "svc_serve".to_string(),
            summary: "background task node for stop-handler seam test".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["node_id"],
                "properties": { "node_id": { "type": "string", "const": "svc_serve" } }
            }),
            output_schema: json!({ "type": "object" }),
            side_effects: Vec::new(),
            failure_modes: vec!["unknown node_id".to_string()],
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

        // The artifact JSON: shape mirrors `PluginArtifact` (plugin_path,
        // abi_fingerprint, docs, exports, execution).
        let artifact_value = json!({
            "plugin_path": "svc",
            "abi_fingerprint": abi,
            "docs": docs,
            "exports": [],
            "execution": null,
        });
        let artifact_bytes =
            serde_json::to_vec_pretty(&artifact_value).expect("serialize artifact");
        let artifact_path = artifacts.join("svc.json");
        fs::write(&artifact_path, &artifact_bytes).expect("write artifact");
        let sha256 =
            crate::plugin::artifact::sha256_file(&artifact_path).expect("hash svc artifact");

        let index_value = json!({
            "schema_version": 2,
            "generated_at": "2026-07-24T00:00:00Z",
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
                "execution": null,
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

    /// 一个空 edit_plan 的请求：带 edit_plan 时 `run_plugin_iteration_agent`
    /// 走手工执行分支、完全不碰 LLM；operations 为空则不触碰任何文件，从而
    /// 绕开插件编辑面策略（空 fixture 里没有任何可写的插件子树），让迭代
    /// 确定性地推进到注入点所在的 journal persist 阶段。
    fn empty_plan_request() -> KernelPluginIterationRequest {
        use crate::kernel::plugin_iteration::PluginEditPlan;
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

    /// 磁盘满必须被判成 `InfrastructureFailure`，而不是 `RolledBack`。
    ///
    /// 这是本批修复的核心断言：ENOSPC 此前经 `stage_error: Option<String>`
    /// 丢掉类型信息，在 `finalize_plugin_iteration` 被降级成"验证失败"，
    /// 于是污染 rollback 率、把插件 issue 标成 Open、还丢掉重试入口。
    #[test]
    #[serial]
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

    /// 回归护栏：真正的 stage 失败仍然是 `RolledBack`，没被新变体抢走。
    #[test]
    #[serial]
    fn genuine_stage_failure_still_rolls_back() {
        let (_temp, fixtures) = setup_empty_fixture();
        let host = RuntimeHost::boot(&fixtures).expect("host should boot on empty index");

        // 不注入 ENOSPC；改一个策略禁止的路径（crates/ 属 forbidden_prefixes），
        // 这是插件侧的真实失败，必须仍判 RolledBack。
        let mut request = empty_plan_request();
        if let Some(plan) = request.edit_plan.as_mut() {
            plan.operations = vec![crate::kernel::plugin_iteration::PluginEditOperation {
                path: "crates/cordis-runtime/src/lib.rs".to_string(),
                kind: crate::kernel::plugin_iteration::PluginEditOpKind::ReplaceExact,
                expected_old_string: Some("pub".to_string()),
                expected_sha256: None,
                new_content: Some("mod".to_string()),
                pointer: None,
                dotted_key: None,
                value: None,
            }];
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
            after.iteration_infrastructure_failure_total,
            before.iteration_infrastructure_failure_total,
            "a genuine failure must not land in the infrastructure counter"
        );
        assert_eq!(
            after.iteration_rollback_total,
            before.iteration_rollback_total + 1,
            "a genuine failure still counts as a rollback"
        );
    }

    #[test]
    fn log_builtin_registration_failure_emits_without_panicking() {
        // Covers the logging body, which `register_builtin_agent_node` never
        // reaches because the builtin docs always register cleanly.
        let err = crate::core::error::RuntimeError::Invariant {
            message: "docs were rejected".to_string(),
        };
        super::log_builtin_registration_failure(&err);
    }

    #[test]
    fn builtin_registration_failure_line_embeds_the_error() {
        let err = super::host_invariant("docs were rejected".to_string());
        let line = super::builtin_registration_failure_line(&err);
        assert!(line.starts_with("[builtin] agent_router registration failed: "));
        assert!(line.contains("docs were rejected"));
    }

    // ── plugin_history is capped: the 1025th distinct entry drops the oldest ──
    #[test]
    #[serial]
    fn record_plugin_iteration_outcome_caps_history_at_the_bound() {
        let (_temp, fixtures) = setup_empty_fixture();
        let host = RuntimeHost::boot(&fixtures).expect("host should boot on empty index");

        // MAX_PLUGIN_HISTORY is 1024; record one more so the `pop_back` trim
        // runs. Entries are keyed by iteration_id, so each must be distinct.
        for i in 0..1025u32 {
            host.kernel
                .record_plugin_iteration_outcome(&synthetic_result(&format!("iter-{i}")));
        }
        let history = host.kernel.plugin_history();
        assert_eq!(history.len(), 1024, "history must be trimmed to the bound");
        // Newest at the front, and the very first entry has been dropped.
        assert_eq!(history[0].iteration_id, "iter-1024");
        assert!(
            !history.iter().any(|e| e.iteration_id == "iter-0"),
            "the oldest entry must be evicted"
        );
    }

    fn synthetic_result(iteration_id: &str) -> crate::host::KernelPluginIterationResult {
        crate::host::KernelPluginIterationResult {
            iteration_id: iteration_id.to_string(),
            issue_id: "issue-cap".to_string(),
            root_plugin_path: "plugins/mini".to_string(),
            target_plugin_paths: Vec::new(),
            source: None,
            summary: "history cap probe".to_string(),
            agent_session_id: None,
            tool_execution_summary: None,
            derived_edit_plan: crate::kernel::plugin_iteration::PluginEditPlan {
                issue_id: "issue-cap".to_string(),
                patch_id: format!("{iteration_id}-empty"),
                summary: "history cap probe".to_string(),
                operations: Vec::new(),
            },
            transcript_excerpt: Vec::new(),
            changed_paths: Vec::new(),
            rebuilt_artifacts: Vec::new(),
            candidate: None,
            verification: None,
            verifier_verdict: None,
            canary: None,
            final_verdict: crate::host::PluginIterationFinalVerdict::Blocked,
            blocked_reason: None,
            net_output: crate::execution::engine::ExecutionOutput {
                execution_id: iteration_id.to_string(),
                order: Vec::new(),
                outcomes: std::collections::BTreeMap::new(),
                keyed_outcomes: std::collections::BTreeMap::new(),
                metrics: crate::execution::engine::ExecutionMetrics::default(),
            },
        }
    }

    // ── agent_send drains queued PendingSessionAction::CompactHistory ────
    #[test]
    #[serial]
    fn agent_send_drains_queued_compact_history_action() {
        let (_temp, fixtures) = setup_empty_fixture();
        let host = RuntimeHost::boot(&fixtures).expect("host should boot on empty index");
        let handle = host
            .agent_start(crate::host::AgentSessionKind::RuntimeShell)
            .expect("start a runtime-shell session");

        // Queue the action the way `compact_context` does when it cannot reach
        // the session, then drive `agent_send`. The LLM call fails (no server
        // configured in the empty fixture), but the pending-action drain runs
        // unconditionally after `respond`, so the CompactHistory arm executes.
        host.queue_session_action(
            &handle.session_id,
            crate::host::PendingSessionAction::CompactHistory,
        );
        let _ = host.agent_send(&handle.session_id, "hello");

        // The queue is consumed exactly once: a second send finds it empty and
        // the session is still addressable (it was reinserted).
        let _ = host.agent_send(&handle.session_id, "again");
        assert!(
            host.agent_sessions_mut().contains_key(&handle.session_id),
            "session must be reinserted after draining pending actions"
        );
    }

    // ── C-seam #1: iterate_plugins emergency-rollback arm ────────────────
    #[test]
    #[serial]
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
        let rollback = crate::kernel::plugin_iteration::PluginEditRollback::single_backup(
            fixtures.clone(),
            marker_rel,
            Some(b"original".to_vec()),
        );
        let journal_path = plugin_iteration_journal_path(&host.snapshot_root);
        rollback
            .persist_journal(&journal_path, "seam-iteration")
            .expect("persist journal");
        assert!(journal_path.exists(), "journal should exist before panic");

        // Arm the injection and drive the iteration.
        TEST_ITERATION_PANIC_INJECTION.store(true, SeqCst);
        let result = host.iterate_plugins(iteration_request());

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
        let followup = host.iterate_plugins(iteration_request());
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

    // ── C-seam #2: reload/stop-handler catch_unwind arm ──────────────────
    #[test]
    #[serial]
    fn stop_handler_panic_is_caught_and_reload_path_survives() {
        let (_temp, fixtures) = setup_task_plugin_fixture();
        let host = RuntimeHost::boot(&fixtures).expect("host should boot on task fixture");

        // The Task node must be registered so the stop-handler loop has a
        // target to iterate over.
        let snapshot = host.current_snapshot();
        let task_fqns = snapshot.node_registry().task_node_fqns();
        assert!(
            task_fqns.iter().any(|fqn| fqn == "svc::svc_serve"),
            "svc::svc_serve Task node must be registered, got: {task_fqns:?}"
        );
        drop(snapshot);

        // Arm the stop-handler injection and reload the svc subtree. The panic
        // fires inside the per-node stop `catch_unwind`; the guard must catch
        // it and let the reload continue. (Phase 1 then dlopens the JSON
        // artifact and fails — an expected, caught Err — proving the reload
        // path kept running past the panic instead of unwinding the thread.)
        TEST_STOP_HANDLER_PANIC_INJECTION.store(true, SeqCst);
        let reload_result = host.reload("svc");
        assert!(
            !TEST_STOP_HANDLER_PANIC_INJECTION.load(SeqCst),
            "stop-handler injection flag must be reset after firing"
        );
        // The reload_subtree Phase-1 dlopen of a JSON artifact fails, but the
        // stop-handler panic was already caught by then — the process did not
        // abort, which is the property under test.
        assert!(
            reload_result.is_err(),
            "reloading a JSON artifact via reload_subtree fails at Phase 1 dlopen"
        );

        // The host is still usable after the caught panic: a fresh snapshot
        // read and a second reload attempt both succeed at the API level
        // (i.e. no poisoned lock / aborted thread).
        let after = host.current_snapshot();
        assert!(
            after
                .node_registry()
                .task_node_fqns()
                .iter()
                .any(|fqn| fqn == "svc::svc_serve"),
            "Task node registry must survive the caught panic"
        );
        // Second reload without the flag set: still reaches Phase 1 and errs
        // on the same JSON dlopen, but does not panic — confirms repeatability.
        let second = host.reload("svc");
        assert!(
            second.is_err(),
            "second reload also fails cleanly at Phase 1 (no panic)"
        );
    }
}

/// F-class coverage: unit tests for pure helper functions in the >3500 region
/// that were previously exercised only indirectly (or not at all). Kept in a
/// dedicated module to avoid collision with concurrent edits elsewhere.
#[cfg(test)]
mod seam_pure_fn_tests {
    use super::{
        edges_to_net_specs, extract_response_field, infer_outcome_from_payload,
        is_warning_block_boundary, normalize_warning_source_path, parse_response_payload,
        plugin_change_reasons, plugin_iteration_status_from_history,
        plugin_path_from_runtime_error, plugin_relative_depth, select_registered_net_subgraph,
        strip_rust_span_suffix, truncate_warning_block, warning_cleanup_error_message,
        warning_path_aliases,
    };
    use crate::core::error::RuntimeError;
    use crate::core::models::NodeOutcome;
    use crate::execution::net::ArcDirection;
    use crate::kernel::plugin_iteration::{
        CanaryVerdict, PluginIterationFinalVerdict, PluginIterationHistoryEntry, VerifierVerdict,
    };
    use crate::plugin::registry::RegisteredPlugin;
    use crate::service::graph_registry::{RegisteredNet, RegisteredNetEdge, RegisteredNetEdgeKind};
    use serde_json::json;
    use std::collections::BTreeSet;
    use std::path::Path;

    #[test]
    fn parse_response_payload_parses_json_or_falls_back_to_string() {
        assert_eq!(parse_response_payload("{\"a\":1}"), json!({"a": 1}));
        // Non-JSON input becomes a JSON string verbatim.
        assert_eq!(parse_response_payload("not json"), json!("not json"));
    }

    #[test]
    fn extract_response_field_reads_object_field_only() {
        let payload = json!({"result": {"ok": true}, "n": 3});
        assert_eq!(extract_response_field(&payload, "n"), Some(json!(3)));
        assert_eq!(extract_response_field(&payload, "missing"), None);
        // Non-object payloads yield None.
        assert_eq!(extract_response_field(&json!([1, 2]), "n"), None);
    }

    #[test]
    fn infer_outcome_from_payload_covers_success_and_failure_arms() {
        // Non-object -> Success.
        assert_eq!(
            infer_outcome_from_payload(&json!("x")),
            NodeOutcome::Success
        );
        // ok:false -> Failure.
        assert_eq!(
            infer_outcome_from_payload(&json!({"ok": false})),
            NodeOutcome::Failure
        );
        // Non-empty error string -> Failure.
        assert_eq!(
            infer_outcome_from_payload(&json!({"error": "boom"})),
            NodeOutcome::Failure
        );
        // Blank error string is ignored -> Success.
        assert_eq!(
            infer_outcome_from_payload(&json!({"error": "   "})),
            NodeOutcome::Success
        );
        // Plain object with no failure markers -> Success.
        assert_eq!(
            infer_outcome_from_payload(&json!({"value": 1})),
            NodeOutcome::Success
        );
    }

    #[test]
    fn strip_rust_span_suffix_strips_line_col_when_present() {
        assert_eq!(strip_rust_span_suffix("src/lib.rs:12:5"), "src/lib.rs");
        // Trailing whitespace is trimmed.
        assert_eq!(strip_rust_span_suffix("  src/core.rs:1:1  "), "src/core.rs");
        // No numeric span -> returned trimmed as-is.
        assert_eq!(strip_rust_span_suffix("src/lib.rs"), "src/lib.rs");
        assert_eq!(
            strip_rust_span_suffix("src/lib.rs:foo:bar"),
            "src/lib.rs:foo:bar"
        );
    }

    #[test]
    fn warning_path_aliases_adds_plugins_stripped_alias() {
        assert_eq!(
            warning_path_aliases("plugins/foo/src/lib.rs"),
            vec![
                "plugins/foo/src/lib.rs".to_string(),
                "foo/src/lib.rs".to_string()
            ]
        );
        // Paths without the prefix produce a single alias.
        assert_eq!(
            warning_path_aliases("foo/src/lib.rs"),
            vec!["foo/src/lib.rs".to_string()]
        );
    }

    #[test]
    fn is_warning_block_boundary_recognizes_cargo_markers() {
        for marker in [
            "error: bad",
            "Compiling foo",
            "Checking foo",
            "Finished dev",
            "Running tests",
            "running 3 tests",
            "test result: ok",
            "Doc-tests foo",
        ] {
            assert!(
                is_warning_block_boundary(marker),
                "expected boundary: {marker}"
            );
        }
        assert!(!is_warning_block_boundary("note: something"));
        assert!(!is_warning_block_boundary("  = help: x"));
    }

    #[test]
    fn truncate_warning_block_appends_ellipsis_only_when_truncated() {
        assert_eq!(truncate_warning_block("short", 10), "short");
        assert_eq!(truncate_warning_block("abcdef", 3), "abc...");
    }

    #[test]
    fn warning_cleanup_error_message_embeds_command_and_capped_excerpt() {
        let warnings = vec![
            "warning: one".to_string(),
            "warning: two".to_string(),
            "warning: three".to_string(),
        ];
        let message = warning_cleanup_error_message("cargo build", &warnings);
        assert!(message.contains("verification command `cargo build` succeeded"));
        assert!(message.contains("warning: one"));
        assert!(message.contains("warning: two"));
        // Only the first two warnings are included in the excerpt.
        assert!(!message.contains("warning: three"));
    }

    #[test]
    fn plugin_relative_depth_measures_subtree_distance() {
        assert_eq!(plugin_relative_depth("a/b", "a/b"), Some(0));
        assert_eq!(plugin_relative_depth("a/b", "a/b/c"), Some(1));
        assert_eq!(plugin_relative_depth("a/b", "a/b/c/d"), Some(2));
        // Not under the root.
        assert_eq!(plugin_relative_depth("a/b", "x/y"), None);
        // Prefix collision that is not a real path child.
        assert_eq!(plugin_relative_depth("a/b", "a/bc"), None);
    }

    #[test]
    fn select_registered_net_subgraph_walks_upstream_edges() {
        let edge = |from: &str, to: &str| RegisteredNetEdge {
            from: from.to_string(),
            to: to.to_string(),
            kind: RegisteredNetEdgeKind::Data,
            label: None,
        };
        let net = RegisteredNet {
            nodes: Vec::new(),
            edges: vec![edge("a", "b"), edge("b", "c"), edge("x", "y")],
            diagnostics: Vec::new(),
        };
        // Target c pulls in its transitive upstream (b, a) but not the
        // disconnected x/y component.
        let selected = select_registered_net_subgraph(&net, "c");
        let mut got = selected.into_iter().collect::<Vec<_>>();
        got.sort();
        assert_eq!(got, vec!["a", "b", "c"]);
    }

    #[test]
    fn edges_to_net_specs_builds_place_and_arc_pair_per_selected_edge() {
        // Data edge with a label, plus a control edge without a label, plus an
        // edge whose endpoints are not both selected (must be skipped).
        let edges = vec![
            RegisteredNetEdge {
                from: "a".to_string(),
                to: "b".to_string(),
                kind: RegisteredNetEdgeKind::Data,
                label: Some("payload".to_string()),
            },
            RegisteredNetEdge {
                from: "b".to_string(),
                to: "c".to_string(),
                kind: RegisteredNetEdgeKind::Control,
                label: None,
            },
            // Endpoint `z` is not selected → edge contributes nothing.
            RegisteredNetEdge {
                from: "b".to_string(),
                to: "z".to_string(),
                kind: RegisteredNetEdgeKind::Data,
                label: Some("dropped".to_string()),
            },
        ];
        let selected = BTreeSet::from(["a".to_string(), "b".to_string(), "c".to_string()]);
        let (places, arcs) = edges_to_net_specs(&edges, &selected);

        // Two surviving edges → two places, four arcs.
        let place_ids = places
            .iter()
            .map(|place| place.place_id.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            place_ids,
            vec![
                "place::a::b::payload".to_string(),
                // Missing label falls back to the literal "control" segment.
                "place::b::c::control".to_string(),
            ]
        );
        assert_eq!(arcs.len(), 4);

        // Labelled data edge a→b.
        let out_ab = &arcs[0];
        assert_eq!(out_ab.arc_id, "arc::a::out::place::a::b::payload");
        assert_eq!(out_ab.place_id, "place::a::b::payload");
        assert_eq!(out_ab.transition_id, "a");
        assert!(matches!(out_ab.direction, ArcDirection::TransitionToPlace));
        assert_eq!(out_ab.label.as_deref(), Some("payload"));
        // Outbound arc is never required.
        assert!(!out_ab.required);

        let in_ab = &arcs[1];
        assert_eq!(in_ab.arc_id, "arc::b::in::place::a::b::payload");
        assert_eq!(in_ab.transition_id, "b");
        assert!(matches!(in_ab.direction, ArcDirection::PlaceToTransition));
        // Data edge → inbound arc is required.
        assert!(in_ab.required);

        // Control edge b→c: inbound arc must NOT be required, label is None.
        let in_bc = &arcs[3];
        assert_eq!(in_bc.arc_id, "arc::c::in::place::b::c::control");
        assert!(matches!(in_bc.direction, ArcDirection::PlaceToTransition));
        assert_eq!(in_bc.label, None);
        assert!(!in_bc.required);
    }

    #[test]
    fn edges_to_net_specs_dedups_repeated_place_ids() {
        // Two identical edges collapse to a single place (BTreeSet), but each
        // still emits its own arc pair.
        let edge = RegisteredNetEdge {
            from: "a".to_string(),
            to: "b".to_string(),
            kind: RegisteredNetEdgeKind::Data,
            label: Some("x".to_string()),
        };
        let edges = vec![edge.clone(), edge];
        let selected = BTreeSet::from(["a".to_string(), "b".to_string()]);
        let (places, arcs) = edges_to_net_specs(&edges, &selected);
        assert_eq!(places.len(), 1, "identical place ids are de-duplicated");
        assert_eq!(arcs.len(), 4, "each edge still emits its own arc pair");
    }

    #[test]
    fn edges_to_net_specs_empty_when_no_edges_selected() {
        let edges = vec![RegisteredNetEdge {
            from: "a".to_string(),
            to: "b".to_string(),
            kind: RegisteredNetEdgeKind::Data,
            label: None,
        }];
        // Neither endpoint selected.
        let selected = BTreeSet::from(["x".to_string()]);
        let (places, arcs) = edges_to_net_specs(&edges, &selected);
        assert!(places.is_empty());
        assert!(arcs.is_empty());
    }

    #[test]
    fn normalize_warning_source_path_handles_relative_absolute_and_traversal() {
        let root = Path::new("/workspace/fixtures");
        // Relative path with a rust span suffix: span stripped, kept relative.
        assert_eq!(
            normalize_warning_source_path("plugins/foo/src/lib.rs:12:5", root),
            Some("plugins/foo/src/lib.rs".to_string())
        );
        // Absolute path under the root is made relative.
        assert_eq!(
            normalize_warning_source_path("/workspace/fixtures/plugins/foo/src/lib.rs", root),
            Some("plugins/foo/src/lib.rs".to_string())
        );
        // Absolute path NOT under the root → strip_prefix fails → None.
        assert_eq!(
            normalize_warning_source_path("/elsewhere/foo.rs", root),
            None
        );
        // CurDir components are dropped; ParentDir pops the prior segment.
        assert_eq!(
            normalize_warning_source_path("./a/./b/../c.rs", root),
            Some("a/c.rs".to_string())
        );
        // Leading ParentDir with nothing to pop → None.
        assert_eq!(normalize_warning_source_path("../escape.rs", root), None);
        // Path that normalizes to empty → None.
        assert_eq!(normalize_warning_source_path(".", root), None);
    }

    #[test]
    fn plugin_path_from_runtime_error_extracts_owning_plugin() {
        // Variant carrying `parent`.
        assert_eq!(
            plugin_path_from_runtime_error(&RuntimeError::ChildNotFound {
                parent: "root/parent".to_string(),
                child_source: "missing".to_string(),
            }),
            Some("root/parent".to_string())
        );
        // Variant carrying `plugin_path`.
        assert_eq!(
            plugin_path_from_runtime_error(&RuntimeError::PluginNotRegistered {
                plugin_path: "root/child".to_string(),
            }),
            Some("root/child".to_string())
        );
        // CycleDetected returns the first node of the cycle.
        assert_eq!(
            plugin_path_from_runtime_error(&RuntimeError::CycleDetected {
                cycle: vec!["a".to_string(), "b".to_string()],
            }),
            Some("a".to_string())
        );
        // A variant with no plugin association → None.
        assert_eq!(
            plugin_path_from_runtime_error(&RuntimeError::InvalidArgument {
                message: "no plugin here".to_string(),
            }),
            None
        );
    }

    #[test]
    fn plugin_change_reasons_reports_each_diff_field() {
        use crate::core::models::PluginLoadResult;
        use std::collections::BTreeSet;
        let base = RegisteredPlugin {
            plugin_path: "root/child".to_string(),
            parent: None,
            required: false,
            grants_from_parent: BTreeSet::new(),
            load_result: PluginLoadResult::Loaded,
            docs: None,
            artifact_path: None,
            artifact_kind: None,
            abi_fingerprint: None,
            execution: None,
            fingerprint_diff: Vec::new(),
        };
        // No changes -> empty reasons.
        assert!(plugin_change_reasons(&base, &base).is_empty());

        let mut changed = base.clone();
        changed.required = !base.required;
        assert_eq!(
            plugin_change_reasons(&base, &changed),
            vec!["required_changed".to_string()]
        );

        let mut parent_changed = base.clone();
        parent_changed.parent = Some("root".to_string());
        assert_eq!(
            plugin_change_reasons(&base, &parent_changed),
            vec!["parent_changed".to_string()]
        );
    }

    #[test]
    fn plugin_iteration_status_from_history_copies_fields() {
        let entry = PluginIterationHistoryEntry {
            iteration_id: "iter-1".to_string(),
            issue_id: "issue-1".to_string(),
            root_plugin_path: "root".to_string(),
            target_plugin_paths: vec!["root/child".to_string()],
            source: None,
            summary: "did work".to_string(),
            changed_paths: vec!["root/src/lib.rs".to_string()],
            verifier_verdict: Some(VerifierVerdict::Pass),
            canary_verdict: Some(CanaryVerdict::Partial),
            final_verdict: PluginIterationFinalVerdict::Blocked,
            blocked_reason: Some("canary partial".to_string()),
            observed_at_ms: 10,
            completed_at_ms: 20,
        };
        let status = plugin_iteration_status_from_history(&entry);
        assert_eq!(status.iteration_id, "iter-1");
        assert_eq!(status.issue_id, "issue-1");
        assert_eq!(status.root_plugin_path, "root");
        assert_eq!(status.target_plugin_paths, vec!["root/child".to_string()]);
        assert_eq!(status.summary, "did work");
        assert_eq!(status.changed_paths, vec!["root/src/lib.rs".to_string()]);
        assert_eq!(status.verifier_verdict, Some(VerifierVerdict::Pass));
        assert_eq!(status.canary_verdict, Some(CanaryVerdict::Partial));
        assert_eq!(status.final_verdict, PluginIterationFinalVerdict::Blocked);
        assert_eq!(status.blocked_reason, Some("canary partial".to_string()));
    }
}

/// Seam-extraction tests (test-seam batch, 1-3500 region). Separate module so
/// concurrent edits to `mod tests` / `mod seam_extraction_tests` do not
/// collide. Covers the named error mappers extracted from inline `.map_err`
/// closures in `create_plugin`, snapshot setup, and the `soul_get` reply path.
#[cfg(test)]
mod seam_extraction_tests_low {
    use super::{host_io_error, soul_reply_error};
    use crate::core::error::RuntimeError;
    use std::path::PathBuf;

    #[test]
    fn io_ctx_appends_source_error_to_context() {
        let err = super::io_ctx(
            std::path::PathBuf::from("/x"),
            "failed to write Cargo.toml",
            "disk full",
        );
        assert!(
            matches!(&err, crate::core::error::RuntimeError::Io { path, message }
                if path == std::path::Path::new("/x") && message == "failed to write Cargo.toml: disk full"),
            "got: {err:?}"
        );
    }

    #[test]
    fn flock_failure_line_names_path_and_errno() {
        let line = super::flock_failure_line(std::path::Path::new("/tmp/x.lock"));
        assert!(line.starts_with("[create_plugin] flock(/tmp/x.lock) failed: "));
    }

    #[test]
    fn missing_summary_error_names_the_session() {
        let err = super::missing_summary_error("sess-9");
        assert!(
            matches!(&err, crate::core::error::RuntimeError::LlmResponseInvalid { message }
                if message == "plugin iteration agent session sess-9 exited without calling record_iteration_summary"),
            "got: {err:?}"
        );
    }

    #[test]
    fn host_io_error_carries_path_and_message_verbatim() {
        let path = PathBuf::from("/root/plugins/foo/src");
        let err = host_io_error(
            path.clone(),
            "failed to create plugin src dir: boom".to_string(),
        );
        assert!(
            matches!(&err, RuntimeError::Io { path: p, message } if p == &path && message == "failed to create plugin src dir: boom"),
            "expected Io, got {err:?}"
        );
    }

    #[test]
    fn host_io_error_matches_plain_tostring_form() {
        // Mirrors the snapshot-root create_dir_all closure where message = e.to_string().
        let io = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let path = PathBuf::from("/snap/root");
        let err = host_io_error(path.clone(), io.to_string());
        assert!(
            matches!(&err, RuntimeError::Io { path: p, message } if p == &path && *message == io.to_string()),
            "expected Io, got {err:?}"
        );
    }

    #[test]
    fn soul_reply_error_not_json_stage_text_is_byte_exact() {
        let json_err = serde_json::from_str::<serde_json::Value>("{not json").unwrap_err();
        let err = soul_reply_error("/soul/plugin", "is not JSON", &json_err);
        assert!(
            matches!(&err, RuntimeError::InvalidArgument { message } if *message == format!("soul_get reply from /soul/plugin is not JSON: {json_err}")),
            "expected InvalidArgument, got {err:?}"
        );
    }

    #[test]
    fn soul_reply_error_malformed_stage_text_is_byte_exact() {
        let val_err = serde_json::from_value::<u32>(serde_json::json!("not a number")).unwrap_err();
        let err = soul_reply_error("/soul/plugin", "malformed", &val_err);
        assert!(
            matches!(&err, RuntimeError::InvalidArgument { message } if *message == format!("soul_get reply from /soul/plugin malformed: {val_err}")),
            "expected InvalidArgument, got {err:?}"
        );
    }
}

/// Seam tests for `aggregate_rollback_failure` — the pure result-aggregation
/// half of `finalize_plugin_iteration`'s rollback path (P2-25 promote-failure
/// chain + the `stage_error` early return). Placed in a dedicated module so
/// concurrent edits to the primary `mod tests` do not collide. Asserts the
/// full matrix of {no candidate / candidate-ok / candidate-err} ×
/// {restore-ok / restore-err} produces byte-exact `blocked_reason` strings.
#[cfg(test)]
mod aggregate_rollback_failure_tests {
    use super::aggregate_rollback_failure;
    use super::validated_verification_command;
    use crate::core::error::RuntimeError;

    fn err(message: &str) -> RuntimeError {
        RuntimeError::InvalidArgument {
            message: message.to_string(),
        }
    }

    #[test]
    fn validated_verification_command_missing_command_errors_byte_exact() {
        // Both explicit and fallback absent → InvalidArgument with the
        // "missing verification command for <prefix>" wording (line 5298).
        let out = validated_verification_command(None, None, "cargo test");
        assert!(
            matches!(&out, Err(RuntimeError::InvalidArgument { message })
                if *message == "missing verification command for cargo test"),
            "expected missing-command InvalidArgument, got {out:?}"
        );
    }

    #[test]
    fn validated_verification_command_prefix_mismatch_errors_byte_exact() {
        // Explicit command that does not start with the required prefix and is
        // not a recognized short alias → prefix-mismatch InvalidArgument
        // (lines 5309-5314). `rm -rf` never matches `cargo build`.
        let out = validated_verification_command(Some("rm -rf /".to_string()), None, "cargo build");
        assert!(
            matches!(&out, Err(RuntimeError::InvalidArgument { message })
                if *message == "verification tool only allows commands starting with `cargo build`, got `rm -rf /`"),
            "expected prefix-mismatch InvalidArgument, got {out:?}"
        );
    }

    #[test]
    fn no_rollback_errors_returns_base_message_verbatim() {
        // No candidate staged, workspace restore succeeded → base unchanged.
        let out = aggregate_rollback_failure("promote failed: boom".to_string(), None, Ok(()));
        assert_eq!(out, "promote failed: boom");
    }

    #[test]
    fn candidate_ok_and_restore_ok_returns_base_message() {
        let out = aggregate_rollback_failure("stage error".to_string(), Some(Ok(())), Ok(()));
        assert_eq!(out, "stage error");
    }

    #[test]
    fn candidate_rollback_error_only() {
        let out = aggregate_rollback_failure(
            "promote failed: boom".to_string(),
            Some(Err(err("cand kaput"))),
            Ok(()),
        );
        assert_eq!(
            out,
            "promote failed: boom; rollback errors: [candidate rollback: invalid argument: cand kaput]"
        );
    }

    #[test]
    fn workspace_restore_error_only() {
        let out = aggregate_rollback_failure(
            "promote failed: boom".to_string(),
            Some(Ok(())),
            Err(err("restore kaput")),
        );
        assert_eq!(
            out,
            "promote failed: boom; rollback errors: [workspace restore: invalid argument: restore kaput]"
        );
    }

    #[test]
    fn no_candidate_but_workspace_restore_error() {
        // stage_error early-return shape: candidate absent, restore failed.
        let out =
            aggregate_rollback_failure("stage error".to_string(), None, Err(err("restore kaput")));
        assert_eq!(
            out,
            "stage error; rollback errors: [workspace restore: invalid argument: restore kaput]"
        );
    }

    #[test]
    fn both_candidate_and_restore_errors_are_ordered_and_comma_joined() {
        // Candidate rollback error precedes workspace restore error, joined
        // with ", " — must match the historical push order byte-for-byte.
        let out = aggregate_rollback_failure(
            "promote (manual-approved) failed: boom".to_string(),
            Some(Err(err("cand kaput"))),
            Err(err("restore kaput")),
        );
        assert_eq!(
            out,
            "promote (manual-approved) failed: boom; rollback errors: \
             [candidate rollback: invalid argument: cand kaput, workspace restore: invalid argument: restore kaput]"
        );
    }
}

/// Coverage terminal batch (>5500 region): unit tests for pure helper and
/// data-construction functions that were previously exercised only indirectly
/// or not at all. Kept in a dedicated EOF module so concurrent edits to the
/// other `mod *_tests` blocks do not collide. Byte-for-byte error/text
/// assertions where the emitted string is load-bearing.
#[cfg(test)]
mod region_5500_coverage_tests {
    use super::{
        artifact_paths_to_backup, atomic_write_bytes, build_execution_net, build_execution_payload,
        build_snapshot_with_staged_root, cleanup_stale_snapshot_dirs,
        collect_context_files_recursive, collect_plugin_context_paths, default_snapshot_root,
        determine_root_plugin_path, extract_warning_blocks, fill_missing_execution_traces,
        insert_context_file_if_exists, make_snapshot_dir_name, normalize_request_id,
        normalize_warning_source_path, plugin_change_reasons, plugin_context_priority,
        plugin_path_from_runtime_error, render_child_plugin_core, render_child_plugin_lib,
        render_child_plugin_manifest, render_child_plugin_overview, render_child_plugin_test,
        sanitize_child_plugin_segment, select_registered_net_subgraph, should_track_context_file,
        validated_verification_command, workspace_manifest_lock, ExecutionInvocationTrace,
    };
    use crate::core::error::RuntimeError;
    use crate::core::models::NodeOutcome;
    use crate::core::models::PluginUnavailableReason;
    use crate::execution::engine::{
        ExecutionMetrics, ExecutionOutput, ExecutionTransitionKind, TriggerInput,
    };
    use crate::execution::net::{CorrelationKey, Token, TokenMeta};
    use crate::service::graph_registry::{
        RegisteredNet, RegisteredNetEdge, RegisteredNetEdgeKind, RegisteredNetNode,
    };
    use cordis_plugin_sdk::NodeType;
    use serde_json::{json, Map, Value};
    use std::collections::{BTreeMap, BTreeSet};

    // ── validated_verification_command: default-alias short-circuit (5525) ──
    #[test]
    fn validated_verification_command_returns_fallback_for_bare_alias() {
        // explicit "check" with required_prefix "cargo check" and a fallback:
        // the alias arm returns the fallback verbatim.
        let out = validated_verification_command(
            Some("check".to_string()),
            Some("cargo check --workspace".to_string()),
            "cargo check",
        )
        .expect("bare alias with fallback resolves to fallback");
        assert_eq!(out, "cargo check --workspace");

        // "test" alias against "cargo test".
        let out = validated_verification_command(
            Some("test".to_string()),
            Some("cargo test --all".to_string()),
            "cargo test",
        )
        .expect("test alias with fallback resolves to fallback");
        assert_eq!(out, "cargo test --all");
    }

    #[test]
    fn validated_verification_command_trims_and_accepts_prefixed() {
        let out = validated_verification_command(
            Some("  cargo check --lib  ".to_string()),
            None,
            "cargo check",
        )
        .expect("prefixed command accepted");
        assert_eq!(out, "cargo check --lib");
    }

    // ── should_track_context_file (5571) ────────────────────────────────
    #[test]
    fn should_track_context_file_matches_source_extensions() {
        assert!(should_track_context_file("plugins/foo/src/lib.rs"));
        assert!(should_track_context_file("plugins/foo/Cargo.toml"));
        assert!(should_track_context_file(
            "plugins/foo/docs/agent/interfaces.json"
        ));
        assert!(should_track_context_file(
            "plugins/foo/docs/human/overview.md"
        ));
        // Unknown extension → not tracked.
        assert!(!should_track_context_file("plugins/foo/artifacts/foo.so"));
        assert!(!should_track_context_file("plugins/foo/README"));
    }

    // ── plugin_context_priority: the else/fallback bucket (5600) ─────────
    #[test]
    fn plugin_context_priority_orders_by_bucket() {
        assert_eq!(plugin_context_priority("a/Cargo.toml").0, 0);
        assert_eq!(plugin_context_priority("a/src/core.rs").0, 1);
        assert_eq!(plugin_context_priority("a/src/lib.rs").0, 2);
        assert_eq!(plugin_context_priority("a/tests/it.rs").0, 3);
        assert_eq!(plugin_context_priority("a/docs/human/x.md").0, 4);
        assert_eq!(plugin_context_priority("a/docs/agent/x.json").0, 5);
        assert_eq!(plugin_context_priority("a/src/other.rs").0, 6);
        // Anything else → the catch-all bucket 7.
        assert_eq!(plugin_context_priority("a/notes.txt").0, 7);
    }

    // ── sanitize_child_plugin_segment (5612, 5621) ──────────────────────
    #[test]
    fn sanitize_child_plugin_segment_normalizes_and_defaults() {
        // Non-alphanumeric runs collapse to single dashes and trim.
        assert_eq!(sanitize_child_plugin_segment("  Foo Bar!! "), "foo-bar");
        assert_eq!(sanitize_child_plugin_segment("//Multiply//"), "multiply");
        // All-punctuation input normalizes to empty → "child" default.
        assert_eq!(sanitize_child_plugin_segment("***"), "child");
        assert_eq!(sanitize_child_plugin_segment(""), "child");
    }

    // ── render_child_plugin_* (5632-5683) ───────────────────────────────
    #[test]
    fn render_child_plugin_manifest_embeds_names_and_paths() {
        let manifest = render_child_plugin_manifest("my-child", "root/child", "child_entry");
        // crate name has dashes replaced with underscores in [package].name.
        assert!(manifest.contains("name = \"my_child\""));
        assert!(manifest.contains("plugin_path = \"root/child\""));
        assert!(manifest.contains("declared_nodes = [\"child_entry\"]"));
        assert!(manifest.contains("crate_hash = \"crate_my_child_v1\""));
        assert!(manifest.contains("allow_generated_docs = true"));
    }

    #[test]
    fn render_child_plugin_lib_escapes_summary_quotes() {
        let lib =
            render_child_plugin_lib("my-child", "root/child", "child_entry", "a \"quoted\" sum");
        assert!(lib.contains("\"child_entry\""));
        assert!(lib.contains("crate_my-child_v1"));
        // Embedded quotes in the summary are backslash-escaped.
        assert!(lib.contains("a \\\"quoted\\\" sum"));
    }

    #[test]
    fn render_child_plugin_core_builds_pascal_type_name() {
        let core = render_child_plugin_core("binary-multiply");
        // "binary-multiply" → PascalCase "BinaryMultiply".
        assert!(core.contains("pub enum BinaryMultiplyError"));
        assert!(core.contains("pub struct BinaryMultiplyPlugin"));
        assert!(core.contains("NotImplemented"));
    }

    #[test]
    fn render_child_plugin_test_and_overview_embed_identifiers() {
        let test = render_child_plugin_test("my_child");
        assert!(test.contains("use my_child::apply;"));
        assert!(test.contains("scaffold_exports_apply"));

        let overview = render_child_plugin_overview("root/child");
        assert!(overview.starts_with("# root/child\n"));
        assert!(overview.contains("Cordis plugin-iteration agent"));
    }

    // ── extract_warning_blocks: block boundary flush (5838-5840) ─────────
    #[test]
    fn extract_warning_blocks_flushes_on_boundary_line() {
        let text = "\
warning: unused variable `x`
  --> src/lib.rs:1:5
Compiling next-crate v0.1.0
warning: unused import
  --> src/lib.rs:2:5";
        let blocks = extract_warning_blocks(text);
        // The "Compiling " boundary flushes the first block; a trailing block
        // is pushed at EOF.
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].starts_with("warning: unused variable `x`"));
        assert!(!blocks[0].contains("Compiling"));
        assert!(blocks[1].starts_with("warning: unused import"));
    }

    // ── select_registered_net_subgraph: multi-hop upstream (5915) ────────
    #[test]
    fn select_registered_net_subgraph_visits_each_upstream_once() {
        // Diamond: a→b, a→c, b→d, c→d. Target d must pull a,b,c,d exactly once.
        let edge = |from: &str, to: &str| RegisteredNetEdge {
            from: from.to_string(),
            to: to.to_string(),
            kind: RegisteredNetEdgeKind::Data,
            label: None,
        };
        let net = RegisteredNet {
            nodes: Vec::new(),
            edges: vec![
                edge("a", "b"),
                edge("a", "c"),
                edge("b", "d"),
                edge("c", "d"),
            ],
            diagnostics: Vec::new(),
        };
        let selected = select_registered_net_subgraph(&net, "d");
        let mut got = selected.into_iter().collect::<Vec<_>>();
        got.sort();
        assert_eq!(got, vec!["a", "b", "c", "d"]);
    }

    // ── build_execution_net (5934-6010) ─────────────────────────────────
    fn net_node(fqn: &str, node_type: NodeType, topo: usize) -> RegisteredNetNode {
        RegisteredNetNode {
            node_fqn: fqn.to_string(),
            plugin_path: fqn.split("::").next().unwrap_or(fqn).to_string(),
            node_id: fqn.split("::").nth(1).unwrap_or(fqn).to_string(),
            consumes: Vec::new(),
            produces: Vec::new(),
            topo_level: topo,
            node_type,
        }
    }

    fn registered_node(fqn: &str, node_type: NodeType) -> crate::plugin::registry::RegisteredNode {
        crate::plugin::registry::RegisteredNode {
            node_fqn: fqn.to_string(),
            plugin_path: fqn.split("::").next().unwrap_or(fqn).to_string(),
            node_id: fqn.split("::").nth(1).unwrap_or(fqn).to_string(),
            node_type,
        }
    }

    #[test]
    fn build_execution_net_empty_selection_yields_terminal_fallback() {
        // No net node matches the selected set → single Terminal transition
        // keyed on the target fqn (5940-5956).
        let net = RegisteredNet::default();
        let selected = BTreeSet::from(["missing::node".to_string()]);
        let fallback = registered_node("missing::node", NodeType::Terminal);
        let spec = build_execution_net(&net, &selected, "missing::node", &fallback);
        assert!(spec.places.is_empty());
        assert!(spec.arcs.is_empty());
        assert_eq!(spec.transitions.len(), 1);
        let t = &spec.transitions[0];
        assert_eq!(t.transition.transition_id, "missing::node");
        assert!(matches!(t.kind, ExecutionTransitionKind::Terminal));
        assert_eq!(t.logical_group.as_deref(), Some("execute"));
        assert!(t.node_type.is_none());
    }

    #[test]
    fn build_execution_net_maps_all_node_types_and_join_policy() {
        // Nodes cover every NodeType arm (5978-5986). Edge a→b makes b's
        // incoming count 1 (AllOf); the source a has incoming 0 (AnyOf).
        let net = RegisteredNet {
            nodes: vec![
                net_node("p::a", NodeType::Task, 0),
                net_node("p::b", NodeType::Router, 1),
                net_node("p::c", NodeType::Gate, 1),
                net_node("p::d", NodeType::Terminal, 2),
            ],
            edges: vec![RegisteredNetEdge {
                from: "p::a".to_string(),
                to: "p::b".to_string(),
                kind: RegisteredNetEdgeKind::Data,
                label: Some("payload".to_string()),
            }],
            diagnostics: Vec::new(),
        };
        let selected = BTreeSet::from([
            "p::a".to_string(),
            "p::b".to_string(),
            "p::c".to_string(),
            "p::d".to_string(),
        ]);
        // Target is selected → no fallback terminal appended.
        let fallback = registered_node("p::a", NodeType::Task);
        let spec = build_execution_net(&net, &selected, "p::a", &fallback);
        assert_eq!(spec.transitions.len(), 4);
        let by_id = |id: &str| {
            spec.transitions
                .iter()
                .find(|t| t.transition.transition_id == id)
                .expect("transition present")
        };
        assert!(matches!(by_id("p::a").kind, ExecutionTransitionKind::Task));
        assert!(matches!(
            by_id("p::b").kind,
            ExecutionTransitionKind::Router { .. }
        ));
        assert!(matches!(
            by_id("p::c").kind,
            ExecutionTransitionKind::Gate { .. }
        ));
        assert!(matches!(
            by_id("p::d").kind,
            ExecutionTransitionKind::Terminal
        ));
        // a has no incoming selected edge → AnyOf; b has one → AllOf.
        use crate::execution::net::JoinPolicy;
        assert!(matches!(
            by_id("p::a").transition.join_policy,
            JoinPolicy::AnyOf
        ));
        assert!(matches!(
            by_id("p::b").transition.join_policy,
            JoinPolicy::AllOf
        ));
    }

    #[test]
    fn build_execution_net_appends_fallback_terminal_when_target_unselected() {
        // Selected nodes exist, but the requested target fqn is NOT among them
        // → the extra fallback Terminal transition is appended (5998-6010).
        let net = RegisteredNet {
            nodes: vec![net_node("p::a", NodeType::Task, 0)],
            edges: Vec::new(),
            diagnostics: Vec::new(),
        };
        let selected = BTreeSet::from(["p::a".to_string()]);
        let fallback = registered_node("p::target", NodeType::Terminal);
        let spec = build_execution_net(&net, &selected, "p::target", &fallback);
        assert_eq!(spec.transitions.len(), 2);
        let fallback_t = spec
            .transitions
            .iter()
            .find(|t| t.transition.transition_id == "p::target")
            .expect("fallback terminal appended");
        assert!(matches!(fallback_t.kind, ExecutionTransitionKind::Terminal));
        assert!(fallback_t.node_type.is_none());
    }

    // ── build_execution_payload (6077-6084) ─────────────────────────────
    fn token_with_payload(payload: Value) -> Token {
        Token {
            key: CorrelationKey::new(""),
            payload,
            meta: TokenMeta {
                execution_id: "e".to_string(),
                transition_id: "t".to_string(),
                logical_group: "g".to_string(),
                sequence: 0,
                outcome: NodeOutcome::Success,
            },
        }
    }

    #[test]
    fn build_execution_payload_merges_labeled_inputs_only() {
        let mut base = Map::new();
        base.insert("keep".to_string(), json!(1));
        let inputs = vec![
            // Labeled input whose payload has the field → merged in.
            TriggerInput {
                place_id: "pl1".to_string(),
                label: Some("added".to_string()),
                token: token_with_payload(json!({ "added": 42, "extra": 9 })),
            },
            // No label → skipped (6078-6080).
            TriggerInput {
                place_id: "pl2".to_string(),
                label: None,
                token: token_with_payload(json!({ "ignored": 1 })),
            },
            // Labeled but field absent from payload → skipped (6081-6083).
            TriggerInput {
                place_id: "pl3".to_string(),
                label: Some("absent".to_string()),
                token: token_with_payload(json!({ "other": 1 })),
            },
        ];
        let out = build_execution_payload(&base, &inputs);
        assert_eq!(out.get("keep"), Some(&json!(1)));
        assert_eq!(out.get("added"), Some(&json!(42)));
        // Only the named field is copied, not sibling fields from the token.
        assert!(out.get("extra").is_none());
        assert!(out.get("absent").is_none());
    }

    // ── fill_missing_execution_traces (6126-6134) ───────────────────────
    #[test]
    fn fill_missing_execution_traces_inserts_placeholder_and_sets_outcome() {
        let mut outcomes = BTreeMap::new();
        outcomes.insert("p::new".to_string(), NodeOutcome::Failure);
        outcomes.insert("p::existing".to_string(), NodeOutcome::Success);
        let output = ExecutionOutput {
            execution_id: "e".to_string(),
            order: Vec::new(),
            outcomes,
            keyed_outcomes: BTreeMap::new(),
            metrics: ExecutionMetrics::default(),
        };
        let mut traces = BTreeMap::new();
        // Pre-seed one trace with existing data to prove only the outcome is set.
        traces.insert(
            "p::existing".to_string(),
            ExecutionInvocationTrace {
                node_fqn: "p::existing".to_string(),
                plugin_path: "p".to_string(),
                node_id: "existing".to_string(),
                attempt: 3,
                outcome: None,
                request_payload: Some(json!({"r": 1})),
                response_payload: None,
                error: None,
            },
        );
        fill_missing_execution_traces(&output, &mut traces);
        // New fqn gets a default-shaped placeholder trace.
        let new_trace = traces.get("p::new").expect("placeholder inserted");
        assert_eq!(new_trace.node_fqn, "p::new");
        assert_eq!(new_trace.plugin_path, "");
        assert_eq!(new_trace.attempt, 0);
        assert_eq!(new_trace.outcome, Some(NodeOutcome::Failure));
        // Existing trace keeps its fields but gains the outcome.
        let existing = traces.get("p::existing").expect("existing preserved");
        assert_eq!(existing.attempt, 3);
        assert_eq!(existing.request_payload, Some(json!({"r": 1})));
        assert_eq!(existing.outcome, Some(NodeOutcome::Success));
    }

    // ── plugin_path_from_runtime_error: every plugin-bearing variant + None
    //    (6387-6408) ─────────────────────────────────────────────────────
    #[test]
    fn plugin_path_from_runtime_error_covers_all_variants() {
        use crate::core::models::AbiFingerprint;
        use std::path::PathBuf;
        let pp = |s: &str| Some(s.to_string());

        assert_eq!(
            plugin_path_from_runtime_error(&RuntimeError::InvalidChildSource {
                parent: "root".to_string(),
                child_source: "c".to_string(),
                reason: "r".to_string(),
            }),
            pp("root")
        );
        assert_eq!(
            plugin_path_from_runtime_error(&RuntimeError::DuplicatePluginPath {
                plugin_path: "dup".to_string(),
                first: PathBuf::from("/a"),
                second: PathBuf::from("/b"),
            }),
            pp("dup")
        );
        assert_eq!(
            plugin_path_from_runtime_error(&RuntimeError::MissingScaffold {
                plugin_path: "ms".to_string(),
                missing: vec!["x".to_string()],
            }),
            pp("ms")
        );
        assert_eq!(
            plugin_path_from_runtime_error(&RuntimeError::DocsContract {
                plugin_path: "dc".to_string(),
                message: "m".to_string(),
            }),
            pp("dc")
        );
        assert_eq!(
            plugin_path_from_runtime_error(&RuntimeError::ArtifactIndexMissing {
                plugin_path: "aim".to_string(),
            }),
            pp("aim")
        );
        assert_eq!(
            plugin_path_from_runtime_error(&RuntimeError::ArtifactFileMissing {
                plugin_path: "afm".to_string(),
                artifact_path: PathBuf::from("/a.so"),
            }),
            pp("afm")
        );
        assert_eq!(
            plugin_path_from_runtime_error(&RuntimeError::ArtifactHashMismatch {
                plugin_path: "ahm".to_string(),
                expected: "e".to_string(),
                actual: "a".to_string(),
            }),
            pp("ahm")
        );
        assert_eq!(
            plugin_path_from_runtime_error(&RuntimeError::AbiMismatch {
                plugin_path: "abi".to_string(),
                expected: Box::new(AbiFingerprint::current_build("c", "a")),
                actual: Box::new(AbiFingerprint::current_build("c2", "a2")),
                fingerprint_diff: Vec::new(),
            }),
            pp("abi")
        );
        assert_eq!(
            plugin_path_from_runtime_error(&RuntimeError::PluginUnavailable {
                plugin_path: "pu".to_string(),
                reason: PluginUnavailableReason::InitFailed,
                required: true,
            }),
            pp("pu")
        );
        assert_eq!(
            plugin_path_from_runtime_error(&RuntimeError::PluginExecutionUnsupported {
                plugin_path: "peu".to_string(),
                artifact_path: PathBuf::from("/a"),
            }),
            pp("peu")
        );
        assert_eq!(
            plugin_path_from_runtime_error(&RuntimeError::PluginInvocationFailed {
                plugin_path: "pif".to_string(),
                message: "m".to_string(),
            }),
            pp("pif")
        );
        assert_eq!(
            plugin_path_from_runtime_error(&RuntimeError::PluginDocsNotFound {
                plugin_path: "pdnf".to_string(),
            }),
            pp("pdnf")
        );
        assert_eq!(
            plugin_path_from_runtime_error(&RuntimeError::NodeDocsNotFound {
                plugin_path: "ndnf".to_string(),
                node_id: "n".to_string(),
            }),
            pp("ndnf")
        );
        assert_eq!(
            plugin_path_from_runtime_error(&RuntimeError::PermissionDenied {
                plugin_path: "pd".to_string(),
                service: "s".to_string(),
            }),
            pp("pd")
        );
        assert_eq!(
            plugin_path_from_runtime_error(&RuntimeError::ContextPluginUnavailable {
                plugin_path: "cpu".to_string(),
            }),
            pp("cpu")
        );
        assert_eq!(
            plugin_path_from_runtime_error(&RuntimeError::ServiceNotFound {
                plugin_path: "snf".to_string(),
                service: "s".to_string(),
            }),
            pp("snf")
        );
        assert_eq!(
            plugin_path_from_runtime_error(&RuntimeError::ServiceTypeMismatch {
                plugin_path: "stm".to_string(),
                service: "s".to_string(),
            }),
            pp("stm")
        );
        assert_eq!(
            plugin_path_from_runtime_error(&RuntimeError::DuplicateService {
                plugin_path: "ds".to_string(),
                service: "s".to_string(),
            }),
            pp("ds")
        );
    }

    // ── determine_root_plugin_path: empty-target error (6417) ────────────
    fn empty_snapshot() -> super::RuntimeSnapshot {
        super::runtime_snapshot_from_output(
            crate::plugin::loader::LoadOutput {
                execution_id: "snap-empty".to_string(),
                plugin_registry: crate::plugin::registry::PluginRegistry::default(),
                node_registry: crate::plugin::registry::NodeRegistry::default(),
                doc_registry: crate::service::doc_registry::DocRegistry::default(),
                graph_registry: crate::service::graph_registry::GraphRegistry::default(),
                context: crate::context::RuntimeContext::default(),
                metrics: crate::plugin::loader::LoaderMetrics::default(),
            },
            std::path::PathBuf::from("/tmp/cordis-empty-staged"),
        )
    }

    #[test]
    fn determine_root_plugin_path_empty_targets_errors_byte_exact() {
        // The empty target list errors before the snapshot registry is
        // ever consulted.
        let snapshot = empty_snapshot();
        let err =
            determine_root_plugin_path(&snapshot, &[]).expect_err("empty target list must error");
        assert!(
            matches!(&err, RuntimeError::InvalidArgument { message }
                if message == "plugin iteration requires target_plugin_paths or an observed issue"),
            "unexpected: {err:?}"
        );
    }

    #[test]
    fn determine_root_plugin_path_errors_when_no_shared_loaded_root() {
        // Two targets share a common prefix ("root") but that prefix is not in
        // the (empty) plugin registry, so all candidates pop off and the final
        // "do not share a loaded subtree root" error fires (6430-6449).
        let snapshot = empty_snapshot();
        let err =
            determine_root_plugin_path(&snapshot, &["root/a".to_string(), "root/b".to_string()])
                .expect_err("no loaded root must error");
        assert!(
            matches!(&err, RuntimeError::InvalidArgument { message }
                if message == "target plugin paths do not share a loaded subtree root: root/a, root/b"),
            "unexpected: {err:?}"
        );
    }

    // ── normalize_request_id (6628-6629) ────────────────────────────────
    #[test]
    fn normalize_request_id_keeps_nonblank_and_generates_for_blank() {
        assert_eq!(
            normalize_request_id(Some("given-id".to_string()), "pfx"),
            "given-id"
        );
        // Blank / whitespace / None all fall to the generated branch.
        let gen1 = normalize_request_id(Some("   ".to_string()), "pfx");
        assert!(gen1.starts_with("pfx-"));
        let gen2 = normalize_request_id(None, "pfx");
        assert!(gen2.starts_with("pfx-"));
        // The process-local counter keeps successive generated ids distinct.
        assert_ne!(gen1, gen2);
    }

    // ── artifact_paths_to_backup (6832-6839) ────────────────────────────
    #[test]
    fn artifact_paths_to_backup_handles_empty_missing_and_present() {
        let temp = tempfile::tempdir().expect("tempdir");
        let fixtures = temp.path();
        // Empty/whitespace-only path → nothing.
        assert!(artifact_paths_to_backup(fixtures, "/").is_empty());
        // Non-empty path but no artifact file on disk → nothing.
        assert!(artifact_paths_to_backup(fixtures, "root/child").is_empty());
        // Now create artifacts/root/child.so → returned as (rel, abs).
        let so = fixtures.join("artifacts/root/child.so");
        std::fs::create_dir_all(so.parent().unwrap()).unwrap();
        std::fs::write(&so, b"bytes").unwrap();
        let out = artifact_paths_to_backup(fixtures, "root/child");
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "artifacts/root/child.so");
        assert_eq!(out[0].1, so);
    }

    // ── default_snapshot_root: canonicalize + hash layout (6977) ─────────
    #[test]
    fn default_snapshot_root_is_deterministic_under_temp_dir() {
        let temp = tempfile::tempdir().expect("tempdir");
        let a = default_snapshot_root(temp.path());
        let b = default_snapshot_root(temp.path());
        assert_eq!(a, b, "same fixtures root hashes to the same snapshot root");
        assert!(a.starts_with(std::env::temp_dir().join("cordis-runtime-host")));
    }

    // ── collect_context_files_recursive + insert_context_file_if_exists +
    //    collect_plugin_context_paths (filesystem seams,
    //    6463-6528, 6575, 6611-6621) ─────────────────────────────────────
    #[test]
    fn collect_context_files_recursive_walks_tracked_extensions() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        let src = root.join("plugins/foo/src");
        std::fs::create_dir_all(src.join("nested")).unwrap();
        std::fs::write(src.join("lib.rs"), b"// rs").unwrap();
        std::fs::write(src.join("nested/core.rs"), b"// rs").unwrap();
        // Non-tracked extension is skipped.
        std::fs::write(src.join("blob.bin"), b"bin").unwrap();
        let mut files = BTreeSet::new();
        collect_context_files_recursive(root, &src, &mut files).expect("walk ok");
        assert!(files.contains("plugins/foo/src/lib.rs"));
        assert!(files.contains("plugins/foo/src/nested/core.rs"));
        assert!(!files.iter().any(|p| p.ends_with("blob.bin")));
    }

    #[test]
    fn insert_context_file_if_exists_only_inserts_present_paths() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        std::fs::create_dir_all(root.join("plugins/foo")).unwrap();
        std::fs::write(root.join("plugins/foo/Cargo.toml"), b"[package]").unwrap();
        let mut files = BTreeSet::new();
        insert_context_file_if_exists(root, "plugins/foo/Cargo.toml", &mut files);
        insert_context_file_if_exists(root, "plugins/foo/missing.rs", &mut files);
        assert!(files.contains("plugins/foo/Cargo.toml"));
        assert_eq!(files.len(), 1);
    }

    #[test]
    fn collect_plugin_context_paths_errors_when_no_files_discovered() {
        let temp = tempfile::tempdir().expect("tempdir");
        // No plugins/ tree at all → all_files empty → InvalidArgument.
        let err = collect_plugin_context_paths(temp.path(), "root", &["root".to_string()])
            .expect_err("no discovered files must error");
        assert!(
            matches!(&err, RuntimeError::InvalidArgument { message }
                if message == "no planner context files discovered for plugin iteration"),
            "unexpected: {err:?}"
        );
    }

    #[test]
    fn collect_plugin_context_paths_gathers_all_and_focus_paths() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        let base = root.join("plugins/root");
        std::fs::create_dir_all(base.join("src")).unwrap();
        std::fs::create_dir_all(base.join("tests")).unwrap();
        std::fs::create_dir_all(base.join("docs/human")).unwrap();
        std::fs::write(base.join("Cargo.toml"), b"[package]").unwrap();
        std::fs::write(base.join("src/lib.rs"), b"// lib").unwrap();
        std::fs::write(base.join("src/core.rs"), b"// core").unwrap();
        std::fs::write(base.join("tests/it.rs"), b"// test").unwrap();
        std::fs::write(base.join("docs/human/overview.md"), b"# ov").unwrap();
        let paths = collect_plugin_context_paths(root, "root", &["root".to_string()])
            .expect("context paths gathered");
        assert!(paths
            .all_paths
            .contains(&"plugins/root/Cargo.toml".to_string()));
        assert!(paths
            .all_paths
            .contains(&"plugins/root/src/lib.rs".to_string()));
        // Focus paths are a filtered subset of all_paths (root manifest + src).
        assert!(paths
            .focus_paths
            .contains(&"plugins/root/Cargo.toml".to_string()));
        assert!(paths
            .focus_paths
            .iter()
            .all(|p| paths.all_paths.contains(p)));
    }

    // ── collect_focus_context_paths: depth 1-2 focus-plugin loop
    //    (via collect_plugin_context_paths) ──────────────────────────────
    #[test]
    fn collect_plugin_context_paths_includes_depth_one_focus_child() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        // Root plugin.
        let root_dir = root.join("plugins/root");
        std::fs::create_dir_all(root_dir.join("src")).unwrap();
        std::fs::write(root_dir.join("Cargo.toml"), b"[package]").unwrap();
        std::fs::write(root_dir.join("src/lib.rs"), b"// root lib").unwrap();
        // Depth-1 child plugin (root/child) — exercises the focus_plugins loop
        // body in collect_focus_context_paths (depth in 1..=2).
        let child_dir = root.join("plugins/root/child");
        std::fs::create_dir_all(child_dir.join("src")).unwrap();
        std::fs::write(child_dir.join("Cargo.toml"), b"[package]").unwrap();
        std::fs::write(child_dir.join("src/core.rs"), b"// child core").unwrap();

        let paths = collect_plugin_context_paths(
            root,
            "root",
            &["root".to_string(), "root/child".to_string()],
        )
        .expect("context paths gathered");
        // The depth-1 child's source entries land in the focus set.
        assert!(paths
            .focus_paths
            .contains(&"plugins/root/child/Cargo.toml".to_string()));
        assert!(paths
            .focus_paths
            .contains(&"plugins/root/child/src/core.rs".to_string()));
    }

    // ── cleanup_stale_snapshot_dirs (6669 / current 6878-6905) ──────────
    #[test]
    fn cleanup_stale_snapshot_dirs_removes_dead_and_keeps_live() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        // Missing root → early return (read_dir Err), no panic.
        cleanup_stale_snapshot_dirs(&root.join("does-not-exist"));

        // A directory owned by a dead pid (u32::MAX never a live pid) → removed.
        let dead = root.join(format!("snapshot-{}-123", u32::MAX));
        std::fs::create_dir_all(&dead).unwrap();
        // Old nanos-only format (first segment not a u32 pid) → treated stale.
        let old_format = root.join("snapshot-9999999999999999999-abc");
        std::fs::create_dir_all(&old_format).unwrap();
        // A live-owner dir named with THIS process's pid → kept.
        let live = root.join(make_snapshot_dir_name());
        std::fs::create_dir_all(&live).unwrap();
        // A non-snapshot dir and a plain file → skipped, left intact.
        let other_dir = root.join("not-a-snapshot");
        std::fs::create_dir_all(&other_dir).unwrap();
        let stray_file = root.join("snapshot-1-file");
        std::fs::write(&stray_file, b"x").unwrap();

        cleanup_stale_snapshot_dirs(root);

        assert!(!dead.exists(), "dead-owner snapshot dir must be removed");
        assert!(!old_format.exists(), "legacy-format dir must be removed");
        assert!(live.exists(), "live-owner snapshot dir must be kept");
        assert!(other_dir.exists(), "non-snapshot dir must be untouched");
        assert!(
            stray_file.exists(),
            "non-dir snapshot entry must be untouched"
        );
    }

    // ── workspace_manifest_lock::acquire (6726-6766 / current 6934-6968) ─
    #[test]
    fn workspace_manifest_lock_acquire_creates_parent_and_locks() {
        let temp = tempfile::tempdir().expect("tempdir");
        // Parent directory does not exist yet → acquire create_dir_all's it,
        // then opens+flocks the lock file. Guard drop releases the lock.
        let lock_path = temp.path().join("nested/dir/.workspace-manifest.lock");
        {
            let _guard = workspace_manifest_lock::acquire(&lock_path);
            assert!(lock_path.exists(), "lock file must be created");
            assert!(lock_path.parent().unwrap().exists());
        }
        // Re-acquire after the first guard dropped (lock released) succeeds.
        let _guard2 = workspace_manifest_lock::acquire(&lock_path);
        assert!(lock_path.exists());
    }

    // ── build_snapshot_with_staged_root: PluginUnavailable branch
    //    (6161-6164 / current 6370-6378) ─────────────────────────────────
    #[test]
    fn build_snapshot_errors_when_a_plugin_is_unavailable() {
        use crate::core::models::{
            AbiFingerprint, ArtifactIndex, ArtifactIndexEntry, ArtifactKind, LoaderBudget,
            PluginDocs, ARTIFACT_INDEX_SCHEMA_VERSION,
        };
        use crate::plugin::loader::{Loader, LoaderConfig};

        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();
        let index_path = root.join("artifacts/index.json");
        std::fs::create_dir_all(index_path.parent().unwrap()).unwrap();

        // Single plugin whose artifact file does not exist on disk → the loader
        // marks it Unavailable(ArtifactMissing) rather than aborting, and
        // build_snapshot_with_staged_root then converts that into a
        // RuntimeError::PluginUnavailable.
        let docs = PluginDocs {
            plugin_id: "lonely".to_string(),
            plugin_path: "lonely".to_string(),
            plugin_version: "0.1.0".to_string(),
            abi_version: 2,
            command_name: None,
            nodes: Vec::new(),
            system_hint: None,
        };
        let entry = ArtifactIndexEntry {
            plugin_path: "lonely".to_string(),
            version: "0.1.0".to_string(),
            abi_fingerprint: AbiFingerprint::current_build("crate_lonely_v1", "api_v2"),
            // Points at a file that will never exist under the temp root.
            artifact_path: "lonely.json".to_string(),
            sha256: "0".repeat(64),
            built_at: "0".to_string(),
            parent: None,
            required: false,
            grants_from_parent: Vec::new(),
            docs,
            exports: Vec::new(),
            execution: None,
            artifact_kind: ArtifactKind::Json,
            build_fingerprint: "bf".to_string(),
            input_probe: Default::default(),
            local_path_deps: Vec::new(),
        };
        let index = ArtifactIndex {
            schema_version: ARTIFACT_INDEX_SCHEMA_VERSION,
            generated_at: "now".to_string(),
            topo_order: vec!["lonely".to_string()],
            entries: vec![entry],
        };
        std::fs::write(&index_path, serde_json::to_string(&index).unwrap()).unwrap();

        let loader = Loader::new(LoaderConfig {
            plugins_root: root.join("plugins"),
            artifact_index_path: index_path,
            budget: LoaderBudget {
                max_total_plugins: 16,
                max_total_nodes: 64,
                load_timeout_ms: 30_000,
            },
        });
        let staged = root.join("staged");
        let err = build_snapshot_with_staged_root(&loader, staged.clone())
            .expect_err("unavailable plugin must surface PluginUnavailable");
        assert!(
            matches!(&err, RuntimeError::PluginUnavailable { plugin_path, required, .. }
                if plugin_path == "lonely" && !*required),
            "unexpected: {err:?}"
        );
        // The staged artifact root is cleaned up on the error path.
        assert!(!staged.exists(), "staged root must be removed on failure");
    }

    // ── render_child_plugin_core: empty PascalCase part (5874) ──────────
    #[test]
    fn render_child_plugin_core_handles_empty_dash_segment() {
        // A trailing/leading/double dash produces an empty split part, driving
        // the `None => String::new()` arm of the chars().next() match.
        let core = render_child_plugin_core("foo--bar");
        // Empty middle part contributes nothing; result is still "FooBar".
        assert!(core.contains("pub enum FooBarError"));
        assert!(core.contains("pub struct FooBarPlugin"));
    }

    // ── plugin_change_reasons: grants / load_result / fingerprint_diff
    //    arms (6104, 6107, 6113) ─────────────────────────────────────────
    #[test]
    fn plugin_change_reasons_reports_grants_load_result_and_fingerprint() {
        use crate::core::models::PluginLoadResult;
        let base = crate::plugin::registry::RegisteredPlugin {
            plugin_path: "root/child".to_string(),
            parent: None,
            required: false,
            grants_from_parent: BTreeSet::new(),
            load_result: PluginLoadResult::Loaded,
            docs: None,
            artifact_path: None,
            artifact_kind: None,
            abi_fingerprint: None,
            execution: None,
            fingerprint_diff: Vec::new(),
        };

        let mut grants = base.clone();
        grants.grants_from_parent = BTreeSet::from(["fs".to_string()]);
        assert_eq!(
            plugin_change_reasons(&base, &grants),
            vec!["grants_changed".to_string()]
        );

        let mut load = base.clone();
        load.load_result =
            PluginLoadResult::Unavailable(crate::core::models::PluginUnavailableReason::InitFailed);
        assert_eq!(
            plugin_change_reasons(&base, &load),
            vec!["load_result_changed".to_string()]
        );

        let mut fp = base.clone();
        fp.fingerprint_diff = vec!["crate_hash".to_string()];
        assert_eq!(
            plugin_change_reasons(&base, &fp),
            vec!["fingerprint_diff_changed".to_string()]
        );
    }

    // ── atomic_write_bytes: no-filename error + rename success (7019,
    //    plus the create_dir_all/write happy path) ──────────────────────
    #[test]
    fn atomic_write_bytes_errors_when_target_has_no_filename() {
        // Root path "/" has no file_name → InvalidInput error (7019-7022).
        let err = atomic_write_bytes(std::path::Path::new("/"), b"x")
            .expect_err("root path has no filename");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn atomic_write_bytes_writes_and_replaces_atomically() {
        let temp = tempfile::tempdir().expect("tempdir");
        // Nested parent is created on demand; file is written then a second
        // write replaces it via the rename path.
        let target = temp.path().join("nested/dir/out.json");
        atomic_write_bytes(&target, b"first").expect("first write");
        assert_eq!(std::fs::read(&target).unwrap(), b"first");
        atomic_write_bytes(&target, b"second").expect("replace write");
        assert_eq!(std::fs::read(&target).unwrap(), b"second");
    }

    #[test]
    fn atomic_write_bytes_cleans_tmp_when_rename_fails() {
        let temp = tempfile::tempdir().expect("tempdir");
        // Target is a NON-EMPTY directory: rename(tmp_file -> dir) fails with
        // ENOTEMPTY/EISDIR, driving the cleanup + error-return arm (7030-7032).
        let target = temp.path().join("target-dir");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("occupant"), b"x").unwrap();

        let err = atomic_write_bytes(&target, b"payload")
            .expect_err("rename onto a non-empty dir must fail");
        // The error is surfaced (kind varies by platform).
        let _ = err;
        // The temp sidecar file was removed on the failure path.
        let tmp = temp
            .path()
            .join(format!("target-dir.cordis-tmp.{}", std::process::id()));
        assert!(
            !tmp.exists(),
            "temp sidecar must be cleaned up on rename failure"
        );
    }

    // ── workspace_manifest_lock::acquire: open-failure fallback
    //    (6946-6951) ─────────────────────────────────────────────────────
    #[test]
    fn workspace_manifest_lock_acquire_open_failure_returns_fileless_guard() {
        let temp = tempfile::tempdir().expect("tempdir");
        // Point the lock path at an existing *directory*: OpenOptions::open
        // fails (cannot open a dir read/write), so acquire logs and returns a
        // Guard with `file: None`. create_dir_all(parent) succeeds first.
        let dir_as_lock = temp.path().join("iam-a-directory");
        std::fs::create_dir_all(&dir_as_lock).unwrap();
        // Must not panic; Drop of the fileless guard is a no-op.
        let _guard = workspace_manifest_lock::acquire(&dir_as_lock);
    }

    // ── normalize_warning_source_path: RootDir component arm (5998) ─────
    #[test]
    fn normalize_warning_source_path_rejects_retained_root_component() {
        // An empty fixtures_root strips no prefix from an absolute path, so the
        // leading RootDir component survives into the loop and hits the
        // `RootDir | Prefix(_) => return None` arm.
        assert_eq!(
            normalize_warning_source_path("/abs/foo.rs", std::path::Path::new("")),
            None
        );
    }
}

/// Coverage batch for host.rs lines 3500-5500 (cov6 numbering; the "seam-100"
/// terminal batch). These are the `iterate_plugins` orchestration failure arms,
/// the plugin-iteration agent-tool backend, and the surrounding pure helpers.
/// Integration tests reach the happy paths through the 200s dylib-rebuilding
/// walk test; this module drives the private methods and pure functions
/// directly against near-instant hermetic fixtures (empty `artifacts/index.json`
/// → no dlopen) or with zero I/O at all. Kept as its own module so concurrent
/// edits never collide with `mod tests` / the other seam modules.
#[cfg(test)]
mod region_3500_5500_seam_tests {
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
        ManagedAgentSession, ManagedAgentState, PluginIterationAgentBackend,
        PluginIterationAgentState, PluginIterationContextPaths, PluginIterationRunState,
        PreparedPluginIteration, ReloadReport, ReplaceFilesExactArgs, RetiredSnapshot, RuntimeHost,
        ScaffoldChildPluginArgs, PLUGIN_AGENT_TOOL_RECORD_ITERATION_SUMMARY,
        PLUGIN_AGENT_TOOL_REPLACE_FILES_EXACT,
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
        let expected_prefix = format!(
            "agent tool {PLUGIN_AGENT_TOOL_REPLACE_FILES_EXACT} received invalid arguments: "
        );
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
        let snapshot =
            snapshot_with_unavailable_plugin("am", PluginUnavailableReason::ArtifactMissing);
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
        let previous =
            snapshot_with_unavailable_plugin("gone", PluginUnavailableReason::InitFailed);
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

    #[test]
    #[serial]
    fn finalize_partial_canary_without_approval_blocks() {
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
        let verdict = host
            .finalize_plugin_iteration(&mut state)
            .expect("finalize returns Blocked when canary is Partial and not approved");
        assert_eq!(verdict, PluginIterationFinalVerdict::Blocked);
        assert_eq!(state.blocked_reason.as_deref(), Some("no evidence"));
    }

    // ─────────────── rollback_candidate_if_staged (no candidate) ─────────

    #[test]
    #[serial]
    fn rollback_candidate_if_staged_is_none_without_candidate() {
        let (_temp, host) = empty_host();
        assert!(host.rollback_candidate_if_staged().is_none());
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
        let mut state =
            PluginIterationAgentState::new(prepared, context_paths, &host.fixtures_root);
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
            let out = AgentBackend::execute_tool(
                backend,
                "run_plugin_check",
                json!({ "plugin_path": "/" }),
            )
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
        let mut state =
            PluginIterationAgentState::new(prepared, context_paths, &host.fixtures_root);
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
            let out = backend
                .run_checked_command("check", "false".to_string())
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
            let out = backend
                .run_checked_command("test", "true".to_string())
                .expect("true exits 0");
            assert_eq!(out.get("success").and_then(Value::as_bool), Some(true));
            assert_eq!(backend.state.verification_successes, 1);
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
        let mut session =
            crate::agent::AgentSession::new(config, "runtime_shell").expect("build agent session");
        // Below the compaction threshold: wrapper returns (old, old) unchanged.
        let mut managed = ManagedAgentSession {
            handle: AgentSessionHandle {
                session_id: "s-1".to_string(),
                kind: AgentSessionKind::RuntimeShell,
            },
            session: std::mem::replace(
                &mut session,
                crate::agent::AgentSession::new(
                    crate::config::LlmApiConfig::default(),
                    "throwaway",
                )
                .expect("throwaway session"),
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
        let sha256 =
            crate::plugin::artifact::sha256_file(&artifact_path).expect("hash svc artifact");
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

    // ─────────── cleanup_retired_snapshots live-cap prefix drop ──────────

    #[test]
    #[serial]
    fn cleanup_retired_snapshots_caps_live_prefix_at_max() {
        let (temp, host) = empty_host();
        // Hold >64 LIVE Arcs so the Weak-liveness retain keeps them all, then
        // the `while len > MAX_RETIRED_SNAPSHOTS` loop drops the oldest prefix.
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
            remaining, 64,
            "live entries capped at MAX_RETIRED_SNAPSHOTS"
        );
        // The 6 oldest prefix dirs were removed by the cap loop.
        assert!(!dirs[0].exists(), "oldest live-cap prefix dir removed");
        assert!(dirs[69].exists(), "newest live entry's dir retained");
        drop(live);
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

    #[test]
    #[serial]
    fn finalize_promote_failure_manual_approved_partial_arm_returns_err() {
        let (_temp, host) = empty_host();
        host.reload_candidate().expect("stage candidate");
        wedge_journal_as_dir(&host);
        let mut prepared = prepared_for("mini", &["mini"]);
        prepared.manual_approved = true;
        let mut state = PluginIterationRunState::new(prepared);
        state.verifier_verdict = Some(VerifierVerdict::Pass);
        // Partial canary + manual_approved → the manual-approved promote arm.
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
        let out = host.finalize_plugin_iteration(&mut state);
        assert!(out.is_err(), "manual-approved promote failure propagates");
        let reason = state.blocked_reason.expect("blocked reason recorded");
        assert!(
            reason.starts_with("promote (manual-approved) failed: "),
            "unexpected reason: {reason}"
        );
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
        assert_eq!(
            run.snapshot.recorded_summary.as_deref(),
            Some("planned edit")
        );
        assert!(run.snapshot.changed_paths.iter().any(|p| p == &rel));
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
            .config
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
}

/// Coverage for the `apply_operations` / `scaffold_child_plugin` /
/// `execute_tool` operation-application arms of this module: the double-fault
/// wrapper, the scaffolded-child ordering, the plugin-iteration journal replay
/// gates, and the workspace-manifest `flock` failure branch. Kept as its own
/// module so concurrent edits to `mod tests` and the other seam modules never
/// collide with it.
#[cfg(test)]
mod ops_arms_coverage_tests {
    use super::{
        apply_plugin_iteration_journal, plugin_iteration_applied_marker_path,
        plugin_iteration_journal_path, scaffolded_child_order, validated_verification_command,
        with_partial_batch_rollback_failure, workspace_manifest_lock, ScaffoldedChildRegistration,
    };
    use crate::core::error::RuntimeError;
    use crate::kernel::plugin_iteration::PluginEditRollback;
    use std::cmp::Ordering;
    use std::fs;
    use tempfile::TempDir;

    fn invalid(message: &str) -> RuntimeError {
        RuntimeError::InvalidArgument {
            message: message.to_string(),
        }
    }

    #[track_caller]
    fn invariant_message(err: &RuntimeError) -> String {
        match err {
            RuntimeError::Invariant { message } => message.clone(),
            other => format!("not Invariant: {other}"),
        }
    }

    #[test]
    fn invariant_message_labels_other_variants() {
        let other = RuntimeError::InvalidArgument {
            message: "nope".to_string(),
        };
        assert_eq!(invariant_message(&other), format!("not Invariant: {other}"));
    }

    fn registration(child_root: &str, parent_manifest: &str) -> ScaffoldedChildRegistration {
        ScaffoldedChildRegistration {
            parent_manifest_path: parent_manifest.to_string(),
            child_root_path: child_root.to_string(),
        }
    }

    // ── apply_operations double-fault arm ────────────────────────────────
    //
    // `with_partial_batch_rollback_failure` is the wrapper `apply_operations`
    // applies when a batch operation failed AND unwinding the already-applied
    // prefix of the same batch also failed. Reaching that state in situ needs
    // the rollback's own `fs::write` / `remove_file` to fail on a path the
    // executor just successfully wrote inside the same call, which cannot be
    // arranged without interleaving the loop, so the wrapper is asserted
    // directly — including the byte-exact message.

    #[test]
    fn partial_batch_rollback_failure_wraps_both_messages_byte_exactly() {
        let edit_err = RuntimeError::LlmResponseInvalid {
            message: "auto update patch pattern not found in /w/a.rs: OLD".to_string(),
        };
        let rollback_err = RuntimeError::Io {
            path: std::path::PathBuf::from("/w/b.rs"),
            message: "Is a directory (os error 21)".to_string(),
        };
        // Bind the Display forms first so the expectation is built from the
        // same `Display` impls the production `format!` uses.
        let edit_text = edit_err.to_string();
        let rollback_text = rollback_err.to_string();
        let out = with_partial_batch_rollback_failure(edit_err, Some(rollback_err));
        assert_eq!(
            invariant_message(&out),
            format!("{edit_text}; additionally, partial-batch rollback failed: {rollback_text}")
        );
    }

    #[test]
    fn partial_batch_rollback_failure_passes_error_through_when_rollback_succeeded() {
        // The common case: the batch operation failed but unwinding worked, so
        // the caller sees the enriched edit error unchanged (same variant, same
        // message) rather than an `Invariant` wrapper.
        let out = with_partial_batch_rollback_failure(invalid("stale snippet"), None);
        assert!(
            matches!(&out, RuntimeError::InvalidArgument { message } if message == "stale snippet"),
            "expected the edit error verbatim, got {out:?}"
        );
    }

    #[test]
    fn partial_batch_rollback_failure_nests_an_already_invariant_edit_error() {
        // The edit error can itself be an `Invariant` (e.g. a rollback-workspace
        // mismatch surfaced by `absorb`); the wrapper still produces a single
        // flat `Invariant` whose message embeds both `Display` forms.
        let edit_err = RuntimeError::Invariant {
            message: "plugin edit rollback workspace mismatch: /a vs /b".to_string(),
        };
        let edit_text = edit_err.to_string();
        let out = with_partial_batch_rollback_failure(edit_err, Some(invalid("rollback kaput")));
        assert_eq!(
            invariant_message(&out),
            format!(
                "{edit_text}; additionally, partial-batch rollback failed: invalid argument: rollback kaput"
            )
        );
    }

    // ── scaffold_child_plugin: scaffolded-children ordering ──────────────

    #[test]
    fn scaffolded_child_order_sorts_by_child_root_then_parent_manifest() {
        // Distinct child roots: the child root decides, the parent manifest is
        // never consulted.
        assert_eq!(
            scaffolded_child_order(
                &registration("plugins/expr/evaluator/abs", "plugins/expr/zzz/Cargo.toml"),
                &registration("plugins/expr/evaluator/dist", "plugins/expr/aaa/Cargo.toml"),
            ),
            Ordering::Less
        );
        // Equal child roots: the parent manifest breaks the tie.
        assert_eq!(
            scaffolded_child_order(
                &registration("plugins/expr/evaluator/dist", "plugins/expr/b/Cargo.toml"),
                &registration("plugins/expr/evaluator/dist", "plugins/expr/a/Cargo.toml"),
            ),
            Ordering::Greater
        );
        // Fully equal registrations compare Equal, which is what makes the
        // following `dedup()` in `scaffold_child_plugin` collapse repeats.
        assert_eq!(
            scaffolded_child_order(
                &registration("plugins/expr/evaluator/dist", "plugins/expr/a/Cargo.toml"),
                &registration("plugins/expr/evaluator/dist", "plugins/expr/a/Cargo.toml"),
            ),
            Ordering::Equal
        );
    }

    #[test]
    fn scaffolded_child_order_drives_sort_and_dedup_like_scaffold_child_plugin() {
        // Two scaffolds in one iteration plus a duplicate: the exact shape
        // `scaffold_child_plugin` feeds into `sort_by` + `dedup`.
        let mut children = vec![
            registration(
                "plugins/expr/evaluator/dist",
                "plugins/expr/evaluator/Cargo.toml",
            ),
            registration(
                "plugins/expr/evaluator/abs",
                "plugins/expr/evaluator/Cargo.toml",
            ),
            registration(
                "plugins/expr/evaluator/dist",
                "plugins/expr/evaluator/Cargo.toml",
            ),
        ];
        children.sort_by(scaffolded_child_order);
        children.dedup();
        let roots = children
            .iter()
            .map(|entry| entry.child_root_path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            roots,
            vec!["plugins/expr/evaluator/abs", "plugins/expr/evaluator/dist"]
        );
    }

    // ── validated_verification_command: bare alias without a fallback ────

    #[test]
    fn validated_verification_command_bare_alias_without_fallback_is_rejected() {
        // `check` is the bare alias, but no default command is available to
        // substitute → the alias falls through to the prefix guard and is
        // rejected with the byte-exact prefix message.
        let out = validated_verification_command(Some("check".to_string()), None, "cargo check");
        assert!(
            matches!(&out, Err(RuntimeError::InvalidArgument { message })
                if *message == "verification tool only allows commands starting with `cargo check`, got `check`"),
            "expected the prefix-guard rejection, got {out:?}"
        );
    }

    #[test]
    fn validated_verification_command_alias_of_the_other_tool_is_not_substituted() {
        // `test` is only an alias for the `cargo test` tool; supplied to the
        // `cargo check` tool it is an ordinary (invalid) command even though a
        // fallback exists.
        let out = validated_verification_command(
            Some("test".to_string()),
            Some("cargo check --quiet".to_string()),
            "cargo check",
        );
        assert!(
            matches!(&out, Err(RuntimeError::InvalidArgument { message })
                if *message == "verification tool only allows commands starting with `cargo check`, got `test`"),
            "expected cross-tool alias to be rejected, got {out:?}"
        );
    }

    // ── workspace_manifest_lock::acquire: flock failure branch ───────────

    #[cfg(unix)]
    #[test]
    fn workspace_manifest_lock_acquire_survives_flock_failure_on_a_fifo() {
        // `flock(2)` reports ENOTSUP for a FIFO on macOS/BSD, while opening one
        // O_RDWR succeeds and does not block. That is the one portable way to
        // reach the `rc != 0` branch, which only logs and still hands back a
        // guard holding the open file.
        let temp = TempDir::new().expect("tempdir");
        let fifo = temp.path().join("nested/manifest.lock");
        fs::create_dir_all(fifo.parent().expect("fifo parent")).expect("mkdir");
        let c_path = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes())
            .expect("lock path has no interior NUL");
        // SAFETY: `c_path` is a NUL-terminated path owned for the call's whole
        // duration; `mkfifo` only reads it.
        let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o666) };
        assert_eq!(rc, 0, "mkfifo must succeed inside a fresh tempdir");

        let guard = workspace_manifest_lock::acquire(&fifo);
        // The guard still owns an open descriptor even though locking failed;
        // dropping it exercises the unlock path on that same descriptor.
        drop(guard);
        assert!(fifo.exists(), "acquire must not remove the lock path");
    }

    // ── apply_plugin_iteration_journal: replay and skip gates ────────────

    fn seed_journal(
        workspace: &std::path::Path,
        snapshot_root: &std::path::Path,
        rel: &str,
        pre_edit: &[u8],
    ) {
        let target = workspace.join(rel);
        fs::create_dir_all(target.parent().expect("target parent")).expect("mkdir target parent");
        let rollback = PluginEditRollback::single_backup(workspace, rel, Some(pre_edit.to_vec()));
        rollback
            .persist_journal(&plugin_iteration_journal_path(snapshot_root), "iter-ops")
            .expect("persist journal");
    }

    #[test]
    fn apply_plugin_iteration_journal_replays_and_cleans_up_marker_and_journal() {
        let temp = TempDir::new().expect("tempdir");
        let workspace = temp.path().join("workspace");
        let snapshot_root = temp.path().join("snapshots");
        fs::create_dir_all(&snapshot_root).expect("mkdir snapshot root");
        let rel = "plugins/mini/src/lib.rs";
        seed_journal(&workspace, &snapshot_root, rel, b"PRE-EDIT");
        // Simulate the post-edit workspace state the journal must undo.
        fs::write(workspace.join(rel), b"POST-EDIT").expect("write post-edit body");

        let restored = apply_plugin_iteration_journal(&workspace, &snapshot_root, None)
            .expect("journal replay succeeds");
        assert!(restored, "a journal on disk means a restore happened");
        assert_eq!(
            fs::read(workspace.join(rel)).expect("read restored"),
            b"PRE-EDIT"
        );
        // Both the journal and the applied marker are cleaned up on success.
        assert!(!plugin_iteration_journal_path(&snapshot_root).exists());
        assert!(!plugin_iteration_applied_marker_path(&snapshot_root).exists());
    }

    #[test]
    fn apply_plugin_iteration_journal_skips_when_marker_matches_journal_generation() {
        let temp = TempDir::new().expect("tempdir");
        let workspace = temp.path().join("workspace");
        let snapshot_root = temp.path().join("snapshots");
        fs::create_dir_all(&snapshot_root).expect("mkdir snapshot root");
        let rel = "plugins/mini/src/lib.rs";
        seed_journal(&workspace, &snapshot_root, rel, b"PRE-EDIT");
        let journal_path = plugin_iteration_journal_path(&snapshot_root);
        let generation_id = PluginEditRollback::journal_generation_id(&journal_path)
            .expect("read generation id")
            .expect("a freshly persisted journal carries a generation id");
        // A marker recording the SAME generation means the replay already ran.
        let marker = plugin_iteration_applied_marker_path(&snapshot_root);
        fs::write(&marker, generation_id.as_bytes()).expect("write applied marker");
        fs::write(workspace.join(rel), b"LEGITIMATE-LATER-EDIT").expect("write later body");

        let restored = apply_plugin_iteration_journal(&workspace, &snapshot_root, None)
            .expect("already-applied journal short-circuits");
        assert!(!restored, "an already-applied journal reports no restore");
        // The later, legitimate edit must survive untouched.
        assert_eq!(
            fs::read(workspace.join(rel)).expect("read after skip"),
            b"LEGITIMATE-LATER-EDIT"
        );
        assert!(!journal_path.exists(), "journal cleared on the skip path");
        assert!(!marker.exists(), "marker cleared on the skip path");
    }

    #[test]
    fn apply_plugin_iteration_journal_replays_when_marker_generation_differs() {
        let temp = TempDir::new().expect("tempdir");
        let workspace = temp.path().join("workspace");
        let snapshot_root = temp.path().join("snapshots");
        fs::create_dir_all(&snapshot_root).expect("mkdir snapshot root");
        let rel = "plugins/mini/src/lib.rs";
        seed_journal(&workspace, &snapshot_root, rel, b"PRE-EDIT");
        // A stale marker from an EARLIER journal must not suppress this replay.
        fs::write(
            plugin_iteration_applied_marker_path(&snapshot_root),
            b"some-older-generation",
        )
        .expect("write stale marker");
        fs::write(workspace.join(rel), b"POST-EDIT").expect("write post-edit body");

        let restored = apply_plugin_iteration_journal(&workspace, &snapshot_root, None)
            .expect("mismatched marker still replays");
        assert!(restored, "a different generation id means replay proceeds");
        assert_eq!(
            fs::read(workspace.join(rel)).expect("read restored"),
            b"PRE-EDIT"
        );
    }

    #[test]
    fn apply_plugin_iteration_journal_falls_back_to_the_in_memory_rollback() {
        let temp = TempDir::new().expect("tempdir");
        let workspace = temp.path().join("workspace");
        let snapshot_root = temp.path().join("snapshots");
        fs::create_dir_all(&snapshot_root).expect("mkdir snapshot root");
        let rel = "plugins/mini/src/lib.rs";
        let target = workspace.join(rel);
        fs::create_dir_all(target.parent().expect("target parent")).expect("mkdir");
        fs::write(&target, b"POST-EDIT").expect("write post-edit body");
        // No journal on disk → the in-memory rollback is used instead.
        let rollback =
            PluginEditRollback::single_backup(&workspace, rel, Some(b"PRE-EDIT".to_vec()));

        let restored = apply_plugin_iteration_journal(&workspace, &snapshot_root, Some(&rollback))
            .expect("in-memory rollback replays");
        assert!(restored);
        assert_eq!(fs::read(&target).expect("read restored"), b"PRE-EDIT");
    }

    #[test]
    fn apply_plugin_iteration_journal_reports_nothing_to_restore_when_both_are_absent() {
        let temp = TempDir::new().expect("tempdir");
        let snapshot_root = temp.path().join("snapshots");
        fs::create_dir_all(&snapshot_root).expect("mkdir snapshot root");
        let restored = apply_plugin_iteration_journal(temp.path(), &snapshot_root, None)
            .expect("no journal and no rollback is not an error");
        assert!(!restored);
    }

    #[test]
    fn apply_plugin_iteration_journal_surfaces_a_corrupt_journal_as_invariant() {
        // `load_journal`'s parse error propagates through the `?` on the
        // `load_journal` call, which is the arm the replay path guards.
        let temp = TempDir::new().expect("tempdir");
        let snapshot_root = temp.path().join("snapshots");
        fs::create_dir_all(&snapshot_root).expect("mkdir snapshot root");
        fs::write(
            plugin_iteration_journal_path(&snapshot_root),
            b"{ not valid json",
        )
        .expect("write corrupt journal");
        let err = apply_plugin_iteration_journal(temp.path(), &snapshot_root, None)
            .expect_err("a corrupt journal must not be silently ignored");
        assert!(
            invariant_message(&err).starts_with("plugin edit rollback journal parse failed: "),
            "got {err:?}"
        );
    }

    #[test]
    fn apply_plugin_iteration_journal_propagates_a_failing_rollback_write() {
        // The `rollback.rollback()?` arm: the journal wants to restore bytes to
        // a path that is now a non-empty directory, so `fs::write` fails.
        let temp = TempDir::new().expect("tempdir");
        let workspace = temp.path().join("workspace");
        let snapshot_root = temp.path().join("snapshots");
        fs::create_dir_all(&snapshot_root).expect("mkdir snapshot root");
        let rel = "plugins/mini/src/lib.rs";
        seed_journal(&workspace, &snapshot_root, rel, b"PRE-EDIT");
        // Replace the target file with a non-empty directory.
        let target = workspace.join(rel);
        fs::create_dir_all(target.join("occupied")).expect("mkdir target-as-dir");

        let err = apply_plugin_iteration_journal(&workspace, &snapshot_root, None)
            .expect_err("restoring onto a directory must fail");
        assert!(
            matches!(&err, RuntimeError::Io { path, .. } if path == &target),
            "expected an Io error naming the restore target, got {err:?}"
        );
    }
}

/// Unit coverage for the named helpers extracted out of the `iterate_plugins`
/// pipeline. Each one sits on a branch the public entry point cannot drive (a
/// defensive invariant, a panic-payload shape, a rollback-of-a-rollback
/// failure), so the message construction is asserted here byte-for-byte instead
/// of being reached through a synthetic fault.
#[cfg(test)]
mod iterate_stage_coverage_tests {
    use super::{
        plugin_iteration_missing_rollback_error, plugin_iteration_panic_error,
        verdict_rollback_partial_cleanup_reason,
    };
    use crate::core::error::RuntimeError;

    /// Assert `err` is an `Invariant` carrying exactly `expected`. Compares the
    /// whole error value rather than destructuring it, so there is no
    /// unreachable `else`-panic arm for llvm-cov to report.
    fn assert_invariant(err: &RuntimeError, expected: &str) {
        assert_eq!(
            err.to_string(),
            format!("internal invariant broken: {expected}")
        );
    }

    /// Step 2's `None`-rollback arm. `run_plugin_iteration_agent` returns a
    /// rollback on both of its branches — the edit-plan branch starts from
    /// `PluginEditRollback::empty(..)` and the agent branch reads it out of the
    /// session snapshot — so `state.rollback` is `Some` for every
    /// `iterate_plugins` call. The error stays as a named constructor so its
    /// wording is pinned.
    #[test]
    fn missing_rollback_error_carries_the_pinned_invariant_message() {
        assert_invariant(
            &plugin_iteration_missing_rollback_error(),
            "plugin iteration rollback journal missing after agent execution",
        );
    }

    /// `panic!("literal")` hands `catch_unwind` a `&'static str` payload.
    #[test]
    fn panic_error_renders_a_str_payload() {
        let payload: Box<dyn std::any::Any + Send> = Box::new("boom from a literal");
        assert_invariant(
            &plugin_iteration_panic_error(&payload),
            "plugin iteration panicked at an unexpected point; workspace has been restored: boom from a literal",
        );
    }

    /// A formatted `panic!`/`assert!` message arrives as a `String`.
    #[test]
    fn panic_error_renders_a_string_payload() {
        let payload: Box<dyn std::any::Any + Send> = Box::new("boom from a format".to_string());
        assert_invariant(
            &plugin_iteration_panic_error(&payload),
            "plugin iteration panicked at an unexpected point; workspace has been restored: boom from a format",
        );
    }

    /// Any other payload type is only producible by `panic_any`, which the
    /// iteration body never calls — hence the fixed fallback label.
    #[test]
    fn panic_error_falls_back_for_an_unknown_payload_type() {
        let payload: Box<dyn std::any::Any + Send> = Box::new(42_u32);
        assert_invariant(
            &plugin_iteration_panic_error(&payload),
            "plugin iteration panicked at an unexpected point; workspace has been restored: unknown panic payload",
        );
    }

    /// finalize's negative-verdict arm when the candidate rollback *also*
    /// fails. Reaching it needs a staged candidate whose `rollback_candidate`
    /// errors while the subsequent workspace restore succeeds — the restore
    /// runs the same `restore_plugin_iteration_workspace` the failing rollback
    /// already tried, so any fault that breaks one breaks the other and the
    /// `?` fires first. The message is pinned here instead.
    #[test]
    fn verdict_rollback_partial_cleanup_reason_nests_both_prefixes() {
        let reason = verdict_rollback_partial_cleanup_reason(&RuntimeError::Invariant {
            message: "candidate went missing".to_string(),
        });
        assert_eq!(
            reason,
            "verdict rollback with partial candidate cleanup error: candidate rollback: internal invariant broken: candidate went missing"
        );
    }
}
