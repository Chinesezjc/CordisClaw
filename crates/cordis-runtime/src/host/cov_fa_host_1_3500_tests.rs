use super::{runtime_snapshot_from_output, RuntimeHost, RuntimeKernel, RuntimeSnapshot};
use crate::core::error::RuntimeError;
use crate::core::models::{ArtifactKind, NodeOutcome};
use crate::kernel::plugin_iteration::{
    KernelPluginIssueSource, KernelPluginIssueStatus, KernelPluginIterationRequest,
};
use cordis_plugin_sdk::{node_doc, plugin_docs, AbiFingerprint, NodeDoc};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

// A two-node "producer → consumer" plugin whose registered net carries an
// upstream edge, but whose node_registry — handed to the snapshot — is
// MISSING the producer node. Executing the consumer therefore fires the
// producer transition, whose `node_registry.get(..)` returns None, driving
// `execute_registered_target`'s registry-missing arm (missing_registry_trace).
fn snapshot_with_ghost_upstream_node() -> RuntimeSnapshot {
    let plugin_path = "ghostplug".to_string();
    // Producer emits "shared"; consumer reads "shared" → a data edge
    // producer→consumer is inferred by the graph builder.
    let producer: NodeDoc = node_doc(
        "producer",
        "emits the shared field",
        json!({ "type": "object", "properties": {} }),
        json!({ "type": "object", "properties": { "shared": { "type": "number" } } }),
        &[],
        &[],
    );
    let consumer: NodeDoc = node_doc(
        "consumer",
        "reads the shared field",
        json!({ "type": "object", "properties": { "shared": { "type": "number" } } }),
        json!({ "type": "object", "properties": {} }),
        &[],
        &[],
    );
    let docs = plugin_docs(
        plugin_path.clone(),
        plugin_path.clone(),
        "0.1.0",
        None,
        vec![producer.clone(), consumer.clone()],
        None,
    );

    let plugin_registry = crate::plugin::registry::PluginRegistry::default();
    plugin_registry.insert_loaded(
        plugin_path.clone(),
        None,
        true,
        BTreeSet::new(),
        docs.clone(),
        PathBuf::from("/nonexistent/cordis-ghost.dylib"),
        ArtifactKind::Dylib,
        AbiFingerprint::current_build("crate_ghost_v1", "api_v2"),
        None,
    );

    // FULL node registry (both nodes) drives the graph/net construction so
    // the net contains the producer→consumer edge.
    let mut full_nodes = crate::plugin::registry::NodeRegistry::default();
    full_nodes
        .register_from_docs(&plugin_path, &docs)
        .expect("register both nodes for graph build");
    let graph_registry = crate::service::graph_registry::GraphRegistry::from_registries(
        &plugin_registry,
        &full_nodes,
    );

    // PARTIAL node registry (consumer only) is what the snapshot carries,
    // so resolving the upstream producer transition fails at runtime.
    let consumer_only_docs = plugin_docs(
        plugin_path.clone(),
        plugin_path.clone(),
        "0.1.0",
        None,
        vec![consumer],
        None,
    );
    let mut partial_nodes = crate::plugin::registry::NodeRegistry::default();
    partial_nodes
        .register_from_docs(&plugin_path, &consumer_only_docs)
        .expect("register consumer only");

    let doc_registry =
        crate::service::doc_registry::DocRegistry::from_plugin_registry(&plugin_registry);

    runtime_snapshot_from_output(
        crate::plugin::loader::LoadOutput {
            execution_id: "snapshot-ghost-upstream".to_string(),
            plugin_registry,
            node_registry: partial_nodes,
            doc_registry,
            graph_registry,
            context: crate::context::RuntimeContext::default(),
            metrics: crate::plugin::loader::LoaderMetrics::default(),
        },
        PathBuf::from("/tmp/cordis-ghost-upstream-staged"),
        BTreeMap::new(),
    )
}

// A plugin's system hint is rendered with its configured settings, so
// deployment-specific values (qq's own uin) live in config/plugins/*.yaml
// instead of the plugin source or a per-message line.
#[test]
fn snapshot_renders_system_hint_with_plugin_settings() {
    let mut plugin_settings = BTreeMap::new();
    plugin_settings.insert(
        "qq".to_string(),
        crate::config::PluginConfigFile {
            plugin: "qq".to_string(),
            enabled: true,
            settings: json!({ "bot_uin": "3832224285" }),
        },
    );
    let snapshot = runtime_snapshot_from_output(
        crate::plugin::loader::LoadOutput {
            execution_id: "snap-hint".to_string(),
            plugin_registry: crate::plugin::registry::PluginRegistry::default(),
            node_registry: crate::plugin::registry::NodeRegistry::default(),
            doc_registry: crate::service::doc_registry::DocRegistry::default(),
            graph_registry: crate::service::graph_registry::GraphRegistry::default(),
            context: crate::context::RuntimeContext::default(),
            metrics: crate::plugin::loader::LoaderMetrics::default(),
        },
        PathBuf::from("/tmp/cordis-hint-staged"),
        plugin_settings,
    );
    assert_eq!(
        snapshot.render_system_hint("qq", "you are qq user {setting:bot_uin}"),
        "you are qq user 3832224285"
    );
    // Unconfigured plugin: the placeholder stays visible.
    assert_eq!(
        snapshot.render_system_hint("feishu", "id={setting:bot_uin}"),
        "id={setting:bot_uin}"
    );
}

// The snapshot carries each plugin's `settings` and hands them to the
// invoker. A JSON-artifact plugin (`cat`) proves what the plugin receives:
// a configured dylib plugin would get `settings` merged into its payload,
// while a process plugin still sees its stdin verbatim.
#[test]
fn snapshot_invoke_passes_configured_plugin_settings() {
    let plugin_path = "json/echo".to_string();
    let node_id = "echo_node".to_string();
    let docs = plugin_docs(
        plugin_path.clone(),
        plugin_path.clone(),
        "0.1.0",
        None,
        vec![node_doc(
            &node_id,
            "echoes the payload",
            json!({ "type": "object" }),
            json!({ "type": "object" }),
            &[],
            &[],
        )],
        None,
    );
    let dir = TempDir::new().expect("tempdir");
    let artifact = dir.path().join("plugin.json");
    fs::write(&artifact, "{}").expect("write artifact stub");

    let plugin_registry = crate::plugin::registry::PluginRegistry::default();
    plugin_registry.insert_loaded(
        plugin_path.clone(),
        None,
        true,
        BTreeSet::new(),
        docs,
        artifact,
        ArtifactKind::Json,
        AbiFingerprint::current_build("crate_echo_v1", "api_v2"),
        Some(crate::core::models::PluginExecution::Process {
            command: "/bin/cat".to_string(),
            args: vec![],
        }),
    );
    let node_registry = crate::plugin::registry::NodeRegistry::default();
    let doc_registry =
        crate::service::doc_registry::DocRegistry::from_plugin_registry(&plugin_registry);
    let graph_registry = crate::service::graph_registry::GraphRegistry::from_registries(
        &plugin_registry,
        &node_registry,
    );

    let with_settings = |settings: BTreeMap<String, crate::config::PluginConfigFile>| {
        runtime_snapshot_from_output(
            crate::plugin::loader::LoadOutput {
                execution_id: "snap-settings".to_string(),
                plugin_registry: plugin_registry.clone(),
                node_registry: node_registry.clone(),
                doc_registry: doc_registry.clone(),
                graph_registry: graph_registry.clone(),
                context: crate::context::RuntimeContext::default(),
                metrics: crate::plugin::loader::LoaderMetrics::default(),
            },
            PathBuf::from("/tmp/cordis-settings-staged"),
            settings,
        )
    };

    // Configured plugin: the invoker is handed its settings block, and the
    // process plugin still receives exactly the caller's stdin.
    let mut configured = BTreeMap::new();
    configured.insert(
        plugin_path.clone(),
        crate::config::PluginConfigFile {
            plugin: plugin_path.clone(),
            enabled: true,
            settings: json!({ "read_roots": ["/tmp/configured-root"] }),
        },
    );
    let echoed = with_settings(configured)
        .invoke(&plugin_path, &node_id, r#"{"hello":"world"}"#.to_string())
        .expect("cat echoes the payload");
    assert_eq!(echoed.payload, r#"{"hello":"world"}"#);

    // Unconfigured plugin: no settings lookup hit, payload unchanged.
    let plain = with_settings(BTreeMap::new())
        .invoke(&plugin_path, &node_id, r#"{"hello":"world"}"#.to_string())
        .expect("cat echoes the payload");
    assert_eq!(plain.payload, r#"{"hello":"world"}"#);
}

#[test]
fn execute_records_missing_registry_trace_for_ghost_upstream_node() {
    let snapshot = snapshot_with_ghost_upstream_node();
    let result = snapshot
        .execute_registered_target("ghostplug::consumer", json!({ "shared": 1 }))
        .expect("execute returns a result even with a ghost upstream node");

    // The upstream producer transition resolves to no registry node, so
    // the closure records the blank-plugin missing-registry trace.
    let trace = result
        .traces
        .get("ghostplug::producer")
        .expect("ghost upstream node should have a trace entry");
    assert_eq!(trace.outcome, Some(NodeOutcome::Failure));
    assert_eq!(trace.plugin_path, "");
    assert_eq!(trace.node_id, "");
    assert_eq!(
        trace.error.as_deref(),
        Some("node missing from registry"),
        "missing_registry_trace error text must be verbatim"
    );
    assert!(trace.request_payload.is_none());
}

// begin_plugin_iteration (private) + select_issue_for_request comparator.
// A hermetic kernel with two OPEN issues of different source priority; a
// no-issue-id request auto-selects, running the sort comparator, and the
// highest-priority issue (LoadFailure=0) is chosen + flipped to Running.
#[test]
fn begin_plugin_iteration_auto_selects_highest_priority_issue() {
    let temp = TempDir::new().expect("tempdir");
    let config = crate::config::RuntimeConfig::default();
    let kernel = RuntimeKernel::new(temp.path(), &config);

    // Observed in "wrong" order: low-urgency first, high-urgency second.
    let _invoke =
        kernel.observe_plugin_issue(KernelPluginIssueSource::InvokeFailure, "expr", "invoke");
    let load = kernel.observe_plugin_issue(KernelPluginIssueSource::LoadFailure, "shell", "load");

    let snapshot = snapshot_with_ghost_upstream_node();
    let prepared = kernel
        .begin_plugin_iteration(
            &snapshot,
            &KernelPluginIterationRequest {
                issue_id: None,
                target_plugin_paths: Vec::new(),
                instruction: Some("auto".to_string()),
                edit_plan: None,
                manual_approved: false,
                tests_command: None,
                safety_command: None,
                verify_profile: None,
                quality_score: None,
            },
        )
        .expect("begin should auto-select an issue");
    // LoadFailure (priority 0) wins the comparator over InvokeFailure (3).
    assert_eq!(prepared.root_plugin_path, "shell");

    // The selected issue was flipped to Running (the `and_modify` arm).
    let running = kernel
        .plugin_issues()
        .into_iter()
        .find(|i| i.issue_id == load.issue_id)
        .expect("load issue present");
    assert_eq!(running.status, KernelPluginIssueStatus::Running);
}

// begin_plugin_iteration active-guard arm: a second begin while one is
// already active returns PluginIterationActive.
#[test]
fn begin_plugin_iteration_rejects_second_concurrent_iteration() {
    let temp = TempDir::new().expect("tempdir");
    let config = crate::config::RuntimeConfig::default();
    let kernel = RuntimeKernel::new(temp.path(), &config);
    let snapshot = snapshot_with_ghost_upstream_node();

    let req = |instruction: &str| KernelPluginIterationRequest {
        issue_id: None,
        target_plugin_paths: vec!["ghostplug".to_string()],
        instruction: Some(instruction.to_string()),
        edit_plan: None,
        manual_approved: false,
        tests_command: None,
        safety_command: None,
        verify_profile: None,
        quality_score: None,
    };

    // First begin succeeds and marks the iteration active.
    let _first = kernel
        .begin_plugin_iteration(&snapshot, &req("first"))
        .expect("first iteration prepares");
    // Second begin hits the active-guard early return.
    let err = kernel
        .begin_plugin_iteration(&snapshot, &req("second"))
        .expect_err("a second concurrent iteration must be rejected");
    assert!(matches!(err, RuntimeError::PluginIterationActive { .. }));
}

// begin_plugin_iteration: when the selected issue's root_plugin_path no
// longer matches any plugin in the snapshot registry, the subtree filter
// yields nothing and the not-found InvalidArgument fires. (Explicit
// target paths hit determine_root_plugin_path first, so the issue route
// is the reachable way into this arm.)
#[test]
fn begin_plugin_iteration_rejects_vanished_issue_subtree() {
    let temp = TempDir::new().expect("tempdir");
    let config = crate::config::RuntimeConfig::default();
    let kernel = RuntimeKernel::new(temp.path(), &config);
    let issue = kernel.observe_plugin_issue(
        KernelPluginIssueSource::LoadFailure,
        "vanished-plugin",
        "plugin was unloaded after the issue was filed",
    );
    let snapshot = snapshot_with_ghost_upstream_node();
    let req = KernelPluginIterationRequest {
        issue_id: Some(issue.issue_id.clone()),
        // Non-empty targets keep root_mode off; the issue's vanished
        // root then filters the registry down to nothing.
        target_plugin_paths: vec!["ghostplug".to_string()],
        instruction: Some("noop".to_string()),
        edit_plan: None,
        manual_approved: false,
        tests_command: None,
        safety_command: None,
        verify_profile: None,
        quality_score: None,
    };
    let err = kernel
        .begin_plugin_iteration(&snapshot, &req)
        .expect_err("vanished issue subtree must be rejected");
    assert!(
        matches!(&err, RuntimeError::InvalidArgument { message }
                if message == "plugin subtree not found for vanished-plugin"),
        "got: {err:?}"
    );
}

// An issue with no plugin root must not surface as the useless
// "plugin subtree not found for " — the agent needs to be told what to
// pass instead (this is what the 2026-10-28 imggen attempts hit).
#[test]
fn begin_plugin_iteration_explains_a_missing_issue_root() {
    let temp = TempDir::new().expect("tempdir");
    let config = crate::config::RuntimeConfig::default();
    let kernel = RuntimeKernel::new(temp.path(), &config);
    let issue = kernel.observe_plugin_issue(
        KernelPluginIssueSource::LoadFailure,
        "",
        "issue without a resolvable plugin root",
    );
    let snapshot = snapshot_with_ghost_upstream_node();
    let req = KernelPluginIterationRequest {
        issue_id: Some(issue.issue_id.clone()),
        target_plugin_paths: vec![String::new()],
        instruction: Some("noop".to_string()),
        edit_plan: None,
        manual_approved: false,
        tests_command: None,
        safety_command: None,
        verify_profile: None,
        quality_score: None,
    };
    let err = kernel
        .begin_plugin_iteration(&snapshot, &req)
        .expect_err("an issue without a plugin root must be rejected");
    assert!(
        matches!(&err, RuntimeError::InvalidArgument { message }
                if message.contains("pass \"/<plugin>\"") && message.contains("whole workspace")),
        "got: {err:?}"
    );
}

// take_blocked_iteration: unknown id surfaces StatusNotFound (the only
// remaining error path now that the verdict check is an invariant).
// host_io_log_line: the single formatting seam behind every best-effort
// I/O failure log in auto-save/shutdown/delete paths.
#[test]
fn host_io_log_line_formats_stage_subject_and_error() {
    let err = std::io::Error::other("disk gone");
    let line = RuntimeHost::host_io_log_line("auto-save", "failed to create sessions dir /x", &err);
    assert_eq!(
        line,
        "[auto-save] failed to create sessions dir /x: disk gone"
    );
}

/// The reconstruct-failure line in `detect_crash_and_recover`'s hydration
/// chain. `AgentSession::from_snapshot` only fails when reqwest cannot build
/// an HTTP client — no on-disk snapshot can force that (every field
/// round-trips and reqwest accepts any timeout, including 0 and u64::MAX) —
/// so the message shape is pinned here rather than by a fixture.
#[test]
fn crash_recovery_reconstruct_log_line_names_the_snapshot_path() {
    let err = RuntimeError::LlmRequestFailed {
        message: "failed to rebuild agent HTTP client from snapshot: nope".to_string(),
    };
    let line = super::crash_recovery_reconstruct_log_line(
        std::path::Path::new("/data/sessions/abc.json"),
        &err,
    );
    assert_eq!(
        line,
        "[crash-recovery] reconstruct failed for /data/sessions/abc.json: \
             LLM request failed: failed to rebuild agent HTTP client from snapshot: nope"
    );
}

// auto_save_session error arms via fault injection: data/sessions blocked
// by a regular file at data/ makes create_dir_all fail; a read-only
// sessions dir makes the tmp write fail; a directory squatting on the
// target makes rename fail. All are best-effort (must not panic).
#[cfg(unix)]
#[test]
fn auto_save_and_delete_session_arms_survive_fs_faults() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::TempDir::new().expect("tempdir");
    let fixtures = temp.path().join("fixtures");
    let artifacts = fixtures.join("artifacts");
    fs::create_dir_all(&artifacts).expect("create artifacts dir");
    fs::write(
            artifacts.join("index.json"),
            r#"{"schema_version":2,"generated_at":"2026-07-24T00:00:00Z","topo_order":[],"entries":[]}"#,
        )
        .expect("write empty index");
    let host = RuntimeHost::boot(&fixtures).expect("boot on empty index");
    let config = crate::config::LlmApiConfig {
        provider: "deepseek".to_string(),
        base_url: "http://127.0.0.1:1/v1".to_string(),
        api_key: Some("test".to_string()),
        model: "m".to_string(),
        ..crate::config::LlmApiConfig::default()
    };
    let session = crate::agent_loop::agent_loop()
        .expect("loop installed")
        .create(config, "runtime_shell")
        .expect("build session");

    // Arm 1: create_dir_all failure — put a FILE where data/sessions goes.
    let data_dir = host.data_dir();
    fs::create_dir_all(&data_dir).unwrap();
    fs::write(data_dir.join("sessions"), b"squatter").unwrap();
    host.auto_save_session("s1", session.as_ref()); // must log-and-return, not panic
    fs::remove_file(data_dir.join("sessions")).unwrap();

    // Arm 2: tmp write failure — sessions dir exists but is read-only.
    let sessions_dir = data_dir.join("sessions");
    fs::create_dir_all(&sessions_dir).unwrap();
    let ro = fs::Permissions::from_mode(0o555);
    fs::set_permissions(&sessions_dir, ro).unwrap();
    if unsafe { libc::geteuid() } != 0 {
        host.auto_save_session("s2", session.as_ref());
    }
    fs::set_permissions(&sessions_dir, fs::Permissions::from_mode(0o700)).unwrap();

    // Arm 3: rename failure — a DIRECTORY squats on the target filename.
    fs::create_dir_all(sessions_dir.join("s3.json")).unwrap();
    host.auto_save_session("s3", session.as_ref());
    fs::remove_dir(sessions_dir.join("s3.json")).unwrap();

    // Arm 4: delete failure — target exists but parent becomes read-only.
    fs::write(sessions_dir.join("s4.json"), b"{}").unwrap();
    fs::set_permissions(&sessions_dir, fs::Permissions::from_mode(0o555)).unwrap();
    if unsafe { libc::geteuid() } != 0 {
        host.delete_session_snapshot("s4");
    }
    fs::set_permissions(&sessions_dir, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn backup_artifacts_into_rollback_absorbs_and_rejects_mismatched_root() {
    use crate::kernel::plugin_iteration::PluginEditRollback;
    let temp = tempfile::TempDir::new().expect("tempdir");
    let fixtures = temp.path().join("fixtures");
    let artifacts = fixtures.join("artifacts");
    fs::create_dir_all(&artifacts).expect("mk artifacts");
    // artifact_paths_to_backup only collects `artifacts/<name>.so`.
    fs::write(artifacts.join("demo.so"), b"\x7fELF").expect("write artifact");
    fs::write(
            artifacts.join("index.json"),
            r#"{"schema_version":2,"generated_at":"t","topo_order":["demo"],"entries":[{"plugin_path":"demo","version":"0.1.0","abi_fingerprint":{"crate_hash":"c","api_hash":"a"},"artifact_path":"demo.json","sha256":"x","built_at":"t","parent":null,"required":true,"grants_from_parent":[],"docs":{"plugin_id":"demo","plugin_path":"demo","plugin_version":"0.1.0","abi_version":2,"nodes":[]},"exports":[],"execution":null,"artifact_kind":"json","local_path_deps":[],"input_probe":[],"build_fingerprint":""}]}"#,
        )
        .expect("write index");

    // Success: same workspace root absorbs the artifact backups.
    let mut ok_rb = PluginEditRollback::empty(fixtures.display().to_string());
    super::backup_artifacts_into_rollback(&fixtures, "demo", &mut ok_rb)
        .expect("absorb into same-root rollback");

    // Failure: a rollback anchored at a different workspace refuses.
    let mut bad_rb = PluginEditRollback::empty("/elsewhere".to_string());
    let err = super::backup_artifacts_into_rollback(&fixtures, "demo", &mut bad_rb)
        .expect_err("mismatched workspace root must fail absorb");
    assert!(
        matches!(err, RuntimeError::Invariant { .. }),
        "got: {err:?}"
    );
}

#[test]
fn take_blocked_iteration_unknown_id_is_not_found() {
    let temp = TempDir::new().expect("tempdir");
    let config = crate::config::RuntimeConfig::default();
    let kernel = RuntimeKernel::new(temp.path(), &config);
    let err = kernel
        .take_blocked_iteration("nope")
        .expect_err("missing id must error");
    assert!(matches!(
        err,
        RuntimeError::PluginIterationStatusNotFound { .. }
    ));
}
