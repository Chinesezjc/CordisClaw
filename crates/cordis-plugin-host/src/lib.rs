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
// Plugins register a handler function that transforms raw messages into
// tagged routing items.  The host calls this handler for each message
// fetched from the plugin's output queue.
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
        let s = unsafe { CStr::from_ptr(c_out) }
            .to_string_lossy()
            .into_owned();
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
        // Boxed to keep the enum small: two inline fingerprints made this
        // the large variant driving clippy::result_large_err across every
        // `Result<_, PluginHostError>`. ABI mismatch is a cold path.
        expected: Box<AbiFingerprint>,
        actual: Box<AbiFingerprint>,
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
    /// Never read directly — held solely to keep the dylib mapped for as
    /// long as `api_ptr` (and any `PluginResponse.payload` allocated by the
    /// plugin) is live. Dropping this field unmaps the module (P0-11).
    #[allow(dead_code)]
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
        let lib =
            unsafe { Library::new(&plugin.artifact_path) }.map_err(|err| PluginHostError::Io {
                path: plugin.artifact_path.clone(),
                message: format!("load dylib failed: {err}"),
            })?;
        let symbol_name = format!("{RUST_PLUGIN_ENTRY_SYMBOL}\0");
        let symbol = unsafe { lib.get::<*const RustPluginApiV2>(symbol_name.as_bytes()) }.map_err(
            |err| PluginHostError::Io {
                path: plugin.artifact_path.clone(),
                message: format!("symbol lookup failed ({RUST_PLUGIN_ENTRY_SYMBOL}): {err}"),
            },
        )?;
        let api_ptr = *symbol;
        // `lib.get` above already returns `Err` for an undefined symbol; a
        // *resolved* symbol's address is its load address, which the dynamic
        // linker guarantees is non-null. libloading exposes exactly this
        // address as `*symbol`, so `api_ptr.is_null()` cannot hold on this
        // path. Encoded as a debug-only invariant rather than a runtime error
        // branch that no input can reach.
        debug_assert!(
            !api_ptr.is_null(),
            "resolved dylib symbol address is never null"
        );
        // Detach the symbol from the borrow of `lib` — the returned raw ptr
        // remains valid because `lib` is stored below and never dropped.
        let _ = symbol;
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
                    expected: Box::new(expected.clone()),
                    actual: Box::new(actual),
                });
            }
        }
        loaded.fingerprint_verified = true;
    }
    let api = unsafe { &*loaded.api_ptr };
    let handle = api.handle;
    let plugin_path_for_msg = plugin.plugin_path.clone();
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        handle(PluginRequest { payload })
    })) {
        Ok(resp) => Ok(resp),
        Err(payload) => {
            let msg = if let Some(s) = payload.downcast_ref::<&'static str>() {
                (*s).to_string()
            } else if let Some(s) = payload.downcast_ref::<String>() {
                s.clone()
            } else {
                "<non-string panic payload>".to_string()
            };
            eprintln!("plugin {plugin_path_for_msg} panicked in handle: {msg}");
            Err(PluginHostError::PluginInvocationFailed {
                plugin_path: plugin_path_for_msg,
                message: format!("plugin handle panicked: {msg}"),
            })
        }
    }
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

            let output = child
                .wait_with_output()
                .map_err(|err| wait_failed_error(&plugin.plugin_path, &err))?;

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
        .map_err(|err| current_dir_error(path, &err))
}

/// Maps a `child.wait_with_output()` failure to `PluginInvocationFailed`.
/// Extracted so the (io-error-only) mapping is unit-testable without having
/// to force a real wait to fail.
fn wait_failed_error(plugin_path: &str, err: &std::io::Error) -> PluginHostError {
    PluginHostError::PluginInvocationFailed {
        plugin_path: plugin_path.to_string(),
        message: format!("wait failed: {err}"),
    }
}

/// Maps a `std::env::current_dir()` failure to `Io`. Extracted so the mapping
/// is unit-testable without having to make the process cwd unreadable.
fn current_dir_error(path: &Path, err: &std::io::Error) -> PluginHostError {
    PluginHostError::Io {
        path: path.to_path_buf(),
        message: err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    // ── temp-fixtures scaffolding ─────────────────────────────────────────
    // No `tempfile` dependency in this crate, so roll a minimal unique dir
    // under the system temp root and clean it up on drop.
    struct TmpFixtures {
        root: PathBuf,
    }

    impl TmpFixtures {
        /// Create `<tmp>/cordis-host-test-<pid>-<n>/artifacts/index.json`
        /// containing `index_json`, and return the fixtures_root (the dir
        /// whose `artifacts/index.json` was written).
        fn with_index(index_json: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let root =
                std::env::temp_dir().join(format!("cordis-host-test-{}-{n}", std::process::id()));
            let artifacts = root.join("artifacts");
            fs::create_dir_all(&artifacts).expect("create temp artifacts dir");
            fs::write(artifacts.join("index.json"), index_json).expect("write temp index.json");
            Self { root }
        }

        fn root(&self) -> &Path {
            &self.root
        }
    }

    impl Drop for TmpFixtures {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn real_fixtures_root() -> PathBuf {
        default_fixtures_root()
    }

    /// Absolute path to a real, dlopen-able artifact shipped in the repo.
    /// prepare-artifacts 按宿主平台命名产物（Linux `time.so` / macOS
    /// `time.dylib`），必须用 DLL_SUFFIX 拼接。
    fn time_dylib_path() -> PathBuf {
        real_fixtures_root().join(format!("artifacts/time{}", std::env::consts::DLL_SUFFIX))
    }

    /// Minimal PluginDocs JSON exposing a single node id.
    fn docs_json(plugin_path: &str, node_id: &str) -> String {
        format!(
            r#"{{
                "plugin_id": "{plugin_path}",
                "plugin_path": "{plugin_path}",
                "plugin_version": "0.1.0",
                "abi_version": 2,
                "nodes": [
                    {{
                        "id": "{node_id}",
                        "summary": "test node",
                        "input_schema": {{}},
                        "output_schema": {{}}
                    }}
                ]
            }}"#
        )
    }

    fn build_catalog(index_json: &str) -> (TmpFixtures, PluginCatalog) {
        let tmp = TmpFixtures::with_index(index_json);
        let catalog = PluginCatalog::load(tmp.root()).expect("load temp catalog");
        (tmp, catalog)
    }

    // ── Message handler registry ──────────────────────────────────────────
    // Round-trips a value through the CString<->malloc boundary so the
    // libc::free in `call_handler` frees allocator-matched memory.
    extern "C" fn upper_handler(input: *const c_char) -> *mut c_char {
        let s = unsafe { CStr::from_ptr(input) }.to_string_lossy();
        let tagged = format!(r#"{{"tag":"agent","content":"{}"}}"#, s.to_uppercase());
        let bytes = tagged.as_bytes();
        // Allocate via libc so `libc::free` (used by call_handler) matches.
        let buf = unsafe { libc::malloc(bytes.len() + 1) as *mut u8 };
        assert!(!buf.is_null(), "malloc failed");
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf, bytes.len());
            *buf.add(bytes.len()) = 0;
        }
        buf as *mut c_char
    }

    #[test]
    fn handler_registry_none_then_registered_roundtrip() {
        // HANDLER is a process-global OnceLock; this is the only test that
        // touches it, so the ordering (None before set, Some after) is
        // deterministic within the test.
        assert!(
            call_handler("before").is_none(),
            "no handler registered yet -> None"
        );
        _cordis_register_handler(upper_handler);
        let out = call_handler("hello").expect("handler now registered -> Some");
        assert_eq!(out, r#"{"tag":"agent","content":"HELLO"}"#);
        // set() is idempotent: a second registration is a no-op, handler stays.
        _cordis_register_handler(upper_handler);
        assert_eq!(
            call_handler("x").unwrap(),
            r#"{"tag":"agent","content":"X"}"#
        );
    }

    // ── Debug impl ────────────────────────────────────────────────────────
    #[test]
    fn catalog_plugin_debug_lists_key_fields() {
        let plugin = CatalogPlugin {
            plugin_path: "demo".to_string(),
            docs: serde_json::from_str(&docs_json("demo", "n")).unwrap(),
            artifact_path: PathBuf::from("/some/where.dylib"),
            execution: None,
            expected_abi_fingerprint: None,
            library: Arc::new(Mutex::new(None)),
        };
        let rendered = format!("{plugin:?}");
        assert!(
            rendered.contains("CatalogPlugin"),
            "struct name: {rendered}"
        );
        assert!(rendered.contains("demo"), "plugin_path shown: {rendered}");
        assert!(
            rendered.contains("where.dylib"),
            "artifact_path shown: {rendered}"
        );
        assert!(rendered.contains(".."), "finish_non_exhaustive marker");
    }

    // ── PluginCatalog::load — happy path over real fixtures ───────────────
    #[test]
    fn load_real_fixtures_and_accessors() {
        let catalog = PluginCatalog::load(real_fixtures_root()).expect("load real fixtures");
        assert!(catalog.fixtures_root().is_absolute());
        assert!(
            catalog.fixtures_root().ends_with("fixtures"),
            "root: {}",
            catalog.fixtures_root().display()
        );
        // Iterator and lookup agree.
        let via_iter = catalog.plugins().count();
        assert!(via_iter > 0, "fixtures ship at least one plugin");
        let time = catalog.plugin("time").expect("time plugin present");
        assert_eq!(time.plugin_path, "time");
        assert!(time.docs.nodes.iter().any(|n| n.id == "time_now"));
        assert!(catalog.plugin("does-not-exist").is_none());
    }

    // ── load error branches ───────────────────────────────────────────────
    #[test]
    fn load_missing_index_is_io_error() {
        let missing = std::env::temp_dir().join(format!(
            "cordis-host-missing-{}-{}",
            std::process::id(),
            "x"
        ));
        let err = PluginCatalog::load(&missing).unwrap_err();
        assert!(
            matches!(&err, PluginHostError::Io { path, .. } if path.ends_with("artifacts/index.json")),
            "expected Io, got {err:?}"
        );
    }

    #[test]
    fn load_bad_json_is_artifact_index_parse_error() {
        let tmp = TmpFixtures::with_index("{ this is not json ]");
        let err = PluginCatalog::load(tmp.root()).unwrap_err();
        assert!(matches!(err, PluginHostError::ArtifactIndexParse { .. }));
    }

    #[test]
    fn load_wrong_schema_version_is_rejected() {
        let index = r#"{ "schema_version": 3, "entries": [] }"#;
        let tmp = TmpFixtures::with_index(index);
        let err = PluginCatalog::load(tmp.root()).unwrap_err();
        assert!(
            matches!(&err, PluginHostError::ArtifactIndexParse { message, .. } if message.contains("unsupported schema_version")),
            "expected ArtifactIndexParse, got {err:?}"
        );
    }

    // ── invoke() lookup errors ────────────────────────────────────────────
    #[test]
    fn invoke_unknown_plugin_errors() {
        let index = format!(
            r#"{{ "schema_version": 2, "entries": [
                {{ "plugin_path": "p", "artifact_path": "/x.dylib", "docs": {}, "execution": null }}
            ] }}"#,
            docs_json("p", "n")
        );
        let (_tmp, catalog) = build_catalog(&index);
        let err = catalog.invoke("nope", "n", "{}".into()).unwrap_err();
        assert!(
            matches!(&err, PluginHostError::PluginNotFound { plugin_path } if plugin_path == "nope"),
            "expected PluginNotFound, got {err:?}"
        );
    }

    #[test]
    fn invoke_unknown_node_errors() {
        let index = format!(
            r#"{{ "schema_version": 2, "entries": [
                {{ "plugin_path": "p", "artifact_path": "/x.dylib", "docs": {}, "execution": null }}
            ] }}"#,
            docs_json("p", "known")
        );
        let (_tmp, catalog) = build_catalog(&index);
        let err = catalog.invoke("p", "missing", "{}".into()).unwrap_err();
        assert!(
            matches!(&err, PluginHostError::NodeNotFound { plugin_path, node_id } if plugin_path == "p" && node_id == "missing"),
            "expected NodeNotFound, got {err:?}"
        );
    }

    // ── real dylib invocation (dlopen + fingerprint verify + handle) ──────
    #[test]
    fn invoke_real_time_dylib_happy_path_and_cache_reuse() {
        // Uses the shipped index.json so `expected_abi_fingerprint` is Some
        // and matches the dylib's own abi_fingerprint() (verification passes).
        let catalog = PluginCatalog::load(real_fixtures_root()).expect("load real fixtures");
        let resp = catalog
            .invoke("time", "time_now", r#"{"node_id":"time_now"}"#.into())
            .expect("first invoke ok");
        let v: serde_json::Value = serde_json::from_str(&resp.payload).unwrap();
        assert_eq!(v["ok"], true, "payload: {}", resp.payload);
        assert!(v["timestamp"].is_number());
        // Second invoke reuses the cached LoadedDylib (guard already Some,
        // fingerprint_verified already true).
        let resp2 = catalog
            .invoke("time", "time_now", r#"{"node_id":"time_now"}"#.into())
            .expect("second invoke reuses handle");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&resp2.payload).unwrap()["ok"],
            true
        );
    }

    #[test]
    fn invoke_dylib_without_expected_fingerprint_skips_verification() {
        // abi_fingerprint omitted -> serde default None -> verification skipped.
        let index = format!(
            r#"{{ "schema_version": 2, "entries": [
                {{ "plugin_path": "time", "artifact_path": "{}", "docs": {}, "execution": null }}
            ] }}"#,
            time_dylib_path().display(),
            docs_json("time", "time_now")
        );
        let (_tmp, catalog) = build_catalog(&index);
        let resp = catalog
            .invoke("time", "time_now", r#"{"node_id":"time_now"}"#.into())
            .expect("invoke without fingerprint check");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&resp.payload).unwrap()["ok"],
            true
        );
    }

    #[test]
    fn invoke_dylib_fingerprint_mismatch_is_rejected() {
        let index = format!(
            r#"{{ "schema_version": 2, "entries": [
                {{
                    "plugin_path": "time",
                    "artifact_path": "{}",
                    "docs": {},
                    "execution": null,
                    "abi_fingerprint": {{
                        "rustc_version": "rustc 0.0.0",
                        "target_triple": "wrong-triple",
                        "crate_hash": "definitely_wrong",
                        "api_hash": "api_v2"
                    }}
                }}
            ] }}"#,
            time_dylib_path().display(),
            docs_json("time", "time_now")
        );
        let (_tmp, catalog) = build_catalog(&index);
        let err = catalog
            .invoke("time", "time_now", r#"{"node_id":"time_now"}"#.into())
            .unwrap_err();
        assert!(
            matches!(&err, PluginHostError::AbiFingerprintMismatch { plugin_path, expected, actual } if plugin_path == "time" && expected.crate_hash == "definitely_wrong" && actual.crate_hash == "crate_time_v1"),
            "expected AbiFingerprintMismatch, got {err:?}"
        );
    }

    #[test]
    fn invoke_dylib_load_failure_is_io_error() {
        // Points at a .dylib path that does not exist -> Library::new fails.
        let bogus = std::env::temp_dir().join("cordis-no-such-plugin.dylib");
        let index = format!(
            r#"{{ "schema_version": 2, "entries": [
                {{ "plugin_path": "p", "artifact_path": "{}", "docs": {}, "execution": null }}
            ] }}"#,
            bogus.display(),
            docs_json("p", "n")
        );
        let (_tmp, catalog) = build_catalog(&index);
        let err = catalog.invoke("p", "n", "{}".into()).unwrap_err();
        assert!(
            matches!(&err, PluginHostError::Io { message, .. } if message.contains("load dylib failed")),
            "expected Io load failure, got {err:?}"
        );
    }

    #[test]
    fn invoke_dylib_missing_entry_symbol_is_io_error() {
        // A valid dylib that lacks the cordis entry symbol -> symbol lookup
        // fails. Copy a real system dylib into the temp tree with a .dylib
        // name. `libgmalloc.dylib` is a genuine on-disk file (unlike e.g.
        // `libz.1.2.12.dylib`, a symlink whose target lives only in the dyld
        // shared cache), so `Path::exists()` is true and `Library::new`
        // succeeds — actually exercising the symbol-lookup-failure branch
        // rather than skipping. If it is ever absent, fall back to a skip.
        let sys = Path::new("/usr/lib/libgmalloc.dylib");
        if !sys.exists() {
            eprintln!("skipping: {} not present", sys.display());
            return;
        }
        let tmp = TmpFixtures::with_index("{}"); // placeholder, overwrite below
        let plugin_dylib = tmp.root().join("artifacts/nosym.dylib");
        fs::copy(sys, &plugin_dylib).expect("copy system dylib");
        let index = format!(
            r#"{{ "schema_version": 2, "entries": [
                {{ "plugin_path": "p", "artifact_path": "nosym.dylib", "docs": {}, "execution": null }}
            ] }}"#,
            docs_json("p", "n")
        );
        fs::write(tmp.root().join("artifacts/index.json"), index).unwrap();
        let catalog = PluginCatalog::load(tmp.root()).expect("load");
        let err = catalog.invoke("p", "n", "{}".into()).unwrap_err();
        assert!(
            matches!(&err, PluginHostError::Io { message, .. } if message.contains("symbol lookup failed")),
            "expected Io symbol failure, got {err:?}"
        );
    }

    // ── invoke_dylib fingerprint-parse + handle-panic branches ────────────
    //
    // These branches are unreachable through a *real* plugin dylib in a unit
    // test: a panic raised inside a separately-compiled dylib links that
    // dylib's own panic runtime, so it cannot unwind back across the C-ABI
    // boundary — modern rustc aborts the process ("Rust cannot catch foreign
    // exceptions") before `catch_unwind` on the host side sees anything.
    //
    // We therefore drive the private `invoke_dylib` directly: pre-populate a
    // `CatalogPlugin.library` with a `LoadedDylib` whose `api_ptr` points at
    // an in-test `static RustPluginApiV2`. The function pointers then live in
    // *this* binary, so a panic is caught by the host `catch_unwind` exactly
    // as it would be for a same-toolchain dylib. A real, on-disk
    // `libgmalloc.dylib` is kept mapped in the `library` field to satisfy the
    // "keep the module alive" invariant the struct documents.

    fn fp_current_build() -> PluginResponse {
        // Serializes to a value that DOES parse as AbiFingerprint.
        PluginResponse {
            payload: serde_json::to_string(&AbiFingerprint::current_build("crate_x", "api_v2"))
                .unwrap(),
        }
    }
    fn fp_not_json() -> PluginResponse {
        PluginResponse {
            payload: "<<not valid json>>".to_string(),
        }
    }
    fn dummy_docs() -> PluginResponse {
        PluginResponse {
            payload: "{}".to_string(),
        }
    }
    fn handle_ok(req: PluginRequest) -> PluginResponse {
        PluginResponse {
            payload: format!("echo:{}", req.payload),
        }
    }
    fn handle_panic_str(_req: PluginRequest) -> PluginResponse {
        panic!("boom static str");
    }
    fn handle_panic_string(_req: PluginRequest) -> PluginResponse {
        std::panic::panic_any(String::from("boom owned string"));
    }
    fn handle_panic_other(_req: PluginRequest) -> PluginResponse {
        std::panic::panic_any(42_u64);
    }

    static API_OK: RustPluginApiV2 = RustPluginApiV2 {
        abi_kind: cordis_plugin_sdk::DylibAbiKind::Rust,
        abi_fingerprint: fp_current_build,
        docs: dummy_docs,
        handle: handle_ok,
    };
    static API_FP_NOT_JSON: RustPluginApiV2 = RustPluginApiV2 {
        abi_kind: cordis_plugin_sdk::DylibAbiKind::Rust,
        abi_fingerprint: fp_not_json,
        docs: dummy_docs,
        handle: handle_ok,
    };
    static API_PANIC_STR: RustPluginApiV2 = RustPluginApiV2 {
        abi_kind: cordis_plugin_sdk::DylibAbiKind::Rust,
        abi_fingerprint: fp_current_build,
        docs: dummy_docs,
        handle: handle_panic_str,
    };
    static API_PANIC_STRING: RustPluginApiV2 = RustPluginApiV2 {
        abi_kind: cordis_plugin_sdk::DylibAbiKind::Rust,
        abi_fingerprint: fp_current_build,
        docs: dummy_docs,
        handle: handle_panic_string,
    };
    static API_PANIC_OTHER: RustPluginApiV2 = RustPluginApiV2 {
        abi_kind: cordis_plugin_sdk::DylibAbiKind::Rust,
        abi_fingerprint: fp_current_build,
        docs: dummy_docs,
        handle: handle_panic_other,
    };

    /// A real on-disk dylib to keep mapped inside the synthetic `LoadedDylib`.
    /// `libgmalloc.dylib` is a genuine file (see the missing-symbol test).
    fn live_gmalloc_or_skip() -> Option<Library> {
        let sys = Path::new("/usr/lib/libgmalloc.dylib");
        if !sys.exists() {
            eprintln!("skipping: {} not present", sys.display());
            return None;
        }
        Some(unsafe { Library::new(sys) }.expect("load libgmalloc"))
    }

    /// Build a `CatalogPlugin` whose `library` slot is pre-loaded with a
    /// `LoadedDylib` pointing at `api` (an in-test static), so `invoke_dylib`
    /// skips `Library::new`/symbol-lookup and runs the fingerprint + handle
    /// tail against our synthetic function table.
    fn plugin_with_static_api(
        api: &'static RustPluginApiV2,
        expected_abi_fingerprint: Option<AbiFingerprint>,
        library: Library,
    ) -> CatalogPlugin {
        CatalogPlugin {
            plugin_path: "synthetic".to_string(),
            docs: serde_json::from_str(&docs_json("synthetic", "n")).unwrap(),
            // A .dylib extension so `invoke_artifact` routes to `invoke_dylib`.
            artifact_path: PathBuf::from("/synthetic/plugin.dylib"),
            execution: None,
            expected_abi_fingerprint,
            library: Arc::new(Mutex::new(Some(LoadedDylib {
                library,
                api_ptr: api as *const RustPluginApiV2,
                fingerprint_verified: false,
            }))),
        }
    }

    #[test]
    fn invoke_dylib_fingerprint_not_parseable_is_invocation_failed() {
        let Some(lib) = live_gmalloc_or_skip() else {
            return;
        };
        // expected fingerprint present -> verification runs -> abi_fingerprint
        // returns non-JSON -> serde parse fails -> PluginInvocationFailed.
        let expected = AbiFingerprint::current_build("crate_x", "api_v2");
        let plugin = plugin_with_static_api(&API_FP_NOT_JSON, Some(expected), lib);
        let err = invoke_dylib(&plugin, "{}".to_string()).unwrap_err();
        assert!(
            matches!(&err, PluginHostError::PluginInvocationFailed { plugin_path, message } if plugin_path == "synthetic" && message.contains("abi_fingerprint response was not parseable")),
            "expected PluginInvocationFailed, got {err:?}"
        );
    }

    #[test]
    fn invoke_dylib_handle_panic_static_str_is_isolated() {
        let Some(lib) = live_gmalloc_or_skip() else {
            return;
        };
        // No expected fingerprint -> verification skipped -> straight to
        // handle, which panics with a &'static str payload.
        let plugin = plugin_with_static_api(&API_PANIC_STR, None, lib);
        let err = invoke_dylib(&plugin, "{}".to_string()).unwrap_err();
        assert!(
            matches!(&err, PluginHostError::PluginInvocationFailed { message, .. } if message.contains("plugin handle panicked") && message.contains("boom static str")),
            "expected PluginInvocationFailed, got {err:?}"
        );
    }

    #[test]
    fn invoke_dylib_handle_panic_owned_string_is_isolated() {
        let Some(lib) = live_gmalloc_or_skip() else {
            return;
        };
        let plugin = plugin_with_static_api(&API_PANIC_STRING, None, lib);
        let err = invoke_dylib(&plugin, "{}".to_string()).unwrap_err();
        assert!(
            matches!(&err, PluginHostError::PluginInvocationFailed { message, .. } if message.contains("boom owned string")),
            "expected PluginInvocationFailed, got {err:?}"
        );
    }

    #[test]
    fn invoke_dylib_handle_panic_non_string_payload_is_isolated() {
        let Some(lib) = live_gmalloc_or_skip() else {
            return;
        };
        // u64 payload -> neither &str nor String downcast matches -> the
        // "<non-string panic payload>" fallback arm.
        let plugin = plugin_with_static_api(&API_PANIC_OTHER, None, lib);
        let err = invoke_dylib(&plugin, "{}".to_string()).unwrap_err();
        assert!(
            matches!(&err, PluginHostError::PluginInvocationFailed { message, .. } if message.contains("<non-string panic payload>")),
            "expected PluginInvocationFailed, got {err:?}"
        );
    }

    #[test]
    fn invoke_dylib_static_api_happy_path_marks_verified() {
        let Some(lib) = live_gmalloc_or_skip() else {
            return;
        };
        // Matching fingerprint -> verification passes and sets
        // fingerprint_verified; handle returns normally.
        let expected = AbiFingerprint::current_build("crate_x", "api_v2");
        let plugin = plugin_with_static_api(&API_OK, Some(expected), lib);
        let resp = invoke_dylib(&plugin, "ping".to_string()).expect("first invoke ok");
        assert_eq!(resp.payload, "echo:ping");
        // Second invoke reuses the already-verified handle (guard is Some,
        // fingerprint_verified now true).
        let resp2 = invoke_dylib(&plugin, "pong".to_string()).expect("second invoke ok");
        assert_eq!(resp2.payload, "echo:pong");
    }

    // ── process (JSON artifact) execution ─────────────────────────────────
    #[test]
    fn invoke_process_echoes_stdin_via_cat() {
        // artifact_path is non-dylib -> invoke_json_artifact; /bin/cat copies
        // stdin to stdout, trimmed.
        let index = format!(
            r#"{{ "schema_version": 2, "entries": [
                {{
                    "plugin_path": "p",
                    "artifact_path": "proc.json",
                    "docs": {},
                    "execution": {{ "kind": "process", "command": "/bin/cat" }}
                }}
            ] }}"#,
            docs_json("p", "run")
        );
        let (_tmp, catalog) = build_catalog(&index);
        let resp = catalog
            .invoke("p", "run", "hello world".into())
            .expect("process invoke");
        assert_eq!(resp.payload, "hello world");
    }

    #[test]
    fn invoke_process_nonzero_status_empty_stderr() {
        let index = format!(
            r#"{{ "schema_version": 2, "entries": [
                {{
                    "plugin_path": "p",
                    "artifact_path": "proc.json",
                    "docs": {},
                    "execution": {{
                        "kind": "process",
                        "command": "/bin/sh",
                        "args": ["-c", "cat > /dev/null; exit 1"]
                    }}
                }}
            ] }}"#,
            docs_json("p", "run")
        );
        let (_tmp, catalog) = build_catalog(&index);
        let err = catalog.invoke("p", "run", "x".into()).unwrap_err();
        assert!(
            matches!(&err, PluginHostError::PluginInvocationFailed { message, .. } if message.contains("process exited with status")),
            "expected PluginInvocationFailed, got {err:?}"
        );
    }

    #[test]
    fn invoke_process_nonzero_status_reports_stderr() {
        let index = format!(
            r#"{{ "schema_version": 2, "entries": [
                {{
                    "plugin_path": "p",
                    "artifact_path": "proc.json",
                    "docs": {},
                    "execution": {{
                        "kind": "process",
                        "command": "/bin/sh",
                        "args": ["-c", "cat > /dev/null; echo boom-on-stderr >&2; exit 3"]
                    }}
                }}
            ] }}"#,
            docs_json("p", "run")
        );
        let (_tmp, catalog) = build_catalog(&index);
        let err = catalog.invoke("p", "run", "x".into()).unwrap_err();
        assert!(
            matches!(&err, PluginHostError::PluginInvocationFailed { message, .. } if message == "boom-on-stderr"),
            "expected PluginInvocationFailed, got {err:?}"
        );
    }

    #[test]
    fn invoke_process_spawn_failure() {
        let index = format!(
            r#"{{ "schema_version": 2, "entries": [
                {{
                    "plugin_path": "p",
                    "artifact_path": "proc.json",
                    "docs": {},
                    "execution": {{ "kind": "process", "command": "/no/such/binary-xyz" }}
                }}
            ] }}"#,
            docs_json("p", "run")
        );
        let (_tmp, catalog) = build_catalog(&index);
        let err = catalog.invoke("p", "run", "x".into()).unwrap_err();
        assert!(
            matches!(&err, PluginHostError::PluginInvocationFailed { message, .. } if message.contains("spawn")),
            "expected PluginInvocationFailed, got {err:?}"
        );
    }

    #[test]
    fn invoke_process_non_utf8_stdout() {
        let index = format!(
            r#"{{ "schema_version": 2, "entries": [
                {{
                    "plugin_path": "p",
                    "artifact_path": "proc.json",
                    "docs": {},
                    "execution": {{
                        "kind": "process",
                        "command": "/bin/sh",
                        "args": ["-c", "cat > /dev/null; printf '\\377'"]
                    }}
                }}
            ] }}"#,
            docs_json("p", "run")
        );
        let (_tmp, catalog) = build_catalog(&index);
        let err = catalog.invoke("p", "run", "x".into()).unwrap_err();
        assert!(
            matches!(&err, PluginHostError::PluginInvocationFailed { message, .. } if message.contains("stdout was not utf-8")),
            "expected PluginInvocationFailed, got {err:?}"
        );
    }

    #[test]
    fn invoke_process_write_stdin_broken_pipe_is_invocation_failed() {
        // `/usr/bin/true` exits immediately and never reads stdin, closing the
        // read end of the pipe. Writing a large payload then fails with
        // BrokenPipe inside `stdin.write_all`, exercising the write-stdin
        // error branch of `invoke_json_artifact`.
        let index = format!(
            r#"{{ "schema_version": 2, "entries": [
                {{
                    "plugin_path": "p",
                    "artifact_path": "proc.json",
                    "docs": {},
                    "execution": {{ "kind": "process", "command": "/usr/bin/true" }}
                }}
            ] }}"#,
            docs_json("p", "run")
        );
        let (_tmp, catalog) = build_catalog(&index);
        // A payload far larger than the OS pipe buffer so the write cannot be
        // fully buffered before the reader exits.
        let big = "x".repeat(4_000_000);
        let err = catalog.invoke("p", "run", big).unwrap_err();
        assert!(
            matches!(&err, PluginHostError::PluginInvocationFailed { message, .. } if message.contains("write stdin failed")),
            "expected PluginInvocationFailed, got {err:?}"
        );
    }

    #[test]
    fn invoke_non_dylib_without_execution_is_unsupported() {
        let index = format!(
            r#"{{ "schema_version": 2, "entries": [
                {{ "plugin_path": "p", "artifact_path": "artifact.json", "docs": {}, "execution": null }}
            ] }}"#,
            docs_json("p", "run")
        );
        let (_tmp, catalog) = build_catalog(&index);
        let err = catalog.invoke("p", "run", "x".into()).unwrap_err();
        assert!(matches!(
            err,
            PluginHostError::PluginExecutionUnsupported { .. }
        ));
    }

    // ── pure path helpers ─────────────────────────────────────────────────
    #[test]
    fn resolve_artifact_path_absolute_and_relative() {
        let index = Path::new("/root/artifacts/index.json");
        assert_eq!(
            resolve_artifact_path(index, "/abs/x.dylib"),
            PathBuf::from("/abs/x.dylib")
        );
        assert_eq!(
            resolve_artifact_path(index, "rel.dylib"),
            PathBuf::from("/root/artifacts/rel.dylib")
        );
    }

    #[test]
    fn resolve_exec_path_absolute_and_relative() {
        let artifact = Path::new("/root/artifacts/proc.json");
        assert_eq!(
            resolve_exec_path(artifact, "/usr/bin/cat"),
            PathBuf::from("/usr/bin/cat")
        );
        assert_eq!(
            resolve_exec_path(artifact, "runner"),
            PathBuf::from("/root/artifacts/runner")
        );
    }

    #[test]
    fn is_dylib_path_matches_known_extensions() {
        assert!(is_dylib_path(Path::new("a.so")));
        assert!(is_dylib_path(Path::new("a.dylib")));
        assert!(is_dylib_path(Path::new("a.dll")));
        assert!(!is_dylib_path(Path::new("a.json")));
        assert!(!is_dylib_path(Path::new("noext")));
    }

    #[test]
    fn absolute_path_passthrough_and_join() {
        let abs = Path::new("/already/absolute");
        assert_eq!(absolute_path(abs).unwrap(), abs.to_path_buf());
        let rel = absolute_path(Path::new("rel/dir")).unwrap();
        assert!(rel.is_absolute(), "relative gets joined onto cwd: {rel:?}");
        assert!(rel.ends_with("rel/dir"));
    }

    #[test]
    fn default_fixtures_root_points_at_repo_fixtures() {
        let root = default_fixtures_root();
        assert!(root.ends_with("fixtures"), "root: {}", root.display());
    }

    // ── error mappers (extracted seams) ───────────────────────────────────
    // `wait_failed_error` / `current_dir_error` wrap io-error branches that a
    // unit test cannot force through the real `child.wait_with_output()` /
    // `env::current_dir()` calls, so the mapping is exercised directly.
    #[test]
    fn wait_failed_error_maps_to_invocation_failed() {
        let io = std::io::Error::other("boom-wait");
        let err = wait_failed_error("plug", &io);
        assert!(
            matches!(&err, PluginHostError::PluginInvocationFailed { plugin_path, message } if plugin_path == "plug" && message == "wait failed: boom-wait"),
            "expected PluginInvocationFailed, got {err:?}"
        );
    }

    #[test]
    fn current_dir_error_maps_to_io() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "boom-cwd");
        let err = current_dir_error(Path::new("rel/path"), &io);
        assert!(
            matches!(&err, PluginHostError::Io { path, message } if path == &PathBuf::from("rel/path") && message == "boom-cwd"),
            "expected Io, got {err:?}"
        );
    }
}
