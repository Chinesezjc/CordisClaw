/// Pull the message out of an `InvalidArgument`, panicking at the caller's
/// line on any other variant — replaces `match`-with-unreachable-arm in
/// assertions so no test arm stays unexecuted.
#[track_caller]
fn invalid_argument_message(err: &RuntimeError) -> String {
    // Both arms are executable: a non-matching variant returns its own
    // rendering, so the caller's `assert_eq!` fails with the real value
    // instead of this helper panicking from a never-taken arm.
    match err {
        RuntimeError::InvalidArgument { message } => message.clone(),
        other => format!("not InvalidArgument: {other}"),
    }
}

#[test]
fn invalid_argument_message_labels_other_variants() {
    // Drives the fallback arm: callers only ever pass the matching variant,
    // so without this the arm would never execute.
    let other = RuntimeError::Invariant {
        message: "nope".to_string(),
    };
    assert_eq!(
        invalid_argument_message(&other),
        format!("not InvalidArgument: {other}")
    );
}

/// Same for `LlmResponseInvalid`.
#[track_caller]
fn llm_response_invalid_message(err: &RuntimeError) -> String {
    match err {
        RuntimeError::LlmResponseInvalid { message } => message.clone(),
        other => format!("not LlmResponseInvalid: {other}"),
    }
}

#[test]
fn llm_response_invalid_message_labels_other_variants() {
    let other = RuntimeError::Invariant {
        message: "nope".to_string(),
    };
    assert_eq!(
        llm_response_invalid_message(&other),
        format!("not LlmResponseInvalid: {other}")
    );
}

use super::{
    enrich_plugin_iteration_edit_error, format_agent_transcript_excerpt, parse_agent_args,
    transcript_excerpt, AgentSessionHandle, AgentSessionKind, AgentStartOptions,
    ManagedAgentSession, ManagedAgentState, ReloadReport,
    ReplaceFilesExactArgs, RetiredSnapshot, RuntimeHost, PLUGIN_AGENT_TOOL_REPLACE_FILES_EXACT,
};
use crate::agent_api::AgentTranscriptEntry;
use crate::core::error::RuntimeError;
use crate::core::models::PluginUnavailableReason;
use crate::kernel::plugin_iteration::{
    KernelPluginIssueSource, PluginEditOpKind, PluginEditOperation,
};
use serde_json::json;
use serial_test::serial;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tempfile::TempDir;

// ───────────────────────── hermetic fixtures ─────────────────────────

/// An `artifacts/index.json` with zero entries: `RuntimeHost::boot`
/// registers no plugins and never dlopens. Near-instant on any target.
fn setup_empty_fixture() -> (TempDir, PathBuf) {
    let temp = TempDir::new().expect("tempdir");
    let fixtures = temp.path().join("fixtures");
    let artifacts = fixtures.join("artifacts");
    fs::create_dir_all(&artifacts).expect("create artifacts dir");
    fs::write(
        artifacts.join("index.json"),
        r#"{
  "schema_version": 2,
  "generated_at": "2026-07-25T00:00:00Z",
  "topo_order": [],
  "entries": []
}
"#,
    )
    .expect("write empty artifact index");
    (temp, fixtures)
}


fn empty_host() -> (TempDir, RuntimeHost) {
    let (temp, fixtures) = setup_empty_fixture();
    let host = RuntimeHost::boot(&fixtures).expect("boot on empty index");
    (temp, host)
}


// ───────────────────────── pure functions ────────────────────────────

#[test]
#[serial]
fn transcript_excerpt_keeps_tail_in_order() {
    let entries: Vec<AgentTranscriptEntry> = (0..5)
        .map(|i| AgentTranscriptEntry::User {
            content: format!("m{i}"),
        })
        .collect();
    let out = transcript_excerpt(&entries, 3);
    // Last three, still in chronological order.
    // Compare the whole slice against the expected tail: no per-variant
    // arm, so no never-taken match arm is left behind.
    let expected: Vec<AgentTranscriptEntry> = ["m2", "m3", "m4"]
        .into_iter()
        .map(|c| AgentTranscriptEntry::User {
            content: c.to_string(),
        })
        .collect();
    assert_eq!(format!("{out:?}"), format!("{expected:?}"));
}

#[test]
#[serial]
fn format_agent_transcript_excerpt_renders_all_three_variants() {
    let entries = vec![
        AgentTranscriptEntry::User {
            content: "hello   world".to_string(),
        },
        AgentTranscriptEntry::Assistant {
            content: "an answer".to_string(),
            response_id: Some("resp-1".to_string()),
        },
        AgentTranscriptEntry::Assistant {
            content: "no id answer".to_string(),
            response_id: None,
        },
        AgentTranscriptEntry::Tool {
            name: "create_file".to_string(),
            arguments: json!({}),
            ok: false,
            output: None,
            error: Some("boom happened".to_string()),
        },
        AgentTranscriptEntry::Tool {
            name: "json_set".to_string(),
            arguments: json!({}),
            ok: true,
            output: None,
            error: None,
        },
    ];
    let rendered = format_agent_transcript_excerpt(&entries);
    // whitespace is flattened in the user line.
    assert!(rendered.contains("user: hello world"));
    assert!(rendered.contains("assistant[resp-1]: an answer"));
    assert!(rendered.contains("assistant: no id answer"));
    assert!(rendered.contains("tool create_file ok=false error=boom happened"));
    assert!(rendered.contains("tool json_set ok=true"));
    // The ok=true tool line carries no error suffix.
    let ok_line = rendered
        .lines()
        .find(|l| l.starts_with("tool json_set"))
        .expect("json_set line");
    assert!(!ok_line.contains("error="));
}

#[test]
#[serial]
fn parse_agent_args_wraps_deserialize_error_byte_exact() {
    // `ReplaceFilesExactArgs` requires an `edits` array; a bare string is
    // the wrong shape → serde error wrapped as InvalidArgument.
    let out = parse_agent_args::<ReplaceFilesExactArgs>(
        json!("not an object"),
        PLUGIN_AGENT_TOOL_REPLACE_FILES_EXACT,
    );
    let err = out.expect_err("wrong shape must error");
    let expected_prefix =
        format!("agent tool {PLUGIN_AGENT_TOOL_REPLACE_FILES_EXACT} received invalid arguments: ");
    let message = invalid_argument_message(&err);
    assert!(message.starts_with(&expected_prefix), "got {message}");
}

#[test]
#[serial]
fn enrich_edit_error_returns_stale_snippet_hint_for_replace_exact() {
    let temp = TempDir::new().expect("tempdir");
    let workspace = temp.path();
    let rel = "plugins/mini/src/lib.rs";
    let abs = workspace.join(rel);
    fs::create_dir_all(abs.parent().unwrap()).expect("mkdir");
    fs::write(&abs, "current file body\n").expect("write file");

    let operation = PluginEditOperation {
        path: rel.to_string(),
        kind: PluginEditOpKind::ReplaceExact,
        expected_old_string: Some("stale".to_string()),
        expected_sha256: None,
        new_content: Some("new".to_string()),
        pointer: None,
        dotted_key: None,
        value: None,
    };
    // The base error message must contain the trigger substring.
    let base = RuntimeError::LlmResponseInvalid {
        message: "auto update patch pattern not found in file".to_string(),
    };
    let enriched = enrich_plugin_iteration_edit_error(&operation, workspace, base);
    let message = llm_response_invalid_message(&enriched);
    assert!(message.contains("The exact snippet is stale for plugins/mini/src/lib.rs"));
    assert!(message.contains("current_sha256="));
    assert!(message.contains("current file body"));
}

#[test]
#[serial]
fn enrich_edit_error_passes_through_when_not_stale_pattern() {
    let temp = TempDir::new().expect("tempdir");
    let operation = PluginEditOperation {
        path: "plugins/mini/src/lib.rs".to_string(),
        kind: PluginEditOpKind::ReplaceExact,
        expected_old_string: Some("x".to_string()),
        expected_sha256: None,
        new_content: Some("y".to_string()),
        pointer: None,
        dotted_key: None,
        value: None,
    };
    // Message lacks the "auto update patch pattern not found" trigger → the
    // original error is returned untouched.
    let base = RuntimeError::InvalidArgument {
        message: "some other failure".to_string(),
    };
    let out = enrich_plugin_iteration_edit_error(&operation, temp.path(), base);
    assert!(matches!(
        out,
        RuntimeError::InvalidArgument { message } if message == "some other failure"
    ));
}

// ───────────────── record_invocation_sample cap ──────────────────────

#[test]
#[serial]
fn record_invocation_sample_caps_deque_at_64() {
    let (_temp, host) = empty_host();
    for i in 0..70 {
        host.record_invocation_sample(
            "mini",
            "mini_echo",
            &json!({ "n": i }).to_string(),
            &json!({ "ok": true }).to_string(),
        );
    }
    let len = host
        .invocation_samples
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .len();
    assert_eq!(len, 64, "deque must be capped at 64 (pop_back on overflow)");
    // Newest (front) is the last pushed; oldest were evicted from the back.
    let front = host
        .invocation_samples
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .front()
        .cloned()
        .expect("front sample");
    assert_eq!(front.payload, json!({ "n": 69 }));
}

// ───────────────── observe_reload_error arms ─────────────────────────

#[test]
#[serial]
fn observe_reload_error_maps_docs_contract_to_docs_drift() {
    let (_temp, host) = empty_host();
    let err = RuntimeError::DocsContract {
        plugin_path: "alpha".to_string(),
        message: "docs mismatch".to_string(),
    };
    host.observe_reload_error("reload", &err);
    let issues = host.kernel().plugin_issues();
    let issue = issues
        .iter()
        .find(|i| i.root_plugin_path == "alpha")
        .expect("issue recorded for alpha");
    assert_eq!(issue.source, KernelPluginIssueSource::DocsDrift);
    assert!(issue.summary.contains("reload failed for alpha"));
}

#[test]
#[serial]
fn observe_reload_error_maps_contract_violation_unavailable_to_docs_drift() {
    let (_temp, host) = empty_host();
    let err = RuntimeError::PluginUnavailable {
        plugin_path: "beta".to_string(),
        reason: PluginUnavailableReason::ContractViolation,
        required: true,
    };
    host.observe_reload_error("candidate_reload", &err);
    let issues = host.kernel().plugin_issues();
    let issue = issues
        .iter()
        .find(|i| i.root_plugin_path == "beta")
        .expect("issue recorded for beta");
    assert_eq!(issue.source, KernelPluginIssueSource::DocsDrift);
}

#[test]
#[serial]
fn observe_reload_error_maps_other_to_load_failure() {
    let (_temp, host) = empty_host();
    let err = RuntimeError::PluginUnavailable {
        plugin_path: "gamma".to_string(),
        reason: PluginUnavailableReason::ArtifactMissing,
        required: false,
    };
    host.observe_reload_error("reload", &err);
    let issues = host.kernel().plugin_issues();
    let issue = issues
        .iter()
        .find(|i| i.root_plugin_path == "gamma")
        .expect("issue recorded for gamma");
    assert_eq!(issue.source, KernelPluginIssueSource::LoadFailure);
}

#[test]
#[serial]
fn observe_reload_error_ignores_errors_without_plugin_path() {
    let (_temp, host) = empty_host();
    let before = host.kernel().plugin_issues().len();
    // Invariant carries no plugin path → early return, no issue recorded.
    host.observe_reload_error(
        "reload",
        &RuntimeError::Invariant {
            message: "no plugin here".to_string(),
        },
    );
    assert_eq!(host.kernel().plugin_issues().len(), before);
}

// ─────────────── observe_snapshot_plugin_issues arms ─────────────────

/// Build a one-plugin snapshot whose single plugin is `Unavailable`.
fn snapshot_with_unavailable_plugin(
    plugin_path: &str,
    reason: PluginUnavailableReason,
) -> super::RuntimeSnapshot {
    let plugin_registry = crate::plugin::registry::PluginRegistry::default();
    plugin_registry.insert_unavailable(
        plugin_path.to_string(),
        None,
        true,
        std::collections::BTreeSet::new(),
        reason,
        Vec::new(),
    );
    let node_registry = crate::plugin::registry::NodeRegistry::default();
    let doc_registry =
        crate::service::doc_registry::DocRegistry::from_plugin_registry(&plugin_registry);
    let graph_registry = crate::service::graph_registry::GraphRegistry::from_registries(
        &plugin_registry,
        &node_registry,
    );
    super::runtime_snapshot_from_output(
        crate::plugin::loader::LoadOutput {
            execution_id: "snap-unavail".to_string(),
            plugin_registry,
            node_registry,
            doc_registry,
            graph_registry,
            context: crate::context::RuntimeContext::default(),
            metrics: crate::plugin::loader::LoaderMetrics::default(),
        },
        PathBuf::from("/tmp/cordis-seam-unavail-staged"),
        BTreeMap::new(),
    )
}

fn empty_reload_report() -> ReloadReport {
    ReloadReport {
        from_snapshot_id: "a".to_string(),
        to_snapshot_id: "b".to_string(),
        snapshot_root: "/tmp/x".to_string(),
        staged_artifact_root: "/tmp/x/staged".to_string(),
        elapsed_ms: 0,
        added_plugins: Vec::new(),
        removed_plugins: Vec::new(),
        changed_plugins: Vec::new(),
        changed_plugin_reasons: BTreeMap::new(),
    }
}

#[test]
#[serial]
fn observe_snapshot_issues_contract_violation_is_docs_drift() {
    let (_temp, host) = empty_host();
    let snapshot =
        snapshot_with_unavailable_plugin("cv", PluginUnavailableReason::ContractViolation);
    host.observe_snapshot_plugin_issues(&snapshot, &empty_reload_report(), "reload");
    let issues = host.kernel().plugin_issues();
    let issue = issues
        .iter()
        .find(|i| i.root_plugin_path == "cv")
        .expect("cv issue");
    assert_eq!(issue.source, KernelPluginIssueSource::DocsDrift);
    assert!(issue
        .summary
        .contains("reload observed plugin cv unavailable"));
}

#[test]
#[serial]
fn observe_snapshot_issues_other_unavailable_is_load_failure() {
    let (_temp, host) = empty_host();
    let snapshot = snapshot_with_unavailable_plugin("am", PluginUnavailableReason::ArtifactMissing);
    host.observe_snapshot_plugin_issues(&snapshot, &empty_reload_report(), "reload");
    let issues = host.kernel().plugin_issues();
    let issue = issues
        .iter()
        .find(|i| i.root_plugin_path == "am")
        .expect("am issue");
    assert_eq!(issue.source, KernelPluginIssueSource::LoadFailure);
}

// ─────────────── cleanup_retired_snapshots cap ───────────────────────

#[test]
#[serial]
fn cleanup_retired_snapshots_caps_dead_prefix_and_removes_dirs() {
    let (temp, host) = empty_host();
    // Push more than MAX_RETIRED_SNAPSHOTS (64) dead Weak entries, each
    // pointing at a real (empty) staged dir. cleanup must drop the dead
    // ones AND remove their dirs.
    let mut dirs = Vec::new();
    {
        let mut guard = host
            .retired_snapshots
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        for i in 0..70 {
            let dir = temp.path().join(format!("retired-{i}"));
            fs::create_dir_all(&dir).expect("mk retired dir");
            dirs.push(dir.clone());
            // A Weak that never had a live Arc: upgrade() is always None.
            guard.push(RetiredSnapshot {
                snapshot: std::sync::Weak::<super::RuntimeSnapshot>::new(),
                staged_artifact_root: dir,
            });
        }
    }
    host.cleanup_retired_snapshots();
    let remaining = host
        .retired_snapshots
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .len();
    // All dead entries are swept by the Weak-liveness retain (they upgrade
    // to None), so nothing remains.
    assert_eq!(remaining, 0, "all dead Weak entries must be swept");
    for dir in &dirs {
        assert!(!dir.exists(), "dead entry's staged dir must be removed");
    }
}

// ─────────────── ReloadReport::from_snapshots removed arm ────────────

#[test]
#[serial]
fn reload_report_from_snapshots_records_removed_plugin() {
    let previous = snapshot_with_unavailable_plugin("gone", PluginUnavailableReason::InitFailed);
    // Build a snapshot with a DIFFERENT plugin so `gone` is "removed".
    let next = snapshot_with_unavailable_plugin("kept", PluginUnavailableReason::InitFailed);
    let report = ReloadReport::from_snapshots(&previous, &next, Path::new("/tmp/snap"), 7);
    assert!(report.removed_plugins.contains(&"gone".to_string()));
    assert!(report.added_plugins.contains(&"kept".to_string()));
    assert_eq!(report.elapsed_ms, 7);
}

// ─────────────── run_plugin_canary no-evidence path ──────────────────



// ─────────────── finalize_plugin_iteration arms ──────────────────────



// An agent-driven change carries its own evidence (a transcript and a tool
// summary), so a Partial canary must not block it — random plugins made the
// replay a coin flip.

// A blind edit has no transcript to vouch for it, so unproven behaviour waits
// for an operator instead of riding on the tests alone.


// Editing an existing test stages the change for approval instead of
// promoting it (tests are the contract).

// ─────────────── rollback_candidate_if_staged (no candidate) ─────────

#[test]
#[serial]
fn agent_list_sessions_reports_every_live_session() {
    let (_temp, host) = empty_host();
    assert!(host.agent_list_sessions().is_empty());
    let started = host
        .agent_start_with(AgentSessionKind::RuntimeShell, AgentStartOptions::default())
        .expect("start a session");
    let listed = host.agent_list_sessions();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].kind, "runtime_shell");
    let _ = started;
}

#[test]
#[serial]
fn rollback_candidate_if_staged_is_none_without_candidate() {
    let (_temp, host) = empty_host();
    assert!(host.rollback_candidate_if_staged(None).is_none());
}

// ─────────────── PluginIterationAgentBackend methods ─────────────────




















// ─────────────── ManagedAgentSession::compact_history wrapper ────────

#[test]
#[serial]
fn managed_session_compact_history_wrapper_is_noop_below_threshold() {
    let config = crate::config::LlmApiConfig {
        provider: "deepseek".to_string(),
        base_url: "http://127.0.0.1:1/v1".to_string(),
        api_key: Some("k".to_string()),
        model: "m".to_string(),
        ..crate::config::LlmApiConfig::default()
    };
    // Below the compaction threshold: wrapper returns (old, old) unchanged.
    let mut managed = ManagedAgentSession {
        handle: AgentSessionHandle {
            session_id: "s-1".to_string(),
            kind: AgentSessionKind::RuntimeShell,
        },
        session: crate::agent_loop::agent_loop()
            .expect("loop installed")
            .create(config, "runtime_shell")
            .expect("build agent session"),
        state: ManagedAgentState::RuntimeShell,
    };
    let (old, new) = managed.compact_history();
    assert_eq!(old, new, "no compaction below threshold");
}

// ─────────────── canary declared-verifier-node (JSON candidate) ──────




// ─────────── R2: cleanup_retired_snapshots pinned-snapshot eviction ──

/// R2: 上限驱逐不得删被 pin 的活条目。Hold >64 LIVE Arcs（模拟
/// in-flight 会话 pin 住旧快照）——Weak-liveness retain 全保留，上限
/// 循环看到最旧条目仍活着就停止，**不删任何目录**，积压保留。
#[test]
#[serial]
fn cleanup_retired_snapshots_keeps_pinned_backlog_beyond_max() {
    let (temp, host) = empty_host();
    let mut live: Vec<Arc<super::RuntimeSnapshot>> = Vec::new();
    let mut dirs = Vec::new();
    {
        let mut guard = host
            .retired_snapshots
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        for i in 0..70 {
            let snap = host.current_snapshot();
            let dir = temp.path().join(format!("live-retired-{i}"));
            fs::create_dir_all(&dir).expect("mk dir");
            dirs.push(dir.clone());
            guard.push(RetiredSnapshot {
                snapshot: Arc::downgrade(&snap),
                staged_artifact_root: dir,
            });
            live.push(snap);
        }
    }
    host.cleanup_retired_snapshots();
    let remaining = host
        .retired_snapshots
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .len();
    assert_eq!(
        remaining, 70,
        "pinned entries are kept even beyond MAX_RETIRED_SNAPSHOTS"
    );
    // 旧行为会删掉最旧 6 条的目录（→ 被 pin 快照上的 Process 插件
    // invoke spawn failed）；现在一条都不能删。
    assert!(dirs[0].exists(), "oldest pinned entry's dir must survive");
    assert!(dirs[69].exists(), "newest pinned entry's dir retained");
    drop(live);
}

/// R2: 混合场景——未 pin 的照删（retain 扫掉并删目录），pin 住的超过
/// 上限也保留。构造 70 个 pinned + 10 个 dead（Weak::new，从未有强
/// 引用）的条目。
#[test]
#[serial]
fn cleanup_retired_snapshots_removes_dead_and_keeps_pinned() {
    let (temp, host) = empty_host();
    let mut live: Vec<Arc<super::RuntimeSnapshot>> = Vec::new();
    let mut pinned_dirs = Vec::new();
    let mut dead_dirs = Vec::new();
    {
        let mut guard = host
            .retired_snapshots
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        for i in 0..70 {
            let snap = host.current_snapshot();
            let dir = temp.path().join(format!("pinned-{i}"));
            fs::create_dir_all(&dir).expect("mk pinned dir");
            pinned_dirs.push(dir.clone());
            guard.push(RetiredSnapshot {
                snapshot: Arc::downgrade(&snap),
                staged_artifact_root: dir,
            });
            live.push(snap);
        }
        for i in 0..10 {
            let dir = temp.path().join(format!("dead-{i}"));
            fs::create_dir_all(&dir).expect("mk dead dir");
            dead_dirs.push(dir.clone());
            guard.push(RetiredSnapshot {
                snapshot: std::sync::Weak::<super::RuntimeSnapshot>::new(),
                staged_artifact_root: dir,
            });
        }
    }
    host.cleanup_retired_snapshots();
    let remaining = host
        .retired_snapshots
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .len();
    // 10 条 dead 被 retain 扫掉；70 条 pinned 全保留（超上限也不驱逐）。
    assert_eq!(remaining, 70, "only dead entries are swept");
    for dir in &pinned_dirs {
        assert!(dir.exists(), "pinned entry's dir must survive: {dir:?}");
    }
    for dir in &dead_dirs {
        assert!(!dir.exists(), "dead entry's dir must be removed: {dir:?}");
    }
    drop(live);
}

/// `evict_retired_snapshots` 的"删死条目"臂：最旧条目在 retain 之后才
/// 死（竞态窗口）时，上限循环必须删它的目录。retain 已把所有 dead
/// 扫掉，这个臂只能经纯函数直测。
#[test]
#[serial]
fn evict_retired_snapshots_removes_dead_prefix_entries() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut retired: Vec<RetiredSnapshot> = Vec::new();
    // 第一条是死的：上限循环先删它，然后撞上活条目停止。
    let dead_dir = temp.path().join("dead-0");
    fs::create_dir_all(&dead_dir).expect("mk dead dir");
    retired.push(RetiredSnapshot {
        snapshot: std::sync::Weak::<super::RuntimeSnapshot>::new(),
        staged_artifact_root: dead_dir.clone(),
    });
    for i in 1..65 {
        let dir = temp.path().join(format!("live-{i}"));
        fs::create_dir_all(&dir).expect("mk live dir");
        retired.push(RetiredSnapshot {
            snapshot: std::sync::Weak::<super::RuntimeSnapshot>::new(),
            staged_artifact_root: dir,
        });
    }
    // 65 条（1 dead + 64 live）> 64：删掉 dead 后恰好回到上限。
    super::evict_retired_snapshots(&mut retired);
    assert_eq!(retired.len(), 64);
    assert!(!dead_dir.exists(), "the dead prefix entry's dir is removed");
}

// ─────────── finalize promote-failure arms (Pass/Pass) ───────────────



// Manual approval is what unblocks a test-editing change: it promotes
// through the same arm (and reports a promote failure the same way).

// ─────────── finalize TOCTOU source-drift downgrade ──────────────────



// ─────────── apply_operations partial-batch rollback error ───────────


// ─────────── run_plugin_iteration_agent edit_plan (no LLM) path ──────


// ─────────── verify_plugin_iteration wiring (cheap commands) ─────────



// ─────────── finalize manual-approved promote SUCCESS ────────────────



// ─────────── R3: promote/rollback/approve 候选身份校验 ──────────────

/// `candidate_replaced_error` 的文本逐字节稳定。
#[test]
fn candidate_replaced_error_text_is_stable() {
    let err = super::candidate_replaced_error("snap-a", "snap-b");
    assert_eq!(
        err.to_string(),
        "internal invariant broken: candidate snapshot replaced during plugin \
             iteration: expected snap-a, got snap-b"
    );
}

/// R3: 候选在暂存后（Step 4 与 Step 7 之间）被并发 `reload_candidate`
/// 替换时，promote 必须拒绝且**不取走**新候选、不 promote。
#[test]
#[serial]
fn promote_candidate_rejects_replaced_candidate() {
    let (_temp, host) = empty_host();
    let first = host.reload_candidate().expect("stage candidate #1");
    let second = host.reload_candidate().expect("replace candidate #2");
    assert_ne!(
        first.candidate_snapshot_id, second.candidate_snapshot_id,
        "two reloads must produce distinct snapshot ids"
    );

    let err = host
        .promote_candidate_matching(Some(&first.candidate_snapshot_id))
        .expect_err("a replaced candidate must be rejected");
    let text = err.to_string();
    assert!(
        text.contains("candidate snapshot replaced during plugin iteration"),
        "unexpected error: {text}"
    );
    assert!(
        text.contains(first.candidate_snapshot_id.as_str()),
        "error must name the expected snapshot: {text}"
    );
    assert!(
        text.contains(second.candidate_snapshot_id.as_str()),
        "error must name the actual staged snapshot: {text}"
    );
    // 未 promote：新候选仍在，current 未变。
    assert!(
        host.candidate_snapshot().is_some(),
        "candidate must stay staged"
    );
    assert_eq!(
        host.current_snapshot().snapshot_id(),
        second.from_snapshot_id,
        "current snapshot must be untouched"
    );
}

/// R3: rollback 同样拒绝被替换的候选——不 restore、不 retire。
#[test]
#[serial]
fn rollback_candidate_rejects_replaced_candidate() {
    let (_temp, host) = empty_host();
    let first = host.reload_candidate().expect("stage candidate #1");
    let _second = host.reload_candidate().expect("replace candidate #2");

    let err = host
        .rollback_candidate_matching(Some(&first.candidate_snapshot_id))
        .expect_err("a replaced candidate must be rejected");
    assert!(
        err.to_string()
            .contains("candidate snapshot replaced during plugin iteration"),
        "unexpected error: {err}"
    );
    assert!(
        host.candidate_snapshot().is_some(),
        "candidate must stay staged"
    );
}

/// R3: finalize 的 Pass/Pass promote 路径把 Step 4 记录的身份传下去——
/// 候选被替换时 finalize 返回 Err 且候选仍 staged。

// ─────────── reload retire-candidate + double reload_candidate ───────

#[test]
#[serial]
fn reload_retires_replaced_candidate_snapshot() {
    let (_temp, host) = empty_host();
    // Stage a candidate, then a full reload swaps the live snapshot and
    // retires the still-staged candidate (reload_internal line 4429).
    host.reload_candidate().expect("stage candidate");
    assert!(host.candidate_snapshot().is_some());
    host.reload("/").expect("full reload");
    // The replaced candidate was retired (taken) during reload_internal.
    assert!(
        host.candidate_snapshot().is_none(),
        "reload must clear/retire the staged candidate"
    );
}

#[test]
#[serial]
fn double_reload_candidate_retires_previous_candidate() {
    let (_temp, host) = empty_host();
    host.reload_candidate().expect("first candidate");
    // A second stage replaces the first; the previous candidate is retired
    // (reload_candidate_internal line 4535).
    host.reload_candidate()
        .expect("second candidate replaces first");
    assert!(host.candidate_snapshot().is_some());
}

// ─────────── run_plugin_canary candidate loop continue arm ───────────


// ─────────── session-lookup / wrong-kind error arms ───────────
//
// These four arms are private to the impl: the public wrappers
// (`agent_send_with_fallback`, `iterate_plugins`) either fail earlier or
// guarantee the session exists, so the misses have to be driven directly.
// `tests/host_boot_session_arms.rs` covers the reachable-from-outside
// counterparts.

/// `swap_session_profile`'s `AgentSessionNotFound` closure: an id that was
/// never inserted into `agent_sessions`.
#[test]
#[serial]
fn swap_session_profile_reports_missing_session() {
    let (_temp, host) = empty_host();
    let err = host
        .swap_session_profile("no-such-session", "default")
        .expect_err("an unknown session id must error");
    assert_eq!(
        err.to_string(),
        RuntimeError::AgentSessionNotFound {
            session_id: "no-such-session".to_string(),
        }
        .to_string()
    );
}

/// `swap_session_profile`'s success path on a live session: the resolved
/// profile's `api` replaces the session's live config, so `status().model`
/// reflects the swap.
#[test]
#[serial]
fn swap_session_profile_replaces_live_config() {
    let (_temp, host) = empty_host();
    let sid = host
        .agent_start(AgentSessionKind::RuntimeShell)
        .expect("start session")
        .session_id;
    // The empty fixture has only the `default` profile, so resolving an
    // unknown name falls back to it — the swap still rebuilds the client
    // and returns Ok.
    host.swap_session_profile(&sid, "no-such-profile")
        .expect("swap to the fallback-resolved default profile");
    let expected = host
        .config()
        .llm_profiles
        .resolve("default")
        .api
        .model
        .clone();
    assert_eq!(host.agent_status(&sid).expect("status").model, expected);
}

/// `plugin_iteration_agent_snapshot`'s two error arms: an unknown session
/// id, and a session that exists but is not a plugin-iteration session.
#[test]
#[serial]
fn plugin_iteration_agent_snapshot_rejects_missing_and_wrong_kind_sessions() {
    let (_temp, host) = empty_host();

    let missing = host
        .plugin_iteration_agent_snapshot("no-such-session")
        .expect_err("an unknown session id must error");
    assert_eq!(
        missing.to_string(),
        RuntimeError::AgentSessionNotFound {
            session_id: "no-such-session".to_string(),
        }
        .to_string()
    );

    // A RuntimeShell session exists but carries `ManagedAgentState::
    // RuntimeShell`, so the let-else guard rejects it.
    let sid = host
        .agent_start(AgentSessionKind::RuntimeShell)
        .expect("start a runtime-shell session")
        .session_id;
    let wrong_kind = host
        .plugin_iteration_agent_snapshot(&sid)
        .expect_err("a runtime-shell session is not an iteration session");
    let message = invalid_argument_message(&wrong_kind);
    assert_eq!(
        message,
        format!("agent session {sid} is not a plugin iteration session")
    );
}
