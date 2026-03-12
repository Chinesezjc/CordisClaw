use crate::config::{LlmApiConfig, PluginConfigFile, RuntimeConfig};
use crate::context::RuntimeContext;
use crate::core::error::RuntimeError;
use crate::core::models::PluginLoadResult;
use crate::kernel::auto_update::{AutoUpdatePlan, AutoUpdateResult, AutoUpdater};
use crate::kernel::evaluator::{EvalHarness, VerificationInput};
use crate::kernel::memory::ChangeRecord;
use crate::kernel::memory::ChangeMemory;
use crate::kernel::planner::{LlmPatchPlanner, PlanRequest, PlannedUpdate};
use crate::kernel::policy::IterationPolicy;
use crate::kernel::r#loop::SelfIterationKernel;
use crate::kernel::verifier::{CommandVerifier, VerificationReport};
use crate::plugin::abi::PluginResponse;
use crate::plugin::invoke::invoke_registered_plugin;
use crate::plugin::loader::{default_loader_config, LoadOutput, Loader};
use crate::plugin::registry::{NodeRegistry, PluginRegistry, RegisteredPlugin};
use crate::service::doc_registry::DocRegistry;
use crate::service::graph_registry::GraphRegistry;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReloadReport {
    pub from_snapshot_id: String,
    pub to_snapshot_id: String,
    pub added_plugins: Vec<String>,
    pub removed_plugins: Vec<String>,
    pub changed_plugins: Vec<String>,
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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
    pub quality_score: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KernelPlanResult {
    pub plan: AutoUpdatePlan,
    pub summary: String,
    pub planner_model: String,
    pub response_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
        self.updater.execute(&mut kernel, plan, |_| Ok(verification))
    }

    pub fn plan_update(&self, request: KernelPlanRequest) -> Result<KernelPlanResult, RuntimeError> {
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
        Ok(kernel_plan_result(planned))
    }

    pub fn plan_and_run_iteration(
        &self,
        request: KernelPlanRequest,
    ) -> Result<KernelPlanApplyResult, RuntimeError> {
        let tests_command = request.tests_command.clone();
        let safety_command = request.safety_command.clone();
        let quality_score = request.quality_score;
        let planned = self.plan_update(request)?;
        let mut verification_report = None;
        let mut kernel = self.inner.lock().unwrap_or_else(|poison| poison.into_inner());
        let result = self.updater.execute(&mut kernel, planned.plan.clone(), |workspace_root| {
            let report = CommandVerifier::verify(
                workspace_root,
                tests_command.as_deref(),
                safety_command.as_deref(),
                quality_score,
            )?;
            let input = report.input.clone();
            verification_report = Some(report);
            Ok(input)
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

    pub fn reload(&self) -> Result<ReloadReport, RuntimeError> {
        let next_snapshot = Arc::new(build_snapshot(&self.loader, &self.snapshot_root)?);
        let previous_snapshot = {
            let mut guard = self
                .current_snapshot
                .write()
                .unwrap_or_else(|poison| poison.into_inner());
            let previous = guard.clone();
            *guard = next_snapshot.clone();
            previous
        };

        let report = ReloadReport::from_snapshots(previous_snapshot.as_ref(), next_snapshot.as_ref());
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
        Ok(report)
    }

    pub fn kernel(&self) -> &RuntimeKernel {
        &self.kernel
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
    fn from_snapshots(previous: &RuntimeSnapshot, next: &RuntimeSnapshot) -> Self {
        let mut added_plugins = Vec::new();
        let mut removed_plugins = Vec::new();
        let mut changed_plugins = Vec::new();

        for (plugin_path, plugin) in next.plugin_registry.iter() {
            match previous.plugin_registry.get(plugin_path) {
                None => added_plugins.push(plugin_path.clone()),
                Some(previous_plugin) if plugin_changed(previous_plugin, plugin) => {
                    changed_plugins.push(plugin_path.clone())
                }
                Some(_) => {}
            }
        }

        for (plugin_path, _) in previous.plugin_registry.iter() {
            if next.plugin_registry.get(plugin_path).is_none() {
                removed_plugins.push(plugin_path.clone());
            }
        }

        Self {
            from_snapshot_id: previous.snapshot_id.clone(),
            to_snapshot_id: next.snapshot_id.clone(),
            added_plugins,
            removed_plugins,
            changed_plugins,
        }
    }
}

fn plugin_changed(previous: &RegisteredPlugin, next: &RegisteredPlugin) -> bool {
    previous.parent != next.parent
        || previous.required != next.required
        || previous.grants_from_parent != next.grants_from_parent
        || previous.load_result != next.load_result
        || previous.docs != next.docs
        || previous.fingerprint_diff != next.fingerprint_diff
}

fn build_snapshot(loader: &Loader, snapshot_root: &Path) -> Result<RuntimeSnapshot, RuntimeError> {
    let staged_artifact_root = snapshot_root.join(make_snapshot_dir_name());
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

fn kernel_plan_result(planned: PlannedUpdate) -> KernelPlanResult {
    KernelPlanResult {
        plan: planned.plan,
        summary: planned.summary,
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
