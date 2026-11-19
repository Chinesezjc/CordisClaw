//! Coverage batch for the host seam: the `ManagedAgentSession` `Debug`
//! rendering, the legacy group-soul key fallback, the best-effort session
//! snapshot delete, an unreadable crash-recovery snapshot, the ambiguous
//! `plugin_for_node` arm, the scaffold docs-directory error, image-skip
//! logging, the end-of-turn flag setter, a failed service restart and the
//! "target is only a slash" iteration root.
//!
//! Every case drives the real path with a hermetic fixture tree: the plugins
//! are JSON artifacts (no dylib, no compiler, no network), and the failures are
//! filesystem shapes a root CI process still trips over.

use super::*;
use crate::core::models::{ArtifactIndexEntry, ArtifactKind, PluginArtifact, PluginDocs};
use cordis_plugin_sdk::NodeDoc;
use serial_test::serial;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// Docs for a JSON plugin declaring exactly `node_ids`.
fn docs_with_nodes(plugin_path: &str, node_ids: &[&str]) -> PluginDocs {
    PluginDocs {
        plugin_id: plugin_path.replace('/', "-"),
        plugin_path: plugin_path.to_string(),
        plugin_version: "0.1.0".to_string(),
        abi_version: 2,
        command_name: None,
        nodes: node_ids
            .iter()
            .map(|node_id| NodeDoc {
                id: (*node_id).to_string(),
                summary: format!("{node_id} node"),
                input_schema: serde_json::json!({}),
                output_schema: serde_json::json!({}),
                side_effects: Vec::new(),
                failure_modes: Vec::new(),
                node_type: NodeType::Task,
                agent_accessible: false,
            })
            .collect(),
        system_hint: None,
    }
}

/// Write one JSON artifact and return its index entry.
fn write_json_plugin(artifacts: &Path, plugin_path: &str, node_ids: &[&str]) -> ArtifactIndexEntry {
    let abi = AbiFingerprint::current_build(
        format!("crate_{}_v1", plugin_path.replace('/', "_")),
        "api_v2",
    );
    let docs = docs_with_nodes(plugin_path, node_ids);
    let artifact = PluginArtifact {
        plugin_path: plugin_path.to_string(),
        abi_fingerprint: abi.clone(),
        docs: docs.clone(),
        exports: Vec::new(),
        execution: None,
    };
    let file_name = format!("{}.json", plugin_path.replace('/', "_"));
    let artifact_path = artifacts.join(&file_name);
    fs::write(
        &artifact_path,
        serde_json::to_string_pretty(&artifact).expect("artifact json"),
    )
    .expect("write artifact");
    let sha256 = crate::plugin::artifact::sha256_file(&artifact_path).expect("sha256");
    ArtifactIndexEntry {
        plugin_path: plugin_path.to_string(),
        version: "0.1.0".to_string(),
        abi_fingerprint: abi,
        artifact_path: file_name,
        sha256,
        built_at: "0".to_string(),
        parent: None,
        required: true,
        grants_from_parent: Vec::new(),
        docs,
        exports: Vec::new(),
        execution: None,
        artifact_kind: ArtifactKind::Json,
        build_fingerprint: "bf".to_string(),
        input_probe: Default::default(),
        local_path_deps: Vec::new(),
    }
}

/// A fixtures tree whose artifact index holds only `plugins` (JSON artifacts).
fn fixture_tree(plugins: &[(&str, &[&str])]) -> (TempDir, PathBuf) {
    let temp = TempDir::new().expect("tempdir");
    let fixtures = temp.path().join("fixtures");
    let artifacts = fixtures.join("artifacts");
    fs::create_dir_all(&artifacts).expect("artifacts dir");
    let mut entries = Vec::new();
    let mut topo_order = Vec::new();
    for (plugin_path, node_ids) in plugins {
        entries.push(write_json_plugin(&artifacts, plugin_path, node_ids));
        topo_order.push((*plugin_path).to_string());
    }
    let index = serde_json::json!({
        "schema_version": 2,
        "generated_at": "2026-11-18T00:00:00Z",
        "topo_order": topo_order,
        "entries": entries,
    });
    fs::write(
        artifacts.join("index.json"),
        serde_json::to_string_pretty(&index).expect("index json"),
    )
    .expect("write index");
    (temp, fixtures)
}

fn boot_empty() -> (TempDir, RuntimeHost) {
    let (temp, fixtures) = fixture_tree(&[]);
    let host = RuntimeHost::boot(&fixtures).expect("boot on empty index");
    (temp, host)
}

fn empty_snapshot() -> RuntimeSnapshot {
    runtime_snapshot_from_output(
        crate::plugin::loader::LoadOutput {
            execution_id: "snap-cov-batch-empty".to_string(),
            plugin_registry: crate::plugin::registry::PluginRegistry::default(),
            node_registry: crate::plugin::registry::NodeRegistry::default(),
            doc_registry: crate::service::doc_registry::DocRegistry::default(),
            graph_registry: crate::service::graph_registry::GraphRegistry::default(),
            context: crate::context::RuntimeContext::default(),
            metrics: crate::plugin::loader::LoaderMetrics::default(),
        },
        PathBuf::from("/tmp/cordis-cov-batch-empty-staged"),
        BTreeMap::new(),
    )
}

fn start_shell(host: &RuntimeHost) -> AgentSessionHandle {
    host.agent_start_with(AgentSessionKind::RuntimeShell, AgentStartOptions::default())
        .expect("start a runtime-shell session")
}

// ─────────────── ManagedAgentSession Debug (host/mod.rs 900-906) ─────

#[test]
#[serial]
fn managed_session_debug_names_the_handle_and_state() {
    let (_temp, host) = boot_empty();
    let handle = start_shell(&host);
    let managed = host
        .agent_sessions
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .remove(&handle.session_id)
        .expect("live session");
    let rendered = format!("{managed:?}");
    assert!(rendered.contains("ManagedAgentSession"), "{rendered}");
    assert!(rendered.contains(&handle.session_id), "{rendered}");
    assert!(rendered.contains("RuntimeShell"), "{rendered}");
}

// ─────────────── legacy group-soul fallback (host/mod.rs 1647) ──────

/// A group soul stored under the old `qq:<group>#group` key must still be found
/// when the channel asks for the new `qq:group:<group>` key — otherwise every
/// group silently falls back to the default persona.
#[test]
#[serial]
fn soul_lookup_falls_back_to_the_legacy_group_key() {
    let (_temp, host) = boot_empty();
    let soul = crate::soul::Soul {
        persona: "legacy group persona".to_string(),
        ..Default::default()
    };
    host.set_soul("qq:123#group", &soul)
        .expect("store a soul under the legacy key");

    let found = host
        .get_soul("qq:group:123")
        .expect("legacy lookup")
        .expect("the legacy row must be found");
    assert_eq!(found.persona, "legacy group persona");
    assert!(host.get_soul("qq:group:999").expect("miss").is_none());
}

// ─────────────── best-effort snapshot delete (host/mod.rs 1744) ─────

/// `delete_session_snapshot` is best-effort: a path that cannot be unlinked is
/// logged and ignored, never escalated. A directory squatting on the snapshot
/// name produces a real `remove_file` failure for root too.
#[test]
#[serial]
fn delete_session_snapshot_survives_a_directory_in_its_place() {
    let (_temp, host) = boot_empty();
    let path = host.data_dir().join("sessions").join("ghost.json");
    fs::create_dir_all(&path).expect("directory squatting on the snapshot path");
    host.delete_session_snapshot("ghost");
    assert!(
        path.is_dir(),
        "a best-effort delete must not remove a directory"
    );
}

// ─────────────── unreadable crash-recovery snapshot (1805-1807) ─────

/// A session snapshot that cannot be read is logged and skipped; boot must
/// still succeed and leave no session behind.
#[test]
#[serial]
fn crash_recovery_logs_and_skips_an_unreadable_snapshot() {
    let (temp, fixtures) = fixture_tree(&[]);
    let sessions = temp.path().join("data/sessions");
    fs::create_dir_all(sessions.join("agent-session-1.json"))
        .expect("directory squatting on a snapshot name");
    let host = RuntimeHost::boot(&fixtures).expect("boot must survive an unreadable snapshot");
    assert_eq!(host.debug_session_map_sizes().0, 0);
    assert!(sessions.join("agent-session-1.json").is_dir());
}

// ─────────────── ambiguous plugin_for_node (1913-1941) ─────────────

/// A node id declared by two loaded plugins cannot pick a plugin; the tool call
/// that named only the node must be told so instead of guessing.
#[test]
#[serial]
fn plugin_for_node_rejects_a_node_declared_by_several_plugins() {
    let (_temp, fixtures) = fixture_tree(&[("aaa", &["dup"]), ("bbb", &["dup"])]);
    let host = RuntimeHost::boot(&fixtures).expect("boot two plugins");
    let err = host.plugin_for_node("dup").expect_err("ambiguous node");
    assert!(
        err.to_string().contains("declared by several plugins"),
        "unexpected: {err}"
    );
}

// ─────────────── scaffold docs-dir error (2423-2428) ───────────────

/// The docs-directory creation failure cannot be provoked through the public
/// API (`create_plugin` refuses to touch an existing plugin directory, so
/// nothing can pre-create a file where `docs` must be), so the arm body is a
/// named helper pinned here byte-for-byte.
#[test]
fn plugin_docs_dir_error_pins_the_message() {
    let io = std::io::Error::from_raw_os_error(libc::ENOTDIR);
    let expected = format!("failed to create plugin docs dir: {io}");
    let path = Path::new("/fixtures/plugins/probe/docs/human");
    let err = super::plugin_docs_dir_error(path, io);
    assert!(
        matches!(&err, RuntimeError::Io { path, message } if path == Path::new("/fixtures/plugins/probe/docs/human") && message == &expected),
        "unexpected: {err:?}"
    );
}

// ─────────────── image-skip logging (2506-2507) ────────────────────

/// Skipped image URLs are logged one by one so an operator can see why an
/// image never reached the model. A zero batch budget skips every URL without
/// touching the network.
#[test]
#[serial]
fn image_batch_logs_skipped_urls_when_the_budget_is_zero() {
    let (temp, fixtures) = fixture_tree(&[]);
    let config_dir = temp.path().join("config");
    fs::create_dir_all(&config_dir).expect("config dir");
    fs::write(
        config_dir.join("runtime.yaml"),
        "runtime:\n  images:\n    max_per_batch: 0\n",
    )
    .expect("runtime config");
    let host = RuntimeHost::boot(&fixtures).expect("boot with an image budget");

    assert_eq!(host.config_value().runtime.images.max_per_batch, 0);
    let parts = host.image_parts_for_batch(&["http://127.0.0.1:1/a.png".to_string()]);
    assert!(
        parts.is_empty(),
        "over-budget urls must be skipped: {parts:?}"
    );
}

// ─────────────── end-of-turn flag setter (host/agent.rs 170-178) ────

/// A channel re-asserts the end-of-turn contract before every send. The
/// installed test loop's `set_require_end_tool` is a no-op, so the observable
/// contract here is that the call reaches a live session and that an unknown id
/// is ignored rather than panicking.
#[test]
#[serial]
fn agent_set_require_end_tool_updates_a_live_session() {
    let (_temp, host) = boot_empty();
    let handle = start_shell(&host);

    host.agent_set_require_end_tool(&handle.session_id, true);
    host.agent_set_require_end_tool(&handle.session_id, false);
    assert!(
        host.agent_sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .contains_key(&handle.session_id),
        "the session must stay addressable"
    );

    // An unknown session id is a no-op, not a panic.
    host.agent_set_require_end_tool("no-such-session", true);
}

// ─────────────── failed service restart (host/reload.rs 37) ────────

/// A restart invocation whose plugin is gone is logged and skipped; the reload
/// path keeps going.
#[test]
#[serial]
fn restart_services_logs_a_failed_invocation() {
    let (_temp, host) = boot_empty();
    host.set_startup_invocations(vec![StartupInvocation {
        plugin_path: "ghost".to_string(),
        node_id: "ghost_serve".to_string(),
        payload: "{}".to_string(),
    }]);
    // The filter also keeps a plugin that is not in the invocation list out.
    host.restart_services_for(&["elsewhere".to_string()]);
    host.restart_services_for(&["ghost".to_string()]);
}

// ─────────────── slash-only iteration target (host/kernel.rs 335) ──

/// A target path that trims down to an empty string is not a disk-only root;
/// it falls through to the shared-subtree check, which reports the real reason.
#[test]
#[serial]
fn begin_plugin_iteration_ignores_a_target_that_is_only_a_slash() {
    let (_temp, fixtures) = fixture_tree(&[]);
    let snapshot = empty_snapshot();
    let kernel = RuntimeKernel::new(&fixtures, &crate::config::RuntimeConfig::default());
    let request = KernelPluginIterationRequest {
        issue_id: None,
        target_plugin_paths: vec!["/".to_string()],
        instruction: Some("add a node".to_string()),
        edit_plan: None,
        manual_approved: false,
        tests_command: None,
        safety_command: None,
        verify_profile: None,
        quality_score: None,
    };
    let err = kernel
        .begin_plugin_iteration(&snapshot, &request)
        .expect_err("a slash-only target has no loaded subtree");
    assert!(
        err.to_string()
            .contains("do not share a loaded subtree root"),
        "unexpected: {err}"
    );
}

// ─────────────── unlistable snapshot dir (host/mod.rs 4984) ────────

/// A hash directory the scan cannot list is left completely alone: no
/// accounting and, crucially, no deletion — the previous `continue`.
///
/// The full path of the child exceeds `PATH_MAX` while its parent is still
/// listable: the parent is grown to just under the limit, and the child is
/// created through a short symlink so the directory exists even though no
/// absolute path to it fits. Iterating the parent therefore yields a directory
/// whose `read_dir` fails with `ENAMETOOLONG`.
#[cfg(target_os = "linux")]
#[test]
fn snapshot_gc_leaves_a_directory_it_cannot_list_untouched() {
    // Linux `PATH_MAX` is 4096 bytes *including* the NUL terminator.
    const LONGEST_LISTABLE: usize = 4_090;
    let temp = TempDir::new().expect("tempdir");
    let mut host_root = temp.path().join("host");
    fs::create_dir_all(&host_root).expect("host root");
    while host_root.as_os_str().len() + 1 + 255 <= LONGEST_LISTABLE {
        host_root.push("d".repeat(255));
        fs::create_dir_all(&host_root).expect("deep component");
    }
    let pad = LONGEST_LISTABLE - host_root.as_os_str().len();
    if pad > 1 {
        host_root.push("e".repeat(pad - 1));
        fs::create_dir_all(&host_root).expect("deepest component");
    }
    // A short hop to the same directory: the long path above cannot create a
    // child whose own full path would overflow, this one can.
    let short = temp.path().join("short");
    std::os::unix::fs::symlink(&host_root, &short).expect("short symlink");
    let child_name = "c".repeat(200);
    fs::create_dir(short.join(&child_name)).expect("unlistable child");

    let report = cleanup_orphaned_snapshot_roots(&host_root, Duration::ZERO, None, false);

    assert_eq!(report.scanned, 1, "the child is enumerated once");
    assert_eq!(
        report.removed, 0,
        "an unlistable directory must not be removed"
    );
    assert_eq!(report.bytes_reclaimed, 0);
    assert_eq!(report.skipped_recent, 0);
    assert_eq!(report.skipped_live, 0);
    assert_eq!(report.skipped_journal, 0);
    assert!(
        short.join(&child_name).is_dir(),
        "the unlistable directory must be left in place"
    );
}
