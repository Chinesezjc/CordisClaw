use crate::core::error::RuntimeError;
use crate::core::models::{ArtifactIndex, ArtifactIndexEntry, PluginArtifact};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

pub fn load_artifact_index(path: &Path) -> Result<BTreeMap<String, ArtifactIndexEntry>, RuntimeError> {
    let text = fs::read_to_string(path).map_err(|e| RuntimeError::Io {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;

    let entries: Vec<ArtifactIndexEntry> = if text.trim_start().starts_with('[') {
        serde_json::from_str(&text).map_err(|e| RuntimeError::ArtifactIndexParse {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?
    } else {
        serde_json::from_str::<ArtifactIndex>(&text)
            .map_err(|e| RuntimeError::ArtifactIndexParse {
                path: path.to_path_buf(),
                message: e.to_string(),
            })?
            .entries
    };

    let mut map = BTreeMap::new();
    for entry in entries {
        map.insert(entry.plugin_path.clone(), entry);
    }
    Ok(map)
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
    let bytes = fs::read(path).map_err(|e| RuntimeError::Io {
        path: path.to_path_buf(),
        message: e.to_string(),
    })?;

    let mut hasher = Sha256::new();
    hasher.update(bytes);
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
