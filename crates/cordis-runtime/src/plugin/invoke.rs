use cordis_plugin_sdk::NodeType;
use libloading;
use crate::core::error::RuntimeError;
use crate::core::models::{
    ArtifactKind, DylibAbiKind, PluginExecution, PluginLoadResult, PluginUnavailableReason,
};
use crate::plugin::abi::{PluginRequest, PluginResponse};
use crate::plugin::dynamic::{is_dylib_path, LoadedDylibApi};
use crate::plugin::loader::{default_loader_config, Loader};
use crate::plugin::registry::PluginRegistry;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Keep-alive storage for dylib Libraries of Task nodes.
/// These libraries must not be dropped because the plugin's
/// background threads (HTTP servers, pollers) run code from them.
static TASK_LIBRARIES: Mutex<Vec<LoadedDylibApi>> = Mutex::new(Vec::new());
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
    let plugin =
        plugin_registry
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

    let artifact_path = plugin
        .artifact_path
        .as_ref()
        .ok_or_else(|| RuntimeError::Invariant {
            message: format!("loaded plugin missing artifact path: {plugin_path}"),
        })?;

    let artifact_kind = plugin
        .artifact_kind
        .clone()
        .ok_or_else(|| RuntimeError::Invariant {
            message: format!("loaded plugin missing artifact kind: {plugin_path}"),
        })?;

    if !matches!(artifact_kind, ArtifactKind::Dylib) && !is_dylib_path(artifact_path) {
        let execution = plugin.execution.clone();
        return invoke_json_artifact(plugin_path, artifact_path, execution, payload);
    }

    let dylib = match LoadedDylibApi::open(artifact_path) {
        Ok(dylib) => dylib,
        Err(err) => {
            plugin_registry.mark_runtime_unavailable(
                plugin_path,
                PluginUnavailableReason::SymbolMissing,
                vec![err.to_string()],
            );
            return Err(err);
        }
    };
    let api = dylib.api();
    if api.abi_kind != DylibAbiKind::Rust {
        plugin_registry.mark_runtime_unavailable(
            plugin_path,
            PluginUnavailableReason::AbiMismatch,
            vec!["runtime exported abi_kind is not rust".to_string()],
        );
        return Err(RuntimeError::PluginUnavailable {
            plugin_path: plugin_path.to_string(),
            reason: PluginUnavailableReason::AbiMismatch,
            required: plugin.required,
        });
    }

    let expected_fingerprint =
        plugin
            .abi_fingerprint
            .clone()
            .ok_or_else(|| RuntimeError::Invariant {
                message: format!("loaded plugin missing abi_fingerprint: {plugin_path}"),
            })?;
    let runtime_fingerprint =
        serde_json::from_str(&(api.abi_fingerprint)().payload).map_err(|err| RuntimeError::Io {
            path: artifact_path.to_path_buf(),
            message: format!("runtime fingerprint parse failed: {err}"),
        })?;
    if runtime_fingerprint != expected_fingerprint {
        let diff = expected_fingerprint.diff(&runtime_fingerprint);
        plugin_registry.mark_runtime_unavailable(
            plugin_path,
            PluginUnavailableReason::AbiMismatch,
            diff.clone(),
        );
        return Err(RuntimeError::AbiMismatch {
            plugin_path: plugin_path.to_string(),
            expected: expected_fingerprint,
            actual: runtime_fingerprint,
            fingerprint_diff: diff,
        });
    }

    let runtime_docs =
        serde_json::from_str(&(api.docs)().payload).map_err(|err| RuntimeError::Io {
            path: artifact_path.to_path_buf(),
            message: format!("runtime docs parse failed: {err}"),
        })?;
    if plugin.docs.as_ref() != Some(&runtime_docs) {
        plugin_registry.mark_runtime_unavailable(
            plugin_path,
            PluginUnavailableReason::ContractViolation,
            vec!["runtime docs mismatch".to_string()],
        );
        return Err(RuntimeError::PluginUnavailable {
            plugin_path: plugin_path.to_string(),
            reason: PluginUnavailableReason::ContractViolation,
            required: plugin.required,
        });
    }

    // Inject node_id into the payload so plugins don't need to duplicate it.
    let mut payload_value: serde_json::Value = serde_json::from_str(&payload)
        .unwrap_or(serde_json::Value::Null);
    if let Some(obj) = payload_value.as_object_mut() {
        obj.entry("node_id").or_insert_with(|| serde_json::json!(node_id));
    }
    let payload = serde_json::to_string(&payload_value).unwrap_or(payload);

    let response = (api.handle)(PluginRequest { payload });

    // For Task nodes: keep the dylib alive and look up the Service VTable.
    let is_task = docs
        .nodes
        .iter()
        .any(|n| n.id == node_id && n.node_type == NodeType::Task);
    if is_task {
        // Look up Service VTable from the plugin.
        let c_node = std::ffi::CString::new(node_id).unwrap_or_default();
        let create_sym: Result<
            libloading::Symbol<
                unsafe extern "C" fn(
                    *const std::ffi::c_char,
                ) -> *const cordis_plugin_sdk::ServiceVTable,
            >,
            _,
        > = unsafe { dylib.lib().get(b"_cordis_create_service\0") };
        if let Ok(create) = create_sym {
            let vtable = unsafe { create(c_node.as_ptr()) };
            if !vtable.is_null() {
                let vtable = unsafe { &*vtable };
                eprintln!(
                    "service: {plugin_path}::{node_id} registered (start={})",
                    (vtable.start)(vtable.data) == 0
                );
            }
        }
        // Keep dylib alive — Task nodes spawn background threads.
        TASK_LIBRARIES.lock().unwrap().push(dylib);
    }

    Ok(response)
}

fn invoke_json_artifact(
    plugin_path: &str,
    artifact_path: &Path,
    execution: Option<PluginExecution>,
    payload: String,
) -> Result<PluginResponse, RuntimeError> {
    match execution {
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
                stdin.write_all(payload.as_bytes()).map_err(|e| {
                    RuntimeError::PluginInvocationFailed {
                        plugin_path: plugin_path.to_string(),
                        message: format!("write stdin failed: {e}"),
                    }
                })?;
            }

            let output =
                child
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
