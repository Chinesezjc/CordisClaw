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
    /// Toolchain facts default to the environment that compiled this SDK
    /// crate, so declarations (e.g. `[package.metadata.cordis.abi_fingerprint]`
    /// in a plugin's Cargo.toml) may omit them and stay portable across
    /// machines; hardcoding them pins the artifact to one toolchain/host.
    #[serde(default = "default_rustc_version")]
    pub rustc_version: String,
    #[serde(default = "default_target_triple")]
    pub target_triple: String,
    pub crate_hash: String,
    pub api_hash: String,
}

fn default_rustc_version() -> String {
    env!("CORDIS_RUSTC_VERSION").to_string()
}

fn default_target_triple() -> String {
    env!("CORDIS_TARGET").to_string()
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
    let ptr = unsafe { libc::dlsym(libc::RTLD_DEFAULT, c"_cordis_agent_trigger".as_ptr()) };
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
pub extern "C" fn _cordis_create_service(
    _node_id: *const std::ffi::c_char,
) -> *const ServiceVTable {
    std::ptr::null()
}

#[cfg(test)]
mod constructor_and_helper_tests {
    use super::*;

    #[test]
    fn fingerprint_diff_reports_each_changed_field() {
        let base = AbiFingerprint {
            rustc_version: "rustc 1".to_string(),
            target_triple: "t1".to_string(),
            crate_hash: "c1".to_string(),
            api_hash: "a1".to_string(),
        };
        // Identical fingerprints diff to empty.
        assert!(base.diff(&base).is_empty());
        // Every field differs -> four diff entries in field order.
        let other = AbiFingerprint {
            rustc_version: "rustc 2".to_string(),
            target_triple: "t2".to_string(),
            crate_hash: "c2".to_string(),
            api_hash: "a2".to_string(),
        };
        let diff = base.diff(&other);
        assert_eq!(diff.len(), 4);
        assert!(diff[0].starts_with("rustc_version:"));
        assert!(diff[1].starts_with("target_triple:"));
        assert!(diff[2].starts_with("crate_hash:"));
        assert!(diff[3].starts_with("api_hash:"));
        assert!(diff[2].contains("c1!=c2"));
    }

    #[test]
    fn plugin_docs_constructor_maps_optionals() {
        let docs = plugin_docs(
            "id",
            "root/p",
            "0.2.0",
            Some("cmd"),
            vec![node_doc(
                "n0",
                "summary",
                serde_json::json!({}),
                serde_json::json!({}),
                &["writes"],
                &["boom"],
            )],
            Some("hint text"),
        );
        assert_eq!(docs.plugin_id, "id");
        assert_eq!(docs.plugin_path, "root/p");
        assert_eq!(docs.plugin_version, "0.2.0");
        assert_eq!(docs.abi_version, DEFAULT_ABI_VERSION);
        assert_eq!(docs.command_name.as_deref(), Some("cmd"));
        assert_eq!(docs.system_hint.as_deref(), Some("hint text"));
        assert_eq!(docs.nodes.len(), 1);
        // None variants pass through as None.
        let bare = plugin_docs("id", "p", "0.1.0", None, vec![], None);
        assert!(bare.command_name.is_none());
        assert!(bare.system_hint.is_none());
    }

    #[test]
    fn node_doc_constructor_defaults_to_router() {
        let doc = node_doc(
            "n0",
            "sum",
            serde_json::json!({"a": 1}),
            serde_json::json!({"b": 2}),
            &["se1", "se2"],
            &["fm1"],
        );
        assert_eq!(doc.id, "n0");
        assert_eq!(doc.summary, "sum");
        assert_eq!(doc.side_effects, vec!["se1", "se2"]);
        assert_eq!(doc.failure_modes, vec!["fm1"]);
        assert!(matches!(doc.node_type, NodeType::Router));
        assert!(!doc.agent_accessible);
    }

    #[test]
    fn task_node_doc_sets_task_type() {
        // Non-empty side_effects / failure_modes so the mapping closures that
        // copy each &str into an owned String are exercised.
        let doc = task_node_doc(
            "svc",
            "background",
            serde_json::json!({}),
            serde_json::json!({}),
            &["writes-log"],
            &["service-crash"],
        );
        assert!(matches!(doc.node_type, NodeType::Task));
        assert!(!doc.agent_accessible);
        assert_eq!(doc.side_effects, vec!["writes-log"]);
        assert_eq!(doc.failure_modes, vec!["service-crash"]);
    }

    #[test]
    fn with_agent_accessible_flips_flag() {
        let doc = node_doc(
            "n",
            "s",
            serde_json::json!({}),
            serde_json::json!({}),
            &[],
            &[],
        )
        .with_agent_accessible();
        assert!(doc.agent_accessible);
    }

    #[test]
    fn json_response_serializes_payload() {
        let doc = node_doc(
            "n",
            "s",
            serde_json::json!({}),
            serde_json::json!({}),
            &[],
            &[],
        );
        let resp = json_response(&doc);
        let round: NodeDoc = serde_json::from_str(&resp.payload).expect("valid json");
        assert_eq!(round.id, "n");
    }

    #[test]
    fn pretty_json_is_multiline() {
        let value = serde_json::json!({ "a": 1, "b": 2 });
        let text = pretty_json(&value);
        assert!(text.contains('\n'), "pretty output should be indented");
    }

    #[test]
    fn agent_trigger_null_symbol_path_is_noop() {
        // In the test process the host symbol `_cordis_agent_trigger` is not
        // exported, so dlsym returns null and agent_trigger must return
        // without invoking anything (exercises the null-branch).
        //
        // NB: this only holds while no other test has dlopen'd a provider of
        // that symbol with RTLD_GLOBAL. `agent_trigger_success_branch_via_...`
        // does exactly that, but it makes no assertion about which branch this
        // test takes — the contract here is simply "must not panic".
        agent_trigger("no host present");
    }

    #[test]
    fn agent_trigger_success_branch_via_dlopened_provider() {
        // Exercise the non-null branch of `agent_trigger`: the dlsym lookup
        // must resolve `_cordis_agent_trigger` and the transmuted fn must be
        // invoked. Test binaries on macOS do not export their own symbols to
        // `dlsym(RTLD_DEFAULT)`, so we compile a tiny, dependency-free helper
        // cdylib that exports `_cordis_agent_trigger` (writing its argument to
        // a file named by `$COV_TRIG_OUT`) and dlopen it with RTLD_GLOBAL so
        // its symbols join the global scope RTLD_DEFAULT searches.
        let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string());

        let dir = std::env::temp_dir().join(format!(
            "cordis-sdk-trig-{}-{:p}",
            std::process::id(),
            &rustc as *const _
        ));
        let _ = std::fs::create_dir_all(&dir);
        let src = dir.join("helper.rs");
        let out_marker = dir.join("trigger.out");
        let dylib = dir.join("libcordis_trig_helper.dylib");

        std::fs::write(
            &src,
            r#"
use std::os::raw::c_char;
#[no_mangle]
pub extern "C" fn _cordis_agent_trigger(msg: *const c_char) {
    let s = unsafe { std::ffi::CStr::from_ptr(msg) }.to_string_lossy().into_owned();
    if let Ok(path) = std::env::var("COV_TRIG_OUT") {
        let _ = std::fs::write(path, format!("triggered:{s}"));
    }
}
"#,
        )
        .expect("write helper source");

        // These tests only ever execute inside a Rust toolchain (the coverage
        // job runs on a rustc-provisioned CI image), so a missing/failing rustc
        // is a broken environment, not a case to silently skip — fail loudly
        // rather than report false coverage on the branches below.
        let status = std::process::Command::new(&rustc)
            .args(["--crate-type", "cdylib", "--edition", "2021", "-o"])
            .arg(&dylib)
            .arg(&src)
            .status()
            .expect("rustc must be runnable to compile the trigger helper");
        assert!(
            status.success() && dylib.exists(),
            "trigger helper cdylib must build"
        );

        // The helper reads COV_TRIG_OUT when called. Setting it in-process is
        // fine because the call happens synchronously below.
        std::env::set_var("COV_TRIG_OUT", &out_marker);
        let _ = std::fs::remove_file(&out_marker);

        // dlopen RTLD_NOW | RTLD_GLOBAL so the export is visible to
        // dlsym(RTLD_DEFAULT). Leak the handle: the symbol must stay resident
        // for the (synchronous) trigger call; unloading is unnecessary in a
        // short-lived test process.
        let c_path = std::ffi::CString::new(dylib.to_string_lossy().as_bytes()).unwrap();
        let handle = unsafe { libc::dlopen(c_path.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) };
        assert!(!handle.is_null(), "dlopen of helper cdylib failed");

        agent_trigger("hello from success branch");

        let written = std::fs::read_to_string(&out_marker).expect("trigger must write marker file");
        assert_eq!(written, "triggered:hello from success branch");

        std::env::remove_var("COV_TRIG_OUT");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_service_returns_null_in_test_build() {
        // The `#[cfg(test)]` stub always returns null.
        let ptr = _cordis_create_service(std::ptr::null());
        assert!(ptr.is_null());
    }

    #[test]
    fn guard_service_call_string_panic_payload_reported() {
        // Panic with a `String` payload (not `&str`) exercises the
        // downcast_ref::<String>() branch in guard_service_call.
        fn panic_string(_data: *mut std::ffi::c_void) -> i32 {
            std::panic::panic_any(String::from("string payload boom"));
        }
        let code = guard_service_call("start", std::ptr::null_mut(), panic_string);
        assert_eq!(code, -1);
    }

    #[test]
    fn guard_service_call_non_string_panic_payload_reported() {
        // Panic with a payload that is neither &str nor String exercises the
        // final `else` branch ("<non-string panic payload>").
        fn panic_int(_data: *mut std::ffi::c_void) -> i32 {
            std::panic::panic_any(42_u64);
        }
        let code = guard_service_call("stop", std::ptr::null_mut(), panic_int);
        assert_eq!(code, -1);
    }

    #[test]
    fn guard_service_call_null_data_success_branch() {
        // A well-behaved body returning early on null data (return 7).
        fn null_ok(data: *mut std::ffi::c_void) -> i32 {
            if data.is_null() {
                return 7;
            }
            0
        }
        assert_eq!(
            guard_service_call("start", std::ptr::null_mut(), null_ok),
            7
        );
        // Non-null data falls through the early return to the tail `0`.
        let mut sentinel: i32 = 0;
        assert_eq!(
            guard_service_call(
                "start",
                (&mut sentinel as *mut i32) as *mut std::ffi::c_void,
                null_ok
            ),
            0
        );
    }

    // Exercise the `export_plugin_api!` macro expansion: the generated
    // `__cordis_sdk_*` functions and the static vtable must be callable.
    mod exported {
        use super::super::*;

        fn fingerprint() -> AbiFingerprint {
            AbiFingerprint::current_build("crate_macro", "api_macro")
        }
        fn docs() -> PluginDocs {
            plugin_docs("macro_id", "root/macro", "0.1.0", None, vec![], None)
        }
        fn handle(req: PluginRequest) -> PluginResponse {
            PluginResponse {
                payload: format!("echo:{}", req.payload),
            }
        }

        export_plugin_api! {
            abi_fingerprint = fingerprint(),
            docs = docs(),
            handle = handle,
        }

        #[test]
        fn macro_generated_vtable_dispatches() {
            assert!(matches!(
                cordis_plugin_api_rust_v2.abi_kind,
                DylibAbiKind::Rust
            ));
            // abi_fingerprint entry serializes the current-build fingerprint.
            let fp_resp = (cordis_plugin_api_rust_v2.abi_fingerprint)();
            let fp: AbiFingerprint =
                serde_json::from_str(&fp_resp.payload).expect("fingerprint json");
            assert_eq!(fp.crate_hash, "crate_macro");
            // docs entry serializes plugin docs.
            let docs_resp = (cordis_plugin_api_rust_v2.docs)();
            let parsed: PluginDocs = serde_json::from_str(&docs_resp.payload).expect("docs json");
            assert_eq!(parsed.plugin_path, "root/macro");
            // handle entry dispatches to our body.
            let resp = (cordis_plugin_api_rust_v2.handle)(PluginRequest {
                payload: "ping".to_string(),
            });
            assert_eq!(resp.payload, "echo:ping");
        }
    }
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
        assert!(
            fp.rustc_version.starts_with("rustc"),
            "looks like rustc --version output: {}",
            fp.rustc_version
        );
        assert!(!fp.target_triple.is_empty(), "target_triple stamped");
        assert_eq!(fp.crate_hash, "crate_test_v1");
        assert_eq!(fp.api_hash, "api_v2");
    }

    #[test]
    fn fingerprint_toolchain_fields_default_to_current_build() {
        // Declarations may omit rustc_version/target_triple (e.g. the
        // `[package.metadata.cordis.abi_fingerprint]` table); they must
        // fill in from the toolchain that built this SDK so contracts
        // stay portable across host machines.
        let fp: AbiFingerprint =
            serde_json::from_str(r#"{"crate_hash":"crate_x_v1","api_hash":"api_v2"}"#).unwrap();
        let built = AbiFingerprint::current_build("crate_x_v1", "api_v2");
        assert_eq!(fp, built);
        // Explicit values still win over the defaults.
        let pinned: AbiFingerprint = serde_json::from_str(
            r#"{"rustc_version":"rustc 0.0.0","target_triple":"t","crate_hash":"c","api_hash":"a"}"#,
        )
        .unwrap();
        assert_eq!(pinned.rustc_version, "rustc 0.0.0");
        assert_eq!(pinned.target_triple, "t");
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
        assert_eq!(
            code, -1,
            "extern C start shim must catch the panic and return -1"
        );
        // The stop path must be equally protected.
        let stop_code = (vtable.stop)(vtable.data);
        assert_eq!(
            stop_code, -1,
            "extern C stop shim must catch the panic and return -1"
        );
    }

    #[test]
    fn service_vtable_macro_passes_through_success_code() {
        // Null data drives `ok_start` through its early-return branch.
        assert_eq!(
            guard_service_call("start", std::ptr::null_mut(), ok_start),
            7,
            "null data returns the early-return sentinel"
        );
        let mut flag: i32 = 0;
        let vtable = service_vtable! {
            data = (&mut flag as *mut i32) as *mut c_void,
            start = ok_start,
            stop = ok_stop,
        };
        assert_eq!(
            (vtable.start)(vtable.data),
            0,
            "non-panicking start returns its own code"
        );
        assert_eq!(
            (vtable.stop)(vtable.data),
            0,
            "non-panicking stop returns 0"
        );
    }
}
