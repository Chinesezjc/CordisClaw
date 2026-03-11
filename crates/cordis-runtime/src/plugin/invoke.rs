use crate::core::error::RuntimeError;
use crate::core::models::{NodeDoc, PluginExecution, PluginLoadResult};
use crate::plugin::abi::{PluginRequest, PluginResponse};
use crate::plugin::artifact::load_plugin_artifact;
use crate::plugin::dynamic::{is_dylib_path, LoadedDylibApi};
use crate::plugin::loader::{default_loader_config, Loader};
use crate::plugin::registry::{PluginRegistry, RegisteredPlugin};
use serde_json::json;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Debug, Clone)]
pub struct PluginInvoker {
    fixtures_root: PathBuf,
    plugin_registry: PluginRegistry,
}

#[derive(Debug, Clone)]
pub struct ShellCommandBinding {
    pub shell_name: String,
    pub plugin_path: String,
    pub node: NodeDoc,
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

    pub fn available_shell_commands(&self) -> Vec<String> {
        let mut commands = self
            .plugin_registry
            .iter()
            .filter_map(|(_, plugin)| shell_command_binding(plugin).ok().flatten())
            .map(|binding| display_shell_name(&binding.shell_name))
            .collect::<Vec<_>>();
        commands.sort();
        commands.dedup();
        commands
    }

    pub fn resolve_shell_command(
        &self,
        command: &str,
    ) -> Result<Option<ShellCommandBinding>, RuntimeError> {
        let mut matches = Vec::new();
        for (_, plugin) in self.plugin_registry.iter() {
            let shell_name = plugin
                .plugin_path
                .rsplit('/')
                .next()
                .unwrap_or(plugin.plugin_path.as_str());
            if !shell_name.eq_ignore_ascii_case(command) {
                continue;
            }
            if let Some(binding) = shell_command_binding(plugin)? {
                matches.push(binding);
            }
        }

        match matches.len() {
            0 => Ok(None),
            1 => Ok(matches.into_iter().next()),
            _ => {
                let plugin_paths = matches
                    .iter()
                    .map(|binding| binding.plugin_path.clone())
                    .collect::<Vec<_>>()
                    .join(", ");
                Err(RuntimeError::ShellPluginInvalidRequest {
                    message: format!(
                        "{command}: command is ambiguous across plugins: {plugin_paths}"
                    ),
                })
            }
        }
    }

    pub fn build_shell_payload(
        &self,
        binding: &ShellCommandBinding,
        display_name: &str,
        raw_args: &str,
    ) -> Result<String, RuntimeError> {
        let input_fields = schema_property_names(&binding.node.input_schema);
        let required_fields = required_field_names(&binding.node.input_schema);
        let trimmed = raw_args.trim();

        match input_fields.as_slice() {
            [] => {
                if trimmed.is_empty() {
                    Ok("{}".to_string())
                } else {
                    Err(RuntimeError::ShellPluginInvalidRequest {
                        message: format!("{display_name}: unexpected arguments"),
                    })
                }
            }
            [field] => {
                if trimmed.is_empty() {
                    if required_fields.contains(field) {
                        Err(RuntimeError::ShellPluginInvalidRequest {
                            message: format!("{display_name}: missing {field}"),
                        })
                    } else {
                        Ok("{}".to_string())
                    }
                } else {
                    Ok(json!({ field: trimmed }).to_string())
                }
            }
            _ => Err(RuntimeError::ShellPluginInvalidRequest {
                message: format!(
                    "{display_name}: plugin command requires {} input fields; builtin shell supports only one",
                    input_fields.len()
                ),
            }),
        }
    }

    pub fn invoke(
        &self,
        plugin_path: &str,
        node_id: &str,
        payload: String,
    ) -> Result<PluginResponse, RuntimeError> {
        let plugin = self
            .plugin_registry
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
            return self.invoke_json_artifact(plugin_path, artifact_path, artifact, payload);
        }

        let dylib = LoadedDylibApi::open(artifact_path)?;
        let api = dylib.api();
        Ok((api.handle)(PluginRequest { payload }))
    }

    fn invoke_json_artifact(
        &self,
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
}

fn shell_command_binding(
    plugin: &RegisteredPlugin,
) -> Result<Option<ShellCommandBinding>, RuntimeError> {
    if !matches!(plugin.load_result, PluginLoadResult::Loaded) {
        return Ok(None);
    }
    let Some(docs) = &plugin.docs else {
        return Ok(None);
    };
    if docs.nodes.len() != 1 {
        return Ok(None);
    }
    let Some(artifact_path) = &plugin.artifact_path else {
        return Ok(None);
    };
    let artifact = load_plugin_artifact(artifact_path)?;
    if !matches!(artifact.execution, Some(PluginExecution::Process { .. })) {
        return Ok(None);
    }

    Ok(Some(ShellCommandBinding {
        shell_name: plugin
            .plugin_path
            .rsplit('/')
            .next()
            .unwrap_or(plugin.plugin_path.as_str())
            .to_string(),
        plugin_path: plugin.plugin_path.clone(),
        node: docs.nodes[0].clone(),
    }))
}

fn schema_property_names(schema: &serde_json::Value) -> Vec<String> {
    let Some(properties) = schema.get("properties").and_then(|value| value.as_object()) else {
        return Vec::new();
    };
    let mut names = properties.keys().cloned().collect::<Vec<_>>();
    names.sort();
    names
}

fn required_field_names(schema: &serde_json::Value) -> Vec<String> {
    let Some(required) = schema.get("required").and_then(|value| value.as_array()) else {
        return Vec::new();
    };
    let mut names = required
        .iter()
        .filter_map(|value| value.as_str().map(ToString::to_string))
        .collect::<Vec<_>>();
    names.sort();
    names
}

fn display_shell_name(name: &str) -> String {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => format!("{}{}", first.to_ascii_uppercase(), chars.as_str()),
        None => String::new(),
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
