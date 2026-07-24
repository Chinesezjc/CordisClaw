use crate::core::error::RuntimeError;
use crate::core::models::{
    ArtifactIndex, ArtifactIndexEntry, ArtifactKind, InputProbe, InputProbeFile, PluginArtifact,
    PluginDocs, PluginExecution, ARTIFACT_INDEX_SCHEMA_VERSION,
};
use crate::plugin::artifact::{
    artifact_index_map, load_artifact_index, load_plugin_artifact, resolve_artifact_path,
    sha256_file,
};
use crate::plugin::dynamic::{is_dylib_path, LoadedDylibApi};
use crate::plugin::package::{PackageResolver, ResolvedPlugin, ResolvedPluginGraph};
use cordis_plugin_sdk::pretty_json;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::env::consts::{DLL_EXTENSION, DLL_PREFIX};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const BUILD_LOCK_FILE: &str = ".artifacts-build.lock";
const STALE_LOCK_TIMEOUT: Duration = Duration::from_secs(300);
const LEGACY_STALE_LOCK_TIMEOUT: Duration = Duration::from_secs(30);
const BUILD_LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrepareMode {
    Incremental,
    Full,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrepareArtifactsReport {
    pub rebuilt: Vec<(String, String)>,
    pub reused: Vec<String>,
    pub full_rebuild: bool,
}

#[derive(Debug, Deserialize)]
struct PluginManifestToml {
    package: Option<PluginManifestPackage>,
    lib: Option<PluginManifestLib>,
}

#[derive(Debug, Deserialize)]
struct PluginManifestPackage {
    name: String,
    version: String,
    metadata: Option<PluginManifestMetadata>,
}

#[derive(Debug, Deserialize)]
struct PluginManifestMetadata {
    cordis: Option<PluginManifestCordis>,
}

#[derive(Debug, Deserialize)]
struct PluginManifestCordis {
    artifact: Option<SourceArtifactConfig>,
}

#[derive(Debug, Deserialize, Default)]
struct PluginManifestLib {
    #[serde(rename = "crate-type", default)]
    crate_type: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct SourceArtifactConfig {
    #[serde(default)]
    exports: Vec<String>,
    #[serde(default)]
    execution: Option<PluginExecution>,
}

#[derive(Debug, Clone)]
struct PluginBuildSpec {
    package_name: String,
    version: String,
    is_dylib: bool,
    artifact: SourceArtifactConfig,
}

#[derive(Debug, Clone)]
struct PluginBuildContext {
    plugin: ResolvedPlugin,
    build_spec: PluginBuildSpec,
    artifact_name: String,
    artifact_path: PathBuf,
    artifact_kind: ArtifactKind,
    local_path_deps: Vec<String>,
    input_files: Vec<PathBuf>,
    input_probe: InputProbe,
    build_fingerprint: Option<String>,
    dirty: bool,
}

#[derive(Debug, Clone)]
struct DependencySnapshot {
    workspace_manifest_path: PathBuf,
    workspace_members: HashSet<String>,
    target_directory: PathBuf,
    local_dep_closure_by_name: HashMap<String, Vec<PathBuf>>,
}

#[derive(Debug, Deserialize)]
struct CargoMetadataOutput {
    packages: Vec<CargoMetadataPackage>,
    workspace_members: Vec<String>,
    target_directory: String,
    resolve: Option<CargoMetadataResolve>,
}

#[derive(Debug, Clone, Deserialize)]
struct CargoMetadataPackage {
    id: String,
    name: String,
    manifest_path: String,
    source: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CargoMetadataResolve {
    nodes: Vec<CargoMetadataNode>,
}

#[derive(Debug, Deserialize)]
struct CargoMetadataNode {
    id: String,
    dependencies: Vec<String>,
}

#[derive(Debug)]
struct ArtifactBuildLock {
    path: PathBuf,
}

#[derive(Debug, Serialize, Deserialize)]
struct ArtifactBuildLockState {
    pid: u32,
    created_at_ms: u128,
}

impl Drop for ArtifactBuildLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub fn ensure_fixture_artifacts(fixtures_root: &Path) -> Result<bool, RuntimeError> {
    let report = prepare_artifacts(fixtures_root, PrepareMode::Incremental)?;
    Ok(!report.rebuilt.is_empty())
}

pub fn prepare_artifacts(
    fixtures_root: &Path,
    mode: PrepareMode,
) -> Result<PrepareArtifactsReport, RuntimeError> {
    let fixtures_root = absolute_path(fixtures_root)?;
    if !can_prepare_fixture_artifacts(&fixtures_root) {
        if matches!(mode, PrepareMode::Full) {
            return Err(RuntimeError::Invariant {
                message: format!(
                    "fixture rebuild requires repo sources next to {}",
                    fixtures_root.display()
                ),
            });
        }
        return Ok(PrepareArtifactsReport::default());
    }

    let _lock = ArtifactBuildLock::acquire(&fixtures_root)?;
    prepare_artifacts_locked(&fixtures_root, mode)
}

pub fn rebuild_fixture_artifacts(
    fixtures_root: &Path,
) -> Result<Vec<(String, String)>, RuntimeError> {
    Ok(prepare_artifacts(fixtures_root, PrepareMode::Full)?.rebuilt)
}

pub fn rebuild_plugin_workspace(
    workspace_root: &Path,
    plugin_path: &str,
) -> Result<Vec<(String, String)>, RuntimeError> {
    // "/" means all plugins; "/qq" means just qq.
    let name = plugin_path.trim_start_matches('/');
    if name.is_empty() {
        return rebuild_fixture_artifacts(workspace_root);
    }
    let mut cmd = std::process::Command::new("cargo");
    cmd.arg("build")
        .arg("--manifest-path")
        .arg(workspace_root.join("plugins").join("Cargo.toml"))
        .arg("-p")
        .arg(name);
    // P2-29: strip HTTP proxy env vars so cargo doesn't try to
    // contact a corporate proxy for the offline / local-path deps that
    // fixtures use. `run_command` (the other cargo path in this file)
    // already does this; the imbalance was itself the bug.
    strip_proxy_envs(&mut cmd);
    // P2-30: enforce a build timeout so an infinite-loop `build.rs`
    // can't hang the whole iteration pipeline. 20 minutes is generous
    // for a from-cold fixture rebuild; anything longer is almost
    // certainly a bug. Override via CORDIS_BUILD_TIMEOUT_SECS.
    let timeout_secs = std::env::var("CORDIS_BUILD_TIMEOUT_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(20 * 60);
    let output = run_command_with_timeout(cmd, std::time::Duration::from_secs(timeout_secs))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(RuntimeError::InvalidArgument {
            message: format!("cargo build -p {name} failed: {stderr}"),
        });
    }
    // P0-9: use platform-native dylib prefix/extension. Previously hardcoded
    // `lib{name}.so` / `{name}.so` — always wrong on macOS (`.dylib`) and
    // Windows (`.dll`), so `iterate_plugins` was un-runnable on any dev
    // machine that isn't Linux. `std::env::consts` gives us the right
    // affixes for the current target triple.
    let target_dir = workspace_root.join("plugins").join("target").join("debug");
    let src_filename = format!(
        "{}{}{}",
        std::env::consts::DLL_PREFIX,
        name.replace('-', "_"),
        std::env::consts::DLL_SUFFIX
    );
    let dst_filename = format!("{}{}", name, std::env::consts::DLL_SUFFIX);
    let src = target_dir.join(&src_filename);
    let dst = workspace_root.join("artifacts").join(&dst_filename);
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).map_err(|e| RuntimeError::Io {
            path: parent.to_path_buf(),
            message: format!("create artifacts dir: {e}"),
        })?;
    }
    // Stage-then-swap: `fs::copy` over a live-mmap'd `.so` risks SIGSEGV on
    // concurrent readers. Write to `<dst>.cordis-tmp`, fsync, rename over
    // the target — the OS unlinks the old file while any existing mapping
    // remains valid until unmap.
    stage_then_rename_file(&src, &dst)?;

    // Post-rebuild: refresh `index.json` sha256 for the freshly-staged artifact
    // (and, if present, `LoadedDylibApi::open`-derived docs so the same rebuild
    // won't be flagged as `HashMismatch` by the loader on next verify pass).
    // Without this, the E2E `iterate_plugins` path builds a candidate `.so`
    // whose bytes have moved but whose sha256 in `index.json` still reflects
    // the *previous* build; the loader's P0-13 sha check then rejects the
    // fresh artifact as unavailable, forcing a hard rollback of code that
    // actually passed tests. See docs/architecture/status-and-open-items.md
    // section 5.2.21 for the observation from the E2E smoke.
    let artifact_index_path = workspace_root.join("artifacts").join("index.json");
    if artifact_index_path.exists() {
        let mut index = load_artifact_index(&artifact_index_path)?;
        let entry_name = name.to_string();
        let mut updated = false;
        for entry in &mut index.entries {
            if entry.plugin_path == entry_name {
                let resolved = resolve_artifact_path(&artifact_index_path, &entry.artifact_path);
                let new_hash = sha256_file(&resolved)?;
                if entry.sha256 != new_hash {
                    entry.sha256 = new_hash;
                    updated = true;
                }
                break;
            }
        }
        if updated {
            index.generated_at = current_build_marker();
            write_pretty_json(&artifact_index_path, &index)?;
        }
    }

    Ok(vec![(
        name.to_string(),
        format!("{} -> {}", src.display(), dst.display()),
    )])
}

/// Copy `src` -> `dst` atomically: write to `<dst>.cordis-tmp`, fsync,
/// rename. On Unix this replaces the file inode; any pre-existing dylib
/// mapping stays valid until munmap while new opens see the fresh bytes.
///
/// (C batch, P0-8 / P0-15 style helper — shared between rebuild_plugin_workspace
/// and materialize_artifact_entry so both stage-swap identically.)
pub(crate) fn stage_then_rename_file(src: &Path, dst: &Path) -> Result<(), RuntimeError> {
    use std::io::Read;
    use std::io::Write;
    let mut src_file = std::fs::File::open(src).map_err(|e| RuntimeError::Io {
        path: src.to_path_buf(),
        message: format!("open source: {e}"),
    })?;
    let mut buf = Vec::new();
    src_file
        .read_to_end(&mut buf)
        .map_err(|e| RuntimeError::Io {
            path: src.to_path_buf(),
            message: format!("read source: {e}"),
        })?;
    let tmp = match dst.file_name() {
        Some(name) => {
            let mut owned = name.to_os_string();
            owned.push(".cordis-tmp");
            dst.with_file_name(owned)
        }
        None => {
            return Err(RuntimeError::Io {
                path: dst.to_path_buf(),
                message: "artifact target has no filename".to_string(),
            });
        }
    };
    {
        let mut tmp_file = std::fs::File::create(&tmp).map_err(|e| RuntimeError::Io {
            path: tmp.clone(),
            message: format!("create tmp: {e}"),
        })?;
        tmp_file.write_all(&buf).map_err(|e| RuntimeError::Io {
            path: tmp.clone(),
            message: format!("write tmp: {e}"),
        })?;
        tmp_file.sync_all().map_err(|e| RuntimeError::Io {
            path: tmp.clone(),
            message: format!("sync tmp: {e}"),
        })?;
        // Preserve executable permissions from the source (dylibs are +x on
        // most platforms; without this the OS refuses to dlopen).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Ok(meta) = src_file.metadata() {
                let mode = meta.permissions().mode();
                let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode));
            }
        }
    }
    std::fs::rename(&tmp, dst).map_err(|e| RuntimeError::Io {
        path: dst.to_path_buf(),
        message: format!("rename {} -> {} failed: {e}", tmp.display(), dst.display()),
    })?;
    Ok(())
}

pub fn sync_plugin_docs(fixtures_root: &Path) -> Result<Vec<PathBuf>, RuntimeError> {
    let fixtures_root = absolute_path(fixtures_root)?;
    let plugins_root = fixtures_root.join("plugins");
    // Defence: fixtures_root must contain a `plugins/` directory with a
    // Cargo workspace manifest.  If `fixtures_root` is already pointing
    // inside `plugins/`, reject it so we don't create nested paths like
    // `fixtures/plugins/plugins/qq/docs/`.
    if !plugins_root.join("Cargo.toml").exists() {
        return Err(RuntimeError::Invariant {
            message: format!(
                "plugins workspace not found at {}; \
                 fixtures_root must be the project fixtures/ directory, \
                 not the plugins/ subdirectory",
                plugins_root.display()
            ),
        });
    }
    let artifact_index_path = fixtures_root.join("artifacts/index.json");
    let index = load_artifact_index(&artifact_index_path)?;

    let mut written = Vec::new();
    for entry in index.entries {
        let docs_path = plugins_root
            .join(
                entry
                    .plugin_path
                    .replace('/', std::path::MAIN_SEPARATOR_STR),
            )
            .join("docs/agent/interfaces.json");
        let docs_dir = docs_path.parent().ok_or_else(|| RuntimeError::Invariant {
            message: format!("docs path missing parent: {}", docs_path.display()),
        })?;
        fs::create_dir_all(docs_dir).map_err(|e| RuntimeError::Io {
            path: docs_dir.to_path_buf(),
            message: e.to_string(),
        })?;
        // P1-17: durable atomic write for interfaces.json.
        write_pretty_json(&docs_path, &entry.docs)?;
        written.push(docs_path);
    }

    Ok(written)
}

pub fn refresh_artifact_index(fixtures_root: &Path) -> Result<Vec<(String, String)>, RuntimeError> {
    let fixtures_root = absolute_path(fixtures_root)?;
    let artifact_index_path = fixtures_root.join("artifacts/index.json");
    let mut index = load_artifact_index(&artifact_index_path)?;
    let mut refreshed = Vec::new();

    for entry in &mut index.entries {
        let artifact_path = resolve_artifact_path(&artifact_index_path, &entry.artifact_path);
        let hash = sha256_file(&artifact_path)?;
        entry.sha256 = hash.clone();
        refreshed.push((entry.plugin_path.clone(), hash));
    }
    index.generated_at = current_build_marker();

    // P1-17: durable atomic write. A torn artifact index blocks the next
    // verify pass entirely.
    write_pretty_json(&artifact_index_path, &index)?;

    Ok(refreshed)
}

pub fn read_plugin_docs(artifact_path: &Path) -> Result<PluginDocs, RuntimeError> {
    if is_dylib_path(artifact_path) {
        let dylib = LoadedDylibApi::open(artifact_path)?;
        serde_json::from_str(&(dylib.api().docs)().payload).map_err(|e| RuntimeError::Io {
            path: artifact_path.to_path_buf(),
            message: format!("runtime docs parse failed: {e}"),
        })
    } else {
        let artifact = load_plugin_artifact(artifact_path)?;
        Ok(artifact.docs)
    }
}

fn prepare_artifacts_locked(
    fixtures_root: &Path,
    mode: PrepareMode,
) -> Result<PrepareArtifactsReport, RuntimeError> {
    let repo_root = fixtures_root
        .parent()
        .ok_or_else(|| RuntimeError::Invariant {
            message: format!("fixtures root missing parent: {}", fixtures_root.display()),
        })?;
    let plugins_root = fixtures_root.join("plugins");
    let artifacts_dir = fixtures_root.join("artifacts");
    let artifact_index_path = artifacts_dir.join("index.json");
    let graph = PackageResolver::new(&plugins_root).resolve()?;
    let dependency_snapshot = DependencySnapshot::load(&plugins_root)?;
    let existing_index = load_artifact_index(&artifact_index_path).ok();
    let mut full_rebuild = matches!(mode, PrepareMode::Full) || existing_index.is_none();

    if full_rebuild && artifacts_dir.exists() {
        fs::remove_dir_all(&artifacts_dir).map_err(|e| RuntimeError::Io {
            path: artifacts_dir.clone(),
            message: e.to_string(),
        })?;
    }
    fs::create_dir_all(&artifacts_dir).map_err(|e| RuntimeError::Io {
        path: artifacts_dir.clone(),
        message: e.to_string(),
    })?;

    let mut contexts =
        build_plugin_contexts(repo_root, &graph, &artifacts_dir, &dependency_snapshot)?;
    let existing_map = existing_index
        .as_ref()
        .map(artifact_index_map)
        .unwrap_or_default();

    for context in &mut contexts {
        context.dirty = if full_rebuild {
            true
        } else {
            compute_dirty_state(
                repo_root,
                context,
                existing_map.get(&context.plugin.plugin_path),
            )?
        };
        if context.dirty {
            context.build_fingerprint =
                Some(compute_build_fingerprint(repo_root, &context.input_files)?);
        }
    }

    if full_rebuild {
        full_rebuild = true;
    } else if contexts
        .iter()
        .any(|context| !existing_map.contains_key(&context.plugin.plugin_path) || context.dirty)
    {
        full_rebuild = false;
    }

    build_dirty_dylib_plugins(fixtures_root, &dependency_snapshot, &contexts)?;

    let built_at = current_build_marker();
    let mut report = PrepareArtifactsReport {
        full_rebuild,
        ..PrepareArtifactsReport::default()
    };
    let mut next_entries = Vec::new();

    for mut context in contexts {
        if !context.dirty {
            if let Some(existing) = existing_map.get(&context.plugin.plugin_path) {
                report.reused.push(context.plugin.plugin_path.clone());
                let mut reused = existing.clone();
                if reused.input_probe != context.input_probe {
                    reused.input_probe = context.input_probe;
                }
                next_entries.push(reused);
                continue;
            }
        }

        let entry = materialize_artifact_entry(
            repo_root,
            &artifacts_dir,
            &dependency_snapshot,
            &mut context,
            &built_at,
        )?;
        report
            .rebuilt
            .push((entry.plugin_path.clone(), entry.sha256.clone()));
        next_entries.push(entry);
    }

    let next_index = ArtifactIndex {
        schema_version: ARTIFACT_INDEX_SCHEMA_VERSION,
        generated_at: current_build_marker(),
        topo_order: graph.topo_order.clone(),
        entries: next_entries,
    };
    // P1-17: durable atomic write.
    write_pretty_json(&artifact_index_path, &next_index)?;
    cleanup_fixture_lockfiles(&plugins_root)?;
    Ok(report)
}

fn build_plugin_contexts(
    repo_root: &Path,
    graph: &ResolvedPluginGraph,
    artifacts_dir: &Path,
    dependency_snapshot: &DependencySnapshot,
) -> Result<Vec<PluginBuildContext>, RuntimeError> {
    let mut contexts = Vec::new();
    for plugin_path in &graph.topo_order {
        let plugin =
            graph
                .plugins
                .get(plugin_path)
                .cloned()
                .ok_or_else(|| RuntimeError::Invariant {
                    message: format!("missing plugin in resolved graph: {plugin_path}"),
                })?;
        let manifest_path = plugin.dir.join("Cargo.toml");
        let build_spec = read_plugin_build_spec(&manifest_path)?;
        let artifact_kind = if build_spec.is_dylib {
            ArtifactKind::Dylib
        } else {
            ArtifactKind::Json
        };
        let artifact_name = expected_artifact_name(plugin_path, build_spec.is_dylib);
        let artifact_path = artifacts_dir.join(&artifact_name);
        let local_dep_dirs = dependency_snapshot.local_dep_dirs_for(&plugin.crate_name);
        let local_path_deps = local_dep_dirs
            .iter()
            .map(|path| relative_display(repo_root, path))
            .collect::<Vec<_>>();
        let input_files = collect_plugin_inputs(&plugin.dir, &local_dep_dirs)?;
        let input_probe = build_input_probe(repo_root, &input_files)?;

        contexts.push(PluginBuildContext {
            plugin,
            build_spec,
            artifact_name,
            artifact_path,
            artifact_kind,
            local_path_deps,
            input_files,
            input_probe,
            build_fingerprint: None,
            dirty: false,
        });
    }
    Ok(contexts)
}

fn compute_dirty_state(
    repo_root: &Path,
    context: &PluginBuildContext,
    existing: Option<&ArtifactIndexEntry>,
) -> Result<bool, RuntimeError> {
    let Some(existing) = existing else {
        return Ok(true);
    };
    if !context.artifact_path.exists() {
        return Ok(true);
    }
    if existing.version != context.build_spec.version
        || existing.abi_fingerprint != context.plugin.metadata.abi_fingerprint
        || existing.artifact_path != context.artifact_name
        || existing.parent != context.plugin.parent
        || existing.required != context.plugin.required
        || existing.grants_from_parent != grants_vec(&context.plugin.grants_from_parent)
        || existing.artifact_kind != context.artifact_kind
        || existing.local_path_deps != context.local_path_deps
    {
        return Ok(true);
    }

    if existing.input_probe == context.input_probe {
        return Ok(false);
    }

    let build_fingerprint = compute_build_fingerprint(repo_root, &context.input_files)?;
    Ok(build_fingerprint != existing.build_fingerprint)
}

fn materialize_artifact_entry(
    repo_root: &Path,
    artifacts_dir: &Path,
    dependency_snapshot: &DependencySnapshot,
    context: &mut PluginBuildContext,
    built_at: &str,
) -> Result<ArtifactIndexEntry, RuntimeError> {
    let docs = if matches!(context.artifact_kind, ArtifactKind::Dylib) {
        let built_path =
            if dependency_snapshot.is_workspace_member(&context.build_spec.package_name) {
                dependency_snapshot.built_dylib_path(&context.build_spec.package_name)
            } else {
                built_dylib_path(
                    &context.plugin.dir.join("Cargo.toml"),
                    &context.build_spec.package_name,
                )?
            };
        // P0-8/C-batch: stage-then-rename over `.so` so any pre-existing
        // mapping (Task-node plugins hold dylibs indefinitely in
        // TASK_LIBRARIES) stays valid until munmap; new opens see the fresh
        // bytes. Previously `fs::copy` truncated + wrote in place, which is
        // the historic SIGSEGV pattern.
        stage_then_rename_file(&built_path, &context.artifact_path)?;

        let (docs, runtime_fingerprint) = inspect_dylib_contract(&context.artifact_path)?;
        if runtime_fingerprint != context.plugin.metadata.abi_fingerprint {
            return Err(RuntimeError::AbiMismatch {
                plugin_path: context.plugin.plugin_path.clone(),
                expected: Box::new(context.plugin.metadata.abi_fingerprint.clone()),
                actual: Box::new(runtime_fingerprint.clone()),
                fingerprint_diff: context
                    .plugin
                    .metadata
                    .abi_fingerprint
                    .diff(&runtime_fingerprint),
            });
        }
        if docs.plugin_path != context.plugin.plugin_path {
            return Err(RuntimeError::DocsContract {
                plugin_path: context.plugin.plugin_path.clone(),
                message: format!(
                    "runtime docs.plugin_path mismatch: expected {}, got {}",
                    context.plugin.plugin_path, docs.plugin_path
                ),
            });
        }
        write_pretty_json(
            &context.plugin.dir.join("docs/agent/interfaces.json"),
            &docs,
        )?;
        docs
    } else {
        let artifact = PluginArtifact {
            plugin_path: context.plugin.plugin_path.clone(),
            abi_fingerprint: context.plugin.metadata.abi_fingerprint.clone(),
            docs: context.plugin.docs.clone(),
            exports: context.build_spec.artifact.exports.clone(),
            execution: context.build_spec.artifact.execution.clone(),
        };
        write_pretty_json(&context.artifact_path, &artifact)?;
        context.plugin.docs.clone()
    };

    let build_fingerprint = match &context.build_fingerprint {
        Some(value) => value.clone(),
        None => compute_build_fingerprint(repo_root, &context.input_files)?,
    };
    let sha256 = sha256_file(&context.artifact_path)?;
    Ok(ArtifactIndexEntry {
        plugin_path: context.plugin.plugin_path.clone(),
        version: context.build_spec.version.clone(),
        abi_fingerprint: context.plugin.metadata.abi_fingerprint.clone(),
        artifact_path: relative_display(artifacts_dir, &context.artifact_path),
        sha256,
        built_at: built_at.to_string(),
        parent: context.plugin.parent.clone(),
        required: context.plugin.required,
        grants_from_parent: grants_vec(&context.plugin.grants_from_parent),
        docs,
        exports: context.build_spec.artifact.exports.clone(),
        execution: context.build_spec.artifact.execution.clone(),
        artifact_kind: context.artifact_kind.clone(),
        build_fingerprint,
        input_probe: context.input_probe.clone(),
        local_path_deps: context.local_path_deps.clone(),
    })
}

fn build_dirty_dylib_plugins(
    fixtures_root: &Path,
    dependency_snapshot: &DependencySnapshot,
    contexts: &[PluginBuildContext],
) -> Result<(), RuntimeError> {
    let repo_root = fixtures_root
        .parent()
        .ok_or_else(|| RuntimeError::Invariant {
            message: format!("fixtures root missing parent: {}", fixtures_root.display()),
        })?;
    let mut workspace_packages = Vec::new();

    for context in contexts {
        if !context.dirty || !matches!(context.artifact_kind, ArtifactKind::Dylib) {
            continue;
        }
        if dependency_snapshot.is_workspace_member(&context.build_spec.package_name) {
            workspace_packages.push(context.build_spec.package_name.clone());
        } else {
            build_plugin_artifact(fixtures_root, &context.plugin.dir.join("Cargo.toml"))?;
        }
    }

    workspace_packages.sort();
    workspace_packages.dedup();
    if workspace_packages.is_empty() {
        return Ok(());
    }

    let mut args = vec![
        "build".to_string(),
        "--manifest-path".to_string(),
        dependency_snapshot
            .workspace_manifest_path
            .display()
            .to_string(),
    ];
    for package_name in workspace_packages {
        args.push("-p".to_string());
        args.push(package_name);
    }
    run_command("cargo", &args, Some(repo_root))?;
    Ok(())
}

fn inspect_dylib_contract(
    artifact_path: &Path,
) -> Result<(PluginDocs, crate::core::models::AbiFingerprint), RuntimeError> {
    let dylib = LoadedDylibApi::open(artifact_path)?;
    let docs: PluginDocs =
        serde_json::from_str(&(dylib.api().docs)().payload).map_err(|e| RuntimeError::Io {
            path: artifact_path.to_path_buf(),
            message: format!("runtime docs parse failed: {e}"),
        })?;
    let fingerprint =
        serde_json::from_str(&(dylib.api().abi_fingerprint)().payload).map_err(|e| {
            RuntimeError::Io {
                path: artifact_path.to_path_buf(),
                message: format!("runtime fingerprint parse failed: {e}"),
            }
        })?;
    Ok((docs, fingerprint))
}

impl DependencySnapshot {
    fn load(plugins_root: &Path) -> Result<Self, RuntimeError> {
        let workspace_manifest_path = plugins_root.join("Cargo.toml");
        let metadata = load_workspace_metadata(&workspace_manifest_path)?;
        let target_directory = PathBuf::from(metadata.target_directory.clone());
        let workspace_member_ids = metadata
            .workspace_members
            .iter()
            .cloned()
            .collect::<HashSet<_>>();
        let packages_by_id = metadata
            .packages
            .iter()
            .map(|package| (package.id.clone(), package.clone()))
            .collect::<HashMap<_, _>>();
        let nodes_by_id = metadata
            .resolve
            .as_ref()
            .map(|resolve| {
                resolve
                    .nodes
                    .iter()
                    .map(|node| (node.id.clone(), node.dependencies.clone()))
                    .collect::<HashMap<_, _>>()
            })
            .unwrap_or_default();

        let mut workspace_members = HashSet::new();
        let mut local_dep_closure_by_name = HashMap::new();
        for package in &metadata.packages {
            if workspace_member_ids.contains(&package.id) {
                workspace_members.insert(package.name.clone());
            }
            let deps = collect_local_dependency_dirs(&package.id, &packages_by_id, &nodes_by_id);
            local_dep_closure_by_name.insert(package.name.clone(), deps);
        }

        Ok(Self {
            workspace_manifest_path,
            workspace_members,
            target_directory,
            local_dep_closure_by_name,
        })
    }

    fn built_dylib_path(&self, package_name: &str) -> PathBuf {
        let dylib_name = format!(
            "{DLL_PREFIX}{}.{}",
            package_name.replace('-', "_"),
            DLL_EXTENSION
        );
        self.target_directory.join("debug").join(dylib_name)
    }

    fn is_workspace_member(&self, package_name: &str) -> bool {
        self.workspace_members.contains(package_name)
    }

    fn local_dep_dirs_for(&self, package_name: &str) -> Vec<PathBuf> {
        self.local_dep_closure_by_name
            .get(package_name)
            .cloned()
            .unwrap_or_default()
    }
}

fn collect_local_dependency_dirs(
    package_id: &str,
    packages_by_id: &HashMap<String, CargoMetadataPackage>,
    nodes_by_id: &HashMap<String, Vec<String>>,
) -> Vec<PathBuf> {
    let Some(root_package) = packages_by_id.get(package_id) else {
        return Vec::new();
    };
    let root_manifest = PathBuf::from(&root_package.manifest_path);
    let root_dir = root_manifest
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let mut visited = HashSet::new();
    let mut stack = vec![package_id.to_string()];
    let mut local_deps = BTreeSet::new();

    while let Some(current) = stack.pop() {
        let Some(dependencies) = nodes_by_id.get(&current) else {
            continue;
        };
        for dep_id in dependencies {
            if !visited.insert(dep_id.clone()) {
                continue;
            }
            let Some(dep_package) = packages_by_id.get(dep_id) else {
                continue;
            };
            if dep_package.source.is_none() {
                let manifest_path = PathBuf::from(&dep_package.manifest_path);
                if let Some(dep_dir) = manifest_path.parent() {
                    if dep_dir != root_dir {
                        local_deps.insert(dep_dir.to_path_buf());
                    }
                }
            }
            stack.push(dep_id.clone());
        }
    }

    local_deps.into_iter().collect()
}

fn load_workspace_metadata(
    workspace_manifest_path: &Path,
) -> Result<CargoMetadataOutput, RuntimeError> {
    let output = run_command(
        "cargo",
        &[
            "metadata".to_string(),
            "--format-version".to_string(),
            "1".to_string(),
            "--manifest-path".to_string(),
            workspace_manifest_path.display().to_string(),
        ],
        workspace_manifest_path.parent(),
    )?;
    serde_json::from_slice(&output).map_err(|e| RuntimeError::ArtifactIndexParse {
        path: workspace_manifest_path.to_path_buf(),
        message: format!("cargo metadata parse failed: {e}"),
    })
}

fn collect_plugin_inputs(
    plugin_dir: &Path,
    local_dep_dirs: &[PathBuf],
) -> Result<Vec<PathBuf>, RuntimeError> {
    let mut files = collect_crate_inputs(plugin_dir, true)?;
    for dep_dir in local_dep_dirs {
        files.extend(collect_crate_inputs(dep_dir, false)?);
    }
    files.sort();
    files.dedup();
    Ok(files)
}

fn collect_crate_inputs(
    crate_dir: &Path,
    include_docs: bool,
) -> Result<Vec<PathBuf>, RuntimeError> {
    let mut files = Vec::new();
    let manifest_path = crate_dir.join("Cargo.toml");
    if manifest_path.exists() {
        files.push(manifest_path);
    }

    let build_rs = crate_dir.join("build.rs");
    if build_rs.exists() {
        files.push(build_rs);
    }

    let src_dir = crate_dir.join("src");
    if src_dir.exists() {
        collect_files_recursively(&src_dir, &mut files)?;
    }

    if include_docs {
        let docs_path = crate_dir.join("docs/agent/interfaces.json");
        if docs_path.exists() {
            files.push(docs_path);
        }
    }

    files.sort();
    files.dedup();
    Ok(files)
}

fn collect_files_recursively(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), RuntimeError> {
    for entry in fs::read_dir(dir).map_err(|e| RuntimeError::Io {
        path: dir.to_path_buf(),
        message: e.to_string(),
    })? {
        let entry = entry.map_err(|e| RuntimeError::Io {
            path: dir.to_path_buf(),
            message: e.to_string(),
        })?;
        let path = entry.path();
        let metadata = fs::metadata(&path).map_err(|e| RuntimeError::Io {
            path: path.clone(),
            message: e.to_string(),
        })?;
        if metadata.is_dir() {
            if entry.file_name() == "target" {
                continue;
            }
            collect_files_recursively(&path, out)?;
        } else if metadata.is_file() {
            out.push(path);
        }
    }
    Ok(())
}

fn build_input_probe(repo_root: &Path, files: &[PathBuf]) -> Result<InputProbe, RuntimeError> {
    let mut probe = InputProbe::default();
    for file in files {
        let metadata = fs::metadata(file).map_err(|e| RuntimeError::Io {
            path: file.clone(),
            message: e.to_string(),
        })?;
        // P2-28: modtime read failure -> fall back to `now` (treat as
        // freshly modified) instead of `UNIX_EPOCH` (treat as ancient).
        // The old behaviour would silently mark a file as "very old" on
        // a filesystem that doesn't support mtime, biasing dirty-tracking
        // in the wrong direction. Log the error so an operator can
        // notice repeat cases.
        let modified_time = metadata.modified().unwrap_or_else(|err| {
            eprintln!(
                "[tooling] failed to read mtime for {}: {err}; treating as freshly modified",
                file.display()
            );
            SystemTime::now()
        });
        let modified_at_ms = modified_time
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        probe.files.push(InputProbeFile {
            path: relative_display(repo_root, file),
            size: metadata.len(),
            modified_at_ms,
        });
    }
    Ok(probe)
}

fn compute_build_fingerprint(repo_root: &Path, files: &[PathBuf]) -> Result<String, RuntimeError> {
    let mut hasher = Sha256::new();
    for file in files {
        hasher.update(relative_display(repo_root, file).as_bytes());
        hasher.update([0_u8]);
        let bytes = fs::read(file).map_err(|e| RuntimeError::Io {
            path: file.clone(),
            message: e.to_string(),
        })?;
        hasher.update(&bytes);
        hasher.update([0_u8]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn relative_display(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .map(|relative| relative.to_string_lossy().replace('\\', "/"))
        .unwrap_or_else(|_| path.to_string_lossy().replace('\\', "/"))
}

fn grants_vec(grants: &BTreeSet<String>) -> Vec<String> {
    grants.iter().cloned().collect()
}

fn can_prepare_fixture_artifacts(fixtures_root: &Path) -> bool {
    fixtures_root.join("plugins/Cargo.toml").exists() && fixtures_root.parent().is_some()
}

fn read_plugin_build_spec(manifest_path: &Path) -> Result<PluginBuildSpec, RuntimeError> {
    let text = fs::read_to_string(manifest_path).map_err(|e| RuntimeError::Io {
        path: manifest_path.to_path_buf(),
        message: e.to_string(),
    })?;
    let manifest: PluginManifestToml =
        toml::from_str(&text).map_err(|e| RuntimeError::CargoParse {
            path: manifest_path.to_path_buf(),
            message: e.to_string(),
        })?;
    let package = manifest
        .package
        .ok_or_else(|| RuntimeError::InvalidWorkspace {
            path: manifest_path.to_path_buf(),
        })?;

    Ok(PluginBuildSpec {
        package_name: package.name,
        version: package.version,
        is_dylib: manifest
            .lib
            .as_ref()
            .map(|lib| {
                lib.crate_type
                    .iter()
                    .any(|kind| kind == "dylib" || kind == "cdylib")
            })
            .unwrap_or(false),
        artifact: package
            .metadata
            .and_then(|metadata| metadata.cordis)
            .and_then(|cordis| cordis.artifact)
            .unwrap_or_default(),
    })
}

fn build_plugin_artifact(fixtures_root: &Path, manifest_path: &Path) -> Result<(), RuntimeError> {
    let repo_root = fixtures_root
        .parent()
        .ok_or_else(|| RuntimeError::Invariant {
            message: format!("fixtures root missing parent: {}", fixtures_root.display()),
        })?;
    run_command(
        "cargo",
        &[
            "build".to_string(),
            "--manifest-path".to_string(),
            manifest_path.display().to_string(),
        ],
        Some(repo_root),
    )?;
    Ok(())
}

fn built_dylib_path(manifest_path: &Path, package_name: &str) -> Result<PathBuf, RuntimeError> {
    let metadata = run_command(
        "cargo",
        &[
            "metadata".to_string(),
            "--format-version".to_string(),
            "1".to_string(),
            "--no-deps".to_string(),
            "--manifest-path".to_string(),
            manifest_path.display().to_string(),
        ],
        manifest_path.parent(),
    )?;
    let parsed: CargoMetadataOutput =
        serde_json::from_slice(&metadata).map_err(|e| RuntimeError::ArtifactIndexParse {
            path: manifest_path.to_path_buf(),
            message: format!("cargo metadata parse failed: {e}"),
        })?;
    let dylib_name = format!(
        "{DLL_PREFIX}{}.{}",
        package_name.replace('-', "_"),
        DLL_EXTENSION
    );
    Ok(PathBuf::from(parsed.target_directory)
        .join("debug")
        .join(dylib_name))
}

fn run_command(
    program: &str,
    args: &[String],
    current_dir: Option<&Path>,
) -> Result<Vec<u8>, RuntimeError> {
    let command_args = if program == "cargo" {
        prepare_local_cargo_args(args)
    } else {
        args.to_vec()
    };
    let mut command = Command::new(program);
    command.args(&command_args);
    if program == "cargo" {
        strip_proxy_envs(&mut command);
    }
    if let Some(dir) = current_dir {
        command.current_dir(dir);
    }
    let output = command.output().map_err(|e| RuntimeError::CommandFailed {
        program: program.to_string(),
        args: command_args.clone(),
        message: e.to_string(),
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let message = if stderr.is_empty() { stdout } else { stderr };
        return Err(RuntimeError::CommandFailed {
            program: program.to_string(),
            args: command_args,
            message,
        });
    }
    Ok(output.stdout)
}

fn prepare_local_cargo_args(args: &[String]) -> Vec<String> {
    let mut command_args = args.to_vec();
    if cargo_command_prefers_offline(args) && !command_args.iter().any(|arg| arg == "--offline") {
        command_args.push("--offline".to_string());
    }
    command_args
}

fn cargo_command_prefers_offline(args: &[String]) -> bool {
    matches!(args.first().map(String::as_str), Some("metadata"))
}

/// P2-30: run `command` with a wall-clock timeout. On expiry, kill + wait
/// so we return promptly with an error rather than blocking indefinitely
/// on `Command::output()`. Poll interval is 100ms; overhead is trivial
/// compared to a cargo build.
fn run_command_with_timeout(
    mut command: Command,
    timeout: std::time::Duration,
) -> Result<std::process::Output, RuntimeError> {
    use std::io::Read;
    use std::process::Stdio;
    use std::time::Instant;
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|e| RuntimeError::InvalidArgument {
        message: format!("cargo spawn failed: {e}"),
    })?;
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    timed_out = true;
                    break std::process::ExitStatus::default();
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => {
                return Err(RuntimeError::InvalidArgument {
                    message: format!("cargo wait failed: {e}"),
                });
            }
        }
    };
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    if let Some(mut o) = child.stdout.take() {
        let _ = o.read_to_end(&mut stdout);
    }
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_end(&mut stderr);
    }
    if timed_out {
        return Err(RuntimeError::InvalidArgument {
            message: format!(
                "cargo build exceeded timeout ({:?}); stderr={}",
                timeout,
                String::from_utf8_lossy(&stderr).trim()
            ),
        });
    }
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

fn strip_proxy_envs(command: &mut Command) {
    for key in [
        "ALL_PROXY",
        "HTTP_PROXY",
        "HTTPS_PROXY",
        "all_proxy",
        "http_proxy",
        "https_proxy",
    ] {
        command.env_remove(key);
    }
}

fn expected_artifact_name(plugin_path: &str, is_dylib: bool) -> String {
    let stem = plugin_path.replace('/', "_");
    if is_dylib {
        format!("{stem}.{DLL_EXTENSION}")
    } else {
        format!("{stem}.json")
    }
}

fn write_pretty_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<(), RuntimeError> {
    let parent = path.parent().ok_or_else(|| RuntimeError::Invariant {
        message: format!("json path missing parent: {}", path.display()),
    })?;
    fs::create_dir_all(parent).map_err(|e| RuntimeError::Io {
        path: parent.to_path_buf(),
        message: e.to_string(),
    })?;
    // P1-17: durable JSON write. Was `fs::write` — a crash mid-write left a
    // torn JSON. `artifact/index.json` is consumed by `PluginInvoker::load`
    // on the very next verify pass; a torn index bricked the runtime.
    // Now: staging tmp + sync_all + rename. Covers `write_pretty_json`'s
    // callers (docs/interfaces.json and artifact/index.json) at 337, 361,
    // 483, 1207.
    use std::io::Write as _;
    let bytes = pretty_json(value);
    let tmp = match path.file_name() {
        Some(name) => {
            let mut owned = name.to_os_string();
            owned.push(format!(".cordis-tmp.{}", std::process::id()));
            path.with_file_name(owned)
        }
        None => {
            return Err(RuntimeError::Invariant {
                message: format!("path has no filename: {}", path.display()),
            });
        }
    };
    {
        let mut file = fs::File::create(&tmp).map_err(|e| RuntimeError::Io {
            path: tmp.clone(),
            message: format!("create tmp: {e}"),
        })?;
        file.write_all(bytes.as_bytes())
            .map_err(|e| RuntimeError::Io {
                path: tmp.clone(),
                message: format!("write tmp: {e}"),
            })?;
        file.sync_all().map_err(|e| RuntimeError::Io {
            path: tmp.clone(),
            message: format!("sync tmp: {e}"),
        })?;
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(RuntimeError::Io {
            path: path.to_path_buf(),
            message: format!("rename tmp -> target: {e}"),
        });
    }
    Ok(())
}

impl ArtifactBuildLock {
    fn acquire(fixtures_root: &Path) -> Result<Self, RuntimeError> {
        let path = fixtures_root.join(BUILD_LOCK_FILE);
        let started_at = Instant::now();

        loop {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    let state = ArtifactBuildLockState {
                        pid: process::id(),
                        created_at_ms: current_epoch_ms(),
                    };
                    let encoded = serde_json::to_vec(&state).map_err(|err| RuntimeError::Io {
                        path: path.clone(),
                        message: format!("lock metadata serialize failed: {err}"),
                    })?;
                    file.write_all(&encoded).map_err(|err| RuntimeError::Io {
                        path: path.clone(),
                        message: err.to_string(),
                    })?;
                    file.flush().map_err(|err| RuntimeError::Io {
                        path: path.clone(),
                        message: err.to_string(),
                    })?;
                    // P1-17: fsync the lock file so a power loss doesn't
                    // leave a 0-byte file that lock_pid_is_live can't parse.
                    file.sync_all().map_err(|err| RuntimeError::Io {
                        path: path.clone(),
                        message: err.to_string(),
                    })?;
                    return Ok(Self { path });
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    maybe_remove_stale_lock(&path)?;
                    if started_at.elapsed() > BUILD_LOCK_WAIT_TIMEOUT {
                        return Err(RuntimeError::ArtifactBuildLockTimeout {
                            path,
                            waited_ms: started_at.elapsed().as_millis(),
                        });
                    }
                    thread::sleep(Duration::from_millis(100));
                }
                Err(err) => {
                    return Err(RuntimeError::Io {
                        path: path.clone(),
                        message: err.to_string(),
                    });
                }
            }
        }
    }
}

fn maybe_remove_stale_lock(path: &Path) -> Result<(), RuntimeError> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(RuntimeError::Io {
                path: path.to_path_buf(),
                message: err.to_string(),
            });
        }
    };
    // P2-28: `unwrap_or(SystemTime::now())` was already the right
    // fallback — mtime read failure -> treat as "just modified" so the
    // lock is considered live. Kept as-is; the comment makes the intent
    // explicit so future refactors don't flip it to UNIX_EPOCH.
    let modified = metadata.modified().unwrap_or_else(|err| {
        eprintln!(
            "[tooling] lock file mtime read failed for {}: {err}",
            path.display()
        );
        SystemTime::now()
    });

    if let Ok(text) = fs::read_to_string(path) {
        if let Ok(state) = serde_json::from_str::<ArtifactBuildLockState>(&text) {
            if !lock_pid_is_live(state.pid)
                || SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_millis().saturating_sub(state.created_at_ms))
                    .unwrap_or_default()
                    > STALE_LOCK_TIMEOUT.as_millis()
            {
                match fs::remove_file(path) {
                    Ok(()) => return Ok(()),
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                    Err(err) => {
                        return Err(RuntimeError::Io {
                            path: path.to_path_buf(),
                            message: err.to_string(),
                        });
                    }
                }
            }
            return Ok(());
        }
    }

    if SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default()
        > LEGACY_STALE_LOCK_TIMEOUT
    {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(RuntimeError::Io {
                    path: path.to_path_buf(),
                    message: err.to_string(),
                });
            }
        }
    }
    Ok(())
}

/// P0-10: on macOS/BSD `/proc` does not exist, so the previous implementation
/// always returned false → every live lock was declared dead the moment the
/// staleness timer expired, letting two concurrent `prepare_artifacts` calls
/// clobber each other. Fix: use `kill(pid, 0)`, which is portable across all
/// Unix platforms and returns EPERM (still alive, other uid) or ESRCH (dead)
/// without actually delivering a signal.
#[cfg(unix)]
pub(crate) fn lock_pid_is_live(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: `kill(pid, 0)` is signal-safe and side-effect free; it only
    // checks whether the caller *could* deliver a signal to `pid`.
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    // errno is thread-local; if the syscall returned an error, distinguish
    // "no such process" (ESRCH → dead) from "permission denied" (EPERM →
    // alive, owned by another user).
    let err = std::io::Error::last_os_error();
    !matches!(err.raw_os_error(), Some(libc::ESRCH))
}

#[cfg(not(unix))]
pub(crate) fn lock_pid_is_live(_pid: u32) -> bool {
    // Non-Unix: no portable liveness probe, so be conservative and assume
    // still alive; the staleness timer eventually reclaims true zombies.
    true
}

fn current_epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn cleanup_fixture_lockfiles(plugins_root: &Path) -> Result<(), RuntimeError> {
    if !plugins_root.exists() {
        return Ok(());
    }
    // P2-19: gate the cleanup behind an opt-in env var. Blindly deleting
    // every `Cargo.lock` under `plugins/` on each successful build breaks
    // concurrent readers (editor lock services, another cargo process)
    // and prevents reproducible builds. The historical behaviour is kept
    // for callers who really need it; the default is now to leave lock
    // files alone.
    if std::env::var("CORDIS_CLEAN_FIXTURE_LOCKFILES")
        .ok()
        .as_deref()
        != Some("1")
    {
        return Ok(());
    }

    for entry in fs::read_dir(plugins_root).map_err(|e| RuntimeError::Io {
        path: plugins_root.to_path_buf(),
        message: e.to_string(),
    })? {
        let entry = entry.map_err(|e| RuntimeError::Io {
            path: plugins_root.to_path_buf(),
            message: e.to_string(),
        })?;
        remove_lockfiles_recursively(&entry.path())?;
    }
    Ok(())
}

fn remove_lockfiles_recursively(path: &Path) -> Result<(), RuntimeError> {
    if !path.exists() {
        return Ok(());
    }
    let metadata = fs::metadata(path).map_err(|e| RuntimeError::Io {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    if metadata.is_file() {
        if path.file_name().and_then(|name| name.to_str()) == Some("Cargo.lock") {
            fs::remove_file(path).map_err(|e| RuntimeError::Io {
                path: path.to_path_buf(),
                message: e.to_string(),
            })?;
        }
        return Ok(());
    }

    for entry in fs::read_dir(path).map_err(|e| RuntimeError::Io {
        path: path.to_path_buf(),
        message: e.to_string(),
    })? {
        let entry = entry.map_err(|e| RuntimeError::Io {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;
        if entry.file_name() == "target" {
            continue;
        }
        remove_lockfiles_recursively(&entry.path())?;
    }
    Ok(())
}

fn current_build_marker() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string())
}

fn absolute_path(path: &Path) -> Result<PathBuf, RuntimeError> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .map_err(|e| RuntimeError::Io {
            path: path.to_path_buf(),
            message: e.to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::{cargo_command_prefers_offline, prepare_local_cargo_args};

    #[test]
    fn metadata_commands_run_offline_for_local_fixture_tooling() {
        let args = vec![
            "metadata".to_string(),
            "--format-version".to_string(),
            "1".to_string(),
        ];
        assert!(cargo_command_prefers_offline(&args));
        assert_eq!(
            prepare_local_cargo_args(&args),
            vec![
                "metadata".to_string(),
                "--format-version".to_string(),
                "1".to_string(),
                "--offline".to_string(),
            ]
        );
    }

    #[test]
    fn non_metadata_commands_keep_original_cargo_args() {
        let args = vec![
            "build".to_string(),
            "--manifest-path".to_string(),
            "fixtures/plugins/Cargo.toml".to_string(),
        ];
        assert!(!cargo_command_prefers_offline(&args));
        assert_eq!(prepare_local_cargo_args(&args), args);
    }

    // ---------- C batch (P0-9 / P0-10) tests ----------

    #[test]
    fn stage_then_rename_replaces_existing_target_atomically() {
        // P0-9 sibling: the shared stage-then-rename helper must overwrite an
        // existing dst with the new source bytes; no leftover .cordis-tmp.
        use super::stage_then_rename_file;
        use tempfile::TempDir;
        let temp = TempDir::new().unwrap();
        let src = temp.path().join("plugin.so");
        let dst = temp.path().join("target.so");
        std::fs::write(&src, b"fresh").unwrap();
        std::fs::write(&dst, b"stale").unwrap();
        stage_then_rename_file(&src, &dst).expect("stage-then-rename ok");
        assert_eq!(std::fs::read(&dst).unwrap(), b"fresh");
        let leftovers: Vec<_> = std::fs::read_dir(temp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".cordis-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "no leftover tmp file expected");
    }

    #[cfg(unix)]
    #[test]
    fn lock_pid_liveness_recognises_current_process_as_alive() {
        // P0-10: previously used `/proc/{pid}` and returned false on macOS/BSD
        // for every live pid. `kill(pid, 0)` is portable; the running process
        // must always be observed as alive.
        use super::lock_pid_is_live;
        assert!(lock_pid_is_live(std::process::id()));
    }

    #[cfg(unix)]
    #[test]
    fn lock_pid_liveness_reports_dead_pid_as_dead() {
        use super::lock_pid_is_live;
        // A pid we're extremely unlikely to have — u32::MAX is above the
        // maximum kernel-allowed pid on all common systems.
        assert!(!lock_pid_is_live(u32::MAX - 1));
    }

    /// P1-17: `write_pretty_json` is the durable writer for
    /// interfaces.json / artifact/index.json / lock files. Verify tmp
    /// + rename semantics: no stray `.cordis-tmp.<pid>` after a
    ///   success.
    #[test]
    fn write_pretty_json_is_atomic_and_leaves_no_tmp() {
        use super::write_pretty_json;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("index.json");
        write_pretty_json(&target, &serde_json::json!({"k": 1})).unwrap();
        assert!(target.exists());
        // No leftover tmp file.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".cordis-tmp."))
            .collect();
        assert!(leftovers.is_empty(), "no tmp leftover expected");
        // Overwrite preserves prior success semantics: file has new bytes.
        write_pretty_json(&target, &serde_json::json!({"k": 2})).unwrap();
        let text = std::fs::read_to_string(&target).unwrap();
        assert!(text.contains("\"k\""));
        assert!(text.contains("2"));
    }

    /// P0-9 sibling: `stage_then_rename_file` preserves the source's
    /// executable bit on Unix (dlopen requires +x on the dylib).
    #[cfg(unix)]
    #[test]
    fn stage_then_rename_preserves_exec_bit() {
        use super::stage_then_rename_file;
        use std::os::unix::fs::PermissionsExt;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("plug.so");
        let dst = dir.path().join("target.so");
        std::fs::write(&src, b"bytes").unwrap();
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o755)).unwrap();
        stage_then_rename_file(&src, &dst).unwrap();
        let mode = std::fs::metadata(&dst).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o755,
            "exec bits should be preserved, got {mode:o}"
        );
    }

    // ---------- path / name helpers ----------

    #[test]
    fn relative_display_strips_base_and_normalises_separators() {
        use super::relative_display;
        use std::path::Path;
        let base = Path::new("/repo/root");
        assert_eq!(
            relative_display(base, Path::new("/repo/root/plugins/qq/src/lib.rs")),
            "plugins/qq/src/lib.rs"
        );
        // Path outside base falls back to the lossy absolute rendering.
        assert_eq!(
            relative_display(base, Path::new("/other/place.rs")),
            "/other/place.rs"
        );
    }

    #[test]
    fn grants_vec_preserves_sorted_order() {
        use super::grants_vec;
        use std::collections::BTreeSet;
        let mut set = BTreeSet::new();
        set.insert("write".to_string());
        set.insert("read".to_string());
        // BTreeSet is ordered, so the Vec comes out sorted.
        assert_eq!(
            grants_vec(&set),
            vec!["read".to_string(), "write".to_string()]
        );
        assert!(grants_vec(&BTreeSet::new()).is_empty());
    }

    #[test]
    fn expected_artifact_name_uses_extension_by_kind() {
        use super::expected_artifact_name;
        use std::env::consts::DLL_EXTENSION;
        assert_eq!(
            expected_artifact_name("expr/evaluator", false),
            "expr_evaluator.json"
        );
        assert_eq!(
            expected_artifact_name("expr/evaluator", true),
            format!("expr_evaluator.{DLL_EXTENSION}")
        );
    }

    #[test]
    fn can_prepare_requires_plugins_manifest() {
        use super::can_prepare_fixture_artifacts;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        assert!(!can_prepare_fixture_artifacts(dir.path()));
        std::fs::create_dir_all(dir.path().join("plugins")).unwrap();
        std::fs::write(dir.path().join("plugins/Cargo.toml"), "[workspace]\n").unwrap();
        assert!(can_prepare_fixture_artifacts(dir.path()));
    }

    #[test]
    fn build_markers_are_numeric_strings() {
        use super::{current_build_marker, current_epoch_ms};
        let marker = current_build_marker();
        assert!(
            marker.parse::<u64>().is_ok(),
            "marker should be secs: {marker}"
        );
        assert!(current_epoch_ms() > 0);
    }

    #[test]
    fn absolute_path_passes_through_absolute_and_joins_relative() {
        use super::absolute_path;
        use std::path::{Path, PathBuf};
        let abs = Path::new("/tmp/some/where");
        assert_eq!(
            absolute_path(abs).unwrap(),
            PathBuf::from("/tmp/some/where")
        );
        let rel = absolute_path(Path::new("rel/child")).unwrap();
        assert!(rel.is_absolute());
        assert!(rel.ends_with("rel/child"));
    }

    // ---------- manifest parsing ----------

    #[test]
    fn read_plugin_build_spec_parses_dylib_and_artifact_config() {
        use super::read_plugin_build_spec;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let manifest = dir.path().join("Cargo.toml");
        std::fs::write(
            &manifest,
            r#"[package]
name = "demo_plugin"
version = "1.2.3"

[lib]
crate-type = ["rlib", "cdylib"]

[package.metadata.cordis.artifact]
exports = ["svc_a", "svc_b"]
"#,
        )
        .unwrap();
        let spec = read_plugin_build_spec(&manifest).unwrap();
        assert_eq!(spec.package_name, "demo_plugin");
        assert_eq!(spec.version, "1.2.3");
        assert!(spec.is_dylib);
        assert_eq!(spec.artifact.exports, vec!["svc_a", "svc_b"]);
    }

    #[test]
    fn read_plugin_build_spec_defaults_to_json_when_no_dylib_crate_type() {
        use super::read_plugin_build_spec;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let manifest = dir.path().join("Cargo.toml");
        std::fs::write(
            &manifest,
            "[package]\nname = \"jsonp\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let spec = read_plugin_build_spec(&manifest).unwrap();
        assert!(!spec.is_dylib);
        assert!(spec.artifact.exports.is_empty());
    }

    #[test]
    fn read_plugin_build_spec_rejects_missing_package_and_bad_toml() {
        use super::read_plugin_build_spec;
        use crate::core::error::RuntimeError;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();

        // Missing file -> Io error.
        let missing = dir.path().join("nope/Cargo.toml");
        assert!(matches!(
            read_plugin_build_spec(&missing),
            Err(RuntimeError::Io { .. })
        ));

        // Malformed TOML -> CargoParse.
        let bad = dir.path().join("bad.toml");
        std::fs::write(&bad, "this is = = not toml").unwrap();
        assert!(matches!(
            read_plugin_build_spec(&bad),
            Err(RuntimeError::CargoParse { .. })
        ));

        // Valid TOML but no [package] -> InvalidWorkspace.
        let no_pkg = dir.path().join("nopkg.toml");
        std::fs::write(&no_pkg, "[lib]\ncrate-type=[\"dylib\"]\n").unwrap();
        assert!(matches!(
            read_plugin_build_spec(&no_pkg),
            Err(RuntimeError::InvalidWorkspace { .. })
        ));
    }

    // ---------- input collection / probe / fingerprint ----------

    fn scaffold_crate(root: &std::path::Path) {
        use std::fs;
        fs::create_dir_all(root.join("src/inner")).unwrap();
        fs::create_dir_all(root.join("docs/agent")).unwrap();
        fs::create_dir_all(root.join("target/debug")).unwrap();
        fs::write(root.join("Cargo.toml"), "[package]\nname=\"c\"\n").unwrap();
        fs::write(root.join("build.rs"), "fn main() {}").unwrap();
        fs::write(root.join("src/lib.rs"), "// lib").unwrap();
        fs::write(root.join("src/inner/mod.rs"), "// inner").unwrap();
        fs::write(root.join("docs/agent/interfaces.json"), "{}").unwrap();
        // Should be excluded by the `target` skip.
        fs::write(root.join("target/debug/artifact.bin"), "x").unwrap();
    }

    #[test]
    fn collect_crate_inputs_gathers_sources_and_skips_target() {
        use super::collect_crate_inputs;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        scaffold_crate(dir.path());

        let files = collect_crate_inputs(dir.path(), true).unwrap();
        let names: Vec<String> = files
            .iter()
            .map(|p| {
                p.strip_prefix(dir.path())
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert!(names.contains(&"Cargo.toml".to_string()));
        assert!(names.contains(&"build.rs".to_string()));
        assert!(names.contains(&"src/lib.rs".to_string()));
        assert!(names.contains(&"src/inner/mod.rs".to_string()));
        assert!(names.contains(&"docs/agent/interfaces.json".to_string()));
        assert!(
            !names.iter().any(|n| n.contains("target/")),
            "target/ must be excluded, got {names:?}"
        );

        // include_docs=false drops the interfaces.json.
        let no_docs = collect_crate_inputs(dir.path(), false).unwrap();
        assert!(!no_docs
            .iter()
            .any(|p| p.ends_with("docs/agent/interfaces.json")));
    }

    #[test]
    fn collect_plugin_inputs_merges_local_deps_deduped_sorted() {
        use super::collect_plugin_inputs;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let plugin = dir.path().join("plugin");
        let dep = dir.path().join("dep");
        scaffold_crate(&plugin);
        scaffold_crate(&dep);

        let files = collect_plugin_inputs(&plugin, std::slice::from_ref(&dep)).unwrap();
        // Sorted + deduped.
        let mut sorted = files.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(files, sorted);
        // Contains files from both crates.
        assert!(files.iter().any(|p| p.starts_with(&plugin)));
        assert!(files.iter().any(|p| p.starts_with(&dep)));
        // Dep contributes no interfaces.json (include_docs=false for deps).
        let dep_docs = dep.join("docs/agent/interfaces.json");
        assert!(!files.contains(&dep_docs));
    }

    #[test]
    fn build_input_probe_records_relative_path_and_size() {
        use super::build_input_probe;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("plugins/qq/src/lib.rs");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"hello world").unwrap();

        let probe = build_input_probe(dir.path(), std::slice::from_ref(&file)).unwrap();
        assert_eq!(probe.files.len(), 1);
        assert_eq!(probe.files[0].path, "plugins/qq/src/lib.rs");
        assert_eq!(probe.files[0].size, 11);
    }

    #[test]
    fn build_input_probe_errors_on_missing_file() {
        use super::build_input_probe;
        use crate::core::error::RuntimeError;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("ghost.rs");
        assert!(matches!(
            build_input_probe(dir.path(), &[missing]),
            Err(RuntimeError::Io { .. })
        ));
    }

    #[test]
    fn compute_build_fingerprint_is_content_sensitive_and_deterministic() {
        use super::compute_build_fingerprint;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("a.rs");
        std::fs::write(&file, b"one").unwrap();

        let fp1 = compute_build_fingerprint(dir.path(), std::slice::from_ref(&file)).unwrap();
        let fp2 = compute_build_fingerprint(dir.path(), std::slice::from_ref(&file)).unwrap();
        assert_eq!(fp1, fp2, "same content -> same fingerprint");

        std::fs::write(&file, b"two").unwrap();
        let fp3 = compute_build_fingerprint(dir.path(), std::slice::from_ref(&file)).unwrap();
        assert_ne!(fp1, fp3, "content change -> new fingerprint");
    }

    #[test]
    fn compute_build_fingerprint_errors_on_missing_file() {
        use super::compute_build_fingerprint;
        use crate::core::error::RuntimeError;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        assert!(matches!(
            compute_build_fingerprint(dir.path(), &[dir.path().join("nope.rs")]),
            Err(RuntimeError::Io { .. })
        ));
    }

    // ---------- compute_dirty_state ----------

    fn sample_abi() -> crate::core::models::AbiFingerprint {
        crate::core::models::AbiFingerprint {
            rustc_version: "rustc".to_string(),
            target_triple: "triple".to_string(),
            crate_hash: "crate_v1".to_string(),
            api_hash: "api_v1".to_string(),
        }
    }

    fn sample_context(artifact_path: std::path::PathBuf) -> super::PluginBuildContext {
        use super::{PluginBuildContext, PluginBuildSpec, SourceArtifactConfig};
        use crate::core::models::{ArtifactKind, CordisMetadata, InputProbe};
        use crate::plugin::package::ResolvedPlugin;
        use cordis_plugin_sdk::plugin_docs;
        use std::collections::BTreeSet;
        let plugin = ResolvedPlugin {
            plugin_path: "foo".to_string(),
            crate_name: "foo".to_string(),
            dir: std::path::PathBuf::from("/repo/plugins/foo"),
            metadata: CordisMetadata {
                plugin_path: "foo".to_string(),
                abi_kind: Default::default(),
                abi_fingerprint: sample_abi(),
                children: Vec::new(),
                declared_nodes: Vec::new(),
                allow_generated_docs: false,
            },
            docs: plugin_docs("foo", "foo", "0.1.0", None, Vec::new(), None),
            parent: None,
            required: true,
            grants_from_parent: BTreeSet::new(),
        };
        PluginBuildContext {
            plugin,
            build_spec: PluginBuildSpec {
                package_name: "foo".to_string(),
                version: "0.1.0".to_string(),
                is_dylib: false,
                artifact: SourceArtifactConfig::default(),
            },
            artifact_name: "foo.json".to_string(),
            artifact_path,
            artifact_kind: ArtifactKind::Json,
            local_path_deps: Vec::new(),
            input_files: Vec::new(),
            input_probe: InputProbe::default(),
            build_fingerprint: None,
            dirty: false,
        }
    }

    fn entry_matching(ctx: &super::PluginBuildContext) -> crate::core::models::ArtifactIndexEntry {
        crate::core::models::ArtifactIndexEntry {
            plugin_path: ctx.plugin.plugin_path.clone(),
            version: ctx.build_spec.version.clone(),
            abi_fingerprint: ctx.plugin.metadata.abi_fingerprint.clone(),
            artifact_path: ctx.artifact_name.clone(),
            sha256: "abc".to_string(),
            built_at: "0".to_string(),
            parent: ctx.plugin.parent.clone(),
            required: ctx.plugin.required,
            grants_from_parent: Vec::new(),
            docs: ctx.plugin.docs.clone(),
            exports: Vec::new(),
            execution: None,
            artifact_kind: ctx.artifact_kind.clone(),
            build_fingerprint: "fp".to_string(),
            input_probe: ctx.input_probe.clone(),
            local_path_deps: ctx.local_path_deps.clone(),
        }
    }

    #[test]
    fn compute_dirty_state_true_when_no_existing_entry() {
        use super::compute_dirty_state;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let artifact = dir.path().join("foo.json");
        std::fs::write(&artifact, "{}").unwrap();
        let ctx = sample_context(artifact);
        assert!(compute_dirty_state(dir.path(), &ctx, None).unwrap());
    }

    #[test]
    fn compute_dirty_state_true_when_artifact_missing() {
        use super::compute_dirty_state;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let artifact = dir.path().join("absent.json");
        let ctx = sample_context(artifact);
        let existing = entry_matching(&ctx);
        assert!(compute_dirty_state(dir.path(), &ctx, Some(&existing)).unwrap());
    }

    #[test]
    fn compute_dirty_state_true_on_metadata_drift() {
        use super::compute_dirty_state;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let artifact = dir.path().join("foo.json");
        std::fs::write(&artifact, "{}").unwrap();
        let ctx = sample_context(artifact);
        let mut existing = entry_matching(&ctx);
        existing.version = "9.9.9".to_string();
        assert!(compute_dirty_state(dir.path(), &ctx, Some(&existing)).unwrap());
    }

    #[test]
    fn compute_dirty_state_false_when_probe_matches() {
        use super::compute_dirty_state;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let artifact = dir.path().join("foo.json");
        std::fs::write(&artifact, "{}").unwrap();
        let ctx = sample_context(artifact);
        let existing = entry_matching(&ctx);
        // probe equal (both default) -> clean.
        assert!(!compute_dirty_state(dir.path(), &ctx, Some(&existing)).unwrap());
    }

    #[test]
    fn compute_dirty_state_uses_fingerprint_when_probe_differs() {
        use super::{compute_build_fingerprint, compute_dirty_state};
        use crate::core::models::{InputProbe, InputProbeFile};
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let artifact = dir.path().join("foo.json");
        std::fs::write(&artifact, "{}").unwrap();
        let ctx = sample_context(artifact);
        // ctx.input_files is empty, so the fingerprint is stable.
        let real_fp = compute_build_fingerprint(dir.path(), &ctx.input_files).unwrap();

        let differing_probe = InputProbe {
            files: vec![InputProbeFile {
                path: "x".to_string(),
                size: 1,
                modified_at_ms: 42,
            }],
        };

        // Probe differs but the recomputed fingerprint matches -> clean.
        let mut same_fp = entry_matching(&ctx);
        same_fp.input_probe = differing_probe.clone();
        same_fp.build_fingerprint = real_fp;
        assert!(!compute_dirty_state(dir.path(), &ctx, Some(&same_fp)).unwrap());

        // Probe differs and stored fingerprint differs -> dirty.
        let mut diff_fp = entry_matching(&ctx);
        diff_fp.input_probe = differing_probe;
        diff_fp.build_fingerprint = "stale-fingerprint".to_string();
        assert!(compute_dirty_state(dir.path(), &ctx, Some(&diff_fp)).unwrap());
    }

    // ---------- read_plugin_docs (JSON artifact path) ----------

    #[test]
    fn read_plugin_docs_reads_json_artifact() {
        use super::read_plugin_docs;
        use crate::core::models::PluginArtifact;
        use cordis_plugin_sdk::{plugin_docs, pretty_json};
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("plug.json");
        let artifact = PluginArtifact {
            plugin_path: "foo".to_string(),
            abi_fingerprint: sample_abi(),
            docs: plugin_docs("foo", "foo", "0.1.0", Some("Foo"), Vec::new(), None),
            exports: Vec::new(),
            execution: None,
        };
        std::fs::write(&path, pretty_json(&artifact)).unwrap();

        let docs = read_plugin_docs(&path).unwrap();
        assert_eq!(docs.plugin_path, "foo");
        assert_eq!(docs.command_name.as_deref(), Some("Foo"));
    }

    #[test]
    fn read_plugin_docs_errors_on_bad_json_artifact() {
        use super::read_plugin_docs;
        use crate::core::error::RuntimeError;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("plug.json");
        std::fs::write(&path, "{ not valid").unwrap();
        assert!(matches!(
            read_plugin_docs(&path),
            Err(RuntimeError::ArtifactIndexParse { .. })
        ));
    }

    // ---------- write_pretty_json error branch ----------

    #[test]
    fn write_pretty_json_errors_when_path_has_no_parent() {
        use super::write_pretty_json;
        use crate::core::error::RuntimeError;
        use std::path::Path;
        // Root path has no parent directory.
        let err = write_pretty_json(Path::new("/"), &serde_json::json!({"k": 1}));
        assert!(matches!(err, Err(RuntimeError::Invariant { .. })));
    }

    // ---------- stage_then_rename_file error branch ----------

    #[test]
    fn stage_then_rename_errors_when_source_missing() {
        use super::stage_then_rename_file;
        use crate::core::error::RuntimeError;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("nope.so");
        let dst = dir.path().join("out.so");
        assert!(matches!(
            stage_then_rename_file(&src, &dst),
            Err(RuntimeError::Io { .. })
        ));
    }

    // ---------- run_command ----------

    #[test]
    fn run_command_returns_stdout_on_success() {
        use super::run_command;
        let out = run_command("echo", &["hello".to_string()], None).unwrap();
        assert_eq!(String::from_utf8_lossy(&out).trim(), "hello");
    }

    #[test]
    fn run_command_maps_nonzero_exit_to_command_failed() {
        use super::run_command;
        use crate::core::error::RuntimeError;
        assert!(matches!(
            run_command("false", &[], None),
            Err(RuntimeError::CommandFailed { .. })
        ));
    }

    #[test]
    fn run_command_maps_spawn_failure_to_command_failed() {
        use super::run_command;
        use crate::core::error::RuntimeError;
        let err = run_command("cordis-no-such-program-xyz", &[], None);
        assert!(matches!(err, Err(RuntimeError::CommandFailed { .. })));
    }

    // ---------- run_command_with_timeout ----------

    #[test]
    fn run_command_with_timeout_returns_quickly_for_fast_command() {
        use super::run_command_with_timeout;
        use std::process::Command;
        let mut cmd = Command::new("echo");
        cmd.arg("done");
        let output = run_command_with_timeout(cmd, std::time::Duration::from_secs(5)).unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "done");
    }

    #[test]
    fn run_command_with_timeout_kills_and_errors_on_expiry() {
        use super::run_command_with_timeout;
        use crate::core::error::RuntimeError;
        use std::process::Command;
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let err = run_command_with_timeout(cmd, std::time::Duration::from_millis(150));
        match err {
            Err(RuntimeError::InvalidArgument { message }) => {
                assert!(message.contains("timeout"), "unexpected message: {message}");
            }
            other => panic!("expected timeout error, got {other:?}"),
        }
    }

    // ---------- lock lifecycle ----------

    #[test]
    fn artifact_build_lock_writes_state_and_drops_file() {
        use super::{ArtifactBuildLock, ArtifactBuildLockState, BUILD_LOCK_FILE};
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join(BUILD_LOCK_FILE);
        {
            let _lock = ArtifactBuildLock::acquire(dir.path()).unwrap();
            assert!(lock_path.exists(), "lock file should exist while held");
            let text = std::fs::read_to_string(&lock_path).unwrap();
            let state: ArtifactBuildLockState = serde_json::from_str(&text).unwrap();
            assert_eq!(state.pid, std::process::id());
        }
        // Drop released the lock file.
        assert!(!lock_path.exists(), "lock file should be removed on drop");
    }

    #[test]
    fn artifact_build_lock_reclaims_dead_pid_lock() {
        use super::current_epoch_ms;
        use super::{ArtifactBuildLock, ArtifactBuildLockState, BUILD_LOCK_FILE};
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let lock_path = dir.path().join(BUILD_LOCK_FILE);
        // Plant a lock owned by a dead pid.
        let stale = ArtifactBuildLockState {
            pid: u32::MAX - 1,
            created_at_ms: current_epoch_ms(),
        };
        std::fs::write(&lock_path, serde_json::to_vec(&stale).unwrap()).unwrap();
        // Acquire should reclaim it promptly rather than time out.
        let _lock = ArtifactBuildLock::acquire(dir.path()).unwrap();
        let text = std::fs::read_to_string(&lock_path).unwrap();
        let state: ArtifactBuildLockState = serde_json::from_str(&text).unwrap();
        assert_eq!(state.pid, std::process::id());
    }

    #[test]
    fn maybe_remove_stale_lock_noop_when_missing() {
        use super::maybe_remove_stale_lock;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        // No file -> Ok, no error.
        maybe_remove_stale_lock(&dir.path().join("absent.lock")).unwrap();
    }

    #[test]
    fn maybe_remove_stale_lock_keeps_live_pid_lock() {
        use super::current_epoch_ms;
        use super::{maybe_remove_stale_lock, ArtifactBuildLockState};
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("live.lock");
        let live = ArtifactBuildLockState {
            pid: std::process::id(),
            created_at_ms: current_epoch_ms(),
        };
        std::fs::write(&path, serde_json::to_vec(&live).unwrap()).unwrap();
        maybe_remove_stale_lock(&path).unwrap();
        assert!(path.exists(), "a live-pid, fresh lock must be preserved");
    }

    #[test]
    fn maybe_remove_stale_lock_removes_dead_pid_lock() {
        use super::current_epoch_ms;
        use super::{maybe_remove_stale_lock, ArtifactBuildLockState};
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("dead.lock");
        let dead = ArtifactBuildLockState {
            pid: u32::MAX - 1,
            created_at_ms: current_epoch_ms(),
        };
        std::fs::write(&path, serde_json::to_vec(&dead).unwrap()).unwrap();
        maybe_remove_stale_lock(&path).unwrap();
        assert!(!path.exists(), "a dead-pid lock must be reclaimed");
    }

    #[test]
    fn maybe_remove_stale_lock_removes_legacy_old_nonjson_file() {
        use super::maybe_remove_stale_lock;
        use std::time::{Duration, SystemTime};
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("legacy.lock");
        std::fs::write(&path, "not json at all").unwrap();
        // Backdate mtime well past LEGACY_STALE_LOCK_TIMEOUT (30s) via std's
        // File::set_modified (no external crate needed).
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_modified(SystemTime::now() - Duration::from_secs(120))
            .unwrap();
        drop(file);
        maybe_remove_stale_lock(&path).unwrap();
        assert!(!path.exists(), "an ancient non-JSON lock must be reclaimed");
    }

    #[test]
    fn maybe_remove_stale_lock_keeps_fresh_legacy_file() {
        use super::maybe_remove_stale_lock;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("legacy_fresh.lock");
        std::fs::write(&path, "not json at all").unwrap();
        // Just-created mtime is within the legacy timeout -> kept.
        maybe_remove_stale_lock(&path).unwrap();
        assert!(path.exists(), "a fresh non-JSON lock must be preserved");
    }

    // ---------- fixture lockfile cleanup ----------

    #[test]
    fn remove_lockfiles_recursively_deletes_cargo_lock_but_skips_target() {
        use super::remove_lockfiles_recursively;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("qq");
        std::fs::create_dir_all(root.join("child")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("Cargo.lock"), "l").unwrap();
        std::fs::write(root.join("child/Cargo.lock"), "l").unwrap();
        std::fs::write(root.join("target/Cargo.lock"), "l").unwrap();
        std::fs::write(root.join("keep.txt"), "k").unwrap();

        remove_lockfiles_recursively(&root).unwrap();

        assert!(!root.join("Cargo.lock").exists());
        assert!(!root.join("child/Cargo.lock").exists());
        assert!(
            root.join("target/Cargo.lock").exists(),
            "target/ is skipped"
        );
        assert!(root.join("keep.txt").exists());
    }

    #[test]
    #[serial_test::serial]
    fn cleanup_fixture_lockfiles_is_gated_by_env() {
        use super::cleanup_fixture_lockfiles;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        let plugins_root = dir.path().join("plugins");
        std::fs::create_dir_all(plugins_root.join("qq")).unwrap();
        std::fs::write(plugins_root.join("qq/Cargo.lock"), "l").unwrap();

        // Without the opt-in env var, lock files are left alone.
        std::env::remove_var("CORDIS_CLEAN_FIXTURE_LOCKFILES");
        cleanup_fixture_lockfiles(&plugins_root).unwrap();
        assert!(plugins_root.join("qq/Cargo.lock").exists());

        // With the opt-in, they are removed.
        std::env::set_var("CORDIS_CLEAN_FIXTURE_LOCKFILES", "1");
        cleanup_fixture_lockfiles(&plugins_root).unwrap();
        std::env::remove_var("CORDIS_CLEAN_FIXTURE_LOCKFILES");
        assert!(!plugins_root.join("qq/Cargo.lock").exists());
    }

    #[test]
    fn cleanup_fixture_lockfiles_noop_for_missing_root() {
        use super::cleanup_fixture_lockfiles;
        use tempfile::TempDir;
        let dir = TempDir::new().unwrap();
        cleanup_fixture_lockfiles(&dir.path().join("does-not-exist")).unwrap();
    }
}
