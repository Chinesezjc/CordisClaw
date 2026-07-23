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
    match err {
        RuntimeError::PluginNotRegistered { plugin_path } => assert_eq!(plugin_path, "ghost"),
        other => panic!("wrong variant: {other:?}"),
    }
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
    match err {
        RuntimeError::PluginUnavailable {
            plugin_path,
            reason,
            required,
        } => {
            assert_eq!(plugin_path, "down");
            assert_eq!(reason, PluginUnavailableReason::InitFailed);
            assert!(required);
        }
        other => panic!("wrong variant: {other:?}"),
    }
}

// A Loaded dylib whose docs lack the requested node id must surface
// `NodeDocsNotFound` (the node-existence guard fires before the artifact is
// opened).
#[test]
fn loaded_dylib_missing_node_errors() {
    let registry = PluginRegistry::default();
    register_loaded_dylib(&registry, "time", &artifacts_dir().join("time.dylib"));
    let err = invoke_registered_plugin(&registry, "time", "no_such_node", "{}".to_string())
        .expect_err("missing node must fail");
    match err {
        RuntimeError::NodeDocsNotFound {
            plugin_path,
            node_id,
        } => {
            assert_eq!(plugin_path, "time");
            assert_eq!(node_id, "no_such_node");
        }
        other => panic!("wrong variant: {other:?}"),
    }
}

// A malformed (non-JSON) payload must be rejected with `InvalidArgument`
// before dispatch into the dylib handle (payload node_id injection step).
#[test]
fn loaded_dylib_rejects_non_json_payload() {
    let registry = PluginRegistry::default();
    register_loaded_dylib(&registry, "time", &artifacts_dir().join("time.dylib"));
    let err = invoke_registered_plugin(&registry, "time", "time_now", "not json".to_string())
        .expect_err("malformed payload must fail");
    match err {
        RuntimeError::InvalidArgument { message } => {
            assert!(message.contains("not valid JSON"), "msg={message}");
        }
        other => panic!("wrong variant: {other:?}"),
    }
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
    register_loaded_dylib(&registry, "time", &artifacts_dir().join("time.dylib"));

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
    register_loaded_dylib(&registry, "time", &artifacts_dir().join("time.dylib"));

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
    register_loaded_dylib(&registry, "vision", &artifacts_dir().join("vision.dylib"));

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
    register_loaded_dylib(&registry, "vision", &artifacts_dir().join("vision.dylib"));
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
    let artifact = artifacts_dir().join("time.dylib");
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
    match err {
        RuntimeError::AbiMismatch {
            plugin_path,
            fingerprint_diff,
            ..
        } => {
            assert_eq!(plugin_path, "time");
            assert!(
                fingerprint_diff.iter().any(|d| d.contains("crate_hash")),
                "diff should mention crate_hash: {fingerprint_diff:?}"
            );
        }
        other => panic!("wrong variant: {other:?}"),
    }

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
    let artifact = artifacts_dir().join("time.dylib");
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
    match err {
        RuntimeError::PluginUnavailable { reason, .. } => {
            assert_eq!(reason, PluginUnavailableReason::ContractViolation);
        }
        other => panic!("wrong variant: {other:?}"),
    }
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
    let (fp, docs) = dylib_fingerprint_and_docs(&artifacts_dir().join("time.dylib"));
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
    match err {
        RuntimeError::PluginExecutionUnsupported { plugin_path, .. } => {
            assert_eq!(plugin_path, "json/noexec");
        }
        other => panic!("wrong variant: {other:?}"),
    }
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
    std::fs::write(&script, "#!/bin/sh\necho 'boom detail' >&2\nexit 3\n").expect("write script");
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
    match err {
        RuntimeError::PluginInvocationFailed {
            plugin_path,
            message,
        } => {
            assert_eq!(plugin_path, "json/fail");
            assert!(message.contains("boom detail"), "msg={message}");
        }
        other => panic!("wrong variant: {other:?}"),
    }
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
    match err {
        RuntimeError::PluginInvocationFailed {
            plugin_path,
            message,
        } => {
            assert_eq!(plugin_path, "json/nospawn");
            assert!(message.contains("spawn"), "msg={message}");
        }
        other => panic!("wrong variant: {other:?}"),
    }
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
        // `false` exits 1 with no output on any stream.
        Some(PluginExecution::Process {
            command: "/usr/bin/false".to_string(),
            args: vec![],
        }),
    );

    let err = invoke_registered_plugin(&registry, "json/silentfail", "n", "{}".to_string())
        .expect_err("non-zero exit must fail");
    match err {
        RuntimeError::PluginInvocationFailed {
            plugin_path,
            message,
        } => {
            assert_eq!(plugin_path, "json/silentfail");
            assert!(
                message.contains("process exited with status"),
                "msg={message}"
            );
        }
        other => panic!("wrong variant: {other:?}"),
    }
}

// Stdout that is not valid UTF-8 must surface `PluginInvocationFailed` with a
// "stdout was not utf-8" message (the `String::from_utf8` error arm).
#[test]
fn json_artifact_non_utf8_stdout_is_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let artifact = dir.path().join("plugin.json");
    std::fs::write(&artifact, "{}").expect("write artifact stub");

    let script = dir.path().join("binary_out.sh");
    // Emit a lone 0xFF byte — never valid UTF-8 — then exit 0.
    std::fs::write(&script, "#!/bin/sh\nprintf '\\377'\n").expect("write script");
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
    match err {
        RuntimeError::PluginInvocationFailed {
            plugin_path,
            message,
        } => {
            assert_eq!(plugin_path, "json/binout");
            assert!(message.contains("not utf-8"), "msg={message}");
        }
        other => panic!("wrong variant: {other:?}"),
    }
}
