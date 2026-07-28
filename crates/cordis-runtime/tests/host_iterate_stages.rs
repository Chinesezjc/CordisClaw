//! Stage-failure coverage for the `iterate_plugins` pipeline in `host.rs`.
//!
//! `host_build_coverage.rs` drives the *happy* paths (full promote, blocked +
//! approve, rollback) against a natively-compiled `mini` dylib. This file
//! covers the per-stage `fail_stage(...)` arms and the observation branches
//! that only run when a stage errors or returns a negative verdict:
//!
//!   - Step 2 `edit`: `persist_journal` failure;
//!   - Step 3 `rebuild`: `rebuild_plugin_workspace` failure and the artifact
//!     backup / journal re-persist seam;
//!   - Step 4 `stage_candidate`: `reload_candidate` failure;
//!   - Step 5 `verify`: a non-spawnable verifier command (`Err` → stage
//!     failure) and a failing one (`Fail` verdict → `observe_plugin_issue`);
//!   - Step 6 `canary`: the `CanaryVerdict::Fail` → `observe_plugin_issue`
//!     branch and the canary `Err` → `fail_stage("canary")` arm;
//!   - `approve_blocked_iteration`'s promote-failure reinsert arm.
//!
//! The fixture is a **JSON-artifact** plugin (`demo`, no `[lib] crate-type =
//! ["dylib"]`), so `prepare_artifacts` materializes `artifacts/demo.json`
//! without shelling out to `cargo build`, and boot is a pure file read. The
//! plugin answers invocations through a `process` execution pointing at a
//! shell script, which makes both `invoke` and `invoke_candidate` real without
//! any compilation.
//!
//! Two responder placements matter:
//!
//!   * **Absolute** command path (outside `fixtures/artifacts`): candidate
//!     staging skips absolute commands, and `prepare_artifacts(Full)` — which
//!     wipes and recreates `artifacts/` — cannot delete it. Used by every test
//!     that needs the candidate to load.
//!   * **Relative** command path (`demo_verify.sh` next to the artifact): the
//!     candidate snapshot stages the command into the staged artifact root, so
//!     a missing script makes `reload_candidate` fail. Used by the
//!     `stage_candidate` test.
//!
//! Every edit plan here touches `plugins/demo/Cargo.toml`, which puts Step 3
//! on the full `rebuild_fixture_artifacts` path. That path regenerates the
//! JSON artifact from sources and never shells out to `cargo build`, so the
//! whole file runs in seconds.

use std::fs;
use std::path::{Path, PathBuf};

use cordis_runtime::core::error::RuntimeError;
use cordis_runtime::host::RuntimeHost;
use cordis_runtime::kernel::plugin_iteration::{
    CanaryVerdict, KernelPluginIssueSource, KernelPluginIterationRequest, PluginEditOpKind,
    PluginEditOperation, PluginEditPlan, PluginIterationFinalVerdict, VerifierVerdict,
};
use cordis_runtime::kernel::verifier::VerificationProfile;
use cordis_runtime::plugin::tooling::{prepare_artifacts, PrepareMode};
use serde_json::json;
use serial_test::serial;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// Where the plugin's `process` execution command points.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Responder {
    /// `<temp>/responder.sh` — outside `artifacts/`, so it survives a full
    /// rebuild and candidate staging skips it (absolute commands are not
    /// copied into the staged artifact root).
    Absolute,
    /// `demo_verify.sh`, resolved next to the artifact. Every snapshot build
    /// (boot included) copies it into its staged artifact root, so boot needs
    /// it present — but `prepare_artifacts(Full)` wipes `artifacts/`, so a
    /// manifest-touching iteration deletes it mid-run and the following
    /// `reload_candidate` fails.
    RelativeWipedByRebuild,
}

/// `<snapshot_root>/plugin-iteration-edit-journal.json`, mirroring the private
/// `plugin_iteration_journal_path` helper.
fn journal_path(snapshot_root: &str) -> PathBuf {
    Path::new(snapshot_root).join("plugin-iteration-edit-journal.json")
}

struct Workspace {
    _temp: TempDir,
    root: PathBuf,
    fixtures: PathBuf,
}

impl Workspace {
    fn manifest(&self) -> PathBuf {
        self.fixtures.join("plugins/demo/Cargo.toml")
    }

    fn source(&self) -> PathBuf {
        self.fixtures.join("plugins/demo/src/lib.rs")
    }

    fn responder(&self) -> PathBuf {
        self.root.join("responder.sh")
    }

    /// Rewrite the responder so it prints `{"value": <value>}`.
    fn write_responder(&self, value: &str) {
        let path = self.responder();
        fs::write(
            &path,
            format!("#!/bin/sh\ncat >/dev/null\necho '{{\"value\":\"{value}\"}}'\n"),
        )
        .expect("write responder script");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&path)
                .expect("responder metadata")
                .permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&path, perms).expect("chmod responder");
        }
    }
}

/// Build `<temp>/fixtures/{plugins,artifacts}` holding one JSON-artifact
/// `demo` plugin whose single node is `node_id`.
fn setup_workspace(node_id: &str, responder: Responder) -> Workspace {
    let temp = TempDir::new().expect("tempdir");
    let root = temp.path().to_path_buf();
    let fixtures = root.join("fixtures");
    let plugins_root = fixtures.join("plugins");
    let dir = plugins_root.join("demo");
    for sub in ["src", "tests", "docs/human", "docs/agent"] {
        fs::create_dir_all(dir.join(sub)).expect("create scaffold dir");
    }
    fs::write(
        plugins_root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"demo\"]\nresolver = \"2\"\n",
    )
    .expect("write workspace manifest");

    let workspace = Workspace {
        _temp: temp,
        root,
        fixtures,
    };

    let command = match responder {
        Responder::Absolute => workspace.responder().display().to_string(),
        Responder::RelativeWipedByRebuild => "demo_verify.sh".to_string(),
    };
    // No `[lib] crate-type` → `read_plugin_build_spec` reports `is_dylib =
    // false`, so the artifact is a JSON descriptor written straight to disk
    // instead of a compiled dylib.
    fs::write(
        dir.join("Cargo.toml"),
        format!(
            r#"[package]
name = "demo"
version = "0.1.0"
edition = "2021"

[package.metadata.cordis]
plugin_path = "demo"
abi_kind = "rust"
declared_nodes = ["{node_id}"]
children = []

[package.metadata.cordis.abi_fingerprint]
crate_hash = "crate_demo_v1"
api_hash = "api_v2"

[package.metadata.cordis.artifact.execution]
kind = "process"
command = "{command}"
args = []
"#
        ),
    )
    .expect("write demo manifest");
    fs::write(dir.join("src/lib.rs"), "pub fn demo() {}\n").expect("write demo source");
    fs::write(dir.join("tests/smoke.rs"), "fn main() {}\n").expect("write demo test");
    fs::write(dir.join("docs/human/overview.md"), "# demo\n").expect("write human doc");
    fs::write(
        dir.join("docs/agent/interfaces.json"),
        serde_json::to_vec_pretty(&json!({
            "plugin_id": "demo",
            "plugin_path": "demo",
            "plugin_version": "0.1.0",
            "abi_version": 2,
            "command_name": "Demo",
            "nodes": [{
                "id": node_id,
                "summary": "demo node",
                "input_schema": { "type": "object" },
                "output_schema": { "type": "object" },
                "side_effects": [],
                "failure_modes": [],
                "node_type": "task",
                "agent_accessible": true
            }],
            "system_hint": null
        }))
        .expect("serialize demo docs"),
    )
    .expect("write demo agent docs");

    if responder == Responder::Absolute {
        workspace.write_responder("pong");
    }
    prepare_artifacts(&workspace.fixtures, PrepareMode::Full).expect("materialize artifacts");
    if responder == Responder::RelativeWipedByRebuild {
        // `prepare_artifacts(Full)` recreated `artifacts/`, so the relative
        // responder has to land afterwards for boot's own staging to find it.
        // The next full rebuild inside the pipeline wipes it again.
        let script = workspace.fixtures.join("artifacts/demo_verify.sh");
        fs::write(&script, "#!/bin/sh\ncat >/dev/null\necho '{}'\n").expect("write responder");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&script).expect("meta").permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&script, perms).expect("chmod responder");
        }
    }
    workspace
}

fn boot(workspace: &Workspace) -> RuntimeHost {
    let host = RuntimeHost::boot(&workspace.fixtures).expect("boot on JSON demo fixture");
    assert!(
        host.current_snapshot()
            .plugin_registry()
            .get("demo")
            .is_some(),
        "demo plugin must be registered"
    );
    host
}

/// An edit plan that appends an inert comment to `plugins/demo/Cargo.toml`.
/// `changed_paths` then ends with `Cargo.toml`, which puts Step 3 on the
/// `plugin_path = "/"` full-rebuild branch (no `cargo build` for a JSON
/// plugin), so the pipeline reaches Step 4 onwards without compiling.
fn manifest_touch_plan(workspace: &Workspace, marker: &str) -> PluginEditPlan {
    let body = fs::read_to_string(workspace.manifest()).expect("read demo manifest");
    PluginEditPlan {
        issue_id: format!("issue-{marker}"),
        patch_id: format!("patch-{marker}"),
        summary: format!("touch the demo manifest ({marker})"),
        operations: vec![PluginEditOperation {
            path: "plugins/demo/Cargo.toml".to_string(),
            kind: PluginEditOpKind::ReplaceExact,
            expected_old_string: Some(body.clone()),
            expected_sha256: None,
            new_content: Some(format!("{body}\n# {marker}\n")),
            pointer: None,
            dotted_key: None,
            value: None,
        }],
    }
}

/// A source-only edit plan. `changed_paths` carries no manifest, so Step 3
/// takes the target-only `cargo build -p demo` branch — which cannot produce a
/// dylib for a JSON plugin, so the rebuild stage fails.
fn source_edit_plan(from: &str, to: &str) -> PluginEditPlan {
    PluginEditPlan {
        issue_id: "issue-demo-src".to_string(),
        patch_id: "patch-demo-src".to_string(),
        summary: "rewrite the demo lib body".to_string(),
        operations: vec![PluginEditOperation {
            path: "plugins/demo/src/lib.rs".to_string(),
            kind: PluginEditOpKind::ReplaceExact,
            expected_old_string: Some(from.to_string()),
            expected_sha256: None,
            new_content: Some(to.to_string()),
            pointer: None,
            dotted_key: None,
            value: None,
        }],
    }
}

/// A request carrying `edit_plan` (so `run_plugin_iteration_agent` takes the
/// no-LLM branch) and trivially-passing verifier commands.
fn request_with(edit_plan: PluginEditPlan) -> KernelPluginIterationRequest {
    KernelPluginIterationRequest {
        issue_id: None,
        target_plugin_paths: vec!["demo".to_string()],
        instruction: Some("drive one iteration stage".to_string()),
        edit_plan: Some(edit_plan),
        manual_approved: false,
        // `true` exits 0 without touching the workspace — the cheapest
        // possible passing verifier stage.
        tests_command: Some("true".to_string()),
        safety_command: Some("true".to_string()),
        // Default profile: no `cargo check` static stage.
        verify_profile: Some(VerificationProfile::Default),
        quality_score: Some(95),
    }
}

/// Replace `path` with a non-empty directory. Writes and `remove_file` against
/// it then fail with `EISDIR` / `ENOTEMPTY` on every unix.
fn wedge_as_nonempty_dir(path: &Path) {
    if path.is_file() {
        fs::remove_file(path).expect("remove pre-existing file");
    }
    fs::create_dir_all(path).expect("create wedge dir");
    fs::write(path.join("blocker"), b"x").expect("make wedge dir non-empty");
}

// ---------------------------------------------------------------------------
// Step 2 — edit: persist_journal failure
// ---------------------------------------------------------------------------

// The journal path is a non-empty directory, so `persist_journal`'s
// `atomic_write` rename onto it fails → `fail_stage(state, "edit", err)`.
// finalize then attempts rollback and `iterate_plugins` returns a RolledBack
// result carrying the stage error.
#[serial]
#[test]
fn iterate_plugins_edit_stage_journal_persist_failure_rolls_back() {
    let workspace = setup_workspace("demo_verify", Responder::Absolute);
    let host = boot(&workspace);
    wedge_as_nonempty_dir(&journal_path(&host.status().snapshot_root));

    let result = host
        .iterate_plugins(request_with(manifest_touch_plan(&workspace, "edit-fail")))
        .expect("a stage failure still yields a result, not an Err");

    assert_eq!(
        result.final_verdict,
        PluginIterationFinalVerdict::RolledBack
    );
    let reason = result.blocked_reason.expect("stage error recorded");
    assert!(
        reason.contains("plugin-iteration-edit-journal.json"),
        "reason should name the journal path: {reason}"
    );
    // The failure short-circuited before verify and canary ever ran.
    assert!(result.verifier_verdict.is_none());
    assert!(result.canary.is_none());
    assert!(result.rebuilt_artifacts.is_empty());
    // `observe_plugin_iteration_failure` only files an issue for
    // `rebuild` / `stage_candidate` stages (or a policy block); a plain `edit`
    // I/O failure takes its `None` source arm and files nothing.
    assert!(
        host.kernel().plugin_issues().is_empty(),
        "an edit-stage Io failure must not file a kernel issue: {:?}",
        host.kernel().plugin_issues()
    );
}

// ---------------------------------------------------------------------------
// Step 3 — rebuild
// ---------------------------------------------------------------------------

// A source-only edit keeps Step 3 on the target-only branch: `cargo build -p
// demo` succeeds (the crate compiles) but produces no dylib, so staging the
// expected artifact fails → `fail_stage(state, "rebuild", err)`.
#[serial]
#[test]
fn iterate_plugins_rebuild_stage_failure_restores_source() {
    let workspace = setup_workspace("demo_verify", Responder::Absolute);
    let host = boot(&workspace);

    let result = host
        .iterate_plugins(request_with(source_edit_plan(
            "pub fn demo() {}\n",
            "pub fn demo() { /* rebuild-fail */ }\n",
        )))
        .expect("a rebuild failure still yields a result");

    assert_eq!(
        result.final_verdict,
        PluginIterationFinalVerdict::RolledBack
    );
    let reason = result.blocked_reason.expect("rebuild stage error recorded");
    assert!(
        reason.contains("demo"),
        "reason should describe the failed rebuild: {reason}"
    );
    assert_eq!(
        fs::read_to_string(workspace.source()).expect("read demo source"),
        "pub fn demo() {}\n",
        "source must be restored after the rebuild-stage failure"
    );
    // rebuild / stage_candidate failures map to a LoadFailure kernel issue.
    assert!(
        host.kernel()
            .plugin_issues()
            .iter()
            .any(|issue| issue.source == KernelPluginIssueSource::LoadFailure),
        "a rebuild-stage failure must file a LoadFailure issue"
    );
}

// Step 3's artifact-backup seam. `artifact_paths_to_backup` only yields a path
// when `artifacts/<name>.so` already exists, so without a seeded `.so` the
// backup loop body never runs. Seeding one makes
// `backup_artifacts_into_rollback` absorb a real backup and the journal
// re-persist carry it. The run then fails at the rebuild (a JSON plugin has no
// dylib to stage) and rolls back.
//
// The rollback's own `restore_plugin_iteration_workspace` finishes with a full
// `rebuild_plugin_workspace("/")`, which wipes and regenerates `artifacts/`,
// so the restored `.so` does not survive to be asserted on. The stable
// observables are that the source edit was undone and that the regenerated
// index is intact.
#[serial]
#[test]
fn iterate_plugins_rebuild_stage_backs_up_an_existing_artifact() {
    let workspace = setup_workspace("demo_verify", Responder::Absolute);
    let host = boot(&workspace);
    fs::write(workspace.fixtures.join("artifacts/demo.so"), b"stale-dylib")
        .expect("seed a stale artifact");

    let result = host
        .iterate_plugins(request_with(source_edit_plan(
            "pub fn demo() {}\n",
            "pub fn demo() { /* artifact-backup */ }\n",
        )))
        .expect("iteration completes with a rebuild failure");

    assert_eq!(
        result.final_verdict,
        PluginIterationFinalVerdict::RolledBack
    );
    assert_eq!(
        fs::read_to_string(workspace.source()).expect("read source"),
        "pub fn demo() {}\n",
        "the source edit must be undone"
    );
    // The rollback's trailing full rebuild left a usable artifacts tree behind.
    assert!(workspace.fixtures.join("artifacts/index.json").exists());
    assert!(workspace.fixtures.join("artifacts/demo.json").exists());
    // The journal was cleared by the successful rollback.
    assert!(!journal_path(&host.status().snapshot_root).exists());
}

// ---------------------------------------------------------------------------
// Step 4 — stage_candidate: reload_candidate failure
// ---------------------------------------------------------------------------

// The plugin's `process` command is a *relative* path, so every snapshot build
// copies it into its staged artifact root. Step 3's full rebuild wipes and
// recreates `artifacts/`, which deletes the script; the Step 4 candidate build
// then cannot stage it → `build_snapshot_with_staged_root` errors →
// `fail_stage(state, "stage_candidate", err)`.
#[serial]
#[test]
fn iterate_plugins_stage_candidate_failure_reports_a_load_failure() {
    let workspace = setup_workspace("demo_verify", Responder::RelativeWipedByRebuild);
    let host = boot(&workspace);

    let result = host
        .iterate_plugins(request_with(manifest_touch_plan(&workspace, "stage-fail")))
        .expect("a candidate-staging failure still yields a result");

    assert_eq!(
        result.final_verdict,
        PluginIterationFinalVerdict::RolledBack
    );
    let reason = result
        .blocked_reason
        .expect("stage_candidate error recorded");
    assert!(
        reason.contains("demo_verify.sh"),
        "reason should name the un-stageable command: {reason}"
    );
    // The rebuild ran (it precedes staging) but no candidate was produced.
    assert!(result.candidate.is_none());
    assert!(host.candidate_snapshot().is_none());
    assert!(
        host.kernel()
            .plugin_issues()
            .iter()
            .any(|issue| issue.source == KernelPluginIssueSource::LoadFailure),
        "a stage_candidate failure must file a LoadFailure issue"
    );
}

// ---------------------------------------------------------------------------
// Step 5 — verify
// ---------------------------------------------------------------------------

// `run_shell_command` maps a spawn failure (the program does not exist) to
// `Err(CommandFailed)`, which `verify_plugin_iteration` propagates → the
// `fail_stage(state, "verify", err)` arm.
#[serial]
#[test]
fn iterate_plugins_verify_stage_spawn_failure_rolls_back() {
    let workspace = setup_workspace("demo_verify", Responder::Absolute);
    let host = boot(&workspace);
    let mut request = request_with(manifest_touch_plan(&workspace, "verify-spawn"));
    request.tests_command = Some("/nonexistent/cordis/verify-binary".to_string());

    let result = host
        .iterate_plugins(request)
        .expect("a verify spawn failure still yields a result");

    assert_eq!(
        result.final_verdict,
        PluginIterationFinalVerdict::RolledBack
    );
    let reason = result.blocked_reason.expect("verify stage error recorded");
    assert!(
        reason.contains("/nonexistent/cordis/verify-binary"),
        "reason should name the unspawnable command: {reason}"
    );
    // The verifier never produced a report, and the canary never ran.
    assert!(result.verification.is_none());
    assert!(result.verifier_verdict.is_none());
    assert!(result.canary.is_none());
}

// A failing-but-spawnable verifier command (`false`) is not a stage error: the
// verifier returns a report with `tests_passed = false`, so Step 5's
// `VerifierVerdict::Fail` → `observe_plugin_issue(VerifierFailure, ..)` branch
// runs instead.
#[serial]
#[test]
fn iterate_plugins_verifier_fail_verdict_observes_a_verifier_failure_issue() {
    let workspace = setup_workspace("demo_verify", Responder::Absolute);
    let host = boot(&workspace);
    let mut request = request_with(manifest_touch_plan(&workspace, "verifier-fail"));
    request.tests_command = Some("false".to_string());

    let result = host
        .iterate_plugins(request)
        .expect("a failing verifier command is a verdict, not a stage error");

    assert_eq!(result.verifier_verdict, Some(VerifierVerdict::Fail));
    assert_eq!(
        result.final_verdict,
        PluginIterationFinalVerdict::RolledBack
    );
    let issues = host.kernel().plugin_issues();
    assert!(
        issues.iter().any(|issue| {
            issue.source == KernelPluginIssueSource::VerifierFailure
                && issue.summary.contains("plugin verifier failed for demo")
                && issue.summary.contains("tests_passed=false")
        }),
        "a Fail verdict must file a VerifierFailure issue: {issues:?}"
    );
}

// ---------------------------------------------------------------------------
// Step 6 — canary
// ---------------------------------------------------------------------------

// A recorded invocation sample the candidate can no longer reproduce yields
// `CanaryVerdict::Fail`, driving Step 6's
// `observe_plugin_issue(CanaryFailure, ..)` branch.
#[serial]
#[test]
fn iterate_plugins_canary_fail_verdict_observes_a_canary_failure_issue() {
    let workspace = setup_workspace("demo_verify", Responder::Absolute);
    let host = boot(&workspace);

    // Record a real successful invocation, then rewrite the responder so the
    // candidate answers differently → replay divergence.
    let seed = host
        .invoke("demo", "demo_verify", json!({}).to_string())
        .expect("seed invocation succeeds");
    assert!(seed.payload.contains("pong"), "payload: {}", seed.payload);
    workspace.write_responder("CHANGED");

    let result = host
        .iterate_plugins(request_with(manifest_touch_plan(&workspace, "canary-fail")))
        .expect("iteration completes with a failing canary");

    assert_eq!(
        result.canary.as_ref().map(|report| report.verdict),
        Some(CanaryVerdict::Fail),
        "blocked_reason={:?}",
        result.blocked_reason
    );
    assert_eq!(
        result.canary.as_ref().map(|report| report.mode.as_str()),
        Some("recent_successful_invocation_replay")
    );
    assert_eq!(
        result.final_verdict,
        PluginIterationFinalVerdict::RolledBack
    );
    let issues = host.kernel().plugin_issues();
    assert!(
        issues.iter().any(|issue| {
            issue.source == KernelPluginIssueSource::CanaryFailure
                && issue.summary.contains("plugin canary failed for demo")
                && issue
                    .summary
                    .contains("candidate replay response diverged from current response")
        }),
        "a Fail canary must file a CanaryFailure issue: {issues:?}"
    );
}

// The canary `Err` arm: the recorded sample's node is served by a process that
// no longer exists, so `invoke_candidate` fails →
// `fail_stage(state, "canary", err)`. The absolute command keeps candidate
// staging (Step 4) happy, so the failure lands in Step 6 and not earlier.
#[serial]
#[test]
fn iterate_plugins_canary_stage_invoke_error_rolls_back() {
    let workspace = setup_workspace("demo_verify", Responder::Absolute);
    let host = boot(&workspace);
    host.invoke("demo", "demo_verify", json!({}).to_string())
        .expect("seed invocation succeeds");
    fs::remove_file(workspace.responder()).expect("remove responder script");

    let result = host
        .iterate_plugins(request_with(manifest_touch_plan(&workspace, "canary-err")))
        .expect("a canary invoke error still yields a result");

    assert_eq!(
        result.final_verdict,
        PluginIterationFinalVerdict::RolledBack
    );
    assert!(
        result.canary.is_none(),
        "a canary stage error leaves no report: {:?}",
        result.canary
    );
    let reason = result.blocked_reason.expect("canary stage error recorded");
    assert!(
        reason.contains("responder.sh"),
        "reason should name the missing responder: {reason}"
    );
    // Verify ran and passed before the canary blew up.
    assert_eq!(result.verifier_verdict, Some(VerifierVerdict::Pass));
}

// ---------------------------------------------------------------------------
// approve_blocked_iteration — promote-failure reinsert arm
// ---------------------------------------------------------------------------

// With no replay sample and a node id containing neither "canary" nor
// "verify", `run_plugin_canary` finds no evidence → `Partial`, so without
// `manual_approved` the iteration is Blocked and the candidate stays staged.
// Wedging the journal path as a directory then makes `promote_candidate`'s
// `clear_plugin_iteration_journal` fail, so `approve_blocked_iteration`
// reinserts the result into `blocked_iterations` and returns the promote error.
#[serial]
#[test]
fn approve_blocked_iteration_reinserts_the_result_when_promote_fails() {
    let workspace = setup_workspace("demo_echo", Responder::Absolute);
    let host = boot(&workspace);

    let result = host
        .iterate_plugins(request_with(manifest_touch_plan(&workspace, "blocked")))
        .expect("iteration completes");
    assert_eq!(
        result.final_verdict,
        PluginIterationFinalVerdict::Blocked,
        "verifier={:?} canary={:?} reason={:?}",
        result.verifier_verdict,
        result.canary.as_ref().map(|r| (r.verdict, &r.mode)),
        result.blocked_reason
    );
    assert_eq!(host.kernel().blocked_iterations().len(), 1);
    assert!(host.candidate_snapshot().is_some());

    // Break promote, then approve.
    let journal = journal_path(&host.status().snapshot_root);
    wedge_as_nonempty_dir(&journal);
    let err = host
        .approve_blocked_iteration(&result.iteration_id)
        .expect_err("a failing promote must surface as Err");
    assert!(
        matches!(&err, RuntimeError::Io { .. }),
        "expected the journal-clear Io error, got {err:?}"
    );
    // The result was put back so the operator can retry after fixing the disk.
    assert_eq!(
        host.kernel().blocked_iterations().len(),
        1,
        "a failed approve must reinsert the blocked iteration"
    );
    assert!(
        host.candidate_snapshot().is_some(),
        "the candidate must still be staged after a failed approve"
    );

    // Un-wedge and retry: the same id now promotes.
    fs::remove_dir_all(&journal).expect("unwedge the journal path");
    let approved = host
        .approve_blocked_iteration(&result.iteration_id)
        .expect("the retry promotes");
    assert_eq!(
        approved.final_verdict,
        PluginIterationFinalVerdict::Promoted
    );
    assert!(host.kernel().blocked_iterations().is_empty());
    assert!(host.candidate_snapshot().is_none());
}
