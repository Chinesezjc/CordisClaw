use crate::core::error::RuntimeError;
use crate::core::models::{ArtifactIndex, ArtifactIndexEntry, PluginArtifact, PluginDocs, PluginExecution};
use crate::plugin::artifact::{
    load_artifact_index, load_plugin_artifact, resolve_artifact_path, sha256_file,
};
use crate::plugin::dynamic::{is_dylib_path, LoadedDylibApi};
use crate::plugin::package::PackageResolver;
use cordis_plugin_sdk::pretty_json;
use serde::Deserialize;
use std::env::consts::{DLL_EXTENSION, DLL_PREFIX};
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const BUILD_LOCK_FILE: &str = ".artifacts-build.lock";
const STALE_LOCK_TIMEOUT: Duration = Duration::from_secs(300);

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

#[derive(Debug)]
struct PluginBuildSpec {
    package_name: String,
    version: String,
    is_dylib: bool,
    artifact: SourceArtifactConfig,
}

#[derive(Debug, Deserialize)]
struct CargoMetadataOutput {
    target_directory: String,
}

struct ArtifactBuildLock {
    path: PathBuf,
}

impl Drop for ArtifactBuildLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub fn ensure_fixture_artifacts(fixtures_root: &Path) -> Result<bool, RuntimeError> {
    let fixtures_root = absolute_path(fixtures_root)?;
    if !can_rebuild_fixture_artifacts(&fixtures_root) {
        return Ok(false);
    }

    if !fixture_artifacts_need_rebuild(&fixtures_root)? {
        return Ok(false);
    }

    let _lock = ArtifactBuildLock::acquire(&fixtures_root)?;
    if !fixture_artifacts_need_rebuild(&fixtures_root)? {
        return Ok(false);
    }

    rebuild_fixture_artifacts(&fixtures_root)?;
    Ok(true)
}

pub fn rebuild_fixture_artifacts(fixtures_root: &Path) -> Result<Vec<(String, String)>, RuntimeError> {
    let fixtures_root = absolute_path(fixtures_root)?;
    if !can_rebuild_fixture_artifacts(&fixtures_root) {
        return Err(RuntimeError::Invariant {
            message: format!(
                "fixture rebuild requires repo sources next to {}",
                fixtures_root.display()
            ),
        });
    }

    let plugins_root = fixtures_root.join("plugins");
    let artifacts_dir = fixtures_root.join("artifacts");
    let graph = PackageResolver::new(&plugins_root).resolve()?;
    let built_at = current_build_marker();

    if artifacts_dir.exists() {
        fs::remove_dir_all(&artifacts_dir).map_err(|e| RuntimeError::Io {
            path: artifacts_dir.clone(),
            message: e.to_string(),
        })?;
    }
    fs::create_dir_all(&artifacts_dir).map_err(|e| RuntimeError::Io {
        path: artifacts_dir.clone(),
        message: e.to_string(),
    })?;

    let mut entries = Vec::new();
    for plugin_path in &graph.topo_order {
        let plugin = graph
            .plugins
            .get(plugin_path)
            .ok_or_else(|| RuntimeError::Invariant {
                message: format!("missing plugin in resolved graph: {plugin_path}"),
            })?;
        let manifest_path = plugin.dir.join("Cargo.toml");
        let build_spec = read_plugin_build_spec(&manifest_path)?;
        let artifact_name = expected_artifact_name(plugin_path, build_spec.is_dylib);
        let artifact_path = artifacts_dir.join(&artifact_name);

        if build_spec.is_dylib {
            build_plugin_artifact(&fixtures_root, &manifest_path)?;
            let built_path = built_dylib_path(&manifest_path, &build_spec.package_name)?;
            fs::copy(&built_path, &artifact_path).map_err(|e| RuntimeError::Io {
                path: artifact_path.clone(),
                message: format!("copy from {} failed: {e}", built_path.display()),
            })?;

            let docs = read_plugin_docs(&artifact_path)?;
            write_pretty_json(&plugin.dir.join("docs/agent/interfaces.json"), &docs)?;
        } else {
            let artifact = PluginArtifact {
                plugin_path: plugin_path.clone(),
                abi_fingerprint: plugin.metadata.abi_fingerprint.clone(),
                docs: plugin.docs.clone(),
                exports: build_spec.artifact.exports.clone(),
                execution: build_spec.artifact.execution.clone(),
            };
            write_pretty_json(&artifact_path, &artifact)?;
        }

        let hash = sha256_file(&artifact_path)?;
        entries.push(ArtifactIndexEntry {
            plugin_path: plugin_path.clone(),
            version: build_spec.version,
            abi_fingerprint: plugin.metadata.abi_fingerprint.clone(),
            artifact_path: artifact_name,
            sha256: hash,
            built_at: built_at.clone(),
        });
    }

    write_pretty_json(&artifacts_dir.join("index.json"), &ArtifactIndex {
        entries: entries.clone(),
    })?;
    cleanup_fixture_lockfiles(&plugins_root)?;

    Ok(entries
        .into_iter()
        .map(|entry| (entry.plugin_path, entry.sha256))
        .collect())
}

pub fn sync_plugin_docs(fixtures_root: &Path) -> Result<Vec<PathBuf>, RuntimeError> {
    let plugins_root = fixtures_root.join("plugins");
    let artifact_index_path = fixtures_root.join("artifacts/index.json");
    let index_map = load_artifact_index(&artifact_index_path)?;

    let mut written = Vec::new();
    for (plugin_path, index_entry) in index_map {
        let artifact_path = resolve_artifact_path(&artifact_index_path, &index_entry.artifact_path);
        if !is_dylib_path(&artifact_path) {
            continue;
        }

        let docs = read_plugin_docs(&artifact_path)?;
        let docs_path = plugins_root
            .join(plugin_path.replace('/', std::path::MAIN_SEPARATOR_STR))
            .join("docs/agent/interfaces.json");
        let docs_dir = docs_path.parent().ok_or_else(|| RuntimeError::Invariant {
            message: format!("docs path missing parent: {}", docs_path.display()),
        })?;
        fs::create_dir_all(docs_dir).map_err(|e| RuntimeError::Io {
            path: docs_dir.to_path_buf(),
            message: e.to_string(),
        })?;
        fs::write(&docs_path, pretty_json(&docs)).map_err(|e| RuntimeError::Io {
            path: docs_path.clone(),
            message: e.to_string(),
        })?;
        written.push(docs_path);
    }

    Ok(written)
}

pub fn refresh_artifact_index(fixtures_root: &Path) -> Result<Vec<(String, String)>, RuntimeError> {
    let artifact_index_path = fixtures_root.join("artifacts/index.json");
    let text = fs::read_to_string(&artifact_index_path).map_err(|e| RuntimeError::Io {
        path: artifact_index_path.clone(),
        message: e.to_string(),
    })?;
    let mut index: ArtifactIndex = serde_json::from_str(&text).map_err(|e| RuntimeError::ArtifactIndexParse {
        path: artifact_index_path.clone(),
        message: e.to_string(),
    })?;

    let mut refreshed = Vec::new();
    for entry in &mut index.entries {
        let artifact_path = resolve_artifact_path(&artifact_index_path, &entry.artifact_path);
        let hash = sha256_file(&artifact_path)?;
        entry.sha256 = hash.clone();
        refreshed.push((entry.plugin_path.clone(), hash));
    }

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

impl ArtifactBuildLock {
    fn acquire(fixtures_root: &Path) -> Result<Self, RuntimeError> {
        let path = fixtures_root.join(BUILD_LOCK_FILE);

        loop {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_) => return Ok(Self { path }),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    maybe_remove_stale_lock(&path)?;
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

fn fixture_artifacts_need_rebuild(fixtures_root: &Path) -> Result<bool, RuntimeError> {
    let plugins_root = fixtures_root.join("plugins");
    let artifacts_dir = fixtures_root.join("artifacts");
    let artifact_index_path = artifacts_dir.join("index.json");
    if !artifact_index_path.exists() {
        return Ok(true);
    }

    let graph = PackageResolver::new(&plugins_root).resolve()?;
    let index_map = match load_artifact_index(&artifact_index_path) {
        Ok(index) => index,
        Err(_) => return Ok(true),
    };
    if index_map.len() != graph.plugins.len() {
        return Ok(true);
    }

    let index_mtime = modified_time(&artifact_index_path)?;
    for plugin_path in &graph.topo_order {
        let plugin = graph
            .plugins
            .get(plugin_path)
            .ok_or_else(|| RuntimeError::Invariant {
                message: format!("missing plugin in resolved graph: {plugin_path}"),
            })?;
        let manifest_path = plugin.dir.join("Cargo.toml");
        let build_spec = read_plugin_build_spec(&manifest_path)?;
        let artifact_name = expected_artifact_name(plugin_path, build_spec.is_dylib);
        let artifact_path = artifacts_dir.join(&artifact_name);
        if !artifact_path.exists() {
            return Ok(true);
        }

        if modified_time(&artifact_path)? > index_mtime {
            return Ok(true);
        }

        let Some(index_entry) = index_map.get(plugin_path) else {
            return Ok(true);
        };
        if index_entry.version != build_spec.version
            || index_entry.abi_fingerprint != plugin.metadata.abi_fingerprint
            || index_entry.artifact_path != artifact_name
        {
            return Ok(true);
        }

        if sha256_file(&artifact_path)? != index_entry.sha256 {
            return Ok(true);
        }

        if latest_mtime_in_tree(&plugin.dir)? > index_mtime {
            return Ok(true);
        }
    }

    let Some(repo_root) = fixtures_root.parent() else {
        return Ok(false);
    };
    for watch_root in [
        repo_root.join("crates/cordis-plugin-sdk"),
        repo_root.join("crates/cordis-runtime"),
    ] {
        if watch_root.exists() && latest_mtime_in_tree(&watch_root)? > index_mtime {
            return Ok(true);
        }
    }

    Ok(false)
}

fn can_rebuild_fixture_artifacts(fixtures_root: &Path) -> bool {
    let Some(repo_root) = fixtures_root.parent() else {
        return false;
    };
    fixtures_root.join("plugins/Cargo.toml").exists()
        && repo_root.join("crates/cordis-plugin-sdk").exists()
        && repo_root.join("crates/cordis-runtime").exists()
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
            .map(|lib| lib.crate_type.iter().any(|kind| kind == "dylib"))
            .unwrap_or(false),
        artifact: package
            .metadata
            .and_then(|metadata| metadata.cordis)
            .and_then(|cordis| cordis.artifact)
            .unwrap_or_default(),
    })
}

fn build_plugin_artifact(fixtures_root: &Path, manifest_path: &Path) -> Result<(), RuntimeError> {
    let repo_root = fixtures_root.parent().ok_or_else(|| RuntimeError::Invariant {
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
    let mut command = Command::new(program);
    command.args(args);
    if let Some(dir) = current_dir {
        command.current_dir(dir);
    }
    let output = command.output().map_err(|e| RuntimeError::CommandFailed {
        program: program.to_string(),
        args: args.to_vec(),
        message: e.to_string(),
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let message = if stderr.is_empty() { stdout } else { stderr };
        return Err(RuntimeError::CommandFailed {
            program: program.to_string(),
            args: args.to_vec(),
            message,
        });
    }
    Ok(output.stdout)
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
    fs::write(path, pretty_json(value)).map_err(|e| RuntimeError::Io {
        path: path.to_path_buf(),
        message: e.to_string(),
    })
}

fn latest_mtime_in_tree(path: &Path) -> Result<SystemTime, RuntimeError> {
    if !path.exists() {
        return Ok(UNIX_EPOCH);
    }

    let metadata = fs::metadata(path).map_err(|e| RuntimeError::Io {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;
    if !metadata.is_dir() {
        if path.file_name().and_then(|name| name.to_str()) == Some("Cargo.lock") {
            return Ok(UNIX_EPOCH);
        }
        return Ok(metadata.modified().unwrap_or(UNIX_EPOCH));
    }

    let mut latest = UNIX_EPOCH;
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
        let child_latest = latest_mtime_in_tree(&entry.path())?;
        if child_latest > latest {
            latest = child_latest;
        }
    }

    Ok(latest)
}

fn modified_time(path: &Path) -> Result<SystemTime, RuntimeError> {
    fs::metadata(path)
        .map_err(|e| RuntimeError::Io {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?
        .modified()
        .map_err(|e| RuntimeError::Io {
            path: path.to_path_buf(),
            message: e.to_string(),
        })
}

fn maybe_remove_stale_lock(path: &Path) -> Result<(), RuntimeError> {
    let modified = match fs::metadata(path) {
        Ok(metadata) => metadata.modified().unwrap_or(SystemTime::now()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(RuntimeError::Io {
                path: path.to_path_buf(),
                message: err.to_string(),
            });
        }
    };
    if SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default()
        > STALE_LOCK_TIMEOUT
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
