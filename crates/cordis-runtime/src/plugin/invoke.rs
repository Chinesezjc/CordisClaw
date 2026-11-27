use crate::core::error::RuntimeError;
use crate::core::models::{
    ArtifactKind, DylibAbiKind, PluginExecution, PluginLoadResult, PluginUnavailableReason,
};
use crate::plugin::abi::{PluginRequest, PluginResponse};
use crate::plugin::dynamic::{is_dylib_path, LoadedDylibApi, UnloadPolicy};
use crate::plugin::loader::{default_loader_config, Loader};
use crate::plugin::registry::PluginRegistry;
use cordis_plugin_sdk::NodeType;
use libloading;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Plugin dylib handles that must stay mapped after `invoke` returns.
///
/// A handle lands here when the invocation left background work behind: either
/// a `NodeType::Task` service, or a plugin that reports live service threads
/// through its SDK-exported thread contract (`_cordis_live_service_threads`).
///
/// Keyed by plugin path so re-entrant invokes replace instead of accumulate.
/// A replacement handle that maps the *same* image releases the old handle
/// (`dlclose` decrements the refcount; the new handle keeps the mapping), so
/// repeated invokes of a running service do not leak. A replacement that maps
/// a *different* image (the artifact was rebuilt) owns a different thread set,
/// so the old handle is joined and only then unloaded.
static HELD_PLUGIN_LIBRARIES: OnceLock<Mutex<HashMap<String, LoadedDylibApi>>> = OnceLock::new();

/// How long the reload/unload path waits for a plugin's threads to exit.
///
/// A `stop` handler that does not join its threads now has a bounded window
/// before the runtime gives up, keeps the mapping, and says so.
pub const THREAD_JOIN_TIMEOUT_MS: u64 = 5_000;

/// Access the held-library map with a tolerant lock policy: a poisoned lock
/// is recovered (the underlying data is invariant-free — it's a keep-alive
/// registry). Previously we called `.lock().unwrap()`, which turned any
/// poison in the invoke hot path into a hard runtime crash.
fn held_libraries_lock() -> std::sync::MutexGuard<'static, HashMap<String, LoadedDylibApi>> {
    HELD_PLUGIN_LIBRARIES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
}

/// Whether `plugin_path` currently has a held dylib handle. Per-path so tests
/// are not perturbed by other plugins being held concurrently.
pub fn is_plugin_library_held(plugin_path: &str) -> bool {
    held_libraries_lock().contains_key(plugin_path)
}

/// Keep `dylib` mapped under `plugin_path`, replacing a previous handle.
///
/// Same-image replacements release the old `dlopen` reference: the new handle
/// already keeps that exact mapping alive, so `dlclose` only drops a refcount.
/// Different-image replacements (a rebuilt artifact) may still have threads
/// running the old code; that handle goes through the join-before-unload path.
fn hold_plugin_library(plugin_path: &str, dylib: LoadedDylibApi) {
    let identity = dylib.identity();
    let previous = held_libraries_lock().insert(plugin_path.to_string(), dylib);
    previous.into_iter().for_each(|mut old| {
        if identity.is_some() && old.identity() == identity {
            // Same mapping: the new handle keeps it alive, so this only drops
            // a duplicate `dlopen` reference.
            old.release_on_drop();
            drop(old);
        } else {
            // A different image (the artifact was rebuilt): its thread set is
            // not the one the new handle owns, so join it before dropping.
            unload_joined(old);
        }
    });
}

/// Join a plugin's threads, then unload it only when every thread exited.
///
/// This is the ordered teardown the fix is about: `stop` (by the caller) →
/// `join` → reference check → `drop`. A legacy dylib without the thread
/// contract, or one whose threads refuse to exit inside
/// [`THREAD_JOIN_TIMEOUT_MS`], is not unloaded: it is retained with the
/// diagnostic `plugin {path} unloaded without joined threads; retained`.
fn unload_joined_with_timeout(mut dylib: LoadedDylibApi, timeout_ms: u64) {
    // Explicit permission first. A plugin that never opted in has no way to
    // report threads it spawned with a raw `std::thread::spawn`, so a zero
    // count proves nothing — such an image is never unloaded.
    let permitted = dylib.allows_unload();
    let unjoined = permitted.then(|| {
        dylib.thread_contract().map(|contract| {
            contract.join(timeout_ms);
            contract.live()
        })
    });
    match unjoined {
        Some(Some(0)) => dylib.release_on_drop(),
        // Missing contract, missing opt-in, or threads that refused to exit:
        // keep the mapping and report it.
        _ => dylib.retain_with_diagnostic(),
    }
    drop(dylib);
}

fn unload_joined(dylib: LoadedDylibApi) {
    unload_joined_with_timeout(dylib, THREAD_JOIN_TIMEOUT_MS);
}

/// Release the held handle for `plugin_path` after joining its threads.
///
/// Called by the reload path before the old snapshot's artifact goes away.
/// Dropping the handle is safe only once `unload_joined` has seen zero live
/// threads; otherwise the mapping is retained (and a diagnostic printed).
pub fn unload_plugin_library(plugin_path: &str) {
    unload_plugin_library_with_timeout(plugin_path, THREAD_JOIN_TIMEOUT_MS);
}

/// [`unload_plugin_library`] with an explicit join timeout (tests use a short
/// one to exercise the "threads refused to exit" fallback without waiting the
/// production five seconds).
pub fn unload_plugin_library_with_timeout(plugin_path: &str, timeout_ms: u64) {
    let held = held_libraries_lock().remove(plugin_path);
    held.into_iter()
        .for_each(|dylib| unload_joined_with_timeout(dylib, timeout_ms));
}
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
    invoke_registered_plugin_with_settings(plugin_registry, plugin_path, node_id, payload, None)
}

/// [`invoke_registered_plugin`] plus the plugin's `settings` block from
/// `config/plugins/<name>.yaml`.
///
/// The settings are merged into the payload for dylib plugins, so a plugin
/// reads its configuration from the config layer instead of hard-coding paths
/// or limits in source. Keys the caller already supplied win: the invocation
/// payload is what the agent/tool explicitly asked for, and the config layer
/// only fills fields the caller left absent.
///
/// JSON/process artifacts are deliberately excluded: they receive the caller's
/// stdin bytes verbatim, so merging settings would mean parsing (and possibly
/// rejecting) input those plugins treat as opaque.
pub fn invoke_registered_plugin_with_settings(
    plugin_registry: &PluginRegistry,
    plugin_path: &str,
    node_id: &str,
    payload: String,
    settings: Option<&serde_json::Value>,
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

    let mut dylib = match LoadedDylibApi::open(artifact_path) {
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
    // `DylibAbiKind` currently has exactly one variant (`Rust`), and the SDK's
    // `export_plugin_api!` macro always stamps `abi_kind: DylibAbiKind::Rust`
    // into the static vtable. Reading any other discriminant out of the
    // `#[repr(C)]` field would already be UB, so a runtime `!= Rust` branch is
    // dead code until the enum grows a second variant. Keep the invariant as a
    // debug assertion (a marker for whoever adds that variant to restore the
    // real `mark_runtime_unavailable` + `PluginUnavailable` handling here)
    // rather than an always-false runtime check + unreachable error path.
    debug_assert_eq!(
        api.abi_kind,
        DylibAbiKind::Rust,
        "dylib exported a non-Rust abi_kind; add real AbiMismatch handling when \
         DylibAbiKind gains variants"
    );

    let expected_fingerprint =
        plugin
            .abi_fingerprint
            .clone()
            .ok_or_else(|| RuntimeError::Invariant {
                message: format!("loaded plugin missing abi_fingerprint: {plugin_path}"),
            })?;
    let runtime_fingerprint = serde_json::from_str(&(api.abi_fingerprint)().payload)
        .map_err(|err| dylib_payload_io(artifact_path, "runtime fingerprint parse failed", err))?;
    if runtime_fingerprint != expected_fingerprint {
        let diff = expected_fingerprint.diff(&runtime_fingerprint);
        plugin_registry.mark_runtime_unavailable(
            plugin_path,
            PluginUnavailableReason::AbiMismatch,
            diff.clone(),
        );
        return Err(RuntimeError::AbiMismatch {
            plugin_path: plugin_path.to_string(),
            expected: Box::new(expected_fingerprint),
            actual: Box::new(runtime_fingerprint),
            fingerprint_diff: diff,
        });
    }

    let runtime_docs = serde_json::from_str(&(api.docs)().payload)
        .map_err(|err| dylib_payload_io(artifact_path, "runtime docs parse failed", err))?;
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

    // Merge configured `settings` and the node id into the payload so plugins
    // don't have to duplicate them or hard-code machine-specific values.
    // P2-21: reject malformed payload instead of silently converting to
    // `null` — a downstream settings/node_id injection would otherwise vanish
    // and the plugin would see just `null` with no clue why.
    //
    // This is the dylib path on purpose: a JSON/process artifact receives the
    // caller's stdin bytes verbatim (see `invoke_json_artifact`), and rewriting
    // that into a parsed object would break process plugins that take
    // non-JSON stdin.
    let mut payload_value: serde_json::Value =
        serde_json::from_str(&payload).map_err(|err| RuntimeError::InvalidArgument {
            message: format!("plugin invoke payload was not valid JSON: {err}"),
        })?;
    inject_plugin_settings(&mut payload_value, settings);
    if let Some(obj) = payload_value.as_object_mut() {
        obj.entry("node_id")
            .or_insert_with(|| serde_json::json!(node_id));
    }
    let payload = serde_json::to_string(&payload_value).map_err(payload_reserialize_invariant)?;

    let response = call_handle_catch_unwind(plugin_path, api.handle, PluginRequest { payload })?;

    // For Task nodes: look up the Service VTable and start it. Task services
    // run until the reload path stops them, so their dylib stays mapped.
    let is_task = docs
        .nodes
        .iter()
        .any(|n| n.id == node_id && n.node_type == NodeType::Task);
    if is_task {
        // P2-20: was `CString::new(node_id).unwrap_or_default()` — a
        // node_id containing an interior NUL byte silently became empty
        // and the plugin returned the wrong service vtable (or null).
        // Fail closed with a clear error.
        let c_node = std::ffi::CString::new(node_id).map_err(nul_node_id_error)?;
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
                // NOTE: `vtable.start` is `extern "C" fn` — modern rustc
                // *auto-aborts* if a Rust panic tries to unwind through
                // a C ABI boundary (see `-C panic=abort` for extern "C"
                // in rustc reference). A `catch_unwind` on this side of
                // the boundary is therefore FUTILE: the panic hits the
                // extern-fn frame, LLVM inserts `abort()`, we never
                // return. The isolation for services must live inside
                // the plugin's own SDK-provided wrapper (defense in
                // depth is deferred to a future SDK change).
                eprintln!(
                    "service: {plugin_path}::{node_id} registered (start={})",
                    (vtable.start)(vtable.data) == 0
                );
            }
        }
    }

    // ── Thread-aware unload ─────────────────────────────────────────────
    // A plugin's `handle` may spawn threads that outlive this call (HTTP
    // servers, message pollers, streaming readers). The SDK exports a joinable
    // thread contract; the runtime may only `dlclose` once every thread has
    // exited, otherwise the thread jumps into unmapped code (the 2026-11-26
    // `qq::qq_serve` SIGSEGV).
    // Explicit opt-in: only a plugin that declares "every background thread I
    // spawn is registered with ServiceThreads" may be unloaded. A raw
    // `std::thread::spawn` is invisible to the thread contract, so a zero
    // count alone must never be taken as proof.
    let may_unload = dylib.allows_unload();
    let live_threads = dylib.thread_contract().map(|contract| contract.live());
    if !is_task && may_unload && live_threads == Some(0) {
        // Opted in and no thread is running: `dlclose` is safe and the
        // mapping does not leak.
        dylib.release_on_drop();
        drop(dylib);
    } else {
        // Task services, live threads, and plugins that did not opt in keep
        // one handle per plugin path; the reload path joins and unloads it.
        dylib.set_unload_policy(UnloadPolicy::Retain);
        hold_plugin_library(plugin_path, dylib);
    }

    Ok(response)
}

/// Map a JSON-payload parse/serialize failure on a dylib's exported wire
/// buffer (`abi_fingerprint()` / `docs()`) to a path-tagged `Io` error.
///
/// These payloads are produced by the plugin's own SDK serialisers, so a
/// parse failure here is a genuine artifact fault (corrupt/incompatible
/// dylib), not caller input — hence `Io` tagged with the artifact path
/// rather than `InvalidArgument`.
fn dylib_payload_io(artifact_path: &Path, context: &str, err: serde_json::Error) -> RuntimeError {
    RuntimeError::Io {
        path: artifact_path.to_path_buf(),
        message: format!("{context}: {err}"),
    }
}

/// Build a `PluginInvocationFailed` error for the JSON-artifact subprocess
/// path (spawn/stdin/wait/exit-status/stdout-decoding failures).
fn plugin_invocation_failed(plugin_path: &str, message: String) -> RuntimeError {
    RuntimeError::PluginInvocationFailed {
        plugin_path: plugin_path.to_string(),
        message,
    }
}

/// Map a failure to re-serialise the (already-parsed) invoke payload back to a
/// JSON string to an `Invariant` error. The value was just deserialised from a
/// valid JSON string and only had a string `node_id` key inserted, so a
/// serialisation failure indicates a broken invariant, not caller input.
fn payload_reserialize_invariant(err: serde_json::Error) -> RuntimeError {
    RuntimeError::Invariant {
        message: format!("plugin invoke payload re-serialize failed: {err}"),
    }
}

/// Merge the plugin's configured `settings` into the invocation payload.
///
/// `map`/`for_each` rather than `if let`, so a non-object payload (a bare
/// string or array) simply keeps its shape instead of leaving an
/// uncovered not-taken arm on the closing brace. Caller-supplied keys win:
/// the tool/agent payload is the explicit request, and config only fills
/// what the caller left absent.
///
/// Empty settings (`settings: {}` is `PluginConfigFile`'s default) are not
/// injected at all: a plugin yaml that only toggles `enabled` must not alter
/// the payload every plugin receives.
fn inject_plugin_settings(payload: &mut serde_json::Value, settings: Option<&serde_json::Value>) {
    let Some(settings) = settings else {
        return;
    };
    let is_empty = match settings {
        serde_json::Value::Null => true,
        serde_json::Value::Object(map) => map.is_empty(),
        _ => false,
    };
    if is_empty {
        return;
    }
    payload.as_object_mut().into_iter().for_each(|obj| {
        obj.entry("settings").or_insert_with(|| settings.clone());
    });
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
                .map_err(|e| {
                    plugin_invocation_failed(
                        plugin_path,
                        format!("spawn {} failed: {e}", command_path.display()),
                    )
                })?;

            // `stdin` is always `Some` here (the spawn above pipes it), so this
            // is a plain write with the None case a no-op. Expressed as
            // `map`/`transpose` rather than `if let` so there is no not-taken
            // arm for a case a piped spawn never produces.
            child
                .stdin
                .as_mut()
                .map(|stdin| stdin.write_all(payload.as_bytes()))
                .transpose()
                .map_err(|e| {
                    plugin_invocation_failed(plugin_path, format!("write stdin failed: {e}"))
                })?;

            let output = child
                .wait_with_output()
                .map_err(|e| plugin_invocation_failed(plugin_path, format!("wait failed: {e}")))?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
                let message = if stderr.is_empty() {
                    format!("process exited with status {}", output.status)
                } else {
                    stderr
                };
                return Err(plugin_invocation_failed(plugin_path, message));
            }

            let stdout = String::from_utf8(output.stdout).map_err(|e| {
                plugin_invocation_failed(plugin_path, format!("stdout was not utf-8: {e}"))
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

/// Guard a plugin `handle` call with `catch_unwind` so a panic inside a
/// plugin's dylib can't unwind across into the runtime call stack and
/// tear down the host process. The panic is converted into a
/// `RuntimeError` tagged with the plugin path.
///
/// This only works because `handle` is `fn(...)` (not `extern "C" fn`).
/// For `extern "C"` service entrypoints (start/stop), modern rustc
/// auto-inserts `abort()` on any panic that would unwind through the
/// C ABI, so catch_unwind on the runtime side is defeated. Panic
/// isolation for services must be added inside the plugin SDK's own
/// service wrapper — deferred.
fn call_handle_catch_unwind(
    plugin_path: &str,
    handle: fn(PluginRequest) -> PluginResponse,
    request: PluginRequest,
) -> Result<PluginResponse, RuntimeError> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle(request))) {
        Ok(resp) => Ok(resp),
        Err(payload) => {
            let msg = panic_message(&payload);
            eprintln!("plugin {plugin_path} panicked during handle: {msg}");
            Err(RuntimeError::InvalidArgument {
                message: format!("plugin {plugin_path} panicked in handle: {msg}"),
            })
        }
    }
}

/// `InvalidArgument` for a node id containing an interior NUL byte — kept as
/// a named fn (directly unit-tested) because no declared fixture node can
/// produce one through the public invoke path.
fn nul_node_id_error(err: std::ffi::NulError) -> RuntimeError {
    RuntimeError::InvalidArgument {
        message: format!("plugin invoke node_id contains NUL byte: {err}"),
    }
}

fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

// The invoke path carries three fail-closed `Invariant` guards for a `Loaded`
// registry entry that is missing an artifact-derived field (docs /
// artifact_kind / abi_fingerprint). The public `PluginRegistry` constructors
// co-populate those fields, so these states are unconstructible through the
// normal API; `insert_raw` (a `#[cfg(test)]` door) builds them directly so the
// guards are covered without weakening the production constructors.
#[cfg(test)]
mod loaded_entry_invariant_tests {
    #[test]
    fn nul_node_id_error_names_the_nul() {
        let nul = std::ffi::CString::new("a\0b").unwrap_err();
        let err = super::nul_node_id_error(nul);
        assert!(
            matches!(&err, crate::core::error::RuntimeError::InvalidArgument { message }
            if message.starts_with("plugin invoke node_id contains NUL byte: "))
        );
    }

    use super::*;
    use crate::core::models::{AbiFingerprint, NodeDoc, PluginDocs, PluginLoadResult};
    use crate::plugin::registry::RegisteredPlugin;
    use std::collections::BTreeSet;

    fn docs_one_node(plugin_path: &str, node_id: &str) -> PluginDocs {
        PluginDocs {
            plugin_id: plugin_path.replace('/', "_"),
            plugin_path: plugin_path.to_string(),
            plugin_version: "0.1.0".to_string(),
            abi_version: 2,
            command_name: None,
            nodes: vec![NodeDoc {
                id: node_id.to_string(),
                summary: "n".to_string(),
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: serde_json::json!({"type": "object"}),
                side_effects: vec![],
                failure_modes: vec![],
                node_type: NodeType::Router,
                agent_accessible: true,
            }],
            system_hint: None,
        }
    }

    fn fingerprint() -> AbiFingerprint {
        AbiFingerprint {
            rustc_version: "rustc-test".to_string(),
            target_triple: "test-triple".to_string(),
            crate_hash: "crate_v1".to_string(),
            api_hash: "api_v2".to_string(),
        }
    }

    /// Base `Loaded` entry with every field populated; individual tests knock
    /// out exactly one field to reach its invariant arm.
    fn loaded_base(plugin_path: &str, node_id: &str) -> RegisteredPlugin {
        RegisteredPlugin {
            plugin_path: plugin_path.to_string(),
            parent: None,
            required: true,
            grants_from_parent: BTreeSet::new(),
            load_result: PluginLoadResult::Loaded,
            docs: Some(docs_one_node(plugin_path, node_id)),
            artifact_path: Some(PathBuf::from("/tmp/does-not-matter.dylib")),
            artifact_kind: Some(ArtifactKind::Dylib),
            abi_fingerprint: Some(fingerprint()),
            execution: None,
            fingerprint_diff: Vec::new(),
        }
    }

    // Loaded but docs == None → PluginDocsNotFound (invoke.rs docs guard).
    #[test]
    fn loaded_missing_docs_is_docs_not_found() {
        let registry = PluginRegistry::default();
        let mut entry = loaded_base("inv/nodocs", "n");
        entry.docs = None;
        registry.insert_raw(entry);
        let err = invoke_registered_plugin(&registry, "inv/nodocs", "n", "{}".to_string())
            .expect_err("missing docs must error");
        assert!(
            matches!(&err, RuntimeError::PluginDocsNotFound { plugin_path } if plugin_path == "inv/nodocs"),
            "wrong variant: {err:?}"
        );
    }

    // Loaded, docs present, node present, but artifact_kind == None →
    // "missing artifact kind" Invariant.
    #[test]
    fn loaded_missing_artifact_kind_is_invariant() {
        let registry = PluginRegistry::default();
        let mut entry = loaded_base("inv/nokind", "n");
        entry.artifact_kind = None;
        registry.insert_raw(entry);
        let err = invoke_registered_plugin(&registry, "inv/nokind", "n", "{}".to_string())
            .expect_err("missing artifact kind must error");
        assert!(
            matches!(&err, RuntimeError::Invariant { message } if message.contains("missing artifact kind") && message.contains("inv/nokind")),
            "wrong variant: {err:?}"
        );
    }

    // Loaded with a real dylib artifact whose fingerprint gate is reached, but
    // abi_fingerprint == None → "missing abi_fingerprint" Invariant. This arm
    // sits after the dylib is opened and its abi_kind checked, so it needs a
    // genuinely loadable fixture dylib; point at the host-native `time` fixture
    // and read its real docs so the docs/kind gates pass first.
    #[test]
    fn loaded_missing_abi_fingerprint_is_invariant() {
        let artifact = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/artifacts")
            .join(format!("time{}", std::env::consts::DLL_SUFFIX));
        // Gate on fixture loadability so a cross-arch host is a no-op rather
        // than a failure. `for`-over-`Result` (rather than `if let`) has no
        // not-taken arm, so the gate leaves no permanently-uncovered brace on
        // hosts where the fixture does load.
        for loaded in LoadedDylibApi::open(&artifact).into_iter() {
            let real_docs: PluginDocs =
                serde_json::from_str(&(loaded.api().docs)().payload).expect("docs json");
            let node_id = real_docs.nodes[0].id.clone();
            let registry = PluginRegistry::default();
            let entry = RegisteredPlugin {
                plugin_path: "inv/nofp".to_string(),
                parent: None,
                required: true,
                grants_from_parent: BTreeSet::new(),
                load_result: PluginLoadResult::Loaded,
                docs: Some(real_docs),
                artifact_path: Some(artifact.clone()),
                artifact_kind: Some(ArtifactKind::Dylib),
                abi_fingerprint: None,
                execution: None,
                fingerprint_diff: Vec::new(),
            };
            registry.insert_raw(entry);
            let err = invoke_registered_plugin(&registry, "inv/nofp", &node_id, "{}".to_string())
                .expect_err("missing abi_fingerprint must error");
            let is_expected = matches!(&err, RuntimeError::Invariant { message } if message.contains("missing abi_fingerprint") && message.contains("inv/nofp"));
            assert!(is_expected, "wrong variant: {err:?}");
        }
    }
}

#[cfg(test)]
mod panic_isolation_tests {
    use super::*;

    #[test]
    fn handle_panic_is_caught_and_reported() {
        fn boom(_: PluginRequest) -> PluginResponse {
            panic!("deliberate test panic")
        }
        let err = call_handle_catch_unwind(
            "test/panicker",
            boom,
            PluginRequest {
                payload: "{}".to_string(),
            },
        )
        .expect_err("panic must convert to Err");
        assert!(
            matches!(&err, RuntimeError::InvalidArgument { message } if message.contains("panicked in handle") && message.contains("deliberate test panic") && message.contains("test/panicker")),
            "wrong variant: {err:?}"
        );
    }

    #[test]
    fn handle_ok_passes_through() {
        fn happy(_: PluginRequest) -> PluginResponse {
            PluginResponse {
                payload: r#"{"ok":true}"#.to_string(),
            }
        }
        let resp = call_handle_catch_unwind(
            "test/happy",
            happy,
            PluginRequest {
                payload: "{}".to_string(),
            },
        )
        .expect("no panic");
        assert_eq!(resp.payload, r#"{"ok":true}"#);
    }

    // panic_message: a `String` payload (as produced by `panic!("{}", s)`
    // with a runtime-formatted argument) must be surfaced verbatim. This
    // hits the `downcast_ref::<String>()` arm, distinct from the
    // `&'static str` arm covered by `handle_panic_is_caught_and_reported`.
    #[test]
    fn panic_message_extracts_owned_string() {
        let payload: Box<dyn std::any::Any + Send> = Box::new(String::from("owned panic text"));
        assert_eq!(panic_message(&payload), "owned panic text");
    }

    // panic_message: a payload that is neither `&'static str` nor `String`
    // must fall through to the placeholder rather than panic. Hits the
    // final `else` arm.
    #[test]
    fn panic_message_handles_non_string_payload() {
        let payload: Box<dyn std::any::Any + Send> = Box::new(42u32);
        assert_eq!(panic_message(&payload), "<non-string panic payload>");
    }

    // A `String`-payload panic routed through the full guard must still be
    // caught and tagged with the plugin path and message text.
    #[test]
    fn handle_string_panic_is_caught() {
        fn boom(_: PluginRequest) -> PluginResponse {
            let detail = "dynamic".to_string();
            panic!("{detail} panic value")
        }
        let err = call_handle_catch_unwind(
            "test/stringpanic",
            boom,
            PluginRequest {
                payload: "{}".to_string(),
            },
        )
        .expect_err("panic must convert to Err");
        assert!(
            matches!(&err, RuntimeError::InvalidArgument { message } if message.contains("dynamic panic value") && message.contains("test/stringpanic")),
            "wrong variant: {err:?}"
        );
    }
}

#[cfg(test)]
mod error_mapper_tests {
    use super::*;
    use serde_json::json;

    // A serde_json::Error is only constructible via a failing parse/serialize.
    fn json_error() -> serde_json::Error {
        serde_json::from_str::<serde_json::Value>("{ not json").unwrap_err()
    }

    // dylib_payload_io tags the artifact path and prefixes the context string
    // before the underlying serde error text.
    #[test]
    fn dylib_payload_io_tags_path_and_context() {
        let err = dylib_payload_io(
            Path::new("/artifacts/plugin.dylib"),
            "runtime fingerprint parse failed",
            json_error(),
        );
        assert!(
            matches!(&err, RuntimeError::Io { path, message } if path == &PathBuf::from("/artifacts/plugin.dylib") && message.starts_with("runtime fingerprint parse failed: ")),
            "wrong variant: {err:?}"
        );
    }

    // plugin_invocation_failed carries the plugin path and the verbatim
    // message through to the PluginInvocationFailed variant.
    #[test]
    fn plugin_invocation_failed_carries_path_and_message() {
        let err = plugin_invocation_failed("json/echo", "spawn foo failed: nope".to_string());
        assert!(
            matches!(&err, RuntimeError::PluginInvocationFailed { plugin_path, message } if plugin_path == "json/echo" && message == "spawn foo failed: nope"),
            "wrong variant: {err:?}"
        );
    }

    // payload_reserialize_invariant maps to an Invariant with the fixed prefix.
    #[test]
    fn payload_reserialize_invariant_is_invariant() {
        let err = payload_reserialize_invariant(json_error());
        assert!(
            matches!(&err, RuntimeError::Invariant { message } if message.starts_with("plugin invoke payload re-serialize failed: ")),
            "wrong variant: {err:?}"
        );
    }

    // No configured settings → payload passes through byte-for-byte.
    #[test]
    fn inject_plugin_settings_without_settings_is_a_noop() {
        let mut payload = json!({"path": "/tmp/x"});
        inject_plugin_settings(&mut payload, None);
        assert_eq!(payload, json!({"path": "/tmp/x"}));
    }

    // `settings: {}` is `PluginConfigFile`'s default for a yaml that only sets
    // `plugin`/`enabled`; injecting it would add noise to every plugin's
    // payload, so empty settings are skipped (null likewise).
    #[test]
    fn inject_plugin_settings_skips_empty_settings() {
        let mut payload = json!({"path": "/tmp/x"});
        inject_plugin_settings(&mut payload, Some(&json!({})));
        assert_eq!(payload, json!({"path": "/tmp/x"}));

        inject_plugin_settings(&mut payload, Some(&json!(null)));
        assert_eq!(payload, json!({"path": "/tmp/x"}));

        // Non-object settings are not "empty": whatever the operator wrote is
        // handed to the plugin as-is.
        inject_plugin_settings(&mut payload, Some(&json!(42)));
        assert_eq!(payload, json!({"path": "/tmp/x", "settings": 42}));
    }

    // Configured settings land under the `settings` key.
    #[test]
    fn inject_plugin_settings_inserts_configured_settings() {
        let mut payload = json!({"path": "/tmp/x"});
        let settings = json!({"read_roots": ["/tmp/root"]});
        inject_plugin_settings(&mut payload, Some(&settings));
        assert_eq!(
            payload,
            json!({"path": "/tmp/x", "settings": {"read_roots": ["/tmp/root"]}})
        );
    }

    // A caller-supplied `settings` wins: config only fills what is absent, so
    // an explicit tool argument can never be silently overridden by a file.
    #[test]
    fn inject_plugin_settings_keeps_caller_settings() {
        let mut payload = json!({"settings": {"read_roots": ["/caller"]}});
        let settings = json!({"read_roots": ["/config"]});
        inject_plugin_settings(&mut payload, Some(&settings));
        assert_eq!(payload, json!({"settings": {"read_roots": ["/caller"]}}));
    }

    // Non-object payloads keep their shape — there is no place to hang a
    // `settings` key, and rewriting the payload would change the plugin's
    // view of what the caller sent.
    #[test]
    fn inject_plugin_settings_leaves_non_object_payload_alone() {
        let mut payload = json!([1, 2, 3]);
        inject_plugin_settings(&mut payload, Some(&json!({"read_roots": ["/config"]})));
        assert_eq!(payload, json!([1, 2, 3]));
    }
}
