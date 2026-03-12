//! Plugin loader implementation.
//! Flow:
//! 1) read artifact index
//! 2) verify artifact hash + availability
//! 3) register plugins/nodes/context from index docs
//! 4) defer dylib ABI/docs guard to first invoke

use crate::core::error::RuntimeError;
use crate::core::models::{ArtifactIndexEntry, ArtifactKind, LoaderBudget, PluginLoadResult, PluginUnavailableReason};
use crate::context::{ContextRegistry, PluginHierarchy, RuntimeContext};
use crate::plugin::artifact::{
    artifact_index_map, load_artifact_index, load_plugin_artifact, resolve_artifact_path, sha256_file,
    stage_artifact_bundle,
};
use crate::plugin::registry::{NodeRegistry, PluginRegistry};
use crate::service::doc_registry::DocRegistry;
use crate::service::graph_registry::GraphRegistry;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone)]
pub struct LoaderConfig {
    /// Root directory containing `plugins/`.
    pub plugins_root: PathBuf,
    /// Artifact index file path (JSON).
    pub artifact_index_path: PathBuf,
    /// Hard limits preventing unbounded load expansion.
    pub budget: LoaderBudget,
}

#[derive(Debug, Default, Clone)]
pub struct LoaderMetrics {
    /// Count of ABI mismatches on dylib path.
    pub dylib_abi_mismatch_total: u64,
    /// Count of failures where fallback was intentionally not attempted.
    pub dylib_no_fallback_total: u64,
    /// Count of plugins marked unavailable.
    pub plugin_unavailable_total: u64,
}

#[derive(Debug, Clone)]
pub struct LoadOutput {
    pub execution_id: String,
    pub plugin_registry: PluginRegistry,
    pub node_registry: NodeRegistry,
    pub doc_registry: DocRegistry,
    pub graph_registry: GraphRegistry,
    pub context: RuntimeContext,
    pub metrics: LoaderMetrics,
}

#[derive(Debug)]
pub struct Loader {
    config: LoaderConfig,
}

impl Loader {
    pub fn new(config: LoaderConfig) -> Self {
        Self { config }
    }

    pub fn load(&self) -> Result<LoadOutput, RuntimeError> {
        self.load_with_staging_root(None)
    }

    pub fn load_with_staging_root(
        &self,
        staged_root: Option<&Path>,
    ) -> Result<LoadOutput, RuntimeError> {
        let started_at = Instant::now();
        let execution_id = make_execution_id();
        let index = load_artifact_index(&self.config.artifact_index_path)?;
        self.ensure_not_timed_out(started_at)?;

        let plugin_count = index.entries.len();
        let declared_nodes = index
            .entries
            .iter()
            .map(|entry| entry.docs.nodes.len())
            .sum::<usize>();
        if plugin_count > self.config.budget.max_total_plugins
            || declared_nodes > self.config.budget.max_total_nodes
        {
            return Err(RuntimeError::BudgetExceeded {
                max_total_plugins: self.config.budget.max_total_plugins,
                max_total_nodes: self.config.budget.max_total_nodes,
                actual_plugins: plugin_count,
                actual_nodes: declared_nodes,
            });
        }

        let index_map = artifact_index_map(&index);
        let plugin_registry = PluginRegistry::default();
        let mut node_registry = NodeRegistry::default();
        let mut metrics = LoaderMetrics::default();

        let hierarchy = PluginHierarchy {
            parent_of: index_map
                .iter()
                .filter_map(|(path, entry)| entry.parent.as_ref().map(|parent| (path.clone(), parent.clone())))
                .collect(),
            grants_from_parent: index_map
                .iter()
                .map(|(path, entry)| {
                    (
                        path.clone(),
                        entry.grants_from_parent.iter().cloned().collect(),
                    )
                })
                .collect(),
        };
        let mut context = RuntimeContext::with_hierarchy(hierarchy);

        for plugin_path in &index.topo_order {
            self.ensure_not_timed_out(started_at)?;
            let entry = index_map
                .get(plugin_path)
                .ok_or_else(|| RuntimeError::ArtifactIndexMissing {
                    plugin_path: plugin_path.clone(),
                })?;

            if let Some(parent) = &entry.parent {
                if let Some(parent_state) = plugin_registry.get(parent) {
                    if !matches!(parent_state.load_result, PluginLoadResult::Loaded) {
                        plugin_registry.insert_unavailable(
                            plugin_path.clone(),
                            entry.parent.clone(),
                            entry.required,
                            entry.grants_from_parent.iter().cloned().collect(),
                            PluginUnavailableReason::InitFailed,
                            Vec::new(),
                        );
                        context.set_plugin_state(
                            plugin_path,
                            PluginLoadResult::Unavailable(PluginUnavailableReason::InitFailed),
                        );
                        metrics.plugin_unavailable_total += 1;
                        continue;
                    }
                }
            }

            let resolved_artifact_path = resolve_artifact_path(
                &self.config.artifact_index_path,
                &entry.artifact_path,
            );
            if !resolved_artifact_path.exists() {
                plugin_registry.insert_unavailable(
                    plugin_path.clone(),
                    entry.parent.clone(),
                    entry.required,
                    entry.grants_from_parent.iter().cloned().collect(),
                    PluginUnavailableReason::ArtifactMissing,
                    vec![format!(
                        "artifact does not exist: {}",
                        resolved_artifact_path.display()
                    )],
                );
                context.set_plugin_state(
                    plugin_path,
                    PluginLoadResult::Unavailable(PluginUnavailableReason::ArtifactMissing),
                );
                metrics.plugin_unavailable_total += 1;
                metrics.dylib_no_fallback_total += 1;
                if entry.required {
                    self.propagate_parent_failure(
                        plugin_path,
                        &index_map,
                        &plugin_registry,
                        &mut node_registry,
                        &mut context,
                    );
                }
                continue;
            }

            let actual_hash = sha256_file(&resolved_artifact_path)?;
            if actual_hash != entry.sha256 {
                plugin_registry.insert_unavailable(
                    plugin_path.clone(),
                    entry.parent.clone(),
                    entry.required,
                    entry.grants_from_parent.iter().cloned().collect(),
                    PluginUnavailableReason::HashMismatch,
                    vec![format!("expected hash {}, got {}", entry.sha256, actual_hash)],
                );
                context.set_plugin_state(
                    plugin_path,
                    PluginLoadResult::Unavailable(PluginUnavailableReason::HashMismatch),
                );
                metrics.plugin_unavailable_total += 1;
                metrics.dylib_no_fallback_total += 1;
                if entry.required {
                    self.propagate_parent_failure(
                        plugin_path,
                        &index_map,
                        &plugin_registry,
                        &mut node_registry,
                        &mut context,
                    );
                }
                continue;
            }

            let artifact_path = match staged_root {
                Some(root) => stage_artifact_bundle(
                    plugin_path,
                    &entry.artifact_path,
                    &resolved_artifact_path,
                    root,
                )?,
                None => resolved_artifact_path,
            };

            if matches!(entry.artifact_kind, ArtifactKind::Json) {
                let artifact = load_plugin_artifact(&artifact_path)?;
                if artifact.plugin_path != *plugin_path {
                    plugin_registry.insert_unavailable(
                        plugin_path.clone(),
                        entry.parent.clone(),
                        entry.required,
                        entry.grants_from_parent.iter().cloned().collect(),
                        PluginUnavailableReason::ContractViolation,
                        vec![format!(
                            "artifact.plugin_path mismatch, expected {}, got {}",
                            plugin_path, artifact.plugin_path
                        )],
                    );
                    context.set_plugin_state(
                        plugin_path,
                        PluginLoadResult::Unavailable(PluginUnavailableReason::ContractViolation),
                    );
                    metrics.plugin_unavailable_total += 1;
                    metrics.dylib_no_fallback_total += 1;
                    if entry.required {
                        self.propagate_parent_failure(
                            plugin_path,
                            &index_map,
                            &plugin_registry,
                            &mut node_registry,
                            &mut context,
                        );
                    }
                    continue;
                }

                if artifact.abi_fingerprint != entry.abi_fingerprint {
                    plugin_registry.insert_unavailable(
                        plugin_path.clone(),
                        entry.parent.clone(),
                        entry.required,
                        entry.grants_from_parent.iter().cloned().collect(),
                        PluginUnavailableReason::AbiMismatch,
                        entry.abi_fingerprint.diff(&artifact.abi_fingerprint),
                    );
                    context.set_plugin_state(
                        plugin_path,
                        PluginLoadResult::Unavailable(PluginUnavailableReason::AbiMismatch),
                    );
                    metrics.plugin_unavailable_total += 1;
                    metrics.dylib_abi_mismatch_total += 1;
                    metrics.dylib_no_fallback_total += 1;
                    if entry.required {
                        self.propagate_parent_failure(
                            plugin_path,
                            &index_map,
                            &plugin_registry,
                            &mut node_registry,
                            &mut context,
                        );
                    }
                    continue;
                }

                if artifact.docs != entry.docs {
                    plugin_registry.insert_unavailable(
                        plugin_path.clone(),
                        entry.parent.clone(),
                        entry.required,
                        entry.grants_from_parent.iter().cloned().collect(),
                        PluginUnavailableReason::ContractViolation,
                        vec!["artifact docs mismatch".to_string()],
                    );
                    context.set_plugin_state(
                        plugin_path,
                        PluginLoadResult::Unavailable(PluginUnavailableReason::ContractViolation),
                    );
                    metrics.plugin_unavailable_total += 1;
                    metrics.dylib_no_fallback_total += 1;
                    if entry.required {
                        self.propagate_parent_failure(
                            plugin_path,
                            &index_map,
                            &plugin_registry,
                            &mut node_registry,
                            &mut context,
                        );
                    }
                    continue;
                }
            }

            node_registry.register_from_docs(plugin_path, &entry.docs)?;
            plugin_registry.insert_loaded(
                plugin_path.clone(),
                entry.parent.clone(),
                entry.required,
                entry.grants_from_parent.iter().cloned().collect(),
                entry.docs.clone(),
                artifact_path,
                entry.artifact_kind.clone(),
                entry.abi_fingerprint.clone(),
                entry.execution.clone(),
            );
            context.set_plugin_state(plugin_path, PluginLoadResult::Loaded);
            context.ensure_local_scope(plugin_path);
            for export in &entry.exports {
                context.provide(
                    crate::context::ContextScope::Local,
                    Some(plugin_path),
                    export,
                    format!("service:{plugin_path}:{export}"),
                )?;
            }
        }

        let doc_registry = DocRegistry::from_plugin_registry(&plugin_registry);
        let graph_registry = GraphRegistry::from_registries(&plugin_registry, &node_registry);
        Ok(LoadOutput {
            execution_id,
            plugin_registry,
            node_registry,
            doc_registry,
            graph_registry,
            context,
            metrics,
        })
    }

    fn propagate_parent_failure(
        &self,
        failed_plugin_path: &str,
        entries: &BTreeMap<String, ArtifactIndexEntry>,
        plugin_registry: &PluginRegistry,
        node_registry: &mut NodeRegistry,
        context: &mut RuntimeContext,
    ) {
        let mut current = entries
            .get(failed_plugin_path)
            .and_then(|entry| entry.parent.clone());

        while let Some(parent_path) = current {
            plugin_registry.mark_unavailable(&parent_path, PluginUnavailableReason::InitFailed);
            node_registry.remove_by_plugin(&parent_path);
            context.set_plugin_state(
                &parent_path,
                PluginLoadResult::Unavailable(PluginUnavailableReason::InitFailed),
            );

            let Some(parent) = entries.get(&parent_path) else {
                break;
            };
            if parent.required {
                current = parent.parent.clone();
            } else {
                break;
            }
        }
    }

    fn ensure_not_timed_out(&self, started_at: Instant) -> Result<(), RuntimeError> {
        let elapsed_ms = started_at.elapsed().as_millis();
        if elapsed_ms > self.config.budget.load_timeout_ms as u128 {
            return Err(RuntimeError::LoadTimeout {
                limit_ms: self.config.budget.load_timeout_ms,
                elapsed_ms,
            });
        }
        Ok(())
    }
}

pub fn default_loader_config(root: impl AsRef<Path>) -> LoaderConfig {
    let root = root.as_ref();
    LoaderConfig {
        plugins_root: root.join("plugins"),
        artifact_index_path: root.join("artifacts/index.json"),
        budget: LoaderBudget {
            max_total_plugins: 256,
            max_total_nodes: 4096,
            load_timeout_ms: 30_000,
        },
    }
}

fn make_execution_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("exec-{nanos}")
}
