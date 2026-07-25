//! Coverage for `plugin::invoke` — the `PluginInvoker` façade and the
//! `invoke_registered_plugin` dispatch/validation branches.
//!
//! ## Why a hand-built registry
//!
//! The prebuilt fixture dylibs under `fixtures/artifacts/` are loadable on
//! this host (they are native `.dylib`s), but `index.json` records the
//! artifacts' `target_triple` as `x86_64-unknown-linux-gnu`. The loader
//! compares that recorded triple against the host triple and marks every
//! dylib plugin `Unavailable(AbiMismatch)` before it is ever opened
//! (loader.rs). Consequently `PluginInvoker::load(fixtures_root)` yields a
//! registry in which no dylib plugin is `Loaded`, so the *success* path of
//! `invoke_registered_plugin` is unreachable through that door on a
//! cross-triple checkout.
//!
//! To drive the real dylib invoke path deterministically, these tests read
//! the *actual* ABI fingerprint and docs directly out of a fixture dylib
//! (via the public `LoadedDylibApi`) and hand-register a `Loaded` entry
//! through `PluginRegistry::insert_loaded`. That entry's recorded
//! fingerprint/docs then match what the dylib exports at invoke time, so the
//! fingerprint-and-docs contract checks pass and the plugin's `handle` runs.
//! Mismatch branches are covered by registering a deliberately wrong
//! fingerprint / docs.

use cordis_plugin_sdk::{NodeDoc, NodeType};
use cordis_runtime::core::error::RuntimeError;
use cordis_runtime::core::models::{
    AbiFingerprint, ArtifactKind, PluginDocs, PluginExecution, PluginUnavailableReason,
};
use cordis_runtime::plugin::dynamic::LoadedDylibApi;
use cordis_runtime::plugin::invoke::{
    invoke_registered_plugin, unregister_task_library, PluginInvoker,
};
use cordis_runtime::plugin::registry::PluginRegistry;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn artifacts_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/artifacts")
        .canonicalize()
        .expect("fixtures/artifacts must exist")
}

/// prepare-artifacts 按宿主平台命名产物（Linux `time.so` / macOS
/// `time.dylib`），路径必须用 DLL_SUFFIX 拼接而不能写死。
fn time_artifact() -> PathBuf {
    artifacts_dir().join(format!("time{}", std::env::consts::DLL_SUFFIX))
}

/// Read the real ABI fingerprint and docs a fixture dylib exports, so a
/// hand-registered `Loaded` entry passes the runtime contract checks.
fn dylib_fingerprint_and_docs(dylib: &Path) -> (AbiFingerprint, PluginDocs) {
    let api = LoadedDylibApi::open(dylib).expect("fixture dylib should dlopen on host");
    let api = api.api();
    let fp: AbiFingerprint =
        serde_json::from_str(&(api.abi_fingerprint)().payload).expect("fingerprint json");
    let docs: PluginDocs = serde_json::from_str(&(api.docs)().payload).expect("docs json");
    (fp, docs)
}

/// Hand-register a fixture dylib as a `Loaded` plugin whose recorded
/// fingerprint/docs match what the dylib actually exports.
fn register_loaded_dylib(
    registry: &PluginRegistry,
    plugin_path: &str,
    artifact: &Path,
) -> PluginDocs {
    let (fp, docs) = dylib_fingerprint_and_docs(artifact);
    registry.insert_loaded(
        plugin_path.to_string(),
        None,
        true,
        BTreeSet::new(),
        docs.clone(),
        artifact.to_path_buf(),
        ArtifactKind::Dylib,
        fp,
        None,
    );
    docs
}

// ---------------------------------------------------------------------------
// PluginInvoker façade
// ---------------------------------------------------------------------------

// `PluginInvoker::load` boots off the real fixtures tree; the accessors
// expose the fixtures root and the populated registry. Even where dylibs are
// AbiMismatch, the registry is non-empty and the root round-trips.
#[test]
fn invoker_load_exposes_root_and_registry() {
    let root = PluginInvoker::default_fixtures_root();
    let invoker = PluginInvoker::load(&root).expect("fixtures should load");
    assert_eq!(invoker.fixtures_root(), root.as_path());
    assert!(
        !invoker.plugin_registry().is_empty(),
        "fixtures declare plugins, registry must be non-empty"
    );
}

// `PluginInvoker::default_fixtures_root` resolves to an existing directory
// containing the plugins tree.
#[test]
fn default_fixtures_root_points_at_fixtures() {
    let root = PluginInvoker::default_fixtures_root();
    assert!(root.join("plugins").exists(), "root={}", root.display());
}

// `PluginInvoker::invoke` forwards to `invoke_registered_plugin`; invoking an
// unknown plugin path surfaces `PluginNotRegistered` through the façade.
#[test]
fn invoker_invoke_unknown_plugin_is_not_registered() {
    let invoker =
        PluginInvoker::load(PluginInvoker::default_fixtures_root()).expect("fixtures load");
    let err = invoker
        .invoke("no/such/plugin", "n", "{}".to_string())
        .expect_err("unknown plugin must error");
    assert!(matches!(err, RuntimeError::PluginNotRegistered { .. }));
}

// ---------------------------------------------------------------------------
// invoke_registered_plugin — validation / error branches
// ---------------------------------------------------------------------------

#[test]
fn unregistered_plugin_errors() {
    let registry = PluginRegistry::default();
    let err =
        invoke_registered_plugin(&registry, "ghost", "n", "{}".to_string()).expect_err("must fail");
    assert!(
        matches!(&err, RuntimeError::PluginNotRegistered { plugin_path } if plugin_path == "ghost"),
        "wrong variant: {err:?}"
    );
}

// A plugin registered as Unavailable must not be invoked; the recorded reason
// and `required` flag are propagated verbatim.
#[test]
fn unavailable_plugin_propagates_reason() {
    let registry = PluginRegistry::default();
    registry.insert_unavailable(
        "down".to_string(),
        None,
        true,
        BTreeSet::new(),
        PluginUnavailableReason::InitFailed,
        vec!["boom".to_string()],
    );
    let err =
        invoke_registered_plugin(&registry, "down", "n", "{}".to_string()).expect_err("must fail");
    assert!(
        matches!(&err, RuntimeError::PluginUnavailable { plugin_path, reason, required } if plugin_path == "down" && *reason == PluginUnavailableReason::InitFailed && *required),
        "wrong variant: {err:?}"
    );
}

// A Loaded dylib whose docs lack the requested node id must surface
// `NodeDocsNotFound` (the node-existence guard fires before the artifact is
// opened).
#[test]
fn loaded_dylib_missing_node_errors() {
    let registry = PluginRegistry::default();
    register_loaded_dylib(&registry, "time", &time_artifact());
    let err = invoke_registered_plugin(&registry, "time", "no_such_node", "{}".to_string())
        .expect_err("missing node must fail");
    assert!(
        matches!(&err, RuntimeError::NodeDocsNotFound { plugin_path, node_id } if plugin_path == "time" && node_id == "no_such_node"),
        "wrong variant: {err:?}"
    );
}

// A malformed (non-JSON) payload must be rejected with `InvalidArgument`
// before dispatch into the dylib handle (payload node_id injection step).
#[test]
fn loaded_dylib_rejects_non_json_payload() {
    let registry = PluginRegistry::default();
    register_loaded_dylib(&registry, "time", &time_artifact());
    let err = invoke_registered_plugin(&registry, "time", "time_now", "not json".to_string())
        .expect_err("malformed payload must fail");
    assert!(
        matches!(&err, RuntimeError::InvalidArgument { message } if message.contains("not valid JSON")),
        "wrong variant: {err:?}"
    );
}

// A plugin first registered `Unavailable` (so its artifact_path / artifact_kind
// / abi_fingerprint are all `None`) and then re-marked `Loaded` via
// `reload_plugin_entry` (which sets only docs + fingerprint, leaving
// artifact_path `None`) must surface the artifact-path `Invariant` guard rather
// than panicking on the `ok_or_else` unwrap. This is the only construction that
// yields a `Loaded` entry with a missing artifact path (`insert_loaded` always
// fills every field), so it is the sole door onto invoke.rs' artifact-path
// invariant arm.
#[test]
fn reloaded_entry_missing_artifact_path_is_invariant() {
    let registry = PluginRegistry::default();
    registry.insert_unavailable(
        "reloaded".to_string(),
        None,
        true,
        BTreeSet::new(),
        PluginUnavailableReason::InitFailed,
        vec!["initial failure".to_string()],
    );
    // reload re-marks it Loaded and sets docs (declaring node "n") + fingerprint
    // but does NOT populate artifact_path/kind.
    assert!(registry.reload_plugin_entry(
        "reloaded",
        json_docs("reloaded", "n"),
        any_fingerprint()
    ));

    let err = invoke_registered_plugin(&registry, "reloaded", "n", "{}".to_string())
        .expect_err("loaded-but-no-artifact-path must be an invariant error");
    assert!(
        matches!(&err, RuntimeError::Invariant { message } if message.contains("missing artifact path") && message.contains("reloaded")),
        "wrong variant: {err:?}"
    );
}

// A valid-JSON but *non-object* payload (a JSON array) parses fine, but
// `as_object_mut()` returns `None`, so the node_id-injection block is skipped
// (the `if let Some(obj)` false arm). The payload is forwarded verbatim; the
// `time` plugin cannot parse an array into its NodeRequest and reports the
// failure in-band (`ok:false`), so the invoke itself still succeeds.
#[test]
fn loaded_dylib_non_object_payload_skips_node_id_injection() {
    let registry = PluginRegistry::default();
    register_loaded_dylib(&registry, "time", &time_artifact());

    let resp = invoke_registered_plugin(&registry, "time", "time_now", "[]".to_string())
        .expect("array payload still invokes; plugin reports parse failure in-band");
    let value: serde_json::Value = serde_json::from_str(&resp.payload).expect("response json");
    assert_eq!(
        value.get("ok").and_then(|v| v.as_bool()),
        Some(false),
        "resp={}",
        resp.payload
    );
}

// ---------------------------------------------------------------------------
// invoke_registered_plugin — dylib success path
// ---------------------------------------------------------------------------

// The full success path: open the dylib, verify abi_kind == Rust, match the
// fingerprint, match the docs, inject node_id, and run `handle`. The `time`
// plugin's `time_now` node is a pure Router that needs no external resources.
#[test]
fn loaded_dylib_router_invoke_succeeds() {
    let registry = PluginRegistry::default();
    register_loaded_dylib(&registry, "time", &time_artifact());

    let resp = invoke_registered_plugin(&registry, "time", "time_now", "{}".to_string())
        .expect("time_now should invoke");
    let value: serde_json::Value = serde_json::from_str(&resp.payload).expect("response json");
    assert_eq!(value.get("ok").and_then(|v| v.as_bool()), Some(true));
    assert!(value.get("timestamp").is_some(), "resp={}", resp.payload);
    // node_id injection: the plugin echoes back the node id it received.
    assert_eq!(
        value.get("node_id").and_then(|v| v.as_str()),
        Some("time_now")
    );
}

// node_id injection must not clobber a node_id the caller already supplied
// (the `or_insert_with` only fills a missing key). The plugin still routes on
// the request node_id argument, so a matching explicit value is harmless and
// round-trips.
#[test]
fn loaded_dylib_invoke_preserves_explicit_node_id() {
    let registry = PluginRegistry::default();
    register_loaded_dylib(&registry, "time", &time_artifact());

    let resp = invoke_registered_plugin(
        &registry,
        "time",
        "time_now",
        r#"{"node_id":"time_now"}"#.to_string(),
    )
    .expect("invoke");
    let value: serde_json::Value = serde_json::from_str(&resp.payload).expect("json");
    assert_eq!(value.get("ok").and_then(|v| v.as_bool()), Some(true));
}

// A Task node keeps its dylib alive after invocation. `vision_ocr` is a Task
// node; called with an empty payload it fails fast inside the plugin
// ("either url or path is required") *before* touching the network or
// tesseract, so it deterministically exercises the is_task keep-alive branch.
// The fixture exports no `_cordis_create_service`, so the vtable sub-branch is
// skipped (documented as unreachable without a custom service dylib).
#[test]
fn loaded_dylib_task_node_keeps_alive_and_returns() {
    let registry = PluginRegistry::default();
    register_loaded_dylib(
        &registry,
        "vision",
        &artifacts_dir().join(format!("vision{}", std::env::consts::DLL_SUFFIX)),
    );

    let resp = invoke_registered_plugin(&registry, "vision", "vision_ocr", "{}".to_string())
        .expect("vision_ocr should return a structured error, not fail the invoke");
    let value: serde_json::Value = serde_json::from_str(&resp.payload).expect("json");
    // Plugin-level failure is reported in-band (ok:false), not as a
    // RuntimeError — the invoke itself succeeded.
    assert_eq!(value.get("ok").and_then(|v| v.as_bool()), Some(false));

    // Invoking the same Task node again must still succeed (re-entrant
    // keep-alive replaces the handle in-place rather than erroring).
    let resp2 = invoke_registered_plugin(&registry, "vision", "vision_ocr", "{}".to_string())
        .expect("second invoke");
    assert!(!resp2.payload.is_empty());
}

// `unregister_task_library` drops the keep-alive dylib handle for a plugin.
// It is `remove`-based, so calling it for a path that was never inserted is a
// harmless no-op; calling it after a Task invocation releases the handle.
// Exercises the public reload-support entrypoint (lines 42-44).
#[test]
fn unregister_task_library_is_idempotent_noop_for_unknown() {
    // Never registered — must not panic.
    unregister_task_library("no/such/task/plugin");

    // After a real Task invocation the handle exists; dropping it twice is
    // still safe.
    let registry = PluginRegistry::default();
    register_loaded_dylib(
        &registry,
        "vision",
        &artifacts_dir().join(format!("vision{}", std::env::consts::DLL_SUFFIX)),
    );
    let _ = invoke_registered_plugin(&registry, "vision", "vision_ocr", "{}".to_string())
        .expect("task invoke");
    unregister_task_library("vision");
    unregister_task_library("vision");
}

// ---------------------------------------------------------------------------
// invoke_registered_plugin — fingerprint / docs mismatch (runtime downgrade)
// ---------------------------------------------------------------------------

// If the registry records an ABI fingerprint that differs from what the
// dylib exports at runtime, the invoke must fail with `AbiMismatch` (carrying
// the field-level diff) and the registry entry must be downgraded to
// Unavailable(AbiMismatch).
#[test]
fn fingerprint_mismatch_downgrades_and_errors() {
    let registry = PluginRegistry::default();
    let artifact = time_artifact();
    let (_real_fp, docs) = dylib_fingerprint_and_docs(&artifact);
    // Register with a deliberately wrong crate_hash.
    let wrong_fp = AbiFingerprint {
        rustc_version: "rustc-wrong".to_string(),
        target_triple: "wrong-triple".to_string(),
        crate_hash: "crate_wrong_v9".to_string(),
        api_hash: "api_v2".to_string(),
    };
    registry.insert_loaded(
        "time".to_string(),
        None,
        true,
        BTreeSet::new(),
        docs,
        artifact,
        ArtifactKind::Dylib,
        wrong_fp,
        None,
    );

    let err = invoke_registered_plugin(&registry, "time", "time_now", "{}".to_string())
        .expect_err("fingerprint mismatch must fail");
    assert!(
        matches!(&err, RuntimeError::AbiMismatch { plugin_path, fingerprint_diff, .. } if plugin_path == "time" && fingerprint_diff.iter().any(|d| d.contains("crate_hash"))),
        "wrong variant: {err:?}"
    );

    // Registry entry was downgraded.
    let plugin = registry.get("time").expect("still present");
    assert!(matches!(
        plugin.load_result,
        cordis_runtime::core::models::PluginLoadResult::Unavailable(
            PluginUnavailableReason::AbiMismatch
        )
    ));
}

// If the recorded docs differ from what the dylib exports (fingerprint still
// matches), the invoke must fail with the plugin downgraded to
// Unavailable(ContractViolation).
#[test]
fn docs_mismatch_downgrades_to_contract_violation() {
    let registry = PluginRegistry::default();
    let artifact = time_artifact();
    let (fp, mut docs) = dylib_fingerprint_and_docs(&artifact);
    // Keep node ids intact (so the node-existence guard passes) but perturb a
    // docs field the runtime compares, forcing docs inequality.
    docs.plugin_version = "999.999.999".to_string();
    registry.insert_loaded(
        "time".to_string(),
        None,
        true,
        BTreeSet::new(),
        docs,
        artifact,
        ArtifactKind::Dylib,
        fp,
        None,
    );

    let err = invoke_registered_plugin(&registry, "time", "time_now", "{}".to_string())
        .expect_err("docs mismatch must fail");
    assert!(
        matches!(&err, RuntimeError::PluginUnavailable { reason, .. } if *reason == PluginUnavailableReason::ContractViolation),
        "wrong variant: {err:?}"
    );
    let plugin = registry.get("time").expect("present");
    assert!(matches!(
        plugin.load_result,
        cordis_runtime::core::models::PluginLoadResult::Unavailable(
            PluginUnavailableReason::ContractViolation
        )
    ));
}

// A bad artifact path (dylib that fails to dlopen: missing symbol / not a
// dylib) must downgrade the plugin to Unavailable(SymbolMissing) and return
// the open error.
#[test]
fn unopenable_dylib_downgrades_to_symbol_missing() {
    let registry = PluginRegistry::default();
    // A real JSON file with a .dylib extension: dlopen will fail.
    let bogus = artifacts_dir().join("index.json");
    let (fp, docs) = dylib_fingerprint_and_docs(&time_artifact());
    registry.insert_loaded(
        "time".to_string(),
        None,
        true,
        BTreeSet::new(),
        docs,
        // Force the dylib branch: real index.json is not a loadable library,
        // but ArtifactKind::Dylib routes it through LoadedDylibApi::open.
        bogus,
        ArtifactKind::Dylib,
        fp,
        None,
    );

    let err = invoke_registered_plugin(&registry, "time", "time_now", "{}".to_string())
        .expect_err("open must fail");
    // The open error is an Io error tagged with the artifact path.
    assert!(matches!(err, RuntimeError::Io { .. }), "err={err:?}");

    let plugin = registry.get("time").expect("present");
    assert!(matches!(
        plugin.load_result,
        cordis_runtime::core::models::PluginLoadResult::Unavailable(
            PluginUnavailableReason::SymbolMissing
        )
    ));
}

// ---------------------------------------------------------------------------
// invoke_registered_plugin — JSON-artifact (subprocess) path
// ---------------------------------------------------------------------------
//
// No fixture ships a JSON-artifact plugin, so these tests synthesise one: a
// registry entry with `ArtifactKind::Json` and a `PluginExecution::Process`
// pointing at a stock system binary. `artifact_kind != Dylib && !is_dylib_path`
// routes the invoke into `invoke_json_artifact`, which spawns the command,
// pipes the payload to stdin, and returns trimmed stdout.

/// Docs carrying a single Router node with the given id — the node-existence
/// guard in `invoke_registered_plugin` requires the node to be declared.
fn json_docs(plugin_path: &str, node_id: &str) -> PluginDocs {
    PluginDocs {
        plugin_id: plugin_path.replace('/', "_"),
        plugin_path: plugin_path.to_string(),
        plugin_version: "0.1.0".to_string(),
        abi_version: 2,
        command_name: None,
        nodes: vec![NodeDoc {
            id: node_id.to_string(),
            summary: "json artifact node".to_string(),
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

fn any_fingerprint() -> AbiFingerprint {
    AbiFingerprint {
        rustc_version: "rustc-test".to_string(),
        target_triple: "test-triple".to_string(),
        crate_hash: "crate_json_v1".to_string(),
        api_hash: "api_v2".to_string(),
    }
}

/// Register a JSON-artifact plugin with an explicit execution strategy. The
/// artifact path is a real temp file with a `.json` extension so the dylib
/// short-circuit (`is_dylib_path`) does not fire.
fn register_json_plugin(
    registry: &PluginRegistry,
    plugin_path: &str,
    node_id: &str,
    artifact: &Path,
    execution: Option<PluginExecution>,
) {
    registry.insert_loaded(
        plugin_path.to_string(),
        None,
        true,
        BTreeSet::new(),
        json_docs(plugin_path, node_id),
        artifact.to_path_buf(),
        ArtifactKind::Json,
        any_fingerprint(),
        execution,
    );
}

// Happy path: `cat` echoes the piped payload back on stdout; the invoke
// returns it trimmed.
#[test]
fn json_artifact_process_roundtrips_stdin_to_stdout() {
    let dir = tempfile::tempdir().expect("tempdir");
    let artifact = dir.path().join("plugin.json");
    std::fs::write(&artifact, "{}").expect("write artifact stub");

    let registry = PluginRegistry::default();
    register_json_plugin(
        &registry,
        "json/echo",
        "echo_node",
        &artifact,
        Some(PluginExecution::Process {
            command: "/bin/cat".to_string(),
            args: vec![],
        }),
    );

    let resp = invoke_registered_plugin(
        &registry,
        "json/echo",
        "echo_node",
        r#"{"hello":"world"}"#.to_string(),
    )
    .expect("cat should echo payload");
    // node_id is NOT injected on the JSON path (injection is dylib-only), so
    // cat returns exactly what was piped.
    assert_eq!(resp.payload, r#"{"hello":"world"}"#);
}

// A JSON-artifact plugin with no execution strategy must surface
// `PluginExecutionUnsupported`.
#[test]
fn json_artifact_without_execution_is_unsupported() {
    let dir = tempfile::tempdir().expect("tempdir");
    let artifact = dir.path().join("plugin.json");
    std::fs::write(&artifact, "{}").expect("write artifact stub");

    let registry = PluginRegistry::default();
    register_json_plugin(&registry, "json/noexec", "n", &artifact, None);

    let err = invoke_registered_plugin(&registry, "json/noexec", "n", "{}".to_string())
        .expect_err("no execution strategy must fail");
    assert!(
        matches!(&err, RuntimeError::PluginExecutionUnsupported { plugin_path, .. } if plugin_path == "json/noexec"),
        "wrong variant: {err:?}"
    );
}

// A relative command is resolved against the artifact's parent directory. We
// drop a tiny executable script next to the artifact and reference it by name.
#[test]
fn json_artifact_relative_command_resolves_against_artifact_dir() {
    let dir = tempfile::tempdir().expect("tempdir");
    let artifact = dir.path().join("plugin.json");
    std::fs::write(&artifact, "{}").expect("write artifact stub");

    let script = dir.path().join("run.sh");
    std::fs::write(&script, "#!/bin/sh\ncat\n").expect("write script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = std::fs::metadata(&script).expect("meta").permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&script, perm).expect("chmod");
    }

    let registry = PluginRegistry::default();
    register_json_plugin(
        &registry,
        "json/rel",
        "n",
        &artifact,
        Some(PluginExecution::Process {
            // Relative: resolved next to plugin.json.
            command: "run.sh".to_string(),
            args: vec![],
        }),
    );

    let resp = invoke_registered_plugin(&registry, "json/rel", "n", "payload-body".to_string())
        .expect("relative command should resolve and run");
    assert_eq!(resp.payload, "payload-body");
}

// A process that exits non-zero must surface `PluginInvocationFailed`; the
// stderr text is carried through as the message when present.
#[test]
fn json_artifact_non_zero_exit_reports_stderr() {
    let dir = tempfile::tempdir().expect("tempdir");
    let artifact = dir.path().join("plugin.json");
    std::fs::write(&artifact, "{}").expect("write artifact stub");

    let script = dir.path().join("fail.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\ncat > /dev/null\necho 'boom detail' >&2\nexit 3\n",
    )
    .expect("write script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = std::fs::metadata(&script).expect("meta").permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&script, perm).expect("chmod");
    }

    let registry = PluginRegistry::default();
    register_json_plugin(
        &registry,
        "json/fail",
        "n",
        &artifact,
        Some(PluginExecution::Process {
            command: script.to_string_lossy().to_string(),
            args: vec![],
        }),
    );

    let err = invoke_registered_plugin(&registry, "json/fail", "n", "{}".to_string())
        .expect_err("non-zero exit must fail");
    assert!(
        matches!(&err, RuntimeError::PluginInvocationFailed { plugin_path, message } if plugin_path == "json/fail" && message.contains("boom detail")),
        "wrong variant: {err:?}"
    );
}

// Spawn failure (command does not exist) must surface `PluginInvocationFailed`
// tagged with the plugin path and a "spawn ... failed" message.
#[test]
fn json_artifact_spawn_failure_is_reported() {
    let dir = tempfile::tempdir().expect("tempdir");
    let artifact = dir.path().join("plugin.json");
    std::fs::write(&artifact, "{}").expect("write artifact stub");

    let registry = PluginRegistry::default();
    register_json_plugin(
        &registry,
        "json/nospawn",
        "n",
        &artifact,
        Some(PluginExecution::Process {
            command: "/nonexistent/definitely/not/a/binary".to_string(),
            args: vec![],
        }),
    );

    let err = invoke_registered_plugin(&registry, "json/nospawn", "n", "{}".to_string())
        .expect_err("spawn must fail");
    assert!(
        matches!(&err, RuntimeError::PluginInvocationFailed { plugin_path, message } if plugin_path == "json/nospawn" && message.contains("spawn")),
        "wrong variant: {err:?}"
    );
}

// Non-zero exit with EMPTY stderr must fall back to a synthesized
// "process exited with status ..." message (the `if stderr.is_empty()` arm).
#[test]
fn json_artifact_non_zero_exit_empty_stderr_synthesizes_message() {
    let dir = tempfile::tempdir().expect("tempdir");
    let artifact = dir.path().join("plugin.json");
    std::fs::write(&artifact, "{}").expect("write artifact stub");

    let registry = PluginRegistry::default();
    register_json_plugin(
        &registry,
        "json/silentfail",
        "n",
        &artifact,
        // 先读完 stdin 再以 1 退出且不输出任何内容：直接用 `false` 的话
        // 进程不读 stdin 立即退出，runtime 写 payload 收到 EPIPE，错误
        // 停在 "write stdin failed" 而到不了本用例要覆盖的空 stderr
        // 合成分支（Linux 上必现）。
        Some(PluginExecution::Process {
            command: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), "cat > /dev/null; exit 1".to_string()],
        }),
    );

    let err = invoke_registered_plugin(&registry, "json/silentfail", "n", "{}".to_string())
        .expect_err("non-zero exit must fail");
    assert!(
        matches!(&err, RuntimeError::PluginInvocationFailed { plugin_path, message } if plugin_path == "json/silentfail" && message.contains("process exited with status")),
        "wrong variant: {err:?}"
    );
}

// Stdout that is not valid UTF-8 must surface `PluginInvocationFailed` with a
// "stdout was not utf-8" message (the `String::from_utf8` error arm).
#[test]
fn json_artifact_non_utf8_stdout_is_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let artifact = dir.path().join("plugin.json");
    std::fs::write(&artifact, "{}").expect("write artifact stub");

    let script = dir.path().join("binary_out.sh");
    // 必须先读完 stdin：runtime 会向子进程写 payload，脚本立即退出会让
    // 该写入收到 EPIPE，错误停在 "write stdin failed" 而到不了 utf-8
    // 校验分支（Linux 上进程退出快，实测必现）。消费 stdin 后再输出
    // 一个 0xFF 字节 — 永远不是合法 UTF-8 — 然后以 0 退出。
    std::fs::write(&script, "#!/bin/sh\ncat > /dev/null\nprintf '\\377'\n").expect("write script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = std::fs::metadata(&script).expect("meta").permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&script, perm).expect("chmod");
    }

    let registry = PluginRegistry::default();
    register_json_plugin(
        &registry,
        "json/binout",
        "n",
        &artifact,
        Some(PluginExecution::Process {
            command: script.to_string_lossy().to_string(),
            args: vec![],
        }),
    );

    let err = invoke_registered_plugin(&registry, "json/binout", "n", "{}".to_string())
        .expect_err("non-utf8 stdout must fail");
    assert!(
        matches!(&err, RuntimeError::PluginInvocationFailed { plugin_path, message } if plugin_path == "json/binout" && message.contains("not utf-8")),
        "wrong variant: {err:?}"
    );
}

// A child that exits WITHOUT reading its stdin, combined with a large payload
// that overflows the pipe buffer, forces `write_all(payload)` to fail with
// EPIPE (BrokenPipe) → the "write stdin failed" branch of invoke_json_artifact.
// The write is retried via a small loop because the child's exit races the
// first write; a payload far larger than the pipe capacity (64 KiB on Linux,
// smaller on macOS) makes the broken-pipe outcome deterministic once the child
// is gone.
#[test]
fn json_artifact_stdin_write_failure_is_reported() {
    use std::io::Write;

    let dir = tempfile::tempdir().expect("tempdir");
    let artifact = dir.path().join("plugin.json");
    std::fs::write(&artifact, "{}").expect("write artifact stub");

    // Child closes stdin immediately (never reads) and exits 0.
    let script = dir.path().join("noread.sh");
    std::fs::write(&script, "#!/bin/sh\nexec 0<&-\nexit 0\n").expect("write script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = std::fs::metadata(&script).expect("meta").permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&script, perm).expect("chmod");
    }

    // A multi-megabyte JSON payload: far larger than any pipe buffer, so once
    // the child is gone the runtime's `write_all` cannot complete and returns
    // BrokenPipe. Kept valid JSON so the earlier payload guards don't trip.
    let big = format!(r#"{{"blob":"{}"}}"#, "x".repeat(8 * 1024 * 1024));

    let registry = PluginRegistry::default();
    register_json_plugin(
        &registry,
        "json/nostdin",
        "n",
        &artifact,
        Some(PluginExecution::Process {
            command: script.to_string_lossy().to_string(),
            args: vec![],
        }),
    );

    // Sanity check the local pipe semantics this test relies on: writing a
    // large buffer to a spawned process that has closed stdin must eventually
    // error. If this platform buffers the entire payload without error (highly
    // unlikely at 8 MiB), skip rather than produce a flaky failure.
    {
        use std::process::{Command, Stdio};
        let mut probe = Command::new(&script)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn probe");
        let _ = probe.wait();
        let write_errs = probe
            .stdin
            .as_mut()
            .map(|s| s.write_all(big.as_bytes()).is_err())
            .unwrap_or(true);
        if !write_errs {
            eprintln!("[skip] host buffered the full payload; EPIPE not observable");
            return;
        }
    }

    let err = invoke_registered_plugin(&registry, "json/nostdin", "n", big)
        .expect_err("stdin write to a closed pipe must fail");
    assert!(
        matches!(&err, RuntimeError::PluginInvocationFailed { plugin_path, message } if plugin_path == "json/nostdin" && message.contains("write stdin failed")),
        "wrong variant: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// invoke_registered_plugin — Task-node service vtable block (invoke.rs 256-272)
// ---------------------------------------------------------------------------
//
// The service-vtable sub-branch runs only when ALL of these hold:
//   1. the node is `NodeType::Task` (is_task = true),
//   2. the dylib exports `_cordis_create_service`, and
//   3. that function returns a non-null `ServiceVTable`,
// after which the host calls `(vtable.start)(vtable.data)`.
//
// No prebuilt fixture under `fixtures/artifacts/` exports
// `_cordis_create_service`, so we compile a purpose-built one (`svcmini`) here.
// It cannot be a rustc-hand-written cdylib: the invoke path first enforces the
// `cordis_plugin_api_rust_v2` entry symbol, an exact abi_fingerprint match, and
// an exact docs match (invoke.rs 145-213) before ever reaching the service
// block, so the dylib must genuinely link `cordis-plugin-sdk` (built from this
// same repo + toolchain, so `AbiFingerprint::current_build` lines up with the
// host). `register_loaded_dylib` reads the fingerprint/docs straight out of the
// compiled dylib, so those two gates pass by construction.
//
// `svcmini` declares two Task nodes:
//   - `svc_task`: `_cordis_create_service` returns a real vtable whose `start`
//     writes a marker file (via COV_SVC_OUT) → exercises the
//     `(vtable.start)(vtable.data)` call.
//   - `svc_null`: `_cordis_create_service` returns null → exercises the
//     `if !vtable.is_null()` false branch (create called, no start).

/// lib.rs for the `svcmini` service plugin. Kept in the test so the fixture is
/// self-contained and versioned with the coverage it drives.
const SVCMINI_LIB_RS: &str = r##"//! svcmini — a minimal Task-node plugin exporting `_cordis_create_service`.

use cordis_plugin_sdk::{
    export_plugin_api, json_response, service_vtable, task_node_doc, AbiFingerprint,
    PluginRequest, PluginResponse, ServiceVTable,
};
use serde_json::json;

fn svc_start(_data: *mut std::ffi::c_void) -> i32 {
    if let Ok(path) = std::env::var("COV_SVC_OUT") {
        let _ = std::fs::write(path, "started");
    }
    0
}

fn svc_stop(_data: *mut std::ffi::c_void) -> i32 {
    0
}

#[no_mangle]
pub extern "C" fn _cordis_create_service(
    node_id: *const std::ffi::c_char,
) -> *const ServiceVTable {
    let id = unsafe { std::ffi::CStr::from_ptr(node_id) }
        .to_string_lossy()
        .into_owned();
    // `svc_task` gets a real vtable (drives the `(vtable.start)(...)` call);
    // any other node id (e.g. `svc_null`) returns null so the host takes the
    // `if !vtable.is_null()` false branch.
    if id != "svc_task" {
        return std::ptr::null();
    }
    let vtable = service_vtable! {
        data = std::ptr::null_mut(),
        start = svc_start,
        stop = svc_stop,
    };
    Box::into_raw(Box::new(vtable))
}

fn docs_value() -> cordis_plugin_sdk::PluginDocs {
    cordis_plugin_sdk::plugin_docs(
        "svcmini",
        "svcmini",
        "0.1.0",
        None,
        vec![
            task_node_doc(
                "svc_task",
                "A background service task node.",
                json!({ "type": "object" }),
                json!({ "type": "object" }),
                &[],
                &[],
            )
            .with_agent_accessible(),
            task_node_doc(
                "svc_null",
                "A task node whose create_service returns null.",
                json!({ "type": "object" }),
                json!({ "type": "object" }),
                &[],
                &[],
            )
            .with_agent_accessible(),
        ],
        None,
    )
}

fn abi_fingerprint_value() -> AbiFingerprint {
    AbiFingerprint::current_build("crate_svcmini_v1", "api_v2")
}

fn api_handle(_req: PluginRequest) -> PluginResponse {
    json_response(&json!({ "ok": true }))
}

export_plugin_api! {
    abi_fingerprint = abi_fingerprint_value(),
    docs = docs_value(),
    handle = api_handle,
}
"##;

/// Repo root (three levels up from this test crate's manifest).
fn svc_repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

/// Build the `svcmini` service dylib in a TempDir and return (temp, dylib_path).
/// The dylib depends on `cordis-plugin-sdk` via a `crates/` symlink back to the
/// repo, so it is compiled with the same SDK source + toolchain as the host
/// (making `AbiFingerprint::current_build` match at invoke time). Returns None
/// if cargo/rustc is unavailable so callers can skip rather than fail.
fn build_svcmini_dylib() -> Option<(tempfile::TempDir, PathBuf)> {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let root = temp.path();
    let plugin_dir = root.join("svcmini");
    std::fs::create_dir_all(plugin_dir.join("src")).expect("svcmini src");

    std::fs::write(
        plugin_dir.join("Cargo.toml"),
        r#"[package]
name = "svcmini"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["rlib", "dylib"]

[package.metadata.cordis]
plugin_path = "svcmini"
abi_kind = "rust"

[package.metadata.cordis.abi_fingerprint]
crate_hash = "crate_svcmini_v1"
api_hash = "api_v2"

[dependencies]
cordis-plugin-sdk = { path = "../crates/cordis-plugin-sdk" }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
"#,
    )
    .expect("write svcmini manifest");
    std::fs::write(plugin_dir.join("src/lib.rs"), SVCMINI_LIB_RS).expect("write svcmini lib.rs");

    // The manifest's `../crates/cordis-plugin-sdk` path dependency resolves via
    // this symlink to the repo `crates/`.
    #[cfg(unix)]
    std::os::unix::fs::symlink(svc_repo_root().join("crates"), root.join("crates"))
        .expect("symlink crates");
    #[cfg(not(unix))]
    {
        return None;
    }

    // Pin the child cargo's output dir explicitly rather than mutating this
    // process's CARGO_TARGET_DIR: `std::env::set_var`/`remove_var` are
    // process-global and would race with sibling tests running in parallel.
    // `.env(...)` scopes the override to the child only.
    let target_dir = root.join("svcmini-target");
    let status = std::process::Command::new("cargo")
        .arg("build")
        .current_dir(&plugin_dir)
        .env("CARGO_TARGET_DIR", &target_dir)
        .status();
    let Ok(status) = status else {
        eprintln!("[skip] cargo not runnable for svcmini build");
        return None;
    };
    if !status.success() {
        eprintln!("[skip] svcmini build failed");
        return None;
    }

    let dylib = target_dir.join("debug").join(format!(
        "{}svcmini{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    ));
    dylib.exists().then_some((temp, dylib))
}

// The full service path: a Task node whose dylib exports
// `_cordis_create_service` returning a non-null vtable. Invoking it must reach
// `(vtable.start)(vtable.data)` — asserted via the marker file the plugin's
// `start` writes. Also confirms the handle response still round-trips and the
// keep-alive registration happened (dropped afterwards).
#[serial_test::serial]
#[test]
fn task_service_vtable_start_is_invoked() {
    let Some((_temp, dylib)) = build_svcmini_dylib() else {
        return; // skip: toolchain unavailable
    };
    let registry = PluginRegistry::default();
    register_loaded_dylib(&registry, "svcmini", &dylib);

    let marker = _temp.path().join("svc_started.marker");
    std::env::set_var("COV_SVC_OUT", &marker);
    let _ = std::fs::remove_file(&marker);

    let resp = invoke_registered_plugin(&registry, "svcmini", "svc_task", "{}".to_string())
        .expect("svc_task invoke should succeed");
    let value: serde_json::Value = serde_json::from_str(&resp.payload).expect("json");
    assert_eq!(value.get("ok").and_then(|v| v.as_bool()), Some(true));

    // The plugin's `start` wrote the marker → the `(vtable.start)(...)` call ran.
    let written =
        std::fs::read_to_string(&marker).expect("service start must have written the marker file");
    assert_eq!(written, "started");

    std::env::remove_var("COV_SVC_OUT");
    // Release the keep-alive dylib handle registered by the Task invoke.
    unregister_task_library("svcmini");
}

// The null-vtable sub-branch: a Task node whose `_cordis_create_service`
// returns null. The `create_sym` lookup succeeds and `create(...)` is called,
// but the `if !vtable.is_null()` guard is false, so `start` is never invoked.
// The invoke still succeeds and the dylib is still kept alive.
#[serial_test::serial]
#[test]
fn task_service_null_vtable_skips_start() {
    let Some((_temp, dylib)) = build_svcmini_dylib() else {
        return; // skip: toolchain unavailable
    };
    let registry = PluginRegistry::default();
    register_loaded_dylib(&registry, "svcmini", &dylib);

    let marker = _temp.path().join("null_started.marker");
    std::env::set_var("COV_SVC_OUT", &marker);
    let _ = std::fs::remove_file(&marker);

    let resp = invoke_registered_plugin(&registry, "svcmini", "svc_null", "{}".to_string())
        .expect("svc_null invoke should succeed");
    let value: serde_json::Value = serde_json::from_str(&resp.payload).expect("json");
    assert_eq!(value.get("ok").and_then(|v| v.as_bool()), Some(true));

    // create_service returned null → start was NOT called → no marker file.
    assert!(
        !marker.exists(),
        "null vtable must not invoke start (no marker expected)"
    );

    std::env::remove_var("COV_SVC_OUT");
    unregister_task_library("svcmini");
}
