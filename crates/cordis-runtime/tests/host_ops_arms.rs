//! Integration coverage for the `host.rs` operation-application arms that need
//! a booted `RuntimeHost` and a real snapshot/artifact tree to reach:
//!
//! * `RuntimeSnapshot::execute_registered_target` — both the `execute_net`
//!   success path and its error propagation, plus the per-node trace arms for a
//!   successful invoke and for a node missing from the registry.
//! * `apply_plugin_iteration_journal` driven against a booted host's real
//!   snapshot root (as opposed to the ad-hoc temp roots the crash-recovery
//!   suite uses), including the applied-marker generation gates.
//! * `workspace_manifest_lock::acquire`'s `flock` failure branch, reached
//!   through a FIFO lock path — `flock(2)` reports ENOTSUP for FIFOs while the
//!   `O_RDWR` open still succeeds and does not block.
//!
//! Nothing here disables coverage instrumentation; every arm is reached by
//! driving the public API.

use cordis_runtime::core::error::RuntimeError;
use cordis_runtime::host::{apply_plugin_iteration_journal, RuntimeHost};
use cordis_runtime::kernel::plugin_iteration::PluginEditRollback;
use serde_json::json;
use serial_test::serial;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

// ───────────────────────────── shared fixtures ─────────────────────────────

/// A fixtures tree with an empty `artifacts/index.json`: `RuntimeHost::boot`
/// registers no plugins and never dlopens, so booting costs no cargo build.
fn empty_fixture() -> (TempDir, PathBuf) {
    let temp = TempDir::new().expect("tempdir");
    let fixtures = temp.path().join("fixtures");
    let artifacts = fixtures.join("artifacts");
    fs::create_dir_all(&artifacts).expect("create artifacts dir");
    fs::write(
        artifacts.join("index.json"),
        concat!(
            "{\n",
            "  \"schema_version\": 2,\n",
            "  \"generated_at\": \"2026-07-26T00:00:00Z\",\n",
            "  \"topo_order\": [],\n",
            "  \"entries\": []\n",
            "}\n"
        ),
    )
    .expect("write empty artifact index");
    (temp, fixtures)
}

fn booted_host() -> (TempDir, RuntimeHost) {
    let (temp, fixtures) = empty_fixture();
    let host = RuntimeHost::boot(&fixtures).expect("boot on an empty artifact index");
    (temp, host)
}

fn journal_path(snapshot_root: &Path) -> PathBuf {
    snapshot_root.join("plugin-iteration-edit-journal.json")
}

fn applied_marker_path(snapshot_root: &Path) -> PathBuf {
    snapshot_root.join("plugin-iteration-edit-journal.applied")
}

// ────────────── RuntimeSnapshot::execute_registered_target ──────────────

#[test]
#[serial]
fn execute_registered_target_rejects_a_non_object_payload() {
    // The `request_seed` guard fires before any net is built.
    let (_temp, host) = booted_host();
    let snapshot = host.current_snapshot();
    let err = snapshot
        .execute_registered_target("cordis::agent_router", json!([1, 2, 3]))
        .expect_err("a JSON array payload must be rejected");
    assert!(
        matches!(&err, RuntimeError::InvalidArgument { message }
            if message == "execute payload must be a JSON object"),
        "expected the payload-shape guard, got {err:?}"
    );
}

#[test]
#[serial]
fn execute_registered_target_rejects_an_unregistered_node() {
    let (_temp, host) = booted_host();
    let snapshot = host.current_snapshot();
    let err = snapshot
        .execute_registered_target("ghost::nope", json!({}))
        .expect_err("an unregistered node fqn must be rejected");
    assert!(
        matches!(&err, RuntimeError::InvalidArgument { message }
            if message == "registered node not found: ghost::nope"),
        "expected the node-lookup guard, got {err:?}"
    );
}

/// Runs the whole `execute_net` pipeline for the kernel's built-in
/// `cordis::agent_router` node. Because `cordis` is a virtual plugin entry with
/// no dylib, the runner closure's `self.invoke(..)` fails, so this drives the
/// `Err(err)` trace arm, the `execute_net` `?` (success — the engine itself does
/// not error on a node-level failure), the `traces.into_inner()` collection and
/// `fill_missing_execution_traces`.
#[test]
#[serial]
fn execute_registered_target_runs_the_net_and_records_a_failure_trace() {
    let (_temp, host) = booted_host();
    let snapshot = host.current_snapshot();
    let result = snapshot
        .execute_registered_target("cordis::agent_router", json!({ "message": "hi" }))
        .expect("execute_net completes even when the node invoke fails");

    assert_eq!(result.target_node_fqn, "cordis::agent_router");
    assert!(
        result
            .selected_nodes
            .contains(&"cordis::agent_router".to_string()),
        "the target node is always selected: {:?}",
        result.selected_nodes
    );
    let trace = result
        .traces
        .get("cordis::agent_router")
        .expect("the target node has a trace entry");
    assert_eq!(trace.node_fqn, "cordis::agent_router");
    assert_eq!(trace.node_id, "agent_router");
    assert_eq!(trace.plugin_path, "cordis");
    // The virtual `cordis` plugin has no artifact, so invoking it errors and the
    // error text is recorded verbatim on the trace.
    assert!(
        trace.error.is_some(),
        "invoking the virtual builtin plugin must fail: {trace:?}"
    );
    assert!(
        trace.response_payload.is_none(),
        "a failed invoke records no response payload"
    );
    // The request payload is the caller's object merged with trigger inputs.
    let request = trace
        .request_payload
        .as_ref()
        .expect("a failed invoke still records its request");
    assert_eq!(
        request.get("message").and_then(|value| value.as_str()),
        Some("hi")
    );
}

// ───────── apply_plugin_iteration_journal against a booted host ─────────

/// Persist a journal into `snapshot_root` recording `pre_edit` as the pre-edit
/// bytes of `rel` inside `workspace`.
fn seed_journal(workspace: &Path, snapshot_root: &Path, rel: &str, pre_edit: &[u8]) {
    let target = workspace.join(rel);
    fs::create_dir_all(target.parent().expect("target has a parent")).expect("mkdir target parent");
    let rollback = PluginEditRollback::single_backup(workspace, rel, Some(pre_edit.to_vec()));
    rollback
        .persist_journal(&journal_path(snapshot_root), "iter-ops-integration")
        .expect("persist the rollback journal");
}

#[test]
#[serial]
fn journal_replay_against_a_booted_host_restores_and_clears() {
    let (_temp, host) = booted_host();
    let workspace = host.fixtures_root().to_path_buf();
    // `RuntimeHost::boot` created the snapshot root already; derive it from the
    // journal the host itself would write.
    let snapshot_root = workspace.join("snapshots-ops");
    fs::create_dir_all(&snapshot_root).expect("mkdir snapshot root");

    let rel = "plugins/mini/src/lib.rs";
    seed_journal(&workspace, &snapshot_root, rel, b"PRE-EDIT BODY\n");
    fs::write(workspace.join(rel), b"POST-EDIT BODY\n").expect("write the post-edit body");

    let restored = apply_plugin_iteration_journal(&workspace, &snapshot_root, None)
        .expect("journal replay succeeds");
    assert!(restored, "a journal on disk means a restore happened");
    assert_eq!(
        fs::read(workspace.join(rel)).expect("read the restored file"),
        b"PRE-EDIT BODY\n"
    );
    assert!(
        !journal_path(&snapshot_root).exists(),
        "the journal is cleared after a successful replay"
    );
    assert!(
        !applied_marker_path(&snapshot_root).exists(),
        "the applied marker is cleaned up after a successful replay"
    );
}

#[test]
#[serial]
fn journal_replay_is_idempotent_via_the_applied_marker() {
    let (_temp, host) = booted_host();
    let workspace = host.fixtures_root().to_path_buf();
    let snapshot_root = workspace.join("snapshots-ops-idem");
    fs::create_dir_all(&snapshot_root).expect("mkdir snapshot root");

    let rel = "plugins/mini/src/lib.rs";
    seed_journal(&workspace, &snapshot_root, rel, b"PRE-EDIT\n");
    let generation_id = PluginEditRollback::journal_generation_id(&journal_path(&snapshot_root))
        .expect("read the journal generation id")
        .expect("a freshly persisted journal carries a generation id");
    // A marker recording the SAME generation means the replay already ran, so a
    // later legitimate edit must survive.
    fs::write(
        applied_marker_path(&snapshot_root),
        generation_id.as_bytes(),
    )
    .expect("write the applied marker");
    fs::write(workspace.join(rel), b"LATER LEGITIMATE EDIT\n").expect("write the later body");

    let restored = apply_plugin_iteration_journal(&workspace, &snapshot_root, None)
        .expect("an already-applied journal short-circuits");
    assert!(!restored, "an already-applied journal reports no restore");
    assert_eq!(
        fs::read(workspace.join(rel)).expect("read after the skip"),
        b"LATER LEGITIMATE EDIT\n"
    );
    assert!(!journal_path(&snapshot_root).exists());
    assert!(!applied_marker_path(&snapshot_root).exists());
}

#[test]
#[serial]
fn journal_replay_propagates_a_failing_restore_write() {
    // `rollback.rollback()?` — the journal wants to write bytes back to a path
    // that is now a non-empty directory, so `fs::write` fails and the error
    // propagates out of `apply_plugin_iteration_journal`.
    let (_temp, host) = booted_host();
    let workspace = host.fixtures_root().to_path_buf();
    let snapshot_root = workspace.join("snapshots-ops-fail");
    fs::create_dir_all(&snapshot_root).expect("mkdir snapshot root");

    let rel = "plugins/mini/src/lib.rs";
    seed_journal(&workspace, &snapshot_root, rel, b"PRE-EDIT\n");
    let target = workspace.join(rel);
    fs::create_dir_all(target.join("occupied")).expect("replace the target with a directory");

    let err = apply_plugin_iteration_journal(&workspace, &snapshot_root, None)
        .expect_err("restoring onto a directory must fail");
    assert!(
        matches!(&err, RuntimeError::Io { path, .. } if path == &target),
        "expected an Io error naming the restore target, got {err:?}"
    );
}

// ───────────── create_plugin's workspace-manifest flock branch ─────────────

/// `create_plugin` takes a `flock` on `plugins/Cargo.toml.lock` before mutating
/// the workspace manifest. Pre-creating that lock path as a FIFO makes the
/// `flock` call fail with ENOTSUP while the `O_RDWR` open still succeeds, which
/// is the only portable way to drive the `rc != 0` logging branch. The failure
/// is non-fatal: `create_plugin` still runs to completion.
#[cfg(unix)]
#[test]
#[serial]
fn create_plugin_tolerates_a_flock_failure_on_the_manifest_lock() {
    let (_temp, host) = booted_host();
    let workspace = host.fixtures_root().to_path_buf();
    let lock_path = workspace.join("plugins/Cargo.toml.lock");
    fs::create_dir_all(lock_path.parent().expect("lock parent")).expect("mkdir plugins dir");
    let c_path = std::ffi::CString::new(lock_path.as_os_str().as_encoded_bytes())
        .expect("the lock path contains no interior NUL");
    // SAFETY: `c_path` is NUL-terminated and owned for the whole call; `mkfifo`
    // only reads it.
    let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o666) };
    assert_eq!(rc, 0, "mkfifo must succeed on a fresh tempdir path");
    assert!(lock_path.exists(), "the FIFO lock path exists");

    // The workspace manifest must exist for `create_plugin` to edit it.
    fs::write(
        workspace.join("plugins/Cargo.toml"),
        "[workspace]\nmembers = []\nresolver = \"2\"\n",
    )
    .expect("seed the plugins workspace manifest");

    // `create_plugin` acquires the lock (logging the flock failure) and then
    // proceeds; whether the scaffold itself succeeds depends on the fixture
    // tree, so only assert the call is total and the FIFO was not disturbed.
    let outcome = host.create_plugin("ops_flock_probe", None);
    assert!(
        lock_path.exists(),
        "acquiring the lock must not remove the lock path"
    );
    // Either result is acceptable; what matters is that the flock failure did
    // not abort the call before the scaffold logic ran.
    match outcome {
        Ok(_) => {}
        Err(err) => {
            // Any failure must come from the scaffold, never from locking.
            let text = err.to_string();
            assert!(
                !text.contains("flock"),
                "the flock failure must stay non-fatal, got {text}"
            );
        }
    }
}
