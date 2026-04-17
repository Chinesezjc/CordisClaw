use crate::agent::{
    AgentBackend, AgentReply, AgentSession, AgentSessionStatus, AgentToolExecutionSummary,
    AgentToolSpec, AgentTranscriptEntry,
};
use crate::config::{LlmApiConfig, PluginConfigFile, RuntimeConfig};
use crate::context::RuntimeContext;
use crate::core::error::RuntimeError;
use crate::core::models::{NodeOutcome, PluginLoadResult, PluginUnavailableReason};
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
use crate::kernel::evaluator::{EvalHarness, VerificationInput};
use crate::kernel::memory::{ChangeMemory, ChangeRecord};
use crate::kernel::planner::{
    validate_reserved_child_keyword_identifiers, LlmPatchPlanner, PlanRequest, PlannedUpdate,
};
use crate::kernel::plugin_iteration::{
    file_sha256, normalize_rel_path, now_ms, CanaryReport, CanaryVerdict, KernelPluginIssue,
    KernelPluginIssueSource, KernelPluginIssueStatus, KernelPluginIterationRequest,
    PluginEditExecutor, PluginEditOpKind, PluginEditOperation, PluginEditPlan, PluginEditRollback,
    PluginIterationFinalVerdict, PluginIterationHistoryEntry, PluginIterationNetSpec,
    PluginIterationPolicy, PluginIterationStatus, VerifierVerdict,
};
use crate::kernel::policy::IterationPolicy;
use crate::kernel::r#loop::SelfIterationKernel;
use crate::kernel::verifier::{
    CommandVerifier, VerificationPlan, VerificationProfile, VerificationReport,
};
use crate::plugin::abi::PluginResponse;
use crate::plugin::invoke::invoke_registered_plugin;
use crate::plugin::loader::{default_loader_config, LoadOutput, Loader};
use crate::plugin::registry::{NodeRegistry, PluginRegistry, RegisteredPlugin};
use crate::plugin::tooling::rebuild_plugin_workspace;
use crate::service::doc_registry::DocRegistry;
use crate::service::graph_registry::{GraphRegistry, RegisteredNet, RegisteredNetEdgeKind};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
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
        let mut traces = BTreeMap::<String, ExecutionInvocationTrace>::new();
        let output = execute_net(
            ExecutionConfig {
                scheduler: SchedulerConfig { max_parallelism: 1 },
                ..ExecutionConfig::default()
            },
            net,
            &mut context,
            |spec, attempt, trigger, _| {
                let transition_id = &spec.transition.transition_id;
                let Some(node) = self.node_registry.get(transition_id) else {
                    traces.insert(
                        transition_id.clone(),
                        ExecutionInvocationTrace {
                            node_fqn: transition_id.clone(),
                            plugin_path: String::new(),
                            node_id: String::new(),
                            attempt,
                            outcome: Some(NodeOutcome::Failure),
                            request_payload: None,
                            response_payload: None,
                            error: Some("node missing from registry".to_string()),
                        },
                    );
                    return TransitionRunResult::from_outcome(NodeOutcome::Failure);
                };

                let request_payload = build_execution_payload(&request_seed, &trigger.inputs);
                let request_text =
                    match serde_json::to_string(&Value::Object(request_payload.clone())) {
                        Ok(payload) => payload,
                        Err(err) => {
                            traces.insert(
                                transition_id.clone(),
                                ExecutionInvocationTrace {
                                    node_fqn: transition_id.clone(),
                                    plugin_path: node.plugin_path.clone(),
                                    node_id: node.node_id.clone(),
                                    attempt,
                                    outcome: Some(NodeOutcome::Failure),
                                    request_payload: Some(Value::Object(request_payload)),
                                    response_payload: None,
                                    error: Some(format!("request serialize failed: {err}")),
                                },
                            );
                            return TransitionRunResult::from_outcome(NodeOutcome::Failure);
                        }
                    };

                match self.invoke(&node.plugin_path, &node.node_id, request_text) {
                    Ok(response) => {
                        let response_payload = parse_response_payload(&response.payload);
                        let outcome = infer_outcome_from_payload(&response_payload);
                        traces.insert(
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
                        traces.insert(
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
        )?;

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KernelPlanRequest {
    pub issue_id: Option<String>,
    pub patch_id: Option<String>,
    pub instruction: String,
    pub paths: Vec<String>,
    pub manual_approved: bool,
    pub tests_command: Option<String>,
    pub safety_command: Option<String>,
    pub verify_profile: Option<VerificationProfile>,
    pub quality_score: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KernelPlanResult {
    pub plan: AutoUpdatePlan,
    pub summary: String,
    pub verification_plan: VerificationPlan,
    pub tests_command: Option<String>,
    pub safety_command: Option<String>,
    pub planner_model: String,
    pub response_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KernelPlanApplyResult {
    pub planned: KernelPlanResult,
    pub verification: VerificationReport,
    pub result: AutoUpdateResult,
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
    updater: AutoUpdater,
    inner: Mutex<SelfIterationKernel>,
}

impl RuntimeKernel {
    pub fn new(workspace_root: impl Into<PathBuf>, config: &RuntimeConfig) -> Self {
        let workspace_root = workspace_root.into();
        let mut policy = IterationPolicy::default();
        policy.path_allowlist = vec!["".to_string()];
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
            updater: AutoUpdater::new(&workspace_root),
            workspace_root,
            inner: Mutex::new(SelfIterationKernel::new(
                policy,
                EvalHarness {
                    min_quality_score: config.kernel.min_quality_score,
                },
                ChangeMemory::with_limit(config.kernel.change_history_limit),
            )),
        }
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn status(&self) -> KernelStatus {
        let kernel = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let metrics = kernel.metrics();
        let memory = kernel.memory();
        let plugin_issues = self
            .plugin_issues
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let blocked_iterations = self
            .blocked_iterations
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let plugin_history = self
            .plugin_history
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let last_plugin_iteration = self
            .last_plugin_iteration
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        KernelStatus {
            workspace_root: self.workspace_root.display().to_string(),
            config_dir: self.config_dir.display().to_string(),
            llm_provider: self.llm_api.provider.clone(),
            llm_model: self.llm_api.model.clone(),
            plugin_config_count: self.plugin_configs.len(),
            iteration_total: metrics.iteration_total,
            iteration_promote_total: metrics.iteration_promote_total,
            iteration_rollback_total: metrics.iteration_rollback_total,
            history_len: memory.len(),
            last_change: memory.recent(1).into_iter().next(),
            plugin_issue_count: plugin_issues.len(),
            blocked_iteration_count: blocked_iterations.len(),
            plugin_iteration_total: plugin_history.len(),
            last_plugin_iteration: last_plugin_iteration
                .as_ref()
                .map(plugin_iteration_status_from_result),
        }
    }

    pub fn history(&self) -> Vec<ChangeRecord> {
        let kernel = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        kernel.memory().recent(kernel.memory().len())
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
        if result.final_verdict != PluginIterationFinalVerdict::Blocked {
            return Err(RuntimeError::InvalidArgument {
                message: format!("iteration {iteration_id} is not blocked"),
            });
        }
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
        let root_plugin_path = if let Some(issue) = &selected_issue {
            issue.root_plugin_path.clone()
        } else {
            determine_root_plugin_path(snapshot, &request.target_plugin_paths)?
        };
        let target_plugin_paths = snapshot
            .plugin_registry()
            .iter()
            .map(|(plugin_path, _)| plugin_path)
            .filter(|plugin_path| {
                plugin_path == &root_plugin_path
                    || plugin_path.starts_with(&format!("{root_plugin_path}/"))
            })
            .collect::<Vec<_>>();
        if target_plugin_paths.is_empty() {
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
        let allowed_plugin_roots = target_plugin_paths
            .iter()
            .map(|plugin_path| (plugin_path.clone(), format!("plugins/{plugin_path}")))
            .collect::<BTreeMap<_, _>>();

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
        }
        *self
            .last_plugin_iteration
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(result.clone());
        match result.final_verdict {
            PluginIterationFinalVerdict::Blocked => {
                self.blocked_iterations
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .insert(result.iteration_id.clone(), result.clone());
                self.update_issue_status(&result.issue_id, KernelPluginIssueStatus::Blocked);
            }
            PluginIterationFinalVerdict::Promoted => {
                self.blocked_iterations
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .remove(&result.iteration_id);
                self.update_issue_status(&result.issue_id, KernelPluginIssueStatus::Resolved);
            }
            PluginIterationFinalVerdict::RolledBack => {
                self.blocked_iterations
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .remove(&result.iteration_id);
                self.update_issue_status(&result.issue_id, KernelPluginIssueStatus::Open);
            }
        }
    }

    pub fn run_iteration(
        &self,
        plan: AutoUpdatePlan,
        verification: VerificationInput,
    ) -> Result<AutoUpdateResult, RuntimeError> {
        let mut kernel = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        self.updater.execute(&mut kernel, plan, |_| {
            Ok(VerificationEnvelope::from(verification))
        })
    }

    pub fn plan_update(
        &self,
        request: KernelPlanRequest,
    ) -> Result<KernelPlanResult, RuntimeError> {
        let verify_profile = request.verify_profile.unwrap_or_default();
        let user_tests_command = request.tests_command.clone();
        let user_safety_command = request.safety_command.clone();
        let planner = LlmPatchPlanner::new(self.llm_api.clone())?;
        let planned = planner.plan(
            &self.workspace_root,
            PlanRequest {
                issue_id: normalize_request_id(request.issue_id, "llm-issue"),
                patch_id: normalize_request_id(request.patch_id, "llm-patch"),
                instruction: request.instruction,
                paths: request.paths,
                manual_approved: request.manual_approved,
            },
        )?;
        let tests_command = user_tests_command.or_else(|| planned.tests_command.clone());
        let safety_command = user_safety_command.or_else(|| planned.safety_command.clone());
        let verification_plan = CommandVerifier::resolve_plan(
            &self.workspace_root,
            verify_profile,
            tests_command.as_deref(),
            safety_command.as_deref(),
        );
        Ok(kernel_plan_result(planned, verification_plan))
    }

    pub fn plan_and_run_iteration(
        &self,
        request: KernelPlanRequest,
    ) -> Result<KernelPlanApplyResult, RuntimeError> {
        let planned = self.plan_update(request.clone())?;
        let quality_score = request.quality_score;
        let mut verification_report = None;
        let mut kernel = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let result = self
            .updater
            .execute(&mut kernel, planned.plan.clone(), |workspace_root| {
                let report = CommandVerifier::verify(
                    workspace_root,
                    planned.verification_plan.profile,
                    planned.verification_plan.tests_command.as_deref(),
                    planned.verification_plan.safety_command.as_deref(),
                    quality_score,
                )?;
                let envelope = VerificationEnvelope {
                    input: report.input.clone(),
                    verification_profile: Some(report.plan.profile.as_str().to_string()),
                };
                verification_report = Some(report);
                Ok(envelope)
            })?;
        let verification = verification_report.ok_or_else(|| RuntimeError::Invariant {
            message: "verification report missing after updater execution".to_string(),
        })?;
        Ok(KernelPlanApplyResult {
            planned,
            verification,
            result,
        })
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentSessionHandle {
    pub session_id: String,
    pub kind: AgentSessionKind,
}

#[derive(Debug)]
struct ManagedAgentSession {
    #[allow(dead_code)]
    handle: AgentSessionHandle,
    session: AgentSession,
    state: ManagedAgentState,
}

#[derive(Debug)]
enum ManagedAgentState {
    RuntimeShell,
    PluginIteration(PluginIterationAgentState),
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
    context_paths: Vec<String>,
    recorded_summary: Option<String>,
    tests_command: Option<String>,
    safety_command: Option<String>,
    verification_attempts: usize,
    verification_successes: usize,
    rollback: PluginEditRollback,
    operations: Vec<PluginEditOperation>,
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
        context_paths: Vec<String>,
        workspace_root: &Path,
    ) -> Self {
        Self {
            prepared,
            context_paths,
            recorded_summary: None,
            tests_command: None,
            safety_command: None,
            verification_attempts: 0,
            verification_successes: 0,
            rollback: PluginEditRollback::empty(workspace_root),
            operations: Vec::new(),
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

impl RuntimeHost {
    pub fn boot(fixtures_root: impl AsRef<Path>) -> Result<Self, RuntimeError> {
        let fixtures_root = fixtures_root.as_ref().to_path_buf();
        let config = RuntimeConfig::load(&fixtures_root)?;
        let loader = Loader::new(default_loader_config(&fixtures_root));
        let snapshot_root = config
            .resolve_snapshot_root(&fixtures_root)
            .unwrap_or_else(|| default_snapshot_root(&fixtures_root));
        fs::create_dir_all(&snapshot_root).map_err(|e| RuntimeError::Io {
            path: snapshot_root.clone(),
            message: e.to_string(),
        })?;
        recover_plugin_iteration_workspace(&fixtures_root, &snapshot_root)?;

        let initial_snapshot = Arc::new(build_snapshot(&loader, &snapshot_root)?);
        Ok(Self {
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
        })
    }

    pub fn fixtures_root(&self) -> &Path {
        &self.fixtures_root
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

    pub fn agent_start(&self, kind: AgentSessionKind) -> Result<AgentSessionHandle, RuntimeError> {
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
        let session = AgentSession::new(self.config.llm_api.clone(), session_kind_label)?;
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
        self.agent_sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(session_id.to_string(), session);
        result
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
        context_paths: Vec<String>,
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
                    state: ManagedAgentState::PluginIteration(PluginIterationAgentState::new(
                        prepared,
                        context_paths,
                        &self.fixtures_root,
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
        self.try_auto_iterate_observed_plugins();
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
        self.try_auto_iterate_observed_plugins();
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

    pub fn reload(&self) -> Result<ReloadReport, RuntimeError> {
        match self.reload_internal() {
            Ok((report, attempt)) => {
                self.record_reload_attempt(attempt);
                let snapshot = self.current_snapshot();
                self.observe_snapshot_plugin_issues(snapshot.as_ref(), &report, "reload");
                self.try_auto_iterate_observed_plugins();
                Ok(report)
            }
            Err((err, attempt)) => {
                self.record_reload_attempt(attempt);
                self.observe_reload_error("reload", &err);
                self.try_auto_iterate_observed_plugins();
                Err(err)
            }
        }
    }

    pub fn reload_with_diagnostics(&self) -> ReloadAttemptReport {
        match self.reload_internal() {
            Ok((report, attempt)) => {
                self.record_reload_attempt(attempt.clone());
                let snapshot = self.current_snapshot();
                self.observe_snapshot_plugin_issues(snapshot.as_ref(), &report, "reload");
                self.try_auto_iterate_observed_plugins();
                attempt
            }
            Err((err, attempt)) => {
                self.record_reload_attempt(attempt.clone());
                self.observe_reload_error("reload", &err);
                self.try_auto_iterate_observed_plugins();
                attempt
            }
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
                self.try_auto_iterate_observed_plugins();
                Ok(status)
            }
            Err((err, attempt)) => {
                self.record_candidate_reload_attempt(attempt);
                self.observe_reload_error("candidate_reload", &err);
                self.try_auto_iterate_observed_plugins();
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
                self.try_auto_iterate_observed_plugins();
                attempt
            }
            Err((err, attempt)) => {
                self.record_candidate_reload_attempt(attempt.clone());
                self.observe_reload_error("candidate_reload", &err);
                self.try_auto_iterate_observed_plugins();
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
        let net = build_plugin_iteration_execution_net();
        let mut context = snapshot.context_baseline().clone();
        let mut state = PluginIterationRunState::new(prepared.clone());
        let result = execute_net(
            ExecutionConfig {
                scheduler: SchedulerConfig { max_parallelism: 1 },
                ..ExecutionConfig::default()
            },
            net,
            &mut context,
            |spec, _, _, _| {
                let transition_id = spec.transition.transition_id.as_str();
                if state.stage_error.is_some() && transition_id != "promote_or_rollback" {
                    return TransitionRunResult {
                        outcome: NodeOutcome::Skipped,
                        payload: json!({ "skipped_after_failure": true }),
                    };
                }
                let stage_result: Result<Value, RuntimeError> = match transition_id {
                    "observe" => Ok(json!({
                        "issue_id": state.prepared.issue_id,
                        "root_plugin_path": state.prepared.root_plugin_path,
                    })),
                    "select_issue" => Ok(json!({
                        "target_plugin_paths": state.prepared.target_plugin_paths,
                    })),
                    "plan" => (|| {
                        let agent = self.run_plugin_iteration_agent(&state.prepared)?;
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
                            return Err(RuntimeError::LlmResponseInvalid {
                                message: format!(
                                    "plugin iteration agent session {} exited without calling record_iteration_summary",
                                    state
                                        .agent_session_id
                                        .as_deref()
                                        .unwrap_or("unknown-session")
                                ),
                            });
                        }
                        Ok(json!({
                            "agent_session_id": state.agent_session_id,
                            "planned_operation_count": state
                                .derived_edit_plan
                                .as_ref()
                                .map(|plan| plan.operations.len())
                                .unwrap_or(0),
                            "tool_execution_summary": state.tool_execution_summary,
                        }))
                    })(),
                    "edit" => (|| {
                        let rollback = state.rollback.as_ref().ok_or_else(|| RuntimeError::Invariant {
                            message: "plugin iteration rollback journal missing after agent execution".to_string(),
                        })?;
                        rollback.persist_journal(
                            &plugin_iteration_journal_path(&self.snapshot_root),
                            &state.prepared.iteration_id,
                        )?;
                        Ok(json!({
                            "changed_paths": state.changed_paths,
                            "diff_lines": state.diff_lines,
                        }))
                    })(),
                    "rebuild" => (|| {
                        let rebuilt = rebuild_plugin_workspace(&self.fixtures_root)?;
                        state.rebuilt_artifacts = rebuilt.clone();
                        Ok(json!({
                            "rebuilt_count": rebuilt.len(),
                        }))
                    })(),
                    "stage_candidate" => (|| {
                        let candidate = self.reload_candidate()?;
                        state.candidate = Some(candidate.clone());
                        Ok(json!({
                            "candidate_snapshot_id": candidate.candidate_snapshot_id,
                        }))
                    })(),
                    "verify" => (|| {
                        let report = self.verify_plugin_iteration(&state)?;
                        let verdict = if report.input.tests_passed && report.input.safety_checks_passed {
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
                        state.verification = Some(report.clone());
                        state.verifier_verdict = Some(verdict);
                        Ok(json!({
                            "verifier_verdict": verdict,
                        }))
                    })(),
                    "canary" => (|| {
                        let report = self.run_plugin_canary(&state)?;
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
                        state.canary = Some(report.clone());
                        Ok(json!({
                            "canary_verdict": report.verdict,
                            "mode": report.mode,
                        }))
                    })(),
                    "promote_or_rollback" => (|| {
                        let final_verdict = self.finalize_plugin_iteration(&mut state)?;
                        Ok(json!({
                            "final_verdict": final_verdict,
                        }))
                    })(),
                    other => Err(RuntimeError::Invariant {
                        message: format!("unknown plugin iteration stage: {other}"),
                    }),
                };
                match stage_result {
                    Ok(payload) => TransitionRunResult {
                        outcome: NodeOutcome::Success,
                        payload,
                    },
                    Err(err) => {
                        self.observe_plugin_iteration_failure(
                            &state.prepared,
                            transition_id,
                            &err,
                        );
                        state.stage_error = Some(err.to_string());
                        TransitionRunResult {
                            outcome: NodeOutcome::Failure,
                            payload: json!({ "error": err.to_string() }),
                        }
                    }
                }
            },
        )
        .and_then(|output| state.into_result(output));
        self.kernel.finish_plugin_iteration(&iteration_id);
        match result {
            Ok(result) => {
                self.kernel.record_plugin_iteration_outcome(&result);
                self.cleanup_retired_snapshots();
                Ok(result)
            }
            Err(err) => {
                self.cleanup_retired_snapshots();
                Err(err)
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
                let (_, op_rollback) = executor.execute(
                    &self.kernel.plugin_iteration_policy,
                    &prepared.allowed_plugin_roots,
                    &single,
                )?;
                rollback.absorb(op_rollback)?;
            }
            return Ok(PluginIterationAgentRun {
                session_id: None,
                tool_summary: None,
                transcript_excerpt: Vec::new(),
                snapshot: PluginIterationAgentSnapshot {
                    recorded_summary: Some(plan.summary.clone()),
                    tests_command: prepared.tests_command.clone(),
                    safety_command: prepared.safety_command.clone(),
                    changed_paths: plan.changed_paths(),
                    rollback,
                    derived_edit_plan: plan.clone(),
                },
            });
        }
        let context_paths = collect_plugin_context_paths(
            &self.fixtures_root,
            prepared.allowed_plugin_roots.values(),
        )?;
        let session_id =
            self.start_plugin_iteration_agent_session(prepared.clone(), context_paths)?;
        let input = prepared
            .instruction
            .clone()
            .unwrap_or_else(|| prepared.summary.clone());
        let _reply = self.agent_send(&session_id, &input)?;
        let snapshot = self.plugin_iteration_agent_snapshot(&session_id)?;
        let transcript = self.agent_transcript(&session_id)?;
        let transcript_excerpt = transcript.into_iter().rev().take(12).collect::<Vec<_>>();
        let transcript_excerpt = transcript_excerpt.into_iter().rev().collect::<Vec<_>>();
        let tool_summary = self.agent_status(&session_id).ok().and_then(|_| {
            self.agent_sessions
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .get(&session_id)
                .map(|managed| managed.session.tool_execution_summary())
        });
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
        let report = CommandVerifier::verify(
            &self.fixtures_root,
            state.prepared.verify_profile,
            tests_command.as_deref(),
            safety_command.as_deref(),
            state.prepared.quality_score,
        )?;
        Ok(report)
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
            let response = self.invoke_candidate(
                &sample.plugin_path,
                &sample.node_id,
                serde_json::to_string(&sample.payload).map_err(|err| RuntimeError::Invariant {
                    message: format!("canary payload serialize failed: {err}"),
                })?,
            )?;
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

        if let Some(candidate) = self.candidate_snapshot() {
            for plugin_path in &state.prepared.target_plugin_paths {
                let Some(plugin) = candidate.plugin_registry().get(plugin_path) else {
                    continue;
                };
                let Some(docs) = plugin.docs else {
                    continue;
                };
                if let Some(node) = docs
                    .nodes
                    .iter()
                    .find(|node| node.id.contains("canary") || node.id.contains("verify"))
                {
                    let response =
                        self.invoke_candidate(plugin_path, &node.id, "{}".to_string())?;
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

    fn finalize_plugin_iteration(
        &self,
        state: &mut PluginIterationRunState,
    ) -> Result<PluginIterationFinalVerdict, RuntimeError> {
        if let Some(stage_error) = state.stage_error.clone() {
            if self.candidate_snapshot().is_some() {
                let _ = self.rollback_candidate();
            }
            let _ = restore_plugin_iteration_workspace(
                &self.fixtures_root,
                &self.snapshot_root,
                state.rollback.as_ref(),
            );
            state.blocked_reason = Some(stage_error);
            state.final_verdict = Some(PluginIterationFinalVerdict::RolledBack);
            return Ok(PluginIterationFinalVerdict::RolledBack);
        }
        let verifier_verdict = state.verifier_verdict.unwrap_or(VerifierVerdict::Partial);
        let canary_verdict = state
            .canary
            .as_ref()
            .map(|report| report.verdict)
            .unwrap_or(CanaryVerdict::Partial);
        let final_verdict =
            if verifier_verdict == VerifierVerdict::Pass && canary_verdict == CanaryVerdict::Pass {
                self.promote_candidate()?;
                PluginIterationFinalVerdict::Promoted
            } else if canary_verdict == CanaryVerdict::Partial {
                state.blocked_reason = Some(
                    state
                        .canary
                        .as_ref()
                        .map(|report| report.message.clone())
                        .unwrap_or_else(|| "canary returned partial".to_string()),
                );
                PluginIterationFinalVerdict::Blocked
            } else {
                if self.candidate_snapshot().is_some() {
                    let _ = self.rollback_candidate();
                }
                restore_plugin_iteration_workspace(
                    &self.fixtures_root,
                    &self.snapshot_root,
                    state.rollback.as_ref(),
                )?;
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

    fn observe_plugin_iteration_failure(
        &self,
        prepared: &PreparedPluginIteration,
        stage: &str,
        err: &RuntimeError,
    ) {
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

    fn try_auto_iterate_observed_plugins(&self) {
        if !self.kernel.can_auto_iterate_plugins() {
            return;
        }
        if self
            .kernel
            .active_plugin_iteration
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .is_some()
        {
            return;
        }
        let Some(issue) = self
            .kernel
            .plugin_issues()
            .into_iter()
            .find(|issue| issue.status == KernelPluginIssueStatus::Open)
        else {
            return;
        };
        let _ = self.iterate_plugins(KernelPluginIterationRequest {
            issue_id: Some(issue.issue_id.clone()),
            target_plugin_paths: Vec::new(),
            instruction: Some(issue.summary.clone()),
            edit_plan: None,
            manual_approved: false,
            tests_command: None,
            safety_command: None,
            verify_profile: Some(VerificationProfile::RustWorkspace),
            quality_score: None,
        });
    }

    fn reload_internal(
        &self,
    ) -> Result<(ReloadReport, ReloadAttemptReport), (RuntimeError, ReloadAttemptReport)> {
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
                    return Err((err, attempt));
                }
            };

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
    }

    fn reload_candidate_internal(
        &self,
    ) -> Result<(CandidateSnapshotStatus, ReloadAttemptReport), (RuntimeError, ReloadAttemptReport)>
    {
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
                    return Err((err, attempt));
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
const PLUGIN_AGENT_TOOL_READ_CONTEXT_FILE: &str = "read_context_file";
const PLUGIN_AGENT_TOOL_READ_CONTEXT_FILES: &str = "read_context_files";
const PLUGIN_AGENT_TOOL_INSPECT_PLUGIN_CATALOG: &str = "inspect_plugin_catalog";
const PLUGIN_AGENT_TOOL_SCAFFOLD_CHILD_PLUGIN: &str = "scaffold_child_plugin";
const PLUGIN_AGENT_TOOL_REPLACE_FILE_EXACT: &str = "replace_file_exact";
const PLUGIN_AGENT_TOOL_CREATE_FILE: &str = "create_file";
const PLUGIN_AGENT_TOOL_DELETE_FILE: &str = "delete_file";
const PLUGIN_AGENT_TOOL_TOML_SET: &str = "toml_set";
const PLUGIN_AGENT_TOOL_JSON_SET: &str = "json_set";
const PLUGIN_AGENT_TOOL_RUN_PLUGIN_CHECK: &str = "run_plugin_check";
const PLUGIN_AGENT_TOOL_RUN_PLUGIN_TEST: &str = "run_plugin_test";
const PLUGIN_AGENT_TOOL_REBUILD_PLUGIN_WORKSPACE: &str = "rebuild_plugin_workspace";
const PLUGIN_AGENT_TOOL_RECORD_ITERATION_SUMMARY: &str = "record_iteration_summary";
const PLUGIN_ITERATION_AGENT_TIMEOUT_CAP_MS: u64 = 120_000;

#[derive(Debug, Clone, Deserialize)]
struct ReadContextFileArgs {
    path: String,
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
    fn respond(&mut self, host: &RuntimeHost, input: &str) -> Result<AgentReply, RuntimeError> {
        match &mut self.state {
            ManagedAgentState::RuntimeShell => self.session.respond_with_runtime_host(host, input),
            ManagedAgentState::PluginIteration(state) => {
                let mut backend = PluginIterationAgentBackend { host, state };
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
                    let _ = local_rollback.rollback();
                    return Err(err);
                }
            }
        }
        self.state.rollback.absorb(local_rollback)?;
        self.state.operations.extend(operations.clone());
        for path in operations.into_iter().map(|operation| operation.path) {
            if should_track_context_file(&path) {
                self.state.context_paths.push(path);
            }
        }
        self.state.context_paths.sort();
        self.state.context_paths.dedup();
        let derived = self.state.snapshot().derived_edit_plan;
        Ok(json!({
            "changed_paths": derived.changed_paths(),
            "operation_count": derived.operations.len(),
        }))
    }

    fn list_context_files(&self) -> Value {
        let mut paths = self.state.context_paths.clone();
        sort_plugin_context_paths(&mut paths);
        json!({
            "root_plugin_path": self.state.prepared.root_plugin_path,
            "phase": self.phase(),
            "paths": paths,
        })
    }

    fn read_context_file(&self, path: &str) -> Result<Value, RuntimeError> {
        let normalized = normalize_rel_path(path)?;
        if !self
            .state
            .context_paths
            .iter()
            .any(|item| item == &normalized)
        {
            return Err(RuntimeError::InvalidArgument {
                message: format!(
                    "context file is not available in this plugin iteration session: {normalized}"
                ),
            });
        }
        let abs_path = self.host.fixtures_root.join(&normalized);
        let content = fs::read_to_string(&abs_path).map_err(|err| RuntimeError::Io {
            path: abs_path.clone(),
            message: err.to_string(),
        })?;
        Ok(json!({
            "path": normalized,
            "sha256": sha256_text(&content),
            "content": content,
        }))
    }

    fn read_context_files(&self, paths: &[String]) -> Result<Value, RuntimeError> {
        let files = paths
            .iter()
            .map(|path| self.read_context_file(path))
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
        let crate_name = child_plugin_path.replace('/', "_").replace('-', "_");
        let summary = args
            .summary
            .clone()
            .unwrap_or_else(|| format!("Child plugin scaffold for {child_plugin_path}"));

        let parent_manifest_rel = format!("plugins/{}/Cargo.toml", args.parent_plugin_path);
        let parent_manifest_abs = self.host.fixtures_root.join(&parent_manifest_rel);
        let parent_manifest_text =
            fs::read_to_string(&parent_manifest_abs).map_err(|err| RuntimeError::Io {
                path: parent_manifest_abs.clone(),
                message: err.to_string(),
            })?;
        let parent_manifest_sha = file_sha256(&parent_manifest_abs)?;
        let parent_toml: TomlValue =
            toml::from_str(&parent_manifest_text).map_err(|err| RuntimeError::CargoParse {
                path: parent_manifest_abs.clone(),
                message: err.to_string(),
            })?;
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
            .map_err(|err| RuntimeError::CommandFailed {
                program: "bash".to_string(),
                args: vec!["-lc".to_string(), command.clone()],
                message: err.to_string(),
            })?;
        if output.status.success() {
            self.state.verification_successes += 1;
        }
        Ok(json!({
            "stage": stage,
            "command": command,
            "success": output.status.success(),
            "exit_code": output.status.code(),
            "stdout": String::from_utf8_lossy(&output.stdout).to_string(),
            "stderr": String::from_utf8_lossy(&output.stderr).to_string(),
        }))
    }
}

impl<'a> AgentBackend for PluginIterationAgentBackend<'a> {
    fn system_prompt(&self) -> String {
        format!(
            "You are the Cordis plugin-iteration agent.\n\
Work directly through tools instead of proposing a large final JSON plan.\n\
You may only modify the selected plugin subtree rooted at {}.\n\
Use read and catalog tools to understand the existing plugin family, then use scaffold and file-edit tools to make incremental changes.\n\
Use run_plugin_check, run_plugin_test, and rebuild_plugin_workspace to validate real changes.\n\
If a child plugin path uses a Rust keyword such as `mod`, keep that keyword in filesystem and `plugin_path` positions like `expr/evaluator/mod`, but never use raw `mod` as a Rust source identifier.\n\
Replace placeholder scaffold implementations, tests, and docs together once the behavior is real.\n\
When the iteration is ready, call record_iteration_summary with a concise summary and any recommended verification commands. record_iteration_summary must be your last tool call and it ends the session immediately.\n\
Do not attempt to modify runtime crates, repository root manifests, config, .git, target, or generated docs under docs/agent.",
            self.state.prepared.root_plugin_path
        )
    }

    fn tool_specs(&self) -> Vec<AgentToolSpec> {
        let mut tools = vec![
            AgentToolSpec {
                name: PLUGIN_AGENT_TOOL_LIST_CONTEXT_FILES,
                description: "List readable context files for the selected plugin subtree.",
                parameters: json!({"type":"object","properties":{},"additionalProperties":false}),
            },
            AgentToolSpec {
                name: PLUGIN_AGENT_TOOL_READ_CONTEXT_FILE,
                description: "Read one context file and return its content and sha256.",
                parameters: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}),
            },
            AgentToolSpec {
                name: PLUGIN_AGENT_TOOL_READ_CONTEXT_FILES,
                description: "Read multiple context files in one call.",
                parameters: json!({"type":"object","properties":{"paths":{"type":"array","items":{"type":"string"}}},"required":["paths"],"additionalProperties":false}),
            },
            AgentToolSpec {
                name: PLUGIN_AGENT_TOOL_INSPECT_PLUGIN_CATALOG,
                description: "Inspect the currently loaded plugin subtree, including child plugins and node summaries.",
                parameters: json!({"type":"object","properties":{},"additionalProperties":false}),
            },
        ];
        if self.state.recorded_summary.is_none() {
            tools.extend([
                AgentToolSpec {
                    name: PLUGIN_AGENT_TOOL_SCAFFOLD_CHILD_PLUGIN,
                    description: "Create a sibling child plugin scaffold under the selected subtree and register it in the parent manifest.",
                    parameters: json!({"type":"object","properties":{"parent_plugin_path":{"type":"string"},"child_name":{"type":"string"},"template_plugin_path":{"type":"string"},"node_id":{"type":"string"},"summary":{"type":"string"}},"required":["parent_plugin_path","child_name"],"additionalProperties":false}),
                },
                AgentToolSpec {
                    name: PLUGIN_AGENT_TOOL_REPLACE_FILE_EXACT,
                    description: "Replace an exact string in a writable file.",
                    parameters: json!({"type":"object","properties":{"path":{"type":"string"},"expected_old_string":{"type":"string"},"new_content":{"type":"string"}},"required":["path","expected_old_string","new_content"],"additionalProperties":false}),
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
            ]);
        }
        if !self.state.operations.is_empty() && self.state.recorded_summary.is_none() {
            tools.extend([
                AgentToolSpec {
                    name: PLUGIN_AGENT_TOOL_RUN_PLUGIN_CHECK,
                    description: "Run a restricted cargo check for the plugin workspace.",
                    parameters: json!({"type":"object","properties":{"command":{"type":"string"}},"additionalProperties":false}),
                },
                AgentToolSpec {
                    name: PLUGIN_AGENT_TOOL_RUN_PLUGIN_TEST,
                    description: "Run a restricted cargo test for the plugin workspace.",
                    parameters: json!({"type":"object","properties":{"command":{"type":"string"}},"additionalProperties":false}),
                },
                AgentToolSpec {
                    name: PLUGIN_AGENT_TOOL_REBUILD_PLUGIN_WORKSPACE,
                    description: "Rebuild plugin artifacts and sync generated docs for the whole plugin workspace.",
                    parameters: json!({"type":"object","properties":{},"additionalProperties":false}),
                },
            ]);
        }
        if !self.state.operations.is_empty()
            && self.state.verification_successes > 0
            && self.state.recorded_summary.is_none()
        {
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
            PLUGIN_AGENT_TOOL_LIST_CONTEXT_FILES => Ok(self.list_context_files()),
            PLUGIN_AGENT_TOOL_READ_CONTEXT_FILE => {
                let args = parse_agent_args::<ReadContextFileArgs>(arguments, name)?;
                self.read_context_file(&args.path)
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
                    vec![PluginEditOperation {
                        path: args.path,
                        kind: PluginEditOpKind::ReplaceExact,
                        expected_old_string: Some(args.expected_old_string),
                        expected_sha256: None,
                        new_content: Some(args.new_content),
                        pointer: None,
                        dotted_key: None,
                        value: None,
                    }],
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
                let command = validated_verification_command(
                    args.command,
                    Some("cargo check --quiet --manifest-path plugins/Cargo.toml".to_string()),
                    "cargo check",
                )?;
                self.run_checked_command("check", command)
            }
            PLUGIN_AGENT_TOOL_RUN_PLUGIN_TEST => {
                let args = parse_agent_args::<RunPluginCommandArgs>(arguments, name)?;
                let command = validated_verification_command(
                    args.command
                        .or_else(|| self.state.prepared.tests_command.clone()),
                    Some("cargo test --quiet --manifest-path plugins/Cargo.toml".to_string()),
                    "cargo test",
                )?;
                self.run_checked_command("test", command)
            }
            PLUGIN_AGENT_TOOL_REBUILD_PLUGIN_WORKSPACE => {
                self.state.verification_attempts += 1;
                let rebuilt = rebuild_plugin_workspace(&self.host.fixtures_root)?;
                self.state.verification_successes += 1;
                Ok(json!({
                    "rebuilt_count": rebuilt.len(),
                    "rebuilt": rebuilt,
                }))
            }
            PLUGIN_AGENT_TOOL_RECORD_ITERATION_SUMMARY => {
                if self.state.operations.is_empty() || self.state.verification_successes == 0 {
                    return Err(RuntimeError::InvalidArgument {
                        message: "record_iteration_summary requires at least one edit and one successful verification step".to_string(),
                    });
                }
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
    let command = explicit
        .or(fallback)
        .ok_or_else(|| RuntimeError::InvalidArgument {
            message: format!("missing verification command for {required_prefix}"),
        })?;
    let trimmed = command.trim();
    if !trimmed.starts_with(required_prefix) {
        return Err(RuntimeError::InvalidArgument {
            message: format!(
                "verification tool only allows commands starting with `{required_prefix}`, got `{trimmed}`"
            ),
        });
    }
    Ok(trimmed.to_string())
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

fn plugin_context_priority(path: &str) -> (u8, String) {
    if path.ends_with("docs/human/overview.md") {
        (0, path.to_string())
    } else if path.ends_with("Cargo.toml") {
        (1, path.to_string())
    } else if path.contains("/docs/human/") {
        (2, path.to_string())
    } else if path.contains("/src/") {
        (3, path.to_string())
    } else if path.contains("/tests/") {
        (4, path.to_string())
    } else if path.contains("/docs/agent/") {
        (5, path.to_string())
    } else {
        (6, path.to_string())
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
        "[package]\nname = \"{crate_name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\ncrate-type = [\"rlib\", \"dylib\"]\n\n[package.metadata.cordis]\nplugin_path = \"{plugin_path}\"\nabi_kind = \"rust\"\ndeclared_nodes = [\"{node_id}\"]\nchildren = []\n\n[package.metadata.cordis.abi_fingerprint]\nrustc_version = \"1.85.1\"\ntarget_triple = \"x86_64-unknown-linux-gnu\"\ncrate_hash = \"crate_{crate_name}_v1\"\napi_hash = \"api_v2\"\n\n[dependencies]\ncordis-plugin-sdk = {{ path = \"../../../../../crates/cordis-plugin-sdk\" }}\nserde = {{ version = \"1\", features = [\"derive\"] }}\nserde_json = \"1\"\nthiserror = \"2\"\n\n[workspace]\n",
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
        "mod core;\n\npub use core::*;\n\nuse cordis_plugin_sdk::{{\n    export_plugin_api, json_response, node_doc, plugin_docs, AbiFingerprint, PluginRequest,\n    PluginResponse,\n}};\nuse serde::{{Deserialize, Serialize}};\nuse serde_json::json;\n\n#[derive(Debug, Deserialize)]\nstruct BinaryOpRequest {{\n    lhs: f64,\n    rhs: f64,\n}}\n\n#[derive(Debug, Serialize)]\nstruct BinaryOpResponse {{\n    #[serde(skip_serializing_if = \"Option::is_none\")]\n    value: Option<f64>,\n    #[serde(skip_serializing_if = \"Option::is_none\")]\n    error: Option<String>,\n}}\n\nfn docs_value() -> cordis_plugin_sdk::PluginDocs {{\n    plugin_docs(\n        \"{crate_name}\",\n        \"{plugin_path}\",\n        \"0.1.0\",\n        None,\n        vec![node_doc(\n            \"{node_id}\",\n            \"{summary}\",\n            json!({{\"type\":\"object\",\"required\":[\"lhs\",\"rhs\"],\"properties\":{{\"lhs\":{{\"type\":\"number\"}},\"rhs\":{{\"type\":\"number\"}}}}}}),\n            json!({{\"type\":\"object\",\"properties\":{{\"value\":{{\"type\":\"number\"}},\"error\":{{\"type\":\"string\"}}}}}}),\n            &[],\n            &[\"not implemented\"],\n        )],\n    )\n}}\n\nfn abi_fingerprint_value() -> AbiFingerprint {{\n    AbiFingerprint {{\n        rustc_version: \"1.85.1\".to_string(),\n        target_triple: \"x86_64-unknown-linux-gnu\".to_string(),\n        crate_hash: \"crate_{crate_name}_v1\".to_string(),\n        api_hash: \"api_v2\".to_string(),\n    }}\n}}\n\nfn api_handle(req: PluginRequest) -> PluginResponse {{\n    let response = match serde_json::from_str::<BinaryOpRequest>(&req.payload) {{\n        Ok(request) => match apply(request.lhs, request.rhs) {{\n            Ok(value) => BinaryOpResponse {{ value: Some(value), error: None }},\n            Err(err) => BinaryOpResponse {{ value: None, error: Some(err.to_string()) }},\n        }},\n        Err(err) => BinaryOpResponse {{ value: None, error: Some(format!(\"invalid request: {{err}}\")) }},\n    }};\n    json_response(&response)\n}}\n\nexport_plugin_api! {{\n    abi_fingerprint = abi_fingerprint_value(),\n    docs = docs_value(),\n    handle = api_handle,\n}}\n",
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
        "use serde::{{Deserialize, Serialize}};\nuse thiserror::Error;\n\n#[derive(Debug, Error, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]\n#[serde(rename_all = \"snake_case\")]\npub enum {type_name}Error {{\n    #[error(\"not implemented\")]\n    NotImplemented,\n}}\n\n#[derive(Debug, Default, Clone, Copy)]\npub struct {type_name}Plugin;\n\nimpl {type_name}Plugin {{\n    pub fn apply(&self, _lhs: f64, _rhs: f64) -> Result<f64, {type_name}Error> {{\n        Err({type_name}Error::NotImplemented)\n    }}\n}}\n\npub fn apply(lhs: f64, rhs: f64) -> Result<f64, {type_name}Error> {{\n    {type_name}Plugin.apply(lhs, rhs)\n}}\n"
    )
}

fn render_child_plugin_test(crate_name: &str) -> String {
    format!(
        "use {crate_name}::apply;\n\n#[test]\nfn scaffold_exports_apply() {{\n    let _ = apply(5.0, 2.0);\n}}\n"
    )
}

fn render_child_plugin_overview(plugin_path: &str) -> String {
    format!(
        "# {}\n\nThis child plugin scaffold was created by the Cordis plugin-iteration agent. Replace the placeholder implementation in `src/core.rs`, then update the placeholder smoke test and docs once the behavior is real.\n",
        plugin_path
    )
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
                    priority: -(node.topo_level as i32),
                    join_policy: if incoming == 0 {
                        JoinPolicy::AnyOf
                    } else {
                        JoinPolicy::AllOf
                    },
                },
                run_policy: RunPolicy::default(),
                kind: if node.node_fqn == target_node_fqn {
                    ExecutionTransitionKind::Terminal
                } else {
                    ExecutionTransitionKind::Task
                },
                logical_group: Some("execute".to_string()),
            }
        })
        .collect::<Vec<_>>();

    let mut places = BTreeSet::<String>::new();
    let mut arcs = Vec::<ArcSpec>::new();

    for edge in net
        .edges
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
        });
    }

    ExecutionNetSpec {
        places: places
            .into_iter()
            .map(|place_id| PlaceSpec { place_id })
            .collect(),
        transitions,
        arcs,
    }
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
    fs::create_dir_all(&staged_artifact_root).map_err(|e| RuntimeError::Io {
        path: staged_artifact_root.clone(),
        message: e.to_string(),
    })?;

    let output = match loader.load_with_staging_root(Some(&staged_artifact_root)) {
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

    Ok(runtime_snapshot_from_output(output, staged_artifact_root))
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

fn build_plugin_iteration_execution_net() -> ExecutionNetSpec {
    let spec = PluginIterationNetSpec::default();
    let transitions = spec
        .transition_ids
        .iter()
        .enumerate()
        .map(|(idx, transition_id)| ExecutionTransitionSpec {
            transition: TransitionSpec {
                transition_id: transition_id.clone(),
                priority: -(idx as i32),
                join_policy: JoinPolicy::AnyOf,
            },
            run_policy: RunPolicy::default(),
            kind: if transition_id == "promote_or_rollback" {
                ExecutionTransitionKind::Terminal
            } else {
                ExecutionTransitionKind::Task
            },
            logical_group: Some("plugin_iteration".to_string()),
        })
        .collect::<Vec<_>>();
    let mut places = Vec::new();
    let mut arcs = Vec::new();
    for window in spec.transition_ids.windows(2) {
        let from = &window[0];
        let to = &window[1];
        let place_id = format!("plugin_iteration::{from}::{to}");
        places.push(PlaceSpec {
            place_id: place_id.clone(),
        });
        arcs.push(ArcSpec {
            arc_id: format!("plugin_iteration::{from}::out"),
            place_id: place_id.clone(),
            transition_id: from.clone(),
            direction: ArcDirection::TransitionToPlace,
            label: Some("control".to_string()),
            required: false,
        });
        arcs.push(ArcSpec {
            arc_id: format!("plugin_iteration::{to}::in"),
            place_id,
            transition_id: to.clone(),
            direction: ArcDirection::PlaceToTransition,
            label: Some("control".to_string()),
            required: false,
        });
    }
    ExecutionNetSpec {
        places,
        transitions,
        arcs,
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

fn collect_plugin_context_paths<'a>(
    workspace_root: &Path,
    allowed_plugin_roots: impl IntoIterator<Item = &'a String>,
) -> Result<Vec<String>, RuntimeError> {
    let mut files = BTreeSet::new();
    for plugin_root in allowed_plugin_roots {
        let manifest_path = format!("{plugin_root}/Cargo.toml");
        if workspace_root.join(&manifest_path).exists() {
            files.insert(manifest_path);
        }
        for subdir in ["src", "tests", "docs/agent", "docs/human"] {
            let dir = workspace_root.join(plugin_root).join(subdir);
            if !dir.exists() {
                continue;
            }
            collect_context_files_recursive(workspace_root, &dir, &mut files)?;
        }
    }
    if files.is_empty() {
        return Err(RuntimeError::InvalidArgument {
            message: "no planner context files discovered for plugin iteration".to_string(),
        });
    }
    Ok(files.into_iter().collect())
}

fn collect_context_files_recursive(
    workspace_root: &Path,
    dir: &Path,
    files: &mut BTreeSet<String>,
) -> Result<(), RuntimeError> {
    let entries = fs::read_dir(dir).map_err(|err| RuntimeError::Io {
        path: dir.to_path_buf(),
        message: err.to_string(),
    })?;
    for entry in entries {
        let entry = entry.map_err(|err| RuntimeError::Io {
            path: dir.to_path_buf(),
            message: err.to_string(),
        })?;
        let path = entry.path();
        let file_type = entry.file_type().map_err(|err| RuntimeError::Io {
            path: path.clone(),
            message: err.to_string(),
        })?;
        if file_type.is_dir() {
            collect_context_files_recursive(workspace_root, &path, files)?;
            continue;
        }
        match path.extension().and_then(|extension| extension.to_str()) {
            Some("rs") | Some("json") | Some("toml") | Some("md") => {
                let relative =
                    path.strip_prefix(workspace_root)
                        .map_err(|_| RuntimeError::Invariant {
                            message: format!(
                                "planner context path {} escaped workspace root {}",
                                path.display(),
                                workspace_root.display()
                            ),
                        })?;
                files.insert(relative.to_string_lossy().to_string());
            }
            _ => {}
        }
    }
    Ok(())
}

fn kernel_plan_result(
    planned: PlannedUpdate,
    verification_plan: VerificationPlan,
) -> KernelPlanResult {
    KernelPlanResult {
        plan: planned.plan,
        summary: planned.summary,
        tests_command: verification_plan.tests_command.clone(),
        safety_command: verification_plan.safety_command.clone(),
        verification_plan,
        planner_model: planned.planner_model,
        response_id: planned.response_id,
    }
}

fn normalize_request_id(raw: Option<String>, prefix: &str) -> String {
    match raw {
        Some(value) if !value.trim().is_empty() => value,
        _ => {
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis();
            format!("{prefix}-{now_ms}")
        }
    }
}

fn make_snapshot_dir_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    format!("snapshot-{nanos}")
}

fn next_staged_artifact_root(snapshot_root: &Path) -> PathBuf {
    snapshot_root.join(make_snapshot_dir_name())
}

fn plugin_iteration_journal_path(snapshot_root: &Path) -> PathBuf {
    snapshot_root.join("plugin-iteration-edit-journal.json")
}

fn clear_plugin_iteration_journal(snapshot_root: &Path) -> Result<(), RuntimeError> {
    crate::kernel::plugin_iteration::PluginEditRollback::clear_journal(
        &plugin_iteration_journal_path(snapshot_root),
    )
}

fn restore_plugin_iteration_workspace(
    fixtures_root: &Path,
    snapshot_root: &Path,
    in_memory_rollback: Option<&crate::kernel::plugin_iteration::PluginEditRollback>,
) -> Result<bool, RuntimeError> {
    let journal_path = plugin_iteration_journal_path(snapshot_root);
    if let Some(rollback) = crate::kernel::plugin_iteration::PluginEditRollback::load_journal(
        fixtures_root,
        &journal_path,
    )? {
        rollback.rollback()?;
        rebuild_plugin_workspace(fixtures_root)?;
        crate::kernel::plugin_iteration::PluginEditRollback::clear_journal(&journal_path)?;
        return Ok(true);
    }

    if let Some(rollback) = in_memory_rollback {
        rollback.rollback()?;
        rebuild_plugin_workspace(fixtures_root)?;
        return Ok(true);
    }

    Ok(false)
}

fn recover_plugin_iteration_workspace(
    fixtures_root: &Path,
    snapshot_root: &Path,
) -> Result<(), RuntimeError> {
    let _ = restore_plugin_iteration_workspace(fixtures_root, snapshot_root, None)?;
    Ok(())
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
    use super::{render_child_plugin_test, sanitize_child_plugin_segment};

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
}
