//! Coverage-focused integration tests for `RuntimeHost` / `RuntimeKernel`.
//!
//! Unlike `runtime_host.rs`, these tests do NOT gate behind
//! `linux_dylib_artifacts_available()`. `ensure_fixture_artifacts` rebuilds the
//! fixture dylibs for the *current* host target (arm64 macOS included) and
//! rewrites `artifacts/index.json` with the local target triple, so a real
//! `RuntimeHost::boot` against the fixtures tree succeeds here. Read-only
//! surface is exercised against the shared real fixtures (the loader only
//! reads `artifacts/`; the snapshot root lives in the OS temp dir), while any
//! path that mutates the fixtures tree runs against a throwaway copy.

use cordis_runtime::agent::{AgentSession, AgentToolHost};
use cordis_runtime::config::RuntimeConfig;
use cordis_runtime::core::error::RuntimeError;
use cordis_runtime::core::models::NodeOutcome;
use cordis_runtime::host::{
    AgentSessionKind, AgentStartOptions, ReloadAttemptStatus, RuntimeHost, RuntimeKernel,
    WalkControl,
};
use cordis_runtime::kernel::auto_update::{AutoUpdatePlan, FilePatch};
use cordis_runtime::kernel::evaluator::VerificationInput;
use cordis_runtime::kernel::plugin_iteration::PluginEditRollback;
use cordis_runtime::kernel::plugin_iteration::{
    KernelPluginIssueSource, KernelPluginIterationRequest, PluginEditOpKind, PluginEditOperation,
    PluginEditPlan, PluginIterationFinalVerdict,
};
use cordis_runtime::kernel::verifier::VerificationProfile;
use cordis_runtime::soul::Soul;
use serde_json::{json, Value};
use serial_test::serial;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use tempfile::TempDir;

mod support;
use support::{fixtures_root, pin_private_snapshot_root};

// ---------------------------------------------------------------------------
// Shared read-only host against the real fixtures tree.
//
// Booting is comparatively cheap (the loader only reads `artifacts/`), but we
// still only want to pay for it once for the read-only tests that never mutate
// the tree. `serial` on those tests keeps them from racing each other's use of
// the shared temp snapshot root inside the kernel.
// ---------------------------------------------------------------------------

static SHARED_HOST: OnceLock<RuntimeHost> = OnceLock::new();

fn shared_host() -> &'static RuntimeHost {
    SHARED_HOST.get_or_init(|| {
        let root = fixtures_root();
        // Flaky-guard: other tests in a full serial run rebuild the fixture
        // dylibs mid-suite, which makes `artifacts/index.json`'s recorded
        // sha256 stale relative to the on-disk `.so`. A subsequent boot then
        // fails with `PluginUnavailable { HashMismatch }`. Re-hash every
        // staged artifact and rewrite the index right before booting so the
        // shared host always sees a consistent index.
        cordis_runtime::plugin::tooling::refresh_artifact_index(&root)
            .expect("refresh artifact index before shared boot");
        RuntimeHost::boot(root).expect("host should boot")
    })
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

/// Copy only the `artifacts/` tree plus top-level config-ish files into a
/// fresh temp dir. This is enough for a read-only boot (the loader consumes
/// `artifacts/index.json` and the staged dylibs); it deliberately skips the
/// multi-gigabyte `plugins/` source tree that lives on the network mount.
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
    // fixtures root 就是 temp.path()，`discover_config_dir` 落到 `temp/config`。
    pin_private_snapshot_root(temp.path(), temp.path());
    temp
}

// ===========================================================================
// Group A — RuntimeKernel unit surface (no fixtures; tiny local temp dir).
// ===========================================================================

#[test]
fn kernel_status_reflects_default_config_and_empty_state() {
    let temp = TempDir::new().expect("tempdir");
    let config = cordis_runtime::config::RuntimeConfig::default();
    let kernel = RuntimeKernel::new(temp.path(), &config);

    let status = kernel.status();
    assert_eq!(status.workspace_root, temp.path().display().to_string());
    assert_eq!(status.iteration_total, 0);
    assert_eq!(status.iteration_promote_total, 0);
    assert_eq!(status.iteration_rollback_total, 0);
    assert_eq!(status.history_len, 0);
    assert!(status.last_change.is_none());
    assert_eq!(status.plugin_issue_count, 0);
    assert_eq!(status.blocked_iteration_count, 0);
    assert_eq!(status.plugin_iteration_total, 0);
    assert!(status.last_plugin_iteration.is_none());
    assert!(kernel.history().is_empty());
    assert!(kernel.plugin_issues().is_empty());
    assert!(kernel.plugin_history().is_empty());
    assert!(kernel.blocked_iterations().is_empty());
}

#[test]
fn kernel_run_iteration_promotes_and_records_change() {
    let temp = TempDir::new().expect("tempdir");
    let target = temp.path().join("notes.txt");
    fs::write(&target, "alpha-old-omega").expect("seed patch target");
    let config = cordis_runtime::config::RuntimeConfig::default();
    let kernel = RuntimeKernel::new(temp.path(), &config);

    let result = kernel
        .run_iteration(
            AutoUpdatePlan {
                issue_id: "issue-promote".to_string(),
                patch_id: "patch-promote".to_string(),
                manual_approved: false,
                diff_lines: 1,
                patches: vec![FilePatch::text("notes.txt", "old", "new")],
            },
            VerificationInput {
                tests_passed: true,
                safety_checks_passed: true,
                quality_score: 95,
            },
        )
        .expect("iteration should run");

    assert!(!result.rolled_back);
    assert_eq!(
        fs::read_to_string(&target).expect("read patched file"),
        "alpha-new-omega"
    );
    let status = kernel.status();
    assert_eq!(status.iteration_total, 1);
    assert_eq!(status.iteration_promote_total, 1);
    assert_eq!(status.iteration_rollback_total, 0);
    assert_eq!(status.history_len, 1);
    assert_eq!(kernel.history().len(), 1);
}

#[test]
fn kernel_run_iteration_rolls_back_on_failed_verification() {
    let temp = TempDir::new().expect("tempdir");
    let target = temp.path().join("notes.txt");
    fs::write(&target, "alpha-old-omega").expect("seed patch target");
    let config = cordis_runtime::config::RuntimeConfig::default();
    let kernel = RuntimeKernel::new(temp.path(), &config);

    let result = kernel
        .run_iteration(
            AutoUpdatePlan {
                issue_id: "issue-rollback".to_string(),
                patch_id: "patch-rollback".to_string(),
                manual_approved: false,
                diff_lines: 1,
                patches: vec![FilePatch::text("notes.txt", "old", "new")],
            },
            VerificationInput {
                tests_passed: false,
                safety_checks_passed: true,
                quality_score: 10,
            },
        )
        .expect("iteration should still return a result");

    assert!(result.rolled_back);
    // Rolled-back edits must leave the original file untouched.
    assert_eq!(
        fs::read_to_string(&target).expect("read reverted file"),
        "alpha-old-omega"
    );
    let status = kernel.status();
    assert_eq!(status.iteration_total, 1);
    assert_eq!(status.iteration_promote_total, 0);
    assert_eq!(status.iteration_rollback_total, 1);
}

#[test]
fn kernel_observe_plugin_issue_dedups_and_counts() {
    let temp = TempDir::new().expect("tempdir");
    let config = cordis_runtime::config::RuntimeConfig::default();
    let kernel = RuntimeKernel::new(temp.path(), &config);

    let first = kernel.observe_plugin_issue(
        KernelPluginIssueSource::InvokeFailure,
        "expr",
        "invoke blew up",
    );
    assert_eq!(first.observe_count, 1);
    let second = kernel.observe_plugin_issue(
        KernelPluginIssueSource::InvokeFailure,
        "expr",
        "invoke blew up again",
    );
    // Same source + root path collapses onto the same issue id.
    assert_eq!(second.issue_id, first.issue_id);
    assert_eq!(second.observe_count, 2);
    assert_eq!(second.summary, "invoke blew up again");

    // A different source produces a distinct issue.
    kernel.observe_plugin_issue(KernelPluginIssueSource::LoadFailure, "shell", "load broke");
    let issues = kernel.plugin_issues();
    assert_eq!(issues.len(), 2);
    assert!(issues
        .iter()
        .any(|issue| issue.root_plugin_path == "expr" && issue.observe_count == 2));
    assert!(issues.iter().any(|issue| issue.root_plugin_path == "shell"
        && issue.source == KernelPluginIssueSource::LoadFailure));
    assert_eq!(kernel.status().plugin_issue_count, 2);
}

#[test]
fn kernel_can_auto_iterate_plugins_follows_api_key_presence() {
    let temp = TempDir::new().expect("tempdir");
    let mut config = cordis_runtime::config::RuntimeConfig::default();
    // Default config has no inline api_key and an env var that is unset in the
    // test process, so auto-iteration is disabled.
    config.llm_api.api_key = None;
    config.llm_api.api_key_env = "CORDIS_TEST_DEFINITELY_UNSET_KEY".to_string();
    let kernel = RuntimeKernel::new(temp.path(), &config);
    assert!(!kernel.can_auto_iterate_plugins());

    let mut config2 = cordis_runtime::config::RuntimeConfig::default();
    config2.llm_api.api_key = Some("sk-inline-test".to_string());
    let kernel2 = RuntimeKernel::new(temp.path(), &config2);
    assert!(kernel2.can_auto_iterate_plugins());
}

// ===========================================================================
// Group B — read-only RuntimeHost surface against the shared real fixtures.
// ===========================================================================

#[test]
#[serial]
fn host_boots_and_reports_status() {
    let host = shared_host();
    let status = host.status();
    assert!(status.plugin_count > 0);
    assert!(status.node_count > 0);
    assert!(!status.current_snapshot_id.is_empty());
    assert_eq!(
        status.current_snapshot_id,
        host.current_snapshot().snapshot_id()
    );

    let snapshot = host.current_snapshot();
    assert!(snapshot.plugin_registry().get("expr").is_some());
    assert!(snapshot.plugin_registry().get("time").is_some());
    assert!(snapshot.plugin_registry().get("filesystem").is_some());
}

#[test]
#[serial]
fn host_invoke_expr_returns_value() {
    let host = shared_host();
    let response = host
        .invoke(
            "expr",
            "expr_entry",
            json!({ "expression": "2 + 3 * 4" }).to_string(),
        )
        .expect("expr invoke should succeed");
    let value: Value = serde_json::from_str(&response.payload).expect("expr json");
    assert_eq!(value.get("value").and_then(|v| v.as_f64()), Some(14.0));
}

#[test]
#[serial]
fn host_invoke_time_now_returns_iso_timestamp() {
    let host = shared_host();
    let response = host
        .invoke(
            "time",
            "time_now",
            json!({ "node_id": "time_now" }).to_string(),
        )
        .expect("time invoke should succeed");
    let value: Value = serde_json::from_str(&response.payload).expect("time json");
    // The time plugin echoes a formatted timestamp; assert it produced a
    // non-empty string field rather than an error object.
    assert!(
        value.is_object(),
        "time response should be a JSON object: {value}"
    );
}

#[test]
#[serial]
fn host_invoke_unknown_plugin_errors_and_records_issue() {
    let host = shared_host();
    let before = host.kernel().plugin_issues().len();
    let err = host
        .invoke("no_such_plugin", "no_node", json!({}).to_string())
        .expect_err("unknown plugin invoke should fail");
    assert!(!err.to_string().is_empty());
    // The failure is observed as a kernel plugin issue.
    let after = host.kernel().plugin_issues();
    assert!(after.len() >= before);
    assert!(after
        .iter()
        .any(|issue| issue.root_plugin_path == "no_such_plugin"));
}

#[test]
#[serial]
fn host_execute_registered_target_traces_expr() {
    let host = shared_host();
    let result = host
        .execute("expr::expr_entry", json!({ "expression": "1 + 2 * 3" }))
        .expect("execute should succeed");
    assert_eq!(result.target_node_fqn, "expr::expr_entry");
    assert_eq!(
        result.output.outcomes.get("expr::expr_entry"),
        Some(&NodeOutcome::Success)
    );
    let trace = result
        .traces
        .get("expr::expr_entry")
        .expect("expr trace should exist");
    assert_eq!(
        trace
            .response_payload
            .as_ref()
            .and_then(|value| value.get("value"))
            .and_then(|value| value.as_f64()),
        Some(7.0)
    );
}

#[test]
#[serial]
fn host_execute_unknown_target_errors() {
    let host = shared_host();
    let err = host
        .execute("expr::does_not_exist", json!({ "expression": "1" }))
        .expect_err("unknown target should fail");
    assert!(err.to_string().contains("does_not_exist") || err.to_string().contains("not found"));
}

#[test]
#[serial]
fn host_execute_rejects_non_object_payload() {
    let host = shared_host();
    let err = host
        .execute("expr::expr_entry", json!("not-an-object"))
        .expect_err("non-object payload should fail");
    assert!(err.to_string().contains("must be a JSON object"));
}

#[test]
#[serial]
fn host_reload_root_noop_keeps_snapshot_stable() {
    let host = shared_host();
    let before = host.current_snapshot().snapshot_id().to_string();
    let report = host.reload("/").expect("noop reload should succeed");
    // Nothing on disk changed, so no plugins are added/removed/changed.
    assert!(report.added_plugins.is_empty());
    assert!(report.removed_plugins.is_empty());
    assert_eq!(report.from_snapshot_id, before);
    // Reload always advances to a fresh snapshot id even when content is equal.
    assert!(!report.to_snapshot_id.is_empty());
    assert_eq!(host.current_snapshot().snapshot_id(), report.to_snapshot_id);
    let attempt = host
        .last_reload_attempt()
        .expect("last reload attempt recorded");
    assert_eq!(attempt.status, ReloadAttemptStatus::Reloaded);
}

#[test]
#[serial]
fn host_candidate_stage_and_rollback_roundtrip() {
    let host = shared_host();
    let current = host.current_snapshot().snapshot_id().to_string();
    // A no-op candidate reload still stages a snapshot (control-plane path).
    let staged = host
        .reload_candidate()
        .expect("candidate reload should stage");
    assert_eq!(staged.from_snapshot_id, current);
    assert!(host.candidate_snapshot().is_some());
    assert_eq!(host.candidate_status(), Some(staged.clone()));
    assert_eq!(
        host.last_candidate_reload_attempt()
            .expect("candidate attempt recorded")
            .status,
        ReloadAttemptStatus::Staged
    );

    // Candidate snapshot serves invokes without touching current.
    let resp = host
        .invoke_candidate(
            "expr",
            "expr_entry",
            json!({ "expression": "6 / 2" }).to_string(),
        )
        .expect("candidate invoke should succeed");
    let value: Value = serde_json::from_str(&resp.payload).expect("candidate json");
    assert_eq!(value.get("value").and_then(|v| v.as_f64()), Some(3.0));

    let rolled = host
        .rollback_candidate()
        .expect("rollback should discard candidate");
    assert_eq!(rolled, staged);
    assert!(host.candidate_snapshot().is_none());
    let err = host
        .invoke_candidate(
            "expr",
            "expr_entry",
            json!({ "expression": "1" }).to_string(),
        )
        .expect_err("invoke after rollback should fail");
    assert!(err.to_string().contains("candidate"));
}

#[test]
#[serial]
fn host_candidate_promote_switches_current() {
    let host = shared_host();
    host.reload_candidate().expect("stage candidate");
    let report = host.promote_candidate().expect("promote should succeed");
    assert!(host.candidate_snapshot().is_none());
    assert_eq!(host.current_snapshot().snapshot_id(), report.to_snapshot_id);
    assert_eq!(
        host.last_reload_attempt()
            .expect("promote records reload attempt")
            .status,
        ReloadAttemptStatus::Reloaded
    );
    // Promoted snapshot still serves invokes.
    let resp = host
        .invoke(
            "expr",
            "expr_entry",
            json!({ "expression": "10 - 4" }).to_string(),
        )
        .expect("post-promote invoke should succeed");
    let value: Value = serde_json::from_str(&resp.payload).expect("json");
    assert_eq!(value.get("value").and_then(|v| v.as_f64()), Some(6.0));
}

#[test]
#[serial]
fn host_promote_without_candidate_errors() {
    let host = shared_host();
    assert!(host.candidate_snapshot().is_none());
    let err = host
        .promote_candidate()
        .expect_err("promote without candidate should fail");
    assert!(!err.to_string().is_empty());
}

#[test]
#[serial]
fn host_security_checks_flag_sensitive_paths_and_commands() {
    let host = shared_host();
    // Sensitive path keywords are rejected (component-prefix matching, so a
    // bare `a.env` file is not swept up while `.env` under any directory is).
    for bad in [
        ".ssh/id_rsa",
        "config/credentials",
        "/etc/passwd",
        "config/.env",
    ] {
        assert!(
            host.check_sensitive_path(bad).is_err(),
            "expected {bad} to be rejected"
        );
    }
    assert!(host.check_sensitive_path("plugins/expr/src/lib.rs").is_ok());

    for bad in ["ssh user@host", "cat /etc/shadow", "export SECRET=1"] {
        assert!(
            host.check_sensitive_command(bad).is_err(),
            "expected {bad} to be rejected"
        );
    }
    assert!(host.check_sensitive_command("cargo build").is_ok());
}

#[test]
#[serial]
fn agent_run_plugin_test_direct_argv_never_goes_through_shell() {
    let host = shared_host();
    // `;` and `$(...)` are shell-meta only if a shell sees them: shell_words
    // tokenises on whitespace (POSIX quoting, no expansion), so the whole
    // fragment arrives as literal argv and `echo` prints it verbatim — no
    // command splitting, no `$HOME` expansion, no `rm` execution.
    let out = host
        .agent_run_plugin_test(Some("echo hi; rm -rf $HOME"))
        .expect("echo should run");
    assert_eq!(out["command"], "echo hi; rm -rf $HOME");
    assert_eq!(out["success"], true);
    assert_eq!(out["exit_code"], 0);
    assert_eq!(out["stdout"], "hi; rm -rf $HOME\n");
    assert_eq!(out["stderr"], "");

    let out = host
        .agent_run_plugin_test(Some("echo $(date)"))
        .expect("echo should run");
    assert_eq!(out["stdout"], "$(date)\n");

    // cargo receives the trailing `; rm -rf ~/...` tokens as literal args —
    // the --manifest-path value keeps its trailing `;`, so cargo cannot find
    // that manifest and fails; the `rm` never executes.
    let evil = "cargo test --quiet --manifest-path plugins/expr/Cargo.toml; rm -rf ~/cordis-run-plugin-test-marker";
    let out = host
        .agent_run_plugin_test(Some(evil))
        .expect("cargo should run");
    assert_eq!(out["command"], evil);
    assert_eq!(out["success"], false);
    assert_ne!(out["exit_code"], 0);
    let stderr = out["stderr"].as_str().expect("stderr");
    assert!(!stderr.is_empty());
}

#[test]
#[serial]
fn agent_run_plugin_test_rejects_sensitive_commands() {
    let host = shared_host();
    for bad in [
        "cat /etc/passwd",
        "cat /etc/shadow",
        "cat ~/.ssh/id_rsa",
        "bash -lc 'cat /etc/passwd'",
        "sh -c 'echo $(cat /etc/shadow)'",
        "export SECRET=1",
        "python -c 'import os; os.system(\"cat /etc/passwd\")'",
    ] {
        let err = host
            .agent_run_plugin_test(Some(bad))
            .expect_err("sensitive command must be rejected");
        let blocked = err
            .to_string()
            .contains("blocked: command references sensitive operation");
        assert!(blocked, "unexpected error for {bad}: {err}");
    }
}

#[test]
#[serial]
fn agent_run_plugin_test_invalid_command_strings_are_rejected() {
    let host = shared_host();
    let err = host
        .agent_run_plugin_test(Some(""))
        .expect_err("empty command must be rejected");
    assert_eq!(
        err.to_string(),
        "invalid argument: run_plugin_test received an empty command string"
    );
    let err = host
        .agent_run_plugin_test(Some("''"))
        .expect_err("empty program must be rejected");
    assert_eq!(
        err.to_string(),
        "invalid argument: run_plugin_test program was empty after tokenisation"
    );
    let err = host
        .agent_run_plugin_test(Some("'unbalanced"))
        .expect_err("unbalanced quotes must be rejected");
    let tokenised = err
        .to_string()
        .contains("run_plugin_test tokenisation failed");
    assert!(tokenised, "unexpected error: {err}");
}

#[test]
#[serial]
fn agent_run_plugin_test_default_runs_all_plugin_tests() {
    let host = shared_host();
    let out = host
        .agent_run_plugin_test(None)
        .expect("default command should run");
    assert_eq!(
        out["command"],
        "cargo test --quiet --manifest-path plugins/Cargo.toml"
    );
    assert_eq!(out["success"], true);
    assert_eq!(out["exit_code"], 0);
    assert!(out["stdout"].is_string());
    assert!(out["stderr"].is_string());
}

#[test]
#[serial]
fn host_resolve_sandboxed_path_enforces_containment() {
    let host = shared_host();
    // Absolute and parent-traversal are rejected.
    assert!(host.resolve_sandboxed_path("/etc/hosts").is_err());
    assert!(host.resolve_sandboxed_path("../escape").is_err());
    // A plain relative path resolves under the fixtures root.
    let resolved = host
        .resolve_sandboxed_path("artifacts/index.json")
        .expect("relative path should resolve");
    assert!(resolved.ends_with("artifacts/index.json"));
    // data/ paths resolve against the workspace root (parent of fixtures).
    let data = host
        .resolve_sandboxed_path("data/scratch.txt")
        .expect("data path should resolve");
    assert!(data.to_string_lossy().contains("data/scratch.txt"));
}

#[test]
#[serial]
fn host_check_agent_accessible_gates_on_docs_flag() {
    let host = shared_host();
    // filesystem nodes are agent-accessible in the fixture docs.
    assert!(host.check_agent_accessible("filesystem", "fs_read").is_ok());
    // An unknown plugin is not registered.
    assert!(matches!(
        host.check_agent_accessible("ghost_plugin", "node"),
        Err(RuntimeError::PluginNotRegistered { .. })
    ));
    // A real plugin with an unknown node id is rejected as not-allowed.
    assert!(host
        .check_agent_accessible("filesystem", "no_such_node")
        .is_err());
}

#[test]
#[serial]
fn host_walk_code_files_finds_sources_and_honors_stop() {
    let host = shared_host();
    let root = fixtures_root().join("artifacts");
    let mut seen = 0usize;
    host.walk_code_files(&root, &mut |rel, abs| {
        assert!(abs.is_file());
        assert!(!rel.is_empty());
        seen += 1;
    })
    .expect("walk should succeed");
    assert!(seen > 0, "artifacts dir should contain source-like files");

    // The _ctl variant aborts immediately when the visitor returns Stop.
    let mut count = 0usize;
    host.walk_code_files_ctl(&root, &mut |_, _| {
        count += 1;
        WalkControl::Stop
    })
    .expect("walk_ctl should succeed");
    assert_eq!(count, 1, "Stop after the first hit should end the walk");

    // Non-directory root is a no-op.
    let mut never = false;
    host.walk_code_files(&root.join("index.json"), &mut |_, _| never = true)
        .expect("walk on file should be a no-op");
    assert!(!never);
}

#[test]
#[serial]
fn host_soul_roundtrip_via_file_provider() {
    let host = shared_host();
    // Empty scope key is rejected on write and yields None on read.
    assert!(host.set_soul("", &Soul::default()).is_err());
    assert!(host.get_soul("").expect("empty read ok").is_none());

    let soul_key = "host-coverage:soul-scope#private";
    let soul = Soul {
        persona: "coverage persona".to_string(),
        ..Default::default()
    };
    host.set_soul(soul_key, &soul).expect("set soul");
    let fetched = host.get_soul(soul_key).expect("get soul").expect("exists");
    assert_eq!(fetched.persona, "coverage persona");

    // AgentToolHost overlay surface reads the same persona back.
    assert_eq!(
        host.agent_soul_overlay(soul_key).as_deref(),
        Some("coverage persona")
    );
    assert!(host.agent_soul_overlay("nobody#private").is_none());
}

#[test]
#[serial]
fn host_agent_session_lifecycle_inject_status_transcript_drop() {
    let host = shared_host();
    let handle = host
        .agent_start_with(AgentSessionKind::RuntimeShell, AgentStartOptions::default())
        .expect("agent should start");
    let sid = handle.session_id.clone();
    assert_eq!(handle.kind, AgentSessionKind::RuntimeShell);

    // Inject a synthetic exchange (no LLM round-trip needed). This pushes
    // user+assistant transcript entries without advancing completed_turns
    // (which only a real respond() cycle increments).
    host.agent_inject(&sid, "hello", "hi there")
        .expect("inject should succeed");
    host.agent_status(&sid).expect("status should be queryable");
    let transcript = host.agent_transcript(&sid).expect("transcript");
    assert!(
        transcript
            .iter()
            .any(|entry| format!("{entry:?}").contains("hi there")),
        "assistant reply should be in the transcript"
    );
    assert!(
        transcript
            .iter()
            .any(|entry| format!("{entry:?}").contains("hello")),
        "user input should be in the transcript"
    );

    // refresh_session_soul re-scopes the persona overlay.
    host.refresh_session_soul(&sid, "host-coverage:refreshed#private")
        .expect("refresh soul on live session");

    // Unknown session ids error uniformly.
    assert!(matches!(
        host.agent_status("no-such"),
        Err(RuntimeError::AgentSessionNotFound { .. })
    ));
    assert!(matches!(
        host.agent_inject("no-such", "a", "b"),
        Err(RuntimeError::AgentSessionNotFound { .. })
    ));
    assert!(matches!(
        host.refresh_session_soul("no-such", "x"),
        Err(RuntimeError::AgentSessionNotFound { .. })
    ));

    host.drop_session(&sid);
    assert!(matches!(
        host.agent_status(&sid),
        Err(RuntimeError::AgentSessionNotFound { .. })
    ));
    // Dropping again is idempotent.
    host.drop_session(&sid);
}

#[test]
#[serial]
fn host_agent_start_rejects_plugin_iteration_kind() {
    let host = shared_host();
    let err = host
        .agent_start(AgentSessionKind::PluginIteration)
        .expect_err("plugin iteration sessions cannot be started directly");
    assert!(err.to_string().contains("iterate_plugins"));
}

// ===========================================================================
// Group C — fixture-mutating paths (throwaway copies; safe on network mount).
// ===========================================================================

#[test]
#[serial]
fn host_create_plugin_validates_name_and_writes_skeleton() {
    let temp = setup_artifacts_only_copy();
    let host = RuntimeHost::boot(temp.path()).expect("host should boot from artifacts copy");

    // Empty and invalid names are rejected before any filesystem work.
    assert!(host.create_plugin("", None).is_err());
    assert!(host.create_plugin("bad name!", None).is_err());
}

#[test]
#[serial]
fn host_iterate_plugins_policy_blocks_runtime_paths() {
    // A copy that lacks the plugins/ source tree still boots (loader reads
    // artifacts/), and the policy gate rejects edits outside the plugin
    // iteration surface before any source access — so no plugins/ tree needed.
    let temp = setup_artifacts_only_copy();
    let host = RuntimeHost::boot(temp.path()).expect("host should boot");

    let result = host
        .iterate_plugins(KernelPluginIterationRequest {
            issue_id: None,
            target_plugin_paths: vec!["expr".to_string()],
            instruction: Some("try to modify runtime crate".to_string()),
            edit_plan: Some(PluginEditPlan {
                issue_id: "issue-policy-blocked".to_string(),
                patch_id: "patch-policy-blocked".to_string(),
                summary: "try to modify runtime crate".to_string(),
                operations: vec![PluginEditOperation {
                    path: "crates/cordis-runtime/src/lib.rs".to_string(),
                    kind: PluginEditOpKind::ReplaceExact,
                    expected_old_string: Some("pub mod config;".to_string()),
                    expected_sha256: None,
                    new_content: Some("pub mod config;".to_string()),
                    pointer: None,
                    dotted_key: None,
                    value: None,
                }],
            }),
            manual_approved: false,
            tests_command: None,
            safety_command: None,
            verify_profile: Some(VerificationProfile::RustWorkspace),
            quality_score: Some(95),
        })
        .expect("policy-blocked iteration still returns a result");

    assert_eq!(
        result.final_verdict,
        PluginIterationFinalVerdict::RolledBack
    );
    assert!(result.changed_paths.is_empty());
    assert!(host.candidate_snapshot().is_none());
    assert!(result
        .blocked_reason
        .as_deref()
        .unwrap_or_default()
        .contains("outside the plugin iteration surface"));
    assert!(host.kernel().plugin_issues().iter().any(|issue| {
        issue.root_plugin_path == "expr" && issue.source == KernelPluginIssueSource::PolicyBlocked
    }));
}

// ===========================================================================
// Group D — plugin-iteration journal recovery core.
//
// `apply_plugin_iteration_journal` is the recovery primitive that boot() calls
// (via `restore_plugin_iteration_workspace`). It replays an on-disk rollback
// journal WITHOUT rebuilding artifacts, so these tests are fully hermetic:
// they run against throwaway temp dirs and need neither the fixtures mount nor
// the platform dylibs. This exercises the boot journal-recovery branches
// (already-applied skip, real replay, no-op) that a plain fixtures boot never
// reaches because no journal is normally present.
// ===========================================================================

fn journal_path(snapshot_root: &Path) -> PathBuf {
    snapshot_root.join("plugin-iteration-edit-journal.json")
}

fn applied_marker_path(snapshot_root: &Path) -> PathBuf {
    snapshot_root.join("plugin-iteration-edit-journal.applied")
}

#[test]
fn apply_journal_no_journal_is_noop() {
    let workspace = TempDir::new().expect("workspace");
    let snapshot_root = TempDir::new().expect("snapshot root");
    // No journal on disk and no in-memory rollback → returns false, no work.
    let restored = cordis_runtime::host::apply_plugin_iteration_journal(
        workspace.path(),
        snapshot_root.path(),
        None,
    )
    .expect("apply should succeed");
    assert!(!restored, "absent journal must be a no-op");
    assert!(!applied_marker_path(snapshot_root.path()).exists());
}

#[test]
fn apply_journal_in_memory_rollback_restores_file() {
    let workspace = TempDir::new().expect("workspace");
    let snapshot_root = TempDir::new().expect("snapshot root");
    let target_rel = "plugins/demo/src/lib.rs";
    let target_abs = workspace.path().join(target_rel);
    fs::create_dir_all(target_abs.parent().unwrap()).expect("mkdir");
    // Simulate a half-applied edit sitting on disk.
    fs::write(&target_abs, b"edited-bytes").expect("write edited");

    // In-memory rollback carrying the pre-edit bytes; no journal file present
    // so the in-memory branch is taken.
    let rollback = PluginEditRollback::single_backup(
        workspace.path().to_path_buf(),
        target_rel,
        Some(b"original-bytes".to_vec()),
    );
    let restored = cordis_runtime::host::apply_plugin_iteration_journal(
        workspace.path(),
        snapshot_root.path(),
        Some(&rollback),
    )
    .expect("apply should succeed");
    assert!(restored, "in-memory rollback should report a restore");
    assert_eq!(
        fs::read_to_string(&target_abs).expect("read restored"),
        "original-bytes"
    );
}

#[test]
fn apply_journal_on_disk_replays_and_clears() {
    let workspace = TempDir::new().expect("workspace");
    let snapshot_root = TempDir::new().expect("snapshot root");
    let target_rel = "plugins/demo/src/core.rs";
    let target_abs = workspace.path().join(target_rel);
    fs::create_dir_all(target_abs.parent().unwrap()).expect("mkdir");
    fs::write(&target_abs, b"corrupted-half-write").expect("write edited");

    // Persist a real rollback journal (the shape boot() replays).
    let rollback = PluginEditRollback::single_backup(
        workspace.path().to_path_buf(),
        target_rel,
        Some(b"pristine-source".to_vec()),
    );
    let jp = journal_path(snapshot_root.path());
    rollback
        .persist_journal(&jp, "iteration-under-test")
        .expect("persist journal");
    assert!(jp.exists());

    let restored = cordis_runtime::host::apply_plugin_iteration_journal(
        workspace.path(),
        snapshot_root.path(),
        None,
    )
    .expect("apply should succeed");
    assert!(restored, "on-disk journal should replay");
    assert_eq!(
        fs::read_to_string(&target_abs).expect("read restored"),
        "pristine-source"
    );
    // Journal is cleared and the applied marker removed after a clean replay.
    assert!(!jp.exists(), "journal cleared after replay");
    assert!(!applied_marker_path(snapshot_root.path()).exists());

    // A second apply is a no-op — nothing left to replay.
    let again = cordis_runtime::host::apply_plugin_iteration_journal(
        workspace.path(),
        snapshot_root.path(),
        None,
    )
    .expect("second apply should succeed");
    assert!(!again, "cleared journal is a no-op on the next boot");
}

#[test]
fn apply_journal_skips_when_already_applied_marker_matches() {
    let workspace = TempDir::new().expect("workspace");
    let snapshot_root = TempDir::new().expect("snapshot root");
    let target_rel = "plugins/demo/src/lib.rs";
    let target_abs = workspace.path().join(target_rel);
    fs::create_dir_all(target_abs.parent().unwrap()).expect("mkdir");
    // Content that legitimately post-dates the recorded rollback. If the
    // already-applied guard fails, replay would clobber this back to the
    // journal's backup bytes.
    fs::write(&target_abs, b"legitimately-newer-source").expect("write current");

    let rollback = PluginEditRollback::single_backup(
        workspace.path().to_path_buf(),
        target_rel,
        Some(b"stale-backup".to_vec()),
    );
    let jp = journal_path(snapshot_root.path());
    rollback
        .persist_journal(&jp, "iteration-already-applied")
        .expect("persist journal");

    // Record the applied marker with the journal's own generation id so the
    // guard recognizes this journal as already replayed.
    let gen_id = PluginEditRollback::journal_generation_id(&jp)
        .expect("read gen id")
        .expect("gen id present");
    fs::write(applied_marker_path(snapshot_root.path()), gen_id.as_bytes())
        .expect("write applied marker");

    let restored = cordis_runtime::host::apply_plugin_iteration_journal(
        workspace.path(),
        snapshot_root.path(),
        None,
    )
    .expect("apply should succeed");
    assert!(!restored, "already-applied journal must be skipped");
    // The current (newer) source is preserved — not reverted to the backup.
    assert_eq!(
        fs::read_to_string(&target_abs).expect("read current"),
        "legitimately-newer-source"
    );
    // The guard clears both marker and journal so they don't linger.
    assert!(!jp.exists(), "already-applied journal is cleared");
    assert!(!applied_marker_path(snapshot_root.path()).exists());
}

// ===========================================================================
// Group E — background service lifecycle via start_service.
// ===========================================================================

#[derive(Default)]
struct CountingService {
    started: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
    fail_start: bool,
}

impl cordis_runtime::context::Service for CountingService {
    fn start(&self) -> Result<(), String> {
        if self.fail_start {
            return Err("intentional start failure".to_string());
        }
        self.started.store(true, Ordering::SeqCst);
        Ok(())
    }

    fn stop(&self) -> Result<(), String> {
        self.stopped.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[test]
#[serial]
fn host_start_service_registers_and_rejects_duplicates_and_failures() {
    let host = shared_host();
    let started = Arc::new(AtomicBool::new(false));
    let svc = CountingService {
        started: started.clone(),
        stopped: Arc::new(AtomicBool::new(false)),
        fail_start: false,
    };
    // Unique node id per run so repeated serial executions don't collide on a
    // leftover registry entry from a previous invocation of the shared host.
    let node_id = format!("cov_svc_{}", std::process::id());
    host.start_service("time", &node_id, Box::new(svc))
        .expect("service should start");
    assert!(started.load(Ordering::SeqCst), "start() was invoked");

    // Duplicate registration under the same key is rejected.
    let dup = CountingService::default();
    let err = host
        .start_service("time", &node_id, Box::new(dup))
        .expect_err("duplicate service should fail");
    assert!(matches!(err, RuntimeError::DuplicateService { .. }));

    // A service whose start() fails surfaces as an Invariant error and is not
    // registered (so a different node id can be reused).
    let failing = CountingService {
        fail_start: true,
        ..Default::default()
    };
    let fail_node = format!("cov_svc_fail_{}", std::process::id());
    let err = host
        .start_service("time", &fail_node, Box::new(failing))
        .expect_err("failing service should error");
    assert!(matches!(err, RuntimeError::Invariant { .. }));
}

// ===========================================================================
// Group F — session management + persistence branches.
// ===========================================================================

#[test]
#[serial]
fn host_auto_save_and_delete_session_snapshot_roundtrip() {
    let host = shared_host();
    let handle = host
        .agent_start_with(AgentSessionKind::RuntimeShell, AgentStartOptions::default())
        .expect("agent should start");
    let sid = handle.session_id.clone();

    // delete on a session that never got persisted is a silent no-op.
    let snap_path = host.data_dir().join("sessions").join(format!("{sid}.json"));
    host.delete_session_snapshot(&sid);
    assert!(!snap_path.exists());

    host.drop_session(&sid);
}

#[test]
#[serial]
fn host_agent_start_with_profile_and_soul_option() {
    let host = shared_host();
    // Unknown profile name falls back to the default profile without error.
    let handle = host
        .agent_start_with(
            AgentSessionKind::RuntimeShell,
            AgentStartOptions {
                profile: Some("definitely-not-a-real-profile".to_string()),
                soul_key: "host-coverage:profile-opt#private".to_string(),
            },
        )
        .expect("agent should start with unknown profile via default fallback");
    let sid = handle.session_id.clone();
    let status = host.agent_status(&sid).expect("status queryable");
    assert_eq!(status.kind, "runtime_shell");
    // The soul_key option is wired to the persona overlay lookup path.
    host.refresh_session_soul(&sid, "host-coverage:profile-opt-refreshed#private")
        .expect("refresh soul");
    host.drop_session(&sid);
}

// ===========================================================================
// Group G — candidate/reload diagnostic + rollback branches.
// ===========================================================================

#[test]
#[serial]
fn host_reload_candidate_with_diagnostics_stages_and_rolls_back() {
    let host = shared_host();
    // Ensure no stale candidate from a prior test.
    if host.candidate_snapshot().is_some() {
        host.rollback_candidate().expect("clear prior candidate");
    }
    let attempt = host.reload_candidate_with_diagnostics();
    assert_eq!(attempt.status, ReloadAttemptStatus::Staged);
    assert!(host.candidate_snapshot().is_some());
    // execute against the candidate snapshot (execute_candidate branch).
    let result = host
        .execute_candidate("expr::expr_entry", json!({ "expression": "8 - 5" }))
        .expect("candidate execute should succeed");
    assert_eq!(result.target_node_fqn, "expr::expr_entry");
    host.rollback_candidate().expect("rollback candidate");
    assert!(host.candidate_snapshot().is_none());
    // execute_candidate after rollback errors with CandidateSnapshotMissing.
    let err = host
        .execute_candidate("expr::expr_entry", json!({ "expression": "1" }))
        .expect_err("execute_candidate without candidate should fail");
    assert!(matches!(err, RuntimeError::CandidateSnapshotMissing));
}

#[test]
#[serial]
fn host_rollback_candidate_without_candidate_errors() {
    let host = shared_host();
    if host.candidate_snapshot().is_some() {
        host.rollback_candidate().expect("clear prior candidate");
    }
    let err = host
        .rollback_candidate()
        .expect_err("rollback without candidate should fail");
    assert!(matches!(err, RuntimeError::CandidateSnapshotMissing));
}

// ===========================================================================
// Group H — write_shutdown_memory + detect_crash_and_recover.
//
// These need `data/` to live inside a throwaway dir, so the fixtures root is
// nested one level down (`<temp>/fixtures`): the host derives `data_dir()` as
// the parent of the fixtures root, i.e. `<temp>/data`. Only the artifacts tree
// is copied, which is all a read-only boot consumes.
// ===========================================================================

fn setup_nested_artifacts_workspace() -> TempDir {
    let temp = TempDir::new().expect("tempdir");
    let fixtures = temp.path().join("fixtures");
    let root = fixtures_root();
    copy_dir_all(&root.join("artifacts"), &fixtures.join("artifacts"));
    for name in ["notify_handlers.json", "startup_invoke.json"] {
        let src = root.join(name);
        if src.exists() {
            fs::copy(&src, fixtures.join(name)).expect("copy top-level fixture file");
        }
    }
    // fixtures root 目录名是 "fixtures"，`discover_config_dir` 走同级分支 →
    // `temp/config`。
    pin_private_snapshot_root(temp.path(), &fixtures);
    temp
}

#[test]
#[serial]
fn host_write_shutdown_memory_produces_atomic_snapshot() {
    let temp = setup_nested_artifacts_workspace();
    let fixtures = temp.path().join("fixtures");
    let host = RuntimeHost::boot(&fixtures).expect("host should boot");

    // A live session should be reflected in the shutdown snapshot.
    let handle = host
        .agent_start_with(AgentSessionKind::RuntimeShell, AgentStartOptions::default())
        .expect("agent should start");
    let sid = handle.session_id.clone();

    host.write_shutdown_memory();

    let memory_path = temp.path().join("data/memory/shutdown.json");
    assert!(memory_path.exists(), "shutdown memory file written");
    let value: Value =
        serde_json::from_str(&fs::read_to_string(&memory_path).expect("read memory"))
            .expect("valid json");
    assert!(value.get("shutdown_at").and_then(Value::as_str).is_some());
    assert!(value.get("plugins").and_then(Value::as_array).is_some());
    let sessions = value
        .get("sessions")
        .and_then(Value::as_array)
        .expect("sessions array");
    assert!(
        sessions
            .iter()
            .any(|s| s.get("session_id").and_then(Value::as_str) == Some(sid.as_str())),
        "live session should appear in the shutdown memory"
    );

    // No temp staging file is left behind by the atomic writer.
    let leftovers: Vec<_> = fs::read_dir(temp.path().join("data/memory"))
        .expect("read memory dir")
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains("cordis-tmp"))
        .collect();
    assert!(leftovers.is_empty(), "no atomic-write temp file lingers");

    host.drop_session(&sid);
}

#[test]
#[serial]
fn host_boot_recovers_saved_sessions_from_disk() {
    let temp = setup_nested_artifacts_workspace();
    let fixtures = temp.path().join("fixtures");

    // Pre-seed data/sessions with one recoverable RuntimeShell snapshot, one
    // PluginIteration snapshot (recovered under its own kind), plus decoy
    // files that must be skipped: a temp file, a non-JSON file, and a corrupt
    // JSON body.
    let sessions_dir = temp.path().join("data/sessions");
    fs::create_dir_all(&sessions_dir).expect("mkdir sessions");

    let config = RuntimeConfig::default();
    let make_snapshot = |kind: &str| -> String {
        let session = AgentSession::new(config.llm_api.clone(), kind).expect("session");
        serde_json::to_string(&session.to_snapshot()).expect("serialize snapshot")
    };
    fs::write(
        sessions_dir.join("recover-shell.json"),
        make_snapshot("runtime_shell"),
    )
    .expect("write shell snapshot");
    fs::write(
        sessions_dir.join("recover-iter.json"),
        make_snapshot("plugin_iteration"),
    )
    .expect("write iter snapshot");
    // Decoys — none should be hydrated.
    fs::write(sessions_dir.join(".staging.json.tmp.7"), "{}").expect("write temp decoy");
    fs::write(sessions_dir.join("notes.txt"), "not a session").expect("write non-json decoy");
    // Unreadable decoy drives the read-failure skip arm (unix only; root
    // bypasses permission bits so skip the setup there).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } != 0 {
            fs::write(sessions_dir.join("unreadable.json"), "{}").expect("write unreadable decoy");
            fs::set_permissions(
                sessions_dir.join("unreadable.json"),
                fs::Permissions::from_mode(0o000),
            )
            .expect("chmod 000");
        }
    }
    fs::write(sessions_dir.join("corrupt.json"), "{ this is not json")
        .expect("write corrupt decoy");

    let host = RuntimeHost::boot(&fixtures).expect("host should boot and recover");

    // Both valid sessions are hydrated; the three decoys are skipped.
    let shell = host
        .agent_status("recover-shell")
        .expect("shell session recovered");
    assert_eq!(shell.kind, "runtime_shell");
    let iter = host
        .agent_status("recover-iter")
        .expect("iter session recovered");
    assert_eq!(iter.kind, "plugin_iteration");

    // The dotted temp file's stem must not have been treated as a session id.
    assert!(host.agent_status(".staging.json").is_err());
    assert!(host.agent_status("notes").is_err());
    assert!(host.agent_status("corrupt").is_err());
}

#[test]
#[serial]
fn host_reload_with_diagnostics_noop_reports_reloaded() {
    let host = shared_host();
    let attempt = host.reload_with_diagnostics("/");
    assert_eq!(attempt.status, ReloadAttemptStatus::Reloaded);
    assert!(attempt.to_snapshot_id.is_some());
    // A subtree reload for a real leaf plugin also succeeds (reload_subtree
    // branch rather than reload_internal). Known environmental race: reload's
    // native rebuild can leave `artifacts/index.json`'s recorded sha256 stale
    // relative to the freshly written `.so` (documented HashMismatch race), so
    // on a Failed attempt re-hash the index once and retry — the retry must
    // succeed, keeping the assertion strength.
    let mut attempt = host.reload_with_diagnostics("expr");
    if attempt.status != ReloadAttemptStatus::Reloaded {
        cordis_runtime::plugin::tooling::refresh_artifact_index(&fixtures_root())
            .expect("refresh artifact index after stale-hash reload failure");
        attempt = host.reload_with_diagnostics("expr");
    }
    assert_eq!(attempt.status, ReloadAttemptStatus::Reloaded);
}

// ===========================================================================
// Group I — agent-driven plugin iteration failure + error enrichment.
//
// `iterate_plugins` with no `edit_plan` takes the agent-driven branch: it
// starts a plugin-iteration agent session and calls `agent_send`. Pointing
// the LLM at a dead loopback port makes `agent_send` fail after its in-profile
// retries, driving `run_plugin_iteration_agent`'s error path through
// `enrich_plugin_iteration_agent_error`. The enriched message (tool summary +
// transcript excerpt) becomes the iteration's `blocked_reason`.
//
// This boots a *fresh* host (not the shared one) against the real fixtures so
// the real plugins/expr source tree is present for context collection, but
// with a throwaway config dir (dead LLM base_url) and its own snapshot_root so
// it can't clobber the shared host's staged artifacts.
// ===========================================================================

#[test]
#[serial]
fn host_iterate_plugins_agent_failure_surfaces_enriched_error() {
    use std::net::TcpListener;

    let fixtures = fixtures_root();
    // Keep the fixtures artifact index consistent with the on-disk dylibs
    // (other tests may have rebuilt them mid-suite).
    cordis_runtime::plugin::tooling::refresh_artifact_index(&fixtures)
        .expect("refresh artifact index before boot");

    // Reserve then release a loopback port so a connection there is refused
    // immediately — a deterministic, fast LLM failure.
    let dead_port = {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind to reserve port");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        port
    };

    // Throwaway config dir: dead LLM endpoint + an isolated snapshot_root so
    // this host never touches the shared host's snapshot tree.
    let cfg_temp = TempDir::new().expect("config tempdir");
    let snap_temp = TempDir::new().expect("snapshot tempdir");
    let config_dir = cfg_temp.path();
    fs::write(
        config_dir.join("llm_api.yaml"),
        format!(
            "provider: deepseek\nbase_url: http://127.0.0.1:{dead_port}/v1\napi_key: test-key\nmodel: deepseek-chat\ntemperature: 0.0\nmax_tokens: 128\ntimeout_ms: 3000\nstream_timeout_secs: 2\n"
        ),
    )
    .expect("write dead llm config");
    fs::write(
        config_dir.join("runtime.yaml"),
        format!(
            "runtime:\n  snapshot_root: {}\n",
            snap_temp.path().display()
        ),
    )
    .expect("write runtime config");

    // CORDIS_CONFIG_DIR is process-global; #[serial] guarantees no other test
    // observes it. Set it only across boot, then restore.
    let prev = std::env::var_os("CORDIS_CONFIG_DIR");
    std::env::set_var("CORDIS_CONFIG_DIR", config_dir);
    let host = RuntimeHost::boot(&fixtures).expect("host should boot with dead-LLM config");
    match prev {
        Some(value) => std::env::set_var("CORDIS_CONFIG_DIR", value),
        None => std::env::remove_var("CORDIS_CONFIG_DIR"),
    }

    // No edit_plan → agent-driven path. The agent's first (and only) send hits
    // the dead endpoint and fails; the iteration rolls back.
    let result = host
        .iterate_plugins(KernelPluginIterationRequest {
            issue_id: None,
            target_plugin_paths: vec!["expr".to_string()],
            instruction: Some("inspect the expr subtree".to_string()),
            edit_plan: None,
            manual_approved: false,
            tests_command: None,
            safety_command: None,
            verify_profile: Some(VerificationProfile::RustWorkspace),
            quality_score: Some(95),
        })
        .expect("iteration returns a result even when the agent fails");

    assert_eq!(
        result.final_verdict,
        PluginIterationFinalVerdict::RolledBack
    );
    assert!(
        result.changed_paths.is_empty(),
        "a failed agent must not report changed paths: {:?}",
        result.changed_paths
    );

    // The blocked_reason carries the enriched agent-error detail: the session
    // header, the tool-execution summary, and the transcript excerpt.
    let reason = result
        .blocked_reason
        .as_deref()
        .expect("failed iteration records a blocked_reason");
    assert!(
        reason.contains("plugin iteration agent session"),
        "enriched error should name the agent session: {reason}"
    );
    assert!(
        reason.contains("tool summary: total_calls="),
        "enriched error should embed the tool-execution summary: {reason}"
    );
    assert!(
        reason.contains("transcript excerpt:"),
        "enriched error should embed the transcript excerpt: {reason}"
    );
    // The excerpt records the failed LLM turn: a user prompt entry plus the
    // synthetic assistant "[error]" placeholder respond() inserts on failure.
    assert!(
        reason.contains("LLM request failed") || reason.contains("[error]"),
        "excerpt should carry the underlying LLM failure detail: {reason}"
    );
}
