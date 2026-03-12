use crate::core::error::RuntimeError;
use crate::core::models::{ArtifactIndex, PluginDocs};
use crate::plugin::artifact::{load_artifact_index, load_plugin_artifact, resolve_artifact_path, sha256_file};
use crate::plugin::dynamic::{is_dylib_path, LoadedDylibApi};
use cordis_plugin_sdk::pretty_json;
use std::fs;
use std::path::{Path, PathBuf};

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
