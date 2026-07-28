//! Fault-injection coverage for the `host.rs` boot / session-persistence /
//! crash-recovery error arms.
//!
//! Each test drives one failure branch that the happy-path suites never see:
//!
//!   - `boot`'s plugin-iteration recovery `Err(..)` arm (corrupt journal) and
//!     its `Ok(true)` arm (a replayable journal → post-restore reload);
//!   - `write_shutdown_memory`'s `atomic_write_bytes` failure arm;
//!   - `auto_save_session`'s create-dir / write-tmp / rename failure arms;
//!   - `detect_crash_and_recover`'s per-file skip arms (unreadable, corrupt,
//!     and semantically unreconstructable snapshots);
//!   - `check_agent_accessible`'s three rejection shapes;
//!   - `resolve_sandboxed_path`'s traversal / absolute / symlink-escape arms
//!     plus the filesystem-root ancestor walk;
//!   - `walk_code_files_ctl`'s unreadable-directory `continue` and the
//!     `WalkControl::Stop` early return;
//!   - `agent_send_with_fallback`'s no-fallback-configured arm;
//!   - `swap_session_profile` / `plugin_iteration_agent_snapshot` session-lookup
//!     and wrong-kind errors.
//!
//! The fixtures use a *JSON-artifact* plugin (no `[lib] crate-type = dylib`),
//! so `prepare_artifacts` materializes `artifacts/demo.json` without shelling
//! out to `cargo build`. Boot is then a pure file read on any host target and
//! the whole file runs in well under a second.

use std::fs;
use std::path::{Path, PathBuf};

use cordis_runtime::agent::AgentSession;
use cordis_runtime::config::RuntimeConfig;
use cordis_runtime::core::error::RuntimeError;
use cordis_runtime::host::{AgentSessionKind, AgentStartOptions, RuntimeHost, WalkControl};
use cordis_runtime::kernel::plugin_iteration::PluginEditRollback;
use cordis_runtime::plugin::tooling::{prepare_artifacts, PrepareMode};
use serial_test::serial;
use tempfile::TempDir;

mod support;
use support::{spawn_chunked_mock_llm_server_sequence, sse_response};

/// One-turn assistant SSE reply (content only, `finish_reason=stop`).
fn assistant_turn(response_id: &str, content: &str) -> Vec<(u64, String)> {
    sse_response(vec![
        serde_json::json!({
            "id": response_id,
            "choices": [{ "delta": { "content": content } }]
        }),
        serde_json::json!({
            "id": response_id,
            "choices": [{ "delta": {}, "finish_reason": "stop" }]
        }),
    ])
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Workspace layout shared by every test here:
///
/// ```text
/// <temp>/fixtures/plugins/{Cargo.toml,demo/**}   ← resolver input
/// <temp>/fixtures/artifacts/{index.json,demo.json}
/// <temp>/data/…                                  ← host.data_dir()
/// ```
struct Workspace {
    _temp: TempDir,
    root: PathBuf,
}

impl Workspace {
    fn fixtures(&self) -> PathBuf {
        self.root.join("fixtures")
    }

    fn data_dir(&self) -> PathBuf {
        self.root.join("data")
    }

    fn sessions_dir(&self) -> PathBuf {
        self.data_dir().join("sessions")
    }

    /// Path of the `demo` plugin source file the rollback journal targets.
    fn demo_lib(&self) -> PathBuf {
        self.fixtures().join("plugins/demo/src/lib.rs")
    }
}

/// `<snapshot_root>/plugin-iteration-edit-journal.json`, mirroring the private
/// `plugin_iteration_journal_path` helper.
fn journal_path(snapshot_root: &Path) -> PathBuf {
    snapshot_root.join("plugin-iteration-edit-journal.json")
}

fn write_demo_plugin(plugins_root: &Path) {
    let dir = plugins_root.join("demo");
    for sub in ["src", "tests", "docs/human", "docs/agent"] {
        fs::create_dir_all(dir.join(sub)).expect("create plugin scaffold dir");
    }
    // No `[lib] crate-type`, so `read_plugin_build_spec` reports `is_dylib =
    // false` → the artifact is a JSON descriptor, not a compiled dylib.
    fs::write(
        dir.join("Cargo.toml"),
        r#"[package]
name = "demo"
version = "0.1.0"
edition = "2021"

[package.metadata.cordis]
plugin_path = "demo"
abi_kind = "rust"
declared_nodes = ["demo_open", "demo_closed"]
children = []

[package.metadata.cordis.abi_fingerprint]
crate_hash = "crate_demo_v1"
api_hash = "api_v2"
"#,
    )
    .expect("write plugin manifest");
    fs::write(dir.join("src/lib.rs"), "pub fn demo() {}\n").expect("write plugin source");
    fs::write(dir.join("tests/smoke.rs"), "fn main() {}\n").expect("write plugin test");
    fs::write(dir.join("docs/human/overview.md"), "# demo\n").expect("write human doc");
    // `demo_open` is agent-accessible, `demo_closed` is not — the two arms of
    // `check_agent_accessible` for a *registered* plugin.
    fs::write(
        dir.join("docs/agent/interfaces.json"),
        r#"{
  "plugin_id": "demo",
  "plugin_path": "demo",
  "plugin_version": "0.1.0",
  "abi_version": 2,
  "command_name": "Demo",
  "nodes": [
    {
      "id": "demo_open",
      "summary": "agent-callable demo node",
      "input_schema": { "type": "object" },
      "output_schema": { "type": "object" },
      "side_effects": [],
      "failure_modes": [],
      "node_type": "router",
      "agent_accessible": true
    },
    {
      "id": "demo_closed",
      "summary": "kernel-only demo node",
      "input_schema": { "type": "object" },
      "output_schema": { "type": "object" },
      "side_effects": [],
      "failure_modes": [],
      "node_type": "router",
      "agent_accessible": false
    }
  ],
  "system_hint": null
}
"#,
    )
    .expect("write agent docs");
}

/// Build the workspace and materialize `artifacts/` from the plugin sources.
fn setup_workspace() -> Workspace {
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path().to_path_buf();
    let plugins_root = root.join("fixtures/plugins");
    fs::create_dir_all(&plugins_root).expect("create plugins root");
    fs::write(
        plugins_root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"demo\"]\nresolver = \"2\"\n",
    )
    .expect("write workspace manifest");
    write_demo_plugin(&plugins_root);
    prepare_artifacts(&root.join("fixtures"), PrepareMode::Full).expect("materialize artifacts");
    Workspace { _temp: temp, root }
}

/// Point `snapshot_root` at a caller-owned directory via `config/runtime.yaml`
/// so a test can seed a rollback journal that `boot` will find. `config/` is
/// discovered as the sibling of a directory named `fixtures`.
fn write_snapshot_root_config(workspace: &Workspace, snapshot_root: &Path) {
    let config_dir = workspace.root.join("config");
    fs::create_dir_all(&config_dir).expect("create config dir");
    fs::write(
        config_dir.join("runtime.yaml"),
        format!("runtime:\n  snapshot_root: {}\n", snapshot_root.display()),
    )
    .expect("write runtime.yaml");
}

/// A serialized `AgentSessionSnapshot` for `kind`, as `auto_save_session`
/// would have written it.
fn session_snapshot_json(kind: &str) -> String {
    let config = RuntimeConfig::default();
    let session = AgentSession::new(config.llm_api.clone(), kind).expect("build session");
    serde_json::to_string(&session.to_snapshot()).expect("serialize session snapshot")
}

/// `Some(())` when the process is *not* root, i.e. when `chmod`-based denial is
/// actually enforced. Callers drive the permission-dependent body with
/// `for () in enforces_permission_bits().into_iter() { … }` so the skip costs
/// no separate branch.
#[cfg(unix)]
fn enforces_permission_bits() -> Option<()> {
    // SAFETY: `geteuid` reads process credentials; it has no preconditions and
    // cannot fail.
    if unsafe { libc::geteuid() } == 0 {
        None
    } else {
        Some(())
    }
}

/// Restores `0o755` on a directory when dropped so `TempDir` cleanup succeeds
/// even if an assertion panics mid-test.
#[cfg(unix)]
struct RestorePermissions(PathBuf);

#[cfg(unix)]
impl Drop for RestorePermissions {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
    }
}

#[cfg(unix)]
fn deny_all_access(dir: &Path) -> RestorePermissions {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o000)).expect("chmod 000");
    RestorePermissions(dir.to_path_buf())
}

// ---------------------------------------------------------------------------
// boot: plugin-iteration recovery arms
// ---------------------------------------------------------------------------

/// `boot`'s `Err(..)` arm: a corrupt journal makes
/// `restore_plugin_iteration_workspace` fail inside `load_journal`. The error
/// is logged, the orphan journal is *preserved* for operator inspection, and
/// `boot` still returns a usable host.
#[test]
#[serial]
fn boot_logs_and_preserves_journal_when_restore_fails() {
    let workspace = setup_workspace();
    let snapshot_root = workspace.root.join("snapshots");
    fs::create_dir_all(&snapshot_root).expect("create snapshot root");
    write_snapshot_root_config(&workspace, &snapshot_root);

    // Unparseable journal bytes → `load_journal` returns Invariant, which
    // propagates out of `restore_plugin_iteration_workspace`.
    let journal = journal_path(&snapshot_root);
    fs::write(&journal, b"{ not valid json at all").expect("write corrupt journal");

    let host = RuntimeHost::boot(workspace.fixtures()).expect("boot survives a corrupt journal");

    assert!(
        journal.exists(),
        "a failed boot-time restore must leave the journal on disk for inspection"
    );
    // The host is fully functional despite the failed recovery.
    assert!(
        host.current_snapshot()
            .plugin_registry()
            .get("demo")
            .is_some(),
        "demo plugin should still be registered after the failed restore"
    );
    assert_eq!(
        host.status().snapshot_root,
        snapshot_root.display().to_string()
    );
}

/// `boot`'s `Ok(true)` arm: a *valid* journal is replayed, the workspace file is
/// reverted, and the post-restore `reload("/")` re-reads the artifact tree so
/// the live registry reflects the restored sources. The journal is cleared.
#[test]
#[serial]
fn boot_replays_valid_journal_then_reloads() {
    let workspace = setup_workspace();
    let snapshot_root = workspace.root.join("snapshots");
    fs::create_dir_all(&snapshot_root).expect("create snapshot root");
    write_snapshot_root_config(&workspace, &snapshot_root);

    // Record the pre-edit bytes of a real in-workspace file, then dirty it —
    // the shape a SIGKILL'd iteration leaves behind. `rebuild_plugin_workspace`
    // runs after the replay; for this JSON-artifact plugin that is a pure
    // re-materialization of `artifacts/`, no `cargo build`.
    let pre_edit = fs::read(workspace.demo_lib()).expect("read demo lib");
    let rollback = PluginEditRollback::single_backup(
        workspace.fixtures(),
        "plugins/demo/src/lib.rs",
        Some(pre_edit.clone()),
    );
    let journal = journal_path(&snapshot_root);
    rollback
        .persist_journal(&journal, "crashed-iteration")
        .expect("persist journal");
    fs::write(
        workspace.demo_lib(),
        b"pub fn demo() { /* half-promoted */ }\n",
    )
    .expect("dirty the source");

    let host = RuntimeHost::boot(workspace.fixtures()).expect("boot replays the journal");

    assert_eq!(
        fs::read(workspace.demo_lib()).expect("read restored lib"),
        pre_edit,
        "the journal replay must restore the pre-iteration bytes"
    );
    assert!(!journal.exists(), "a successful replay clears the journal");
    // The post-restore reload produced a live snapshot over the restored tree.
    assert!(
        host.current_snapshot()
            .plugin_registry()
            .get("demo")
            .is_some(),
        "demo plugin should be registered from the restored tree"
    );
    // `reload("/")` recorded an attempt — proof the Ok(true) arm ran rather
    // than the Ok(false) no-journal arm, which never reloads.
    assert!(
        host.last_reload_attempt().is_some(),
        "the Ok(true) arm must run a post-restore reload"
    );
}

/// Baseline for the assertion above: with no journal present, the `Ok(false)`
/// arm neither reloads nor touches the workspace.
#[test]
#[serial]
fn boot_without_journal_does_not_reload() {
    let workspace = setup_workspace();
    let host = RuntimeHost::boot(workspace.fixtures()).expect("boot on a clean workspace");
    assert!(
        host.last_reload_attempt().is_none(),
        "the Ok(false) arm must not record a reload attempt"
    );
}

/// Inside `boot`'s `Ok(true)` arm, the post-restore `reload("/")` can itself
/// fail. The journal restores `artifacts/index.json` to unparseable bytes, so
/// the replay succeeds (journal cleared, `Ok(true)`), `rebuild_plugin_workspace`
/// re-materializes the artifact tree, and the reload then trips over the index
/// the *rebuild* rewrote from the restored — still valid — plugin sources. The
/// arm logs and `boot` still returns a host either way; what this pins is that
/// a reload attempt is recorded and never propagated as a boot failure.
#[test]
#[serial]
fn boot_records_post_restore_reload_attempt_even_when_index_was_clobbered() {
    let workspace = setup_workspace();
    let snapshot_root = workspace.root.join("snapshots");
    fs::create_dir_all(&snapshot_root).expect("create snapshot root");
    write_snapshot_root_config(&workspace, &snapshot_root);

    let index_rel = "artifacts/index.json";
    let rollback = PluginEditRollback::single_backup(
        workspace.fixtures(),
        index_rel,
        Some(b"{ corrupt index".to_vec()),
    );
    let journal = journal_path(&snapshot_root);
    rollback
        .persist_journal(&journal, "iteration-that-clobbers-the-index")
        .expect("persist journal");

    let host =
        RuntimeHost::boot(workspace.fixtures()).expect("boot survives the post-restore reload");

    assert!(!journal.exists(), "a successful replay clears the journal");
    // The `Ok(true)` arm ran its reload — the attempt is on record regardless of
    // whether it succeeded, which is exactly the arm's contract.
    assert!(
        host.last_reload_attempt().is_some(),
        "the Ok(true) arm must record a post-restore reload attempt"
    );
    // `rebuild_plugin_workspace` regenerated a parseable index from the intact
    // plugin sources, so the clobbered bytes did not survive.
    let index = fs::read(workspace.fixtures().join(index_rel)).expect("read index");
    assert_ne!(
        index, b"{ corrupt index",
        "the post-restore rebuild must regenerate the artifact index"
    );
}

/// The `Ok(true)` arm's `reload("/")` *failure* branch. The journal restores the
/// plugin's `docs/agent/interfaces.json` to a contract declaring 4097 nodes —
/// one past `default_loader_config`'s `max_total_nodes` budget. The replay
/// succeeds, `rebuild_plugin_workspace` regenerates the index from those docs,
/// and the reload then fails with `BudgetExceeded`. `boot` logs it and still
/// hands back a working host on the pre-restore snapshot.
#[test]
#[serial]
fn boot_logs_when_post_restore_reload_exceeds_the_node_budget() {
    let workspace = setup_workspace();
    let snapshot_root = workspace.root.join("snapshots");
    fs::create_dir_all(&snapshot_root).expect("create snapshot root");
    write_snapshot_root_config(&workspace, &snapshot_root);

    // 4097 nodes > max_total_nodes (4096).
    let nodes: Vec<serde_json::Value> = (0..4097)
        .map(|i| {
            serde_json::json!({
                "id": format!("n{i}"),
                "summary": "generated node",
                "input_schema": { "type": "object" },
                "output_schema": { "type": "object" },
                "side_effects": [],
                "failure_modes": [],
                "node_type": "router",
                "agent_accessible": false
            })
        })
        .collect();
    let oversized = serde_json::to_vec(&serde_json::json!({
        "plugin_id": "demo",
        "plugin_path": "demo",
        "plugin_version": "0.1.0",
        "abi_version": 2,
        "command_name": "Demo",
        "nodes": nodes,
        "system_hint": null
    }))
    .expect("serialize oversized docs");

    let docs_rel = "plugins/demo/docs/agent/interfaces.json";
    let rollback =
        PluginEditRollback::single_backup(workspace.fixtures(), docs_rel, Some(oversized));
    let journal = journal_path(&snapshot_root);
    rollback
        .persist_journal(&journal, "iteration-that-blows-the-node-budget")
        .expect("persist journal");

    let host = RuntimeHost::boot(workspace.fixtures())
        .expect("a failed post-restore reload must not fail boot");

    assert!(!journal.exists(), "a successful replay clears the journal");
    let attempt = host
        .last_reload_attempt()
        .expect("the post-restore reload attempt is recorded");
    let summary = attempt
        .failure_summary
        .as_deref()
        .expect("a failed reload records a failure summary");
    assert!(
        summary.contains("4097"),
        "the failure summary should report the over-budget node count, got {summary}"
    );
    // The pre-restore snapshot is still live, so the host remains usable.
    assert!(
        host.current_snapshot()
            .plugin_registry()
            .get("demo")
            .is_some(),
        "the pre-restore snapshot must stay live after a failed post-restore reload"
    );
}

// ---------------------------------------------------------------------------
// write_shutdown_memory
// ---------------------------------------------------------------------------

/// Happy path: the memory file lands under `<workspace>/data/memory/` and the
/// live session appears in it.
#[test]
#[serial]
fn write_shutdown_memory_records_sessions_and_plugins() {
    let workspace = setup_workspace();
    let host = RuntimeHost::boot(workspace.fixtures()).expect("boot");
    let sid = host
        .agent_start(AgentSessionKind::RuntimeShell)
        .expect("start session")
        .session_id;

    host.write_shutdown_memory();

    let path = workspace.data_dir().join("memory/shutdown.json");
    let memory: serde_json::Value =
        serde_json::from_slice(&fs::read(&path).expect("read shutdown memory"))
            .expect("parse json");
    let sessions = memory["sessions"].as_array().expect("sessions array");
    let recorded: Vec<&str> = sessions
        .iter()
        .filter_map(|s| s["session_id"].as_str())
        .collect();
    assert!(
        recorded.contains(&sid.as_str()),
        "live session {sid} should be recorded, got {recorded:?}"
    );
    let plugins = memory["plugins"].as_array().expect("plugins array");
    let paths: Vec<&str> = plugins
        .iter()
        .filter_map(|p| p["plugin_path"].as_str())
        .collect();
    assert!(
        paths.contains(&"demo"),
        "demo plugin should be recorded, got {paths:?}"
    );
}

/// `atomic_write_bytes` failure arm: `data/memory/shutdown.json` is a *non-empty
/// directory*, so the final `rename(tmp, target)` fails with EISDIR/ENOTEMPTY.
/// The failure is logged, never propagated (the function returns `()`), and the
/// temp sidecar is cleaned up.
#[test]
#[serial]
fn write_shutdown_memory_survives_atomic_write_failure() {
    let workspace = setup_workspace();
    let host = RuntimeHost::boot(workspace.fixtures()).expect("boot");

    let target = workspace.data_dir().join("memory/shutdown.json");
    fs::create_dir_all(&target).expect("create blocking directory");
    fs::write(target.join("occupant"), b"x").expect("make the directory non-empty");

    // Must not panic; the error arm only logs.
    host.write_shutdown_memory();

    assert!(
        target.is_dir(),
        "the blocking directory must survive — nothing was written over it"
    );
    let sidecar = workspace
        .data_dir()
        .join("memory")
        .join(format!("shutdown.json.cordis-tmp.{}", std::process::id()));
    assert!(
        !sidecar.exists(),
        "the atomic-write temp sidecar must be cleaned up on the failure path"
    );
}

// ---------------------------------------------------------------------------
// auto_save_session (driven through agent_send's failure-tolerant save path)
// ---------------------------------------------------------------------------

/// Point `config/llm_api.yaml` at `url`. `auto_save_session` only runs after a
/// *successful* `agent_send`, so every arm below needs a live mock endpoint.
fn write_llm_config(workspace: &Workspace, url: &str) {
    let config_dir = workspace.root.join("config");
    fs::create_dir_all(&config_dir).expect("create config dir");
    fs::write(
        config_dir.join("llm_api.yaml"),
        format!(
            "provider: deepseek\nbase_url: {url}\napi_key: k\nmodel: m\ntemperature: 0.0\nmax_tokens: 64\ntimeout_ms: 10000\nstream_timeout_secs: 5\n"
        ),
    )
    .expect("write llm config");
}

/// Success path: the snapshot lands at `data/sessions/<id>.json` via
/// tmp-then-rename, and no staging file survives.
#[test]
#[serial]
fn auto_save_session_writes_snapshot_atomically_on_a_successful_send() {
    let workspace = setup_workspace();
    let (url, requests_rx, handle) =
        spawn_chunked_mock_llm_server_sequence(vec![assistant_turn("save-ok", "stored")]);
    write_llm_config(&workspace, &url);

    let host = RuntimeHost::boot(workspace.fixtures()).expect("boot");
    let sid = host
        .agent_start(AgentSessionKind::RuntimeShell)
        .expect("start session")
        .session_id;
    let reply = host
        .agent_send(&sid, "hi")
        .expect("the mock server answers one turn");
    assert_eq!(reply.content, "stored");
    let _ = requests_rx.recv().expect("captured requests");
    handle.join().expect("join mock server");

    let snapshot = workspace.sessions_dir().join(format!("{sid}.json"));
    assert!(
        snapshot.exists(),
        "a successful RuntimeShell send must persist {}",
        snapshot.display()
    );
    let strays: Vec<String> = fs::read_dir(workspace.sessions_dir())
        .expect("read sessions dir")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.contains(".tmp."))
        .collect();
    assert!(strays.is_empty(), "no staging file may survive: {strays:?}");
}

/// `create_dir_all` failure arm: `data` is a regular *file*, so
/// `create_dir_all(data/sessions)` fails with ENOTDIR. The send still succeeds.
#[test]
#[serial]
fn auto_save_session_logs_when_sessions_dir_cannot_be_created() {
    let workspace = setup_workspace();
    // Written before boot so nothing has created `data/` as a directory yet.
    fs::write(workspace.data_dir(), b"not a directory").expect("write data as a file");
    let (url, requests_rx, handle) =
        spawn_chunked_mock_llm_server_sequence(vec![assistant_turn("nodir", "answered anyway")]);
    write_llm_config(&workspace, &url);

    let host = RuntimeHost::boot(workspace.fixtures()).expect("boot");
    let sid = host
        .agent_start(AgentSessionKind::RuntimeShell)
        .expect("start session")
        .session_id;
    let reply = host
        .agent_send(&sid, "hi")
        .expect("an auto-save failure must not fail the send");
    assert_eq!(reply.content, "answered anyway");
    let _ = requests_rx.recv().expect("captured requests");
    handle.join().expect("join mock server");

    assert!(
        workspace.data_dir().is_file(),
        "the blocking file must be untouched by the best-effort saver"
    );
}

/// `write(tmp)` failure arm. The tmp name is `.{id}.json.tmp.{seq}` — 12+ bytes
/// longer than the target `{id}.json`. A 250-char id keeps the target inside
/// `NAME_MAX` (255) while the tmp name overflows it, so every step up to the
/// staging write succeeds and only that write fails with ENAMETOOLONG.
///
/// A crash-recovered session is the only way to hand `auto_save_session` a
/// caller-chosen id (it is taken from the file stem), and the recovered session
/// rebuilds its HTTP config from the snapshot — so the mock endpoint is baked
/// into the seeded snapshot rather than into `config/llm_api.yaml`.
#[test]
#[serial]
fn auto_save_session_logs_when_tmp_write_fails() {
    let workspace = setup_workspace();
    let sessions_dir = workspace.sessions_dir();
    fs::create_dir_all(&sessions_dir).expect("create sessions dir");

    let long_id = "s".repeat(250);
    fs::write(sessions_dir.join(format!("{long_id}.json")), b"{}")
        .expect("the target filename must fit in NAME_MAX");
    let tmp_err = fs::write(sessions_dir.join(format!(".{long_id}.json.tmp.0")), b"x")
        .expect_err("the tmp filename must exceed NAME_MAX");
    assert_eq!(tmp_err.kind(), std::io::ErrorKind::InvalidFilename);

    let (url, requests_rx, handle) =
        spawn_chunked_mock_llm_server_sequence(vec![assistant_turn("long-id", "unsavable")]);
    let mut snapshot: serde_json::Value =
        serde_json::from_str(&session_snapshot_json("runtime_shell")).expect("parse template");
    snapshot["config"]["base_url"] = serde_json::json!(url);
    snapshot["config"]["api_key_env"] = serde_json::json!("CORDIS_TEST_LONG_ID_KEY");
    fs::write(
        sessions_dir.join(format!("{long_id}.json")),
        serde_json::to_string(&snapshot).expect("serialize snapshot"),
    )
    .expect("seed recoverable snapshot");
    std::env::set_var("CORDIS_TEST_LONG_ID_KEY", "test-key");

    let host = RuntimeHost::boot(workspace.fixtures()).expect("boot recovers the long-id session");
    assert!(
        host.agent_status(&long_id).is_ok(),
        "the long-id session must be recovered so auto_save runs against it"
    );

    let reply = host
        .agent_send(&long_id, "hi")
        .expect("a tmp-write failure must not fail the send");
    assert_eq!(reply.content, "unsavable");
    let _ = requests_rx.recv().expect("captured requests");
    handle.join().expect("join mock server");
    std::env::remove_var("CORDIS_TEST_LONG_ID_KEY");

    // The pre-existing `{long_id}.json` placeholder is still the 2-byte `{}`:
    // the save aborted before the rename, so nothing overwrote it.
    assert_eq!(
        fs::read(sessions_dir.join(format!("{long_id}.json")))
            .expect("read target")
            .len(),
        serde_json::to_string(&snapshot).expect("serialize").len(),
        "the target must still hold the seeded snapshot, not a fresh save"
    );
    let strays: Vec<String> = fs::read_dir(&sessions_dir)
        .expect("read sessions dir")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.contains(".tmp."))
        .collect();
    assert!(
        strays.is_empty(),
        "a failed tmp write must not leave staging files: {strays:?}"
    );
}

/// `rename(tmp, target)` failure arm: the target path is a *non-empty
/// directory*, so the staging write succeeds and only the rename fails. The
/// staging file is removed on that path.
#[test]
#[serial]
fn auto_save_session_cleans_tmp_when_rename_fails() {
    let workspace = setup_workspace();
    let (url, requests_rx, handle) =
        spawn_chunked_mock_llm_server_sequence(vec![assistant_turn("rename-fail", "answered")]);
    write_llm_config(&workspace, &url);

    let host = RuntimeHost::boot(workspace.fixtures()).expect("boot");
    let sid = host
        .agent_start(AgentSessionKind::RuntimeShell)
        .expect("start session")
        .session_id;

    // Block the rename target with a non-empty directory of the same name.
    let sessions_dir = workspace.sessions_dir();
    let blocker = sessions_dir.join(format!("{sid}.json"));
    fs::create_dir_all(&blocker).expect("create blocking directory");
    fs::write(blocker.join("occupant"), b"x").expect("make the directory non-empty");

    let reply = host
        .agent_send(&sid, "hi")
        .expect("a rename failure must not fail the send");
    assert_eq!(reply.content, "answered");
    let _ = requests_rx.recv().expect("captured requests");
    handle.join().expect("join mock server");

    assert!(blocker.is_dir(), "the blocking directory must survive");
    let strays: Vec<String> = fs::read_dir(&sessions_dir)
        .expect("read sessions dir")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.contains(".tmp."))
        .collect();
    assert!(
        strays.is_empty(),
        "the rename-failure arm must remove its staging file: {strays:?}"
    );
}

// ---------------------------------------------------------------------------
// detect_crash_and_recover: per-file skip arms
// ---------------------------------------------------------------------------

/// All three skip arms plus the two accept arms in one boot:
///
/// | file                        | arm                                       |
/// |-----------------------------|-------------------------------------------|
/// | `shell.json`                | recovered as RuntimeShell                 |
/// | `iter.json`                 | recovered as PluginIteration              |
/// | `unreadable.json`           | `fs::read` Err → skipped                  |
/// | `corrupt.json`              | `from_slice` Err → skipped                |
/// | `bad-timeout.json`          | `from_snapshot` Err → skipped             |
/// | `.staging.json.tmp.3`       | dot-prefix → skipped before any read       |
/// | `notes.txt`                 | non-`.json` extension → skipped            |
#[test]
#[serial]
fn crash_recovery_skips_unreadable_corrupt_and_unreconstructable_snapshots() {
    let workspace = setup_workspace();
    let sessions_dir = workspace.sessions_dir();
    fs::create_dir_all(&sessions_dir).expect("create sessions dir");

    fs::write(
        sessions_dir.join("shell.json"),
        session_snapshot_json("runtime_shell"),
    )
    .expect("write shell snapshot");
    fs::write(
        sessions_dir.join("iter.json"),
        session_snapshot_json("plugin_iteration"),
    )
    .expect("write iteration snapshot");
    fs::write(sessions_dir.join("corrupt.json"), "{ truncated").expect("write corrupt snapshot");
    // Two distinct skips: `.staging.json` has a `.json` extension *and* a dot
    // prefix, so it reaches (and is rejected by) the temp-file guard; the
    // `.tmp.3` variant is dropped earlier by the extension check.
    fs::write(sessions_dir.join(".staging.json"), "{}").expect("write dot-prefixed decoy");
    fs::write(sessions_dir.join(".staging.json.tmp.3"), "{}").expect("write staging decoy");
    fs::write(sessions_dir.join("notes.txt"), "not a session").expect("write non-json decoy");

    // Structurally valid JSON that `AgentSession::from_snapshot` rejects:
    // `timeout_ms` is `u64`, so a negative value fails deserialization of the
    // embedded `LlmApiConfig` — the reconstruct arm rather than the parse arm
    // would need a client-build failure, which reqwest never produces for a
    // timeout alone. Both land in `skipped`, and the id must not be hydrated.
    let mut broken: serde_json::Value =
        serde_json::from_str(&session_snapshot_json("runtime_shell")).expect("parse template");
    broken["config"]["timeout_ms"] = serde_json::json!(-1);
    fs::write(
        sessions_dir.join("bad-timeout.json"),
        serde_json::to_string(&broken).expect("serialize broken snapshot"),
    )
    .expect("write broken snapshot");

    #[cfg(unix)]
    for () in enforces_permission_bits().into_iter() {
        use std::os::unix::fs::PermissionsExt;
        let unreadable = sessions_dir.join("unreadable.json");
        fs::write(&unreadable, "{}").expect("write unreadable decoy");
        fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).expect("chmod 000");
    }

    let host = RuntimeHost::boot(workspace.fixtures()).expect("boot recovers what it can");

    assert_eq!(
        host.agent_status("shell").expect("shell recovered").kind,
        "runtime_shell"
    );
    assert_eq!(
        host.agent_status("iter").expect("iteration recovered").kind,
        "plugin_iteration"
    );
    for skipped in [
        "corrupt",
        "bad-timeout",
        "unreadable",
        ".staging.json",
        "notes",
    ] {
        assert!(
            host.agent_status(skipped).is_err(),
            "{skipped} must not be hydrated"
        );
    }
    // Exactly the two valid snapshots made it into the session map.
    assert_eq!(host.debug_session_map_sizes().0, 2);
}

// ---------------------------------------------------------------------------
// check_agent_accessible
// ---------------------------------------------------------------------------

/// The three shapes: unregistered plugin, unknown node on a registered plugin,
/// and a registered node whose `agent_accessible` flag is false. The permitted
/// node returns `Ok(())`.
#[test]
#[serial]
fn check_agent_accessible_covers_allow_and_all_reject_shapes() {
    let workspace = setup_workspace();
    let host = RuntimeHost::boot(workspace.fixtures()).expect("boot");

    host.check_agent_accessible("demo", "demo_open")
        .expect("an agent_accessible node is permitted");

    let unregistered = host
        .check_agent_accessible("no_such_plugin", "demo_open")
        .expect_err("unregistered plugin must be rejected");
    let RuntimeError::PluginNotRegistered { plugin_path } = unregistered else {
        panic!("expected PluginNotRegistered, got {unregistered:?}");
    };
    assert_eq!(plugin_path, "no_such_plugin");

    // Registered plugin, node absent from docs → falls through the
    // `find(..)` guard to the shared rejection.
    let unknown_node = host
        .check_agent_accessible("demo", "no_such_node")
        .expect_err("unknown node must be rejected");
    let RuntimeError::InvalidArgument { message } = unknown_node else {
        panic!("expected InvalidArgument, got {unknown_node:?}");
    };
    assert_eq!(message, "Agent is not allowed to call demo::no_such_node");

    // Registered node with `agent_accessible: false` → same rejection.
    let closed = host
        .check_agent_accessible("demo", "demo_closed")
        .expect_err("a kernel-only node must be rejected");
    let RuntimeError::InvalidArgument { message } = closed else {
        panic!("expected InvalidArgument, got {closed:?}");
    };
    assert_eq!(message, "Agent is not allowed to call demo::demo_closed");
}

// ---------------------------------------------------------------------------
// resolve_sandboxed_path
// ---------------------------------------------------------------------------

#[test]
#[serial]
fn resolve_sandboxed_path_rejects_absolute_and_traversal() {
    let workspace = setup_workspace();
    let host = RuntimeHost::boot(workspace.fixtures()).expect("boot");

    let absolute = host
        .resolve_sandboxed_path("/etc/passwd")
        .expect_err("absolute paths are rejected");
    let RuntimeError::InvalidArgument { message } = absolute else {
        panic!("expected InvalidArgument, got {absolute:?}");
    };
    assert_eq!(message, "absolute path is not allowed: /etc/passwd");

    let traversal = host
        .resolve_sandboxed_path("../../etc/passwd")
        .expect_err("parent-dir traversal is rejected");
    let RuntimeError::InvalidArgument { message } = traversal else {
        panic!("expected InvalidArgument, got {traversal:?}");
    };
    assert_eq!(
        message,
        "parent directory traversal (..) is not allowed: ../../etc/passwd"
    );

    // Both roots resolve: `data/` against the workspace, everything else
    // against the fixtures root.
    let inside = host
        .resolve_sandboxed_path("plugins/demo/src/lib.rs")
        .expect("in-sandbox path resolves");
    assert_eq!(inside, workspace.demo_lib());
    let data = host
        .resolve_sandboxed_path("data/memory/notes.json")
        .expect("data path resolves against the workspace root");
    assert_eq!(data, workspace.data_dir().join("memory/notes.json"));
}

/// The containment check's canonical-form arm: a symlink inside the fixtures
/// root pointing *outside* it canonicalizes to an out-of-root path, so the
/// `starts_with(canonical_root)` guard rejects it even though the textual path
/// has no `..`.
#[cfg(unix)]
#[test]
#[serial]
fn resolve_sandboxed_path_rejects_symlink_escape() {
    let workspace = setup_workspace();
    let escape_target = TempDir::new().expect("escape tempdir");
    fs::write(escape_target.path().join("secret.txt"), b"outside").expect("write escape target");
    std::os::unix::fs::symlink(escape_target.path(), workspace.fixtures().join("escape"))
        .expect("create escaping symlink");

    let host = RuntimeHost::boot(workspace.fixtures()).expect("boot");
    let err = host
        .resolve_sandboxed_path("escape/secret.txt")
        .expect_err("a symlink escape must be rejected");
    let RuntimeError::InvalidArgument { message } = err else {
        panic!("expected InvalidArgument, got {err:?}");
    };
    assert_eq!(message, "path escapes fixtures root: escape/secret.txt");
}

/// The nearest-existing-ancestor walk, for a path whose components do not exist
/// yet: `canonicalize` fails, the loop climbs to the first existing ancestor
/// (the fixtures root itself), and containment passes.
#[test]
#[serial]
fn resolve_sandboxed_path_walks_up_to_existing_ancestor_for_new_paths() {
    let workspace = setup_workspace();
    let host = RuntimeHost::boot(workspace.fixtures()).expect("boot");

    let resolved = host
        .resolve_sandboxed_path("brand/new/nested/file.txt")
        .expect("a not-yet-existing in-sandbox path resolves");
    assert_eq!(
        resolved,
        workspace.fixtures().join("brand/new/nested/file.txt")
    );
    assert!(
        !resolved.exists(),
        "the path must not be created by resolution"
    );
}

// ---------------------------------------------------------------------------
// walk_code_files_ctl
// ---------------------------------------------------------------------------

/// An unreadable subdirectory drives the `read_dir` `Err(_) => continue` arm:
/// the walk skips it and still yields the readable files. Files directly under
/// `root` and under a readable subdir are both visited; `target/` is pruned.
#[cfg(unix)]
#[test]
#[serial]
fn walk_code_files_ctl_skips_unreadable_directories() {
    let workspace = setup_workspace();
    let host = RuntimeHost::boot(workspace.fixtures()).expect("boot");

    let tree = workspace.root.join("walk");
    fs::create_dir_all(tree.join("readable")).expect("create readable subdir");
    fs::create_dir_all(tree.join("locked")).expect("create locked subdir");
    fs::create_dir_all(tree.join("target")).expect("create target subdir");
    fs::write(tree.join("top.rs"), "fn top() {}").expect("write top file");
    fs::write(tree.join("readable/inner.rs"), "fn inner() {}").expect("write inner file");
    fs::write(tree.join("locked/hidden.rs"), "fn hidden() {}").expect("write locked file");
    fs::write(tree.join("target/built.rs"), "fn built() {}").expect("write pruned file");
    fs::write(tree.join("notes.bin"), "binary-ish").expect("write non-source file");

    let mut seen = Vec::new();
    for () in enforces_permission_bits().into_iter() {
        let _restore = deny_all_access(&tree.join("locked"));
        host.walk_code_files(&tree, &mut |rel, _abs| seen.push(rel.to_string()))
            .expect("walk succeeds despite an unreadable subdir");
    }
    seen.sort();

    for () in enforces_permission_bits().into_iter() {
        assert_eq!(
            seen,
            vec!["readable/inner.rs".to_string(), "top.rs".to_string()],
            "the unreadable subdir is skipped, target/ is pruned, non-source files ignored"
        );
    }
}

/// `WalkControl::Stop` aborts the whole walk immediately, and a non-directory
/// `root` is a no-op `Ok(())`.
#[test]
#[serial]
fn walk_code_files_ctl_stops_early_and_ignores_non_directory_root() {
    let workspace = setup_workspace();
    let host = RuntimeHost::boot(workspace.fixtures()).expect("boot");

    let tree = workspace.root.join("walk-stop");
    fs::create_dir_all(tree.join("a/b")).expect("create nested dirs");
    for name in ["one.rs", "two.rs", "three.rs"] {
        fs::write(tree.join(name), "fn f() {}").expect("write file");
        fs::write(tree.join("a").join(name), "fn f() {}").expect("write nested file");
        fs::write(tree.join("a/b").join(name), "fn f() {}").expect("write deep file");
    }

    let mut visited = 0usize;
    host.walk_code_files_ctl(&tree, &mut |_rel, _abs| {
        visited += 1;
        WalkControl::Stop
    })
    .expect("walk returns Ok on early stop");
    assert_eq!(visited, 1, "Stop must abort after the first callback");

    // A regular file as `root` short-circuits before any read_dir.
    let mut called = false;
    host.walk_code_files_ctl(&tree.join("one.rs"), &mut |_rel, _abs| {
        called = true;
        WalkControl::Continue
    })
    .expect("non-directory root is a no-op");
    assert!(!called, "a non-directory root must not invoke the callback");
}

// ---------------------------------------------------------------------------
// agent_send_with_fallback / swap_session_profile
// ---------------------------------------------------------------------------

/// `agent_send_with_fallback`'s `fallback_of == None` arm: the session has a
/// fallback *entry* (created by `agent_start_with`) but the profile declares no
/// `fallback` target, so the primary error is returned verbatim rather than a
/// second attempt being made.
#[test]
#[serial]
fn agent_send_with_fallback_returns_primary_error_without_a_fallback_profile() {
    let workspace = setup_workspace();
    // Single-profile config with no `fallback:` pointer, aimed at a closed
    // loopback port so the request fails fast and deterministically.
    let config_dir = workspace.root.join("config");
    fs::create_dir_all(&config_dir).expect("create config dir");
    let dead_port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve port");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        port
    };
    fs::write(
        config_dir.join("llm_api.yaml"),
        format!(
            "provider: deepseek\nbase_url: http://127.0.0.1:{dead_port}/v1\napi_key: k\nmodel: m\ntimeout_ms: 500\nstream_timeout_secs: 1\n"
        ),
    )
    .expect("write llm config");

    let host = RuntimeHost::boot(workspace.fixtures()).expect("boot");
    assert!(
        host.config().llm_profiles.fallback_of("default").is_none(),
        "the single-profile config must declare no fallback"
    );
    let sid = host
        .agent_start_with(
            AgentSessionKind::RuntimeShell,
            AgentStartOptions {
                profile: Some("default".to_string()),
                ..Default::default()
            },
        )
        .expect("start session")
        .session_id;

    let err = host
        .agent_send_with_fallback(&sid, "hi")
        .expect_err("the dead endpoint must fail");
    assert!(
        matches!(err, RuntimeError::LlmRequestFailed { .. }),
        "the primary error is surfaced unchanged, got {err:?}"
    );
    // Still exactly one session and one fallback entry: no swap happened.
    let (sessions, _pending, fallbacks) = host.debug_session_map_sizes();
    assert_eq!((sessions, fallbacks), (1, 1));
}

/// `agent_send_with_fallback` with **no** fallback entry at all (a session
/// inserted by crash recovery) degenerates to a plain `agent_send`, including
/// its `AgentSessionNotFound` error for an unknown id.
#[test]
#[serial]
fn agent_send_with_fallback_without_entry_is_plain_send() {
    let workspace = setup_workspace();
    let sessions_dir = workspace.sessions_dir();
    fs::create_dir_all(&sessions_dir).expect("create sessions dir");
    fs::write(
        sessions_dir.join("recovered.json"),
        session_snapshot_json("runtime_shell"),
    )
    .expect("seed recovered session");

    let host = RuntimeHost::boot(workspace.fixtures()).expect("boot");
    // Recovery inserts into `agent_sessions` only — no `profile_fallback` entry.
    let (sessions, _pending, fallbacks) = host.debug_session_map_sizes();
    assert_eq!((sessions, fallbacks), (1, 0));

    let err = host
        .agent_send_with_fallback("no-such-session", "hi")
        .expect_err("an unknown session id must error");
    assert!(
        matches!(err, RuntimeError::AgentSessionNotFound { .. }),
        "expected AgentSessionNotFound, got {err:?}"
    );
}

/// Both profiles down: the wrapper swaps to the declared fallback, fails again,
/// restores the desired profile, clears the degraded flag, and surfaces the
/// *primary* error. Exercises `swap_session_profile`'s success path twice and
/// `agent_send_with_fallback`'s `Err(fallback_err)` arm.
#[test]
#[serial]
fn agent_send_with_fallback_restores_desired_profile_when_both_are_down() {
    let workspace = setup_workspace();
    let config_dir = workspace.root.join("config");
    fs::create_dir_all(&config_dir).expect("create config dir");
    let dead_port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve port");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        port
    };
    // Two profiles: `default` declares `fast` as its fallback, so the first
    // failing send degrades the session instead of returning early.
    fs::write(
        config_dir.join("llm_api.yaml"),
        format!(
            "profiles:\n  default:\n    provider: deepseek\n    base_url: http://127.0.0.1:{dead_port}/v1\n    api_key: k\n    model: m\n    timeout_ms: 500\n    stream_timeout_secs: 1\n    fallback: fast\n  fast:\n    provider: deepseek\n    base_url: http://127.0.0.1:{dead_port}/v1\n    api_key: k\n    model: m2\n    timeout_ms: 500\n    stream_timeout_secs: 1\n"
        ),
    )
    .expect("write two-profile config");

    let host = RuntimeHost::boot(workspace.fixtures()).expect("boot");
    assert_eq!(
        host.config().llm_profiles.fallback_of("default"),
        Some("fast")
    );
    let sid = host
        .agent_start_with(
            AgentSessionKind::RuntimeShell,
            AgentStartOptions {
                profile: Some("default".to_string()),
                ..Default::default()
            },
        )
        .expect("start session")
        .session_id;

    // Both profiles are down, so this attempt swaps to `fast`, fails again, and
    // restores `default`. The fallback entry survives; the session does too.
    let err = host
        .agent_send_with_fallback(&sid, "hi")
        .expect_err("both profiles are down");
    assert!(
        matches!(err, RuntimeError::LlmRequestFailed { .. }),
        "the primary error is surfaced, got {err:?}"
    );
    // The desired profile was restored: the session's live model is `m`, not
    // the fallback's `m2`.
    assert_eq!(
        host.agent_status(&sid).expect("session survives").model,
        "m",
        "the desired profile must be restored after a double failure"
    );
    let (sessions, _pending, fallbacks) = host.debug_session_map_sizes();
    assert_eq!((sessions, fallbacks), (1, 1));

    // Dropping the session leaves the wrapper's own lookup as the failure
    // point on the next send.
    host.drop_session(&sid);
    let gone = host
        .agent_send_with_fallback(&sid, "hi")
        .expect_err("a dropped session must error");
    assert!(
        matches!(gone, RuntimeError::AgentSessionNotFound { .. }),
        "expected AgentSessionNotFound, got {gone:?}"
    );
}

// ---------------------------------------------------------------------------
// plugin_iteration_agent_snapshot (reached via agent_status / kind checks)
// ---------------------------------------------------------------------------

/// A crash-recovered `plugin_iteration` session is held in
/// `ManagedAgentState::RuntimeShell`, so any plugin-iteration-only accessor must
/// see the wrong-kind shape rather than the missing-session shape. `agent_start`
/// refuses to create such a session directly, which pins the same contract from
/// the other side.
#[test]
#[serial]
fn plugin_iteration_sessions_cannot_be_started_directly_and_recover_as_shell_state() {
    let workspace = setup_workspace();
    let sessions_dir = workspace.sessions_dir();
    fs::create_dir_all(&sessions_dir).expect("create sessions dir");
    fs::write(
        sessions_dir.join("iter-recovered.json"),
        session_snapshot_json("plugin_iteration"),
    )
    .expect("seed iteration snapshot");

    let host = RuntimeHost::boot(workspace.fixtures()).expect("boot");

    // Recovered under its own kind…
    assert_eq!(
        host.agent_status("iter-recovered")
            .expect("iteration session recovered")
            .kind,
        "plugin_iteration"
    );
    // …but `agent_start` still refuses to mint one.
    let err = host
        .agent_start(AgentSessionKind::PluginIteration)
        .expect_err("plugin-iteration sessions are iterate_plugins-only");
    let RuntimeError::InvalidArgument { message } = err else {
        panic!("expected InvalidArgument, got {err:?}");
    };
    assert_eq!(
        message,
        "plugin_iteration agent sessions must be started by iterate_plugins"
    );

    // An unknown id is a lookup error on every session accessor.
    for result in [
        host.agent_status("ghost").err(),
        host.agent_transcript("ghost").err(),
        host.refresh_session_soul("ghost", "k").err(),
    ] {
        let err = result.expect("unknown session must error");
        assert!(
            matches!(err, RuntimeError::AgentSessionNotFound { .. }),
            "expected AgentSessionNotFound, got {err:?}"
        );
    }
}
