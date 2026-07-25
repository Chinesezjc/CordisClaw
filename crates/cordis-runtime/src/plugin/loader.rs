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
                // The serialize/write/rename outcome and all its `eprintln!`
                // logging live in `heal_write_pretty` so the (unreachable-in-
                // production) serialize-failure arm is covered by a direct unit
                // test rather than an uncoverable arm in `load`. A `true` return
                // means the write failed at the tmp stage → skip to the next
                // drift item, matching the original `continue`.
                if heal_write_pretty(new_docs, &docs_path, HealTarget::Docs { plugin_path }) {
                    continue;
                }
            }
            // Atomic write-back of the artifact index.
            heal_write_pretty(&index, index_path, HealTarget::Index);
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

/// Failure of the write-to-staging-then-rename sequence used by the docs-drift
/// auto-heal. Carries the staging path so the (site-specific) caller can log a
/// message identical to the original inline handling.
#[derive(Debug)]
enum AtomicWriteError {
    Write { tmp: PathBuf, err: std::io::Error },
    Rename { tmp: PathBuf, err: std::io::Error },
}

/// Write `contents` to a per-invocation staging sibling of `target`, then
/// atomically rename it over `target`. On a rename failure the staging file is
/// removed so no partial artifact is left behind. Extracted from the two
/// near-identical inline blocks in the docs-drift auto-heal so the
/// staging/rename/cleanup sequence is unit-testable; callers keep their own
/// `eprintln!` so the log text stays byte-for-byte identical.
fn atomic_write_via_staging(target: &Path, contents: &str) -> Result<(), AtomicWriteError> {
    let tmp = unique_staging_path(target);
    if let Err(err) = std::fs::write(&tmp, contents) {
        return Err(AtomicWriteError::Write { tmp, err });
    }
    if let Err(err) = std::fs::rename(&tmp, target) {
        let _ = std::fs::remove_file(&tmp);
        return Err(AtomicWriteError::Rename { tmp, err });
    }
    Ok(())
}

/// Which docs-drift auto-heal write is being logged. Selects the exact log
/// wording so the two call sites keep byte-for-byte identical messages after
/// the logic was hoisted out of `load`.
enum HealTarget<'a> {
    Docs { plugin_path: &'a str },
    Index,
}

/// Serialize `value` as pretty JSON and atomically write it to `target`,
/// emitting the docs-drift auto-heal log lines for every outcome.
///
/// Returns `true` only when the write failed at the tmp (staging) stage — the
/// caller uses that to `continue` to the next drift item, matching the original
/// inline control flow. All other outcomes (success, serialize failure, rename
/// failure) return `false`.
///
/// Hoisted out of `load` so the serialize-failure arm — unreachable in
/// production because `PluginDocs` / the artifact index always serialize — is
/// exercisable by a direct unit test that feeds a value whose `Serialize` impl
/// fails (a map with non-string keys).
fn heal_write_pretty<T: serde::Serialize>(value: &T, target: &Path, which: HealTarget<'_>) -> bool {
    let json = match serde_json::to_string_pretty(value) {
        Ok(json) => json,
        Err(e) => {
            match which {
                HealTarget::Docs { plugin_path } => eprintln!(
                    "[loader] docs-drift: failed to serialize docs for {plugin_path}: {e}"
                ),
                HealTarget::Index => {
                    eprintln!("[loader] docs-drift: failed to serialize artifact index: {e}")
                }
            }
            return false;
        }
    };
    match atomic_write_via_staging(target, &json) {
        Ok(()) => {
            match which {
                HealTarget::Docs { plugin_path } => eprintln!(
                    "[loader] docs-drift: auto-healed {plugin_path} — \
                     interfaces.json refreshed from artifact"
                ),
                HealTarget::Index => {}
            }
            false
        }
        Err(AtomicWriteError::Write { tmp, err }) => {
            match which {
                HealTarget::Docs { .. } => eprintln!(
                    "[loader] docs-drift: failed to write tmp {}: {err}",
                    tmp.display()
                ),
                HealTarget::Index => eprintln!(
                    "[loader] docs-drift: failed to write index tmp {}: {err}",
                    tmp.display()
                ),
            }
            // Only the docs-item write failure drives a `continue`; the index
            // write is the last statement in the block, so its return value is
            // ignored either way. Signal the tmp-stage failure uniformly.
            true
        }
        Err(AtomicWriteError::Rename { tmp, err }) => {
            match which {
                HealTarget::Docs { .. } => eprintln!(
                    "[loader] docs-drift: failed to rename {} → {}: {err}",
                    tmp.display(),
                    target.display()
                ),
                HealTarget::Index => eprintln!(
                    "[loader] docs-drift: failed to rename index {} → {}: {err}",
                    tmp.display(),
                    target.display()
                ),
            }
            false
        }
    }
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
    use super::{
        atomic_write_via_staging, heal_write_pretty, unique_staging_path, AtomicWriteError,
        HealTarget,
    };
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    /// A value whose `Serialize` impl fails: `serde_json` cannot encode a map
    /// with non-string keys, so `to_string_pretty` returns Err. Used to drive
    /// the serialize-failure arm that `PluginDocs` / the artifact index can
    /// never reach in production.
    fn unserializable() -> BTreeMap<Vec<u8>, i32> {
        let mut m = BTreeMap::new();
        m.insert(vec![1u8, 2, 3], 7);
        m
    }

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

    // A target with no file_name (`/`) exercises the `None` arm, which falls
    // back to `with_extension`. `with_extension` on a path with no file name is
    // a no-op, so the returned path equals the input — the point is that the
    // fallback arm is taken without panicking.
    #[test]
    fn unique_staging_path_no_filename_takes_extension_fallback() {
        let out = unique_staging_path(Path::new("/"));
        assert_eq!(out, PathBuf::from("/"));
    }

    // atomic_write_via_staging happy path: writes contents to target via a
    // staging sibling that is renamed into place; no staging file is left
    // behind.
    #[test]
    fn atomic_write_via_staging_writes_and_cleans_staging() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("out.json");
        atomic_write_via_staging(&target, "{\"k\":1}").expect("write ok");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "{\"k\":1}");
        // No .cordis-tmp. staging leftovers next to the target.
        let leftover: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".cordis-tmp."))
            .collect();
        assert!(leftover.is_empty(), "staging leftover: {leftover:?}");
    }

    // Write-arm failure: the staging file cannot be created because a path
    // component under the target is a regular file → the Write variant carries
    // the staging path.
    #[test]
    fn atomic_write_via_staging_write_failure_reports_tmp() {
        let tmp = tempfile::TempDir::new().unwrap();
        // A regular file where a parent directory would need to be.
        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let target = blocker.join("sub/out.json");
        let result = atomic_write_via_staging(&target, "{}");
        assert!(
            matches!(&result, Err(AtomicWriteError::Write { tmp, .. }) if tmp.to_string_lossy().contains(".cordis-tmp.")),
            "expected Write error, got {result:?}"
        );
    }

    // Rename-arm failure: the target already exists AS A DIRECTORY, so the
    // rename onto it fails (EISDIR); the staging file is cleaned up.
    #[test]
    fn atomic_write_via_staging_rename_failure_cleans_up() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("target_is_dir");
        std::fs::create_dir_all(&target).unwrap();
        let result = atomic_write_via_staging(&target, "{}");
        assert!(
            matches!(&result, Err(AtomicWriteError::Rename { tmp: staged, .. }) if !staged.exists()),
            "expected Rename error (staging file must be removed on rename failure), got {result:?}"
        );
        // No stray staging leftovers.
        let leftover: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".cordis-tmp."))
            .collect();
        assert!(leftover.is_empty(), "staging leftover: {leftover:?}");
    }

    // heal_write_pretty happy path (Docs): serializes and writes; returns false
    // (no tmp-stage failure) and the file lands on disk.
    #[test]
    fn heal_write_pretty_docs_ok_writes_and_returns_false() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("interfaces.json");
        let value = serde_json::json!({"plugin_path": "p"});
        let skip = heal_write_pretty(&value, &target, HealTarget::Docs { plugin_path: "p" });
        assert!(!skip, "successful write must not signal skip");
        assert!(target.exists());
    }

    // heal_write_pretty index happy path: writes and returns false.
    #[test]
    fn heal_write_pretty_index_ok_writes_and_returns_false() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("index.json");
        let value = serde_json::json!({"entries": []});
        let skip = heal_write_pretty(&value, &target, HealTarget::Index);
        assert!(!skip);
        assert!(target.exists());
    }

    // Serialize-failure arm (Docs): an unserializable value returns false (not a
    // tmp-stage failure) and never touches disk. This is the production-
    // unreachable arm the extraction exists to cover.
    #[test]
    fn heal_write_pretty_docs_serialize_failure_returns_false() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("interfaces.json");
        let skip = heal_write_pretty(
            &unserializable(),
            &target,
            HealTarget::Docs { plugin_path: "p" },
        );
        assert!(!skip, "serialize failure is not a tmp-stage skip");
        assert!(
            !target.exists(),
            "nothing should be written on serialize failure"
        );
    }

    // Serialize-failure arm (Index): same, via the Index log branch.
    #[test]
    fn heal_write_pretty_index_serialize_failure_returns_false() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("index.json");
        let skip = heal_write_pretty(&unserializable(), &target, HealTarget::Index);
        assert!(!skip);
        assert!(!target.exists());
    }

    // Write-stage failure (Docs): a blocked staging path makes the tmp write
    // fail → returns true (signals the caller to `continue`).
    #[test]
    fn heal_write_pretty_docs_write_failure_returns_true() {
        let tmp = tempfile::TempDir::new().unwrap();
        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let target = blocker.join("sub/interfaces.json");
        let value = serde_json::json!({"k": 1});
        let skip = heal_write_pretty(&value, &target, HealTarget::Docs { plugin_path: "p" });
        assert!(skip, "tmp write failure must signal skip");
    }

    // Write-stage failure (Index): same blocked-path setup through the Index
    // log branch; the return value is ignored by the caller but must be true.
    #[test]
    fn heal_write_pretty_index_write_failure_returns_true() {
        let tmp = tempfile::TempDir::new().unwrap();
        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let target = blocker.join("sub/index.json");
        let value = serde_json::json!({"entries": []});
        let skip = heal_write_pretty(&value, &target, HealTarget::Index);
        assert!(skip);
    }

    // Rename-stage failure (Docs): target exists as a directory → rename fails;
    // returns false (rename failure does not drive a skip).
    #[test]
    fn heal_write_pretty_docs_rename_failure_returns_false() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("interfaces_dir");
        std::fs::create_dir_all(&target).unwrap();
        let value = serde_json::json!({"k": 1});
        let skip = heal_write_pretty(&value, &target, HealTarget::Docs { plugin_path: "p" });
        assert!(!skip, "rename failure is not a tmp-stage skip");
    }

    // Rename-stage failure (Index): same, through the Index log branch.
    #[test]
    fn heal_write_pretty_index_rename_failure_returns_false() {
        let tmp = tempfile::TempDir::new().unwrap();
        let target = tmp.path().join("index_dir");
        std::fs::create_dir_all(&target).unwrap();
        let value = serde_json::json!({"entries": []});
        let skip = heal_write_pretty(&value, &target, HealTarget::Index);
        assert!(!skip);
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

    // A single plugin declaring the SAME export twice makes the second
    // `context.provide(Local, ...)` return `DuplicateService`, which the
    // export loop propagates via `?` straight out of `load` (line ~454).
    #[test]
    fn load_duplicate_export_propagates_duplicate_service() {
        let tmp = TempDir::new().unwrap();
        let artifacts_dir = tmp.path().join("artifacts");
        fs::create_dir_all(&artifacts_dir).unwrap();
        let abi = abi_host("crate_v1", "api_v2");
        let d = docs("alpha");
        let artifact = PluginArtifact {
            plugin_path: "alpha".to_string(),
            abi_fingerprint: abi.clone(),
            docs: d.clone(),
            exports: vec!["svc.db".to_string(), "svc.db".to_string()],
            execution: None,
        };
        let (rel, sha) = write_json_artifact(&artifacts_dir, "alpha.json", &artifact);
        let mut entry = json_entry("alpha", &rel, &sha, abi, d);
        // Duplicate export id in the same local scope → DuplicateService.
        entry.exports = vec!["svc.db".to_string(), "svc.db".to_string()];
        let index = ArtifactIndex {
            schema_version: ARTIFACT_INDEX_SCHEMA_VERSION,
            generated_at: "now".to_string(),
            topo_order: vec!["alpha".to_string()],
            entries: vec![entry],
        };
        let config = write_index(tmp.path(), &index);
        let err = Loader::new(config).load().unwrap_err();
        assert!(
            matches!(err, RuntimeError::DuplicateService { .. }),
            "err={err:?}"
        );
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

    // --- staging error propagation (line ~305) -------------------------------

    // `load_with_staging_root(Some(root))` runs `stage_artifact_bundle` before
    // the artifact is parsed. If the staging destination cannot be created
    // (a path component is a regular file), `stage_file`'s `create_dir_all`
    // fails and the `?` propagates the Io error straight out of `load`.
    #[test]
    fn load_with_staging_root_staging_failure_propagates() {
        let tmp = TempDir::new().unwrap();
        let config = single_json_plugin(tmp.path(), "alpha");
        // A regular file where the staging root's parent directory must be, so
        // create_dir_all(<file>/alpha ...) fails with NotADirectory.
        let blocker = tmp.path().join("blocker");
        fs::write(&blocker, b"x").unwrap();
        let staged_root = blocker.join("staged");
        let err = Loader::new(config)
            .load_with_staging_root(Some(&staged_root))
            .unwrap_err();
        assert!(matches!(err, RuntimeError::Io { .. }), "err={err:?}");
    }

    // --- docs-drift auto-heal: interfaces.json FS failure arms ---------------

    /// Build a single JSON plugin that WILL drift (artifact docs carry a
    /// system_hint the index entry lacks) and return the finished config with
    /// `plugins_root` overridden. The drift makes the loader attempt to sync
    /// interfaces.json under `plugins_root`.
    fn drifting_json_plugin_with_plugins_root(
        root: &Path,
        override_plugins_root: PathBuf,
    ) -> LoaderConfig {
        let artifacts_dir = root.join("artifacts");
        fs::create_dir_all(&artifacts_dir).unwrap();
        let abi = abi_host("crate_v1", "api_v2");
        let mut artifact_docs = docs("alpha");
        artifact_docs.system_hint = Some("fresh".to_string());
        let artifact = PluginArtifact {
            plugin_path: "alpha".to_string(),
            abi_fingerprint: abi.clone(),
            docs: artifact_docs,
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
        let index_path = artifacts_dir.join("index.json");
        fs::write(&index_path, serde_json::to_string_pretty(&index).unwrap()).unwrap();
        LoaderConfig {
            plugins_root: override_plugins_root,
            artifact_index_path: index_path,
            budget: LoaderBudget {
                max_total_plugins: 256,
                max_total_nodes: 4096,
                load_timeout_ms: 120_000,
            },
        }
    }

    // The interfaces.json parent directory cannot be created because a path
    // component under plugins_root is a regular file. The auto-heal logs and
    // `continue`s (create_dir_all error arm); the plugin still loads with the
    // healed docs and the index is still rewritten.
    #[test]
    fn load_docs_drift_interfaces_dir_create_failure_is_logged_and_skipped() {
        let tmp = TempDir::new().unwrap();
        // plugins_root sits under a regular file → create_dir_all fails.
        let blocker = tmp.path().join("blocker");
        fs::write(&blocker, b"x").unwrap();
        let config = drifting_json_plugin_with_plugins_root(tmp.path(), blocker.join("plugins"));
        let index_path = config.artifact_index_path.clone();
        // Load must still succeed: interface sync failure is non-fatal.
        let out = Loader::new(config).load().unwrap();
        let state = out.plugin_registry.get("alpha").unwrap();
        assert!(matches!(state.load_result, PluginLoadResult::Loaded));
        assert_eq!(
            state.docs.as_ref().unwrap().system_hint.as_deref(),
            Some("fresh")
        );
        // Index on disk WAS still rewritten with the healed docs.
        let rewritten = load_artifact_index(&index_path).unwrap();
        assert_eq!(
            rewritten.entries[0].docs.system_hint.as_deref(),
            Some("fresh")
        );
    }

    // The interfaces.json rename fails because the destination path already
    // exists as a DIRECTORY. create_dir_all(parent) and the tmp write both
    // succeed, but rename(tmp -> interfaces.json/) is EISDIR → the rename error
    // arm runs, logs, and removes the tmp file. Load still succeeds.
    #[test]
    fn load_docs_drift_interfaces_rename_failure_is_logged_and_cleaned() {
        let tmp = TempDir::new().unwrap();
        let plugins_root = tmp.path().join("plugins");
        // Pre-create interfaces.json AS A DIRECTORY so rename onto it fails.
        let iface_as_dir = plugins_root.join("alpha/docs/agent/interfaces.json");
        fs::create_dir_all(&iface_as_dir).unwrap();
        let config = drifting_json_plugin_with_plugins_root(tmp.path(), plugins_root.clone());
        let out = Loader::new(config).load().unwrap();
        assert!(matches!(
            out.plugin_registry.get("alpha").unwrap().load_result,
            PluginLoadResult::Loaded
        ));
        // interfaces.json is still the directory (rename never replaced it) and
        // no stray tmp staging file was left behind next to it.
        assert!(iface_as_dir.is_dir());
        let agent_dir = plugins_root.join("alpha/docs/agent");
        let leftover: Vec<_> = fs::read_dir(&agent_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".cordis-tmp."))
            .collect();
        assert!(leftover.is_empty(), "tmp staging file was not cleaned up");
    }

    // ensure_not_timed_out returns LoadTimeout when the elapsed time since
    // `started_at` exceeds the configured budget. A 0ms budget plus a small
    // real sleep makes `elapsed_ms > 0` hold deterministically, driving the
    // error arm directly (the full-load `load_timeout_zero_budget_trips` test
    // can race to a clean load on an impossibly fast host; this exercises the
    // arm without that dependency).
    #[test]
    fn ensure_not_timed_out_trips_on_zero_budget() {
        let tmp = TempDir::new().unwrap();
        let mut config = single_json_plugin(tmp.path(), "alpha");
        config.budget.load_timeout_ms = 0;
        let loader = Loader::new(config);
        let started = Instant::now();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let err = loader
            .ensure_not_timed_out(started)
            .expect_err("0ms budget after a 2ms sleep must trip");
        assert!(
            matches!(&err, RuntimeError::LoadTimeout { limit_ms, elapsed_ms } if *limit_ms == 0 && *elapsed_ms > 0),
            "wrong variant: {err:?}"
        );
    }

    // docs-drift auto-heal, interfaces.json staging WRITE failure arm
    // (AtomicWriteError::Write). The interfaces.json parent dir
    // (`.../docs/agent`) is pre-created read-only, so `create_dir_all(parent)`
    // returns Ok (it already exists) but writing the `.cordis-tmp.` staging
    // sibling into a read-only dir fails → the Write arm logs and `continue`s.
    // The index write-back (to the still-writable artifacts dir) still runs.
    #[cfg(unix)]
    #[test]
    fn load_docs_drift_interfaces_staging_write_failure_is_logged_and_skipped() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let plugins_root = tmp.path().join("plugins");
        // Pre-create the interfaces.json parent dir and make it read-only so the
        // staging-file write (not create_dir_all) is what fails.
        let agent_dir = plugins_root.join("alpha/docs/agent");
        fs::create_dir_all(&agent_dir).unwrap();
        fs::set_permissions(&agent_dir, fs::Permissions::from_mode(0o555)).unwrap();

        let config = drifting_json_plugin_with_plugins_root(tmp.path(), plugins_root.clone());
        let index_path = config.artifact_index_path.clone();
        let out = Loader::new(config).load().unwrap();
        // Load still succeeds with healed docs despite the interfaces sync failure.
        let state = out.plugin_registry.get("alpha").unwrap();
        assert!(matches!(state.load_result, PluginLoadResult::Loaded));
        assert_eq!(
            state.docs.as_ref().unwrap().system_hint.as_deref(),
            Some("fresh")
        );
        // The index on disk WAS still rewritten with the healed docs.
        let rewritten = load_artifact_index(&index_path).unwrap();
        assert_eq!(
            rewritten.entries[0].docs.system_hint.as_deref(),
            Some("fresh")
        );
        // No staging leftover in the read-only dir.
        fs::set_permissions(&agent_dir, fs::Permissions::from_mode(0o755)).unwrap();
        let leftover: Vec<_> = fs::read_dir(&agent_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".cordis-tmp."))
            .collect();
        assert!(leftover.is_empty(), "tmp staging file was not cleaned up");
    }

    // docs-drift auto-heal, artifact-index write-back staging WRITE failure arm
    // (AtomicWriteError::Write, lines 532-537). The artifacts dir holding
    // index.json is made read-only, so the interfaces sync under the writable
    // plugins_root succeeds (Ok arm) but the index write-back's `.cordis-tmp.`
    // staging write into the read-only artifacts dir fails → the index Write arm
    // logs. Load still succeeds (interface + index sync are both non-fatal).
    #[cfg(unix)]
    #[test]
    fn load_docs_drift_index_writeback_staging_write_failure_is_logged() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let plugins_root = tmp.path().join("plugins");
        let config = drifting_json_plugin_with_plugins_root(tmp.path(), plugins_root.clone());
        let index_path = config.artifact_index_path.clone();
        let artifacts_dir = index_path.parent().unwrap().to_path_buf();
        // Make the artifacts dir read-only so the index write-back staging write
        // fails while the interfaces sync (under plugins_root) still succeeds.
        fs::set_permissions(&artifacts_dir, fs::Permissions::from_mode(0o555)).unwrap();

        let out = Loader::new(config).load().unwrap();
        let state = out.plugin_registry.get("alpha").unwrap();
        assert!(matches!(state.load_result, PluginLoadResult::Loaded));
        assert_eq!(
            state.docs.as_ref().unwrap().system_hint.as_deref(),
            Some("fresh")
        );
        // Interfaces.json under the writable plugins_root WAS healed.
        let iface = plugins_root.join("alpha/docs/agent/interfaces.json");
        assert!(iface.exists(), "interfaces.json should have been written");
        // The on-disk index write-back failed, so index.json is unchanged (still
        // lacks the healed system_hint).
        fs::set_permissions(&artifacts_dir, fs::Permissions::from_mode(0o755)).unwrap();
        let on_disk = load_artifact_index(&index_path).unwrap();
        assert_eq!(on_disk.entries[0].docs.system_hint, None);
        // No staging leftover next to the index.
        let leftover: Vec<_> = fs::read_dir(&artifacts_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".cordis-tmp."))
            .collect();
        assert!(
            leftover.is_empty(),
            "index tmp staging file was not cleaned up"
        );
    }

    // --- propagate_parent_failure break arms ---------------------------------

    // A required child whose parent edge is NOT required: propagation marks the
    // (non-required) parent unavailable, then stops at the `else { break }` arm
    // rather than climbing further. A grandparent above the non-required parent
    // must therefore stay Loaded.
    #[test]
    fn propagate_stops_at_non_required_parent() {
        let tmp = TempDir::new().unwrap();
        let artifacts_dir = tmp.path().join("artifacts");
        fs::create_dir_all(&artifacts_dir).unwrap();
        let abi = abi_host("crate_v1", "api_v2");

        // Grandparent "gp" loads fine and is required.
        let gp_art = PluginArtifact {
            plugin_path: "gp".to_string(),
            abi_fingerprint: abi.clone(),
            docs: docs("gp"),
            exports: Vec::new(),
            execution: None,
        };
        let (grel, gsha) = write_json_artifact(&artifacts_dir, "gp.json", &gp_art);
        let gp_entry = json_entry("gp", &grel, &gsha, abi.clone(), docs("gp"));

        // Parent "gp/p" loads fine but its edge to gp is NOT required.
        let p_art = PluginArtifact {
            plugin_path: "gp/p".to_string(),
            abi_fingerprint: abi.clone(),
            docs: docs("gp/p"),
            exports: Vec::new(),
            execution: None,
        };
        let (prel, psha) = write_json_artifact(&artifacts_dir, "p.json", &p_art);
        let mut p_entry = json_entry("gp/p", &prel, &psha, abi.clone(), docs("gp/p"));
        p_entry.parent = Some("gp".to_string());
        p_entry.required = false;

        // Child "gp/p/c" is required and its artifact is missing → failure
        // propagates to parent "gp/p", which is non-required → break.
        let mut c_entry = json_entry("gp/p/c", "ghost.json", &"0".repeat(64), abi, docs("gp/p/c"));
        c_entry.parent = Some("gp/p".to_string());
        c_entry.required = true;

        let index = ArtifactIndex {
            schema_version: ARTIFACT_INDEX_SCHEMA_VERSION,
            generated_at: "now".to_string(),
            topo_order: vec!["gp".to_string(), "gp/p".to_string(), "gp/p/c".to_string()],
            entries: vec![gp_entry, p_entry, c_entry],
        };
        let config = write_index(tmp.path(), &index);
        let out = Loader::new(config).load().unwrap();
        // Child failed, parent dragged to InitFailed...
        assert!(matches!(
            out.plugin_registry.get("gp/p/c").unwrap().load_result,
            PluginLoadResult::Unavailable(PluginUnavailableReason::ArtifactMissing)
        ));
        assert!(matches!(
            out.plugin_registry.get("gp/p").unwrap().load_result,
            PluginLoadResult::Unavailable(PluginUnavailableReason::InitFailed)
        ));
        // ...but grandparent stays Loaded because propagation broke at the
        // non-required parent.
        assert!(matches!(
            out.plugin_registry.get("gp").unwrap().load_result,
            PluginLoadResult::Loaded
        ));
    }

    // A required plugin whose declared parent is NOT present in the index:
    // propagation calls `mark_unavailable` (a no-op for the unknown parent)
    // then hits the `let Some(parent) = entries.get(...) else { break }` arm.
    // The load still completes.
    #[test]
    fn propagate_breaks_on_dangling_parent() {
        let tmp = TempDir::new().unwrap();
        let abi = abi_host("crate_v1", "api_v2");
        // Required plugin with a missing artifact AND a parent that has no
        // index entry ("ghost_parent").
        let mut entry = json_entry("orphan", "ghost.json", &"0".repeat(64), abi, docs("orphan"));
        entry.parent = Some("ghost_parent".to_string());
        entry.required = true;
        let index = ArtifactIndex {
            schema_version: ARTIFACT_INDEX_SCHEMA_VERSION,
            generated_at: "now".to_string(),
            // Only "orphan" is in topo_order/entries; "ghost_parent" is not.
            topo_order: vec!["orphan".to_string()],
            entries: vec![entry],
        };
        let config = write_index(tmp.path(), &index);
        let out = Loader::new(config).load().unwrap();
        assert!(matches!(
            out.plugin_registry.get("orphan").unwrap().load_result,
            PluginLoadResult::Unavailable(PluginUnavailableReason::ArtifactMissing)
        ));
        // The dangling parent was never registered.
        assert!(out.plugin_registry.get("ghost_parent").is_none());
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
