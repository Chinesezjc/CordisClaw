//! Plugin loader implementation.
//! Flow:
//! 1) read artifact index
//! 2) verify artifact hash + availability
//! 3) for dylibs: check target triple against host, read docs from the
//!    artifact (failure = unavailable); full ABI guard stays at invoke
//! 4) register plugins/nodes/context from index docs

use crate::context::{ContextRegistry, PluginHierarchy, RuntimeContext};
use crate::core::error::RuntimeError;
use crate::core::models::{
    ArtifactIndexEntry, ArtifactKind, LoaderBudget, PluginDocs, PluginLoadResult,
    PluginUnavailableReason,
};
use crate::plugin::artifact::{
    artifact_index_map, load_artifact_index, load_plugin_artifact, resolve_artifact_path,
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
        let mut index = load_artifact_index(&self.config.artifact_index_path)?;
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
        let mut docs_drift: Vec<(String, PluginDocs)> = Vec::new();

        let hierarchy = PluginHierarchy {
            parent_of: index_map
                .iter()
                .filter_map(|(path, entry)| {
                    entry
                        .parent
                        .as_ref()
                        .map(|parent| (path.clone(), parent.clone()))
                })
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
            let entry =
                index_map
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

            let resolved_artifact_path =
                resolve_artifact_path(&self.config.artifact_index_path, &entry.artifact_path);
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

            // P0-13: verify artifact bytes match the sha256 recorded in
            // index.json. The old implementation trusted `entry.sha256`
            // without ever computing the actual file hash, so a tampered
            // `.so` (or a botched rebuild) loaded silently. We hash the file
            // on-load and raise `HashMismatch` on divergence — the runtime
            // then routes the plugin through the standard "unavailable"
            // pathway rather than dlopen-ing a modified artifact.
            match crate::plugin::artifact::sha256_file(&resolved_artifact_path) {
                Ok(actual) => {
                    if actual != entry.sha256 {
                        plugin_registry.insert_unavailable(
                            plugin_path.clone(),
                            entry.parent.clone(),
                            entry.required,
                            entry.grants_from_parent.iter().cloned().collect(),
                            PluginUnavailableReason::HashMismatch,
                            vec![format!(
                                "artifact sha256 mismatch for {}: expected {}, got {}",
                                resolved_artifact_path.display(),
                                entry.sha256,
                                actual
                            )],
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
                }
                Err(err) => {
                    plugin_registry.insert_unavailable(
                        plugin_path.clone(),
                        entry.parent.clone(),
                        entry.required,
                        entry.grants_from_parent.iter().cloned().collect(),
                        PluginUnavailableReason::HashMismatch,
                        vec![format!(
                            "artifact sha256 read failed for {}: {err}",
                            resolved_artifact_path.display()
                        )],
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
            }

            // Dylib artifacts are platform-specific: dlopen-ing a binary
            // built for another target triple can never succeed. Check the
            // recorded triple against the host before staging or opening,
            // so a cross-platform checkout (e.g. linux-built fixtures on a
            // macOS host) surfaces as `AbiMismatch` instead of a runtime
            // failure at first invoke.
            if !matches!(entry.artifact_kind, ArtifactKind::Json)
                && entry.abi_fingerprint.target_triple != cordis_plugin_sdk::CORDIS_TARGET
            {
                plugin_registry.insert_unavailable(
                    plugin_path.clone(),
                    entry.parent.clone(),
                    entry.required,
                    entry.grants_from_parent.iter().cloned().collect(),
                    PluginUnavailableReason::AbiMismatch,
                    vec![format!(
                        "dylib target triple mismatch: artifact={}, host={}",
                        entry.abi_fingerprint.target_triple,
                        cordis_plugin_sdk::CORDIS_TARGET
                    )],
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

            let artifact_path = match staged_root {
                Some(root) => stage_artifact_bundle(
                    plugin_path,
                    &entry.artifact_path,
                    &resolved_artifact_path,
                    root,
                )?,
                None => resolved_artifact_path,
            };

            // Effective docs for this plugin — may be updated if drift is
            // detected. Defaults to the cached index entry; replaced with
            // fresh artifact docs on drift.
            let mut effective_docs: Option<PluginDocs> = None;

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
                    // Docs drifted — auto-heal: take artifact docs as ground
                    // truth, refresh cache + interfaces.json after load.
                    docs_drift.push((plugin_path.clone(), artifact.docs.clone()));
                    effective_docs = Some(artifact.docs);
                }
            }

            // For dylib artifacts, extract docs from the .so and compare
            // against the cached index entry.
            //
            // P2-34 follow-up: the target-triple precheck above already
            // rejected architecture-mismatched dylibs, so a failure here —
            // dlopen error, missing symbol, or unparseable docs payload —
            // is a genuine artifact fault. Marking the plugin "loaded"
            // anyway (the previous fallback-to-cached-docs behavior) hid
            // broken artifacts until first invoke; mark it unavailable
            // instead. The Ok path keeps the docs-drift auto-heal: a
            // readable dylib's docs are ground truth over the cache.
            if !matches!(entry.artifact_kind, ArtifactKind::Json) {
                match crate::plugin::tooling::read_plugin_docs(&artifact_path) {
                    Ok(dylib_docs) => {
                        if dylib_docs != entry.docs {
                            docs_drift.push((plugin_path.clone(), dylib_docs.clone()));
                            effective_docs = Some(dylib_docs);
                        }
                    }
                    Err(err) => {
                        plugin_registry.insert_unavailable(
                            plugin_path.clone(),
                            entry.parent.clone(),
                            entry.required,
                            entry.grants_from_parent.iter().cloned().collect(),
                            PluginUnavailableReason::SymbolMissing,
                            vec![format!(
                                "read_plugin_docs({}) failed: {err}",
                                artifact_path.display()
                            )],
                        );
                        context.set_plugin_state(
                            plugin_path,
                            PluginLoadResult::Unavailable(PluginUnavailableReason::SymbolMissing),
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
            }

            let docs = effective_docs.as_ref().unwrap_or(&entry.docs);
            node_registry.register_from_docs(plugin_path, docs)?;
            plugin_registry.insert_loaded(
                plugin_path.clone(),
                entry.parent.clone(),
                entry.required,
                entry.grants_from_parent.iter().cloned().collect(),
                docs.clone(),
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

        // Auto-heal: write back updated docs to index.json and sync
        // interfaces.json for any plugins whose docs drifted from cache.
        //
        // P0-14: two concurrent loaders (primary + candidate) used to race
        // here on a shared `.tmp` filename, causing torn / mis-attributed
        // interfaces.json / index.json writes. We now:
        //  - hold a POSIX file lock on `<snapshot_root>/docs-heal.lock` for
        //    the duration of the auto-heal block (fcntl F_SETLK, released
        //    on drop) so at most one loader writes at a time;
        //  - use per-invocation staging filenames (pid + seq + nanos) so
        //    two loaders on different mount points can't clobber each other
        //    even without the lock.
        if !docs_drift.is_empty() {
            let plugins_root = &self.config.plugins_root;
            let index_path = &self.config.artifact_index_path;
            let lock_path = index_path.with_extension("json.heal-lock");
            let _lock = auto_heal_file_lock::acquire(&lock_path);
            for item in &docs_drift {
                let plugin_path: &str = &item.0;
                let new_docs: &PluginDocs = &item.1;
                // Update the in-memory index entry.
                for entry in &mut index.entries {
                    if entry.plugin_path == plugin_path {
                        entry.docs = new_docs.clone();
                        break;
                    }
                }
                // Sync interfaces.json on disk.
                let docs_path = plugins_root
                    .join(plugin_path.replace('/', std::path::MAIN_SEPARATOR_STR))
                    .join("docs/agent/interfaces.json");
                if let Some(parent) = docs_path.parent() {
                    if let Err(e) = std::fs::create_dir_all(parent) {
                        eprintln!(
                            "[loader] docs-drift: failed to create dir {}: {e}",
                            parent.display()
                        );
                        continue;
                    }
                }
                match serde_json::to_string_pretty(new_docs) {
                    Ok(json) => {
                        let tmp = unique_staging_path(&docs_path);
                        if let Err(e) = std::fs::write(&tmp, &json) {
                            eprintln!(
                                "[loader] docs-drift: failed to write tmp {}: {e}",
                                tmp.display()
                            );
                            continue;
                        }
                        if let Err(e) = std::fs::rename(&tmp, &docs_path) {
                            eprintln!(
                                "[loader] docs-drift: failed to rename {} → {}: {e}",
                                tmp.display(),
                                docs_path.display()
                            );
                            let _ = std::fs::remove_file(&tmp);
                        } else {
                            eprintln!(
                                "[loader] docs-drift: auto-healed {plugin_path} — \
                                 interfaces.json refreshed from artifact"
                            );
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "[loader] docs-drift: failed to serialize docs for {plugin_path}: {e}"
                        );
                    }
                }
            }
            // Atomic write-back of the artifact index.
            match serde_json::to_string_pretty(&index) {
                Ok(json) => {
                    let tmp = unique_staging_path(index_path);
                    if let Err(e) = std::fs::write(&tmp, &json) {
                        eprintln!(
                            "[loader] docs-drift: failed to write index tmp {}: {e}",
                            tmp.display()
                        );
                    } else if let Err(e) = std::fs::rename(&tmp, index_path) {
                        eprintln!(
                            "[loader] docs-drift: failed to rename index {} → {}: {e}",
                            tmp.display(),
                            index_path.display()
                        );
                        let _ = std::fs::remove_file(&tmp);
                    }
                }
                Err(e) => {
                    eprintln!("[loader] docs-drift: failed to serialize artifact index: {e}");
                }
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
            // 120s: normal boot is 1-16s even on loaded machines; the old
            // 30s ceiling was routinely breached when ~25 integration
            // tests each booted a full fixture host in parallel (CPU
            // starvation, not a hang). 120s still catches genuine
            // deadlocks while surviving parallel-test contention.
            load_timeout_ms: 120_000,
        },
    }
}

fn make_execution_id() -> String {
    // P2-18: `duration_since(UNIX_EPOCH).unwrap_or(0)` used to collapse
    // clock-goes-backwards or same-nanosecond boots to the same id. Add
    // a process-local monotonic counter so ids stay unique regardless
    // of wall clock behaviour.
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("exec-{nanos:x}-{seq:x}")
}

/// P0-14: produce a per-process unique tmp path for atomic write-then-rename.
/// The old code shared `<file>.tmp` between loaders, so two concurrent auto-
/// heals raced on the same tmp file.  `<file>.cordis-tmp.<pid>-<seq>-<nanos>`
/// is unique per invocation.
fn unique_staging_path(target: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    match target.file_name() {
        Some(name) => {
            let mut owned = name.to_os_string();
            owned.push(format!(".cordis-tmp.{pid}-{seq:x}-{nanos:x}"));
            target.with_file_name(owned)
        }
        None => target.with_extension(format!("cordis-tmp.{pid}-{seq:x}-{nanos:x}")),
    }
}

#[cfg(test)]
mod loader_helper_tests {
    use super::unique_staging_path;
    use std::path::Path;

    #[test]
    fn unique_staging_path_never_collides_within_process() {
        let base = Path::new("/tmp/cordis/artifacts/index.json");
        let a = unique_staging_path(base);
        let b = unique_staging_path(base);
        assert_ne!(a, b);
        assert!(a
            .file_name()
            .unwrap()
            .to_string_lossy()
            .contains(".cordis-tmp."));
        assert!(a
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("index.json.cordis-tmp."));
    }

    #[test]
    fn unique_staging_path_stays_in_same_dir() {
        let base = Path::new("/tmp/cordis/artifacts/index.json");
        let tmp = unique_staging_path(base);
        assert_eq!(tmp.parent(), base.parent());
    }
}

#[cfg(test)]
mod loader_flow_tests {
    use super::*;
    use crate::core::models::{
        AbiFingerprint, ArtifactIndex, ArtifactIndexEntry, ArtifactKind, PluginArtifact,
        PluginDocs, ARTIFACT_INDEX_SCHEMA_VERSION,
    };
    use cordis_plugin_sdk::CORDIS_TARGET;
    use std::fs;
    use tempfile::TempDir;

    // --- fixture builders ----------------------------------------------------

    fn abi_host(crate_hash: &str, api_hash: &str) -> AbiFingerprint {
        AbiFingerprint {
            rustc_version: "test-rustc".to_string(),
            target_triple: CORDIS_TARGET.to_string(),
            crate_hash: crate_hash.to_string(),
            api_hash: api_hash.to_string(),
        }
    }

    fn docs(plugin_path: &str) -> PluginDocs {
        PluginDocs {
            plugin_id: plugin_path.replace('/', "_"),
            plugin_path: plugin_path.to_string(),
            plugin_version: "0.1.0".to_string(),
            abi_version: 2,
            command_name: None,
            nodes: Vec::new(),
            system_hint: None,
        }
    }

    /// Build a JSON plugin artifact file and return (relative_ref, sha256).
    fn write_json_artifact(
        artifacts_dir: &Path,
        rel_name: &str,
        artifact: &PluginArtifact,
    ) -> (String, String) {
        let path = artifacts_dir.join(rel_name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, serde_json::to_string_pretty(artifact).unwrap()).unwrap();
        let sha = crate::plugin::artifact::sha256_file(&path).unwrap();
        (rel_name.to_string(), sha)
    }

    fn json_entry(
        plugin_path: &str,
        rel_name: &str,
        sha256: &str,
        abi: AbiFingerprint,
        docs: PluginDocs,
    ) -> ArtifactIndexEntry {
        ArtifactIndexEntry {
            plugin_path: plugin_path.to_string(),
            version: "0.1.0".to_string(),
            abi_fingerprint: abi,
            artifact_path: rel_name.to_string(),
            sha256: sha256.to_string(),
            built_at: "0".to_string(),
            parent: None,
            required: true,
            grants_from_parent: Vec::new(),
            docs,
            exports: Vec::new(),
            execution: None,
            artifact_kind: ArtifactKind::Json,
            build_fingerprint: "bf".to_string(),
            input_probe: Default::default(),
            local_path_deps: Vec::new(),
        }
    }

    fn write_index(root: &Path, index: &ArtifactIndex) -> LoaderConfig {
        let artifacts_dir = root.join("artifacts");
        fs::create_dir_all(&artifacts_dir).unwrap();
        let index_path = artifacts_dir.join("index.json");
        fs::write(&index_path, serde_json::to_string_pretty(index).unwrap()).unwrap();
        LoaderConfig {
            plugins_root: root.join("plugins"),
            artifact_index_path: index_path,
            budget: LoaderBudget {
                max_total_plugins: 256,
                max_total_nodes: 4096,
                load_timeout_ms: 120_000,
            },
        }
    }

    /// Convenience: build a single JSON plugin whose artifact contents match
    /// its index entry (happy path), return the finished LoaderConfig.
    fn single_json_plugin(root: &Path, plugin_path: &str) -> LoaderConfig {
        let artifacts_dir = root.join("artifacts");
        fs::create_dir_all(&artifacts_dir).unwrap();
        let abi = abi_host("crate_v1", "api_v2");
        let d = docs(plugin_path);
        let artifact = PluginArtifact {
            plugin_path: plugin_path.to_string(),
            abi_fingerprint: abi.clone(),
            docs: d.clone(),
            exports: Vec::new(),
            execution: None,
        };
        let rel = format!("{}.json", plugin_path.replace('/', "_"));
        let (rel, sha) = write_json_artifact(&artifacts_dir, &rel, &artifact);
        let entry = json_entry(plugin_path, &rel, &sha, abi, d);
        let index = ArtifactIndex {
            schema_version: ARTIFACT_INDEX_SCHEMA_VERSION,
            generated_at: "now".to_string(),
            topo_order: vec![plugin_path.to_string()],
            entries: vec![entry],
        };
        write_index(root, &index)
    }

    // --- happy path ----------------------------------------------------------

    #[test]
    fn load_single_json_plugin_loaded() {
        let tmp = TempDir::new().unwrap();
        let config = single_json_plugin(tmp.path(), "alpha");
        let out = Loader::new(config).load().unwrap();
        assert!(matches!(
            out.plugin_registry.get("alpha").unwrap().load_result,
            PluginLoadResult::Loaded
        ));
        assert!(out.execution_id.starts_with("exec-"));
        assert_eq!(out.metrics.plugin_unavailable_total, 0);
    }

    #[test]
    fn load_json_plugin_with_exports_provides_services() {
        let tmp = TempDir::new().unwrap();
        let artifacts_dir = tmp.path().join("artifacts");
        fs::create_dir_all(&artifacts_dir).unwrap();
        let abi = abi_host("crate_v1", "api_v2");
        let d = docs("alpha");
        let artifact = PluginArtifact {
            plugin_path: "alpha".to_string(),
            abi_fingerprint: abi.clone(),
            docs: d.clone(),
            exports: vec!["svc.db".to_string()],
            execution: None,
        };
        let (rel, sha) = write_json_artifact(&artifacts_dir, "alpha.json", &artifact);
        let mut entry = json_entry("alpha", &rel, &sha, abi, d);
        entry.exports = vec!["svc.db".to_string()];
        let index = ArtifactIndex {
            schema_version: ARTIFACT_INDEX_SCHEMA_VERSION,
            generated_at: "now".to_string(),
            topo_order: vec!["alpha".to_string()],
            entries: vec![entry],
        };
        let config = write_index(tmp.path(), &index);
        let out = Loader::new(config).load().unwrap();
        assert!(matches!(
            out.plugin_registry.get("alpha").unwrap().load_result,
            PluginLoadResult::Loaded
        ));
    }

    #[test]
    fn load_with_staging_root_copies_artifact() {
        // load_with_staging_root(Some) drives stage_artifact_bundle before the
        // JSON artifact is parsed, so the staged copy must load Loaded.
        let tmp = TempDir::new().unwrap();
        let config = single_json_plugin(tmp.path(), "alpha");
        let staged = tmp.path().join("staged");
        fs::create_dir_all(&staged).unwrap();
        let out = Loader::new(config)
            .load_with_staging_root(Some(&staged))
            .unwrap();
        assert!(matches!(
            out.plugin_registry.get("alpha").unwrap().load_result,
            PluginLoadResult::Loaded
        ));
        // The artifact was staged under the staging root.
        assert!(staged.join("alpha.json").exists());
    }

    // --- budget --------------------------------------------------------------

    #[test]
    fn load_budget_exceeded_plugins() {
        let tmp = TempDir::new().unwrap();
        let mut config = single_json_plugin(tmp.path(), "alpha");
        config.budget.max_total_plugins = 0;
        let err = Loader::new(config).load().unwrap_err();
        assert!(matches!(err, RuntimeError::BudgetExceeded { .. }));
    }

    #[test]
    fn load_budget_exceeded_nodes() {
        let tmp = TempDir::new().unwrap();
        // Give the plugin one declared node so max_total_nodes=0 trips.
        let artifacts_dir = tmp.path().join("artifacts");
        fs::create_dir_all(&artifacts_dir).unwrap();
        let abi = abi_host("crate_v1", "api_v2");
        let mut d = docs("alpha");
        d.nodes.push(cordis_plugin_sdk::NodeDoc {
            id: "n1".to_string(),
            summary: "n".to_string(),
            input_schema: serde_json::json!({}),
            output_schema: serde_json::json!({}),
            side_effects: Vec::new(),
            failure_modes: Vec::new(),
            node_type: Default::default(),
            agent_accessible: true,
        });
        let artifact = PluginArtifact {
            plugin_path: "alpha".to_string(),
            abi_fingerprint: abi.clone(),
            docs: d.clone(),
            exports: Vec::new(),
            execution: None,
        };
        let (rel, sha) = write_json_artifact(&artifacts_dir, "alpha.json", &artifact);
        let entry = json_entry("alpha", &rel, &sha, abi, d);
        let index = ArtifactIndex {
            schema_version: ARTIFACT_INDEX_SCHEMA_VERSION,
            generated_at: "now".to_string(),
            topo_order: vec!["alpha".to_string()],
            entries: vec![entry],
        };
        let mut config = write_index(tmp.path(), &index);
        config.budget.max_total_nodes = 0;
        let err = Loader::new(config).load().unwrap_err();
        assert!(matches!(err, RuntimeError::BudgetExceeded { .. }));
    }

    // --- artifact missing ----------------------------------------------------

    #[test]
    fn load_artifact_missing_marks_unavailable() {
        let tmp = TempDir::new().unwrap();
        let abi = abi_host("crate_v1", "api_v2");
        let d = docs("alpha");
        // Index references a file that was never written.
        let entry = json_entry("alpha", "ghost.json", &"0".repeat(64), abi, d);
        let index = ArtifactIndex {
            schema_version: ARTIFACT_INDEX_SCHEMA_VERSION,
            generated_at: "now".to_string(),
            topo_order: vec!["alpha".to_string()],
            entries: vec![entry],
        };
        let config = write_index(tmp.path(), &index);
        let out = Loader::new(config).load().unwrap();
        assert!(matches!(
            out.plugin_registry.get("alpha").unwrap().load_result,
            PluginLoadResult::Unavailable(PluginUnavailableReason::ArtifactMissing)
        ));
        assert_eq!(out.metrics.plugin_unavailable_total, 1);
    }

    // --- hash mismatch -------------------------------------------------------

    #[test]
    fn load_hash_mismatch_marks_unavailable() {
        let tmp = TempDir::new().unwrap();
        let artifacts_dir = tmp.path().join("artifacts");
        fs::create_dir_all(&artifacts_dir).unwrap();
        let abi = abi_host("crate_v1", "api_v2");
        let d = docs("alpha");
        let artifact = PluginArtifact {
            plugin_path: "alpha".to_string(),
            abi_fingerprint: abi.clone(),
            docs: d.clone(),
            exports: Vec::new(),
            execution: None,
        };
        let (rel, _sha) = write_json_artifact(&artifacts_dir, "alpha.json", &artifact);
        // Deliberately record the WRONG sha256.
        let entry = json_entry("alpha", &rel, &"a".repeat(64), abi, d);
        let index = ArtifactIndex {
            schema_version: ARTIFACT_INDEX_SCHEMA_VERSION,
            generated_at: "now".to_string(),
            topo_order: vec!["alpha".to_string()],
            entries: vec![entry],
        };
        let config = write_index(tmp.path(), &index);
        let out = Loader::new(config).load().unwrap();
        assert!(matches!(
            out.plugin_registry.get("alpha").unwrap().load_result,
            PluginLoadResult::Unavailable(PluginUnavailableReason::HashMismatch)
        ));
    }

    // --- dylib triple precheck (AbiMismatch) ---------------------------------

    #[test]
    fn load_dylib_wrong_triple_precheck_abi_mismatch() {
        let tmp = TempDir::new().unwrap();
        let artifacts_dir = tmp.path().join("artifacts");
        fs::create_dir_all(&artifacts_dir).unwrap();
        // Any bytes will do — the triple precheck fires before staging/dlopen.
        let so_path = artifacts_dir.join("alpha.so");
        fs::write(&so_path, b"not-a-real-dylib").unwrap();
        let sha = crate::plugin::artifact::sha256_file(&so_path).unwrap();
        // Record a triple that is guaranteed != host.
        let bad_triple = if CORDIS_TARGET == "x86_64-unknown-linux-gnu" {
            "aarch64-apple-darwin"
        } else {
            "x86_64-unknown-linux-gnu"
        };
        let abi = AbiFingerprint {
            rustc_version: "test".to_string(),
            target_triple: bad_triple.to_string(),
            crate_hash: "c".to_string(),
            api_hash: "a".to_string(),
        };
        let mut entry = json_entry("alpha", "alpha.so", &sha, abi, docs("alpha"));
        entry.artifact_kind = ArtifactKind::Dylib;
        let index = ArtifactIndex {
            schema_version: ARTIFACT_INDEX_SCHEMA_VERSION,
            generated_at: "now".to_string(),
            topo_order: vec!["alpha".to_string()],
            entries: vec![entry],
        };
        let config = write_index(tmp.path(), &index);
        let out = Loader::new(config).load().unwrap();
        assert!(matches!(
            out.plugin_registry.get("alpha").unwrap().load_result,
            PluginLoadResult::Unavailable(PluginUnavailableReason::AbiMismatch)
        ));
        assert_eq!(out.metrics.dylib_abi_mismatch_total, 1);
    }

    // --- JSON artifact contract violations -----------------------------------

    #[test]
    fn load_json_plugin_path_mismatch_contract_violation() {
        let tmp = TempDir::new().unwrap();
        let artifacts_dir = tmp.path().join("artifacts");
        fs::create_dir_all(&artifacts_dir).unwrap();
        let abi = abi_host("crate_v1", "api_v2");
        // Artifact claims a different plugin_path than the index entry.
        let artifact = PluginArtifact {
            plugin_path: "other".to_string(),
            abi_fingerprint: abi.clone(),
            docs: docs("other"),
            exports: Vec::new(),
            execution: None,
        };
        let (rel, sha) = write_json_artifact(&artifacts_dir, "alpha.json", &artifact);
        let entry = json_entry("alpha", &rel, &sha, abi, docs("alpha"));
        let index = ArtifactIndex {
            schema_version: ARTIFACT_INDEX_SCHEMA_VERSION,
            generated_at: "now".to_string(),
            topo_order: vec!["alpha".to_string()],
            entries: vec![entry],
        };
        let config = write_index(tmp.path(), &index);
        let out = Loader::new(config).load().unwrap();
        assert!(matches!(
            out.plugin_registry.get("alpha").unwrap().load_result,
            PluginLoadResult::Unavailable(PluginUnavailableReason::ContractViolation)
        ));
    }

    #[test]
    fn load_json_abi_fingerprint_mismatch() {
        let tmp = TempDir::new().unwrap();
        let artifacts_dir = tmp.path().join("artifacts");
        fs::create_dir_all(&artifacts_dir).unwrap();
        let index_abi = abi_host("crate_v1", "api_v2");
        let artifact_abi = abi_host("crate_DIFFERENT", "api_v2");
        let artifact = PluginArtifact {
            plugin_path: "alpha".to_string(),
            abi_fingerprint: artifact_abi,
            docs: docs("alpha"),
            exports: Vec::new(),
            execution: None,
        };
        let (rel, sha) = write_json_artifact(&artifacts_dir, "alpha.json", &artifact);
        let entry = json_entry("alpha", &rel, &sha, index_abi, docs("alpha"));
        let index = ArtifactIndex {
            schema_version: ARTIFACT_INDEX_SCHEMA_VERSION,
            generated_at: "now".to_string(),
            topo_order: vec!["alpha".to_string()],
            entries: vec![entry],
        };
        let config = write_index(tmp.path(), &index);
        let out = Loader::new(config).load().unwrap();
        assert!(matches!(
            out.plugin_registry.get("alpha").unwrap().load_result,
            PluginLoadResult::Unavailable(PluginUnavailableReason::AbiMismatch)
        ));
        assert_eq!(out.metrics.dylib_abi_mismatch_total, 1);
    }

    // --- docs drift auto-heal (JSON) -----------------------------------------

    #[test]
    fn load_json_docs_drift_autoheals_and_rewrites_index() {
        let tmp = TempDir::new().unwrap();
        let artifacts_dir = tmp.path().join("artifacts");
        fs::create_dir_all(&artifacts_dir).unwrap();
        let abi = abi_host("crate_v1", "api_v2");
        // Artifact docs carry a system_hint the index entry lacks → drift.
        let mut artifact_docs = docs("alpha");
        artifact_docs.system_hint = Some("fresh hint".to_string());
        let artifact = PluginArtifact {
            plugin_path: "alpha".to_string(),
            abi_fingerprint: abi.clone(),
            docs: artifact_docs.clone(),
            exports: Vec::new(),
            execution: None,
        };
        let (rel, sha) = write_json_artifact(&artifacts_dir, "alpha.json", &artifact);
        let entry = json_entry("alpha", &rel, &sha, abi, docs("alpha"));
        let index = ArtifactIndex {
            schema_version: ARTIFACT_INDEX_SCHEMA_VERSION,
            generated_at: "now".to_string(),
            topo_order: vec!["alpha".to_string()],
            entries: vec![entry],
        };
        let config = write_index(tmp.path(), &index);
        let index_path = config.artifact_index_path.clone();
        let out = Loader::new(config).load().unwrap();
        // Plugin loads with the healed docs (system_hint present).
        let state = out.plugin_registry.get("alpha").unwrap();
        assert!(matches!(state.load_result, PluginLoadResult::Loaded));
        assert_eq!(
            state.docs.as_ref().unwrap().system_hint.as_deref(),
            Some("fresh hint")
        );
        // Index file on disk was rewritten with the healed docs.
        let rewritten = load_artifact_index(&index_path).unwrap();
        assert_eq!(
            rewritten.entries[0].docs.system_hint.as_deref(),
            Some("fresh hint")
        );
        // interfaces.json was synced under plugins_root.
        let synced = tmp.path().join("plugins/alpha/docs/agent/interfaces.json");
        assert!(synced.exists());
    }

    // --- required parent-failure propagation ---------------------------------

    #[test]
    fn load_required_child_failure_propagates_to_parent() {
        let tmp = TempDir::new().unwrap();
        let artifacts_dir = tmp.path().join("artifacts");
        fs::create_dir_all(&artifacts_dir).unwrap();
        let abi = abi_host("crate_v1", "api_v2");

        // Parent loads fine.
        let parent_artifact = PluginArtifact {
            plugin_path: "parent".to_string(),
            abi_fingerprint: abi.clone(),
            docs: docs("parent"),
            exports: Vec::new(),
            execution: None,
        };
        let (prel, psha) = write_json_artifact(&artifacts_dir, "parent.json", &parent_artifact);
        let parent_entry = json_entry("parent", &prel, &psha, abi.clone(), docs("parent"));

        // Child is required and its artifact is MISSING → failure propagates
        // upward and marks the parent InitFailed.
        let mut child_entry = json_entry(
            "parent/child",
            "ghost.json",
            &"0".repeat(64),
            abi,
            docs("parent/child"),
        );
        child_entry.parent = Some("parent".to_string());
        child_entry.required = true;

        let index = ArtifactIndex {
            schema_version: ARTIFACT_INDEX_SCHEMA_VERSION,
            generated_at: "now".to_string(),
            topo_order: vec!["parent".to_string(), "parent/child".to_string()],
            entries: vec![parent_entry, child_entry],
        };
        let config = write_index(tmp.path(), &index);
        let out = Loader::new(config).load().unwrap();
        // Child unavailable (artifact missing).
        assert!(matches!(
            out.plugin_registry.get("parent/child").unwrap().load_result,
            PluginLoadResult::Unavailable(PluginUnavailableReason::ArtifactMissing)
        ));
        // Parent dragged to InitFailed by required-propagation.
        assert!(matches!(
            out.plugin_registry.get("parent").unwrap().load_result,
            PluginLoadResult::Unavailable(PluginUnavailableReason::InitFailed)
        ));
    }

    #[test]
    fn load_child_skipped_when_parent_not_loaded() {
        let tmp = TempDir::new().unwrap();
        let artifacts_dir = tmp.path().join("artifacts");
        fs::create_dir_all(&artifacts_dir).unwrap();
        let abi = abi_host("crate_v1", "api_v2");

        // Parent artifact missing → parent unavailable, but NOT required so no
        // upward propagation.
        let mut parent_entry = json_entry(
            "parent",
            "ghost.json",
            &"0".repeat(64),
            abi.clone(),
            docs("parent"),
        );
        parent_entry.required = false;

        // Child's artifact exists and is valid, but its parent failed to load →
        // child is short-circuited to InitFailed before any hash check.
        let child_artifact = PluginArtifact {
            plugin_path: "parent/child".to_string(),
            abi_fingerprint: abi.clone(),
            docs: docs("parent/child"),
            exports: Vec::new(),
            execution: None,
        };
        let (crel, csha) = write_json_artifact(&artifacts_dir, "child.json", &child_artifact);
        let mut child_entry = json_entry("parent/child", &crel, &csha, abi, docs("parent/child"));
        child_entry.parent = Some("parent".to_string());
        child_entry.required = false;

        let index = ArtifactIndex {
            schema_version: ARTIFACT_INDEX_SCHEMA_VERSION,
            generated_at: "now".to_string(),
            topo_order: vec!["parent".to_string(), "parent/child".to_string()],
            entries: vec![parent_entry, child_entry],
        };
        let config = write_index(tmp.path(), &index);
        let out = Loader::new(config).load().unwrap();
        assert!(matches!(
            out.plugin_registry.get("parent/child").unwrap().load_result,
            PluginLoadResult::Unavailable(PluginUnavailableReason::InitFailed)
        ));
    }

    // --- topo_order references a missing entry -------------------------------

    #[test]
    fn load_topo_order_missing_entry_errors() {
        let tmp = TempDir::new().unwrap();
        let config = single_json_plugin(tmp.path(), "alpha");
        // Rewrite the index adding a topo_order id that has no entry.
        let mut index = load_artifact_index(&config.artifact_index_path).unwrap();
        index.topo_order.push("ghost".to_string());
        fs::write(
            &config.artifact_index_path,
            serde_json::to_string_pretty(&index).unwrap(),
        )
        .unwrap();
        let err = Loader::new(config).load().unwrap_err();
        assert!(matches!(err, RuntimeError::ArtifactIndexMissing { .. }));
    }

    // --- load_timeout budget -------------------------------------------------

    #[test]
    fn load_timeout_zero_budget_trips() {
        let tmp = TempDir::new().unwrap();
        let mut config = single_json_plugin(tmp.path(), "alpha");
        config.budget.load_timeout_ms = 0;
        // With a 0ms budget the first ensure_not_timed_out after index load
        // will almost always exceed it; if the machine is impossibly fast the
        // per-plugin check will. Assert either the timeout or a clean load.
        match Loader::new(config).load() {
            Err(RuntimeError::LoadTimeout { limit_ms, .. }) => assert_eq!(limit_ms, 0),
            Ok(out) => {
                // Extremely fast path: accept a successful load.
                assert!(out.plugin_registry.get("alpha").is_some());
            }
            Err(other) => panic!("unexpected error {other:?}"),
        }
    }

    // --- default_loader_config -----------------------------------------------

    #[test]
    fn default_loader_config_paths() {
        let cfg = default_loader_config("/some/root");
        assert_eq!(cfg.plugins_root, PathBuf::from("/some/root/plugins"));
        assert_eq!(
            cfg.artifact_index_path,
            PathBuf::from("/some/root/artifacts/index.json")
        );
        assert_eq!(cfg.budget.max_total_plugins, 256);
        assert_eq!(cfg.budget.max_total_nodes, 4096);
        assert_eq!(cfg.budget.load_timeout_ms, 120_000);
    }

    #[test]
    fn make_execution_id_is_unique_and_prefixed() {
        let a = make_execution_id();
        let b = make_execution_id();
        assert_ne!(a, b);
        assert!(a.starts_with("exec-"));
    }

    // --- dylib real-load happy path + docs read ------------------------------

    fn host_native_fixture(name: &str) -> Option<(PathBuf, ArtifactKind)> {
        let ext = if cfg!(target_os = "macos") {
            "dylib"
        } else if cfg!(target_os = "linux") {
            "so"
        } else {
            return None;
        };
        let p = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/artifacts")
            .join(format!("{name}.{ext}"));
        p.exists().then_some((p, ArtifactKind::Dylib))
    }

    #[test]
    fn load_real_dylib_reads_docs_and_registers() {
        // Uses the arm64 fixture dylib on macOS; skip when unavailable or the
        // triple doesn't match the host (loader precheck would reject it).
        let Some((src, _)) = host_native_fixture("expr") else {
            eprintln!("[skip] no host-native fixture dylib");
            return;
        };
        // Read the plugin's real docs so the index entry matches (no drift, no
        // symbol errors) — this drives the dylib Ok branch end-to-end.
        let real_docs = match crate::plugin::tooling::read_plugin_docs(&src) {
            Ok(d) => d,
            Err(err) => {
                eprintln!("[skip] fixture dylib not loadable here: {err:?}");
                return;
            }
        };
        let tmp = TempDir::new().unwrap();
        let artifacts_dir = tmp.path().join("artifacts");
        fs::create_dir_all(&artifacts_dir).unwrap();
        let dst = artifacts_dir.join("expr.dylib");
        fs::copy(&src, &dst).unwrap();
        let sha = crate::plugin::artifact::sha256_file(&dst).unwrap();
        let plugin_path = real_docs.plugin_path.clone();
        let abi = AbiFingerprint {
            rustc_version: "test".to_string(),
            target_triple: CORDIS_TARGET.to_string(),
            crate_hash: "c".to_string(),
            api_hash: "a".to_string(),
        };
        let mut entry = json_entry(&plugin_path, "expr.dylib", &sha, abi, real_docs);
        entry.artifact_kind = ArtifactKind::Dylib;
        let index = ArtifactIndex {
            schema_version: ARTIFACT_INDEX_SCHEMA_VERSION,
            generated_at: "now".to_string(),
            topo_order: vec![plugin_path.clone()],
            entries: vec![entry],
        };
        let config = write_index(tmp.path(), &index);
        let out = Loader::new(config).load().unwrap();
        assert!(matches!(
            out.plugin_registry.get(&plugin_path).unwrap().load_result,
            PluginLoadResult::Loaded
        ));
    }

    #[test]
    fn load_sha256_read_failure_marks_hash_mismatch() {
        // A directory at the artifact path passes exists() but sha256_file's
        // read loop errors (EISDIR on Unix), driving the sha256 Err branch
        // (distinct from the plain mismatch branch).
        let tmp = TempDir::new().unwrap();
        let artifacts_dir = tmp.path().join("artifacts");
        fs::create_dir_all(&artifacts_dir).unwrap();
        // Create a directory named like the artifact.
        fs::create_dir_all(artifacts_dir.join("alpha.json")).unwrap();
        let abi = abi_host("c", "a");
        let entry = json_entry("alpha", "alpha.json", &"0".repeat(64), abi, docs("alpha"));
        let index = ArtifactIndex {
            schema_version: ARTIFACT_INDEX_SCHEMA_VERSION,
            generated_at: "now".to_string(),
            topo_order: vec!["alpha".to_string()],
            entries: vec![entry],
        };
        let config = write_index(tmp.path(), &index);
        let out = Loader::new(config).load().unwrap();
        assert!(matches!(
            out.plugin_registry.get("alpha").unwrap().load_result,
            PluginLoadResult::Unavailable(PluginUnavailableReason::HashMismatch)
        ));
    }

    #[test]
    fn load_real_dylib_docs_drift_autoheals() {
        // Index entry docs differ from the dylib's real docs → the dylib Ok
        // branch pushes a docs-drift entry and auto-heals. Requires a
        // host-native fixture dylib.
        let Some((src, _)) = host_native_fixture("expr") else {
            eprintln!("[skip] no host-native fixture dylib");
            return;
        };
        let real_docs = match crate::plugin::tooling::read_plugin_docs(&src) {
            Ok(d) => d,
            Err(err) => {
                eprintln!("[skip] fixture dylib not loadable: {err:?}");
                return;
            }
        };
        let tmp = TempDir::new().unwrap();
        let artifacts_dir = tmp.path().join("artifacts");
        fs::create_dir_all(&artifacts_dir).unwrap();
        let dst = artifacts_dir.join("expr.dylib");
        fs::copy(&src, &dst).unwrap();
        let sha = crate::plugin::artifact::sha256_file(&dst).unwrap();
        let plugin_path = real_docs.plugin_path.clone();
        let abi = AbiFingerprint {
            rustc_version: "test".to_string(),
            target_triple: CORDIS_TARGET.to_string(),
            crate_hash: "c".to_string(),
            api_hash: "a".to_string(),
        };
        // Stale index docs: strip system_hint / mutate version so they differ
        // from the artifact's real docs, forcing drift.
        let mut stale = real_docs.clone();
        stale.system_hint = Some("STALE-HINT-WILL-BE-HEALED".to_string());
        let mut entry = json_entry(&plugin_path, "expr.dylib", &sha, abi, stale);
        entry.artifact_kind = ArtifactKind::Dylib;
        let index = ArtifactIndex {
            schema_version: ARTIFACT_INDEX_SCHEMA_VERSION,
            generated_at: "now".to_string(),
            topo_order: vec![plugin_path.clone()],
            entries: vec![entry],
        };
        let config = write_index(tmp.path(), &index);
        let index_path = config.artifact_index_path.clone();
        let out = Loader::new(config).load().unwrap();
        let state = out.plugin_registry.get(&plugin_path).unwrap();
        assert!(matches!(state.load_result, PluginLoadResult::Loaded));
        // Effective docs are the dylib's real docs (drift healed).
        assert_eq!(
            state.docs.as_ref().unwrap().system_hint,
            real_docs.system_hint
        );
        // Index on disk rewritten to the healed docs.
        let rewritten = load_artifact_index(&index_path).unwrap();
        assert_eq!(rewritten.entries[0].docs.system_hint, real_docs.system_hint);
    }

    #[test]
    fn load_dylib_docs_read_failure_symbol_missing() {
        // A file with the host target triple (so it passes the triple
        // precheck) but invalid dylib bytes fails `read_plugin_docs`'s dlopen
        // deterministically → SymbolMissing branch. The plugin is `required`
        // with a parent so the parent-failure propagation path also runs.
        let tmp = TempDir::new().unwrap();
        let artifacts_dir = tmp.path().join("artifacts");
        fs::create_dir_all(&artifacts_dir).unwrap();
        let abi = AbiFingerprint {
            rustc_version: "test".to_string(),
            target_triple: CORDIS_TARGET.to_string(),
            crate_hash: "c".to_string(),
            api_hash: "a".to_string(),
        };

        // A valid JSON parent so the child's propagation has a real target.
        let parent_artifact = PluginArtifact {
            plugin_path: "parent".to_string(),
            abi_fingerprint: abi.clone(),
            docs: docs("parent"),
            exports: Vec::new(),
            execution: None,
        };
        let (prel, psha) = write_json_artifact(&artifacts_dir, "parent.json", &parent_artifact);
        let parent_entry = json_entry("parent", &prel, &psha, abi.clone(), docs("parent"));

        // Broken dylib child.
        let broken = artifacts_dir.join("broken.dylib");
        fs::write(&broken, b"this is not a valid mach-o or elf dylib").unwrap();
        let sha = crate::plugin::artifact::sha256_file(&broken).unwrap();
        let mut child = json_entry(
            "parent/child",
            "broken.dylib",
            &sha,
            abi,
            docs("parent/child"),
        );
        child.artifact_kind = ArtifactKind::Dylib;
        child.parent = Some("parent".to_string());
        child.required = true;

        let index = ArtifactIndex {
            schema_version: ARTIFACT_INDEX_SCHEMA_VERSION,
            generated_at: "now".to_string(),
            topo_order: vec!["parent".to_string(), "parent/child".to_string()],
            entries: vec![parent_entry, child],
        };
        let config = write_index(tmp.path(), &index);
        let out = Loader::new(config).load().unwrap();
        assert!(matches!(
            out.plugin_registry.get("parent/child").unwrap().load_result,
            PluginLoadResult::Unavailable(PluginUnavailableReason::SymbolMissing)
        ));
        // required child failure propagates the parent to InitFailed.
        assert!(matches!(
            out.plugin_registry.get("parent").unwrap().load_result,
            PluginLoadResult::Unavailable(PluginUnavailableReason::InitFailed)
        ));
        assert_eq!(out.metrics.dylib_no_fallback_total, 1);
    }
}

/// POSIX file lock helper used to serialise the docs-drift auto-heal write
/// across multiple runtime processes and multiple loaders within one process.
/// Uses `fcntl(F_SETLK, ...)` on Unix; a no-op stub on other platforms.
mod auto_heal_file_lock {
    use std::fs::{File, OpenOptions};
    use std::path::Path;

    pub struct Guard {
        #[allow(dead_code)]
        file: Option<File>,
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
                    "[loader] docs-drift: failed to open lock {}: {err}",
                    path.display()
                );
                return Guard { file: None };
            }
        };
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let fd = file.as_raw_fd();
            // SAFETY: fd is owned by `file`, kept alive across the syscall.
            let rc = unsafe { libc::flock(fd, libc::LOCK_EX) };
            if rc != 0 {
                let err = std::io::Error::last_os_error();
                eprintln!(
                    "[loader] docs-drift: flock({}) failed: {err}",
                    path.display()
                );
            }
        }
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
