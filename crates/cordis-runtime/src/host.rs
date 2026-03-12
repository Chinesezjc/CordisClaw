use crate::config::{LlmApiConfig, PluginConfigFile, RuntimeConfig};
use crate::context::RuntimeContext;
use crate::core::error::RuntimeError;
use crate::core::models::{NodeOutcome, PluginLoadResult};
use crate::execution::dag::{DagInputSpec, DagNodeSpec};
use crate::execution::engine::{
    execute_graph, ExecutionConfig, ExecutionOutput, ExecutionNodeKind, ExecutionNodeSpec,
};
use crate::execution::gate::RunPolicy;
use crate::execution::scheduler::SchedulerConfig;
use crate::kernel::auto_update::{AutoUpdatePlan, AutoUpdateResult, AutoUpdater, VerificationEnvelope};
use crate::kernel::evaluator::{EvalHarness, VerificationInput};
use crate::kernel::memory::{ChangeMemory, ChangeRecord};
use crate::kernel::planner::{LlmPatchPlanner, PlanRequest, PlannedUpdate};
use crate::kernel::policy::IterationPolicy;
use crate::kernel::r#loop::SelfIterationKernel;
use crate::kernel::verifier::{
    CommandVerifier, VerificationPlan, VerificationProfile, VerificationReport,
};
use crate::plugin::abi::PluginResponse;
use crate::plugin::invoke::invoke_registered_plugin;
use crate::plugin::loader::{default_loader_config, LoadOutput, Loader};
use crate::plugin::registry::{NodeRegistry, PluginRegistry, RegisteredPlugin};
use crate::service::doc_registry::DocRegistry;
use crate::service::graph_registry::{
    GraphRegistry, RegisteredDag, RegisteredDagEdgeKind, RegisteredDagNode,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

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
        let request_seed = payload.as_object().cloned().ok_or_else(|| RuntimeError::InvalidArgument {
            message: "execute payload must be a JSON object".to_string(),
        })?;
        let target_node = self
            .node_registry
            .get(target_node_fqn)
            .ok_or_else(|| RuntimeError::InvalidArgument {
                message: format!("registered node not found: {target_node_fqn}"),
            })?;
        let registered_dag = self.graph_registry.dag();
        let selected_nodes = select_registered_dag_subgraph(registered_dag, target_node_fqn);
        let specs = build_execution_specs(registered_dag, &selected_nodes, target_node_fqn, target_node);

        let mut context = self.context_baseline.clone();
        let mut node_responses = BTreeMap::<String, Value>::new();
        let mut traces = BTreeMap::<String, ExecutionInvocationTrace>::new();
        let output = execute_graph(
            ExecutionConfig {
                scheduler: SchedulerConfig { max_parallelism: 1 },
                ..ExecutionConfig::default()
            },
            specs,
            &mut context,
            |spec, attempt, _| {
                let Some(node) = self.node_registry.get(&spec.dag.node_id) else {
                    traces.insert(
                        spec.dag.node_id.clone(),
                        ExecutionInvocationTrace {
                            node_fqn: spec.dag.node_id.clone(),
                            plugin_path: String::new(),
                            node_id: String::new(),
                            attempt,
                            outcome: Some(NodeOutcome::Failure),
                            request_payload: None,
                            response_payload: None,
                            error: Some("node missing from registry".to_string()),
                        },
                    );
                    return NodeOutcome::Failure;
                };

                let request_payload = build_execution_payload(&request_seed, &spec.dag.consumes, &node_responses);
                let request_text = match serde_json::to_string(&Value::Object(request_payload.clone())) {
                    Ok(payload) => payload,
                    Err(err) => {
                        traces.insert(
                            spec.dag.node_id.clone(),
                            ExecutionInvocationTrace {
                                node_fqn: spec.dag.node_id.clone(),
                                plugin_path: node.plugin_path.clone(),
                                node_id: node.node_id.clone(),
                                attempt,
                                outcome: Some(NodeOutcome::Failure),
                                request_payload: Some(Value::Object(request_payload)),
                                response_payload: None,
                                error: Some(format!("request serialize failed: {err}")),
                            },
                        );
                        return NodeOutcome::Failure;
                    }
                };

                match self.invoke(&node.plugin_path, &node.node_id, request_text) {
                    Ok(response) => {
                        let response_payload = parse_response_payload(&response.payload);
                        let outcome = infer_outcome_from_payload(&response_payload);
                        node_responses.insert(spec.dag.node_id.clone(), response_payload.clone());
                        traces.insert(
                            spec.dag.node_id.clone(),
                            ExecutionInvocationTrace {
                                node_fqn: spec.dag.node_id.clone(),
                                plugin_path: node.plugin_path.clone(),
                                node_id: node.node_id.clone(),
                                attempt,
                                outcome: Some(outcome),
                                request_payload: Some(Value::Object(request_payload)),
                                response_payload: Some(response_payload),
                                error: None,
                            },
                        );
                        outcome
                    }
                    Err(err) => {
                        traces.insert(
                            spec.dag.node_id.clone(),
                            ExecutionInvocationTrace {
                                node_fqn: spec.dag.node_id.clone(),
                                plugin_path: node.plugin_path.clone(),
                                node_id: node.node_id.clone(),
                                attempt,
                                outcome: Some(NodeOutcome::Failure),
                                request_payload: Some(Value::Object(request_payload)),
                                response_payload: None,
                                error: Some(err.to_string()),
                            },
                        );
                        NodeOutcome::Failure
                    }
                }
            },
        )?;

        fill_missing_execution_traces(&output, &mut traces);
        Ok(RuntimeExecutionResult {
            target_node_fqn: target_node_fqn.to_string(),
            selected_nodes: selected_nodes.into_iter().collect(),
            dag_diagnostics: registered_dag.diagnostics.clone(),
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
    pub added_plugins: Vec<String>,
    pub removed_plugins: Vec<String>,
    pub changed_plugins: Vec<String>,
    pub changed_plugin_reasons: BTreeMap<String, Vec<String>>,
    pub failure_summary: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RuntimeExecutionResult {
    pub target_node_fqn: String,
    pub selected_nodes: Vec<String>,
    pub dag_diagnostics: Vec<String>,
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
    pub last_reload: Option<ReloadAttemptReport>,
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

#[derive(Debug)]
pub struct RuntimeKernel {
    workspace_root: PathBuf,
    config_dir: PathBuf,
    llm_api: LlmApiConfig,
    plugin_configs: BTreeMap<String, PluginConfigFile>,
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
        let kernel = self.inner.lock().unwrap_or_else(|poison| poison.into_inner());
        let metrics = kernel.metrics();
        let memory = kernel.memory();
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
        }
    }

    pub fn history(&self) -> Vec<ChangeRecord> {
        let kernel = self.inner.lock().unwrap_or_else(|poison| poison.into_inner());
        kernel.memory().recent(kernel.memory().len())
    }

    pub fn run_iteration(
        &self,
        plan: AutoUpdatePlan,
        verification: VerificationInput,
    ) -> Result<AutoUpdateResult, RuntimeError> {
        let mut kernel = self.inner.lock().unwrap_or_else(|poison| poison.into_inner());
        self.updater
            .execute(&mut kernel, plan, |_| Ok(VerificationEnvelope::from(verification)))
    }

    pub fn plan_update(&self, request: KernelPlanRequest) -> Result<KernelPlanResult, RuntimeError> {
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
        let mut kernel = self.inner.lock().unwrap_or_else(|poison| poison.into_inner());
        let result = self.updater.execute(&mut kernel, planned.plan.clone(), |workspace_root| {
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
    retired_snapshots: Mutex<Vec<RetiredSnapshot>>,
    last_reload_attempt: Mutex<Option<ReloadAttemptReport>>,
    kernel: RuntimeKernel,
}

#[derive(Debug)]
struct RetiredSnapshot {
    snapshot: Weak<RuntimeSnapshot>,
    staged_artifact_root: PathBuf,
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

        let initial_snapshot = Arc::new(build_snapshot(&loader, &snapshot_root)?);
        Ok(Self {
            kernel: RuntimeKernel::new(&fixtures_root, &config),
            config,
            fixtures_root,
            loader,
            snapshot_root,
            current_snapshot: RwLock::new(initial_snapshot),
            retired_snapshots: Mutex::new(Vec::new()),
            last_reload_attempt: Mutex::new(None),
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

    pub fn status(&self) -> RuntimeHostStatus {
        let snapshot = self.current_snapshot();
        RuntimeHostStatus {
            fixtures_root: self.fixtures_root.display().to_string(),
            snapshot_root: self.snapshot_root.display().to_string(),
            current_snapshot_id: snapshot.snapshot_id().to_string(),
            plugin_count: snapshot.plugin_registry().iter().count(),
            node_count: snapshot.node_registry().len(),
            last_reload: self.last_reload_attempt(),
        }
    }

    pub fn last_reload_attempt(&self) -> Option<ReloadAttemptReport> {
        self.last_reload_attempt
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
        self.cleanup_retired_snapshots();
        result
    }

    pub fn reload(&self) -> Result<ReloadReport, RuntimeError> {
        match self.reload_internal() {
            Ok((report, attempt)) => {
                self.record_reload_attempt(attempt);
                Ok(report)
            }
            Err((err, attempt)) => {
                self.record_reload_attempt(attempt);
                Err(err)
            }
        }
    }

    pub fn reload_with_diagnostics(&self) -> ReloadAttemptReport {
        match self.reload_internal() {
            Ok((_, attempt)) => {
                self.record_reload_attempt(attempt.clone());
                attempt
            }
            Err((_, attempt)) => {
                self.record_reload_attempt(attempt.clone());
                attempt
            }
        }
    }

    pub fn kernel(&self) -> &RuntimeKernel {
        &self.kernel
    }

    fn reload_internal(&self) -> Result<(ReloadReport, ReloadAttemptReport), (RuntimeError, ReloadAttemptReport)> {
        let previous_snapshot = self.current_snapshot();
        let staged_artifact_root = next_staged_artifact_root(&self.snapshot_root);
        let started_at = Instant::now();

        let next_snapshot = match build_snapshot_with_staged_root(&self.loader, staged_artifact_root.clone()) {
            Ok(snapshot) => Arc::new(snapshot),
            Err(err) => {
                let attempt = ReloadAttemptReport {
                    status: ReloadAttemptStatus::Failed,
                    from_snapshot_id: previous_snapshot.snapshot_id().to_string(),
                    to_snapshot_id: None,
                    snapshot_root: self.snapshot_root.display().to_string(),
                    staged_artifact_root: staged_artifact_root.display().to_string(),
                    elapsed_ms: started_at.elapsed().as_millis(),
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
        self.cleanup_retired_snapshots();

        let attempt = ReloadAttemptReport::from_report(&report);
        Ok((report, attempt))
    }

    fn record_reload_attempt(&self, attempt: ReloadAttemptReport) {
        let mut guard = self
            .last_reload_attempt
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
    fn from_report(report: &ReloadReport) -> Self {
        Self {
            status: ReloadAttemptStatus::Reloaded,
            from_snapshot_id: report.from_snapshot_id.clone(),
            to_snapshot_id: Some(report.to_snapshot_id.clone()),
            snapshot_root: report.snapshot_root.clone(),
            staged_artifact_root: report.staged_artifact_root.clone(),
            elapsed_ms: report.elapsed_ms,
            added_plugins: report.added_plugins.clone(),
            removed_plugins: report.removed_plugins.clone(),
            changed_plugins: report.changed_plugins.clone(),
            changed_plugin_reasons: report.changed_plugin_reasons.clone(),
            failure_summary: None,
        }
    }
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

fn select_registered_dag_subgraph(dag: &RegisteredDag, target_node_fqn: &str) -> BTreeSet<String> {
    let mut selected = BTreeSet::from([target_node_fqn.to_string()]);
    let mut queue = VecDeque::from([target_node_fqn.to_string()]);

    while let Some(current) = queue.pop_front() {
        for edge in dag.edges.iter().filter(|edge| edge.to == current) {
            if selected.insert(edge.from.clone()) {
                queue.push_back(edge.from.clone());
            }
        }
    }

    selected
}

fn build_execution_specs(
    dag: &RegisteredDag,
    selected_nodes: &BTreeSet<String>,
    target_node_fqn: &str,
    fallback_target: &crate::plugin::registry::RegisteredNode,
) -> Vec<ExecutionNodeSpec> {
    let mut dag_nodes = dag
        .nodes
        .iter()
        .filter(|node| selected_nodes.contains(&node.node_fqn))
        .cloned()
        .collect::<Vec<_>>();
    dag_nodes.sort_by(|left, right| {
        left.topo_level
            .cmp(&right.topo_level)
            .then_with(|| left.node_fqn.cmp(&right.node_fqn))
    });

    if dag_nodes.is_empty() {
        return vec![ExecutionNodeSpec {
            dag: DagNodeSpec {
                node_id: target_node_fqn.to_string(),
                priority: 0,
                consumes: Vec::new(),
                produces: Vec::new(),
                control_deps: Vec::new(),
            },
            run_policy: RunPolicy::default(),
            kind: ExecutionNodeKind::Terminal,
        }];
    }

    dag_nodes
        .into_iter()
        .map(|node| execution_spec_from_registered_dag_node(dag, selected_nodes, target_node_fqn, &node))
        .chain(
            (!selected_nodes.contains(target_node_fqn)).then(|| ExecutionNodeSpec {
                dag: DagNodeSpec {
                    node_id: fallback_target.node_fqn.clone(),
                    priority: 0,
                    consumes: Vec::new(),
                    produces: Vec::new(),
                    control_deps: Vec::new(),
                },
                run_policy: RunPolicy::default(),
                kind: ExecutionNodeKind::Terminal,
            }),
        )
        .collect()
}

fn execution_spec_from_registered_dag_node(
    dag: &RegisteredDag,
    selected_nodes: &BTreeSet<String>,
    target_node_fqn: &str,
    node: &RegisteredDagNode,
) -> ExecutionNodeSpec {
    let consumes = dag
        .edges
        .iter()
        .filter(|edge| edge.to == node.node_fqn && selected_nodes.contains(&edge.from))
        .filter_map(|edge| match edge.kind {
            RegisteredDagEdgeKind::Data => Some(DagInputSpec {
                input_type: edge
                    .label
                    .clone()
                    .unwrap_or_else(|| format!("input_from_{}", edge.from)),
                required: false,
                explicit_producer: Some(edge.from.clone()),
            }),
            RegisteredDagEdgeKind::Control => None,
        })
        .collect::<Vec<_>>();
    let control_deps = dag
        .edges
        .iter()
        .filter(|edge| edge.to == node.node_fqn && selected_nodes.contains(&edge.from))
        .filter(|edge| matches!(edge.kind, RegisteredDagEdgeKind::Control))
        .map(|edge| edge.from.clone())
        .collect::<Vec<_>>();

    ExecutionNodeSpec {
        dag: DagNodeSpec {
            node_id: node.node_fqn.clone(),
            priority: 0,
            consumes,
            produces: node.produces.clone(),
            control_deps,
        },
        run_policy: RunPolicy::default(),
        kind: if node.node_fqn == target_node_fqn {
            ExecutionNodeKind::Terminal
        } else {
            ExecutionNodeKind::Task
        },
    }
}

fn build_execution_payload(
    base_payload: &Map<String, Value>,
    consumes: &[DagInputSpec],
    node_responses: &BTreeMap<String, Value>,
) -> Map<String, Value> {
    let mut payload = base_payload.clone();
    for input in consumes {
        let Some(producer) = &input.explicit_producer else {
            continue;
        };
        let Some(response_payload) = node_responses.get(producer) else {
            continue;
        };
        let Some(value) = extract_response_field(response_payload, &input.input_type) else {
            continue;
        };
        payload.insert(input.input_type.clone(), value);
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
        let entry = traces.entry(node_fqn.clone()).or_insert_with(|| ExecutionInvocationTrace {
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

fn runtime_snapshot_from_output(output: LoadOutput, staged_artifact_root: PathBuf) -> RuntimeSnapshot {
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

fn kernel_plan_result(planned: PlannedUpdate, verification_plan: VerificationPlan) -> KernelPlanResult {
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
