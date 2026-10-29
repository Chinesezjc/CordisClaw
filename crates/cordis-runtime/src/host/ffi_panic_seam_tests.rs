use super::{
    plugin_iteration_journal_path, RuntimeHost, TEST_AGENT_RESPOND_PANIC_INJECTION,
    TEST_ITERATION_ENOSPC_INJECTION, TEST_ITERATION_PANIC_INJECTION,
    TEST_STOP_HANDLER_PANIC_INJECTION,
};
use crate::core::error::RuntimeError;
use crate::kernel::plugin_iteration::{
    KernelPluginIssueSource, KernelPluginIterationRequest, PluginIterationFinalVerdict,
};
use cordis_plugin_sdk::{AbiFingerprint, NodeDoc, NodeType, PluginDocs};
use serde_json::json;
use serial_test::serial;
use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::Ordering::SeqCst;
use tempfile::TempDir;

/// A near-instant, cross-platform fixtures tree: `artifacts/index.json`
/// with zero entries. `RuntimeHost::boot` registers no plugins and never
/// touches a dylib.
fn setup_empty_fixture() -> (TempDir, PathBuf) {
    let temp = TempDir::new().expect("tempdir");
    let fixtures = temp.path().join("fixtures");
    let artifacts = fixtures.join("artifacts");
    fs::create_dir_all(&artifacts).expect("create artifacts dir");
    fs::write(
        artifacts.join("index.json"),
        r#"{
  "schema_version": 2,
  "generated_at": "2026-07-24T00:00:00Z",
  "topo_order": [],
  "entries": []
}
"#,
    )
    .expect("write empty artifact index");
    (temp, fixtures)
}

/// 在 fixtures 旁写 `config/runtime.yaml`，把 snapshot staging 钉在
/// `TempDir` 内，避免测试往全局 temp 目录堆 staged 工件。
/// `discover_config_dir` 对 file_name 为 `fixtures` 的 root 取兄弟
/// `../config`。
fn pin_snapshot_root_beside_fixtures(fixtures: &std::path::Path, snapshot_root: &std::path::Path) {
    let config_dir = fixtures
        .parent()
        .expect("fixtures has a parent")
        .join("config");
    fs::create_dir_all(&config_dir).expect("create config dir");
    fs::write(
        config_dir.join("runtime.yaml"),
        format!("runtime:\n  snapshot_root: {}\n", snapshot_root.display()),
    )
    .expect("write runtime.yaml");
}

#[test]
#[serial]
fn host_drop_removes_live_staged_root() {
    let (temp, fixtures) = setup_empty_fixture();
    let snapshot_root = temp.path().join("snapshots");
    pin_snapshot_root_beside_fixtures(&fixtures, &snapshot_root);

    let host = RuntimeHost::boot(&fixtures).expect("host should boot on empty index");
    let staged = host.current_snapshot().staged_artifact_root.clone();
    // 不带格式参数：`staged.display()` 只在断言失败时求值，会留下一行
    // 永久无覆盖。断言本身已足够定位问题。
    assert!(staged.starts_with(&snapshot_root));
    assert!(
        staged.exists(),
        "live staged root exists while host is alive"
    );

    // boot 后不 reload 直接 drop：此前没有任何路径回收 live staged root。
    drop(host);

    assert!(
        !staged.exists(),
        "Drop must reclaim the live snapshot's staged root"
    );
}

#[test]
#[serial]
fn cleanup_live_snapshot_is_idempotent() {
    let (temp, fixtures) = setup_empty_fixture();
    let snapshot_root = temp.path().join("snapshots");
    pin_snapshot_root_beside_fixtures(&fixtures, &snapshot_root);

    let host = RuntimeHost::boot(&fixtures).expect("host should boot on empty index");
    let staged = host.current_snapshot().staged_artifact_root.clone();

    // 信号处理路径显式调一次，随后 Drop 还会再调一次：必须幂等。
    host.cleanup_live_snapshot();
    assert!(!staged.exists());
    host.cleanup_live_snapshot();
    drop(host);
    assert!(!staged.exists());
}

/// A JSON-artifact plugin `svc` that declares one `Task` node. JSON
/// artifacts skip dlopen at load time, so this boots on any host. The
/// artifact file's `abi_fingerprint`, `plugin_path`, and `docs` must match
/// the index entry (the loader cross-checks all three) and the recorded
/// sha256 must match the artifact bytes.
fn setup_task_plugin_fixture() -> (TempDir, PathBuf) {
    let temp = TempDir::new().expect("tempdir");
    let fixtures = temp.path().join("fixtures");
    let artifacts = fixtures.join("artifacts");
    fs::create_dir_all(&artifacts).expect("create artifacts dir");

    let abi = AbiFingerprint::current_build("crate_svc_v1", "api_v2");
    let node = NodeDoc {
        id: "svc_serve".to_string(),
        summary: "background task node for stop-handler seam test".to_string(),
        input_schema: json!({
            "type": "object",
            "required": ["node_id"],
            "properties": { "node_id": { "type": "string", "const": "svc_serve" } }
        }),
        output_schema: json!({ "type": "object" }),
        side_effects: Vec::new(),
        failure_modes: vec!["unknown node_id".to_string()],
        node_type: NodeType::Task,
        agent_accessible: false,
    };
    let docs = PluginDocs {
        plugin_id: "svc".to_string(),
        plugin_path: "svc".to_string(),
        plugin_version: "0.1.0".to_string(),
        abi_version: 2,
        command_name: Some("Svc".to_string()),
        nodes: vec![node],
        system_hint: None,
    };

    // The artifact JSON: shape mirrors `PluginArtifact` (plugin_path,
    // abi_fingerprint, docs, exports, execution).
    let artifact_value = json!({
        "plugin_path": "svc",
        "abi_fingerprint": abi,
        "docs": docs,
        "exports": [],
        "execution": null,
    });
    let artifact_bytes = serde_json::to_vec_pretty(&artifact_value).expect("serialize artifact");
    let artifact_path = artifacts.join("svc.json");
    fs::write(&artifact_path, &artifact_bytes).expect("write artifact");
    let sha256 = crate::plugin::artifact::sha256_file(&artifact_path).expect("hash svc artifact");

    let index_value = json!({
        "schema_version": 2,
        "generated_at": "2026-07-24T00:00:00Z",
        "topo_order": ["svc"],
        "entries": [{
            "plugin_path": "svc",
            "version": "0.1.0",
            "abi_fingerprint": abi,
            "artifact_path": "svc.json",
            "sha256": sha256,
            "built_at": "0",
            "parent": null,
            "required": true,
            "grants_from_parent": [],
            "docs": docs,
            "exports": [],
            "execution": null,
            "artifact_kind": "json",
            "build_fingerprint": "bf",
            "input_probe": { "files": [] },
            "local_path_deps": []
        }]
    });
    fs::write(
        artifacts.join("index.json"),
        serde_json::to_vec_pretty(&index_value).expect("serialize index"),
    )
    .expect("write index");

    (temp, fixtures)
}

fn iteration_request() -> KernelPluginIterationRequest {
    // Root mode (empty targets) so `begin_plugin_iteration` succeeds even
    // with zero registered plugins.
    KernelPluginIterationRequest {
        issue_id: None,
        target_plugin_paths: Vec::new(),
        instruction: Some("seam panic injection".to_string()),
        edit_plan: None,
        manual_approved: false,
        tests_command: None,
        safety_command: None,
        verify_profile: None,
        quality_score: None,
    }
}

/// 一个空 edit_plan 的请求：带 edit_plan 时 `run_plugin_iteration_agent`
/// 走手工执行分支、完全不碰 LLM；operations 为空则不触碰任何文件，从而
/// 绕开插件编辑面策略（空 fixture 里没有任何可写的插件子树），让迭代
/// 确定性地推进到注入点所在的 journal persist 阶段。
fn empty_plan_request() -> KernelPluginIterationRequest {
    use crate::kernel::plugin_iteration::PluginEditPlan;
    KernelPluginIterationRequest {
        issue_id: None,
        target_plugin_paths: Vec::new(),
        instruction: Some("enospc injection".to_string()),
        edit_plan: Some(PluginEditPlan {
            issue_id: "enospc-issue".to_string(),
            patch_id: "enospc-patch".to_string(),
            summary: "no-op plan".to_string(),
            operations: Vec::new(),
        }),
        manual_approved: true,
        tests_command: None,
        safety_command: None,
        verify_profile: None,
        quality_score: None,
    }
}

/// 磁盘满必须被判成 `InfrastructureFailure`，而不是 `RolledBack`。
///
/// 这是本批修复的核心断言：ENOSPC 此前经 `stage_error: Option<String>`
/// 丢掉类型信息，在 `finalize_plugin_iteration` 被降级成"验证失败"，
/// 于是污染 rollback 率、把插件 issue 标成 Open、还丢掉重试入口。
#[test]
#[serial]
fn iterate_plugins_reports_infrastructure_failure_on_enospc() {
    let (_temp, fixtures) = setup_empty_fixture();
    let host = RuntimeHost::boot(&fixtures).expect("host should boot on empty index");

    let before = host.kernel().status();

    TEST_ITERATION_ENOSPC_INJECTION.store(true, SeqCst);
    let result = host
        .iterate_plugins(empty_plan_request())
        .expect("infrastructure failure is a verdict, not an Err");

    assert_eq!(
        result.final_verdict,
        PluginIterationFinalVerdict::InfrastructureFailure,
        "ENOSPC must not be reported as RolledBack; blocked_reason={:?}",
        result.blocked_reason
    );
    let reason = result.blocked_reason.as_deref().unwrap_or_default();
    assert!(
        reason.contains("No space left on device"),
        "the errno text must survive to blocked_reason, got: {reason}"
    );

    let after = host.kernel().status();
    assert_eq!(
        after.iteration_rollback_total, before.iteration_rollback_total,
        "infrastructure failures must not inflate the rollback metric"
    );
    assert_eq!(
        after.iteration_infrastructure_failure_total,
        before.iteration_infrastructure_failure_total + 1,
        "infrastructure failures get their own counter"
    );

    // 插件没有被冤枉：不产生 LoadFailure issue。
    assert!(
        !host
            .kernel()
            .plugin_issues()
            .iter()
            .any(|issue| issue.source == KernelPluginIssueSource::LoadFailure),
        "a full disk must not be recorded as a plugin LoadFailure"
    );

    // 仍可重试：迭代留在 blocked_iterations 里（RolledBack 会被摘除）。
    assert!(
        host.kernel()
            .blocked_iterations()
            .iter()
            .any(|entry| entry.iteration_id == result.iteration_id),
        "infrastructure failures stay retryable"
    );
}

/// 回归护栏：真正的 stage 失败仍然是 `RolledBack`，没被新变体抢走。
#[test]
#[serial]
fn genuine_stage_failure_still_rolls_back() {
    let (_temp, fixtures) = setup_empty_fixture();
    let host = RuntimeHost::boot(&fixtures).expect("host should boot on empty index");

    // 不注入 ENOSPC；改一个策略禁止的路径（crates/ 属 forbidden_prefixes），
    // 这是插件侧的真实失败，必须仍判 RolledBack。
    let mut request = empty_plan_request();
    if let Some(plan) = request.edit_plan.as_mut() {
        plan.operations = vec![crate::kernel::plugin_iteration::PluginEditOperation {
            path: "crates/cordis-runtime/src/lib.rs".to_string(),
            kind: crate::kernel::plugin_iteration::PluginEditOpKind::ReplaceExact,
            expected_old_string: Some("pub".to_string()),
            expected_sha256: None,
            new_content: Some("mod".to_string()),
            pointer: None,
            dotted_key: None,
            value: None,
        }];
    }

    let before = host.kernel().status();
    let result = host
        .iterate_plugins(request)
        .expect("policy block is a verdict, not an Err");

    assert_eq!(
        result.final_verdict,
        PluginIterationFinalVerdict::RolledBack,
        "a genuine policy-blocked edit must remain RolledBack"
    );
    let after = host.kernel().status();
    assert_eq!(
        after.iteration_infrastructure_failure_total, before.iteration_infrastructure_failure_total,
        "a genuine failure must not land in the infrastructure counter"
    );
    assert_eq!(
        after.iteration_rollback_total,
        before.iteration_rollback_total + 1,
        "a genuine failure still counts as a rollback"
    );
}

#[test]
fn log_builtin_registration_failure_emits_without_panicking() {
    // Covers the logging body, which `register_builtin_agent_node` never
    // reaches because the builtin docs always register cleanly.
    let err = crate::core::error::RuntimeError::Invariant {
        message: "docs were rejected".to_string(),
    };
    super::log_builtin_registration_failure(&err);
}

#[test]
fn builtin_registration_failure_line_embeds_the_error() {
    let err = super::host_invariant("docs were rejected".to_string());
    let line = super::builtin_registration_failure_line(&err);
    assert!(line.starts_with("[builtin] agent_router registration failed: "));
    assert!(line.contains("docs were rejected"));
}

// ── plugin_history is capped: the 1025th distinct entry drops the oldest ──
#[test]
#[serial]
fn record_plugin_iteration_outcome_caps_history_at_the_bound() {
    let (_temp, fixtures) = setup_empty_fixture();
    let host = RuntimeHost::boot(&fixtures).expect("host should boot on empty index");

    // MAX_PLUGIN_HISTORY is 1024; record one more so the `pop_back` trim
    // runs. Entries are keyed by iteration_id, so each must be distinct.
    for i in 0..1025u32 {
        host.kernel
            .record_plugin_iteration_outcome(&synthetic_result(&format!("iter-{i}")));
    }
    let history = host.kernel.plugin_history();
    assert_eq!(history.len(), 1024, "history must be trimmed to the bound");
    // Newest at the front, and the very first entry has been dropped.
    assert_eq!(history[0].iteration_id, "iter-1024");
    assert!(
        !history.iter().any(|e| e.iteration_id == "iter-0"),
        "the oldest entry must be evicted"
    );
}

// ── R9: blocked_iterations 有上限：第 65 条 Blocked 淘汰最旧一条 ──
#[test]
#[serial]
fn blocked_iterations_capped_at_max_evicts_oldest() {
    let (_temp, fixtures) = setup_empty_fixture();
    let host = RuntimeHost::boot(&fixtures).expect("host should boot on empty index");

    // `synthetic_result` 的 final_verdict 是 Blocked，且 iteration_id 每
    // 次唯一 → 反复插入会无界增长；上限 64 条，插 65 条必须淘汰最旧。
    for i in 0..65u32 {
        host.kernel
            .record_plugin_iteration_outcome(&synthetic_result(&format!("iter-{i:03}")));
    }
    let blocked = host.kernel.blocked_iterations();
    assert_eq!(blocked.len(), 64, "blocked must be capped at the bound");
    assert!(
        !blocked.iter().any(|entry| entry.iteration_id == "iter-000"),
        "the oldest blocked entry must be evicted"
    );
    assert!(
        blocked.iter().any(|entry| entry.iteration_id == "iter-064"),
        "the newest blocked entry must be retained"
    );
}

fn synthetic_result(iteration_id: &str) -> crate::host::KernelPluginIterationResult {
    crate::host::KernelPluginIterationResult {
        iteration_id: iteration_id.to_string(),
        issue_id: "issue-cap".to_string(),
        root_plugin_path: "plugins/mini".to_string(),
        target_plugin_paths: Vec::new(),
        source: None,
        summary: "history cap probe".to_string(),
        agent_session_id: None,
        tool_execution_summary: None,
        derived_edit_plan: crate::kernel::plugin_iteration::PluginEditPlan {
            issue_id: "issue-cap".to_string(),
            patch_id: format!("{iteration_id}-empty"),
            summary: "history cap probe".to_string(),
            operations: Vec::new(),
        },
        transcript_excerpt: Vec::new(),
        changed_paths: Vec::new(),
        rebuilt_artifacts: Vec::new(),
        candidate: None,
        verification: None,
        verifier_verdict: None,
        canary: None,
        final_verdict: crate::host::PluginIterationFinalVerdict::Blocked,
        blocked_reason: None,
        net_output: crate::execution::engine::ExecutionOutput {
            execution_id: iteration_id.to_string(),
            order: Vec::new(),
            outcomes: std::collections::BTreeMap::new(),
            keyed_outcomes: std::collections::BTreeMap::new(),
            metrics: crate::execution::engine::ExecutionMetrics::default(),
        },
    }
}

// ── agent_send drains queued PendingSessionAction::CompactHistory ────
#[test]
#[serial]
fn agent_send_drains_queued_compact_history_action() {
    let (_temp, fixtures) = setup_empty_fixture();
    let host = RuntimeHost::boot(&fixtures).expect("host should boot on empty index");
    let handle = host
        .agent_start(crate::host::AgentSessionKind::RuntimeShell)
        .expect("start a runtime-shell session");

    // Queue the action the way `compact_context` does when it cannot reach
    // the session, then drive `agent_send`. The LLM call fails (no server
    // configured in the empty fixture), but the pending-action drain runs
    // unconditionally after `respond`, so the CompactHistory arm executes.
    host.queue_session_action(
        &handle.session_id,
        crate::host::PendingSessionAction::CompactHistory,
    );
    let _ = host.agent_send(&handle.session_id, "hello");

    // The queue is consumed exactly once: a second send finds it empty and
    // the session is still addressable (it was reinserted).
    let _ = host.agent_send(&handle.session_id, "again");
    assert!(
        host.agent_sessions_mut().contains_key(&handle.session_id),
        "session must be reinserted after draining pending actions"
    );
}

// ── R1: respond panic 收尾仍把会话插回（不再永久丢失）──────────────
#[test]
#[serial]
fn agent_send_respond_panic_preserves_session_and_returns_invariant() {
    let (_temp, fixtures) = setup_empty_fixture();
    let host = RuntimeHost::boot(&fixtures).expect("host should boot on empty index");
    let handle = host
        .agent_start(crate::host::AgentSessionKind::RuntimeShell)
        .expect("start a runtime-shell session");
    let sid = handle.session_id.clone();

    // 注入：下一次 respond 直接 panic（等价于 agent 循环内 assert 炸掉）。
    TEST_AGENT_RESPOND_PANIC_INJECTION.store(true, std::sync::atomic::Ordering::SeqCst);
    let err = host
        .agent_send(&sid, "hello")
        .expect_err("a respond panic must surface as Err, not unwind");
    let text = err.to_string();
    assert!(
        text.contains("agent respond panicked"),
        "error must name the panicked respond: {text}"
    );
    assert!(
        text.contains("test panic injection: agent respond"),
        "error must carry the injected panic text: {text}"
    );
    // 标记被消费且只消费一次。
    assert!(
        !TEST_AGENT_RESPOND_PANIC_INJECTION.load(std::sync::atomic::Ordering::SeqCst),
        "injection flag must reset after firing"
    );

    // 会话没有随 panic 丢失：收尾把它插回了 map。
    assert!(
        host.agent_status(&sid).is_ok(),
        "session must survive a respond panic"
    );
    // 且仍可继续使用：第二次 send 走正常路径（空 fixtures 无 provider
    // 插件 → NoLlmProvider，是普通 Err 而不是 panic）。
    let err2 = host
        .agent_send(&sid, "again")
        .expect_err("second send runs the normal path");
    assert!(
        matches!(err2, RuntimeError::NoLlmProvider),
        "unexpected second-send error: {err2:?}"
    );
}

/// `agent_respond_panicked_error` 的文本：三种 panic 载荷（&str、String、
/// 其它）各给稳定、可读的错误信息。
#[test]
fn agent_respond_panicked_error_formats_all_payload_kinds() {
    let str_payload: Box<dyn std::any::Any + Send> = Box::new("boom");
    let from_str = super::agent_respond_panicked_error(&str_payload);
    assert_eq!(
        from_str.to_string(),
        "internal invariant broken: agent respond panicked: boom"
    );
    let string_payload: Box<dyn std::any::Any + Send> = Box::new("boom".to_string());
    let from_string = super::agent_respond_panicked_error(&string_payload);
    assert_eq!(
        from_string.to_string(),
        "internal invariant broken: agent respond panicked: boom"
    );
    let other_payload: Box<dyn std::any::Any + Send> = Box::new(42_u32);
    let from_other = super::agent_respond_panicked_error(&other_payload);
    assert_eq!(
        from_other.to_string(),
        "internal invariant broken: agent respond panicked: unknown panic payload"
    );
}

/// `mark_dropped_if_inflight` 的两臂：in-flight 时标记，否则不动。
#[test]
fn mark_dropped_if_inflight_marks_only_inflight_ids() {
    let inflight = std::sync::Mutex::new(BTreeSet::from(["s1".to_string()]));
    let dropped = std::sync::Mutex::new(BTreeSet::new());
    super::mark_dropped_if_inflight(&inflight, &dropped, "s1");
    assert!(dropped.lock().unwrap().contains("s1"));
    super::mark_dropped_if_inflight(&inflight, &dropped, "s2");
    assert!(!dropped.lock().unwrap().contains("s2"));
}

// ── C-seam #1: iterate_plugins emergency-rollback arm ────────────────
#[test]
#[serial]
fn iterate_plugins_panic_is_caught_and_rolls_back_candidate_and_clears_journal() {
    let (_temp, fixtures) = setup_empty_fixture();
    let host = RuntimeHost::boot(&fixtures).expect("host should boot on empty index");

    // Seed a candidate snapshot so we can prove the panic arm rolls it back.
    host.reload_candidate().expect("stage candidate");
    assert!(
        host.candidate_snapshot().is_some(),
        "candidate should be staged before the panic"
    );

    // Seed a rollback journal so we can prove the panic arm clears it. The
    // journal records the (empty) current bytes of an in-workspace file;
    // rollback restores those same bytes (a no-op edit), so `restore_
    // plugin_iteration_workspace` runs cleanly without a cargo rebuild.
    let marker_rel = "artifacts/seam-marker.txt";
    let marker_abs = fixtures.join(marker_rel);
    fs::write(&marker_abs, b"original").expect("seed marker file");
    let rollback = crate::kernel::plugin_iteration::PluginEditRollback::single_backup(
        fixtures.clone(),
        marker_rel,
        Some(b"original".to_vec()),
    );
    let journal_path = plugin_iteration_journal_path(&host.snapshot_root);
    rollback
        .persist_journal(&journal_path, "seam-iteration")
        .expect("persist journal");
    assert!(journal_path.exists(), "journal should exist before panic");

    // Arm the injection and drive the iteration.
    TEST_ITERATION_PANIC_INJECTION.store(true, SeqCst);
    let result = host.iterate_plugins(iteration_request());

    // The panic was caught: iterate_plugins returns an Invariant error
    // whose message carries the injected panic text.
    let err = result.expect_err("panic must surface as Err, not unwind");
    let msg = err.to_string();
    assert!(
        msg.contains("test panic injection: plugin iteration"),
        "error must carry the injected panic message, got: {msg}"
    );
    assert!(
        msg.contains("workspace has been restored"),
        "error must report the emergency restore, got: {msg}"
    );

    // The flag was consumed exactly once (swapped back to false).
    assert!(
        !TEST_ITERATION_PANIC_INJECTION.load(SeqCst),
        "injection flag must be reset after firing"
    );
    // Candidate was rolled back.
    assert!(
        host.candidate_snapshot().is_none(),
        "candidate must be rolled back by the panic arm"
    );
    // Journal was cleared.
    assert!(
        !journal_path.exists(),
        "rollback journal must be cleared by the panic arm"
    );
    // The iteration lock was released (finish_plugin_iteration ran before
    // the match arm), so a subsequent iteration can start. Re-arm the flag
    // so the follow-up panics at the closure's first line again — this
    // proves the lock was freed WITHOUT driving a real LLM agent turn (an
    // unarmed, edit-plan-less iteration would otherwise call out to the
    // configured LLM endpoint).
    TEST_ITERATION_PANIC_INJECTION.store(true, SeqCst);
    let followup = host.iterate_plugins(iteration_request());
    let followup_msg = followup
        .expect_err("re-armed follow-up must also surface as Err")
        .to_string();
    assert!(
        followup_msg.contains("test panic injection: plugin iteration"),
        "follow-up must reach the injection point (lock was released), got: {followup_msg}"
    );
    assert!(
        !TEST_ITERATION_PANIC_INJECTION.load(SeqCst),
        "injection flag must be reset after the follow-up fires"
    );
}

// ── C-seam #2: reload/stop-handler catch_unwind arm ──────────────────
#[test]
#[serial]
fn stop_handler_panic_is_caught_and_reload_path_survives() {
    // A panic inside a plugin's stop handler runs in that plugin's dylib,
    // so it is caught at the FFI seam and the reload keeps going instead of
    // unwinding the process. Use a real dylib plugin so Phase 1 validates
    // and Phase 2 actually runs the stop handler.
    let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .canonicalize()
        .expect("fixtures root");
    let host = RuntimeHost::boot(&fixtures).expect("host should boot on the repo fixtures");
    let task_fqns = host.current_snapshot().node_registry().task_node_fqns();
    assert!(
        task_fqns.iter().any(|fqn| fqn == "qq::qq_serve"),
        "qq::qq_serve Task node must be registered, got: {task_fqns:?}"
    );

    TEST_STOP_HANDLER_PANIC_INJECTION.store(true, SeqCst);
    let reload_result = host.reload("qq");
    assert!(
        !TEST_STOP_HANDLER_PANIC_INJECTION.load(SeqCst),
        "stop-handler injection flag must be reset after firing"
    );
    assert!(
        reload_result.is_ok(),
        "a panic in a stop handler must not abort the reload: {reload_result:?}"
    );
}

// A reload rejected during validation must leave the running plugins
// untouched: stop handlers run in Phase 2, after Phase 1 accepts the new
// artifact. (Before that reorder, a rejected reload had already killed
// e.g. qq's 8099/8002 listeners — the 2026-10-28 outage.)
#[test]
#[serial]
fn failed_reload_does_not_stop_services() {
    let (_temp, fixtures) = setup_task_plugin_fixture();
    let host = RuntimeHost::boot(&fixtures).expect("host should boot on task fixture");
    TEST_STOP_HANDLER_PANIC_INJECTION.store(true, SeqCst);

    // The fixture's artifact is JSON, so Phase 1's dlopen fails.
    let err = host
        .reload("svc")
        .expect_err("JSON artifact cannot be dlopened");
    let message = err.to_string();
    assert!(!message.is_empty(), "the failure must be reported: {err:?}");

    assert!(
        TEST_STOP_HANDLER_PANIC_INJECTION.load(SeqCst),
        "a rejected reload must not run stop handlers"
    );
    TEST_STOP_HANDLER_PANIC_INJECTION.store(false, SeqCst);
}

// ── C-seam #2b: reload stop-loop tolerates a failing stop invoke ──────
#[test]
#[serial]
fn reload_stop_loop_survives_failing_stop_invoke() {
    // No panic injection this time: the reload stop-loop performs a real
    // `stop` invoke against the JSON Task node. That node has
    // `execution: null`, so `invoke` returns Err(PluginExecutionUnsupported)
    // — the invoke-error diagnostic branch. The property under test is that
    // this failing/diagnostic-producing stop does NOT abort the reload: the
    // reload still proceeds to Phase 1 (where the JSON dlopen fails, an
    // expected caught Err) and the host stays usable afterwards.
    //
    // The diagnostic itself is written via `eprintln!` from inside the
    // per-node `catch_unwind` closure, which is not straightforward to
    // capture in-process; the exact message content is asserted directly
    // against the pure `stop_result_diagnostic` helper in
    // `seam_pure_fn_tests`. Here we only confirm the reload flow completes.
    let (_temp, fixtures) = setup_task_plugin_fixture();
    let host = RuntimeHost::boot(&fixtures).expect("host should boot on task fixture");

    // Sanity: the Task node the stop-loop iterates over is registered.
    let snapshot = host.current_snapshot();
    assert!(
        snapshot
            .node_registry()
            .task_node_fqns()
            .iter()
            .any(|fqn| fqn == "svc::svc_serve"),
        "svc::svc_serve Task node must be registered"
    );
    drop(snapshot);

    // Flag is NOT set: the stop-loop runs a real (failing) stop invoke.
    assert!(
        !TEST_STOP_HANDLER_PANIC_INJECTION.load(SeqCst),
        "panic injection must be off for this test"
    );
    let reload_result = host.reload("svc");
    // The reload still fails at Phase 1's JSON dlopen — but it reached
    // Phase 1, which means the failing stop invoke in the earlier stop-loop
    // did not interrupt the reload path.
    assert!(
        reload_result.is_err(),
        "reloading a JSON artifact fails at Phase 1 dlopen"
    );

    // Host remains usable: the Task node registry survived and a second
    // reload still runs the same clean path.
    let after = host.current_snapshot();
    assert!(
        after
            .node_registry()
            .task_node_fqns()
            .iter()
            .any(|fqn| fqn == "svc::svc_serve"),
        "Task node registry must survive the failing stop invoke"
    );
}
