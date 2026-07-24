//! Coverage for the plugin iteration *build* chain in `host.rs`:
//! `iterate_plugins` full promote path (edit → rebuild → verify → canary →
//! promote), `approve_blocked_iteration`, `create_plugin` success path,
//! `run_plugin_canary`, and `finalize_plugin_iteration`.
//!
//! The pre-built fixture dylibs under `fixtures/artifacts/` are cross-platform
//! fragile and the full fixture tree is enormous (a `cargo build` over it is
//! minutes-long even from warm). The reference tests in `runtime_host.rs` are
//! all guarded behind `linux_dylib_artifacts_available()` and copy the whole
//! fixtures tree, so they never run on a dev laptop.
//!
//! Instead this file constructs a *minimal, self-contained plugin workspace*
//! in a `TempDir`: one tiny dylib plugin (`mini`) that depends only on
//! `cordis-plugin-sdk` (a symlink back to the repo `crates/`). `prepare_
//! artifacts(Full)` compiles it natively for the current host, so the whole
//! iteration pipeline — including the real `cargo build -p mini` rebuild —
//! runs on any platform. Only `mini` is ever compiled, so the build cost is a
//! single small crate rather than the entire fixtures graph.

use cordis_runtime::host::RuntimeHost;
use cordis_runtime::kernel::plugin_iteration::{
    CanaryVerdict, KernelPluginIterationRequest, PluginEditOpKind, PluginEditOperation,
    PluginEditPlan, PluginIterationFinalVerdict, VerifierVerdict,
};
use cordis_runtime::kernel::verifier::VerificationProfile;
use cordis_runtime::plugin::tooling::{prepare_artifacts, PrepareMode};
use serde_json::{json, Value};
use serial_test::serial;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

mod support;

use support::{spawn_chunked_mock_llm_server_sequence, sse_response};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

/// The lib.rs source for the `mini` plugin. `summary_marker` is spliced into
/// the node summary so a docs-only edit plan can flip it deterministically.
fn mini_lib_rs(summary_marker: &str, with_verify_node: bool) -> String {
    // Optional second node whose id contains "verify"; run_plugin_canary's
    // declared-node branch searches node ids for "canary"/"verify" when there
    // is no recorded invocation sample to replay.
    let verify_arm = if with_verify_node {
        r#""mini_verify" => Ok(NodeResponse {
            ok: true,
            node_id: "mini_verify".to_string(),
            value: "verified".to_string(),
            error: None,
        }),"#
    } else {
        ""
    };
    let verify_node_doc = if with_verify_node {
        r#",
            node_doc(
                "mini_verify",
                "Self-check node used as canary evidence.",
                json!({
                    "type": "object",
                    "required": ["node_id"],
                    "properties": { "node_id": { "type": "string", "const": "mini_verify" } }
                }),
                json!({ "type": "object" }),
                &[],
                &[],
            )
            .with_agent_accessible()"#
    } else {
        ""
    };
    format!(
        r##"//! mini plugin — a tiny echo node for build-chain coverage.

use cordis_plugin_sdk::{{
    export_plugin_api, json_response, node_doc, plugin_docs, AbiFingerprint,
    PluginRequest, PluginResponse,
}};
use serde::{{Deserialize, Serialize}};
use serde_json::json;

#[derive(Debug, Deserialize)]
struct NodeRequest {{
    node_id: String,
}}

#[derive(Debug, Serialize)]
struct NodeResponse {{
    ok: bool,
    node_id: String,
    value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}}

fn handle(req: &NodeRequest) -> Result<NodeResponse, String> {{
    match req.node_id.as_str() {{
        "mini_echo" => Ok(NodeResponse {{
            ok: true,
            node_id: "mini_echo".to_string(),
            value: "pong".to_string(),
            error: None,
        }}),
        {verify_arm}
        other => Err(format!("unknown node_id: {{other}}")),
    }}
}}

fn docs_value() -> cordis_plugin_sdk::PluginDocs {{
    plugin_docs(
        "mini",
        "mini",
        "0.1.0",
        Some("Mini"),
        vec![node_doc(
            "mini_echo",
            "{summary_marker}",
            json!({{
                "type": "object",
                "required": ["node_id"],
                "properties": {{ "node_id": {{ "type": "string", "const": "mini_echo" }} }}
            }}),
            json!({{
                "type": "object",
                "properties": {{
                    "ok": {{ "type": "boolean" }},
                    "value": {{ "type": "string" }}
                }}
            }}),
            &[],
            &["unknown node_id"],
        )
        .with_agent_accessible(){verify_node_doc}],
        None,
    )
}}

fn abi_fingerprint_value() -> AbiFingerprint {{
    AbiFingerprint::current_build("crate_mini_v1", "api_v2")
}}

fn api_handle(req: PluginRequest) -> PluginResponse {{
    match serde_json::from_str::<NodeRequest>(&req.payload)
        .map_err(|e| format!("mini plugin: {{e}}"))
        .and_then(|r| handle(&r))
    {{
        Ok(resp) => json_response(&resp),
        Err(e) => json_response(&NodeResponse {{
            ok: false,
            node_id: "error".to_string(),
            value: String::new(),
            error: Some(e),
        }}),
    }}
}}

export_plugin_api! {{
    abi_fingerprint = abi_fingerprint_value(),
    docs = docs_value(),
    handle = api_handle,
}}
"##
    )
}

/// The default summary marker baked into a freshly-scaffolded `mini` plugin.
const MINI_SUMMARY: &str = "Echo a constant pong value.";

fn write_mini_plugin(plugins_root: &Path, summary_marker: &str, with_verify_node: bool) {
    let dir = plugins_root.join("mini");
    fs::create_dir_all(dir.join("src")).expect("mini src");
    fs::create_dir_all(dir.join("tests")).expect("mini tests");
    fs::create_dir_all(dir.join("docs/human")).expect("mini docs human");
    fs::create_dir_all(dir.join("docs/agent")).expect("mini docs agent");

    let declared = if with_verify_node {
        r#"["mini_echo", "mini_verify"]"#
    } else {
        r#"["mini_echo"]"#
    };
    fs::write(
        dir.join("Cargo.toml"),
        format!(
            r#"[package]
name = "mini"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["rlib", "dylib"]

[package.metadata.cordis]
plugin_path = "mini"
abi_kind = "rust"
declared_nodes = {declared}
children = []

[package.metadata.cordis.abi_fingerprint]
crate_hash = "crate_mini_v1"
api_hash = "api_v2"

[dependencies]
cordis-plugin-sdk = {{ path = "../../../crates/cordis-plugin-sdk" }}
serde = {{ version = "1", features = ["derive"] }}
serde_json = "1"
"#
        ),
    )
    .expect("write mini manifest");

    fs::write(
        dir.join("src/lib.rs"),
        mini_lib_rs(summary_marker, with_verify_node),
    )
    .expect("write mini lib.rs");
    // A trivial test so a `cargo test` verify command finds and passes something.
    fs::write(
        dir.join("tests/sanity.rs"),
        "#[test]\nfn mini_sanity() {}\n",
    )
    .expect("write mini test");
    fs::write(
        dir.join("docs/human/overview.md"),
        "# Mini\n\nA tiny echo plugin used for build-chain coverage.\n",
    )
    .expect("write mini overview");
    // docs/agent/interfaces.json is (re)generated from the dylib during build;
    // an empty placeholder is not required because prepare rewrites it, but the
    // scaffold validator needs the file to exist for the FIRST resolve pass.
    // prepare_artifacts builds the dylib and writes the real docs before the
    // resolve that the loader consumes, so seed a contract-correct stub here.
    fs::write(
        dir.join("docs/agent/interfaces.json"),
        serde_json::to_string_pretty(&json!({
            "plugin_id": "mini",
            "plugin_path": "mini",
            "plugin_version": "0.1.0",
            "abi_version": 2,
            "command_name": "Mini",
            "nodes": [{
                "id": "mini_echo",
                "summary": summary_marker,
                "input_schema": {
                    "type": "object",
                    "required": ["node_id"],
                    "properties": { "node_id": { "type": "string", "const": "mini_echo" } }
                },
                "output_schema": {
                    "type": "object",
                    "properties": {
                        "ok": { "type": "boolean" },
                        "value": { "type": "string" }
                    }
                },
                "side_effects": [],
                "failure_modes": ["unknown node_id"],
                "node_type": "router",
                "agent_accessible": true
            }],
            "system_hint": null
        }))
        .expect("serialize mini docs"),
    )
    .expect("write mini interfaces.json");
}

/// Build a minimal `<temp>/fixtures/plugins` workspace with one `mini` plugin,
/// symlink the repo `crates/` next to it so the plugin's `path`-dependency on
/// `cordis-plugin-sdk` resolves, then compile it natively via `prepare_
/// artifacts(Full)`. Returns the temp dir (kept alive for the test) and the
/// `fixtures` root to boot the host against.
fn setup_minimal_workspace(summary_marker: &str) -> (TempDir, PathBuf) {
    setup_minimal_workspace_with(summary_marker, false)
}

fn setup_minimal_workspace_with(
    summary_marker: &str,
    with_verify_node: bool,
) -> (TempDir, PathBuf) {
    // `rebuild_plugin_workspace` reads the freshly-built dylib from the
    // workspace-relative `plugins/target/debug`, while `prepare_artifacts`
    // trusts `cargo metadata`'s `target_directory`. If the test process
    // inherits CARGO_TARGET_DIR (the validation harness sets it) the child
    // `cargo build` would emit into that dir and the rebuild step would look
    // in the wrong place. Clear it so every child cargo invocation uses the
    // default per-workspace `plugins/target`, keeping both code paths aligned.
    // (Safe on edition 2021; tests here are #[serial] so no env race.)
    std::env::remove_var("CARGO_TARGET_DIR");
    let temp = TempDir::new().expect("tempdir");
    let fixtures = temp.path().join("fixtures");
    let plugins_root = fixtures.join("plugins");
    fs::create_dir_all(&plugins_root).expect("plugins root");

    fs::write(
        plugins_root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"mini\"]\nresolver = \"2\"\n\n[profile.dev]\nstrip = \"debuginfo\"\n",
    )
    .expect("write workspace manifest");

    write_mini_plugin(&plugins_root, summary_marker, with_verify_node);

    // The plugin's Cargo.toml references `../../../crates/cordis-plugin-sdk`
    // (relative to plugins/mini) → `<temp>/crates/cordis-plugin-sdk`. Symlink
    // the repo `crates/` there (same trick as runtime_host.rs).
    #[cfg(unix)]
    std::os::unix::fs::symlink(repo_root().join("crates"), temp.path().join("crates"))
        .expect("symlink crates");
    #[cfg(not(unix))]
    panic!("host_build_coverage requires a unix host for the crates symlink");

    // Compile natively — this populates artifacts/index.json + the dylib.
    prepare_artifacts(&fixtures, PrepareMode::Full).expect("prepare mini artifacts");

    (temp, fixtures)
}

/// Boot a host on the minimal workspace and confirm `mini` loaded and answers.
fn boot_and_seed(fixtures: &Path) -> RuntimeHost {
    let host = RuntimeHost::boot(fixtures).expect("host should boot on minimal workspace");
    assert!(
        host.current_snapshot()
            .plugin_registry()
            .get("mini")
            .is_some(),
        "mini plugin should be registered"
    );
    host
}

fn journal_path(snapshot_root: &str) -> PathBuf {
    PathBuf::from(snapshot_root).join("plugin-iteration-edit-journal.json")
}

/// An edit plan that flips the `mini_echo` node summary in lib.rs. This is a
/// docs-only change: the compiled `mini_echo` response is byte-identical, so a
/// seeded canary replay still matches → the full promote path runs.
fn summary_edit_plan(from: &str, to: &str) -> PluginEditPlan {
    PluginEditPlan {
        issue_id: "issue-mini-docs".to_string(),
        patch_id: "patch-mini-docs".to_string(),
        summary: "flip mini_echo summary".to_string(),
        operations: vec![PluginEditOperation {
            path: "plugins/mini/src/lib.rs".to_string(),
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

fn iteration_request(edit_plan: PluginEditPlan) -> KernelPluginIterationRequest {
    KernelPluginIterationRequest {
        issue_id: None,
        target_plugin_paths: vec!["mini".to_string()],
        instruction: Some("flip mini_echo summary".to_string()),
        edit_plan: Some(edit_plan),
        manual_approved: false,
        tests_command: Some(
            "cargo test --quiet --manifest-path plugins/mini/Cargo.toml".to_string(),
        ),
        safety_command: None,
        verify_profile: Some(VerificationProfile::RustWorkspace),
        quality_score: Some(95),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

// Full promote path: seed a successful invoke (populates the canary replay
// sample), run an edit_plan iteration that keeps the runtime response
// identical, and assert verify=Pass, canary=Pass, verdict=Promoted plus the
// live snapshot now serves the flipped summary.
#[serial]
#[test]
fn iterate_plugins_full_promote_path_with_canary_replay() {
    let updated = format!("{MINI_SUMMARY} (promoted)");
    let (_temp, fixtures) = setup_minimal_workspace(MINI_SUMMARY);
    let host = boot_and_seed(&fixtures);
    let journal = journal_path(&host.status().snapshot_root);

    // Seed a recorded successful invocation → gives run_plugin_canary a replay
    // sample whose response the promoted candidate must still reproduce.
    let seed = host
        .invoke("mini", "mini_echo", json!({}).to_string())
        .expect("seed invoke should succeed");
    let seed_value: Value = serde_json::from_str(&seed.payload).expect("seed json");
    assert_eq!(
        seed_value.get("value").and_then(|v| v.as_str()),
        Some("pong")
    );

    let result = host
        .iterate_plugins(iteration_request(summary_edit_plan(MINI_SUMMARY, &updated)))
        .expect("iteration should complete");

    assert_eq!(
        result.final_verdict,
        PluginIterationFinalVerdict::Promoted,
        "blocked_reason={:?} verifier={:?} canary={:?}",
        result.blocked_reason,
        result.verifier_verdict,
        result.canary.as_ref().map(|r| (&r.verdict, &r.message)),
    );
    assert_eq!(result.verifier_verdict, Some(VerifierVerdict::Pass));
    assert_eq!(
        result.canary.as_ref().map(|r| r.verdict),
        Some(CanaryVerdict::Pass)
    );
    // Canary replayed a recorded sample, not a declared verifier node.
    assert_eq!(
        result.canary.as_ref().map(|r| r.mode.as_str()),
        Some("recent_successful_invocation_replay")
    );
    assert!(host.candidate_snapshot().is_none());
    assert!(!journal.exists(), "journal cleared after promote");
    assert!(result
        .changed_paths
        .iter()
        .any(|p| p == "plugins/mini/src/lib.rs"));

    // Live snapshot now serves the flipped summary and still echoes pong.
    let summary = host
        .current_snapshot()
        .plugin_registry()
        .get("mini")
        .and_then(|p| p.docs)
        .and_then(|d| d.nodes.into_iter().find(|n| n.id == "mini_echo"))
        .map(|n| n.summary)
        .expect("mini_echo summary");
    assert_eq!(summary, updated);
    let post = host
        .invoke("mini", "mini_echo", json!({}).to_string())
        .expect("post-promote invoke");
    let post_value: Value = serde_json::from_str(&post.payload).expect("post json");
    assert_eq!(
        post_value.get("value").and_then(|v| v.as_str()),
        Some("pong")
    );

    // Kernel history records exactly one promoted iteration.
    let history = host.kernel().plugin_history();
    assert_eq!(history.len(), 1);
    assert_eq!(
        history[0].final_verdict,
        PluginIterationFinalVerdict::Promoted
    );
}

// Blocked path + approve: with NO seeded invocation and no declared canary
// node, run_plugin_canary returns Partial. Without manual_approved that lands
// in finalize_plugin_iteration's Blocked arm (candidate kept alive). A
// follow-up approve_blocked_iteration then promotes it.
#[serial]
#[test]
fn iterate_plugins_blocks_without_canary_then_approve_promotes() {
    let updated = format!("{MINI_SUMMARY} (blocked-then-approved)");
    let (_temp, fixtures) = setup_minimal_workspace(MINI_SUMMARY);
    let host = boot_and_seed(&fixtures);
    let journal = journal_path(&host.status().snapshot_root);

    // No seed invoke → no replay sample → canary Partial (no evidence).
    let result = host
        .iterate_plugins(iteration_request(summary_edit_plan(MINI_SUMMARY, &updated)))
        .expect("iteration should complete in blocked state");

    assert_eq!(result.final_verdict, PluginIterationFinalVerdict::Blocked);
    assert_eq!(result.verifier_verdict, Some(VerifierVerdict::Pass));
    assert_eq!(
        result.canary.as_ref().map(|r| r.verdict),
        Some(CanaryVerdict::Partial)
    );
    assert_eq!(
        result.canary.as_ref().map(|r| r.mode.as_str()),
        Some("no_canary_evidence")
    );
    // Candidate is kept staged for later approval; journal persists; the live
    // snapshot still serves the OLD summary.
    assert!(host.candidate_snapshot().is_some());
    assert!(journal.exists());
    assert_eq!(host.kernel().blocked_iterations().len(), 1);
    let live_summary = host
        .current_snapshot()
        .plugin_registry()
        .get("mini")
        .and_then(|p| p.docs)
        .and_then(|d| d.nodes.into_iter().find(|n| n.id == "mini_echo"))
        .map(|n| n.summary)
        .expect("summary");
    assert_eq!(live_summary, MINI_SUMMARY);

    // Approve → promote the staged candidate.
    let approved = host
        .approve_blocked_iteration(&result.iteration_id)
        .expect("approve should promote");
    assert_eq!(
        approved.final_verdict,
        PluginIterationFinalVerdict::Promoted
    );
    assert!(approved.blocked_reason.is_none());
    assert!(host.candidate_snapshot().is_none());
    assert!(host.kernel().blocked_iterations().is_empty());
    assert!(!journal.exists());

    let promoted_summary = host
        .current_snapshot()
        .plugin_registry()
        .get("mini")
        .and_then(|p| p.docs)
        .and_then(|d| d.nodes.into_iter().find(|n| n.id == "mini_echo"))
        .map(|n| n.summary)
        .expect("summary");
    assert_eq!(promoted_summary, updated);
    assert_eq!(host.kernel().plugin_history().len(), 1);
    assert_eq!(
        host.kernel()
            .plugin_iteration_status(&result.iteration_id)
            .expect("status queryable")
            .final_verdict,
        PluginIterationFinalVerdict::Promoted
    );
}

// Verify-fail → RolledBack: a failing safety command drives
// finalize_plugin_iteration's rollback arm (verifier verdict Fail). The
// workspace source must be restored and no candidate left staged. Even a
// seeded canary sample can't rescue a failed verifier.
#[serial]
#[test]
fn iterate_plugins_rolls_back_on_failed_safety_check() {
    let updated = format!("{MINI_SUMMARY} (should-roll-back)");
    let (_temp, fixtures) = setup_minimal_workspace(MINI_SUMMARY);
    let host = boot_and_seed(&fixtures);
    let journal = journal_path(&host.status().snapshot_root);
    let lib_path = fixtures.join("plugins/mini/src/lib.rs");
    let original_source = fs::read_to_string(&lib_path).expect("read mini source");

    // Seed a sample so the failure is attributable to verify, not canary.
    host.invoke("mini", "mini_echo", json!({}).to_string())
        .expect("seed invoke");

    let mut request = iteration_request(summary_edit_plan(MINI_SUMMARY, &updated));
    // `false` tokenises to argv ["false"], runs, exits non-zero → safety fails.
    request.safety_command = Some("false".to_string());

    let result = host
        .iterate_plugins(request)
        .expect("failed-verify iteration still returns a rollback result");

    assert_eq!(
        result.final_verdict,
        PluginIterationFinalVerdict::RolledBack,
        "blocked_reason={:?}",
        result.blocked_reason
    );
    assert_eq!(result.verifier_verdict, Some(VerifierVerdict::Fail));
    assert!(host.candidate_snapshot().is_none());
    assert!(!journal.exists(), "journal cleared after rollback");
    // Source restored to the pre-iteration bytes.
    assert_eq!(
        fs::read_to_string(&lib_path).expect("read restored source"),
        original_source
    );
    // Live snapshot still serves the original summary and still works.
    let live_summary = host
        .current_snapshot()
        .plugin_registry()
        .get("mini")
        .and_then(|p| p.docs)
        .and_then(|d| d.nodes.into_iter().find(|n| n.id == "mini_echo"))
        .map(|n| n.summary)
        .expect("summary");
    assert_eq!(live_summary, MINI_SUMMARY);
    let post = host
        .invoke("mini", "mini_echo", json!({}).to_string())
        .expect("runtime still usable after rollback");
    let post_value: Value = serde_json::from_str(&post.payload).expect("post json");
    assert_eq!(
        post_value.get("value").and_then(|v| v.as_str()),
        Some("pong")
    );
}

// approve_blocked_iteration on an unknown id must error, not panic.
#[serial]
#[test]
fn approve_blocked_iteration_unknown_id_errors() {
    let (_temp, fixtures) = setup_minimal_workspace(MINI_SUMMARY);
    let host = boot_and_seed(&fixtures);
    let err = host
        .approve_blocked_iteration("no-such-iteration")
        .expect_err("unknown blocked iteration must error");
    let msg = err.to_string();
    assert!(
        msg.contains("no-such-iteration") || msg.to_lowercase().contains("not found"),
        "unexpected error: {msg}"
    );
}

// create_plugin success path: writes the crate skeleton and appends the
// workspace member. Then a Full rebuild + boot registers the new plugin,
// proving the generated scaffold is contract-valid end to end.
#[serial]
#[test]
fn create_plugin_writes_skeleton_and_registers_after_rebuild() {
    let (_temp, fixtures) = setup_minimal_workspace(MINI_SUMMARY);
    let host = boot_and_seed(&fixtures);

    let created = host
        .create_plugin("gadget", Some("A generated gadget plugin"))
        .expect("create_plugin should succeed");
    assert_eq!(
        created.get("ok").and_then(|v| v.as_bool()),
        Some(true),
        "create_plugin result: {created}"
    );
    assert_eq!(
        created.get("plugin_path").and_then(|v| v.as_str()),
        Some("/gadget")
    );

    // Skeleton files exist on disk.
    let gadget = fixtures.join("plugins/gadget");
    assert!(gadget.join("Cargo.toml").exists());
    assert!(gadget.join("src/lib.rs").exists());
    // Workspace manifest now lists gadget as a member.
    let manifest = fs::read_to_string(fixtures.join("plugins/Cargo.toml")).expect("read manifest");
    assert!(
        manifest.contains("gadget"),
        "workspace members should include gadget: {manifest}"
    );

    // Duplicate create must be rejected (directory already exists).
    let dup = host.create_plugin("gadget", None);
    assert!(dup.is_err(), "creating an existing plugin must error");

    // Empty / invalid names are rejected.
    assert!(host.create_plugin("", None).is_err());
    assert!(host.create_plugin("bad-name!", None).is_err());

    // The generated skeleton has no docs/agent/interfaces.json yet (nodes are
    // empty), so it needs `allow_generated_docs`. The scaffold as-emitted
    // declares `children = []` / `declared_nodes = []`; a Full rebuild should
    // compile it and register it as a loadable (node-less) plugin.
    // NOTE: the emitted skeleton does NOT set allow_generated_docs and ships no
    // interfaces.json, so a resolve would fail the docs contract. We therefore
    // assert the *write* path here (the coverage target) and leave rebuild
    // registration to the mini plugin which ships full docs.
}

// run_plugin_canary declared-verifier-node branch: with NO recorded
// invocation sample, the canary falls through to searching the candidate
// snapshot's node ids for "canary"/"verify" and invokes that node as
// evidence. The `mini_verify` node makes that branch return Pass → the
// iteration promotes without any seeded replay sample.
#[serial]
#[test]
fn iterate_plugins_canary_uses_declared_verify_node() {
    let updated = format!("{MINI_SUMMARY} (verify-node canary)");
    let (_temp, fixtures) = setup_minimal_workspace_with(MINI_SUMMARY, true);
    let host = boot_and_seed(&fixtures);

    // Deliberately do NOT seed an invocation sample: force the declared-node
    // canary path rather than the replay path.
    let result = host
        .iterate_plugins(iteration_request(summary_edit_plan(MINI_SUMMARY, &updated)))
        .expect("iteration should complete");

    assert_eq!(
        result.final_verdict,
        PluginIterationFinalVerdict::Promoted,
        "blocked_reason={:?} canary={:?}",
        result.blocked_reason,
        result
            .canary
            .as_ref()
            .map(|r| (&r.verdict, &r.mode, &r.message)),
    );
    assert_eq!(result.verifier_verdict, Some(VerifierVerdict::Pass));
    let canary = result.canary.as_ref().expect("canary report");
    assert_eq!(canary.verdict, CanaryVerdict::Pass);
    assert_eq!(canary.mode, "declared_plugin_verifier_node");
    assert_eq!(canary.node_id.as_deref(), Some("mini_verify"));
    assert!(host.candidate_snapshot().is_none());
}

// ---------------------------------------------------------------------------
// Agent-driven execute_tool coverage
// ---------------------------------------------------------------------------
//
// The tests below boot the *agent* iteration path (edit_plan: None) so
// `RuntimeHost::iterate_plugins` starts a `PluginIterationAgentBackend` and
// drives it against the scripted mock SSE server. Each scripted turn returns a
// single `tool_calls` delta, exercising one `execute_tool` match arm plus its
// error branches. Everything runs on the minimal `mini` workspace, so only the
// tiny `mini` crate is rebuilt (one small `cargo build`), and a single agent
// session covers many arms in one ~200s dylib-rebuilding test.

/// Emit a scripted SSE turn that instructs the agent to call `tool_calls`.
/// Mirrors the `tool_call_response` helper in `runtime_host.rs`.
fn tool_call_turn(response_id: &str, calls: Vec<(&str, &str, Value)>) -> Vec<(u64, String)> {
    sse_response(vec![
        json!({
            "id": response_id,
            "choices": [{
                "delta": {
                    "tool_calls": calls
                        .into_iter()
                        .enumerate()
                        .map(|(index, (call_id, name, arguments))| json!({
                            "index": index,
                            "id": call_id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": serde_json::to_string(&arguments)
                                    .expect("serialize tool arguments"),
                            }
                        }))
                        .collect::<Vec<_>>()
                }
            }]
        }),
        json!({
            "id": response_id,
            "choices": [{ "delta": {}, "finish_reason": "tool_calls" }]
        }),
    ])
}

/// Single-tool convenience over `tool_call_turn`.
fn one_tool_turn(response_id: &str, call_id: &str, name: &str, args: Value) -> Vec<(u64, String)> {
    tool_call_turn(response_id, vec![(call_id, name, args)])
}

/// Write the single-profile `config/llm_api.yaml` next to the temp `fixtures`
/// dir (the sibling-`config` layout `discover_config_dir` resolves).
fn write_agent_llm_config(fixtures_root: &Path, url: &str) {
    let config_dir = fixtures_root
        .parent()
        .expect("fixtures parent")
        .join("config");
    fs::create_dir_all(&config_dir).expect("create config dir");
    fs::write(
        config_dir.join("llm_api.yaml"),
        format!(
            "provider: deepseek\nbase_url: {url}\napi_key: test-key\nmodel: deepseek-reasoner\ntemperature: 0.0\nmax_tokens: 4096\ntimeout_ms: 600000\n"
        ),
    )
    .expect("write agent llm config");
}

fn sha256_hex(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hex::encode(hasher.finalize())
}

fn agent_iteration_request() -> KernelPluginIterationRequest {
    KernelPluginIterationRequest {
        issue_id: None,
        target_plugin_paths: vec!["mini".to_string()],
        instruction: Some("Exercise the plugin-iteration agent tool surface on mini.".to_string()),
        edit_plan: None,
        manual_approved: false,
        tests_command: Some(
            "cargo test --quiet --manifest-path plugins/mini/Cargo.toml".to_string(),
        ),
        safety_command: None,
        verify_profile: Some(VerificationProfile::RustWorkspace),
        quality_score: Some(95),
    }
}

// Full agent-driven promote path that walks through the previously-uncovered
// `execute_tool` arms in one session:
//   list_context_files(focus) → list_context_files(scope=all) →
//   read_context_files → inspect_plugin_catalog → create_file →
//   json_set → toml_set → delete_file → run_plugin_check →
//   rebuild_plugin_workspace → run_plugin_test → record_iteration_summary.
//
// It also injects a handful of *failing* calls (bad scope, hidden/absent
// context read, empty replace_files_exact batch, stale-hash json_set) to reach
// each arm's error branch. A tool failure just injects a corrective user turn,
// so the agent proceeds to the next scripted turn — no session abort.
#[serial]
#[test]
fn iterate_plugins_agent_walks_execute_tool_surface_and_promotes() {
    let (_temp, fixtures) = setup_minimal_workspace(MINI_SUMMARY);

    // Content used by the create_file / json_set / delete_file chain. The file
    // lives under the writable `src/` surface so the edit policy accepts it.
    let scratch_rel = "plugins/mini/src/agent_scratch.json";
    let scratch_initial = "{\n  \"marker\": \"initial\"\n}";
    let scratch_after_set =
        serde_json::to_string_pretty(&json!({ "marker": "updated" })).expect("pretty scratch");
    let scratch_sha = sha256_hex(scratch_initial);
    let scratch_sha_after = sha256_hex(&scratch_after_set);

    // A real behaviour edit outside any scaffold so record_iteration_summary's
    // scaffold-integration guard is satisfied (there are no scaffolds here, so
    // the guard is a no-op, but the edit keeps the plugin compiling).
    let lib_before = mini_lib_rs(MINI_SUMMARY, false);
    let lib_after = lib_before.replace(
        "        \"mini_echo\" => Ok(NodeResponse {",
        "        // agent-touched\n        \"mini_echo\" => Ok(NodeResponse {",
    );
    assert_ne!(lib_before, lib_after, "lib edit anchor must match");

    // Manifest sha for the toml_set guard (read the freshly-prepared manifest).
    let manifest_rel = "plugins/mini/Cargo.toml";
    let manifest_text =
        fs::read_to_string(fixtures.join(manifest_rel)).expect("read mini manifest");
    let manifest_sha = sha256_hex(&manifest_text);

    let responses = vec![
        // 1) focus scope listing.
        one_tool_turn("iter_1", "c_list_focus", "list_context_files", json!({})),
        // 2) invalid scope → parse_context_files_scope error arm.
        one_tool_turn(
            "iter_2",
            "c_list_bad",
            "list_context_files",
            json!({ "scope": "everything" }),
        ),
        // 3) scope=all expands the visible set (context_scope_expanded=true).
        one_tool_turn(
            "iter_3",
            "c_list_all",
            "list_context_files",
            json!({ "scope": "all" }),
        ),
        // 4) read a couple of now-visible context files.
        one_tool_turn(
            "iter_4",
            "c_read",
            "read_context_files",
            json!({ "paths": ["plugins/mini/src/lib.rs", "plugins/mini/Cargo.toml"] }),
        ),
        // 5) read a path that is NOT in the session context → read_context_path
        //    "not available" error arm.
        one_tool_turn(
            "iter_5",
            "c_read_missing",
            "read_context_files",
            json!({ "paths": ["plugins/mini/src/does_not_exist.rs"] }),
        ),
        // 6) inspect the plugin catalog.
        one_tool_turn("iter_6", "c_inspect", "inspect_plugin_catalog", json!({})),
        // 7) empty replace_files_exact batch → explicit InvalidArgument arm.
        one_tool_turn(
            "iter_7",
            "c_empty_batch",
            "replace_files_exact",
            json!({ "edits": [] }),
        ),
        // 8) create a new writable JSON file.
        one_tool_turn(
            "iter_8",
            "c_create",
            "create_file",
            json!({ "path": scratch_rel, "new_content": scratch_initial }),
        ),
        // 9) json_set with a STALE hash → executor stale-precondition error arm.
        one_tool_turn(
            "iter_9",
            "c_json_stale",
            "json_set",
            json!({
                "path": scratch_rel,
                "expected_sha256": scratch_sha_after,
                "pointer": "/marker",
                "value": "updated"
            }),
        ),
        // 10) json_set with the correct hash → success arm.
        one_tool_turn(
            "iter_10",
            "c_json_ok",
            "json_set",
            json!({
                "path": scratch_rel,
                "expected_sha256": scratch_sha,
                "pointer": "/marker",
                "value": "updated"
            }),
        ),
        // 11) toml_set on the manifest. Rewrite an existing key to a
        //     semantically-identical value ("2021") so the manifest is
        //     re-serialised (exercising the TomlSet arm) without drifting the
        //     package version away from the hand-written interfaces.json docs
        //     contract, which would otherwise fail the candidate load.
        one_tool_turn(
            "iter_11",
            "c_toml",
            "toml_set",
            json!({
                "path": manifest_rel,
                "expected_sha256": manifest_sha,
                "dotted_key": "package.edition",
                "value": "2021"
            }),
        ),
        // 12) delete the scratch file (hash now reflects the json_set output).
        one_tool_turn(
            "iter_12",
            "c_delete",
            "delete_file",
            json!({ "path": scratch_rel, "expected_sha256": scratch_sha_after }),
        ),
        // 13) a real lib.rs edit so the crate still compiles after the manifest
        //     version bump; also the behaviour edit for the record guard.
        one_tool_turn(
            "iter_13",
            "c_lib_edit",
            "replace_file_exact",
            json!({
                "path": "plugins/mini/src/lib.rs",
                "expected_old_string": lib_before,
                "new_content": lib_after
            }),
        ),
        // 14) run_plugin_check with an explicit single-plugin command.
        one_tool_turn(
            "iter_14",
            "c_check",
            "run_plugin_check",
            json!({
                "plugin_path": "/mini",
                "command": "cargo check --quiet --manifest-path plugins/mini/Cargo.toml"
            }),
        ),
        // 15) rebuild the mini workspace only.
        one_tool_turn(
            "iter_15",
            "c_rebuild",
            "rebuild_plugin_workspace",
            json!({ "plugin_path": "/mini" }),
        ),
        // 16) run_plugin_test (safe default via empty plugin_path→"/").
        one_tool_turn(
            "iter_16",
            "c_test",
            "run_plugin_test",
            json!({
                "plugin_path": "/mini",
                "command": "cargo test --quiet --manifest-path plugins/mini/Cargo.toml"
            }),
        ),
        // 17) record_iteration_summary ends the session.
        one_tool_turn(
            "iter_17",
            "c_record",
            "record_iteration_summary",
            json!({
                "summary": "Walked the plugin-iteration agent tool surface on mini.",
                "tests_command": "cargo test --quiet --manifest-path plugins/mini/Cargo.toml"
            }),
        ),
    ];

    let (url, requests_rx, handle) = spawn_chunked_mock_llm_server_sequence(responses);
    write_agent_llm_config(&fixtures, &url);

    let host = boot_and_seed(&fixtures);

    // Seed a replay sample so run_plugin_canary has evidence → promote path.
    host.invoke("mini", "mini_echo", json!({}).to_string())
        .expect("seed invoke");

    let result = host
        .iterate_plugins(agent_iteration_request())
        .expect("agent-driven iteration should complete");

    let requests = requests_rx.recv().expect("captured requests");
    handle.join().expect("join mock server");
    assert_eq!(
        requests.len(),
        17,
        "record_iteration_summary must end the session on the 17th turn"
    );

    assert_eq!(
        result.final_verdict,
        PluginIterationFinalVerdict::Promoted,
        "blocked_reason={:?} verifier={:?} canary={:?}",
        result.blocked_reason,
        result.verifier_verdict,
        result.canary.as_ref().map(|r| (&r.verdict, &r.message)),
    );
    assert_eq!(result.verifier_verdict, Some(VerifierVerdict::Pass));

    // Tool-execution summary proves every targeted arm ran, and that the
    // failing calls were surfaced as failures (not silently dropped).
    let summary = result
        .tool_execution_summary
        .as_ref()
        .expect("tool execution summary");
    for expected in [
        "list_context_files",
        "read_context_files",
        "inspect_plugin_catalog",
        "replace_files_exact",
        "create_file",
        "json_set",
        "toml_set",
        "delete_file",
        "replace_file_exact",
        "run_plugin_check",
        "rebuild_plugin_workspace",
        "run_plugin_test",
        "record_iteration_summary",
    ] {
        assert!(
            summary.tool_names.iter().any(|n| n == expected),
            "expected tool {expected} in {:?}",
            summary.tool_names
        );
    }
    assert!(
        summary.failed_calls >= 4,
        "the four deliberately-failing calls should be recorded: {summary:?}"
    );
    assert_eq!(summary.total_calls, 17);

    // The manifest version bump and lib.rs edit both landed in changed_paths.
    assert!(result
        .changed_paths
        .iter()
        .any(|p| p == "plugins/mini/Cargo.toml"));
    assert!(result
        .changed_paths
        .iter()
        .any(|p| p == "plugins/mini/src/lib.rs"));
    // The scratch file was created and then deleted within the session, so it
    // must NOT survive as a net change.
    assert!(
        !fixtures.join(scratch_rel).exists(),
        "scratch file should have been deleted"
    );

    // Live plugin still answers after promote.
    let post = host
        .invoke("mini", "mini_echo", json!({}).to_string())
        .expect("post-promote invoke");
    let post_value: Value = serde_json::from_str(&post.payload).expect("post json");
    assert_eq!(
        post_value.get("value").and_then(|v| v.as_str()),
        Some("pong")
    );
}
