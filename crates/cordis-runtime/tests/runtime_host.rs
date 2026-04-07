use cordis_runtime::host::{ReloadAttemptStatus, RuntimeHost, RuntimeSnapshot};
use cordis_runtime::kernel::auto_update::{AutoUpdatePlan, FilePatch};
use cordis_runtime::kernel::evaluator::VerificationInput;
use cordis_runtime::kernel::plugin_iteration::{
    CanaryVerdict, KernelPluginIssueSource, KernelPluginIterationRequest, PluginEditOpKind,
    PluginEditOperation, PluginEditPlan, PluginIterationFinalVerdict, VerifierVerdict,
};
use cordis_runtime::kernel::verifier::VerificationProfile;
use cordis_runtime::plugin::tooling::refresh_artifact_index;
use serde_json::{json, Value};
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

mod support;

use support::fixtures_root;

fn copy_dir_all(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).expect("create destination");
    for entry in fs::read_dir(src).expect("read dir") {
        let entry = entry.expect("dir entry");
        let ty = entry.file_type().expect("file type");
        if ty.is_dir() && entry.file_name() == "target" {
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

fn setup_fixture_copy() -> TempDir {
    let temp = TempDir::new().expect("tempdir");
    copy_dir_all(&fixtures_root(), temp.path());
    temp
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

fn setup_fixture_workspace_copy() -> TempDir {
    let temp = TempDir::new().expect("tempdir");
    let temp_fixtures = temp.path().join("fixtures");
    copy_dir_all(&fixtures_root(), &temp_fixtures);
    fs::copy(
        repo_root().join("Cargo.toml"),
        temp.path().join("Cargo.toml"),
    )
    .expect("copy workspace manifest");
    #[cfg(unix)]
    symlink(repo_root().join("crates"), temp.path().join("crates"))
        .expect("symlink workspace crates");
    #[cfg(not(unix))]
    copy_dir_all(&repo_root().join("crates"), &temp.path().join("crates"));
    temp
}

fn plugin_node_summary(snapshot: &RuntimeSnapshot, plugin_path: &str, node_id: &str) -> String {
    snapshot
        .plugin_registry()
        .get(plugin_path)
        .and_then(|plugin| plugin.docs)
        .and_then(|docs| {
            docs.nodes
                .into_iter()
                .find(|node| node.id == node_id)
                .map(|node| node.summary)
        })
        .expect("node summary should exist")
}

fn workspace_manifest_path(root: &Path) -> PathBuf {
    root.join("plugins/Cargo.toml")
}

fn update_workspace_members(root: &Path, members: &[&str]) {
    let manifest_path = workspace_manifest_path(root);
    let mut text = fs::read_to_string(&manifest_path).expect("read workspace manifest");
    let start = text.find("members = [").expect("members line should exist");
    let end = text[start..]
        .find(']')
        .map(|idx| start + idx)
        .expect("members list should terminate");
    let replacement = format!(
        "members = [{}]",
        members
            .iter()
            .map(|member| format!("\"{member}\""))
            .collect::<Vec<_>>()
            .join(", ")
    );
    text.replace_range(start..=end, &replacement);
    fs::write(&manifest_path, text).expect("write workspace manifest");
}

fn add_demo_process_plugin(root: &Path, version: &str) {
    let plugin_dir = root.join("plugins/demo");
    fs::create_dir_all(plugin_dir.join("src")).expect("create demo src");
    fs::create_dir_all(plugin_dir.join("tests")).expect("create demo tests");
    fs::create_dir_all(plugin_dir.join("docs/agent")).expect("create demo docs");
    fs::create_dir_all(plugin_dir.join("docs/human")).expect("create demo docs");

    fs::write(
        plugin_dir.join("Cargo.toml"),
        r#"[package]
name = "demo"
version = "0.1.0"
edition = "2021"

[package.metadata.cordis]
plugin_path = "demo"
abi_kind = "rust"
declared_nodes = ["demo_entry"]

[package.metadata.cordis.abi_fingerprint]
rustc_version = "1.85.1"
target_triple = "x86_64-unknown-linux-gnu"
crate_hash = "crate_demo_v1"
api_hash = "api_v2"
"#,
    )
    .expect("write demo manifest");

    fs::write(
        plugin_dir.join("src/lib.rs"),
        "pub fn demo_plugin_marker() {}\n",
    )
    .expect("write demo src");
    fs::write(
        plugin_dir.join("tests/basic.rs"),
        "#[test]\nfn demo_scaffold_test() {}\n",
    )
    .expect("write demo test");
    fs::write(
        plugin_dir.join("docs/agent/interfaces.json"),
        format!(
            r#"{{
  "plugin_id": "demo",
  "plugin_path": "demo",
  "plugin_version": "{version}",
  "abi_version": 2,
  "nodes": [
    {{
      "id": "demo_entry",
      "summary": "demo process task",
      "input_schema": {{
        "type": "object",
        "properties": {{
          "message": {{ "type": "string" }}
        }},
        "required": ["message"]
      }},
      "output_schema": {{
        "type": "object",
        "properties": {{
          "version": {{ "type": "string" }}
        }}
      }},
      "side_effects": ["process"],
      "failure_modes": ["process_error"]
    }}
  ]
}}
"#
        ),
    )
    .expect("write demo docs");
    fs::write(
        plugin_dir.join("docs/human/overview.md"),
        "# Demo\n\nProcess-backed demo plugin.\n",
    )
    .expect("write demo overview");

    write_demo_artifacts(root, version);
    append_demo_index_entry(root, version);
    update_workspace_members(root, &["root", "expr", "shell", "demo"]);
    refresh_artifact_index(root).expect("refresh artifact index for demo");
}

fn write_demo_artifacts(root: &Path, version: &str) {
    let artifacts_dir = root.join("artifacts");
    fs::create_dir_all(&artifacts_dir).expect("create artifacts dir");
    fs::write(
        artifacts_dir.join("demo.json"),
        format!(
            r#"{{
  "plugin_path": "demo",
  "abi_fingerprint": {{
    "rustc_version": "1.85.1",
    "target_triple": "x86_64-unknown-linux-gnu",
    "crate_hash": "crate_demo_v1",
    "api_hash": "api_v2"
  }},
  "docs": {{
    "plugin_id": "demo",
    "plugin_path": "demo",
    "plugin_version": "{version}",
    "abi_version": 2,
    "nodes": [
      {{
        "id": "demo_entry",
        "summary": "demo process task",
        "input_schema": {{
          "type": "object",
          "properties": {{
            "message": {{ "type": "string" }}
          }},
          "required": ["message"]
        }},
        "output_schema": {{
          "type": "object",
          "properties": {{
            "version": {{ "type": "string" }}
          }}
        }},
        "side_effects": ["process"],
        "failure_modes": ["process_error"]
      }}
    ]
  }},
  "exports": [],
  "execution": {{
    "kind": "process",
    "command": "./demo_runner.sh",
    "args": []
  }}
}}
"#
        ),
    )
    .expect("write demo artifact");
    fs::write(
        artifacts_dir.join("demo_runner.sh"),
        format!(
            "#!/usr/bin/env bash\nset -euo pipefail\ncat >/dev/null\nprintf '%s\\n' '{{\"version\":\"{version}\"}}'\n"
        ),
    )
    .expect("write demo runner");
    make_executable(&artifacts_dir.join("demo_runner.sh"));
}

#[cfg(unix)]
fn make_executable(path: &Path) {
    let mut permissions = fs::metadata(path).expect("runner metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("set runner executable");
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) {}

fn append_demo_index_entry(root: &Path, version: &str) {
    let index_path = root.join("artifacts/index.json");
    let mut value: Value =
        serde_json::from_str(&fs::read_to_string(&index_path).expect("read index"))
            .expect("parse index");
    value["generated_at"] = Value::String("2026-03-11T00:00:00Z".to_string());
    value
        .get_mut("topo_order")
        .and_then(|items| items.as_array_mut())
        .expect("topo order")
        .push(Value::String("demo".to_string()));
    let entries = value
        .get_mut("entries")
        .and_then(|entries| entries.as_array_mut())
        .expect("entries array");
    entries.push(json!({
        "plugin_path": "demo",
        "version": version,
        "abi_fingerprint": {
            "rustc_version": "1.85.1",
            "target_triple": "x86_64-unknown-linux-gnu",
            "crate_hash": "crate_demo_v1",
            "api_hash": "api_v2"
        },
        "artifact_path": "demo.json",
        "sha256": "",
        "built_at": "2026-03-11T00:00:00Z",
        "parent": null,
        "required": true,
        "grants_from_parent": [],
        "docs": {
            "plugin_id": "demo",
            "plugin_path": "demo",
            "plugin_version": version,
            "abi_version": 2,
            "nodes": [
                {
                    "id": "demo_entry",
                    "summary": "demo process task",
                    "input_schema": {
                        "type": "object",
                        "properties": {
                            "message": { "type": "string" }
                        },
                        "required": ["message"]
                    },
                    "output_schema": {
                        "type": "object",
                        "properties": {
                            "version": { "type": "string" }
                        }
                    },
                    "side_effects": ["process"],
                    "failure_modes": ["process_error"]
                }
            ]
        },
        "exports": [],
        "execution": {
            "kind": "process",
            "command": "./demo_runner.sh",
            "args": []
        },
        "artifact_kind": "json",
        "build_fingerprint": format!("demo-{version}"),
        "input_probe": { "files": [] },
        "local_path_deps": []
    }));
    fs::write(
        &index_path,
        serde_json::to_string_pretty(&value).expect("serialize index"),
    )
    .expect("write index");
}

fn sync_demo_index_entry(root: &Path, version: &str) {
    let index_path = root.join("artifacts/index.json");
    let mut value: Value =
        serde_json::from_str(&fs::read_to_string(&index_path).expect("read index"))
            .expect("parse index");
    let entries = value
        .get_mut("entries")
        .and_then(|entries| entries.as_array_mut())
        .expect("entries array");
    let entry = entries
        .iter_mut()
        .find(|entry| entry.get("plugin_path").and_then(|v| v.as_str()) == Some("demo"))
        .expect("demo entry");
    entry["version"] = Value::String(version.to_string());
    entry["built_at"] = Value::String("2026-03-11T00:00:00Z".to_string());
    entry["build_fingerprint"] = Value::String(format!("demo-{version}"));
    entry["docs"]["plugin_version"] = Value::String(version.to_string());
    fs::write(
        &index_path,
        serde_json::to_string_pretty(&value).expect("serialize index"),
    )
    .expect("write index");
}

fn overwrite_index_hash(root: &Path, plugin_path: &str, hash: &str) {
    let index_path = root.join("artifacts/index.json");
    let mut value: Value =
        serde_json::from_str(&fs::read_to_string(&index_path).expect("read index"))
            .expect("parse index");
    let entries = value
        .get_mut("entries")
        .and_then(|entries| entries.as_array_mut())
        .expect("entries array");
    let entry = entries
        .iter_mut()
        .find(|entry| entry.get("plugin_path").and_then(|v| v.as_str()) == Some(plugin_path))
        .expect("plugin entry");
    entry["sha256"] = Value::String(hash.to_string());
    fs::write(
        &index_path,
        serde_json::to_string_pretty(&value).expect("serialize index"),
    )
    .expect("write index");
}

fn update_index_node_summary(root: &Path, plugin_path: &str, node_id: &str, summary: &str) {
    let index_path = root.join("artifacts/index.json");
    let mut value: Value =
        serde_json::from_str(&fs::read_to_string(&index_path).expect("read index"))
            .expect("parse index");
    let entries = value
        .get_mut("entries")
        .and_then(|entries| entries.as_array_mut())
        .expect("entries array");
    let entry = entries
        .iter_mut()
        .find(|entry| entry.get("plugin_path").and_then(|v| v.as_str()) == Some(plugin_path))
        .expect("plugin entry");
    let node = entry["docs"]["nodes"]
        .as_array_mut()
        .expect("docs nodes")
        .iter_mut()
        .find(|node| node.get("id").and_then(|v| v.as_str()) == Some(node_id))
        .expect("node entry");
    node["summary"] = Value::String(summary.to_string());
    fs::write(
        &index_path,
        serde_json::to_string_pretty(&value).expect("serialize index"),
    )
    .expect("write index");
}

#[test]
fn runtime_host_loads_yaml_config_and_uses_custom_snapshot_root() {
    let temp = setup_fixture_copy();
    let config_dir = temp.path().join("config");
    let plugin_config_dir = config_dir.join("plugins");
    fs::create_dir_all(&plugin_config_dir).expect("create config/plugins");

    fs::write(
        config_dir.join("runtime.yaml"),
        "runtime:\n  snapshot_root: snapshots\nkernel:\n  change_history_limit: 64\n  min_quality_score: 91\n",
    )
    .expect("write runtime config");
    fs::write(
        config_dir.join("llm_api.yaml"),
        "provider: openai\nbase_url: https://api.openai.com/v1\napi_key_env: OPENAI_API_KEY\nmodel: gpt-4.1-mini\ntemperature: 0.1\nmax_tokens: 2048\ntimeout_ms: 30000\n",
    )
    .expect("write llm config");
    fs::write(
        plugin_config_dir.join("expr.yaml"),
        "plugin: expr\nenabled: true\nsettings:\n  command_name: Expr\n",
    )
    .expect("write plugin config");

    let host = RuntimeHost::boot(temp.path()).expect("host should boot with config");
    let status = host.kernel().status();

    assert_eq!(status.config_dir, config_dir.display().to_string());
    assert_eq!(status.llm_provider, "openai");
    assert_eq!(status.llm_model, "gpt-4.1-mini");
    assert_eq!(status.plugin_config_count, 1);
    assert!(
        host.current_snapshot()
            .staged_artifact_root()
            .starts_with(config_dir.join("snapshots")),
        "snapshot root should honor runtime.yaml"
    );
}

#[test]
fn runtime_host_boots_and_invokes_loaded_plugins() {
    let temp = setup_fixture_copy();
    let host = RuntimeHost::boot(temp.path()).expect("host should boot");

    let snapshot = host.current_snapshot();
    assert!(snapshot.plugin_registry().get("expr").is_some());
    assert!(snapshot.plugin_registry().get("shell").is_some());

    let response = host
        .invoke(
            "expr",
            "expr_entry",
            json!({ "expression": "1 + 2 * 3" }).to_string(),
        )
        .expect("expr invoke should succeed");
    let value: Value = serde_json::from_str(&response.payload).expect("expr response json");
    assert_eq!(value.get("value").and_then(|v| v.as_f64()), Some(7.0));
}

#[test]
fn runtime_host_reload_adds_top_level_plugin() {
    let temp = setup_fixture_copy();
    let host = RuntimeHost::boot(temp.path()).expect("host should boot");

    add_demo_process_plugin(temp.path(), "v1");
    let report = host.reload().expect("reload with demo should succeed");

    assert!(report.added_plugins.iter().any(|plugin| plugin == "demo"));
    assert!(host
        .current_snapshot()
        .plugin_registry()
        .get("demo")
        .is_some());

    let response = host
        .invoke(
            "demo",
            "demo_entry",
            json!({ "message": "hello" }).to_string(),
        )
        .expect("demo invoke should succeed");
    let value: Value = serde_json::from_str(&response.payload).expect("demo response json");
    assert_eq!(value.get("version").and_then(|v| v.as_str()), Some("v1"));
}

#[test]
fn runtime_host_reload_removes_plugin_but_old_snapshot_stays_usable() {
    let temp = setup_fixture_copy();
    let host = RuntimeHost::boot(temp.path()).expect("host should boot");
    let old_snapshot = host.current_snapshot();

    let index_path = temp.path().join("artifacts/index.json");
    let mut value: Value =
        serde_json::from_str(&fs::read_to_string(&index_path).expect("read index"))
            .expect("parse index");
    value
        .get_mut("entries")
        .and_then(|entries| entries.as_array_mut())
        .expect("entries array")
        .retain(|entry| entry.get("plugin_path").and_then(|v| v.as_str()) != Some("shell"));
    value
        .get_mut("topo_order")
        .and_then(|items| items.as_array_mut())
        .expect("topo order")
        .retain(|entry| entry.as_str() != Some("shell"));
    fs::write(
        &index_path,
        serde_json::to_string_pretty(&value).expect("serialize index"),
    )
    .expect("write updated index");
    let report = host.reload().expect("reload without shell should succeed");

    assert!(report
        .removed_plugins
        .iter()
        .any(|plugin| plugin == "shell"));
    assert!(host
        .current_snapshot()
        .plugin_registry()
        .get("shell")
        .is_none());

    let response = old_snapshot
        .invoke(
            "shell",
            "shell_entry",
            json!({ "action": "start_terminal", "command": "echo hi" }).to_string(),
        )
        .expect("old snapshot shell should still run");
    let value: Value = serde_json::from_str(&response.payload).expect("shell response json");
    assert_eq!(value.get("output").and_then(|v| v.as_str()), Some("hi"));
}

#[test]
fn runtime_host_reload_failure_keeps_current_snapshot() {
    let temp = setup_fixture_copy();
    let host = RuntimeHost::boot(temp.path()).expect("host should boot");
    let snapshot_id = host.current_snapshot().snapshot_id().to_string();

    overwrite_index_hash(temp.path(), "shell", "deadbeef");
    let err = host
        .reload()
        .expect_err("reload should fail on hash mismatch");
    assert!(err.to_string().contains("HashMismatch") || err.to_string().contains("hash"));

    assert_eq!(host.current_snapshot().snapshot_id(), snapshot_id);
    let response = host
        .invoke(
            "expr",
            "expr_entry",
            json!({ "expression": "3 + 4" }).to_string(),
        )
        .expect("old snapshot should still be active");
    let value: Value = serde_json::from_str(&response.payload).expect("expr response json");
    assert_eq!(value.get("value").and_then(|v| v.as_f64()), Some(7.0));
}

#[test]
fn runtime_host_reload_observes_docs_drift_issue() {
    let temp = setup_fixture_copy();
    let host = RuntimeHost::boot(temp.path()).expect("host should boot");
    let updated_summary = "Start the CordisClaw terminal with updated docs.";

    update_index_node_summary(temp.path(), "shell", "shell_entry", updated_summary);
    let report = host.reload().expect("reload should succeed");

    assert!(report
        .changed_plugin_reasons
        .get("shell")
        .map(|reasons| reasons.iter().any(|reason| reason == "docs_changed"))
        .unwrap_or(false));
    assert_eq!(
        plugin_node_summary(host.current_snapshot().as_ref(), "shell", "shell_entry"),
        updated_summary
    );
    assert!(host.kernel().plugin_issues().iter().any(|issue| {
        issue.root_plugin_path == "shell" && issue.source == KernelPluginIssueSource::DocsDrift
    }));
}

#[test]
fn runtime_host_snapshot_keeps_old_staged_process_artifact_after_reload() {
    let temp = setup_fixture_copy();
    add_demo_process_plugin(temp.path(), "v1");
    let host = RuntimeHost::boot(temp.path()).expect("host should boot with demo");
    let old_snapshot = host.current_snapshot();
    let old_stage = old_snapshot.staged_artifact_root().to_path_buf();

    write_demo_artifacts(temp.path(), "v2");
    sync_demo_index_entry(temp.path(), "v2");
    refresh_artifact_index(temp.path()).expect("refresh index after demo update");
    host.reload()
        .expect("reload with updated demo should succeed");

    let old_response = old_snapshot
        .invoke(
            "demo",
            "demo_entry",
            json!({ "message": "hello" }).to_string(),
        )
        .expect("old snapshot invoke should succeed");
    let new_response = host
        .invoke(
            "demo",
            "demo_entry",
            json!({ "message": "hello" }).to_string(),
        )
        .expect("new snapshot invoke should succeed");

    let old_value: Value = serde_json::from_str(&old_response.payload).expect("old demo json");
    let new_value: Value = serde_json::from_str(&new_response.payload).expect("new demo json");
    assert_eq!(
        old_value.get("version").and_then(|v| v.as_str()),
        Some("v1")
    );
    assert_eq!(
        new_value.get("version").and_then(|v| v.as_str()),
        Some("v2")
    );

    drop(old_snapshot);
    let _ = host.invoke(
        "demo",
        "demo_entry",
        json!({ "message": "hello" }).to_string(),
    );
    assert!(
        !old_stage.exists(),
        "old staged artifact root should be cleaned after snapshot drop"
    );
}

#[test]
fn runtime_host_kernel_state_persists_across_reload() {
    let temp = setup_fixture_copy();
    let host = RuntimeHost::boot(temp.path()).expect("host should boot");
    let patch_target = temp.path().join("notes.txt");
    fs::write(&patch_target, "alpha-old-omega").expect("write patch target");

    let result = host
        .kernel()
        .run_iteration(
            AutoUpdatePlan {
                issue_id: "issue-1".to_string(),
                patch_id: "patch-1".to_string(),
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
        .expect("kernel iteration should succeed");
    assert!(!result.rolled_back);

    update_workspace_members(temp.path(), &["root", "expr"]);
    host.reload().expect("reload should succeed");

    let status = host.kernel().status();
    assert_eq!(status.iteration_total, 1);
    assert_eq!(status.iteration_promote_total, 1);
    assert_eq!(host.kernel().history().len(), 1);
    assert_eq!(
        fs::read_to_string(&patch_target).expect("read patch target"),
        "alpha-new-omega"
    );
}

#[test]
fn runtime_host_execute_runs_registered_target_through_execution_engine() {
    let host = RuntimeHost::boot(fixtures_root()).expect("host should boot");
    let result = host
        .execute("expr::expr_entry", json!({ "expression": "1 + 2 * 3" }))
        .expect("execute should succeed");

    assert_eq!(result.target_node_fqn, "expr::expr_entry");
    assert!(result
        .output
        .order
        .iter()
        .any(|node| node == "expr::expr_entry"));
    assert_eq!(
        result.output.outcomes.get("expr::expr_entry"),
        Some(&cordis_runtime::core::models::NodeOutcome::Success)
    );
    let trace = result
        .traces
        .get("expr::expr_entry")
        .expect("trace should exist");
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
fn runtime_host_reload_with_diagnostics_reports_failure_summary() {
    let temp = setup_fixture_copy();
    let host = RuntimeHost::boot(temp.path()).expect("host should boot");
    let snapshot_id = host.current_snapshot().snapshot_id().to_string();

    overwrite_index_hash(temp.path(), "shell", "deadbeef");
    let report = host.reload_with_diagnostics();

    assert_eq!(report.status, ReloadAttemptStatus::Failed);
    assert_eq!(report.from_snapshot_id, snapshot_id);
    assert!(report.to_snapshot_id.is_none());
    assert!(
        report
            .failure_summary
            .as_deref()
            .unwrap_or_default()
            .contains("HashMismatch"),
        "report: {report:?}"
    );
    assert_eq!(host.current_snapshot().snapshot_id(), snapshot_id);
    assert_eq!(host.status().last_reload, Some(report));
}

#[test]
fn runtime_host_candidate_reload_stages_snapshot_without_switching_current() {
    let temp = setup_fixture_copy();
    let host = RuntimeHost::boot(temp.path()).expect("host should boot");
    let current_snapshot_id = host.current_snapshot().snapshot_id().to_string();

    add_demo_process_plugin(temp.path(), "v1");
    let status = host
        .reload_candidate()
        .expect("candidate reload with demo should succeed");

    assert_eq!(status.from_snapshot_id, current_snapshot_id);
    assert!(status.added_plugins.iter().any(|plugin| plugin == "demo"));
    assert!(host
        .current_snapshot()
        .plugin_registry()
        .get("demo")
        .is_none());
    assert_eq!(host.candidate_status(), Some(status.clone()));
    assert_eq!(
        host.last_candidate_reload_attempt()
            .expect("candidate reload attempt should be recorded")
            .status,
        ReloadAttemptStatus::Staged
    );

    let response = host
        .invoke_candidate(
            "demo",
            "demo_entry",
            json!({ "message": "hello" }).to_string(),
        )
        .expect("candidate snapshot demo invoke should succeed");
    let value: Value =
        serde_json::from_str(&response.payload).expect("candidate demo response json");
    assert_eq!(value.get("version").and_then(|v| v.as_str()), Some("v1"));
}

#[test]
fn runtime_host_candidate_reload_observes_load_failure_issue() {
    let temp = setup_fixture_copy();
    let host = RuntimeHost::boot(temp.path()).expect("host should boot");

    overwrite_index_hash(temp.path(), "shell", "deadbeef");
    let err = host
        .reload_candidate()
        .expect_err("candidate reload should fail on hash mismatch");
    assert!(err.to_string().contains("shell"));
    assert!(host.candidate_snapshot().is_none());
    assert!(host.kernel().plugin_issues().iter().any(|issue| {
        issue.root_plugin_path == "shell" && issue.source == KernelPluginIssueSource::LoadFailure
    }));
}

#[test]
fn runtime_host_promote_candidate_switches_current_and_keeps_old_snapshot_usable() {
    let temp = setup_fixture_copy();
    let host = RuntimeHost::boot(temp.path()).expect("host should boot");
    let old_snapshot = host.current_snapshot();
    let old_snapshot_id = old_snapshot.snapshot_id().to_string();

    add_demo_process_plugin(temp.path(), "v1");
    host.reload_candidate()
        .expect("candidate reload with demo should succeed");

    let report = host.promote_candidate().expect("promote should succeed");

    assert_eq!(report.from_snapshot_id, old_snapshot_id);
    assert!(report.added_plugins.iter().any(|plugin| plugin == "demo"));
    assert!(host.candidate_snapshot().is_none());
    assert!(host.status().candidate_snapshot.is_none());
    assert!(host
        .current_snapshot()
        .plugin_registry()
        .get("demo")
        .is_some());
    assert_eq!(
        host.last_reload_attempt()
            .expect("promote should record last reload")
            .status,
        ReloadAttemptStatus::Reloaded
    );

    let old_response = old_snapshot
        .invoke(
            "expr",
            "expr_entry",
            json!({ "expression": "3 + 4" }).to_string(),
        )
        .expect("old snapshot should still invoke expr");
    let old_value: Value =
        serde_json::from_str(&old_response.payload).expect("old expr response json");
    assert_eq!(old_value.get("value").and_then(|v| v.as_f64()), Some(7.0));

    let new_response = host
        .invoke(
            "demo",
            "demo_entry",
            json!({ "message": "hello" }).to_string(),
        )
        .expect("promoted snapshot should invoke demo");
    let new_value: Value =
        serde_json::from_str(&new_response.payload).expect("new demo response json");
    assert_eq!(
        new_value.get("version").and_then(|v| v.as_str()),
        Some("v1")
    );
}

#[test]
fn runtime_host_rollback_candidate_discards_staged_snapshot() {
    let temp = setup_fixture_copy();
    let host = RuntimeHost::boot(temp.path()).expect("host should boot");

    add_demo_process_plugin(temp.path(), "v1");
    let staged = host
        .reload_candidate()
        .expect("candidate reload with demo should succeed");

    let rolled_back = host
        .rollback_candidate()
        .expect("rollback should discard candidate");

    assert_eq!(rolled_back, staged);
    assert!(host.candidate_snapshot().is_none());
    assert!(host.status().candidate_snapshot.is_none());
    assert!(host
        .current_snapshot()
        .plugin_registry()
        .get("demo")
        .is_none());
    let err = host
        .invoke_candidate(
            "demo",
            "demo_entry",
            json!({ "message": "hello" }).to_string(),
        )
        .expect_err("candidate invoke should fail once candidate is rolled back");
    assert!(err.to_string().contains("candidate snapshot not staged"));
}

#[test]
fn runtime_host_iterate_plugins_promotes_after_canary_replay() {
    let temp = setup_fixture_workspace_copy();
    let fixtures = temp.path().join("fixtures");
    let host = RuntimeHost::boot(&fixtures).expect("host should boot");
    let original_summary =
        plugin_node_summary(host.current_snapshot().as_ref(), "shell", "shell_entry");
    let updated_summary = format!("{original_summary} (plugin iteration)");

    let response = host
        .invoke(
            "shell",
            "shell_entry",
            json!({ "action": "start_terminal", "command": "echo hi" }).to_string(),
        )
        .expect("expr invoke should seed canary replay");
    let value: Value = serde_json::from_str(&response.payload).expect("shell response json");
    assert_eq!(value.get("output").and_then(|v| v.as_str()), Some("hi"));

    let result = host
        .iterate_plugins(KernelPluginIterationRequest {
            issue_id: None,
            target_plugin_paths: vec!["shell".to_string()],
            instruction: Some("update shell docs summary".to_string()),
            edit_plan: Some(PluginEditPlan {
                issue_id: "issue-shell-docs".to_string(),
                patch_id: "patch-shell-docs".to_string(),
                summary: "update shell docs summary".to_string(),
                operations: vec![PluginEditOperation {
                    path: "plugins/shell/src/lib.rs".to_string(),
                    kind: PluginEditOpKind::ReplaceExact,
                    expected_old_string: Some(original_summary.clone()),
                    expected_sha256: None,
                    new_content: Some(updated_summary.clone()),
                    pointer: None,
                    dotted_key: None,
                    value: None,
                }],
            }),
            manual_approved: false,
            tests_command: Some(
                "cargo test --quiet --manifest-path plugins/shell/Cargo.toml".to_string(),
            ),
            safety_command: None,
            verify_profile: Some(VerificationProfile::RustWorkspace),
            quality_score: Some(95),
        })
        .expect("plugin iteration should succeed");

    assert_eq!(result.final_verdict, PluginIterationFinalVerdict::Promoted);
    assert_eq!(result.verifier_verdict, Some(VerifierVerdict::Pass));
    assert_eq!(
        result.canary.as_ref().map(|report| report.verdict),
        Some(CanaryVerdict::Pass)
    );
    assert!(host.candidate_snapshot().is_none());
    assert_eq!(
        plugin_node_summary(host.current_snapshot().as_ref(), "shell", "shell_entry"),
        updated_summary
    );

    let post_promote = host
        .invoke(
            "shell",
            "shell_entry",
            json!({ "action": "start_terminal", "command": "echo promoted" }).to_string(),
        )
        .expect("promoted snapshot should still execute shell");
    let value: Value = serde_json::from_str(&post_promote.payload).expect("shell response json");
    assert_eq!(
        value.get("output").and_then(|v| v.as_str()),
        Some("promoted")
    );

    let history = host.kernel().plugin_history();
    assert_eq!(history.len(), 1);
    assert_eq!(
        history[0].final_verdict,
        PluginIterationFinalVerdict::Promoted
    );
    assert_eq!(
        host.kernel()
            .plugin_iteration_status(&result.iteration_id)
            .expect("status should be queryable")
            .final_verdict,
        PluginIterationFinalVerdict::Promoted
    );
}

#[test]
fn runtime_host_iterate_plugins_blocks_without_canary_evidence_and_approve_promotes() {
    let temp = setup_fixture_workspace_copy();
    let fixtures = temp.path().join("fixtures");
    let host = RuntimeHost::boot(&fixtures).expect("host should boot");
    let original_summary =
        plugin_node_summary(host.current_snapshot().as_ref(), "shell", "shell_entry");
    let updated_summary = format!("{original_summary} (blocked candidate)");

    let result = host
        .iterate_plugins(KernelPluginIterationRequest {
            issue_id: None,
            target_plugin_paths: vec!["shell".to_string()],
            instruction: Some("update shell docs summary without replay evidence".to_string()),
            edit_plan: Some(PluginEditPlan {
                issue_id: "issue-shell-blocked".to_string(),
                patch_id: "patch-shell-blocked".to_string(),
                summary: "update shell docs summary without replay evidence".to_string(),
                operations: vec![PluginEditOperation {
                    path: "plugins/shell/src/lib.rs".to_string(),
                    kind: PluginEditOpKind::ReplaceExact,
                    expected_old_string: Some(original_summary.clone()),
                    expected_sha256: None,
                    new_content: Some(updated_summary.clone()),
                    pointer: None,
                    dotted_key: None,
                    value: None,
                }],
            }),
            manual_approved: false,
            tests_command: Some(
                "cargo test --quiet --manifest-path plugins/shell/Cargo.toml".to_string(),
            ),
            safety_command: None,
            verify_profile: Some(VerificationProfile::RustWorkspace),
            quality_score: Some(95),
        })
        .expect("plugin iteration should complete in blocked state");

    assert_eq!(result.final_verdict, PluginIterationFinalVerdict::Blocked);
    assert_eq!(result.verifier_verdict, Some(VerifierVerdict::Pass));
    assert_eq!(
        result.canary.as_ref().map(|report| report.verdict),
        Some(CanaryVerdict::Partial)
    );
    assert!(host.candidate_snapshot().is_some());
    assert_eq!(
        plugin_node_summary(host.current_snapshot().as_ref(), "shell", "shell_entry"),
        original_summary
    );
    assert_eq!(
        plugin_node_summary(
            host.candidate_snapshot()
                .expect("candidate should remain staged")
                .as_ref(),
            "shell",
            "shell_entry",
        ),
        updated_summary
    );
    assert_eq!(host.kernel().blocked_iterations().len(), 1);

    let approved = host
        .approve_blocked_iteration(&result.iteration_id)
        .expect("manual approve should promote candidate");
    assert_eq!(
        approved.final_verdict,
        PluginIterationFinalVerdict::Promoted
    );
    assert!(host.candidate_snapshot().is_none());
    assert!(host.kernel().blocked_iterations().is_empty());
    assert_eq!(
        plugin_node_summary(host.current_snapshot().as_ref(), "shell", "shell_entry"),
        updated_summary
    );
    assert_eq!(host.kernel().plugin_history().len(), 1);
    assert_eq!(host.kernel().status().plugin_iteration_total, 1);
    assert_eq!(
        host.kernel()
            .plugin_iteration_status(&result.iteration_id)
            .expect("approved iteration should remain queryable")
            .final_verdict,
        PluginIterationFinalVerdict::Promoted
    );
}

#[test]
fn runtime_host_iterate_plugins_policy_blocks_runtime_paths() {
    let temp = setup_fixture_workspace_copy();
    let fixtures = temp.path().join("fixtures");
    let host = RuntimeHost::boot(&fixtures).expect("host should boot");

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
        .expect("policy-blocked iteration should still return a result");

    assert_eq!(
        result.final_verdict,
        PluginIterationFinalVerdict::RolledBack
    );
    assert!(result.changed_paths.is_empty());
    assert!(host.candidate_snapshot().is_none());
    assert!(
        result
            .blocked_reason
            .as_deref()
            .unwrap_or_default()
            .contains("outside the plugin iteration surface"),
        "result: {result:?}"
    );
    assert!(host.kernel().plugin_issues().iter().any(|issue| {
        issue.root_plugin_path == "expr" && issue.source == KernelPluginIssueSource::PolicyBlocked
    }));
}

#[test]
fn runtime_host_iterate_plugins_rolls_back_invalid_plugin_manifest_and_keeps_runtime_alive() {
    let temp = setup_fixture_workspace_copy();
    let fixtures = temp.path().join("fixtures");
    let host = RuntimeHost::boot(&fixtures).expect("host should boot");
    let manifest_path = fixtures.join("plugins/root/Cargo.toml");
    let original_manifest = fs::read_to_string(&manifest_path).expect("read root manifest");

    let result = host
        .iterate_plugins(KernelPluginIterationRequest {
            issue_id: None,
            target_plugin_paths: vec!["root".to_string()],
            instruction: Some("break root child source".to_string()),
            edit_plan: Some(PluginEditPlan {
                issue_id: "issue-root-manifest".to_string(),
                patch_id: "patch-root-manifest".to_string(),
                summary: "break root child source".to_string(),
                operations: vec![PluginEditOperation {
                    path: "plugins/root/Cargo.toml".to_string(),
                    kind: PluginEditOpKind::ReplaceExact,
                    expected_old_string: Some("./child".to_string()),
                    expected_sha256: None,
                    new_content: Some("./missing-child".to_string()),
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
        .expect("invalid manifest iteration should return rollback result");

    assert_eq!(
        result.final_verdict,
        PluginIterationFinalVerdict::RolledBack
    );
    assert!(host.candidate_snapshot().is_none());
    assert_eq!(
        fs::read_to_string(&manifest_path).expect("manifest should be restored"),
        original_manifest
    );
    assert!(host.kernel().plugin_issues().iter().any(|issue| {
        issue.root_plugin_path == "root" && issue.source == KernelPluginIssueSource::LoadFailure
    }));

    let response = host
        .invoke(
            "expr",
            "expr_entry",
            json!({ "expression": "9 - 4" }).to_string(),
        )
        .expect("runtime should stay usable after rollback");
    let value: Value = serde_json::from_str(&response.payload).expect("expr response json");
    assert_eq!(value.get("value").and_then(|v| v.as_f64()), Some(5.0));
}

#[test]
fn serve_mode_supports_plugins_reload_and_kernel_status() {
    let temp = setup_fixture_copy();
    let bin = env!("CARGO_BIN_EXE_cordis-runtime");
    let mut child = Command::new(bin)
        .args([
            "serve",
            temp.path().to_str().expect("temp path utf-8"),
            "--runtime-only",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn serve cli");

    let stdin = child.stdin.as_mut().expect("stdin pipe");
    use std::io::Write as _;
    stdin
        .write_all(
            b"status\nplugins\nexecute expr::expr_entry {\"expression\":\"1 + 2 * 3\"}\nkernel status\nreload\nexit\n",
        )
        .expect("write serve commands");

    let output = child.wait_with_output().expect("wait for serve cli");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout utf-8");
    assert!(stdout.contains("serve ready snapshot_id="));
    assert!(stdout.contains("\"current_snapshot_id\""));
    assert!(stdout.contains("shell Loaded"));
    assert!(stdout.contains("\"target_node_fqn\":\"expr::expr_entry\""));
    assert!(stdout.contains("\"iteration_total\":0"));
    assert!(stdout.contains("\"from_snapshot_id\""));
    assert!(stdout.contains("\"status\":\"reloaded\""));
}

#[test]
fn serve_mode_supports_candidate_control_plane() {
    let temp = setup_fixture_copy();
    add_demo_process_plugin(temp.path(), "v1");

    let bin = env!("CARGO_BIN_EXE_cordis-runtime");
    let mut child = Command::new(bin)
        .args([
            "serve",
            temp.path().to_str().expect("temp path utf-8"),
            "--runtime-only",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn serve cli");

    let stdin = child.stdin.as_mut().expect("stdin pipe");
    use std::io::Write as _;
    stdin
        .write_all(
            b"candidate status\ncandidate reload\ncandidate status\ncandidate invoke demo demo_entry {\"message\":\"hello\"}\ncandidate promote\nstatus\ncandidate status\nexit\n",
        )
        .expect("write serve candidate commands");

    let output = child.wait_with_output().expect("wait for serve cli");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout utf-8");
    assert!(stdout.contains("serve ready snapshot_id="));
    assert!(stdout.contains("null"));
    assert!(stdout.contains("\"status\":\"staged\""));
    assert!(stdout.contains("\"candidate_snapshot_id\""));
    assert!(stdout.contains("{\"version\":\"v1\"}"));
    assert!(stdout.contains("\"current_snapshot_id\""));
    assert!(stdout.contains("\"candidate_snapshot\":null"));
}
