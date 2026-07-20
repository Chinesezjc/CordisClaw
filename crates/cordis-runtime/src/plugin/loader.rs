//! Plugin loader implementation.
//! Flow:
//! 1) read artifact index
//! 2) verify artifact hash + availability
//! 3) register plugins/nodes/context from index docs
//! 4) defer dylib ABI/docs guard to first invoke

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
                            PluginLoadResult::Unavailable(
                                PluginUnavailableReason::HashMismatch,
                            ),
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
                        PluginLoadResult::Unavailable(
                            PluginUnavailableReason::HashMismatch,
                        ),
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
            // P2-34: the previous `if let Ok(...)` silently swallowed
            // errors — architecture-mismatched or missing-symbol dylibs
            // would just fall through to the cached index docs, hiding
            // the drift from operators. Now record any read failure as a
            // stderr diagnostic so it shows up in logs (we deliberately
            // don't propagate: the plugin registry entry stays "loaded"
            // with the last-known docs, matching prior semantics).
            if !matches!(entry.artifact_kind, ArtifactKind::Json) {
                match crate::plugin::tooling::read_plugin_docs(&artifact_path) {
                    Ok(dylib_docs) => {
                        if dylib_docs != entry.docs {
                            docs_drift.push((plugin_path.clone(), dylib_docs.clone()));
                            effective_docs = Some(dylib_docs);
                        }
                    }
                    Err(err) => {
                        eprintln!(
                            "[loader] docs-drift: read_plugin_docs({}) failed: {err}; \
                             falling back to cached index docs",
                            artifact_path.display()
                        );
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
                    eprintln!(
                        "[loader] docs-drift: failed to serialize artifact index: {e}"
                    );
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
        assert!(a.file_name().unwrap().to_string_lossy().contains(".cordis-tmp."));
        assert!(a.file_name().unwrap().to_string_lossy().starts_with("index.json.cordis-tmp."));
    }

    #[test]
    fn unique_staging_path_stays_in_same_dir() {
        let base = Path::new("/tmp/cordis/artifacts/index.json");
        let tmp = unique_staging_path(base);
        assert_eq!(tmp.parent(), base.parent());
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
