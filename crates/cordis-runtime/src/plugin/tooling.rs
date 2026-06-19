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
    plugin_path: Option<&str>,
) -> Result<Vec<(String, String)>, RuntimeError> {
    match plugin_path {
        Some(name) => {
            let mut cmd = std::process::Command::new("cargo");
            cmd.arg("build")
                .arg("--manifest-path")
                .arg(workspace_root.join("plugins").join("Cargo.toml"))
                .arg("-p").arg(name);
            let output = cmd.output().map_err(|e| RuntimeError::InvalidArgument {
                message: format!("cargo build failed to start: {e}"),
            })?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(RuntimeError::InvalidArgument {
                    message: format!("cargo build -p {name} failed: {stderr}"),
                });
            }
            // Sync the .so.
            let target_dir = workspace_root.join("plugins").join("target").join("debug");
            let src = target_dir.join(format!("lib{}.so", name.replace('-', "_")));
            let dst = workspace_root.join("artifacts").join(format!("{}.so", name));
            let _ = std::fs::create_dir_all(dst.parent().unwrap());
            std::fs::copy(&src, &dst).map_err(|e| RuntimeError::Io {
                path: dst.clone(),
                message: format!("{e}"),
            })?;
            Ok(vec![(name.to_string(), format!("{} -> {}", src.display(), dst.display()))])
        }
        None => rebuild_fixture_artifacts(workspace_root),
    }
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
        fs::write(&docs_path, pretty_json(&entry.docs)).map_err(|e| RuntimeError::Io {
            path: docs_path.clone(),
            message: e.to_string(),
        })?;
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

    fs::write(&artifact_index_path, pretty_json(&index)).map_err(|e| RuntimeError::Io {
        path: artifact_index_path,
        message: e.to_string(),
    })?;

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
        .any(|context| existing_map.get(&context.plugin.plugin_path).is_none() || context.dirty)
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
    fs::write(&artifact_index_path, pretty_json(&next_index)).map_err(|e| RuntimeError::Io {
        path: artifact_index_path,
        message: e.to_string(),
    })?;
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
        fs::copy(&built_path, &context.artifact_path).map_err(|e| RuntimeError::Io {
            path: context.artifact_path.clone(),
            message: format!("copy from {} failed: {e}", built_path.display()),
        })?;

        let (docs, runtime_fingerprint) = inspect_dylib_contract(&context.artifact_path)?;
        if runtime_fingerprint != context.plugin.metadata.abi_fingerprint {
            return Err(RuntimeError::AbiMismatch {
                plugin_path: context.plugin.plugin_path.clone(),
                expected: context.plugin.metadata.abi_fingerprint.clone(),
                actual: runtime_fingerprint.clone(),
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
        let modified_at_ms = metadata
            .modified()
            .unwrap_or(UNIX_EPOCH)
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
            .map(|lib| lib.crate_type.iter().any(|kind| kind == "dylib" || kind == "cdylib"))
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
}

fn write_pretty_json<T: serde::Serialize>(path: &Path, value: &T) -> Result<(), RuntimeError> {
    let parent = path.parent().ok_or_else(|| RuntimeError::Invariant {
        message: format!("json path missing parent: {}", path.display()),
    })?;
    fs::create_dir_all(parent).map_err(|e| RuntimeError::Io {
        path: parent.to_path_buf(),
        message: e.to_string(),
    })?;
    fs::write(path, pretty_json(value)).map_err(|e| RuntimeError::Io {
        path: path.to_path_buf(),
        message: e.to_string(),
    })
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
    let modified = metadata.modified().unwrap_or(SystemTime::now());

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

#[cfg(unix)]
fn lock_pid_is_live(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

#[cfg(not(unix))]
fn lock_pid_is_live(_pid: u32) -> bool {
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
