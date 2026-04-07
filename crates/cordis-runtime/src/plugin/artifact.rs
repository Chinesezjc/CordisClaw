use crate::core::error::RuntimeError;
use crate::core::models::{
    ArtifactIndex, ArtifactIndexEntry, PluginArtifact, PluginExecution,
    ARTIFACT_INDEX_SCHEMA_VERSION,
};
use crate::plugin::dynamic::{is_dylib_path, sidecar_json_path};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

pub fn load_artifact_index(path: &Path) -> Result<ArtifactIndex, RuntimeError> {
    let text = fs::read_to_string(path).map_err(|e| RuntimeError::Io {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;

    let index = serde_json::from_str::<ArtifactIndex>(&text).map_err(|e| {
        RuntimeError::ArtifactIndexParse {
            path: path.to_path_buf(),
            message: e.to_string(),
        }
    })?;
    if index.schema_version != ARTIFACT_INDEX_SCHEMA_VERSION {
        return Err(RuntimeError::ArtifactIndexParse {
            path: path.to_path_buf(),
            message: format!(
                "unsupported schema_version {}, expected {}",
                index.schema_version, ARTIFACT_INDEX_SCHEMA_VERSION
            ),
        });
    }
    Ok(index)
}

pub fn artifact_index_map(index: &ArtifactIndex) -> BTreeMap<String, ArtifactIndexEntry> {
    index
        .entries
        .iter()
        .cloned()
        .map(|entry| (entry.plugin_path.clone(), entry))
        .collect()
}

pub fn resolve_artifact_path(index_path: &Path, artifact_path: &str) -> PathBuf {
    let candidate = Path::new(artifact_path);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        index_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(candidate)
    }
}

pub fn sha256_file(path: &Path) -> Result<String, RuntimeError> {
    let mut file = fs::File::open(path).map_err(|e| RuntimeError::Io {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;

    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 8192];
    loop {
        let read = file.read(&mut buffer).map_err(|e| RuntimeError::Io {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

pub fn load_plugin_artifact(path: &Path) -> Result<PluginArtifact, RuntimeError> {
    let text = fs::read_to_string(path).map_err(|e| RuntimeError::Io {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;

    serde_json::from_str::<PluginArtifact>(&text).map_err(|e| RuntimeError::ArtifactIndexParse {
        path: path.to_path_buf(),
        message: format!("artifact parse failed: {e}"),
    })
}

pub fn stage_artifact_bundle(
    plugin_path: &str,
    artifact_reference: &str,
    artifact_path: &Path,
    staged_root: &Path,
) -> Result<PathBuf, RuntimeError> {
    let staged_artifact_path =
        staged_artifact_path(plugin_path, artifact_reference, artifact_path, staged_root)?;
    stage_file(artifact_path, &staged_artifact_path)?;

    if is_dylib_path(artifact_path) {
        let original_sidecar = sidecar_json_path(artifact_path);
        if original_sidecar.exists() {
            let staged_sidecar = sidecar_json_path(&staged_artifact_path);
            stage_file(&original_sidecar, &staged_sidecar)?;
        }
        return Ok(staged_artifact_path);
    }

    let artifact = load_plugin_artifact(artifact_path)?;
    if let Some(PluginExecution::Process { command, .. }) = artifact.execution {
        stage_process_command(artifact_path, &staged_artifact_path, &command, staged_root)?;
    }

    Ok(staged_artifact_path)
}

fn staged_artifact_path(
    plugin_path: &str,
    artifact_reference: &str,
    artifact_path: &Path,
    staged_root: &Path,
) -> Result<PathBuf, RuntimeError> {
    let artifact_ref = Path::new(artifact_reference);
    let relative = if artifact_ref.is_absolute() {
        let file_name = artifact_path
            .file_name()
            .ok_or_else(|| RuntimeError::Invariant {
                message: format!(
                    "artifact path missing file name for plugin {plugin_path}: {}",
                    artifact_path.display()
                ),
            })?;
        PathBuf::from(plugin_path.replace('/', std::path::MAIN_SEPARATOR_STR)).join(file_name)
    } else {
        artifact_ref.to_path_buf()
    };

    Ok(staged_root.join(relative))
}

fn stage_process_command(
    original_artifact_path: &Path,
    staged_artifact_path: &Path,
    command: &str,
    staged_root: &Path,
) -> Result<(), RuntimeError> {
    let command_path = Path::new(command);
    if command_path.is_absolute() {
        return Ok(());
    }

    let source_path = original_artifact_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(command_path);
    let target_path = staged_artifact_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(command_path);

    let target_parent = target_path
        .parent()
        .ok_or_else(|| RuntimeError::Invariant {
            message: format!(
                "staged process command missing parent: {}",
                target_path.display()
            ),
        })?;
    fs::create_dir_all(target_parent).map_err(|e| RuntimeError::Io {
        path: target_parent.to_path_buf(),
        message: e.to_string(),
    })?;

    let normalized_target = target_parent
        .canonicalize()
        .unwrap_or_else(|_| target_parent.to_path_buf());
    let normalized_root = staged_root
        .canonicalize()
        .unwrap_or_else(|_| staged_root.to_path_buf());
    if !normalized_target.starts_with(&normalized_root) {
        return Err(RuntimeError::Invariant {
            message: format!(
                "staged process command escapes snapshot root: {}",
                target_path.display()
            ),
        });
    }

    stage_file(&source_path, &target_path)
}

fn stage_file(source: &Path, target: &Path) -> Result<(), RuntimeError> {
    if source == target {
        return Ok(());
    }

    let target_parent = target.parent().ok_or_else(|| RuntimeError::Invariant {
        message: format!("staged artifact missing parent: {}", target.display()),
    })?;
    fs::create_dir_all(target_parent).map_err(|e| RuntimeError::Io {
        path: target_parent.to_path_buf(),
        message: e.to_string(),
    })?;

    if target.exists() {
        fs::remove_file(target).map_err(|e| RuntimeError::Io {
            path: target.to_path_buf(),
            message: e.to_string(),
        })?;
    }

    fs::copy(source, target)
        .map(|_| ())
        .map_err(|e| RuntimeError::Io {
            path: target.to_path_buf(),
            message: e.to_string(),
        })
}
