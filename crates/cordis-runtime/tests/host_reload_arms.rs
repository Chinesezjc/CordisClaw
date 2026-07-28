//! Reload-chain arm coverage for `RuntimeHost`: the error / observation
//! branches inside `reload_subtree`, `reload_internal`, `reload_candidate` and
//! `notify_sessions_of_reload` that the existing `host_coverage*.rs` batches do
//! not reach.
//!
//! Two fixture styles are used, picked per test by what the code path needs:
//!
//! * **Hermetic JSON-artifact trees** (`json_fixture`) for everything driven by
//!   `reload_internal` / `reload_candidate` / the pre-dlopen half of
//!   `reload_subtree`. JSON artifacts skip `dlopen` at load time, so these boot
//!   in milliseconds on any host and let a test rewrite `index.json` and the
//!   artifact file to synthesize drift that a prebuilt dylib cannot express.
//! * **The real fixture artifacts** (`support::fixtures_root`, rebuilt for the
//!   current host by `ensure_fixture_artifacts`) for the one test that needs a
//!   *successful* `reload_subtree`, since Phase 1 really does `dlopen` the
//!   artifact.
//!
//! Kept in its own file so it does not contend with the other coverage batches
//! over one `OnceLock`-shared host.

use cordis_plugin_sdk::{AbiFingerprint, NodeDoc, NodeType, PluginDocs};
use cordis_runtime::agent::AgentTranscriptEntry;
use cordis_runtime::context::Service;
use cordis_runtime::core::error::RuntimeError;
use cordis_runtime::host::{AgentSessionKind, AgentStartOptions, ReloadAttemptStatus, RuntimeHost};
use serde_json::json;
use serial_test::serial;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::Mutex;
use tempfile::TempDir;

mod support;
use support::fixtures_root;

// ---------------------------------------------------------------------------
// Hermetic JSON-artifact fixture tree.
// ---------------------------------------------------------------------------

/// Docs for a single-node plugin. `summary` is the knob a test turns to make
/// two docs revisions of the same plugin compare unequal.
fn plugin_docs(plugin_path: &str, node_type: NodeType, summary: &str) -> PluginDocs {
    let node_id = format!("{}_entry", plugin_path.replace('/', "_"));
    PluginDocs {
        plugin_id: plugin_path.to_string(),
        plugin_path: plugin_path.to_string(),
        plugin_version: "0.1.0".to_string(),
        abi_version: 2,
        command_name: None,
        nodes: vec![NodeDoc {
            id: node_id.clone(),
            summary: summary.to_string(),
            input_schema: json!({ "type": "object" }),
            output_schema: json!({ "type": "object" }),
            side_effects: Vec::new(),
            failure_modes: Vec::new(),
            node_type,
            agent_accessible: false,
        }],
        system_hint: None,
    }
}

/// Write `<artifacts>/<plugin>.json` and return the index entry describing it.
/// Mirrors the `PluginArtifact` shape the loader cross-checks (plugin_path /
/// abi_fingerprint / docs must agree with the entry, and the recorded sha256
/// must match the bytes on disk).
fn write_json_plugin(artifacts: &Path, plugin_path: &str, docs: &PluginDocs) -> serde_json::Value {
    let abi = AbiFingerprint::current_build(format!("crate_{plugin_path}_v1"), "api_v2");
    let artifact_value = json!({
        "plugin_path": plugin_path,
        "abi_fingerprint": abi,
        "docs": docs,
        "exports": [],
        "execution": null,
    });
    let file_name = format!("{}.json", plugin_path.replace('/', "_"));
    let artifact_file = artifacts.join(&file_name);
    fs::write(
        &artifact_file,
        serde_json::to_vec_pretty(&artifact_value).expect("serialize artifact"),
    )
    .expect("write artifact");
    let sha256 = cordis_runtime::plugin::artifact::sha256_file(&artifact_file).expect("hash");

    json!({
        "plugin_path": plugin_path,
        "version": "0.1.0",
        "abi_fingerprint": abi,
        "artifact_path": file_name,
        "sha256": sha256,
        "built_at": "0",
        "parent": null,
        "required": true,
        "grants_from_parent": [],
        "docs": docs,
        "exports": [],
        "execution": null,
        "artifact_kind": "json",
        "build_fingerprint": format!("fp_{plugin_path}"),
    })
}

/// A fixtures tree of JSON-artifact plugins, one `Task` node each. Returns the
/// `TempDir` (kept alive by the caller) and the fixtures root to boot from.
fn json_fixture(plugin_paths: &[&str]) -> (TempDir, PathBuf) {
    let temp = TempDir::new().expect("tempdir");
    let fixtures = temp.path().join("fixtures");
    let artifacts = fixtures.join("artifacts");
    fs::create_dir_all(&artifacts).expect("create artifacts dir");

    let entries: Vec<serde_json::Value> = plugin_paths
        .iter()
        .map(|p| {
            let docs = plugin_docs(p, NodeType::Task, "revision one");
            write_json_plugin(&artifacts, p, &docs)
        })
        .collect();

    write_index(&artifacts, plugin_paths, entries);
    (temp, fixtures)
}

fn write_index(artifacts: &Path, topo_order: &[&str], entries: Vec<serde_json::Value>) {
    let index = json!({
        "schema_version": 2,
        "generated_at": "2026-07-24T00:00:00Z",
        "topo_order": topo_order,
        "entries": entries,
    });
    fs::write(
        artifacts.join("index.json"),
        serde_json::to_vec_pretty(&index).expect("serialize index"),
    )
    .expect("write index");
}

fn read_index(fixtures: &Path) -> serde_json::Value {
    let raw = fs::read_to_string(fixtures.join("artifacts/index.json")).expect("read index");
    serde_json::from_str(&raw).expect("parse index")
}

fn write_index_value(fixtures: &Path, index: &serde_json::Value) {
    fs::write(
        fixtures.join("artifacts/index.json"),
        serde_json::to_vec_pretty(index).expect("serialize index"),
    )
    .expect("write index");
}

/// A service whose `stop()` blocks until the test signals it. Used to force a
/// real stop-timeout so `ServiceRegistry::zombie_count()` becomes non-zero.
struct BlockingStopService {
    rx: Mutex<Receiver<()>>,
}

impl Service for BlockingStopService {
    fn start(&self) -> Result<(), String> {
        Ok(())
    }
    fn stop(&self) -> Result<(), String> {
        let _ = self
            .rx
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .recv();
        Ok(())
    }
}

/// A service that returns immediately from both lifecycle calls. Used purely as
/// an observable marker: whether the reload path called `stop_plugin_services`
/// for a plugin is visible as the registry's entry count dropping.
struct NoopService;

impl Service for NoopService {
    fn start(&self) -> Result<(), String> {
        Ok(())
    }
    fn stop(&self) -> Result<(), String> {
        Ok(())
    }
}

// ===========================================================================
// reload_subtree — target selection and the artifact-missing arm.
// ===========================================================================

/// `reload("")` trims to an empty prefix, which takes the `normalized
/// .is_empty()` side of the target filter: every registered plugin becomes a
/// target rather than none. Proven by the outcome differing from the
/// empty-target no-op: with JSON artifacts selected, Phase 1's `dlopen` of a
/// `.json` file fails, so the reload reports an error instead of the no-op
/// success an empty target list would produce.
#[test]
#[serial]
fn reload_empty_prefix_selects_every_plugin() {
    let (_temp, fixtures) = json_fixture(&["alpha", "beta"]);
    let host = RuntimeHost::boot(&fixtures).expect("boot on json fixture");
    let before = host.current_snapshot().snapshot_id().to_string();

    // Control: the same host, asked for a prefix that matches nothing, takes
    // the empty-target branch and succeeds as a no-op with an unchanged id.
    let noop = host
        .reload("no_such_prefix")
        .expect("unmatched prefix is a no-op success");
    assert!(noop.changed_plugins.is_empty());
    assert_eq!(noop.to_snapshot_id, before);

    // Empty prefix: targets are non-empty, so control reaches Phase 1 and
    // fails on the JSON artifact.
    let err = host
        .reload("")
        .expect_err("an empty prefix selects real targets, which then fail Phase 1 dlopen");
    let is_io = matches!(&err, RuntimeError::Io { .. });
    assert!(is_io, "expected the Phase-1 dlopen Io failure, got {err:?}");
    let attempt = host.last_reload_attempt().expect("attempt recorded");
    assert_eq!(attempt.status, ReloadAttemptStatus::Failed);
}

/// A plugin that is live in the registry but absent from a *loadable*
/// `index.json` drives the `ok_or_else` arm in Phase 1: the target lookup misses
/// and `fail_reload` packages `PluginUnavailable { ArtifactMissing }` with a
/// recorded Failed attempt. Distinct from the existing "index file deleted"
/// tests, which fail earlier, at `load_artifact_index`.
#[test]
#[serial]
fn reload_subtree_target_absent_from_index_reports_artifact_missing() {
    let (_temp, fixtures) = json_fixture(&["alpha", "beta"]);
    let host = RuntimeHost::boot(&fixtures).expect("boot on json fixture");
    assert!(host
        .current_snapshot()
        .plugin_registry()
        .get("beta")
        .is_some());

    // Drop `beta` from the index while leaving the index itself valid and the
    // live snapshot untouched.
    let mut index = read_index(&fixtures);
    let entries = index
        .get_mut("entries")
        .and_then(|v| v.as_array_mut())
        .expect("entries array");
    entries.retain(|e| e.get("plugin_path").and_then(|v| v.as_str()) != Some("beta"));
    index["topo_order"] = json!(["alpha"]);
    write_index_value(&fixtures, &index);

    let err = host
        .reload("beta")
        .expect_err("a target with no index entry must abort the reload");
    let expected = RuntimeError::PluginUnavailable {
        plugin_path: "beta".to_string(),
        reason: cordis_runtime::core::models::PluginUnavailableReason::ArtifactMissing,
        required: false,
    };
    assert_eq!(err.to_string(), expected.to_string());

    let attempt = host.last_reload_attempt().expect("failed attempt recorded");
    assert_eq!(attempt.status, ReloadAttemptStatus::Failed);
    assert_eq!(attempt.to_snapshot_id, None);
    assert_eq!(attempt.failure_summary, Some(expected.to_string()));
}

// ===========================================================================
// reload_internal — the two service-stop loops around the snapshot swap.
// ===========================================================================

/// A plugin dropped from the index is gone from the next snapshot, so the
/// whole-tree reload stops its services before swapping. Observed through the
/// service registry emptying.
#[test]
#[serial]
fn reload_internal_stops_services_of_removed_plugin() {
    let (_temp, fixtures) = json_fixture(&["alpha", "beta"]);
    let host = RuntimeHost::boot(&fixtures).expect("boot on json fixture");
    host.service_registry
        .start_service("beta", "beta_entry", Box::new(NoopService))
        .expect("register a service for the plugin about to be removed");
    assert_eq!(host.service_registry.len(), 1);

    let mut index = read_index(&fixtures);
    let entries = index
        .get_mut("entries")
        .and_then(|v| v.as_array_mut())
        .expect("entries array");
    entries.retain(|e| e.get("plugin_path").and_then(|v| v.as_str()) != Some("beta"));
    index["topo_order"] = json!(["alpha"]);
    write_index_value(&fixtures, &index);

    let report = host.reload("/").expect("whole-tree reload should succeed");
    assert!(report.removed_plugins.iter().any(|p| p == "beta"));
    assert_eq!(
        host.service_registry.len(),
        0,
        "the removed plugin's service must be stopped by reload_internal"
    );
    assert!(host
        .current_snapshot()
        .plugin_registry()
        .get("beta")
        .is_none());
}

/// A plugin whose docs change between snapshots also has its services stopped,
/// because the new revision may declare different Task nodes. A JSON artifact
/// makes this expressible: the artifact file is the docs ground truth, so
/// rewriting it (plus the matching index entry) yields a genuinely different
/// docs revision instead of the loader's drift auto-heal converging back.
#[test]
#[serial]
fn reload_internal_stops_services_of_plugin_whose_docs_changed() {
    let (_temp, fixtures) = json_fixture(&["alpha"]);
    let host = RuntimeHost::boot(&fixtures).expect("boot on json fixture");
    let first_summary = plugin_summary(&host, "alpha");
    assert_eq!(first_summary, "revision one");

    host.service_registry
        .start_service("alpha", "alpha_entry", Box::new(NoopService))
        .expect("register a service for the plugin about to change");
    assert_eq!(host.service_registry.len(), 1);

    // Publish docs revision two, consistently, in both the artifact and index.
    let artifacts = fixtures.join("artifacts");
    let docs = plugin_docs("alpha", NodeType::Task, "revision two");
    let entry = write_json_plugin(&artifacts, "alpha", &docs);
    write_index(&artifacts, &["alpha"], vec![entry]);

    host.reload("/").expect("whole-tree reload should succeed");
    assert_eq!(plugin_summary(&host, "alpha"), "revision two");
    assert_eq!(
        host.service_registry.len(),
        0,
        "a docs change must stop the plugin's services so the new revision restarts them"
    );
}

fn plugin_summary(host: &RuntimeHost, plugin_path: &str) -> String {
    let snapshot = host.current_snapshot();
    let plugin = snapshot
        .plugin_registry()
        .get(plugin_path)
        .expect("plugin registered");
    let docs = plugin.docs.expect("loaded plugin carries docs");
    docs.nodes
        .first()
        .expect("single-node fixture")
        .summary
        .clone()
}

// ===========================================================================
// notify_sessions_of_reload — the per-session injection loop body.
// ===========================================================================

/// A reload that changes plugins injects a notice into every live agent
/// session. With no session started the loop body never runs (the existing
/// reload tests all take that shape), so this one starts a `RuntimeShell`
/// session first — `agent_start_with` only resolves the configured LLM profile
/// and inserts into the session map, it issues no request — then reloads and
/// reads the notice back out of the transcript.
#[test]
#[serial]
fn reload_injects_notice_into_live_agent_sessions() {
    let (_temp, fixtures) = json_fixture(&["alpha"]);
    let host = RuntimeHost::boot(&fixtures).expect("boot on json fixture");
    let handle = host
        .agent_start_with(AgentSessionKind::RuntimeShell, AgentStartOptions::default())
        .expect("a runtime-shell session starts without any LLM traffic");

    // Publish a new docs revision so the reload reports `alpha` as changed —
    // an unchanged reload returns early before the notify loop.
    let artifacts = fixtures.join("artifacts");
    let docs = plugin_docs("alpha", NodeType::Task, "revision two");
    let entry = write_json_plugin(&artifacts, "alpha", &docs);
    write_index(&artifacts, &["alpha"], vec![entry]);

    let report = host.reload("/").expect("whole-tree reload should succeed");
    assert!(
        report.changed_plugins.iter().any(|p| p == "alpha"),
        "the docs revision must register as a change, got {:?}",
        report.changed_plugins
    );

    let transcript = host
        .agent_transcript(&handle.session_id)
        .expect("session transcript readable");
    let notice = transcript.iter().find_map(|entry| match entry {
        AgentTranscriptEntry::User { content }
            if content.starts_with("[system] Plugin reloaded:") =>
        {
            Some(content.clone())
        }
        _ => None,
    });
    let notice = notice.expect("the reload notice must be injected into the live session");
    assert!(
        notice.contains("alpha"),
        "the notice must name the changed plugin, got: {notice}"
    );

    // The paired assistant acknowledgement is injected alongside it.
    let acked = transcript
        .iter()
        .any(|e| matches!(e, AgentTranscriptEntry::Assistant { content, .. } if content == "Acknowledged."));
    assert!(acked, "agent_inject records the acknowledgement half too");
}

// ===========================================================================
// reload_candidate — the Err arm of the non-diagnostics entry point.
// ===========================================================================

/// `reload_candidate()` (as opposed to `reload_candidate_with_diagnostics()`)
/// records the failed attempt, reports the issue, and returns the error. Driven
/// by removing the artifact index so candidate staging cannot build a snapshot.
#[test]
#[serial]
fn reload_candidate_error_arm_records_attempt_and_returns_err() {
    let (_temp, fixtures) = json_fixture(&["alpha"]);
    let host = RuntimeHost::boot(&fixtures).expect("boot on json fixture");
    fs::remove_file(fixtures.join("artifacts/index.json")).expect("remove index");

    let err = host
        .reload_candidate()
        .expect_err("candidate staging without an index must fail");
    assert!(!err.to_string().is_empty());
    assert!(
        host.candidate_snapshot().is_none(),
        "a failed staging must not leave a candidate behind"
    );
    let attempt = host
        .last_candidate_reload_attempt()
        .expect("failed candidate attempt recorded");
    assert_eq!(attempt.status, ReloadAttemptStatus::Failed);
    assert_eq!(attempt.to_snapshot_id, None);
    assert_eq!(attempt.failure_summary, Some(err.to_string()));
}

// ===========================================================================
// reload_subtree — the zombie-service observation arm (needs a *successful*
// subtree reload, hence the real dylib fixtures).
// ===========================================================================

/// After Phase 2, `reload_subtree` reports any services still parked on the
/// zombie list. A service whose `stop()` blocks past the registry's 5s deadline
/// becomes such a zombie; a subsequent successful reload of an unrelated
/// subtree then takes the `zombie_count > 0` arm.
///
/// The zombie is parked under its own plugin path, not under the reload target:
/// the target's services go through the *untimed* `stop_plugin_services` at the
/// top of `reload_subtree`, which would block the reload rather than time out.
#[test]
#[serial]
fn reload_subtree_reports_leftover_zombie_services() {
    let temp = setup_artifacts_only_copy();
    let host = RuntimeHost::boot(temp.path()).expect("host should boot from artifacts copy");

    let (tx, rx) = std::sync::mpsc::channel::<()>();
    host.service_registry
        .start_service(
            "blocked_stopper",
            "svc",
            Box::new(BlockingStopService { rx: Mutex::new(rx) }),
        )
        .expect("start the blocking service");
    // Blocks ~5s on the stuck stop, then parks it as a zombie.
    host.service_registry
        .stop_plugin_services_timed("blocked_stopper");
    assert_eq!(host.service_registry.zombie_count(), 1);

    // A successful subtree reload of an unrelated plugin now runs with a
    // non-empty zombie list and reports it.
    let report = host.reload("expr").expect("subtree reload should succeed");
    assert!(report.changed_plugins.iter().any(|p| p == "expr"));
    assert_eq!(
        host.service_registry.zombie_count(),
        1,
        "the reload only observes zombies; cleanup stays with kill_zombie_services"
    );

    // Unblock the parked stop thread and reap it so the temp dir can drop.
    tx.send(()).expect("unblock the stuck stop");
    let mut polls = 0;
    while polls < 100 && host.service_registry.zombie_count() > 0 {
        host.service_registry
            .kill_zombie_services("blocked_stopper");
        std::thread::sleep(std::time::Duration::from_millis(20));
        polls += 1;
    }
    assert_eq!(host.service_registry.zombie_count(), 0);
}

fn copy_dir_all(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).expect("create destination");
    for entry in fs::read_dir(src).expect("read dir") {
        let entry = entry.expect("dir entry");
        let ty = entry.file_type().expect("file type");
        if ty.is_dir() && (entry.file_name() == "target") {
            continue;
        }
        let target = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).expect("copy file");
        }
    }
}

/// Copy just `artifacts/` plus the top-level config files into a throwaway dir,
/// so a test may mutate them without touching the shared fixtures tree.
fn setup_artifacts_only_copy() -> TempDir {
    let temp = TempDir::new().expect("tempdir");
    let root = fixtures_root();
    copy_dir_all(&root.join("artifacts"), &temp.path().join("artifacts"));
    for name in ["notify_handlers.json", "startup_invoke.json"] {
        let src = root.join(name);
        if src.exists() {
            fs::copy(&src, temp.path().join(name)).expect("copy top-level fixture file");
        }
    }
    temp
}
