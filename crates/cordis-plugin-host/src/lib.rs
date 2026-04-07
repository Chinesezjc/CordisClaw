use cordis_plugin_sdk::{
    PluginDocs, PluginRequest, PluginResponse, RustPluginApiV2, RUST_PLUGIN_ENTRY_SYMBOL,
};
use libloading::Library;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum PluginHostError {
    #[error("I/O at {path}: {message}")]
    Io { path: PathBuf, message: String },

    #[error("artifact index parse failed at {path}: {message}")]
    ArtifactIndexParse { path: PathBuf, message: String },

    #[error("plugin docs parse failed at {path}: {message}")]
    PluginDocsParse { path: PathBuf, message: String },

    #[error("plugin not found: {plugin_path}")]
    PluginNotFound { plugin_path: String },

    #[error("node docs not found: {plugin_path}::{node_id}")]
    NodeNotFound {
        plugin_path: String,
        node_id: String,
    },

    #[error("plugin invocation failed for {plugin_path}: {message}")]
    PluginInvocationFailed {
        plugin_path: String,
        message: String,
    },

    #[error("plugin execution unsupported for {plugin_path}: artifact={artifact_path}")]
    PluginExecutionUnsupported {
        plugin_path: String,
        artifact_path: PathBuf,
    },
}

#[derive(Debug, Clone)]
pub struct CatalogPlugin {
    pub plugin_path: String,
    pub docs: PluginDocs,
    artifact_path: PathBuf,
    execution: Option<PluginExecution>,
}

#[derive(Debug, Clone)]
pub struct PluginCatalog {
    fixtures_root: PathBuf,
    plugins: BTreeMap<String, CatalogPlugin>,
}

#[derive(Debug, Deserialize)]
struct ArtifactIndex {
    schema_version: u32,
    entries: Vec<ArtifactIndexEntry>,
}

#[derive(Debug, Deserialize)]
struct ArtifactIndexEntry {
    plugin_path: String,
    artifact_path: String,
    docs: PluginDocs,
    execution: Option<PluginExecution>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum PluginExecution {
    Process {
        command: String,
        #[serde(default)]
        args: Vec<String>,
    },
}

impl PluginCatalog {
    pub fn load(fixtures_root: impl AsRef<Path>) -> Result<Self, PluginHostError> {
        let fixtures_root = absolute_path(fixtures_root.as_ref())?;
        let artifact_index_path = fixtures_root.join("artifacts/index.json");
        let artifact_index = load_artifact_index(&artifact_index_path)?;
        if artifact_index.schema_version != 2 {
            return Err(PluginHostError::ArtifactIndexParse {
                path: artifact_index_path,
                message: "unsupported schema_version".to_string(),
            });
        }

        let mut plugins = BTreeMap::new();
        for entry in artifact_index.entries {
            let plugin_path = entry.plugin_path.clone();
            let artifact_path = resolve_artifact_path(&artifact_index_path, &entry.artifact_path);
            plugins.insert(
                plugin_path.clone(),
                CatalogPlugin {
                    plugin_path,
                    docs: entry.docs,
                    artifact_path,
                    execution: entry.execution,
                },
            );
        }

        Ok(Self {
            fixtures_root,
            plugins,
        })
    }

    pub fn fixtures_root(&self) -> &Path {
        &self.fixtures_root
    }

    pub fn plugin(&self, plugin_path: &str) -> Option<&CatalogPlugin> {
        self.plugins.get(plugin_path)
    }

    pub fn plugins(&self) -> impl Iterator<Item = &CatalogPlugin> {
        self.plugins.values()
    }

    pub fn invoke(
        &self,
        plugin_path: &str,
        node_id: &str,
        payload: String,
    ) -> Result<PluginResponse, PluginHostError> {
        let plugin = self
            .plugin(plugin_path)
            .ok_or_else(|| PluginHostError::PluginNotFound {
                plugin_path: plugin_path.to_string(),
            })?;

        if !plugin.docs.nodes.iter().any(|node| node.id == node_id) {
            return Err(PluginHostError::NodeNotFound {
                plugin_path: plugin_path.to_string(),
                node_id: node_id.to_string(),
            });
        }

        invoke_artifact(plugin, payload)
    }
}

pub fn default_fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from("fixtures"))
}

fn load_artifact_index(path: &Path) -> Result<ArtifactIndex, PluginHostError> {
    let text = fs::read_to_string(path).map_err(|err| PluginHostError::Io {
        path: path.to_path_buf(),
        message: err.to_string(),
    })?;
    serde_json::from_str(&text).map_err(|err| PluginHostError::ArtifactIndexParse {
        path: path.to_path_buf(),
        message: err.to_string(),
    })
}

fn resolve_artifact_path(index_path: &Path, artifact_path: &str) -> PathBuf {
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

fn invoke_artifact(
    plugin: &CatalogPlugin,
    payload: String,
) -> Result<PluginResponse, PluginHostError> {
    if is_dylib_path(&plugin.artifact_path) {
        return invoke_dylib(plugin, payload);
    }

    invoke_json_artifact(plugin, payload)
}

fn invoke_dylib(
    plugin: &CatalogPlugin,
    payload: String,
) -> Result<PluginResponse, PluginHostError> {
    let lib =
        unsafe { Library::new(&plugin.artifact_path) }.map_err(|err| PluginHostError::Io {
            path: plugin.artifact_path.clone(),
            message: format!("load dylib failed: {err}"),
        })?;
    let symbol_name = format!("{RUST_PLUGIN_ENTRY_SYMBOL}\0");
    let symbol =
        unsafe { lib.get::<*const RustPluginApiV2>(symbol_name.as_bytes()) }.map_err(|err| {
            PluginHostError::Io {
                path: plugin.artifact_path.clone(),
                message: format!("symbol lookup failed ({RUST_PLUGIN_ENTRY_SYMBOL}): {err}"),
            }
        })?;
    let api_ptr = *symbol;
    if api_ptr.is_null() {
        return Err(PluginHostError::Io {
            path: plugin.artifact_path.clone(),
            message: "symbol resolved to null pointer".to_string(),
        });
    }

    let api = unsafe { &*api_ptr };
    Ok((api.handle)(PluginRequest { payload }))
}

fn invoke_json_artifact(
    plugin: &CatalogPlugin,
    payload: String,
) -> Result<PluginResponse, PluginHostError> {
    match plugin.execution.clone() {
        Some(PluginExecution::Process { command, args }) => {
            let command_path = resolve_exec_path(&plugin.artifact_path, &command);
            let mut child = Command::new(&command_path)
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|err| PluginHostError::PluginInvocationFailed {
                    plugin_path: plugin.plugin_path.clone(),
                    message: format!("spawn {} failed: {err}", command_path.display()),
                })?;

            if let Some(stdin) = child.stdin.as_mut() {
                stdin.write_all(payload.as_bytes()).map_err(|err| {
                    PluginHostError::PluginInvocationFailed {
                        plugin_path: plugin.plugin_path.clone(),
                        message: format!("write stdin failed: {err}"),
                    }
                })?;
            }

            let output = child.wait_with_output().map_err(|err| {
                PluginHostError::PluginInvocationFailed {
                    plugin_path: plugin.plugin_path.clone(),
                    message: format!("wait failed: {err}"),
                }
            })?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                return Err(PluginHostError::PluginInvocationFailed {
                    plugin_path: plugin.plugin_path.clone(),
                    message: if stderr.is_empty() {
                        format!("process exited with status {}", output.status)
                    } else {
                        stderr
                    },
                });
            }

            let stdout = String::from_utf8(output.stdout).map_err(|err| {
                PluginHostError::PluginInvocationFailed {
                    plugin_path: plugin.plugin_path.clone(),
                    message: format!("stdout was not utf-8: {err}"),
                }
            })?;

            Ok(PluginResponse {
                payload: stdout.trim().to_string(),
            })
        }
        None => Err(PluginHostError::PluginExecutionUnsupported {
            plugin_path: plugin.plugin_path.clone(),
            artifact_path: plugin.artifact_path.clone(),
        }),
    }
}

fn resolve_exec_path(artifact_path: &Path, command: &str) -> PathBuf {
    let command_path = Path::new(command);
    if command_path.is_absolute() {
        command_path.to_path_buf()
    } else {
        artifact_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(command_path)
    }
}

fn is_dylib_path(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some("so") | Some("dylib") | Some("dll")
    )
}

fn absolute_path(path: &Path) -> Result<PathBuf, PluginHostError> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .map_err(|err| PluginHostError::Io {
            path: path.to_path_buf(),
            message: err.to_string(),
        })
}
