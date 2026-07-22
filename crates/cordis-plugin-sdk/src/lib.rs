use serde::{Deserialize, Serialize};

pub mod workflow;

pub use workflow::{
    session, AskUserSpec, CallSpec, EventSpec, JoinPolicy, JoinSpec, RacePolicy, RaceSpec,
    SleepSpec, WaitFuture, WaitHandle, WaitKind, WaitOutcome, WaitSpec, WorkflowError,
    WorkflowErrorKind, WorkflowRuntime, WorkflowSession,
};

pub const RUST_PLUGIN_ENTRY_SYMBOL: &str = "cordis_plugin_api_rust_v2";
pub const DEFAULT_ABI_VERSION: u32 = 2;

#[repr(u8)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum DylibAbiKind {
    #[default]
    Rust,
}

#[repr(C)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct AbiFingerprint {
    pub rustc_version: String,
    pub target_triple: String,
    pub crate_hash: String,
    pub api_hash: String,
}

/// Target triple of the toolchain that compiled this SDK crate (and, by
/// extension, any binary linking it). Loaders compare this against a dylib
/// artifact's recorded `target_triple` before attempting to dlopen it.
pub const CORDIS_TARGET: &str = env!("CORDIS_TARGET");

impl AbiFingerprint {
    /// P2-13: construct an `AbiFingerprint` whose `rustc_version` and
    /// `target_triple` come from the compiler that built this SDK crate.
    /// Plugins call this in their `abi_fingerprint_value()` and supply
    /// only the plugin-specific `crate_hash` / `api_hash`, e.g.:
    ///
    /// ```ignore
    /// fn abi_fingerprint_value() -> AbiFingerprint {
    ///     AbiFingerprint::current_build("crate_my_plugin_v1", "api_v2")
    /// }
    /// ```
    ///
    /// Values are baked in by `build.rs`; two builds on different
    /// toolchains produce different fingerprints automatically.
    pub fn current_build(crate_hash: impl Into<String>, api_hash: impl Into<String>) -> Self {
        Self {
            rustc_version: env!("CORDIS_RUSTC_VERSION").to_string(),
            target_triple: env!("CORDIS_TARGET").to_string(),
            crate_hash: crate_hash.into(),
            api_hash: api_hash.into(),
        }
    }

    pub fn diff(&self, other: &Self) -> Vec<String> {
        let mut out = Vec::new();
        if self.rustc_version != other.rustc_version {
            out.push(format!(
                "rustc_version:{}!={}",
                self.rustc_version, other.rustc_version
            ));
        }
        if self.target_triple != other.target_triple {
            out.push(format!(
                "target_triple:{}!={}",
                self.target_triple, other.target_triple
            ));
        }
        if self.crate_hash != other.crate_hash {
            out.push(format!(
                "crate_hash:{}!={}",
                self.crate_hash, other.crate_hash
            ));
        }
        if self.api_hash != other.api_hash {
            out.push(format!("api_hash:{}!={}", self.api_hash, other.api_hash));
        }
        out
    }
}

#[repr(C)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PluginDocs {
    pub plugin_id: String,
    pub plugin_path: String,
    pub plugin_version: String,
    pub abi_version: u32,
    #[serde(default)]
    pub command_name: Option<String>,
    #[serde(default)]
    pub nodes: Vec<NodeDoc>,
    /// Optional hint injected into the Agent's system prompt when this plugin
    /// is loaded. Use for plugin-specific usage instructions, protocol
    /// conventions (e.g. "output suspend for casual chat"), or behavioural
    /// rules that the Agent should follow when interacting with this plugin.
    #[serde(default)]
    pub system_hint: Option<String>,
}

#[repr(C)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NodeDoc {
    pub id: String,
    pub summary: String,
    pub input_schema: serde_json::Value,
    pub output_schema: serde_json::Value,
    #[serde(default)]
    pub side_effects: Vec<String>,
    #[serde(default)]
    pub failure_modes: Vec<String>,
    /// Node type: Task (long-running background service), Router, Gate, or
    /// Terminal.  Defaults to Router for backward compatibility.
    #[serde(default)]
    pub node_type: NodeType,
    /// Set to true if the Agent (LLM) is allowed to invoke this node
    /// directly via invoke_plugin/execute_target.  Default: false.
    #[serde(default)]
    pub agent_accessible: bool,
}

impl NodeDoc {
    pub fn with_agent_accessible(mut self) -> Self {
        self.agent_accessible = true;
        self
    }
}

/// Class of execution semantics for a plugin node.
#[repr(C)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum NodeType {
    /// Long-running background service with lifecycle (start/stop).
    Task,
    /// Conditionally routes execution to one of several downstream nodes.
    #[default]
    Router,
    /// Guards a subgraph behind a policy check.
    Gate,
    /// Terminal node — produces a final output and ends the execution.
    Terminal,
}

// P2-5: `#[repr(C)]` on structs containing `String` is misleading. The
// outer struct layout (field offsets, sizes) is C-stable, but `String`'s
// internal layout is NOT stable across rustc versions or crates. Passing
// these across a real FFI boundary is only safe when the host and plugin
// are compiled with the exact same rustc release (which the runtime
// enforces via `AbiFingerprint::rustc_version`).
//
// If you're introducing a truly cross-toolchain plugin ABI, replace the
// `String` field with `*mut c_char + len + free_fn` — do NOT rely on this
// annotation alone.
#[repr(C)]
#[derive(Debug, Clone)]
pub struct PluginRequest {
    pub payload: String,
}

#[repr(C)]
#[derive(Debug, Clone)]
pub struct PluginResponse {
    pub payload: String,
}

#[repr(C)]
pub struct RustPluginApiV2 {
    pub abi_kind: DylibAbiKind,
    pub abi_fingerprint: fn() -> PluginResponse,
    pub docs: fn() -> PluginResponse,
    pub handle: fn(PluginRequest) -> PluginResponse,
}

pub fn json_response<T: Serialize>(value: &T) -> PluginResponse {
    PluginResponse {
        payload: serde_json::to_string(value).expect("plugin sdk serialize response"),
    }
}

pub fn agent_trigger(msg: &str) {
    // Resolve at runtime via dlsym so plugins don't get a hard
    // undefined-symbol dependency on the host binary.
    type TriggerFn = unsafe extern "C" fn(*const std::ffi::c_char);
    let ptr = unsafe {
        libc::dlsym(libc::RTLD_DEFAULT, c"_cordis_agent_trigger".as_ptr())
    };
    if !ptr.is_null() {
        let c_str = std::ffi::CString::new(msg).unwrap();
        unsafe {
            let trigger: TriggerFn = std::mem::transmute(ptr);
            trigger(c_str.as_ptr());
        }
    }
}

pub fn pretty_json<T: Serialize>(value: &T) -> String {
    serde_json::to_string_pretty(value).expect("plugin sdk serialize pretty json")
}

pub fn plugin_docs(
    plugin_id: impl Into<String>,
    plugin_path: impl Into<String>,
    plugin_version: impl Into<String>,
    command_name: Option<&str>,
    nodes: Vec<NodeDoc>,
    system_hint: Option<&str>,
) -> PluginDocs {
    PluginDocs {
        plugin_id: plugin_id.into(),
        plugin_path: plugin_path.into(),
        plugin_version: plugin_version.into(),
        abi_version: DEFAULT_ABI_VERSION,
        command_name: command_name.map(ToString::to_string),
        nodes,
        system_hint: system_hint.map(ToString::to_string),
    }
}

pub fn node_doc(
    id: impl Into<String>,
    summary: impl Into<String>,
    input_schema: serde_json::Value,
    output_schema: serde_json::Value,
    side_effects: &[&str],
    failure_modes: &[&str],
) -> NodeDoc {
    NodeDoc {
        id: id.into(),
        summary: summary.into(),
        input_schema,
        output_schema,
        side_effects: side_effects.iter().map(|v| (*v).to_string()).collect(),
        failure_modes: failure_modes.iter().map(|v| (*v).to_string()).collect(),
        node_type: NodeType::Router,
        agent_accessible: false,
    }
}

pub fn task_node_doc(
    id: impl Into<String>,
    summary: impl Into<String>,
    input_schema: serde_json::Value,
    output_schema: serde_json::Value,
    side_effects: &[&str],
    failure_modes: &[&str],
) -> NodeDoc {
    NodeDoc {
        id: id.into(),
        summary: summary.into(),
        input_schema,
        output_schema,
        side_effects: side_effects.iter().map(|v| (*v).to_string()).collect(),
        failure_modes: failure_modes.iter().map(|v| (*v).to_string()).collect(),
        node_type: NodeType::Task,
        agent_accessible: false,
    }
}

#[macro_export]
macro_rules! export_plugin_api {
    (
        abi_fingerprint = $abi_fingerprint:expr,
        docs = $docs:expr,
        handle = $handle:path $(,)?
    ) => {
        fn __cordis_sdk_abi_fingerprint() -> $crate::PluginResponse {
            $crate::json_response(&$abi_fingerprint)
        }

        fn __cordis_sdk_docs() -> $crate::PluginResponse {
            $crate::json_response(&$docs)
        }

        #[no_mangle]
        pub static cordis_plugin_api_rust_v2: $crate::RustPluginApiV2 = $crate::RustPluginApiV2 {
            abi_kind: $crate::DylibAbiKind::Rust,
            abi_fingerprint: __cordis_sdk_abi_fingerprint,
            docs: __cordis_sdk_docs,
            handle: $handle,
        };
    };
}


// ── Service Factory Bridge ─────────────────────────────────────────────

/// C-compatible function table for a Service.
/// Plugins fill this in and return it from `_cordis_create_service`.
#[repr(C)]
pub struct ServiceVTable {
    /// Opaque data pointer passed to start/stop.
    pub data: *mut std::ffi::c_void,
    /// Start the service. Returns 0 on success, non-zero on error.
    pub start: extern "C" fn(*mut std::ffi::c_void) -> i32,
    /// Stop the service. Returns 0 on success, non-zero on error.
    pub stop: extern "C" fn(*mut std::ffi::c_void) -> i32,
}

// Safety: function pointers in the vtable are thread-safe (they only
// access plugin-local state behind mutexes).
unsafe impl Send for ServiceVTable {}
unsafe impl Sync for ServiceVTable {}

/// Panic firewall for service `start`/`stop` calls.
///
/// `ServiceVTable::{start,stop}` are `extern "C" fn`.  Modern rustc inserts an
/// implicit `abort()` when a Rust panic tries to unwind *through* a C ABI
/// frame, so a `catch_unwind` on the runtime (host) side is futile — the
/// process is already gone by the time control would return.  The isolation
/// therefore has to happen **inside** the plugin, on Rust frames, before the
/// unwind ever reaches the `extern "C"` boundary.
///
/// This helper runs the user's (panic-prone) service body under
/// `catch_unwind`.  On a caught panic it prints a contextual diagnostic and
/// returns `-1`, so the surrounding `extern "C"` shim returns an ordinary
/// error code instead of unwinding.  Use it via [`service_vtable!`] rather
/// than hand-writing `extern "C" fn`s.
pub fn guard_service_call(
    op: &str,
    data: *mut std::ffi::c_void,
    body: fn(*mut std::ffi::c_void) -> i32,
) -> i32 {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| body(data))) {
        Ok(code) => code,
        Err(payload) => {
            let msg = if let Some(s) = payload.downcast_ref::<&str>() {
                (*s).to_string()
            } else if let Some(s) = payload.downcast_ref::<String>() {
                s.clone()
            } else {
                "<non-string panic payload>".to_string()
            };
            eprintln!(
                "cordis-plugin-sdk: service {op} panicked, isolated before C ABI boundary (returning -1): {msg}"
            );
            -1
        }
    }
}

/// Build a [`ServiceVTable`] whose `start`/`stop` entry points are wrapped in a
/// panic firewall ([`guard_service_call`]).
///
/// Plugins supply ordinary safe Rust `fn(*mut c_void) -> i32` bodies (which are
/// *allowed to panic*); the macro generates the `extern "C"` shims and routes
/// them through `catch_unwind`, so a panicking service can never abort the
/// host process.  Return `0` for success, non-zero for a handled error; a
/// panic is reported as `-1`.
///
/// ```ignore
/// #[no_mangle]
/// pub extern "C" fn _cordis_create_service(
///     node_id: *const std::ffi::c_char,
/// ) -> *const cordis_plugin_sdk::ServiceVTable {
///     // ... match node_id, build `data` ...
///     let vtable = cordis_plugin_sdk::service_vtable! {
///         data = my_data_ptr,
///         start = my_start,   // fn(*mut c_void) -> i32, may panic
///         stop = my_stop,     // fn(*mut c_void) -> i32, may panic
///     };
///     Box::into_raw(Box::new(vtable))
/// }
/// ```
#[macro_export]
macro_rules! service_vtable {
    (
        data = $data:expr,
        start = $start:path,
        stop = $stop:path $(,)?
    ) => {{
        extern "C" fn __cordis_service_start(data: *mut ::std::ffi::c_void) -> i32 {
            $crate::guard_service_call("start", data, $start)
        }
        extern "C" fn __cordis_service_stop(data: *mut ::std::ffi::c_void) -> i32 {
            $crate::guard_service_call("stop", data, $stop)
        }
        $crate::ServiceVTable {
            data: $data,
            start: __cordis_service_start,
            stop: __cordis_service_stop,
        }
    }};
}

#[cfg(not(test))]
extern "C" {
    /// Plugin exports this.  Called with a node_id; returns a ServiceVTable
    /// or null if the node doesn't have a Service implementation.
    fn _cordis_create_service(node_id: *const std::ffi::c_char) -> *const ServiceVTable;
}

#[cfg(test)]
#[no_mangle]
pub extern "C" fn _cordis_create_service(_node_id: *const std::ffi::c_char) -> *const ServiceVTable {
    std::ptr::null()
}

#[cfg(test)]
mod fingerprint_tests {
    use super::*;

    #[test]
    fn current_build_populates_rustc_and_target() {
        // P2-13: the two env values are stamped by build.rs from cargo's
        // `RUSTC` and `TARGET`. Both should be non-empty; rustc_version
        // should start with the word "rustc".
        let fp = AbiFingerprint::current_build("crate_test_v1", "api_v2");
        assert!(!fp.rustc_version.is_empty(), "rustc_version stamped");
        assert!(fp.rustc_version.starts_with("rustc"), "looks like rustc --version output: {}", fp.rustc_version);
        assert!(!fp.target_triple.is_empty(), "target_triple stamped");
        assert_eq!(fp.crate_hash, "crate_test_v1");
        assert_eq!(fp.api_hash, "api_v2");
    }

    #[test]
    fn default_dylib_abi_kind_is_rust() {
        // Derived `#[default]` on the `Rust` variant must preserve the
        // previous hand-written `impl Default` behaviour.
        assert!(matches!(DylibAbiKind::default(), DylibAbiKind::Rust));
    }

    #[test]
    fn default_node_type_is_router() {
        // Derived `#[default]` on the `Router` variant must preserve the
        // previous hand-written `impl Default` behaviour (used by
        // `#[serde(default)]` on `NodeDoc::node_type`).
        assert!(matches!(NodeType::default(), NodeType::Router));
    }
}

#[cfg(test)]
mod service_panic_isolation_tests {
    use super::*;
    use std::ffi::c_void;

    // A service `start` body that panics.  Under `guard_service_call` (and thus
    // through the `service_vtable!`-generated `extern "C"` shim) this must be
    // caught on Rust frames and reported as -1 — the process must NOT abort.
    fn panicking_start(_data: *mut c_void) -> i32 {
        panic!("boom from service start");
    }

    fn panicking_stop(_data: *mut c_void) -> i32 {
        panic!("boom from service stop");
    }

    // A well-behaved body: reads a flag out of the data pointer and returns it.
    fn ok_start(data: *mut c_void) -> i32 {
        if data.is_null() {
            return 7;
        }
        unsafe { *(data as *mut i32) }
    }

    fn ok_stop(_data: *mut c_void) -> i32 {
        0
    }

    #[test]
    fn guarded_panicking_start_returns_minus_one_without_abort() {
        // Directly exercise the firewall.  If the panic escaped, the test
        // binary would abort and this assert would never run.
        let code = guard_service_call("start", std::ptr::null_mut(), panicking_start);
        assert_eq!(code, -1, "panicking start must be isolated as -1");
    }

    #[test]
    fn service_vtable_macro_isolates_start_panic() {
        // Build a real vtable via the public macro and call it exactly the way
        // the runtime does: `(vtable.start)(vtable.data)`.
        let vtable = service_vtable! {
            data = std::ptr::null_mut(),
            start = panicking_start,
            stop = panicking_stop,
        };
        let code = (vtable.start)(vtable.data);
        assert_eq!(code, -1, "extern C start shim must catch the panic and return -1");
        // The stop path must be equally protected.
        let stop_code = (vtable.stop)(vtable.data);
        assert_eq!(stop_code, -1, "extern C stop shim must catch the panic and return -1");
    }

    #[test]
    fn service_vtable_macro_passes_through_success_code() {
        let mut flag: i32 = 0;
        let vtable = service_vtable! {
            data = (&mut flag as *mut i32) as *mut c_void,
            start = ok_start,
            stop = ok_stop,
        };
        assert_eq!((vtable.start)(vtable.data), 0, "non-panicking start returns its own code");
        assert_eq!((vtable.stop)(vtable.data), 0, "non-panicking stop returns 0");
    }
}
