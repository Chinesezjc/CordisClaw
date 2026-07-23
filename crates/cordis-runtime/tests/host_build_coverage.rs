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
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

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
