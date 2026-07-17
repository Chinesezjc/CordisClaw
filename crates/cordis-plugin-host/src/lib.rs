use cordis_plugin_sdk::{
    AbiFingerprint, PluginDocs, PluginRequest, PluginResponse, RustPluginApiV2,
    RUST_PLUGIN_ENTRY_SYMBOL,
};
use libloading::Library;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::ffi::{CStr, CString};
use std::fs;
use std::io::Write;
use std::os::raw::c_char;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use thiserror::Error;

// ── Message Handler Registry ──────────────────────────────────────────────
/// Plugins register a handler function that transforms raw messages into
/// tagged routing items.  The host calls this handler for each message
/// fetched from the plugin's output queue.

type HandlerFn = extern "C" fn(*const c_char) -> *mut c_char;
static HANDLER: OnceLock<HandlerFn> = OnceLock::new();

/// Registered by the runtime at boot.  Plugins export this symbol.
#[no_mangle]
pub extern "C" fn _cordis_register_handler(f: HandlerFn) {
    let _ = HANDLER.set(f);
}

/// Called by the host for each raw message.  Returns a tagged JSON string
/// like `{"tag":"agent","content":"..."}`, or `None` if no handler is
/// registered.
pub fn call_handler(input: &str) -> Option<String> {
    HANDLER.get().map(|f| {
        let c_in = CString::new(input).unwrap();
        let c_out = f(c_in.as_ptr());
        let s = unsafe { CStr::from_ptr(c_out) }.to_string_lossy().into_owned();
        unsafe { libc::free(c_out as *mut std::ffi::c_void) };
        s
    })
}

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

    #[error("plugin ABI mismatch for {plugin_path}: expected {expected:?}, actual {actual:?}")]
    AbiFingerprintMismatch {
        plugin_path: String,
        expected: AbiFingerprint,
        actual: AbiFingerprint,
    },
}

/// Cached, keep-alive dylib handle. Sits alongside `CatalogPlugin` inside an
/// `Arc<Mutex>` so multiple invocations reuse the same mapping — this is the
/// P0-11 fix. Previously `invoke_dylib` called `Library::new` on every
/// invocation and dropped it on return, which unmaps the dylib while the
/// `PluginResponse.payload` heap allocation (owned by the plugin's
/// allocator) is still live in the caller. On any platform where dlclose
/// actually unmaps (musl, custom allocators), that is a use-after-free.
struct LoadedDylib {
    library: Library,
    /// Non-null `*const RustPluginApiV2` inside the (now-resident) dylib.
    /// Safe to dereference while `library` is alive.
    api_ptr: *const RustPluginApiV2,
    /// Whether we've already run the P0-12 fingerprint check for this handle.
    fingerprint_verified: bool,
}

// SAFETY: `Library` is not `Send + Sync` in libloading's type system, but the
// underlying dlopen'd module is process-global; concurrent use through
// separate `&Library` references is fine, and we serialise every access via
// the `Mutex<LoadedDylib>` wrapper.
unsafe impl Send for LoadedDylib {}
unsafe impl Sync for LoadedDylib {}

#[derive(Clone)]
pub struct CatalogPlugin {
    pub plugin_path: String,
    pub docs: PluginDocs,
    artifact_path: PathBuf,
    execution: Option<PluginExecution>,
    /// Expected ABI fingerprint from `index.json`. Optional to keep older
    /// index files loadable; when present it's cross-checked against the
    /// dylib's own `abi_fingerprint()` on first invoke (P0-12).
    expected_abi_fingerprint: Option<AbiFingerprint>,
    /// Lazily loaded dylib handle. `None` for JSON/process artifacts.
    library: Arc<Mutex<Option<LoadedDylib>>>,
}

impl std::fmt::Debug for CatalogPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CatalogPlugin")
            .field("plugin_path", &self.plugin_path)
            .field("artifact_path", &self.artifact_path)
            .field("execution", &self.execution)
            .field("expected_abi_fingerprint", &self.expected_abi_fingerprint)
            .finish_non_exhaustive()
    }
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
    /// Present for entries produced by the current runtime; absent in older
    /// or JSON-only artifact indices. When present, host verifies the loaded
    /// dylib reports the same fingerprint on first invoke (P0-12).
    #[serde(default)]
    abi_fingerprint: Option<AbiFingerprint>,
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
                    expected_abi_fingerprint: entry.abi_fingerprint,
                    library: Arc::new(Mutex::new(None)),
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
    // P0-11: cache the `Library` for the lifetime of `CatalogPlugin` so any
    // heap allocation the plugin returns (its `PluginResponse.payload`
    // `String`) still points at mapped memory after this function returns.
    // The previous implementation dlopen'd + dlclose'd on every invocation,
    // creating a use-after-free window when the returned String owned heap
    // memory from the (now-unmapped) plugin allocator.
    //
    // P0-12: the first time we open the dylib we call its `abi_fingerprint`
    // and compare against the fingerprint the catalog was constructed from
    // (`expected_abi_fingerprint`, populated from `index.json`). On mismatch
    // we return `AbiFingerprintMismatch` and refuse the invocation.
    let mut guard = plugin
        .library
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if guard.is_none() {
        let lib = unsafe { Library::new(&plugin.artifact_path) }.map_err(|err| {
            PluginHostError::Io {
                path: plugin.artifact_path.clone(),
                message: format!("load dylib failed: {err}"),
            }
        })?;
        let symbol_name = format!("{RUST_PLUGIN_ENTRY_SYMBOL}\0");
        let symbol = unsafe { lib.get::<*const RustPluginApiV2>(symbol_name.as_bytes()) }
            .map_err(|err| PluginHostError::Io {
                path: plugin.artifact_path.clone(),
                message: format!("symbol lookup failed ({RUST_PLUGIN_ENTRY_SYMBOL}): {err}"),
            })?;
        let api_ptr = *symbol;
        if api_ptr.is_null() {
            return Err(PluginHostError::Io {
                path: plugin.artifact_path.clone(),
                message: "symbol resolved to null pointer".to_string(),
            });
        }
        // Detach the symbol from the borrow of `lib` — the returned raw ptr
        // remains valid because `lib` is stored below and never dropped.
        drop(symbol);
        *guard = Some(LoadedDylib {
            library: lib,
            api_ptr,
            fingerprint_verified: false,
        });
    }
    let loaded = guard.as_mut().expect("just populated");
    if !loaded.fingerprint_verified {
        if let Some(expected) = plugin.expected_abi_fingerprint.as_ref() {
            let api = unsafe { &*loaded.api_ptr };
            let raw = (api.abi_fingerprint)();
            let actual: AbiFingerprint = serde_json::from_str(&raw.payload).map_err(|err| {
                PluginHostError::PluginInvocationFailed {
                    plugin_path: plugin.plugin_path.clone(),
                    message: format!("abi_fingerprint response was not parseable: {err}"),
                }
            })?;
            if &actual != expected {
                return Err(PluginHostError::AbiFingerprintMismatch {
                    plugin_path: plugin.plugin_path.clone(),
                    expected: expected.clone(),
                    actual,
                });
            }
        }
        loaded.fingerprint_verified = true;
    }
    let api = unsafe { &*loaded.api_ptr };
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
