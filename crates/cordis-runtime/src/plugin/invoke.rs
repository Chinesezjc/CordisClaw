use crate::core::error::RuntimeError;
use crate::core::models::{PluginExecution, PluginLoadResult};
use crate::plugin::abi::{PluginRequest, PluginResponse};
use crate::plugin::artifact::load_plugin_artifact;
use crate::plugin::dynamic::{is_dylib_path, LoadedDylibApi};
use crate::plugin::loader::{default_loader_config, Loader};
use crate::plugin::registry::PluginRegistry;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone)]
pub struct PluginInvoker {
    fixtures_root: PathBuf,
    plugin_registry: PluginRegistry,
}

impl PluginInvoker {
    pub fn load(fixtures_root: impl AsRef<Path>) -> Result<Self, RuntimeError> {
        let fixtures_root = fixtures_root.as_ref().to_path_buf();
        let loader = Loader::new(default_loader_config(&fixtures_root));
        let output = loader.load()?;
        Ok(Self {
            fixtures_root,
            plugin_registry: output.plugin_registry,
        })
    }

    pub fn default_fixtures_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures")
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from("fixtures"))
    }

    pub fn fixtures_root(&self) -> &Path {
        &self.fixtures_root
    }

    pub fn plugin_registry(&self) -> &PluginRegistry {
        &self.plugin_registry
    }

    pub fn invoke(
        &self,
        plugin_path: &str,
        node_id: &str,
        payload: String,
    ) -> Result<PluginResponse, RuntimeError> {
        invoke_registered_plugin(&self.plugin_registry, plugin_path, node_id, payload)
    }
}

pub fn invoke_registered_plugin(
    plugin_registry: &PluginRegistry,
    plugin_path: &str,
    node_id: &str,
    payload: String,
) -> Result<PluginResponse, RuntimeError> {
    let plugin = plugin_registry
        .get(plugin_path)
        .ok_or_else(|| RuntimeError::PluginNotRegistered {
            plugin_path: plugin_path.to_string(),
        })?;

    match &plugin.load_result {
        PluginLoadResult::Loaded => {}
        PluginLoadResult::Unavailable(reason) => {
            return Err(RuntimeError::PluginUnavailable {
                plugin_path: plugin_path.to_string(),
                reason: reason.clone(),
                required: plugin.required,
            });
        }
    }

    let docs = plugin
        .docs
        .as_ref()
        .ok_or_else(|| RuntimeError::PluginDocsNotFound {
            plugin_path: plugin_path.to_string(),
        })?;
    if !docs.nodes.iter().any(|node| node.id == node_id) {
        return Err(RuntimeError::NodeDocsNotFound {
            plugin_path: plugin_path.to_string(),
            node_id: node_id.to_string(),
        });
    }

    let artifact_path = plugin.artifact_path.as_ref().ok_or_else(|| RuntimeError::Invariant {
        message: format!("loaded plugin missing artifact path: {plugin_path}"),
    })?;

    if !is_dylib_path(artifact_path) {
        let artifact = load_plugin_artifact(artifact_path)?;
        return invoke_json_artifact(plugin_path, artifact_path, artifact, payload);
    }

    let dylib = LoadedDylibApi::open(artifact_path)?;
    let api = dylib.api();
    Ok((api.handle)(PluginRequest { payload }))
}

fn invoke_json_artifact(
    plugin_path: &str,
    artifact_path: &Path,
    artifact: crate::core::models::PluginArtifact,
    payload: String,
) -> Result<PluginResponse, RuntimeError> {
    match artifact.execution {
        Some(PluginExecution::Process { command, args }) => {
            let command_path = resolve_exec_path(artifact_path, &command);
            let mut child = Command::new(&command_path)
                .args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .map_err(|e| RuntimeError::PluginInvocationFailed {
                    plugin_path: plugin_path.to_string(),
                    message: format!("spawn {} failed: {e}", command_path.display()),
                })?;

            if let Some(stdin) = child.stdin.as_mut() {
                stdin
                    .write_all(payload.as_bytes())
                    .map_err(|e| RuntimeError::PluginInvocationFailed {
                        plugin_path: plugin_path.to_string(),
                        message: format!("write stdin failed: {e}"),
                    })?;
            }

            let output = child
                .wait_with_output()
                .map_err(|e| RuntimeError::PluginInvocationFailed {
                    plugin_path: plugin_path.to_string(),
                    message: format!("wait failed: {e}"),
                })?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                return Err(RuntimeError::PluginInvocationFailed {
                    plugin_path: plugin_path.to_string(),
                    message: if stderr.is_empty() {
                        format!("process exited with status {}", output.status)
                    } else {
                        stderr
                    },
                });
            }

            let stdout = String::from_utf8(output.stdout).map_err(|e| {
                RuntimeError::PluginInvocationFailed {
                    plugin_path: plugin_path.to_string(),
                    message: format!("stdout was not utf-8: {e}"),
                }
            })?;

            Ok(PluginResponse {
                payload: stdout.trim().to_string(),
            })
        }
        None => Err(RuntimeError::PluginExecutionUnsupported {
            plugin_path: plugin_path.to_string(),
            artifact_path: artifact_path.to_path_buf(),
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
