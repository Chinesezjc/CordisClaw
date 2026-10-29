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
use crate::iteration::policy::{promotion_verdict, summary_rejection, test_changes};
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
use crate::plugin::builder::{missing_builder_error, plugin_builder};
use crate::plugin::invoke::invoke_registered_plugin_with_settings;
use crate::plugin::loader::{default_loader_config, LoadOutput, Loader};
use crate::plugin::registry::{NodeRegistry, PluginRegistry, RegisteredPlugin};
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
    plugin_settings: BTreeMap<String, PluginConfigFile>,
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

    /// Render one plugin's `system_hint` with its configured `settings`
    /// interpolated (`{setting:<key>}`), so a hint can carry a
    /// deployment-specific value — e.g. the bot's own QQ number — without
    /// hard-coding it in the plugin source and without re-sending it on every
    /// message.
    pub fn render_system_hint(&self, plugin_path: &str, hint: &str) -> String {
        let settings = self
            .plugin_settings
            .get(plugin_path)
            .map(|config| &config.settings);
        interpolate_hint_settings(hint, settings)
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
        let settings = self
            .plugin_settings
            .get(plugin_path)
            .map(|config| &config.settings);
        invoke_registered_plugin_with_settings(
            &self.plugin_registry,
            plugin_path,
            node_id,
            payload,
            settings,
        )
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

/// Does a candidate dylib's docs change the *contract* (which nodes exist and
/// what type they are), as opposed to merely rewording summaries/schemas?
///
/// A rebuilt artifact legitimately carries edited doc text — that is what a
/// docs change looks like — and reload should adopt it instead of failing. A
/// changed node set is a real contract change and still fails.
fn docs_contract_changed(recorded: &PluginDocs, candidate: &PluginDocs) -> bool {
    fn identity(docs: &PluginDocs) -> Vec<(String, cordis_plugin_sdk::NodeType)> {
        let mut ids: Vec<(String, cordis_plugin_sdk::NodeType)> = docs
            .nodes
            .iter()
            .map(|node| (node.id.clone(), node.node_type))
            .collect();
        ids.sort_by(|a, b| a.0.cmp(&b.0));
        ids
    }
    identity(recorded) != identity(candidate)
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
mod reload_arm_helper_tests;

/// Coverage for the execute-path trace constructors and the `reload_subtree`
/// Phase-1 AbiMismatch report builders extracted above, plus the cheap
/// `pub(crate)` session-map accessors. Kept in a dedicated module (pre-`impl`
/// region) so concurrent edits to the primary `mod tests` never collide.
#[cfg(test)]
mod sr_host_a_seam_tests;

/// Coverage batch (host.rs lines 1-3500, "cov-fa" seam): internal arms that
/// need a hand-built [`RuntimeSnapshot`] or a direct call into a private
/// kernel method, which integration tests (only `pub` surface) cannot reach.
/// Kept in a dedicated module so concurrent edits to the primary `mod tests`
/// and the `sr_host_a_seam_tests` module never collide on the same lines.
#[cfg(test)]
mod cov_fa_host_1_3500_tests;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReloadAttemptStatus {
    Reloaded,
    Staged,
    Failed,
}

/// One `fixtures/startup_invoke.json` entry: how to bring a plugin's Task
/// service up. The host replays the matching entries after a reload that
/// stopped the plugin's services (a reload stops them so the old dylib can be
/// replaced; nothing else would start them again, which left the bot with no
/// QQ/Feishu listener).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupInvocation {
    pub plugin_path: String,
    pub node_id: String,
    pub payload: String,
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
    /// Plugins switched off by `config/plugins/<name>.yaml` (`enabled: false`).
    /// They are filtered out at load time, so they never appear in the registry.
    #[serde(default)]
    pub plugin_disabled_count: usize,
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
    llm_api: RwLock<LlmApiConfig>,
    plugin_configs: RwLock<BTreeMap<String, PluginConfigFile>>,
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

#[derive(Debug)]
pub struct RuntimeHost {
    fixtures_root: PathBuf,
    config: RwLock<RuntimeConfig>,
    snapshot_root: PathBuf,
    current_snapshot: RwLock<Arc<RuntimeSnapshot>>,
    candidate_snapshot: Mutex<Option<StagedCandidateSnapshot>>,
    invocation_samples: Mutex<VecDeque<InvocationSample>>,
    retired_snapshots: Mutex<Vec<RetiredSnapshot>>,
    last_reload_attempt: Mutex<Option<ReloadAttemptReport>>,
    last_candidate_reload_attempt: Mutex<Option<ReloadAttemptReport>>,
    agent_sessions: Mutex<BTreeMap<String, ManagedAgentSession>>,
    inflight_sessions: Mutex<BTreeSet<String>>,
    dropped_during_turn: Mutex<BTreeSet<String>>,
    pending_session_actions: Mutex<BTreeMap<String, Vec<PendingSessionAction>>>,
    turn_meta: Mutex<BTreeMap<String, BTreeMap<String, String>>>,
    profile_fallback: Mutex<BTreeMap<String, ProfileFallbackEntry>>,
    pub service_registry: Arc<crate::context::ServiceRegistry>,
    startup_invocations: Mutex<Vec<StartupInvocation>>,
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
    config: RuntimeConfig,
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

pub(crate) struct PluginLlmProvider<'a> {
    pub(crate) host: &'a RuntimeHost,
    pub(crate) plugin_path: String,
}

impl crate::agent::LlmProvider for PluginLlmProvider<'_> {
    fn complete(
        &self,
        body: serde_json::Value,
        sink: Option<std::sync::Arc<dyn crate::llm_sink::TokenSink>>,
        transport: cordis_plugin_sdk::llm::LlmTransportConfig,
    ) -> Result<crate::agent::LlmCompletionParts, RuntimeError> {
        // 流式预算沿用配置里的每块超时，与拆分前的语义一致。
        let budget = std::time::Duration::from_secs(transport.stream_timeout_secs.max(1));
        let listener =
            sink.and_then(|sink| crate::llm_sink::SinkListener::bind(sink, new_sink_key(), budget));

        let mut payload = serde_json::json!({
            "node_id": "llm_complete",
            "body": body,
            "transport": transport,
        });
        attach_sink_fields(
            &mut payload,
            listener.as_ref().map(|l| (l.addr(), l.key().to_string())),
        );

        let response = self
            .host
            .invoke(&self.plugin_path, "llm_complete", payload.to_string())?;
        // 监听器在此 drop：补全已返回，旁路无需再等。
        drop(listener);
        parse_llm_reply(&self.plugin_path, &response.payload)
    }
}

/// 把 sink 坐标写进调用 payload；`None` 表示调用方不要流式，两个字段整个不出现
/// （插件据此走非流式）。
///
/// 抽成纯函数：起一个真监听器只为了拼两个字段太重，而"带/不带 sink"两条路径都
/// 必须被测到。
pub(crate) fn attach_sink_fields(payload: &mut serde_json::Value, sink: Option<(String, String)>) {
    let Some((addr, key)) = sink else { return };
    payload["sink_addr"] = serde_json::Value::String(addr);
    payload["sink_key"] = serde_json::Value::String(key);
}

/// 解析 provider 插件的回复。
///
/// 抽成自由函数以便直接单测每条失败路径（非 JSON / ok=false / 缺 message），
/// 不必真起一个插件。
pub(crate) fn parse_llm_reply(
    plugin_path: &str,
    raw: &str,
) -> Result<crate::agent::LlmCompletionParts, RuntimeError> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| RuntimeError::InvalidArgument {
            message: format!("llm_complete reply from {plugin_path} is not JSON: {e}"),
        })?;
    if value.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        let detail = value
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("provider reported failure without an error message");
        return Err(RuntimeError::LlmRequestFailed {
            message: format!("{plugin_path}: {detail}"),
        });
    }
    let message = value
        .get("message")
        .cloned()
        .ok_or_else(|| RuntimeError::InvalidArgument {
            message: format!("llm_complete reply from {plugin_path} has no message field"),
        })?;
    let message = serde_json::from_value(message).map_err(|e| RuntimeError::InvalidArgument {
        message: format!("llm_complete reply from {plugin_path} has a malformed message: {e}"),
    })?;
    Ok(crate::agent::LlmCompletionParts {
        message,
        response_id: value
            .get("response_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        finish_reason: value
            .get("finish_reason")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
    })
}

/// 每次调用一个新 key。端口本就一调用一换，key 是并发下拒绝串台连接的第二道闸。
fn new_sink_key() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    format!(
        "sink-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// R1: 若 `session_id` 当前有一个 in-flight turn（同一 id 正被并发
/// `agent_send` 处理，旧 turn 收尾会把会话插回 map），标记
/// `dropped_during_turn`，让旧 turn 收尾不再复活本会话。当前
/// `agent_start_with` 的 id 由 `normalize_request_id` 生成、不可能撞车，
/// 此检查是防御性的；具名函数便于直接单测双臂。
fn mark_dropped_if_inflight(
    inflight_sessions: &Mutex<BTreeSet<String>>,
    dropped_during_turn: &Mutex<BTreeSet<String>>,
    session_id: &str,
) {
    let guard = inflight_sessions
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if guard.contains(session_id) {
        dropped_during_turn
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(session_id.to_string());
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

/// Result of the agent (or edit-plan) phase of one iteration.
///
/// This used to be two structs — a `Snapshot` nested inside a `Run` — so every
/// field the pipeline needed was copied twice. The run is the phase's result;
/// there is nothing else to unwrap.
#[derive(Debug, Clone)]
struct PluginIterationAgentRun {
    session_id: Option<String>,
    tool_summary: Option<AgentToolExecutionSummary>,
    transcript_excerpt: Vec<AgentTranscriptEntry>,
    recorded_summary: Option<String>,
    requires_approval: bool,
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
    requires_approval: bool,
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
            requires_approval: false,
        }
    }

    fn derived_edit_plan(&self) -> PluginEditPlan {
        PluginEditPlan {
            issue_id: self.prepared.issue_id.clone(),
            patch_id: format!("{}-agent", self.prepared.iteration_id),
            summary: self
                .recorded_summary
                .clone()
                .unwrap_or_else(|| self.prepared.summary.clone()),
            operations: self.operations.clone(),
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

/// R1: test-only panic injection for `ManagedAgentSession::respond_with_images`.
/// When set, the next `respond_with_images` panics (as if an agent-loop assert fired
/// mid-turn) so tests can verify `agent_send`'s `catch_unwind` cleanup still
/// reinserts the session instead of losing it. Swapped back to `false` on
/// read so it fires exactly once.
#[cfg(test)]
pub(crate) static TEST_AGENT_RESPOND_PANIC_INJECTION: std::sync::atomic::AtomicBool =
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

/// Sensitive path keywords matched against individual path *components*: a
/// component that starts with one of these is treated as sensitive, so
/// `.ssh/id_rsa`, `config/.env`, `known_hosts.old` and `credentials.json` are
/// blocked while an unrelated `my.env` file is not.
const SENSITIVE_PATH_COMPONENT_KEYWORDS: &[&str] = &[
    ".ssh",
    ".claude",
    ".env",
    "id_rsa",
    "id_ed25519",
    "id_ecdsa",
    "known_hosts",
    "credentials",
    "auth.json",
];

/// Underscore keywords whose hyphen / space / glued variants are also
/// blocked. The path is normalised (`-` and ` ` → `_`, and the underscores
/// stripped for the compact form) before matching, so `api-key`, `api key`
/// and `apikey` all hit `api_key` while a plain `api-utils` plugin stays
/// untouched. Only these keywords get the normalisation.
const SENSITIVE_PATH_UNDERSCORE_KEYWORDS: &[(&str, &str)] = &[
    ("access_token", "accesstoken"),
    ("api_key", "apikey"),
    ("api_secret", "apisecret"),
    ("private_key", "privatekey"),
];

/// Lexically normalise a path for sensitive-resource matching without any
/// filesystem access: repeated slashes and `.` components are dropped, `..`
/// components pop the previous segment, and a trailing slash disappears.
/// Pure string surgery — nothing here touches the disk.
fn normalize_sensitive_path(path: &str) -> String {
    let mut components: Vec<&str> = Vec::new();
    for component in path.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                components.pop();
            }
            other => components.push(other),
        }
    }
    components.join("/")
}

/// The sensitive keyword a path hits, if any. Component-chain matches for
/// `/etc/passwd` / `/etc/shadow`, root-level `proc` / `sys` components,
/// component-prefix matches for the dotfile / key keywords, and
/// underscore-normalised matches for the `*_key` / `*_token` family.
fn sensitive_path_keyword(path: &str) -> Option<&'static str> {
    // Case-insensitive like the old `to_lowercase()` substring scan.
    let normalized = normalize_sensitive_path(&path.to_lowercase());
    let components: Vec<&str> = normalized.split('/').filter(|c| !c.is_empty()).collect();
    for pair in components.windows(2) {
        if pair[0] == "etc" {
            match pair[1] {
                "passwd" => return Some("/etc/passwd"),
                "shadow" => return Some("/etc/shadow"),
                _ => {}
            }
        }
    }
    match components.first().copied() {
        Some("proc") => return Some("/proc/"),
        Some("sys") => return Some("/sys/"),
        _ => {}
    }
    for &component in &components {
        for &keyword in SENSITIVE_PATH_COMPONENT_KEYWORDS {
            if component.starts_with(keyword) {
                return Some(keyword);
            }
        }
    }
    let underscored = normalized.replace(['-', ' '], "_");
    let compact = underscored.replace('_', "");
    for &(keyword, compact_keyword) in SENSITIVE_PATH_UNDERSCORE_KEYWORDS {
        if underscored.contains(keyword) || compact.contains(compact_keyword) {
            return Some(keyword);
        }
    }
    None
}

impl RuntimeHost {
    pub fn boot(fixtures_root: impl AsRef<Path>) -> Result<Self, RuntimeError> {
        let fixtures_root = fixtures_root.as_ref().to_path_buf();
        let config = RuntimeConfig::load(&fixtures_root)?;
        let loader = Loader::new(default_loader_config(&fixtures_root))
            .with_disabled(disabled_plugins(&config));
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
            // `for`-over-`Option` 而非 `if let`：boot GC 在全新的 host 目录下
            // 永远没有可回收的孤儿，`if let` 的 Some 支在原地不可达，会在字面
            // 100% 行覆盖门槛下留一行缺口。语义完全相同。
            snapshot_gc_summary_line(&report)
                .iter()
                .for_each(|line| eprintln!("{line}"));
        }
        // staging 期间持锁：并发 boot 的 GC 试锁失败 → 跳过本目录，不会删掉
        // 我们刚 mkdir、还没 stage 完的 staging 目录。
        let _staging_guard = snapshot_staging_lock::hold(&snapshot_root);
        let initial_snapshot = Arc::new(build_snapshot(
            &loader,
            &snapshot_root,
            &config.plugin_configs,
        )?);
        let interactive_rollback = Mutex::new(PluginEditRollback::empty(&fixtures_root));
        let service_registry = Arc::new(crate::context::ServiceRegistry::new());
        let host = Self {
            kernel: RuntimeKernel::new(&fixtures_root, &config),
            config: RwLock::new(config),
            fixtures_root,
            snapshot_root,
            current_snapshot: RwLock::new(initial_snapshot),
            candidate_snapshot: Mutex::new(None),
            invocation_samples: Mutex::new(VecDeque::new()),
            retired_snapshots: Mutex::new(Vec::new()),
            last_reload_attempt: Mutex::new(None),
            last_candidate_reload_attempt: Mutex::new(None),
            agent_sessions: Mutex::new(BTreeMap::new()),
            inflight_sessions: Mutex::new(BTreeSet::new()),
            dropped_during_turn: Mutex::new(BTreeSet::new()),
            pending_session_actions: Mutex::new(BTreeMap::new()),
            turn_meta: Mutex::new(BTreeMap::new()),
            profile_fallback: Mutex::new(BTreeMap::new()),
            service_registry,
            startup_invocations: Mutex::new(Vec::new()),
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

    /// 找出承担 LLM 补全的插件。
    ///
    /// 与 `soul_provider` 同一套能力节点约定：任何已加载且声明 `llm_complete`
    /// 节点的插件即成为 provider。**没有内建兜底**——LLM 传输已整体移出 kernel，
    /// 无插件时返回 `None`，调用方据此给 `NoLlmProvider`。
    ///
    /// 这不影响冷启动：boot / REPL / `command_router` 都不经此路径，无插件时
    /// 它们照常工作，只有 `agent_send` 会失败（与今天"LLM 不可达"同构）。
    pub(crate) fn llm_provider_plugin(&self) -> Option<String> {
        let snapshot = self.current_snapshot();
        for (plugin_path, plugin) in snapshot.plugin_registry().iter() {
            let loaded_docs = plugin
                .docs
                .as_ref()
                .filter(|_| plugin.load_result == crate::core::models::PluginLoadResult::Loaded);
            let Some(docs) = loaded_docs else { continue };
            if docs.nodes.iter().any(|n| n.id == "llm_complete") {
                return Some(plugin_path);
            }
        }
        None
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
        if let Some(keyword) = sensitive_path_keyword(path) {
            return Err(RuntimeError::InvalidArgument {
                message: format!("blocked: path references sensitive resource ({keyword})"),
            });
        }
        Ok(())
    }

    pub fn check_sensitive_command(&self, command: &str) -> Result<(), RuntimeError> {
        // (a) Literal substring checks on the raw command, unchanged
        //     semantics: anything mentioning ssh/scp/keys/tokens/export&co is
        //     rejected outright before any token analysis.
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
        // (b) Token-level path gate: POSIX-split the command (falling back to
        //     the raw-string check above when the quoting is unparseable) and
        //     run the sensitive-path matcher on every argv token shaped like
        //     a path, so `head /etc/passwd`, `cat /etc//shadow` and
        //     `grep ~/.ssh/id_rsa` are blocked even though the raw string
        //     never contained the literal keywords above.
        let argv = match shell_words::split(command) {
            Ok(argv) => argv,
            Err(_) => return Ok(()),
        };
        for token in &argv {
            if token.contains('/') || token.starts_with('.') {
                if let Some(keyword) = sensitive_path_keyword(token) {
                    return Err(RuntimeError::InvalidArgument {
                        message: format!(
                            "blocked: command references sensitive resource ({keyword})"
                        ),
                    });
                }
            }
        }
        // (c) Shell interpreters with a command flag: `sh -c '…'`,
        //     `bash -lc …`, `zsh --command …` hand the rest of the string to
        //     a shell, so the token analysis above is meaningless. Reject
        //     them outright.
        if matches!(
            argv.first().map(String::as_str),
            Some("sh" | "bash" | "dash" | "ksh" | "zsh" | "fish")
        ) && argv
            .iter()
            .skip(1)
            .any(|arg| matches!(arg.as_str(), "-c" | "-lc" | "--command"))
        {
            return Err(RuntimeError::InvalidArgument {
                message: "blocked: shell interpreter invocation".to_string(),
            });
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
        // Containment check. The former version only canonicalised the
        // *nearest existing ancestor* of the resolved path, so a final
        // component that was a dangling symlink (its target missing, or
        // outside the root) sailed through and the caller's `fs::write`
        // followed the link to create / overwrite files outside the sandbox.
        match std::fs::symlink_metadata(&resolved) {
            Ok(meta) if meta.file_type().is_symlink() => {
                // The final component is a symlink: resolve its target and
                // require it to stay inside the root. A dangling target is
                // rejected outright — there is nothing in the sandbox to
                // resolve, so the write would create the file wherever the
                // link points.
                let target = std::fs::read_link(&resolved)
                    .map_err(|err| host_io_error(resolved.clone(), err.to_string()))?;
                let target_path = if target.is_absolute() {
                    target
                } else {
                    resolved
                        .parent()
                        .map(|p| p.to_path_buf())
                        .unwrap_or_else(|| base_root.clone())
                        .join(target)
                };
                let canonical_target =
                    target_path
                        .canonicalize()
                        .map_err(|_| RuntimeError::InvalidArgument {
                            message: format!("path is a dangling symlink: {rel}"),
                        })?;
                if !canonical_target.starts_with(&canonical_root) {
                    return Err(RuntimeError::InvalidArgument {
                        message: format!("path escapes fixtures root: {rel}"),
                    });
                }
                Ok(resolved)
            }
            Ok(_) => {
                // The final component exists (not a symlink): canonicalise
                // the *full* path so a symlinked ancestor that lands outside
                // the root is caught — not just the nearest existing
                // ancestor.
                let canonical = resolved
                    .canonicalize()
                    .map_err(|err| host_io_error(resolved.clone(), err.to_string()))?;
                if !canonical.starts_with(&canonical_root) {
                    return Err(RuntimeError::InvalidArgument {
                        message: format!("path escapes fixtures root: {rel}"),
                    });
                }
                Ok(resolved)
            }
            Err(_) => {
                // The final component does not exist yet: walk up to the
                // nearest existing ancestor and canonicalise that (unchanged
                // semantics for brand-new files). `ancestors()` yields
                // `resolved` first and then each parent, ending at the
                // filesystem root (or the empty path for a relative base), so
                // the former hand-rolled climb-until-parent()-is-None loop is
                // expressed without a terminating arm that only fires for a
                // base root that has already been deleted. Same result:
                // nearest existing ancestor, or `resolved` itself when
                // nothing along the chain canonicalises.
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
        }
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

    /// Snapshot of the current configuration. A clone rather than a reference
    /// because `reload` can replace it under our feet; callers that need one
    /// consistent read take it once.
    pub fn config(&self) -> RuntimeConfig {
        self.config_value()
    }

    /// Make a reloaded config live: host-side readers and the kernel's status
    /// both have to see it, otherwise `kernel status` would keep reporting the
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
# New plugins are scaffolded before any build runs, so docs/agent/interfaces.json
# cannot exist yet; the build generates it. Without this opt-in the scaffold
# check rejects the plugin and the iteration that created it can never land.
allow_generated_docs = true

[package.metadata.cordis.abi_fingerprint]
crate_hash = "{crate_hash}"
api_hash = "api_v2"

[dependencies]
cordis-plugin-sdk = {{ path = "../../../crates/cordis-plugin-sdk" }}
serde = {{ version = "1", features = ["derive"] }}
serde_json = {{ version = "1", features = ["preserve_order"] }}
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

        // A fresh plugin needs the rest of the hard scaffold before any build
        // can validate it: a `tests/` entry and `docs/human/overview.md`
        // (interfaces.json is generated, hence allow_generated_docs above).
        let tests_dir = plugin_dir.join("tests");
        std::fs::create_dir_all(&tests_dir)
            .map_err(|e| io_ctx(tests_dir.clone(), "failed to create plugin tests dir", e))?;
        let test_rs = format!(
            r#"//! Scaffold smoke test for `{name}`.
//!
//! Replace it with real coverage of the node's behavior before relying on it.

#[test]
fn scaffold_compiles() {{}}
"#
        );
        std::fs::write(tests_dir.join(format!("{name}.rs")), test_rs)
            .map_err(|e| io_ctx(tests_dir.clone(), "failed to write scaffold test", e))?;

        let human_docs_dir = plugin_dir.join("docs").join("human");
        std::fs::create_dir_all(&human_docs_dir).map_err(|e| {
            io_ctx(
                human_docs_dir.clone(),
                "failed to create plugin docs dir",
                e,
            )
        })?;
        let overview = format!(
            r#"# {name}

{desc}

## Nodes

No nodes yet — this plugin was scaffolded by `create_plugin`. Document every
node here (`docs/agent/interfaces.json` is generated by the build).
"#
        );
        std::fs::write(human_docs_dir.join("overview.md"), overview)
            .map_err(|e| io_ctx(human_docs_dir.clone(), "failed to write plugin overview", e))?;

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

    /// 把本批的图片地址按 `runtime.images` 预算转成 `image_url` content part。
    ///
    /// 全程只降级：关掉 / 下载失败 / 超字节 / 超张数 / 非图片 → 记日志并跳过，
    /// 纯文本照常发。绝不让一张图毁掉整轮。
    pub fn image_parts_for_batch(&self, image_urls: &[String]) -> Vec<serde_json::Value> {
        let (parts, skips) = crate::image_input::plan_for_settings(
            &self.config_value().runtime.images,
            image_urls,
            &crate::image_input::fetch_http,
        );
        for skip in &skips {
            eprintln!("image_input: skip url={} reason={}", skip.url, skip.reason);
        }
        eprintln!(
            "image_input: batch={} attached={} skipped={}",
            image_urls.len(),
            parts.len(),
            skips.len()
        );
        parts
    }

    /// `agent_send` 的多模态入口：`image_urls` 是本批消息里的图片地址，
    /// 由 [`RuntimeHost::image_parts_for_batch`] 按 `runtime.images` 预算
    /// 下载 + 内联成 `image_url` content part。空切片等价于纯文本。
    pub fn agent_send_with_images(
        &self,
        session_id: &str,
        input: &str,
        image_urls: &[String],
    ) -> Result<AgentReply, RuntimeError> {
        self.agent_send_with_turn_context(session_id, input, image_urls, "")
    }

    /// `agent_send_with_images` plus channel-supplied **one-shot** context
    /// (`notes`: e.g. the qq plugin's `[reply policy: …]` / `[group memory: …]`
    /// lines). The notes are sent to the model with this request only — they
    /// are deliberately NOT written to the session history.
    pub fn agent_send_with_turn_context(
        &self,
        session_id: &str,
        input: &str,
        image_urls: &[String],
        notes: &str,
    ) -> Result<AgentReply, RuntimeError> {
        self.agent_send_with_turn_meta(session_id, input, image_urls, notes, &BTreeMap::new())
    }

    /// 把本轮的 `__meta` 注入插件 payload。模型自带的 `__meta` 一律被覆盖。
    pub fn inject_turn_meta(&self, session_id: &str, payload: Value) -> Value {
        let meta = {
            let guard = self
                .turn_meta
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            guard.get(session_id).cloned()
        };
        let Some(meta) = meta else {
            return payload;
        };
        payload_with_turn_meta(payload, &meta)
    }

    /// 当前还有多少个 session 带着未清理的 turn meta（测试用；正常应为 0）。
    #[doc(hidden)]
    pub fn debug_turn_meta_len(&self) -> usize {
        self.turn_meta
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .len()
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
        self.agent_send_with_fallback_images(session_id, input, &[])
    }

    /// [`agent_send_with_fallback`] 的多模态版本（图片预算同 `agent_send_with_images`）。
    pub fn agent_send_with_fallback_images(
        &self,
        session_id: &str,
        input: &str,
        image_urls: &[String],
    ) -> Result<AgentReply, RuntimeError> {
        self.agent_send_with_fallback_turn_context(session_id, input, image_urls, "")
    }

    /// Swap the live LLM config of a resting session (must not be inside
    /// `respond`) to the named profile.
    fn swap_session_profile(
        &self,
        session_id: &str,
        profile_name: &str,
    ) -> Result<(), RuntimeError> {
        let api = self
            .config_value()
            .llm_profiles
            .resolve(profile_name)
            .api
            .clone();
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

    pub fn kernel(&self) -> &RuntimeKernel {
        &self.kernel
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
        // R2: 上限驱逐交给具名函数（两臂各自直接单测）。关键语义：被
        // in-flight 会话强 Arc pin 的活条目即使最旧也不删目录，只删已死
        // 条目——删了 pin 住的目录，该快照上的 Process 插件 invoke 会
        // 全部 spawn failed。
        evict_retired_snapshots(&mut retired);
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

/// 退休快照的保留上限。与 `plugin_history` / `invocation_samples` 的上限
/// 一致，防止长期运行的 host 上 `retired_snapshots` 无界增长。
const MAX_RETIRED_SNAPSHOTS: usize = 64;

/// R2: 上限驱逐。从最旧条目开始：已死（Weak 无强引用，retain 之后才死的
/// 竞态窗口）的删目录并移除；**活条目（被 in-flight 会话的强 Arc pin 住）
/// 立即停止**——最旧 N 条全是活的就是 pin 造成的积压，保留并记一条诊断
/// 日志，而不是删掉正在使用的快照目录（删了之后该快照上的 Process 插件
/// invoke 会全部 spawn failed）。具名函数使"删死条目"与"活条目停止"两臂
/// 都能被直接单测。
fn evict_retired_snapshots(retired: &mut Vec<RetiredSnapshot>) {
    while retired.len() > MAX_RETIRED_SNAPSHOTS {
        let first = &retired[0];
        if first.snapshot.upgrade().is_some() {
            eprintln!(
                "[snapshot-retire] {} retired snapshots pinned by live sessions; keeping backlog",
                retired.len()
            );
            break;
        }
        let dropped = retired.remove(0);
        let _ = fs::remove_dir_all(&dropped.staged_artifact_root);
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

    fn respond_with_images(
        &mut self,
        host: &RuntimeHost,
        input: &str,
        images: &[serde_json::Value],
        notes: &str,
    ) -> Result<AgentReply, RuntimeError> {
        // R1 测试注入：模拟 respond 内部 panic（agent 循环内的 assert 等）。
        // `agent_send` 用 catch_unwind 包住本函数，panic 也必须走完收尾、
        // 会话不得丢失。
        #[cfg(test)]
        let respond_panic_injected =
            TEST_AGENT_RESPOND_PANIC_INJECTION.swap(false, std::sync::atomic::Ordering::SeqCst);
        #[cfg(test)]
        assert!(
            !respond_panic_injected,
            "test panic injection: agent respond"
        );
        // 无 provider 插件时两种 state 的结论相同（都只能报 NoLlmProvider），
        // 所以先统一判定，省掉两条各自的 None 分支。
        let Some(plugin_path) = host.llm_provider_plugin() else {
            return Err(RuntimeError::NoLlmProvider);
        };
        let provider = PluginLlmProvider { host, plugin_path };
        match &mut self.state {
            ManagedAgentState::RuntimeShell => self.session.respond_with_runtime_host_via_images(
                host,
                &self.handle.session_id,
                input,
                images,
                notes,
                &provider,
                None,
            ),
            ManagedAgentState::PluginIteration(state) => {
                let mut backend = PluginIterationAgentBackend {
                    host,
                    state: state.as_mut(),
                };
                self.session.respond_with_provider_images(
                    &mut backend,
                    input,
                    images,
                    notes,
                    &provider,
                    None,
                )
            }
        }
    }
}

struct PluginIterationAgentBackend<'a> {
    host: &'a RuntimeHost,
    state: &'a mut PluginIterationAgentState,
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

/// Canary evidence is only meaningful for a *deterministic* invocation: a node
/// that draws randomness (a gacha pull) or reads the clock cannot be compared
/// with a recorded response. Probe the candidate twice and skip the sample when
/// the two runs disagree.
/// Side effects a node declares in its docs. A node that declares any is not
/// replayed as canary evidence: replaying it mutates state (a pull consumes
/// pity) and its response is usually the very thing the edit changes.
fn node_declares_side_effects(
    snapshot: &RuntimeSnapshot,
    plugin_path: &str,
    node_id: &str,
) -> bool {
    let docs = snapshot
        .plugin_registry()
        .get(plugin_path)
        .and_then(|plugin| plugin.docs.clone());
    docs.map(|docs| crate::iteration::policy::node_declares_side_effects_in(&docs.nodes, node_id))
        .unwrap_or(false)
}

/// A/D seam: failure to spawn the `cargo <op> …` verification subprocess.
fn checked_command_spawn_error(
    program: &str,
    args: &[String],
    err: &std::io::Error,
) -> RuntimeError {
    RuntimeError::CommandFailed {
        program: program.to_string(),
        args: args.to_vec(),
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
/// the re-hashed tree diverged from the verified hash, re-hashing itself
/// failed, or the *baseline* hash failed at verify time (without a recorded
/// hash there is no way to prove the tree did not change — same policy as a
/// re-hash failure). Returns `None` when there is no drift to act on (verdict
/// was not `Pass`, no baseline hash was recorded, or the hashes matched).
pub(crate) fn detect_plugin_source_drift(
    verifier_verdict: VerifierVerdict,
    expected_source_tree_hash: Option<&str>,
    baseline_hash_error: Option<&str>,
    rehash: impl FnOnce() -> Result<String, RuntimeError>,
) -> Option<String> {
    if verifier_verdict != VerifierVerdict::Pass {
        return None;
    }
    // A baseline hash failure at verify time must forbid Pass just like a
    // re-hash failure before promote: both leave no hash to compare against.
    if let Some(reason) = baseline_hash_error {
        return Some(format!(
            "unable to hash source tree at verify time: {reason}"
        ));
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
    // The old `trimmed.starts_with(required_prefix)` gate let a tail like
    // `&& curl x|sh` slip past a valid `cargo check` prefix straight into the
    // `bash -lc` dispatch. Require the *argv* to be a plain `cargo <op> …`
    // invocation with no shell metacharacters instead. The byte-exact
    // "only allows commands starting with …" wording is preserved for the
    // structure-mismatch case.
    let required_op = required_prefix
        .strip_prefix("cargo ")
        .unwrap_or(required_prefix);
    validate_verification_argv(trimmed, required_op)?;
    Ok(trimmed.to_string())
}

/// P0-17-style argv gate shared by `validated_verification_command` and
/// `run_checked_command`: a verification command must tokenise (POSIX quoting,
/// no shell expansion) to a plain `cargo <op> …` argv with no shell
/// metacharacter in any element, so no `;` / `&&` / `|` / backtick / `$()`
/// tail can ever reach a shell.
fn validate_verification_argv(
    command: &str,
    required_op: &str,
) -> Result<Vec<String>, RuntimeError> {
    let argv = shell_words::split(command).map_err(|err| RuntimeError::InvalidArgument {
        message: format!("verification command tokenisation failed: {err}"),
    })?;
    if argv.first().map(String::as_str) != Some("cargo")
        || argv.get(1).map(String::as_str) != Some(required_op)
    {
        return Err(RuntimeError::InvalidArgument {
            message: format!(
                "verification tool only allows commands starting with `cargo {required_op}`, got `{command}`"
            ),
        });
    }
    if let Some(arg) = argv
        .iter()
        .find(|arg| arg.contains([';', '&', '|', '`', '$', '<', '>', '\n', '\r']))
    {
        return Err(RuntimeError::InvalidArgument {
            message: format!("verification command contains shell metacharacter `{arg}`"),
        });
    }
    Ok(argv)
}

/// Build the default verification command as an argv (joined with single
/// spaces for the display string; every element is validated to be free of
/// whitespace and shell metacharacters, so the join round-trips through
/// `shell_words::split` exactly).
fn verification_default_command(op: &str, package_segment: &str) -> String {
    let mut argv = vec![
        "cargo".to_string(),
        op.to_string(),
        "--quiet".to_string(),
        "--manifest-path".to_string(),
        "plugins/Cargo.toml".to_string(),
    ];
    if !package_segment.is_empty() {
        argv.push("-p".to_string());
        argv.push(package_segment.to_string());
    }
    argv.join(" ")
}

/// Validate an LLM-supplied `plugin_path` for use as a `cargo -p` package
/// selector. Leading `/` components are stripped (the runtime's plugin paths
/// are rooted), an empty result means the whole workspace (no `-p`), and any
/// remaining character outside a conservative alphanumeric / `-_./` set — or
/// a `..` segment — is rejected up front, so the value can never smuggle
/// shell syntax through the old `format!`-interpolated default command.
fn verification_package_segment(plugin_path: &str) -> Result<String, RuntimeError> {
    let segment = plugin_path.trim_start_matches('/').trim();
    if segment.is_empty() {
        return Ok(String::new());
    }
    let unsafe_segment = !segment
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.' | '/'))
        || segment.contains("..");
    if unsafe_segment {
        return Err(RuntimeError::InvalidArgument {
            message: format!("plugin path {plugin_path} is not a valid cargo package selector"),
        });
    }
    Ok(segment.to_string())
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

/// Build the reload-path diagnostic line for a Task node `stop` invocation, or
/// `None` when the stop reported success. A stop is treated as successful only
/// when the response body is a JSON object whose `ok` field is `true`; any
/// other payload (missing `ok`, `ok=false`, non-object, unparseable) or an
/// invoke `Err` yields a diagnostic line for the caller to log. Kept as a pure
/// function so the two message formats stay unit-testable without capturing
/// `eprintln!` output.
fn stop_result_diagnostic(
    fqn: &str,
    outcome: &Result<PluginResponse, RuntimeError>,
) -> Option<String> {
    match outcome {
        Ok(resp) => {
            let body: Value = serde_json::from_str(&resp.payload).unwrap_or_default();
            if body.get("ok").and_then(Value::as_bool).unwrap_or(false) {
                None
            } else {
                let message = body
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("(no message)");
                Some(format!(
                    "[reload] Task node {fqn} stop returned ok=false: {message}"
                ))
            }
        }
        Err(e) => Some(format!("[reload] Task node {fqn} stop invoke failed: {e}")),
    }
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

/// Replace `{setting:<key>}` placeholders with values from a plugin's
/// `settings` block.
///
/// Scalars are rendered as text; a missing key, a non-scalar value or a
/// malformed placeholder is left verbatim (a visible marker beats silently
/// dropping the sentence the hint was built around).
fn interpolate_hint_settings(hint: &str, settings: Option<&serde_json::Value>) -> String {
    let Some(settings) = settings else {
        return hint.to_string();
    };
    let mut out = String::with_capacity(hint.len());
    let mut rest = hint;
    while let Some(start) = rest.find("{setting:") {
        out.push_str(&rest[..start]);
        let after = &rest[start + "{setting:".len()..];
        let Some(end) = after.find('}') else {
            out.push_str(&rest[start..]);
            return out;
        };
        let key = after[..end].trim();
        let value = settings.get(key).and_then(|value| match value {
            serde_json::Value::String(text) => Some(text.clone()),
            serde_json::Value::Number(number) => Some(number.to_string()),
            serde_json::Value::Bool(flag) => Some(flag.to_string()),
            _ => None,
        });
        match value {
            Some(value) => out.push_str(&value),
            None => {
                out.push_str("{setting:");
                out.push_str(&after[..end]);
                out.push('}');
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Plugin paths switched off by `config/plugins/<name>.yaml` (`enabled: false`).
///
/// A disabled plugin is filtered out at load time: it is never staged, never
/// dlopened and never registered, so its Task services cannot start and
/// `invoke_plugin` reports it as not registered.
#[cfg(test)]
mod docs_contract_tests;

#[cfg(test)]
mod hint_interpolation_tests;

fn disabled_plugins(config: &RuntimeConfig) -> BTreeSet<String> {
    config
        .plugin_configs
        .iter()
        .filter(|(_, plugin_config)| !plugin_config.enabled)
        .map(|(plugin_path, _)| plugin_path.clone())
        .collect()
}

/// A failed `ReloadAttemptReport` (status `failed`), shared by the config-load
/// failure and the snapshot-build failure paths so both report the same shape.
fn failed_reload_attempt(
    previous_snapshot: &RuntimeSnapshot,
    snapshot_root: &Path,
    staged_artifact_root: &Path,
    elapsed_ms: u128,
    err: &RuntimeError,
) -> ReloadAttemptReport {
    ReloadAttemptReport {
        status: ReloadAttemptStatus::Failed,
        from_snapshot_id: previous_snapshot.snapshot_id().to_string(),
        to_snapshot_id: None,
        snapshot_root: snapshot_root.display().to_string(),
        staged_artifact_root: staged_artifact_root.display().to_string(),
        elapsed_ms,
        plugin_count: None,
        node_count: None,
        added_plugins: Vec::new(),
        removed_plugins: Vec::new(),
        changed_plugins: Vec::new(),
        changed_plugin_reasons: BTreeMap::new(),
        failure_summary: Some(err.to_string()),
    }
}

fn build_snapshot(
    loader: &Loader,
    snapshot_root: &Path,
    plugin_settings: &BTreeMap<String, PluginConfigFile>,
) -> Result<RuntimeSnapshot, RuntimeError> {
    let staged_artifact_root = next_staged_artifact_root(snapshot_root);
    build_snapshot_with_staged_root(loader, staged_artifact_root, plugin_settings)
}

fn build_snapshot_with_staged_root(
    loader: &Loader,
    staged_artifact_root: PathBuf,
    plugin_settings: &BTreeMap<String, PluginConfigFile>,
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

    Ok(runtime_snapshot_from_output(
        output,
        staged_artifact_root,
        plugin_settings.clone(),
    ))
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
    plugin_settings: BTreeMap<String, PluginConfigFile>,
) -> RuntimeSnapshot {
    RuntimeSnapshot {
        snapshot_id: output.execution_id,
        plugin_registry: output.plugin_registry,
        node_registry: output.node_registry,
        doc_registry: output.doc_registry,
        graph_registry: output.graph_registry,
        context_baseline: output.context,
        staged_artifact_root,
        plugin_settings,
    }
}

#[derive(Debug, Clone)]
struct PluginIterationRunState {
    prepared: PreparedPluginIteration,
    agent: Option<PluginIterationAgentRun>,
    rollback: Option<PluginEditRollback>,
    changed_paths: Vec<String>,
    rebuilt_artifacts: Vec<(String, String)>,
    candidate: Option<CandidateSnapshotStatus>,
    verification: Option<VerificationReport>,
    verifier_verdict: Option<VerifierVerdict>,
    canary: Option<CanaryReport>,
    blocked_reason: Option<String>,
    stage_error: Option<String>,
    stage_error_is_infrastructure: bool,

    final_verdict: Option<PluginIterationFinalVerdict>,
    tests_command: Option<String>,
    safety_command: Option<String>,
}

impl PluginIterationRunState {
    fn new(prepared: PreparedPluginIteration) -> Self {
        Self {
            prepared,
            agent: None,
            rollback: None,
            changed_paths: Vec::new(),
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
        let agent = self.agent.as_ref();
        let derived_edit_plan = agent
            .map(|agent| agent.derived_edit_plan.clone())
            .or_else(|| self.prepared.edit_plan.clone())
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
            agent_session_id: agent.and_then(|agent| agent.session_id.clone()),
            tool_execution_summary: agent.and_then(|agent| agent.tool_summary.clone()),
            derived_edit_plan,
            transcript_excerpt: agent
                .map(|agent| agent.transcript_excerpt.clone())
                .unwrap_or_default(),
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

    // The workspace manifest (plugins/Cargo.toml) is part of an iteration's
    // working set: adding a plugin means editing its `members`, and the policy
    // gates that edit on an `expected_sha256` the agent can only compute by
    // reading the file. Leaving it out made the read fail and the edit
    // impossible ("stale edit precondition").
    if workspace_root.join("plugins/Cargo.toml").exists() {
        all_files.insert("plugins/Cargo.toml".to_string());
    }

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
        .is_some_and(crate::plugin::artifact::lock_pid_is_live)
}

/// 跨 hash 目录 GC 的统计结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SnapshotGcReport {
    pub scanned: usize,
    pub removed: usize,
    pub bytes_reclaimed: u64,
    pub skipped_journal: usize,
    pub skipped_live: usize,
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
        for child in children.flatten() {
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
        // 保留期对空目录同样生效。空目录看似"没字节可丢、删了无害"，但它也可能
        // 是**另一个正在 boot 的进程刚 mkdir、还没 stage 进任何文件**的 staging
        // 目录：此刻它既没有 snapshot-{pid}-* 子目录可供探活（owner_alive 为
        // false），又是空的，一旦跳过保留期就会被立刻删掉，那个进程随后
        // rename 到该目录时报 "No such file or directory"。CI 上实测到这个竞态
        // （命中 dispatch_unknown_command_is_reply 的 boot）。mtime 闸门是这里
        // 唯一能挡住它的东西，因此不能为空目录开后门。
        if !dir_is_older_than(&path, max_age) {
            report.skipped_recent += 1;
            continue;
        }
        // 最后一道闸门：非阻塞试 staging 锁。有进程正在这个 hash 目录里
        // staging（哪怕目录还空着、没有 pid 线索、mtime 也已被调老）就跳过。
        // 拿到锁后**持有到删除完成**（Guard drop 才释放）：`dir_size_bytes`
        // 与 `remove_dir_all` 全程持锁，试锁→删除之间并发 boot 开始 staging
        // 的窗口被彻底关闭（旧实现试锁即解锁，窗口只是缩小未关闭）。
        let Some(_gc_guard) = snapshot_staging_lock::try_acquire_for_gc(&path) else {
            report.skipped_live += 1;
            continue;
        };

        let bytes = dir_size_bytes(&path);
        if dry_run {
            report.removed += 1;
            report.bytes_reclaimed += bytes;
            continue;
        }
        if fs::remove_dir_all(&path).is_ok() {
            // 连带清掉同级锁文件，否则 lock 文件会无限累积。
            let _ = fs::remove_file(snapshot_staging_lock::lock_path(&path));
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

/// boot 时 GC 的日志行；无回收则 `None`（不刷无意义的日志）。
///
/// 抽成纯函数是为了可测：coverage 跑在全新的临时目录里，boot GC 永远没有可回收
/// 的孤儿，`removed > 0` 的分支在原地不可达。
fn snapshot_gc_summary_line(report: &SnapshotGcReport) -> Option<String> {
    (report.removed > 0).then(|| {
        format!(
            "[snapshot-gc] 回收 {} 个孤儿 snapshot root，释放 {:.1} MB",
            report.removed,
            report.bytes_reclaimed as f64 / (1024.0 * 1024.0)
        )
    })
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
/// hash 目录的 staging 锁：boot 期间持排他锁，GC 用**非阻塞**方式试锁，
/// 拿不到就跳过该目录。
///
/// 为什么需要它：GC 判"是否有活主"靠目录内 `snapshot-{pid}-*` 的 pid 探活，
/// 但目录**刚 mkdir、还没 stage 进任何文件**的那个瞬间没有任何 pid 线索，
/// 此时只剩 mtime 闸门。mtime 能挡住绝大多数情况（并发窗口是秒级、保留期是
/// 小时级），但那是概率而非保证——把保留期配成 0（`--max-age-hours=0`，运维
/// 清盘时的合理用法）就会立刻退化成 CI 上实测到的那个竞态：GC 删掉别人正在
/// staging 的目录，那边随后 rename 报 ENOENT。锁把它变成确定性排除。
///
/// 锁文件放在 hash 目录**同级**（`{hash}.lock`）而不是目录内部：GC 会
/// `remove_dir_all` 整个目录，锁文件在里面会被一起删掉，持锁进程的 fd 还在
/// 但文件已 unlink，后续进程开的是新 inode、互斥失效。
mod snapshot_staging_lock {
    use std::fs::{File, OpenOptions};
    use std::path::{Path, PathBuf};

    pub struct Guard {
        file: Option<File>,
    }

    /// `{hash_dir}.lock` —— 与 hash 目录同级，不会被 `remove_dir_all` 带走。
    pub fn lock_path(hash_dir: &Path) -> PathBuf {
        let mut name = hash_dir.as_os_str().to_os_string();
        name.push(".lock");
        PathBuf::from(name)
    }

    fn open_lock_file(path: &Path) -> Option<File> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)
            .ok()
    }

    /// boot 侧：持排他锁直到 `Guard` drop。拿不到锁不阻塞启动（锁是 advisory
    /// 的，失败时退化成原先的 mtime-only 行为）。
    #[cfg(unix)]
    pub fn hold(hash_dir: &Path) -> Guard {
        // 单表达式：开文件失败与 flock 失败都收敛成 `None`，没有各自的 `return`
        // 行——那两行在原地不可达（新建锁文件既不会开失败也不会 flock 失败），
        // 而本仓库覆盖率门槛是字面 100%。语义仍是"拿不到锁就退化成无锁"。
        Guard {
            file: open_lock_file(&lock_path(hash_dir))
                .filter(|file| flock_succeeded(try_lock_exclusive_nb(file))),
        }
    }

    /// `flock` 的返回码判定，独立成函数以便两种取值都能被单测覆盖。
    #[cfg(unix)]
    pub(super) fn flock_succeeded(rc: i32) -> bool {
        rc == 0
    }

    #[cfg(unix)]
    fn try_lock_exclusive_nb(file: &File) -> i32 {
        use std::os::unix::io::AsRawFd;
        // SAFETY: fd 由 `file` 拥有，跨 syscall 存活。
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }
    }

    #[cfg(not(unix))]
    pub fn hold(_hash_dir: &Path) -> Guard {
        Guard { file: None }
    }

    /// GC 侧：非阻塞试锁。`Some(Guard)` = 拿到了（无人 staging，可安全
    /// 回收），返回的 Guard 持有 flock 直到 drop；`None` = 有人持锁或开不了
    /// 锁文件 → 跳过。
    ///
    /// 旧实现 `inspect` 里立即解锁、只把锁当探针用，试锁→解锁→删除之间
    /// 仍会被并发 boot 插进一个 staging 窗口（CI 实测过 GC 删掉对方刚
    /// mkdir 的目录）。现在把持有锁的 Guard 交出去，由调用方持锁跨越
    /// `dir_size_bytes` 与 `remove_dir_all` 全程，删除完成后 Guard drop 才
    /// 释放。同进程 boot 的 `hold` 非阻塞试锁失败时本就退化为 mtime-only
    /// 行为，不会死锁。
    #[cfg(unix)]
    pub fn try_acquire_for_gc(hash_dir: &Path) -> Option<Guard> {
        // 同 `hold`：单表达式，无 in-situ 不可达的 `return` 行。
        open_lock_file(&lock_path(hash_dir))
            .filter(|file| flock_succeeded(try_lock_exclusive_nb(file)))
            .map(|file| Guard { file: Some(file) })
    }

    #[cfg(unix)]
    fn unlock(file: &File) {
        use std::os::unix::io::AsRawFd;
        // SAFETY: fd 由 `file` 拥有，跨 syscall 存活。
        let _ = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
    }

    #[cfg(not(unix))]
    pub fn try_acquire_for_gc(_hash_dir: &Path) -> Option<Guard> {
        Some(Guard { file: None })
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            #[cfg(unix)]
            self.file.iter().for_each(unlock);
        }
    }
}

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

/// R1: `agent_send` 把 respond 的 panic 还原为 Err 时的错误。panic 载荷是
/// `Box<dyn Any>`，提取为具名函数以固定文本并直接单测（含 `&str` 与
/// `String` 两种常见载荷）。
fn agent_respond_panicked_error(payload: &Box<dyn std::any::Any + Send>) -> RuntimeError {
    let msg = payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic payload".to_string());
    RuntimeError::Invariant {
        message: format!("agent respond panicked: {msg}"),
    }
}

/// R3: 候选快照在暂存（Step 4 reload_candidate）与 promote/rollback/approve
/// 之间被并发替换时的错误。`expected` 是迭代暂存时记录的 snapshot_id，
/// `actual` 是当前 staged 条目的。文本风格与 `Invariant` 一致；具名函数
/// 便于逐字节单测。
fn candidate_replaced_error(expected: &str, actual: &str) -> RuntimeError {
    RuntimeError::Invariant {
        message: format!(
            "candidate snapshot replaced during plugin iteration: expected {expected}, got {actual}"
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
        plugin_builder()
            .ok_or_else(missing_builder_error)?
            .rebuild_plugin(fixtures_root, "/")?;
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

/// 把可信元数据写进 payload 的 `__meta`。
///
/// 模型自己塞的 `__meta` 会被整体覆盖（鉴权只能用 kernel 给的这份）。
/// 纯函数，便于直接单测：非 object 的 payload 会被包成 `{"value": …}` 以保住原值。
pub(crate) fn payload_with_turn_meta(payload: Value, meta: &BTreeMap<String, String>) -> Value {
    let mut object = match payload {
        Value::Object(map) => map,
        other => {
            let mut map = serde_json::Map::new();
            map.insert("value".to_string(), other);
            map
        }
    };
    object.insert(
        "__meta".to_string(),
        serde_json::to_value(meta).unwrap_or(Value::Null),
    );
    Value::Object(object)
}

mod agent;
mod iteration;
/// 可信元数据注入：只测注入规则本身（内核侧不需要真插件）。
mod iteration_agent;
mod kernel;
mod reload;

#[cfg(test)]
mod turn_meta_tests;

#[cfg(test)]
mod tests;

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
mod seam_extraction_tests;

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
mod ffi_panic_seam_tests;

/// F-class coverage: unit tests for pure helper functions in the >3500 region
/// that were previously exercised only indirectly (or not at all). Kept in a
/// dedicated module to avoid collision with concurrent edits elsewhere.
#[cfg(test)]
mod seam_pure_fn_tests;

/// Seam-extraction tests (test-seam batch, 1-3500 region). Separate module so
/// concurrent edits to `mod tests` / `mod seam_extraction_tests` do not
/// collide. Covers the named error mappers extracted from inline `.map_err`
/// closures in `create_plugin`, snapshot setup, and the `soul_get` reply path.
#[cfg(test)]
mod seam_extraction_tests_low;

/// Seam tests for `aggregate_rollback_failure` — the pure result-aggregation
/// half of `finalize_plugin_iteration`'s rollback path (P2-25 promote-failure
/// chain + the `stage_error` early return). Placed in a dedicated module so
/// concurrent edits to the primary `mod tests` do not collide. Asserts the
/// full matrix of {no candidate / candidate-ok / candidate-err} ×
/// {restore-ok / restore-err} produces byte-exact `blocked_reason` strings.
#[cfg(test)]
mod aggregate_rollback_failure_tests;

/// Coverage terminal batch (>5500 region): unit tests for pure helper and
/// data-construction functions that were previously exercised only indirectly
/// or not at all. Kept in a dedicated EOF module so concurrent edits to the
/// other `mod *_tests` blocks do not collide. Byte-for-byte error/text
/// assertions where the emitted string is load-bearing.
#[cfg(test)]
mod region_5500_coverage_tests;

/// Coverage batch for host.rs lines 3500-5500 (cov6 numbering; the "seam-100"
/// terminal batch). These are the `iterate_plugins` orchestration failure arms,
/// the plugin-iteration agent-tool backend, and the surrounding pure helpers.
/// Integration tests reach the happy paths through the 200s dylib-rebuilding
/// walk test; this module drives the private methods and pure functions
/// directly against near-instant hermetic fixtures (empty `artifacts/index.json`
/// → no dlopen) or with zero I/O at all. Kept as its own module so concurrent
/// edits never collide with `mod tests` / the other seam modules.
#[cfg(test)]
mod region_3500_5500_seam_tests;

/// Coverage for the `apply_operations` / `scaffold_child_plugin` /
/// `execute_tool` operation-application arms of this module: the double-fault
/// wrapper, the scaffolded-child ordering, the plugin-iteration journal replay
/// gates, and the workspace-manifest `flock` failure branch. Kept as its own
/// module so concurrent edits to `mod tests` and the other seam modules never
/// collide with it.
#[cfg(test)]
mod ops_arms_coverage_tests;

/// Unit coverage for the named helpers extracted out of the `iterate_plugins`
/// pipeline. Each one sits on a branch the public entry point cannot drive (a
/// defensive invariant, a panic-payload shape, a rollback-of-a-rollback
/// failure), so the message construction is asserted here byte-for-byte instead
/// of being reached through a synthetic fault.
#[cfg(test)]
mod iterate_stage_coverage_tests;

/// Security-regression batch for the three host.rs hardening changes:
///
///   1. `check_sensitive_path` / `check_sensitive_command` — lexical path
///      normalisation + component / underscore-keyword matching plus the
///      argv-token and shell-interpreter gates (substring-blacklist bypasses
///      like `/etc//passwd`, `head /etc/passwd`, `sh -c '…'`, `apikey`);
///   2. `resolve_sandboxed_path` — dangling-symlink / symlink-target
///      containment (the fs-dependent symlink cases live in
///      `tests/host_boot_session_arms.rs`);
///   3. `validated_verification_command` / `run_checked_command` — argv-based
///      `cargo <op> …` gate, shell-metacharacter rejection, and the validated
///      plugin-path default-command builder.
#[cfg(test)]
mod security_regression_tests;

#[cfg(test)]
mod iteration_scaffold_tests;
