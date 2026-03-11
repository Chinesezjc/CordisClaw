//! Plugin loader implementation.
//! Flow:
//! 1) resolve package tree and contracts
//! 2) verify artifact index + fingerprint + hash
//! 3) instantiate plugin runtime (dylib symbol or JSON artifact)
//! 4) register plugins/nodes/context with required/optional propagation

use crate::core::error::RuntimeError;
use crate::core::models::{DylibAbiKind, LoaderBudget, PluginLoadResult, PluginUnavailableReason};
use crate::context::{ContextRegistry, PluginHierarchy, RuntimeContext};
use crate::plugin::artifact::{
    load_artifact_index, load_plugin_artifact, resolve_artifact_path, sha256_file,
};
use crate::plugin::dynamic::{is_dylib_path, sidecar_json_path, LoadedDylibApi};
use crate::plugin::package::PackageResolver;
use crate::plugin::registry::{NodeRegistry, PluginRegistry};
use crate::service::doc_registry::DocRegistry;
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
        let started_at = Instant::now();
        let execution_id = make_execution_id();
        // Phase A: discover + resolve tree from workspace roots.
        let resolver = PackageResolver::new(&self.config.plugins_root);
        let graph = resolver.resolve()?;
        self.ensure_not_timed_out(started_at)?;

        // Budget is enforced before touching artifacts.
        let plugin_count = graph.plugins.len();
        let declared_nodes = graph
            .plugins
            .values()
            .map(|p| p.docs.nodes.len())
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

        let index_map = load_artifact_index(&self.config.artifact_index_path)?;
        self.ensure_not_timed_out(started_at)?;

        let mut plugin_registry = PluginRegistry::default();
        let mut node_registry = NodeRegistry::default();
        let mut metrics = LoaderMetrics::default();

        // Build parent/grants lookup table for context injection.
        let hierarchy = PluginHierarchy {
            parent_of: graph
                .plugins
                .iter()
                .filter_map(|(path, plugin)| plugin.parent.as_ref().map(|p| (path.clone(), p.clone())))
                .collect(),
            grants_from_parent: graph
                .plugins
                .iter()
                .map(|(path, plugin)| (path.clone(), plugin.grants_from_parent.clone()))
                .collect(),
        };

        let mut context = RuntimeContext::with_hierarchy(hierarchy);

        // Phase B: instantiate plugins in resolved topological order.
        for plugin_path in &graph.topo_order {
            self.ensure_not_timed_out(started_at)?;
            let plugin = graph
                .plugins
                .get(plugin_path)
                .ok_or_else(|| RuntimeError::Invariant {
                    message: format!("missing plugin in graph: {plugin_path}"),
                })?;

            // Parent unavailable means this branch is blocked immediately.
            if let Some(parent) = &plugin.parent {
                if let Some(parent_state) = plugin_registry.get(parent) {
                    if !matches!(parent_state.load_result, PluginLoadResult::Loaded) {
                        plugin_registry.insert_unavailable(
                            plugin_path.clone(),
                            plugin.parent.clone(),
                            plugin.required,
                            plugin.grants_from_parent.clone(),
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

            // Current runtime only accepts pure Rust ABI for dylib.
            if plugin.metadata.abi_kind != DylibAbiKind::Rust {
                plugin_registry.insert_unavailable(
                    plugin_path.clone(),
                    plugin.parent.clone(),
                    plugin.required,
                    plugin.grants_from_parent.clone(),
                    PluginUnavailableReason::ContractViolation,
                    vec!["abi_kind must be rust".to_string()],
                );
                context.set_plugin_state(
                    plugin_path,
                    PluginLoadResult::Unavailable(PluginUnavailableReason::ContractViolation),
                );
                metrics.plugin_unavailable_total += 1;
                if plugin.required {
                    self.propagate_parent_failure(
                        plugin_path,
                        &graph.plugins,
                        &mut plugin_registry,
                        &mut node_registry,
                        &mut context,
                    );
                }
                continue;
            }

            // Every resolved plugin must have an explicit prebuilt artifact entry.
            let index_entry = match index_map.get(plugin_path) {
                Some(entry) => entry,
                None => {
                    plugin_registry.insert_unavailable(
                        plugin_path.clone(),
                        plugin.parent.clone(),
                        plugin.required,
                        plugin.grants_from_parent.clone(),
                        PluginUnavailableReason::ArtifactMissing,
                        vec!["artifact index entry missing".to_string()],
                    );
                    context.set_plugin_state(
                        plugin_path,
                        PluginLoadResult::Unavailable(PluginUnavailableReason::ArtifactMissing),
                    );
                    metrics.plugin_unavailable_total += 1;
                    metrics.dylib_no_fallback_total += 1;
                    if plugin.required {
                        self.propagate_parent_failure(
                            plugin_path,
                            &graph.plugins,
                            &mut plugin_registry,
                            &mut node_registry,
                            &mut context,
                        );
                    }
                    continue;
                }
            };

            // Strict fingerprint check: all fields must match.
            if index_entry.abi_fingerprint != plugin.metadata.abi_fingerprint {
                let diff = plugin
                    .metadata
                    .abi_fingerprint
                    .diff(&index_entry.abi_fingerprint);
                plugin_registry.insert_unavailable(
                    plugin_path.clone(),
                    plugin.parent.clone(),
                    plugin.required,
                    plugin.grants_from_parent.clone(),
                    PluginUnavailableReason::AbiMismatch,
                    diff,
                );
                context.set_plugin_state(
                    plugin_path,
                    PluginLoadResult::Unavailable(PluginUnavailableReason::AbiMismatch),
                );
                metrics.plugin_unavailable_total += 1;
                metrics.dylib_abi_mismatch_total += 1;
                metrics.dylib_no_fallback_total += 1;
                if plugin.required {
                    self.propagate_parent_failure(
                        plugin_path,
                        &graph.plugins,
                        &mut plugin_registry,
                        &mut node_registry,
                        &mut context,
                    );
                }
                continue;
            }

            let artifact_path = resolve_artifact_path(
                &self.config.artifact_index_path,
                &index_entry.artifact_path,
            );
            // Missing artifact is terminal for this plugin (no cross-type fallback).
            if !artifact_path.exists() {
                plugin_registry.insert_unavailable(
                    plugin_path.clone(),
                    plugin.parent.clone(),
                    plugin.required,
                    plugin.grants_from_parent.clone(),
                    PluginUnavailableReason::ArtifactMissing,
                    vec![format!("artifact does not exist: {}", artifact_path.display())],
                );
                context.set_plugin_state(
                    plugin_path,
                    PluginLoadResult::Unavailable(PluginUnavailableReason::ArtifactMissing),
                );
                metrics.plugin_unavailable_total += 1;
                metrics.dylib_no_fallback_total += 1;
                if plugin.required {
                    self.propagate_parent_failure(
                        plugin_path,
                        &graph.plugins,
                        &mut plugin_registry,
                        &mut node_registry,
                        &mut context,
                    );
                }
                continue;
            }

            // Hash check guarantees artifact content integrity.
            let actual_hash = sha256_file(&artifact_path)?;
            if actual_hash != index_entry.sha256 {
                plugin_registry.insert_unavailable(
                    plugin_path.clone(),
                    plugin.parent.clone(),
                    plugin.required,
                    plugin.grants_from_parent.clone(),
                    PluginUnavailableReason::HashMismatch,
                    vec![format!("expected hash {}, got {}", index_entry.sha256, actual_hash)],
                );
                context.set_plugin_state(
                    plugin_path,
                    PluginLoadResult::Unavailable(PluginUnavailableReason::HashMismatch),
                );
                metrics.plugin_unavailable_total += 1;
                metrics.dylib_no_fallback_total += 1;
                if plugin.required {
                    self.propagate_parent_failure(
                        plugin_path,
                        &graph.plugins,
                        &mut plugin_registry,
                        &mut node_registry,
                        &mut context,
                    );
                }
                continue;
            }

            if is_dylib_path(&artifact_path) {
                // Dylib path: load fixed Rust symbol table from shared object.
                let dylib = match LoadedDylibApi::open(&artifact_path) {
                    Ok(v) => v,
                    Err(err) => {
                        plugin_registry.insert_unavailable(
                            plugin_path.clone(),
                            plugin.parent.clone(),
                            plugin.required,
                            plugin.grants_from_parent.clone(),
                            PluginUnavailableReason::SymbolMissing,
                            vec![err.to_string()],
                        );
                        context.set_plugin_state(
                            plugin_path,
                            PluginLoadResult::Unavailable(PluginUnavailableReason::SymbolMissing),
                        );
                        metrics.plugin_unavailable_total += 1;
                        metrics.dylib_no_fallback_total += 1;
                        if plugin.required {
                            self.propagate_parent_failure(
                                plugin_path,
                                &graph.plugins,
                                &mut plugin_registry,
                                &mut node_registry,
                                &mut context,
                            );
                        }
                        continue;
                    }
                };

                let api = dylib.api();
                if api.abi_kind != DylibAbiKind::Rust {
                    plugin_registry.insert_unavailable(
                        plugin_path.clone(),
                        plugin.parent.clone(),
                        plugin.required,
                        plugin.grants_from_parent.clone(),
                        PluginUnavailableReason::AbiMismatch,
                        vec!["runtime exported abi_kind is not rust".to_string()],
                    );
                    context.set_plugin_state(
                        plugin_path,
                        PluginLoadResult::Unavailable(PluginUnavailableReason::AbiMismatch),
                    );
                    metrics.plugin_unavailable_total += 1;
                    metrics.dylib_abi_mismatch_total += 1;
                    metrics.dylib_no_fallback_total += 1;
                    if plugin.required {
                        self.propagate_parent_failure(
                            plugin_path,
                            &graph.plugins,
                            &mut plugin_registry,
                            &mut node_registry,
                            &mut context,
                        );
                    }
                    continue;
                }

                let runtime_fingerprint = api.abi_fingerprint.to_owned();
                // Validate runtime-exported fingerprint against resolved contract.
                if runtime_fingerprint != plugin.metadata.abi_fingerprint {
                    let diff = plugin
                        .metadata
                        .abi_fingerprint
                        .diff(&runtime_fingerprint);
                    plugin_registry.insert_unavailable(
                        plugin_path.clone(),
                        plugin.parent.clone(),
                        plugin.required,
                        plugin.grants_from_parent.clone(),
                        PluginUnavailableReason::AbiMismatch,
                        diff,
                    );
                    context.set_plugin_state(
                        plugin_path,
                        PluginLoadResult::Unavailable(PluginUnavailableReason::AbiMismatch),
                    );
                    metrics.plugin_unavailable_total += 1;
                    metrics.dylib_abi_mismatch_total += 1;
                    metrics.dylib_no_fallback_total += 1;
                    if plugin.required {
                        self.propagate_parent_failure(
                            plugin_path,
                            &graph.plugins,
                            &mut plugin_registry,
                            &mut node_registry,
                            &mut context,
                        );
                    }
                    continue;
                }

                let plugin_handle = (api.init)();
                let docs = (api.docs)(plugin_handle.as_ref());
                // Runtime docs must point back to the same plugin path.
                if docs.plugin_path != *plugin_path {
                    plugin_registry.insert_unavailable(
                        plugin_path.clone(),
                        plugin.parent.clone(),
                        plugin.required,
                        plugin.grants_from_parent.clone(),
                        PluginUnavailableReason::ContractViolation,
                        vec!["docs.plugin_path mismatch".to_string()],
                    );
                    context.set_plugin_state(
                        plugin_path,
                        PluginLoadResult::Unavailable(PluginUnavailableReason::ContractViolation),
                    );
                    metrics.plugin_unavailable_total += 1;
                    metrics.dylib_no_fallback_total += 1;
                    (api.drop)(plugin_handle);
                    if plugin.required {
                        self.propagate_parent_failure(
                            plugin_path,
                            &graph.plugins,
                            &mut plugin_registry,
                            &mut node_registry,
                            &mut context,
                        );
                    }
                    continue;
                }

                node_registry.register_from_docs(plugin_path, &docs)?;
                plugin_registry.insert_loaded(
                    plugin_path.clone(),
                    plugin.parent.clone(),
                    plugin.required,
                    plugin.grants_from_parent.clone(),
                    docs,
                    artifact_path.clone(),
                );
                context.set_plugin_state(plugin_path, PluginLoadResult::Loaded);
                context.ensure_local_scope(plugin_path);
                (api.drop)(plugin_handle);

                let sidecar_path = sidecar_json_path(&artifact_path);
                // Optional sidecar can export Local services for child injection.
                if sidecar_path.exists() {
                    let sidecar = load_plugin_artifact(&sidecar_path)?;
                    for export in sidecar.exports {
                        context.provide(
                            crate::context::ContextScope::Local,
                            Some(plugin_path),
                            &export,
                            format!("service:{plugin_path}:{export}"),
                        )?;
                    }
                }
            } else {
                // JSON artifact path: fully materialized docs/exports in one file.
                let artifact = load_plugin_artifact(&artifact_path)?;
                if artifact.plugin_path != *plugin_path {
                    plugin_registry.insert_unavailable(
                        plugin_path.clone(),
                        plugin.parent.clone(),
                        plugin.required,
                        plugin.grants_from_parent.clone(),
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
                    if plugin.required {
                        self.propagate_parent_failure(
                            plugin_path,
                            &graph.plugins,
                            &mut plugin_registry,
                            &mut node_registry,
                            &mut context,
                        );
                    }
                    continue;
                }

                if artifact.abi_fingerprint != plugin.metadata.abi_fingerprint {
                    let diff = plugin
                        .metadata
                        .abi_fingerprint
                        .diff(&artifact.abi_fingerprint);
                    plugin_registry.insert_unavailable(
                        plugin_path.clone(),
                        plugin.parent.clone(),
                        plugin.required,
                        plugin.grants_from_parent.clone(),
                        PluginUnavailableReason::AbiMismatch,
                        diff,
                    );
                    context.set_plugin_state(
                        plugin_path,
                        PluginLoadResult::Unavailable(PluginUnavailableReason::AbiMismatch),
                    );
                    metrics.plugin_unavailable_total += 1;
                    metrics.dylib_abi_mismatch_total += 1;
                    metrics.dylib_no_fallback_total += 1;
                    if plugin.required {
                        self.propagate_parent_failure(
                            plugin_path,
                            &graph.plugins,
                            &mut plugin_registry,
                            &mut node_registry,
                            &mut context,
                        );
                    }
                    continue;
                }

                if artifact.docs.plugin_path != *plugin_path {
                    plugin_registry.insert_unavailable(
                        plugin_path.clone(),
                        plugin.parent.clone(),
                        plugin.required,
                        plugin.grants_from_parent.clone(),
                        PluginUnavailableReason::ContractViolation,
                        vec!["docs.plugin_path mismatch".to_string()],
                    );
                    context.set_plugin_state(
                        plugin_path,
                        PluginLoadResult::Unavailable(PluginUnavailableReason::ContractViolation),
                    );
                    metrics.plugin_unavailable_total += 1;
                    if plugin.required {
                        self.propagate_parent_failure(
                            plugin_path,
                            &graph.plugins,
                            &mut plugin_registry,
                            &mut node_registry,
                            &mut context,
                        );
                    }
                    continue;
                }

                node_registry.register_from_docs(plugin_path, &artifact.docs)?;
                plugin_registry.insert_loaded(
                    plugin_path.clone(),
                    plugin.parent.clone(),
                    plugin.required,
                    plugin.grants_from_parent.clone(),
                    artifact.docs,
                    artifact_path,
                );
                context.set_plugin_state(plugin_path, PluginLoadResult::Loaded);
                context.ensure_local_scope(plugin_path);
                for export in artifact.exports {
                    context.provide(
                        crate::context::ContextScope::Local,
                        Some(plugin_path),
                        &export,
                        format!("service:{plugin_path}:{export}"),
                    )?;
                }
            }
        }

        let doc_registry = DocRegistry::from_plugin_registry(&plugin_registry);

        Ok(LoadOutput {
            execution_id,
            plugin_registry,
            node_registry,
            doc_registry,
            context,
            metrics,
        })
    }

    fn propagate_parent_failure(
        &self,
        failed_plugin_path: &str,
        plugins: &BTreeMap<String, crate::plugin::package::ResolvedPlugin>,
        plugin_registry: &mut PluginRegistry,
        node_registry: &mut NodeRegistry,
        context: &mut RuntimeContext,
    ) {
        // Required child failure bubbles upward until first non-required edge.
        let mut current = plugins
            .get(failed_plugin_path)
            .and_then(|plugin| plugin.parent.clone());

        while let Some(parent_path) = current {
            plugin_registry.mark_unavailable(&parent_path, PluginUnavailableReason::InitFailed);
            node_registry.remove_by_plugin(&parent_path);
            context.set_plugin_state(
                &parent_path,
                PluginLoadResult::Unavailable(PluginUnavailableReason::InitFailed),
            );

            let parent = match plugins.get(&parent_path) {
                Some(v) => v,
                None => break,
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
            // Conservative defaults suitable for local prototype deployments.
            max_total_plugins: 256,
            max_total_nodes: 4096,
            load_timeout_ms: 15_000,
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
